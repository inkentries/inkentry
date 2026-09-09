// Unified `search` surface + behaviour (ADR-081 rank fusion, ADR-082 surface
// collapse): the removed surfaces exit 2 naming their replacement, the corpus
// filters are mutually constrained, results come back as the nested code/memory
// envelope interleaved in fused order, the second query embed is elided when a
// filter makes it redundant, --budget packs the fused typed list, and the
// memory-only modifiers (--as-of, --expand-graph) plus --graph enrichment carry
// over.

use crate::plumbing_helpers;

use plumbing_helpers::{inkentry_bin_in, mount_health, mount_index_embed};
use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;
use wiremock::MockServer;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, ResponseTemplate};

// ── removed surfaces: no stub, but the error names the replacement ─────────────

fn assert_usage_error(args: &[&str], needles: &[&str]) {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let out = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args(args)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "`inkentry {}` must be a usage error (exit 2); stderr={:?}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "removed surface must write nothing to stdout; stdout={:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    for needle in needles {
        assert!(
            stderr.contains(needle),
            "expected error containing {needle:?}; stderr={stderr:?}"
        );
    }
}

#[test]
fn removed_search_mode_flag_names_the_corpus_filters() {
    for value in ["text", "ast-grep", "semantic"] {
        assert_usage_error(
            &["search", "anything", "--mode", value],
            &["--mode", "--only-text"],
        );
    }
}

#[test]
fn removed_top_level_graph_names_the_graph_replacements() {
    assert_usage_error(
        &["graph", "some_symbol"],
        &["--graph", "plumbing graph-edges"],
    );
}

// The replacement has to be named because clap's own did-you-mean points at
// `memory archive`, one edit away and completely unrelated.
#[test]
fn removed_memory_search_names_only_memory_and_not_archive() {
    assert_usage_error(&["memory", "search", "anything"], &["--only-memory"]);
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let out = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args(["memory", "search", "anything"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("archive"),
        "must not misdirect to `memory archive`; stderr={stderr:?}"
    );
}

// `memory graph` survives the removal of the top-level `graph` porcelain, so it
// must not be swept up by the migration hint.
#[test]
fn memory_graph_still_runs() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let out = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args(["memory", "graph", "--help"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`memory graph --help` must still work"
    );
}

#[test]
fn only_code_and_only_memory_are_mutually_exclusive() {
    assert_usage_error(
        &["search", "anything", "--only-code", "--only-memory"],
        &["cannot be used with"],
    );
}

// ── the interleaved code/memory envelope (deterministic, offline FTS) ──────────

// An initialised project with one code chunk and one memory entry that both
// match the word "authentication", so a `--only-text` search (zero embeds)
// returns one result from each corpus deterministically.
fn project_with_code_and_memory(home: &Path, proj: &Path) {
    std::fs::write(
        proj.join("auth.rs"),
        "pub fn login() {\n    // authentication flow entry point\n    let _ = 1;\n}\n",
    )
    .unwrap();
    inkentry_bin_in(home)
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj)
        .args(["index", "."])
        .assert()
        .success();
    inkentry_bin_in(home)
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj)
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "Authentication decision",
            "--body",
            "we standardised on JWT authentication",
        ])
        .assert()
        .success();
}

