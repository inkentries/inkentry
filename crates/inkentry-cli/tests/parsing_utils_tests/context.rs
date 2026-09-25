use crate::plumbing_helpers;
use plumbing_helpers::{init_git_repo, inkentry_bin};

use assert_cmd::Command;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn setup_context_project() -> (TempDir, PathBuf, PathBuf) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let tmp = TempDir::new().expect("create temp dir");
    // `context` fails closed without a local `.inkentry/` project; the memory store then resolves
    // to `<tmp>/.inkentry/memory.db`, where the entries below are seeded.
    std::fs::create_dir_all(tmp.path().join(".inkentry")).expect("create .inkentry");
    let db_path = tmp.path().join(".inkentry").join("index.db");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock_server = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "embedding": vec![0.1f32; 896], "index": 0 }],
                "model": "test-model",
                "object": "list",
                "usage": { "prompt_tokens": 5, "total_tokens": 5 }
            })))
            .mount(&server)
            .await;
        server
    });

    let mock_url = mock_server.uri();
    let config_path = write_config_for_context(tmp.path(), &db_path, &mock_url);

    let mem_path = db_path.with_file_name("memory.db");

    let entries: &[(&str, &str, &str)] = &[
        (
            "handoff",
            "Handoff: session #1",
            "Implemented the context command. Next: tests and docs.",
        ),
        (
            "handoff",
            "Handoff: session #2",
            "Reviewed PR #134. Fixed 5 CI blockers.",
        ),
        (
            "decision",
            "Use sqlite for memory backend",
            "Chose sqlite over git-notes for performance.",
        ),
        (
            "decision",
            "JSONL for plumbing output",
            "All plumbing commands emit JSONL, one object per line.",
        ),
        (
            "decision",
            "Context command design",
            "inkentry context replaces three separate memory list calls for agent workflow.",
        ),
        (
            "question",
            "Should we support remote backends?",
            "Need architect input on remote memory sync priority.",
        ),
        (
            "question",
            "What about pagination?",
            "If we have 1000+ decisions, should context paginate?",
        ),
        (
            "requirement",
            "All commands must support --format json",
            "Porcelain commands need machine-readable output mode.",
        ),
        (
            "requirement",
            "Exit codes follow protocol",
            "0=ok, 1=no-results, 2=error for all plumbing commands.",
        ),
        (
            "note",
            "GitHub Actions CI is flaky on macOS",
            "Intermittent timeouts on macos-14 runner.",
        ),
    ];

    for (kind, title, body) in entries {
        inkentry_bin()
            // The git-notes carrier follows the process CWD's repo and `--db` does not redirect it;
            // seed from the temp project or the entries land in the repo under test.
            .current_dir(tmp.path())
            .arg("--config")
            .arg(&config_path)
            .arg("memory")
            .arg("--db")
            .arg(&mem_path)
            .arg("add")
            .arg("--kind")
            .arg(kind)
            .arg("--title")
            .arg(title)
            .arg("--body")
            .arg(body)
            .assert()
            .success();
    }

    (tmp, db_path, config_path)
}

fn write_config_for_context(dir: &Path, db_path: &Path, api_base: &str) -> PathBuf {
    let cfg = format!(
        "db_path = {:?}\napi_base_url = {:?}\nllm_model = \"test-chat\"\n",
        db_path, api_base
    );
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, cfg).expect("write config");
    config_path
}

// Passes no `--db`: the command derives memory.db from the config's db_path, which is where
// `setup_context_project` seeds entries.
fn context_cmd(_db_path: &Path, config_path: &Path) -> Command {
    let mut cmd = inkentry_bin();
    // Run from the temp dir so find_project_db doesn't discover the real .inkentry/index.db above.
    if let Some(dir) = config_path.parent() {
        cmd.current_dir(dir);
    }
    cmd.arg("--config").arg(config_path).arg("context");
    cmd
}

