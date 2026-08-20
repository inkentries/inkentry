// Egress containment for local-tier flows: the only outbound connections a
// local-tier command may make are to the auto-discovered loopback inference
// server. Every test here wires `egress_trap::EgressTrap` around a real
// `inkentry` subprocess and fails loudly, naming the destination, if any
// call escapes it.
//
// ## Update-check (ADR-050) coverage: not yet implementable
//
// ADR-050 (`docs/adr/050-cli-auto-update-check.md`) designs an opt-out
// `api.github.com` update-notification check, but at the time this suite was
// written no code in the workspace implements it yet: there is no
// `INKENTRY_NO_UPDATE_CHECK`, no `UpdateConfig`, no `releases/latest` call
// site, no `state.toml`. `update_check_unimplemented_tripwire` below is a
// deliberate tripwire, not a behavioral test: it fails the moment someone
// adds the feature, which is the cue to replace it with real coverage of
// D2/D3 (trigger cadence, opt-out precedence, silent-offline swallow). Until
// then, every `zero_egress` test below already proves the CLI makes no
// `api.github.com` call in practice (it is simply one destination among
// "any non-loopback host", all of which are caught identically).
//
// ## embed_hub (Hugging Face download) coverage
//
// `embed_hub`/`hf-hub` live only in `inkentry-server` behind the optional
// `embed-native` feature; `inkentry-cli`'s production `[dependencies]` do not
// depend on `inkentry-server` at all (only a `[dev-dependencies]` entry with
// `default-features = false`, used by unrelated relay tests). So "CLI local
// flows never trigger embed_hub" is a compile-time property of the
// dependency graph, not just a runtime observation:
// `embed_hub_unreachable_from_cli_binary` below asserts that graph shape
// directly so a future Cargo.toml change that pulls `hf-hub` into the
// `inkentry` binary fails CI immediately. The first-run download itself
// (only reaches `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF` on the default HF
// endpoint) already has pinning coverage in
// `crates/inkentry-server/src/embed_hub.rs`'s `prequantized_gguf_repo_*`
// tests; this suite does not duplicate a live download against the real HF
// host (network-dependent and slow, ~339 MB, not appropriate for this
// harness).

use crate::egress_trap;
use crate::plumbing_helpers;

use egress_trap::{EgressTrap, loopback_discovery_port};
use plumbing_helpers::{init_git_repo, inkentry_bin_in, mount_health, mount_index_embed};
use predicates::prelude::*;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Failsafe only: a hung child would otherwise block the whole suite forever.
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);

fn ensure_sqlite_vec() {
    use std::sync::OnceLock;
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

fn write_project(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).expect("create src dir");
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn greet(name: &str) -> String {\n    format!(\"hello, {name}\")\n}\n\
         pub fn farewell(name: &str) -> String {\n    format!(\"bye, {name}\")\n}\n",
    )
    .expect("write lib.rs");
}

// `POST /v1/projects/{id}/search`: the endpoint `inkentry search --mode
// semantic|hybrid` uses to embed the query server-side (`search_query` in
// `server_client.rs`); distinct from `/index/embed` (`embed_text`), which
// `memory search`/`memory add`/`plumbing embed` use instead.
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

// Build a `inkentry` command isolated from ambient `INKENTRY_*` env, wired
// against `project` as CWD and `state_dir` for loopback auto-discovery.
// A command that never parsed cannot have leaked, so a clean trap over one
// proves nothing. Argument-parsing failures are the way these invocations go
// stale: a renamed or removed subcommand still exits, still touches no socket,
// and still passes every egress assertion.
fn assert_ran(out: &std::process::Output, label: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    for marker in [
        "unrecognized subcommand",
        "unexpected argument",
        "invalid value",
    ] {
        assert!(
            !stderr.contains(marker),
            "`plumbing {label}` did not parse ({marker}), so its clean trap says \
             nothing about egress: {stderr}"
        );
    }
}

fn local_tier_cmd(home: &Path, project: &Path, state_dir: &Path) -> assert_cmd::Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.current_dir(project)
        .timeout(CHILD_TIMEOUT)
        .env_remove("INKENTRY_SERVER_URL")
        .env_remove("INKENTRY_MODE")
        .env_remove("INKENTRY_PROJECT_ID")
        .env_remove("INKENTRY_NO_SERVER")
        .env("INKENTRY_STATE_DIR", state_dir);
    cmd
}