#[test]
fn only_text_interleaves_code_and_memory_in_fused_order() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    project_with_code_and_memory(home.path(), proj.path());

    let out = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args([
            "search",
            "authentication",
            "--only-text",
            "--format",
            "json",
            "--no-stale-check",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let results: Vec<serde_json::Value> =
        serde_json::from_slice(&out).expect("stdout must be a JSON array");
    assert!(
        results.len() >= 2,
        "expected at least one code and one memory result; got {results:?}"
    );

    // Every ranked member carries the discriminator + fusion metadata, and
    // exactly one of code/memory is present, matching `type`.
    for r in &results {
        assert!(r.get("fused_rank").unwrap().is_number());
        assert!(r.get("fused_score").unwrap().is_number());
        assert!(r.get("corpus_rank").unwrap().is_number());
        let has_code = r.get("code").is_some();
        let has_memory = r.get("memory").is_some();
        assert!(has_code ^ has_memory, "exactly one payload key: {r}");
        let ty = r.get("type").unwrap().as_str().unwrap();
        assert_eq!(has_code, ty == "code", "type must match the payload: {r}");
    }

    let types: Vec<&str> = results
        .iter()
        .map(|r| r.get("type").unwrap().as_str().unwrap())
        .collect();
    assert!(
        types.contains(&"code"),
        "a code result must appear: {types:?}"
    );
    assert!(
        types.contains(&"memory"),
        "a memory result must appear: {types:?}"
    );
    // Code rank 1 ties memory rank 1, broken code-before-memory, so the code
    // result leads.
    assert_eq!(types[0], "code", "code precedes memory at equal rank");

    // The payloads nest the existing serializers verbatim.
    let code = results.iter().find(|r| r["type"] == "code").unwrap();
    assert_eq!(code["code"]["file_path"], "auth.rs");
    let mem = results.iter().find(|r| r["type"] == "memory").unwrap();
    assert_eq!(mem["memory"]["title"], "Authentication decision");
    // The portable identity rides along additively; the documented fields
    // around it are untouched.
    let entity_id = mem["memory"]["entity_id"].as_str().expect("entity_id");
    assert_eq!(entity_id.len(), 64);
    assert!(mem["memory"]["id"].as_str().is_some());
}

// A corpus pair deep enough for the fused order to distinguish ordering rules:
// three code chunks and three memory entries all matching "reticulation". The
// memory bodies repeat the term and the code chunks mention it once, so the two
// corpora's raw relevance magnitudes are separated and ordered memory-first —
// the opposite of the rank-fused order, which leads with code.
fn project_with_three_of_each(home: &Path, proj: &Path) {
    for i in 1..=3 {
        std::fs::write(
            proj.join(format!("mod{i}.rs")),
            format!("pub fn stage{i}() {{\n    // reticulation step {i}\n    let _ = {i};\n}}\n"),
        )
        .unwrap();
    }
    inkentry_bin_in(home)
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj)
        .args(["index", "."])
        .assert()
        .success();
    for i in 1..=3 {
        inkentry_bin_in(home)
            .env("INKENTRY_NO_SERVER", "1")
            .current_dir(proj)
            .args([
                "memory",
                "add",
                "--kind",
                "decision",
                "--title",
                &format!("Reticulation decision {i}"),
                "--body",
                "reticulation reticulation reticulation reticulation reticulation",
            ])
            .assert()
            .success();
    }
}