// Fixed-size notes keep `--budget` math deterministic (chars/4): a 4-char title (1 token) plus
// a 400-char body (100) is 101 tokens each. 1 handoff, 3 questions, 2 decisions, 2 requirements.
fn setup_budget_project() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("create temp dir");
    std::fs::create_dir_all(tmp.path().join(".inkentry")).expect("create .inkentry");
    let db_path = tmp.path().join(".inkentry").join("index.db");
    // No embed server: memory add stores without a vector, irrelevant to budget packing.
    let config_path = write_config_for_context(tmp.path(), &db_path, "http://127.0.0.1:19999");
    let mem_path = db_path.with_file_name("memory.db");

    let body = "x".repeat(400);
    let entries: &[(&str, &str)] = &[
        ("handoff", "hnd0"),
        ("question", "qst0"),
        ("question", "qst1"),
        ("question", "qst2"),
        ("decision", "dec0"),
        ("decision", "dec1"),
        ("requirement", "req0"),
        ("requirement", "req1"),
    ];
    for (kind, title) in entries {
        inkentry_bin()
            // The git-notes carrier follows process CWD, not `--db`.
            .current_dir(tmp.path())
            .arg("--config")
            .arg(&config_path)
            .arg("memory")
            .arg("--db")
            .arg(&mem_path)
            .arg("add")
            .arg("--kind")
            .arg(kind)
            .arg("--title")
            .arg(title)
            .arg("--body")
            .arg(&body)
            .assert()
            .success();
    }
    (tmp, config_path)
}

#[test]
fn context_budget_keeps_durable_drops_questions_e2e() {
    // A 505-token budget fits every decision+requirement+handoff (5 * 101) with nothing left for
    // the 3 questions, so questions drop first even though they display before decisions.
    let (_tmp, config_path) = setup_budget_project();

    let mut cmd = inkentry_bin();
    if let Some(dir) = config_path.parent() {
        cmd.current_dir(dir);
    }
    let output = cmd
        .arg("--config")
        .arg(&config_path)
        .arg("context")
        .arg("--budget")
        .arg("505")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);
    let obj: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let parsed = obj["sections"].as_array().expect("sections array");

    // Display order is unchanged by packing; the intent section leads (empty: no sessions seeded).
    let kinds: Vec<&str> = parsed.iter().map(|s| s[0].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["intent", "handoff", "question", "decision", "requirement"],
        "section display order must stay intent -> handoff -> question -> decision -> requirement"
    );

    let len_of = |kind: &str| -> usize {
        parsed
            .iter()
            .find(|s| s[0].as_str() == Some(kind))
            .and_then(|s| s[1].as_array())
            .map(|n| n.len())
            .unwrap_or(0)
    };
    assert_eq!(len_of("decision"), 2, "every durable decision survives");
    assert_eq!(
        len_of("requirement"),
        2,
        "every durable requirement survives"
    );
    assert_eq!(len_of("handoff"), 1, "handoff outranks question, survives");
    assert_eq!(len_of("question"), 0, "ephemeral questions drop first");

    assert_eq!(obj["token_budget"].as_u64(), Some(505));
    assert_eq!(obj["tokens_used"].as_u64(), Some(505));
    assert_eq!(obj["tokens_remaining"].as_u64(), Some(0));
}

#[test]
fn context_outputs_all_four_sections_by_default() {
    let (_tmp, db_path, config_path) = setup_context_project();

    let output = context_cmd(&db_path, &config_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);

    assert!(stdout.contains("Handoffs"), "expected 'Handoffs' header");
    assert!(
        stdout.contains("Open questions"),
        "expected 'Open questions' header"
    );
    assert!(stdout.contains("Decisions"), "expected 'Decisions' header");
    assert!(
        stdout.contains("Requirements"),
        "expected 'Requirements' header"
    );
}

#[test]
fn context_json_output_is_valid_object() {
    let (_tmp, db_path, config_path) = setup_context_project();

    let output = context_cmd(&db_path, &config_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);

    let obj: serde_json::Value =
        serde_json::from_str(&stdout).expect("--format json should produce valid JSON");
    let parsed = obj["sections"]
        .as_array()
        .expect("sections should be array");

    assert!(!parsed.is_empty(), "expected at least one section");
    for item in parsed {
        let arr = item.as_array().expect("each item should be [kind, notes]");
        assert_eq!(arr.len(), 2, "each item should be a [kind, notes] pair");
        assert!(arr[0].is_string(), "first element should be kind string");
        assert!(arr[1].is_array(), "second element should be notes array");
    }
}