// [`local_tier_cmd`] with loopback auto-discovery pointed at a mock inference
// server on `url`, for the commands that are supposed to find one. The rest of
// this file keeps `local_tier_cmd`, whose `inkentry_bin_in` already disables
// discovery outright.
fn loopback_tier_cmd(
    home: &Path,
    project: &Path,
    state_dir: &Path,
    url: &str,
) -> assert_cmd::Command {
    let mut cmd = local_tier_cmd(home, project, state_dir);
    cmd.env("INKENTRY_TEST_DISCOVERY_PORT", loopback_discovery_port(url));
    cmd
}

// How many requests the mock inference server has served whose path ends with
// `suffix`.
//
// Every test here asserts an absence (nothing reached the egress trap), which a
// run that did nothing at all satisfies just as well as a run that stayed
// local. Counting what the sanctioned loopback server actually served is what
// separates the two.
async fn served(server: &MockServer, suffix: &str) -> usize {
    server
        .received_requests()
        .await
        .expect("wiremock request journaling must stay enabled")
        .iter()
        .filter(|r| r.url.path().ends_with(suffix))
        .count()
}

// ── init ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn init_zero_egress() {
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    let state_dir = TempDir::new().expect("state dir");
    init_git_repo(project.path());
    write_project(project.path());

    let trap = EgressTrap::start().await;
    let mut cmd = local_tier_cmd(home.path(), project.path(), state_dir.path());
    trap.wire(&mut cmd);
    cmd.arg("init").arg("--no-index");
    cmd.assert().success();

    trap.assert_clean().await;
}

// ── index (loopback auto-discovery) ─────────────────────────────────────

#[tokio::test]
async fn index_local_tier_zero_egress() {
    ensure_sqlite_vec();
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    let state_dir = TempDir::new().expect("state dir");
    init_git_repo(project.path());
    write_project(project.path());

    let inference = MockServer::start().await;
    mount_health(&inference).await;
    mount_index_embed(&inference).await;

    let trap = EgressTrap::start().await;
    let mut cmd = loopback_tier_cmd(
        home.path(),
        project.path(),
        state_dir.path(),
        &inference.uri(),
    );
    trap.wire(&mut cmd);
    cmd.arg("index").arg(".");
    cmd.assert().success();

    assert!(
        served(&inference, "/index/embed").await > 0,
        "the index never embedded anything, so a clean trap says only that \
         nothing ran"
    );
    trap.assert_clean().await;
}

// ── search --mode text (no server needed at all) ───────────────────────

#[tokio::test]
async fn search_text_mode_zero_egress() {
    ensure_sqlite_vec();
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    let state_dir = TempDir::new().expect("state dir");
    init_git_repo(project.path());
    write_project(project.path());

    // Index once (loopback-backed) so there is something to search; this
    // setup phase runs *outside* the trap on purpose: only the command
    // actually under test is wired.
    {
        let inference = MockServer::start().await;
        mount_health(&inference).await;
        mount_index_embed(&inference).await;
        loopback_tier_cmd(
            home.path(),
            project.path(),
            state_dir.path(),
            &inference.uri(),
        )
        .arg("index")
        .arg(".")
        .assert()
        .success();
        assert!(
            served(&inference, "/index/embed").await > 0,
            "the corpus the search under test reads was never embedded, so a \
             clean trap below would say only that nothing ran"
        );
    }

    // No inference server at all for the search itself: text mode must not
    // need one, and a stray loopback probe (default port 4655, nothing
    // listening) must fail closed locally, never touch the trap.
    let empty_state_dir = TempDir::new().expect("empty state dir");
    let trap = EgressTrap::start().await;
    let mut cmd = local_tier_cmd(home.path(), project.path(), empty_state_dir.path());
    trap.wire(&mut cmd);
    cmd.arg("search").arg("greet").arg("--only-text");
    cmd.assert().success();

    trap.assert_clean().await;
}