// The load-bearing ranking property, asserted end to end: the fused order is
// the two corpora's *rank positions* interleaved, never their raw relevance
// magnitudes merged. Swapping `fuse`'s rank sort for a cross-corpus distance
// sort groups each corpus together instead — corpus_rank comes back as
// 1,2,3,1,2,3 rather than 1,1,2,2,3,3 — so this fails on the whole shape, not
// on one position that could coincide.
#[test]
fn fused_order_interleaves_rank_positions_not_raw_distances() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    project_with_three_of_each(home.path(), proj.path());

    let out = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args([
            "search",
            "reticulation",
            "--only-text",
            "--limit",
            "6",
            "--format",
            "json",
            "--no-stale-check",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let results: Vec<serde_json::Value> =
        serde_json::from_slice(&out).expect("stdout must be a JSON array");
    let shape: Vec<(String, u64)> = results
        .iter()
        .map(|r| {
            (
                r["type"].as_str().unwrap().to_string(),
                r["corpus_rank"].as_u64().unwrap(),
            )
        })
        .collect();

    let expected: Vec<(String, u64)> = [
        ("code", 1),
        ("memory", 1),
        ("code", 2),
        ("memory", 2),
        ("code", 3),
        ("memory", 3),
    ]
    .iter()
    .map(|(t, r)| (t.to_string(), *r))
    .collect();
    assert_eq!(
        shape,
        expected,
        "fused order must pair the corpora rank-for-rank, code first at each \
         rank; got {shape:?} from {}",
        String::from_utf8_lossy(&out)
    );

    // Guard the guard: the interleave above only rules out a magnitude sort
    // while the two corpora's raw distances stay in separate bands, because a
    // sort on them then groups each corpus instead of pairing them. If a scale
    // change ever overlaps the bands, this fires and the test needs a new
    // fixture rather than quietly ceasing to test anything.
    let band = |kind: &str, key: &str| -> (f64, f64) {
        let v: Vec<f64> = results
            .iter()
            .filter(|r| r["type"] == kind)
            .map(|r| r[key]["distance"].as_f64().unwrap())
            .collect();
        (
            v.iter().cloned().fold(f64::INFINITY, f64::min),
            v.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        )
    };
    let (code_lo, code_hi) = band("code", "code");
    let (mem_lo, mem_hi) = band("memory", "memory");
    assert!(
        code_hi < mem_lo || mem_hi < code_lo,
        "fixture no longer separates the corpora's raw distances, so a distance \
         sort would not group: code={code_lo}..{code_hi} memory={mem_lo}..{mem_hi}"
    );
}

#[test]
fn only_text_jsonl_is_one_envelope_object_per_line() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    project_with_code_and_memory(home.path(), proj.path());

    let out = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args([
            "search",
            "authentication",
            "--only-text",
            "--format",
            "jsonl",
            "--no-stale-check",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8_lossy(&out);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(
        lines.len() >= 2,
        "expected >= 2 jsonl objects; got {text:?}"
    );
    for line in &lines {
        let v: serde_json::Value =
            serde_json::from_str(line).expect("each jsonl line is one object");
        assert!(v.get("type").is_some(), "each line carries a type: {line}");
    }
}

#[test]
fn only_text_human_output_labels_each_corpus() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    project_with_code_and_memory(home.path(), proj.path());

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args([
            "search",
            "authentication",
            "--only-text",
            "--no-stale-check",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("[code]"))
        .stdout(predicate::str::contains("[memory]"))
        .stdout(predicate::str::contains("auth.rs"))
        .stdout(predicate::str::contains("Authentication decision"));
}

// ── second-embed elision (ADR-081): code prefix via /search, QA prefix via
// /index/embed; the redundant one is skipped under a corpus filter ─────────────

async fn mount_search(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/search$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "query_vector": vec![0.1f32; 896],
            "mode": "semantic",
        })))
        .mount(server)
        .await;
}

// Point loopback auto-discovery's fixed-port fallback (step 3b) at the mock.
// Step 3a's `server.port` file is not usable from a test: it now honours a
// responder only when the pid recorded beside it is a live `inkentry-server`
// process reporting the recorded instance id.
fn loopback_discovery_port(state_dir: &Path, url: &str) -> String {
    std::fs::create_dir_all(state_dir).unwrap();
    url.rsplit(':')
        .next()
        .unwrap()
        .trim_end_matches('/')
        .to_string()
}