#[test]
fn context_json_includes_all_kinds() {
    let (_tmp, db_path, config_path) = setup_context_project();

    let output = context_cmd(&db_path, &config_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);
    let obj: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let parsed = obj["sections"].as_array().expect("sections array");

    let kinds: Vec<&str> = parsed
        .iter()
        .map(|item| item[0].as_str().unwrap_or(""))
        .collect();

    assert!(kinds.contains(&"handoff"), "should include handoff section");
    assert!(
        kinds.contains(&"decision"),
        "should include decision section"
    );
    assert!(
        kinds.contains(&"question"),
        "should include question section"
    );
    assert!(
        kinds.contains(&"requirement"),
        "should include requirement section"
    );
}

#[test]
fn context_kind_filter_shows_only_requested_kind() {
    let (_tmp, db_path, config_path) = setup_context_project();

    let output = context_cmd(&db_path, &config_path)
        .arg("--kind")
        .arg("decision")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);

    assert!(
        stdout.contains("Decisions"),
        "should show Decisions when --kind decision"
    );
    assert!(
        !stdout.contains("Handoffs"),
        "should NOT show Handoffs when --kind decision"
    );
    assert!(
        !stdout.contains("Open questions"),
        "should NOT show questions"
    );
    assert!(
        !stdout.contains("Requirements"),
        "should NOT show requirements"
    );
}

#[test]
fn context_kind_filter_json_returns_single_section() {
    let (_tmp, db_path, config_path) = setup_context_project();

    let output = context_cmd(&db_path, &config_path)
        .arg("--kind")
        .arg("question")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);
    let obj: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let parsed = obj["sections"].as_array().expect("sections array");

    assert_eq!(parsed.len(), 1, "--kind should return exactly one section");
    assert_eq!(parsed[0][0].as_str().unwrap(), "question");
}

#[test]
fn context_limit_flag_respects_count() {
    let (_tmp, db_path, config_path) = setup_context_project();

    let output = context_cmd(&db_path, &config_path)
        .arg("--kind")
        .arg("decision")
        .arg("--limit")
        .arg("2")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);
    let obj: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let parsed = obj["sections"].as_array().expect("sections array");

    let notes = parsed[0][1].as_array().expect("notes should be array");
    assert_eq!(
        notes.len(),
        2,
        "--limit 2 should return exactly 2 entries, got {}",
        notes.len()
    );
}

#[test]
fn context_default_limits_respected() {
    let (_tmp, db_path, config_path) = setup_context_project();

    // 2 handoffs are seeded and the default handoff limit is 3, so both appear.
    let output = context_cmd(&db_path, &config_path)
        .arg("--kind")
        .arg("handoff")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);
    let obj: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let parsed = obj["sections"].as_array().expect("sections array");

    let notes = parsed[0][1].as_array().expect("notes should be array");
    assert_eq!(
        notes.len(),
        2,
        "default handoff limit of 3 should include both entries"
    );
}

#[test]
fn context_empty_memory_exits_zero_with_no_output() {
    let tmp = TempDir::new().expect("create temp dir");
    // A local `.inkentry/` makes this a real (empty) project.
    std::fs::create_dir_all(tmp.path().join(".inkentry")).expect("create .inkentry");
    let db_path = tmp.path().join(".inkentry").join("index.db");
    let config_path = write_config_for_context(tmp.path(), &db_path, "http://127.0.0.1:19999");

    let output = context_cmd(&db_path, &config_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);
    let obj: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let parsed = obj["sections"].as_array().expect("sections array");

    for item in parsed {
        let notes = item[1].as_array().expect("notes should be array");
        assert!(
            notes.is_empty(),
            "section with no entries should have empty array"
        );
    }
}