// ── semantic search via loopback server ─────────────────────────────────

#[tokio::test]
async fn search_semantic_zero_egress() {
    ensure_sqlite_vec();
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    let state_dir = TempDir::new().expect("state dir");
    init_git_repo(project.path());
    write_project(project.path());

    let inference = MockServer::start().await;
    mount_health(&inference).await;
    mount_index_embed(&inference).await;
    mount_search(&inference).await;

    loopback_tier_cmd(
        home.path(),
        project.path(),
        state_dir.path(),
        &inference.uri(),
    )
    .arg("index")
    .arg(".")
    .assert()
    .success();

    let trap = EgressTrap::start().await;
    let mut cmd = loopback_tier_cmd(
        home.path(),
        project.path(),
        state_dir.path(),
        &inference.uri(),
    );
    trap.wire(&mut cmd);
    // Default best-available ranking over the code corpus: the query embed goes
    // to the loopback server, never off-box. --only-code keeps this to the one
    // code-prefix embed the test is about.
    cmd.arg("search").arg("greet").arg("--only-code");
    cmd.assert().success();

    assert!(
        served(&inference, "/search").await > 0,
        "the query was never embedded against the loopback server, so this \
         proves only that a search which did nothing sent nothing"
    );
    trap.assert_clean().await;
}

// ── memory add / list (pure local) ──────────────────────────────────────

#[tokio::test]
async fn memory_add_list_zero_egress() {
    ensure_sqlite_vec();
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    let state_dir = TempDir::new().expect("state dir");
    init_git_repo(project.path());
    write_project(project.path());

    local_tier_cmd(home.path(), project.path(), state_dir.path())
        .arg("init")
        .arg("--no-index")
        .assert()
        .success();

    let trap = EgressTrap::start().await;

    let mut add_cmd = local_tier_cmd(home.path(), project.path(), state_dir.path());
    trap.wire(&mut add_cmd);
    add_cmd
        .arg("memory")
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("egress test note")
        .arg("--body")
        .arg("written by egress_containment.rs");
    add_cmd.assert().success();

    let mut list_cmd = local_tier_cmd(home.path(), project.path(), state_dir.path());
    trap.wire(&mut list_cmd);
    list_cmd.arg("memory").arg("list");
    list_cmd.assert().success();

    trap.assert_clean().await;
}

// ── memory-corpus search (hybrid, loopback-embedded query) ─────────────────────

#[tokio::test]
async fn memory_search_zero_egress() {
    ensure_sqlite_vec();
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    let state_dir = TempDir::new().expect("state dir");
    init_git_repo(project.path());
    write_project(project.path());

    let inference = MockServer::start().await;
    mount_health(&inference).await;
    mount_index_embed(&inference).await;

    loopback_tier_cmd(
        home.path(),
        project.path(),
        state_dir.path(),
        &inference.uri(),
    )
    .arg("init")
    .arg("--no-index")
    .assert()
    .success();
    loopback_tier_cmd(
        home.path(),
        project.path(),
        state_dir.path(),
        &inference.uri(),
    )
    .arg("memory")
    .arg("add")
    .arg("--kind")
    .arg("note")
    .arg("--title")
    .arg("egress test note")
    .arg("--body")
    .arg("written by egress_containment.rs")
    .assert()
    .success();

    let embeds_before = served(&inference, "/index/embed").await;
    let trap = EgressTrap::start().await;
    let mut cmd = loopback_tier_cmd(
        home.path(),
        project.path(),
        state_dir.path(),
        &inference.uri(),
    );
    trap.wire(&mut cmd);
    // Memory-only unified search: the QA query embed goes to the loopback server,
    // the note KNN runs locally, and no index.db is required for `--only-memory`.
    cmd.arg("search").arg("egress").arg("--only-memory");
    cmd.assert().success();

    assert!(
        served(&inference, "/index/embed").await > embeds_before,
        "the search embedded no query against the loopback server, so a clean \
         trap says only that nothing ran"
    );
    trap.assert_clean().await;
}