#[tokio::test]
async fn corpus_filters_elide_the_redundant_query_embed() {
    let mock = MockServer::start().await;
    mount_health(&mock).await;
    mount_index_embed(&mock).await; // POST .../index/embed  → memory QA embed
    mount_search(&mock).await; // POST .../search        → code-prefix embed

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    // Index offline so the project resolves; the searches below reach the mock
    // via loopback auto-discovery.
    std::fs::write(
        proj.path().join("auth.rs"),
        "pub fn login() { let _ = 1; }\n",
    )
    .unwrap();
    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args(["index", "."])
        .assert()
        .success();

    let state_dir = TempDir::new().unwrap();
    let discovery_port = loopback_discovery_port(state_dir.path(), &mock.uri());

    // (args, expected /search embeds, expected /index/embed embeds)
    let cases: &[(&[&str], usize, usize)] = &[
        (&["search", "login"], 1, 1),                  // default: both prefixes
        (&["search", "login", "--only-code"], 1, 0),   // memory embed elided
        (&["search", "login", "--only-memory"], 0, 1), // code embed elided
        (&["search", "login", "--only-text"], 0, 0),   // no embed at all
    ];

    let mut seen = 0usize;
    for (args, want_search, want_embed) in cases {
        inkentry_bin_in(home.path())
            .env_remove("INKENTRY_NO_SERVER")
            .env_remove("INKENTRY_SERVER_URL")
            .env_remove("INKENTRY_MODE")
            .env("INKENTRY_STATE_DIR", state_dir.path())
            .env("INKENTRY_TEST_DISCOVERY_PORT", &discovery_port)
            .current_dir(proj.path())
            .args(*args)
            .args(["--no-stale-check"])
            .assert()
            .success();

        let all = mock.received_requests().await.unwrap_or_default();
        let window = &all[seen..];
        let searches = window
            .iter()
            .filter(|r| r.url.path().ends_with("/search"))
            .count();
        let embeds = window
            .iter()
            .filter(|r| r.url.path().ends_with("/index/embed"))
            .count();
        seen = all.len();
        assert_eq!(
            (searches, embeds),
            (*want_search, *want_embed),
            "for `inkentry {}`: expected {want_search} /search + {want_embed} /index/embed, \
             got {searches} + {embeds}",
            args.join(" "),
        );
    }
}

// ── --budget over the fused, typed list ────────────────────────────────────────