#[test]
fn context_exits_zero_on_success() {
    let (_tmp, db_path, config_path) = setup_context_project();

    context_cmd(&db_path, &config_path).assert().code(0);
}

#[test]
fn context_exits_zero_with_kind_filter() {
    let (_tmp, db_path, config_path) = setup_context_project();

    context_cmd(&db_path, &config_path)
        .arg("--kind")
        .arg("decision")
        .assert()
        .code(0);
}

#[test]
fn context_exits_zero_when_kind_has_no_entries() {
    // No `intent` entries are seeded, but empty results are not an error for porcelain.
    let (_tmp, db_path, config_path) = setup_context_project();

    context_cmd(&db_path, &config_path)
        .arg("--kind")
        .arg("intent")
        .assert()
        .code(0);
}

#[test]
fn context_explicit_sqlite_backend_works() {
    let (_tmp, db_path, config_path) = setup_context_project();

    context_cmd(&db_path, &config_path)
        .arg("--backend")
        .arg("sqlite")
        .arg("--format")
        .arg("json")
        .assert()
        .success();
}

#[test]
fn context_json_notes_have_required_fields() {
    let (_tmp, db_path, config_path) = setup_context_project();

    let output = context_cmd(&db_path, &config_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&output);
    let obj: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let parsed = obj["sections"].as_array().expect("sections array");

    for item in parsed {
        let notes = item[1].as_array().expect("notes should be array");
        for note in notes {
            assert!(note.get("id").is_some(), "note missing 'id': {note}");
            assert!(note.get("kind").is_some(), "note missing 'kind': {note}");
            assert!(note.get("title").is_some(), "note missing 'title': {note}");
            assert!(note.get("body").is_some(), "note missing 'body': {note}");
        }
    }
}

#[test]
fn context_exits_nonzero_when_config_invalid() {
    // A db_path under an existing regular file cannot have its parent created, so MemoryStore
    // errors. A real file (not `/dev/null`) keeps this cross-platform.
    let tmp = TempDir::new().expect("create temp dir");
    let config_path = tmp.path().join("config.toml");
    let blocker = tmp.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").expect("write blocker file");
    let db_path = blocker.join("impossible").join("inkentry.db");

    std::fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:19999\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .expect("write config");

    inkentry_bin()
        .current_dir(&tmp)
        .arg("--config")
        .arg(&config_path)
        .arg("context")
        .assert()
        .failure();
}

#[test]
fn context_unknown_format_falls_back_to_text() {
    let (_tmp, db_path, config_path) = setup_context_project();

    context_cmd(&db_path, &config_path)
        .arg("--format")
        .arg("yaml")
        .assert()
        .success();
}

// The "Active agent sessions" section lists `intent` entries and warns when a file this
// worktree modified is claimed by an active intent. The roster packs last under `--budget`;
// overlap warnings are budget-exempt.

// Call while `tmp` is NOT yet a git repo so the git-notes carrier stays a no-op.
fn add_note(
    tmp: &Path,
    config: &Path,
    mem: &Path,
    kind: &str,
    title: &str,
    body: &str,
    files: Option<&str>,
) {
    let mut cmd = inkentry_bin();
    cmd.env("INKENTRY_NO_SERVER", "1")
        .current_dir(tmp)
        .arg("--config")
        .arg(config)
        .arg("memory")
        .arg("--db")
        .arg(mem)
        .arg("add")
        .arg("--kind")
        .arg(kind)
        .arg("--title")
        .arg(title)
        .arg("--body")
        .arg(body);
    if let Some(f) = files {
        cmd.arg("--files").arg(f);
    }
    cmd.assert().success();
}

fn setup_intents_project(intents: &[(&str, Option<&str>)]) -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().expect("create temp dir");
    std::fs::create_dir_all(tmp.path().join(".inkentry")).expect("create .inkentry");
    let db_path = tmp.path().join(".inkentry").join("index.db");
    // Unreachable URL: intent listing/overlap needs no embedding server.
    let config_path = write_config_for_context(tmp.path(), &db_path, "http://127.0.0.1:19999");
    let mem_path = db_path.with_file_name("memory.db");
    for (title, files) in intents {
        add_note(
            tmp.path(),
            &config_path,
            &mem_path,
            "intent",
            title,
            "active work",
            *files,
        );
    }
    (tmp, db_path, config_path)
}