// ── graph edges (index-backed, local reads) ─────────────────────────────

#[tokio::test]
async fn graph_edges_zero_egress() {
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    let state_dir = TempDir::new().expect("state dir");
    init_git_repo(project.path());
    write_project(project.path());
    // A real caller so the extracted call graph has an edge into `greet`.
    std::fs::write(
        project.path().join("src").join("caller.rs"),
        "pub fn call_it() -> String {\n    greet(\"world\")\n}\n",
    )
    .expect("write caller.rs");

    // Index offline (no loopback state ⇒ no server): parsing extracts the call
    // graph without embedding, which is all `plumbing graph-edges` needs.
    local_tier_cmd(home.path(), project.path(), state_dir.path())
        .arg("index")
        .arg(".")
        .assert()
        .success();

    let trap = EgressTrap::start().await;
    let mut cmd = local_tier_cmd(home.path(), project.path(), state_dir.path());
    trap.wire(&mut cmd);
    // The graph capability's machine surface after the top-level `graph`
    // porcelain was removed: exact-edge lookup, JSONL, index-backed, local-only.
    cmd.args(["plumbing", "graph-edges", "--symbol", "greet"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("caller.rs"));

    trap.assert_clean().await;
}

// ── plumbing (local reads) ───────────────────────────────────────────────

#[tokio::test]
async fn plumbing_local_reads_zero_egress() {
    ensure_sqlite_vec();
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    let state_dir = TempDir::new().expect("state dir");
    init_git_repo(project.path());
    write_project(project.path());

    let inference = MockServer::start().await;
    mount_health(&inference).await;
    mount_index_embed(&inference).await;
    loopback_tier_cmd(
        home.path(),
        project.path(),
        state_dir.path(),
        &inference.uri(),
    )
    .arg("index")
    .arg(".")
    .assert()
    .success();

    let embeds_after_index = served(&inference, "/index/embed").await;
    assert!(
        embeds_after_index > 0,
        "the corpus these reads run over was never embedded, so a clean trap \
         would say only that nothing ran"
    );

    let db_path = project.path().join(".inkentry").join("index.db");
    let trap = EgressTrap::start().await;

    // `publish-notes` is excluded deliberately: it pushes `refs/notes/inkentry`
    // to a git remote, an explicit, expected-egress operation, not a
    // local-tier read this zero-egress claim covers.
    for args in [
        vec!["ls-files"],
        vec!["cat-chunks", "src/lib.rs"],
        vec!["hash-file", "src/lib.rs"],
        vec!["parse-file", "src/lib.rs"],
        vec!["graph-edges"],
        vec!["read-memory"],
    ] {
        let mut cmd = loopback_tier_cmd(
            home.path(),
            project.path(),
            state_dir.path(),
            &inference.uri(),
        );
        trap.wire(&mut cmd);
        cmd.arg("plumbing").arg("--db").arg(&db_path);
        for a in &args {
            cmd.arg(a);
        }
        // Exit code varies by subcommand semantics (e.g. `ls-files` exits 1
        // on an empty result), so the outcome is not asserted. What is
        // asserted is that the subcommand still parses: renaming one would
        // otherwise leave this green having run nothing at all.
        let out = cmd.output().expect("run plumbing subcommand");
        assert_ran(&out, &args.join(" "));
    }

    // `knn` takes its query vector pre-embedded on stdin (unlike `embed`, it
    // never calls the inference server itself), so it gets its own
    // invocation instead of joining the no-stdin loop above.
    let mut knn_cmd = loopback_tier_cmd(
        home.path(),
        project.path(),
        state_dir.path(),
        &inference.uri(),
    );
    trap.wire(&mut knn_cmd);
    knn_cmd
        .arg("plumbing")
        .arg("--db")
        .arg(&db_path)
        .arg("knn")
        .write_stdin(serde_json::json!({"vector": vec![0.1f32; 896]}).to_string());
    // Exits 1 on an empty result set, same caveat as the loop above.
    let knn_out = knn_cmd.output().expect("run plumbing knn");
    assert_ran(&knn_out, "knn");

    // A discoverable loopback server was available to every one of those
    // commands and none of them used it: that is the claim, and it is not the
    // same claim as "no server was reachable".
    assert_eq!(
        served(&inference, "/index/embed").await,
        embeds_after_index,
        "a plumbing local read embedded against the inference server"
    );
    trap.assert_clean().await;
}

#[tokio::test]
async fn plumbing_embed_zero_egress() {
    ensure_sqlite_vec();
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    let state_dir = TempDir::new().expect("state dir");
    init_git_repo(project.path());
    write_project(project.path());

    let inference = MockServer::start().await;
    mount_health(&inference).await;
    mount_index_embed(&inference).await;
    let project_id = "test/plumbing-embed";
    loopback_tier_cmd(
        home.path(),
        project.path(),
        state_dir.path(),
        &inference.uri(),
    )
    .arg("init")
    .arg("--no-index")
    .arg("--name")
    .arg(project_id)
    .assert()
    .success();

    // `plumbing`'s dispatch (`cli/cmd/plumbing/mod.rs`) hands `embed_cmd` the
    // raw `Config` without first running the tier-probe/`effective_config`
    // bridge every other inference-calling command goes through (see
    // `capability::tier::Tier::effective_config`'s doc comment on why that
    // bridge exists), so unlike `search`/`memory search`, loopback
    // auto-discovery alone (`INKENTRY_STATE_DIR`) does not reach it; only an
    // explicit `server_url` under `cloud_first` does. That URL is still a
    // loopback address here (the same mock server as every other test in
    // this file uses), so the egress claim under test (zero non-loopback
    // connections) is identical; only the config knob differs.
    //
    // `write_project_server_config` overwrites the whole `config.toml`, so
    // the `project_id` `init --name` just wrote must be passed back in
    // explicitly or it's lost: `ServerInferenceClient` reads `cfg.project_id`
    // verbatim (no derive-from-git fallback; that only happens in the
    // `effective_config` bridge this code path skips), so a lost project_id
    // means an empty `{project_id}` URL segment and a 404, not an egress leak.
    plumbing_helpers::write_project_server_config(project.path(), &inference.uri(), project_id);

    let db_path = project.path().join(".inkentry").join("index.db");
    let embeds_before = served(&inference, "/index/embed").await;
    let trap = EgressTrap::start().await;
    let mut cmd = loopback_tier_cmd(
        home.path(),
        project.path(),
        state_dir.path(),
        &inference.uri(),
    );
    trap.wire(&mut cmd);
    cmd.env("INKENTRY_MODE", "cloud_first");
    // `plumbing embed` reads lines from stdin (`--query` only toggles which
    // F2LLM instruction prefix to apply), not a positional query arg.
    cmd.arg("plumbing")
        .arg("--db")
        .arg(&db_path)
        .arg("embed")
        .arg("--query")
        .write_stdin("hello world\n");
    cmd.assert().success();

    assert!(
        served(&inference, "/index/embed").await > embeds_before,
        "`plumbing embed` embedded nothing, so a clean trap says only that \
         nothing ran"
    );
    trap.assert_clean().await;
}

// ── ADR-050 update check: tripwire, see module doc ──────────────────────

#[test]
fn update_check_unimplemented_tripwire() {
    // Only production `src/` trees: walking `tests/` would trip on this
    // very file's doc comment naming these identifiers.
    let crates_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut hits = Vec::new();
    for needle in [
        "INKENTRY_NO_UPDATE_CHECK",
        "releases/latest",
        "UpdateConfig",
    ] {
        for crate_dir in [
            "inkentry-cli",
            "inkentry-core",
            "inkentry-server",
            "inkentry-embed",
        ] {
            let src = crates_root.join(crate_dir).join("src");
            walk_rs_files(&src, &mut |path, contents| {
                if contents.contains(needle) {
                    hits.push(format!("{needle} in {}", path.display()));
                }
            });
        }
    }
    assert!(
        hits.is_empty(),
        "ADR-050 update-check code has landed ({hits:?}); replace this tripwire with real \
         coverage of D2 (opt-out precedence: env > config > auto-detect), D3 (fires only when \
         due, silent + non-blocking when offline), per the story acceptance criteria",
    );
}

fn walk_rs_files(dir: &Path, f: &mut impl FnMut(&Path, &str)) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            walk_rs_files(&path, f);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs")
            && let Ok(contents) = std::fs::read_to_string(&path)
        {
            f(&path, &contents);
        }
    }
    true
}