#[test]
fn budget_packs_the_fused_typed_list_within_the_token_budget() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    project_with_code_and_memory(home.path(), proj.path());

    let out = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args([
            "search",
            "authentication",
            "--only-text",
            "--budget",
            "100000",
            "--format",
            "json",
            "--no-stale-check",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let resp: serde_json::Value = serde_json::from_slice(&out).expect("budget json object");
    assert_eq!(resp["token_budget"], 100_000);
    let used = resp["tokens_used"]
        .as_u64()
        .expect("tokens_used is a number");
    assert!(
        used <= 100_000,
        "tokens_used must not overshoot the budget: {used}"
    );
    assert_eq!(
        resp["tokens_used"].as_u64().unwrap() + resp["tokens_remaining"].as_u64().unwrap(),
        100_000,
        "used + remaining must equal the budget"
    );

    let results = resp["results"].as_array().expect("results is an array");
    // A generous budget packs the whole fused list: both corpora, and each item
    // still a typed envelope (so memory items are token-estimated and packed too).
    let types: Vec<&str> = results
        .iter()
        .map(|r| r["type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"code"), "code result packed: {types:?}");
    assert!(types.contains(&"memory"), "memory result packed: {types:?}");
    for r in results {
        let has_code = r.get("code").is_some();
        let has_memory = r.get("memory").is_some();
        assert!(
            has_code ^ has_memory,
            "each packed item is one typed envelope: {r}"
        );
    }
}

#[test]
fn budget_zero_packs_nothing_but_stays_a_valid_envelope() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    project_with_code_and_memory(home.path(), proj.path());

    let out = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args([
            "search",
            "authentication",
            "--only-text",
            "--budget",
            "0",
            "--format",
            "json",
            "--no-stale-check",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let resp: serde_json::Value = serde_json::from_slice(&out).expect("budget json object");
    assert_eq!(resp["token_budget"], 0);
    assert_eq!(resp["tokens_used"], 0);
    assert!(
        resp["results"].as_array().unwrap().is_empty(),
        "a zero budget packs nothing: {resp}"
    );
}

// ── code project: --graph appendix + exact-symbol-at-top ───────────────────────

// A project where `reticulate_splines` calls `helper_xyz`. The FTS query for the
// caller's exact name matches only the caller's chunk, so `helper_xyz` can only
// enter results via the call-graph appendix (it never matches the query text).
fn indexed_code_project(home: &Path, proj: &Path) {
    std::fs::write(
        proj.join("splines.rs"),
        "pub fn reticulate_splines() {\n    helper_xyz();\n}\n\n\
         pub fn helper_xyz() {\n    let _ = 1;\n}\n",
    )
    .unwrap();
    inkentry_bin_in(home)
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj)
        .args(["index", "."])
        .assert()
        .success();
}

#[test]
fn exact_symbol_query_returns_its_chunk_at_the_top() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    indexed_code_project(home.path(), proj.path());

    let out = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args([
            "search",
            "reticulate_splines",
            "--only-code",
            "--only-text",
            "--format",
            "json",
            "--no-stale-check",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let results: serde_json::Value = serde_json::from_slice(&out).expect("json array");
    let first = &results.as_array().expect("array")[0];
    assert_eq!(first["type"], "code");
    assert_eq!(
        first["code"]["name"], "reticulate_splines",
        "an exact identifier query ranks its own chunk first (FTS-on-name): {results}"
    );
    assert_eq!(first["fused_rank"], 1);
}

#[test]
fn graph_flag_appends_call_graph_neighbours_e2e() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    indexed_code_project(home.path(), proj.path());

    let out = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args([
            "search",
            "reticulate_splines",
            "--graph",
            "--only-code",
            "--only-text",
            "--format",
            "json",
            "--no-stale-check",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let results = serde_json::from_slice::<serde_json::Value>(&out)
        .expect("json array")
        .as_array()
        .expect("array")
        .clone();

    // The ranked member is the caller; it is a real ranked result, not an appendix.
    assert!(
        results
            .iter()
            .any(|r| r["code"]["name"] == "reticulate_splines" && r["fused_rank"].is_number()),
        "the queried symbol is a ranked member: {results:?}"
    );

    // `helper_xyz` never matched the query text; it can only be here via --graph,
    // appended with from_graph = true and null fusion metadata.
    let appended: Vec<&serde_json::Value> = results
        .iter()
        .filter(|r| r["code"]["from_graph"] == serde_json::Value::Bool(true))
        .collect();
    assert!(
        !appended.is_empty(),
        "--graph appends 1-hop neighbours: {results:?}"
    );
    let neighbour = appended[0];
    assert_eq!(neighbour["type"], "code");
    assert_eq!(neighbour["code"]["name"], "helper_xyz");
    assert!(neighbour["fused_rank"].is_null(), "appendix rank is null");
    assert!(neighbour["fused_score"].is_null(), "appendix score is null");
    assert!(
        neighbour["corpus_rank"].is_null(),
        "appendix corpus_rank is null"
    );
}

// ── memory-only modifiers on search: --as-of and --expand-graph ────────────────

fn memory_project(home: &Path, proj: &Path) {
    std::fs::write(proj.join("x.rs"), "pub fn x() {}\n").unwrap();
    inkentry_bin_in(home)
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj)
        .args(["index", "."])
        .assert()
        .success();
}

fn memory_add(home: &Path, proj: &Path, extra: &[&str]) -> String {
    let out = inkentry_bin_in(home)
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj)
        .args(["memory", "add"])
        .args(extra)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "memory add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn search_memory_stdout(home: &Path, proj: &Path, extra: &[&str]) -> String {
    let out = inkentry_bin_in(home)
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj)
        .args(["search"])
        .args(extra)
        .args(["--only-memory", "--only-text", "--no-stale-check"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn as_of_filters_the_memory_corpus() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    memory_project(home.path(), proj.path());
    memory_add(
        home.path(),
        proj.path(),
        &[
            "--kind",
            "decision",
            "--title",
            "Timegate widget policy",
            "--body",
            "the widget policy body",
        ],
    );

    // The note was created now (valid_at defaults to created_at), so a point in
    // the distant past predates it and it must not appear.
    let past = search_memory_stdout(
        home.path(),
        proj.path(),
        &["widget", "--as-of", "2000-01-01"],
    );
    assert!(
        !past.contains("Timegate widget policy"),
        "an as-of before the note existed must exclude it: {past}"
    );

    // Without --as-of, the active note is returned.
    let now = search_memory_stdout(home.path(), proj.path(), &["widget"]);
    assert!(
        now.contains("Timegate widget policy"),
        "the note is returned without a temporal filter: {now}"
    );
}

#[test]
fn expand_graph_pulls_in_related_memory_neighbours() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    memory_project(home.path(), proj.path());

    let a = memory_add(
        home.path(),
        proj.path(),
        &[
            "--kind",
            "decision",
            "--title",
            "Alpha about frobnicators",
            "--body",
            "frobnicator design",
        ],
    );
    // "Stored [decision] #<uuid>: <title>"
    let a_id: String = a
        .split('#')
        .nth(1)
        .and_then(|s| s.split(':').next())
        .map(|s| s.trim().to_string())
        .expect("stored output carries an id");
    assert!(
        uuid::Uuid::parse_str(&a_id).is_ok(),
        "stored id must be a UUID, got {a_id:?}"
    );
    // B relates to A but shares no query terms with it.
    memory_add(
        home.path(),
        proj.path(),
        &[
            "--kind",
            "note",
            "--title",
            "Beta unrelated sidenote",
            "--body",
            "nothing to see here",
            "--relates-to",
            &a_id,
        ],
    );

    // Plain memory search finds only A (B never matches "frobnicator").
    let plain = search_memory_stdout(home.path(), proj.path(), &["frobnicator"]);
    assert!(
        plain.contains("Alpha about frobnicators"),
        "A matches: {plain}"
    );
    assert!(
        !plain.contains("Beta unrelated sidenote"),
        "B does not match the query without expansion: {plain}"
    );

    // --expand-graph pulls B in via the relates_to edge.
    let expanded =
        search_memory_stdout(home.path(), proj.path(), &["frobnicator", "--expand-graph"]);
    assert!(
        expanded.contains("Alpha about frobnicators"),
        "A still present: {expanded}"
    );
    assert!(
        expanded.contains("Beta unrelated sidenote"),
        "--expand-graph surfaces the related note: {expanded}"
    );

    // B was reached from A, not ranked against the query, so it is an
    // attachment: null fusion metadata, exactly like a --graph code neighbour.
    // With a corpus_rank it would take a ranked position and displace a
    // genuinely matched result (ADR-081).
    let json = search_memory_stdout(
        home.path(),
        proj.path(),
        &["frobnicator", "--expand-graph", "--format", "json"],
    );
    let results: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let a = results
        .iter()
        .find(|r| r["memory"]["title"] == "Alpha about frobnicators")
        .unwrap();
    let b = results
        .iter()
        .find(|r| r["memory"]["title"] == "Beta unrelated sidenote")
        .unwrap();
    assert_eq!(a["corpus_rank"], 1, "the matched note is ranked: {a}");
    assert!(
        b["fused_rank"].is_null() && b["fused_score"].is_null() && b["corpus_rank"].is_null(),
        "an expand-graph neighbour must carry no fusion metadata: {b}"
    );
    let ranked_last = results
        .iter()
        .position(|r| r["fused_rank"].is_number())
        .zip(results.iter().rposition(|r| r["fused_rank"].is_number()));
    let attached_first = results.iter().position(|r| r["fused_rank"].is_null());
    if let (Some((_, last)), Some(first)) = (ranked_last, attached_first) {
        assert!(
            last < first,
            "attachments follow every ranked member: {json}"
        );
    }
}