// Creates and commits `files`, then dirties each: tracked-and-modified is unambiguous across
// gix versions, unlike untracked-file defaults.
fn make_files_modified(dir: &Path, files: &[&str]) {
    for f in files {
        let path = dir.join(f);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(&path, "orig\n").expect("write file");
    }
    init_git_repo(dir);
    for f in files {
        use std::io::Write as _;
        let mut fh = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(f))
            .expect("open for append");
        writeln!(fh, "changed").expect("append to file");
    }
}

fn context_in(dir: &Path, config: &Path) -> Command {
    let mut cmd = inkentry_bin();
    cmd.env("INKENTRY_NO_SERVER", "1")
        .current_dir(dir)
        .arg("--config")
        .arg(config)
        .arg("context");
    cmd
}

#[test]
fn context_lists_active_intent_without_overlap() {
    let (tmp, _db, config) = setup_intents_project(&[(
        "Refactoring auth middleware",
        Some("src/auth/middleware.rs"),
    )]);

    let stdout = context_in(tmp.path(), &config)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&stdout);

    assert!(
        text.contains("Active agent sessions"),
        "expected the active-sessions section; got:\n{text}"
    );
    assert!(
        text.contains("Refactoring auth middleware"),
        "roster must list the intent title; got:\n{text}"
    );
    assert!(
        text.contains("files: src/auth/middleware.rs"),
        "roster must show the intent's linked files; got:\n{text}"
    );
    assert!(
        !text.contains("Overlap"),
        "a clean worktree must produce no overlap warning; got:\n{text}"
    );
}

#[test]
fn context_zero_intents_omits_section_but_json_has_empty_intent_and_overlaps() {
    let (tmp, _db, config) = setup_intents_project(&[]);
    let mem = tmp.path().join(".inkentry").join("memory.db");
    // A non-intent note so the store exists and the project is non-empty.
    add_note(
        tmp.path(),
        &config,
        &mem,
        "decision",
        "some choice",
        "why",
        None,
    );

    let text_out = context_in(tmp.path(), &config)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&text_out);
    assert!(
        !text.contains("Active agent sessions"),
        "zero intents must omit the section entirely; got:\n{text}"
    );

    let json_out = context_in(tmp.path(), &config)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let obj: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&json_out)).expect("valid JSON");
    let sections = obj["sections"].as_array().expect("sections array");
    let intent = sections
        .iter()
        .find(|s| s[0].as_str() == Some("intent"))
        .expect("JSON must still carry an intent section");
    assert!(
        intent[1].as_array().expect("intent notes array").is_empty(),
        "intent section must be an empty array when there are no intents"
    );
    assert_eq!(
        obj["overlaps"].as_array().expect("overlaps array").len(),
        0,
        "overlaps must be an empty array when there are no intents"
    );
}

#[test]
fn context_lists_multiple_active_intents() {
    let (tmp, _db, config) = setup_intents_project(&[
        ("session one", None),
        ("session two", None),
        ("session three", None),
    ]);

    let out = context_in(tmp.path(), &config)
        .arg("--kind")
        .arg("intent")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let obj: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out)).expect("valid JSON");
    let sections = obj["sections"].as_array().expect("sections array");
    let intent = sections
        .iter()
        .find(|s| s[0].as_str() == Some("intent"))
        .expect("intent section present");
    assert_eq!(
        intent[1].as_array().expect("intent notes").len(),
        3,
        "every active intent must be listed"
    );
}