// ── embed_hub: compile-time unreachable from the CLI binary ─────────────

#[test]
fn embed_hub_unreachable_from_cli_binary() {
    // `cargo tree --edges normal` walks the *production* dependency graph
    // (dev-dependencies excluded), which is exactly the graph the shipped
    // `inkentry` binary links. `hf-hub` lives only behind inkentry-server's
    // optional `embed-native` feature; asserting it is absent here is a
    // structural guarantee, not a runtime sample.
    let out = std::process::Command::new("cargo")
        .args([
            "tree",
            "-p",
            "inkentry-cli",
            "--edges",
            "normal",
            "--offline",
        ])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join(".."))
        .output()
        .expect("run cargo tree");
    assert!(
        out.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tree = String::from_utf8_lossy(&out.stdout);
    assert!(
        !tree.contains("hf-hub"),
        "hf-hub is reachable from the inkentry-cli production dependency graph: \
         embed_hub's Hugging Face download path must stay confined to inkentry-server's \
         embed-native feature, never linked into the CLI binary local-tier commands run in.\n{tree}",
    );
}

// ── self-test: prove the trap actually catches a rogue call ─────────────
//
// Critical per the story: a harness that only ever asserts "clean" proves
// nothing about its own ability to detect a violation. This drives a real
// `reqwest::Client` (the same HTTP stack every local-tier command uses)
// against a rogue non-loopback host under the identical proxy-env wiring
// `EgressTrap::wire` applies to a subprocess, and asserts the trap names the
// destination. `#[serial]` because, unlike every other test in this file,
// this one mutates process-global env directly instead of scoping the env
// to a spawned child via `Command::env` (there is no "child process" to
// scope to when proving the mechanism itself, only the test process).
#[tokio::test]
#[serial_test::serial]
async fn self_test_trap_catches_rogue_call() {
    let trap = EgressTrap::start().await;
    let proxy = trap.proxy_url();
    let proxy_vars = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ];
    let no_proxy_vars = ["NO_PROXY", "no_proxy"];
    // SAFETY: `#[serial]` guarantees no other test in this binary reads or
    // sets process env concurrently.
    unsafe {
        for var in proxy_vars {
            std::env::set_var(var, &proxy);
        }
        for var in no_proxy_vars {
            std::env::set_var(var, "127.0.0.1,localhost,::1");
        }
    }

    let client = reqwest::Client::builder()
        .build()
        .expect("build client with env-derived proxy config");
    let rogue_call = client
        .get("https://example.invalid/telemetry")
        .timeout(Duration::from_secs(5))
        .send()
        .await;

    // SAFETY: same justification as above.
    unsafe {
        for var in proxy_vars.into_iter().chain(no_proxy_vars) {
            std::env::remove_var(var);
        }
    }

    let seen = trap.destinations_seen().await;
    assert!(
        seen.iter().any(|d| d.contains("example.invalid")),
        "self-test failed: the egress trap did not catch a deliberate rogue call to \
         example.invalid (rogue_call result: {rogue_call:?}); the harness cannot be trusted \
         to catch a real regression. Seen: {seen:?}",
    );
}