#[test]
fn context_overlap_warning_has_exact_wording() {
    let (tmp, _db, config) = setup_intents_project(&[("touching x", Some("src/x.rs"))]);
    make_files_modified(tmp.path(), &["src/x.rs"]);

    let out = context_in(tmp.path(), &config)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);

    assert!(text.contains("Active agent sessions"), "section shown");
    assert!(
        text.contains("⚠  Overlap: src/x.rs is listed in an active intent"),
        "overlap line must match check's exact wording (two spaces after the \
         glyph); got:\n{text}"
    );
}

#[test]
fn context_overlap_one_line_per_overlapping_file() {
    let (tmp, _db, config) =
        setup_intents_project(&[("touching x and y", Some("src/x.rs,src/y.rs"))]);
    make_files_modified(tmp.path(), &["src/x.rs", "src/y.rs"]);

    let out = context_in(tmp.path(), &config)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);

    assert!(text.contains("⚠  Overlap: src/x.rs is listed in an active intent"));
    assert!(text.contains("⚠  Overlap: src/y.rs is listed in an active intent"));
    assert_eq!(
        text.matches("⚠  Overlap:").count(),
        2,
        "exactly one warning line per overlapping file; got:\n{text}"
    );
}

#[test]
fn context_no_overlap_when_modified_file_is_not_in_any_intent() {
    let (tmp, _db, config) = setup_intents_project(&[("touching x", Some("src/x.rs"))]);
    make_files_modified(tmp.path(), &["src/other.rs"]);

    let out = context_in(tmp.path(), &config)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);

    assert!(
        !text.contains("Overlap"),
        "no warning when the modified file matches no intent; got:\n{text}"
    );
    assert!(
        text.contains("touching x"),
        "roster is still shown even with no overlap; got:\n{text}"
    );
}

#[test]
fn context_no_overlap_in_non_git_worktree() {
    let (tmp, _db, config) = setup_intents_project(&[("touching x", Some("src/x.rs"))]);

    let out = context_in(tmp.path(), &config)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);

    assert!(
        !text.contains("Overlap"),
        "no git => no overlap; got:\n{text}"
    );
    assert!(text.contains("Active agent sessions"), "roster still shown");
    assert!(text.contains("touching x"));
}

// 101 tokens per note (4-char title + 400-char body).
fn setup_budget_overlap_project() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("create temp dir");
    std::fs::create_dir_all(tmp.path().join(".inkentry")).expect("create .inkentry");
    let db_path = tmp.path().join(".inkentry").join("index.db");
    let config = write_config_for_context(tmp.path(), &db_path, "http://127.0.0.1:19999");
    let mem = db_path.with_file_name("memory.db");
    let body = "x".repeat(400);
    add_note(
        tmp.path(),
        &config,
        &mem,
        "intent",
        "int0",
        &body,
        Some("src/x.rs"),
    );
    add_note(tmp.path(), &config, &mem, "decision", "dec0", &body, None);
    add_note(tmp.path(), &config, &mem, "handoff", "hnd0", &body, None);
    make_files_modified(tmp.path(), &["src/x.rs"]);
    (tmp, config)
}

#[test]
fn context_budget_drops_intent_roster_but_keeps_overlap_uncounted() {
    let (tmp, config) = setup_budget_overlap_project();

    // 202 tokens fits decision + handoff (101 each); the roster packs last and is dropped.
    // Overlaps are budget-exempt.
    let out = context_in(tmp.path(), &config)
        .arg("--budget")
        .arg("202")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let obj: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out)).expect("valid JSON");

    let sections = obj["sections"].as_array().expect("sections array");
    let intent = sections
        .iter()
        .find(|s| s[0].as_str() == Some("intent"))
        .expect("intent section present");
    assert!(
        intent[1].as_array().unwrap().is_empty(),
        "intent roster drops first under a tight budget"
    );
    assert_eq!(
        obj["overlaps"].as_array().unwrap(),
        &vec![serde_json::json!("src/x.rs")],
        "overlap survives even when the roster is budget-dropped"
    );
    assert_eq!(
        obj["tokens_used"].as_u64(),
        Some(202),
        "tokens_used counts the two surviving notes only, never overlap text"
    );
}

#[test]
fn context_budget_zero_still_emits_overlap_warning() {
    let (tmp, config) = setup_budget_overlap_project();

    let out = context_in(tmp.path(), &config)
        .arg("--budget")
        .arg("0")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);

    assert!(
        text.contains("Active agent sessions"),
        "section header shows because a warning exists; got:\n{text}"
    );
    assert!(
        text.contains("⚠  Overlap: src/x.rs is listed in an active intent"),
        "overlap warning is emitted even at --budget 0; got:\n{text}"
    );
    assert!(
        !text.contains("int0"),
        "the roster itself is dropped at --budget 0; got:\n{text}"
    );
    assert!(text.contains("tokens used: 0/0"));
}

#[test]
fn context_generous_budget_counts_roster_not_overlap() {
    let (tmp, config) = setup_budget_overlap_project();

    let out = context_in(tmp.path(), &config)
        .arg("--budget")
        .arg("100000")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let obj: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out)).expect("valid JSON");

    let sections = obj["sections"].as_array().expect("sections array");
    let intent = sections
        .iter()
        .find(|s| s[0].as_str() == Some("intent"))
        .expect("intent section present");
    assert_eq!(
        intent[1].as_array().unwrap().len(),
        1,
        "roster survives a generous budget"
    );
    // 3 notes * 101 tokens; overlap text adds nothing to the count.
    assert_eq!(obj["token_budget"].as_u64(), Some(100_000));
    assert_eq!(obj["tokens_used"].as_u64(), Some(303));
    assert_eq!(obj["tokens_remaining"].as_u64(), Some(100_000 - 303));
    assert_eq!(
        obj["overlaps"].as_array().unwrap(),
        &vec![serde_json::json!("src/x.rs")]
    );
}

#[test]
fn context_json_overlaps_survive_budget_when_roster_emptied() {
    let (tmp, config) = setup_budget_overlap_project();

    let out = context_in(tmp.path(), &config)
        .arg("--budget")
        .arg("0")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let obj: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out)).expect("valid JSON");

    let sections = obj["sections"].as_array().expect("sections array");
    let intent = sections
        .iter()
        .find(|s| s[0].as_str() == Some("intent"))
        .expect("intent section present");
    assert!(
        intent[1].as_array().unwrap().is_empty(),
        "roster emptied at --budget 0"
    );
    assert_eq!(
        obj["overlaps"].as_array().unwrap(),
        &vec![serde_json::json!("src/x.rs")],
        "overlaps survive in JSON even when the roster is emptied"
    );
    assert_eq!(obj["tokens_used"].as_u64(), Some(0));
}

#[test]
fn context_local_only_matches_default_for_intents() {
    let (tmp, _db, config) = setup_intents_project(&[("local intent", Some("src/a.rs"))]);

    let run = |extra: Option<&str>| -> serde_json::Value {
        let mut cmd = context_in(tmp.path(), &config);
        cmd.arg("--kind").arg("intent").arg("--format").arg("json");
        if let Some(flag) = extra {
            cmd.arg(flag);
        }
        let out = cmd.assert().success().get_output().stdout.clone();
        serde_json::from_str(&String::from_utf8_lossy(&out)).expect("valid JSON")
    };

    let without = run(None);
    let with_local_only = run(Some("--local-only"));
    assert_eq!(
        without["sections"], with_local_only["sections"],
        "intents are already local; --local-only must not change the roster"
    );
    assert_eq!(without["overlaps"], with_local_only["overlaps"]);
}

#[test]
fn context_kind_intent_shows_only_active_sessions() {
    let (tmp, _db, config) = setup_intents_project(&[("only intent", None)]);
    let mem = tmp.path().join(".inkentry").join("memory.db");
    add_note(
        tmp.path(),
        &config,
        &mem,
        "decision",
        "a decision",
        "why",
        None,
    );
    add_note(
        tmp.path(),
        &config,
        &mem,
        "handoff",
        "a handoff",
        "state",
        None,
    );

    let out = context_in(tmp.path(), &config)
        .arg("--kind")
        .arg("intent")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);

    assert!(text.contains("Active agent sessions"));
    assert!(!text.contains("Handoffs"), "no other section; got:\n{text}");
    assert!(!text.contains("Decisions"));
    assert!(!text.contains("Open questions"));
    assert!(!text.contains("Requirements"));
    assert!(!text.contains("Conventions"));
}

#[test]
fn context_path_filter_scopes_roster_and_overlap() {
    let (tmp, _db, config) = setup_intents_project(&[
        ("intent A", Some("src/a.rs")),
        ("intent B", Some("src/b.rs")),
    ]);
    make_files_modified(tmp.path(), &["src/a.rs", "src/b.rs"]);

    let out = context_in(tmp.path(), &config)
        .arg("--path")
        .arg("src/a.rs")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);

    assert!(text.contains("intent A"), "path-matching intent shown");
    assert!(!text.contains("intent B"), "non-matching intent excluded");
    assert!(text.contains("⚠  Overlap: src/a.rs is listed in an active intent"));
    assert!(
        !text.contains("src/b.rs"),
        "overlap is computed only from the path-scoped intent set; got:\n{text}"
    );
}

#[test]
fn context_limit_overrides_intent_default_cap() {
    let (tmp, _db, config) =
        setup_intents_project(&[("one", None), ("two", None), ("three", None)]);

    let out = context_in(tmp.path(), &config)
        .arg("--kind")
        .arg("intent")
        .arg("--limit")
        .arg("2")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let obj: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out)).expect("valid JSON");
    let sections = obj["sections"].as_array().expect("sections array");
    let intent = sections
        .iter()
        .find(|s| s[0].as_str() == Some("intent"))
        .expect("intent section present");
    assert_eq!(
        intent[1].as_array().unwrap().len(),
        2,
        "--limit overrides the intent roster's default cap"
    );
}

#[test]
fn context_json_has_intent_section_and_overlaps_array() {
    let (tmp, _db, config) = setup_intents_project(&[("touching x", Some("src/x.rs"))]);
    make_files_modified(tmp.path(), &["src/x.rs"]);

    let out = context_in(tmp.path(), &config)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let obj: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out)).expect("valid JSON");

    let sections = obj["sections"].as_array().expect("sections array");
    let intent = sections
        .iter()
        .find(|s| s[0].as_str() == Some("intent"))
        .expect("intent section present in JSON");
    assert_eq!(intent[1].as_array().unwrap().len(), 1);
    let overlaps = obj["overlaps"].as_array().expect("overlaps is an array");
    assert_eq!(overlaps, &vec![serde_json::json!("src/x.rs")]);
    assert!(
        overlaps.iter().all(|v| v.is_string()),
        "overlaps is a flat array of path strings"
    );
}

#[test]
fn context_agent_env_includes_intent_and_overlaps() {
    let (tmp, _db, config) = setup_intents_project(&[("touching x", Some("src/x.rs"))]);
    make_files_modified(tmp.path(), &["src/x.rs"]);

    // AGENT=true with no --format yields JSON.
    let out = context_in(tmp.path(), &config)
        .env("AGENT", "true")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let obj: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out)).expect("AGENT=true yields JSON");

    let sections = obj["sections"].as_array().expect("sections array");
    assert!(
        sections.iter().any(|s| s[0].as_str() == Some("intent")),
        "AGENT JSON must carry the intent section"
    );
    assert_eq!(
        obj["overlaps"].as_array().unwrap(),
        &vec![serde_json::json!("src/x.rs")]
    );
}

#[test]
fn context_color_disabled_strips_ansi_but_keeps_glyph() {
    let (tmp, _db, config) = setup_intents_project(&[("touching x", Some("src/x.rs"))]);
    make_files_modified(tmp.path(), &["src/x.rs"]);

    let out = context_in(tmp.path(), &config)
        .arg("--color")
        .arg("never")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);

    assert!(
        !text.contains('\u{1b}'),
        "color-disabled output must contain no ANSI escapes; got:\n{text:?}"
    );
    assert!(
        text.contains('⚠'),
        "the warning glyph is retained even with color off; got:\n{text}"
    );
}
