// The only outbound connections a local-tier command may make are to the auto-discovered
// loopback inference server. Every test wires `egress_trap::EgressTrap` around a real
// `inkentry` subprocess and fails loudly, naming the destination, if any call escapes it.

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

// A command that never parsed cannot have leaked: a renamed or removed subcommand still exits,
// touches no socket, and passes every egress assertion.
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

// Points loopback auto-discovery at a mock inference server; the rest of the file keeps
// `local_tier_cmd`, which disables discovery.
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

// Every test asserts an absence, which a run that did nothing satisfies as well as one that
// stayed local; counting what the sanctioned loopback server served separates the two.
async fn served(server: &MockServer, suffix: &str) -> usize {
    server
        .received_requests()
        .await
        .expect("wiremock request journaling must stay enabled")
        .iter()
        .filter(|r| r.url.path().ends_with(suffix))
        .count()
}

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

#[tokio::test]
async fn search_text_mode_zero_egress() {
    ensure_sqlite_vec();
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    let state_dir = TempDir::new().expect("state dir");
    init_git_repo(project.path());
    write_project(project.path());

    // Setup runs outside the trap on purpose: only the command under test is wired.
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

    // No inference server for the search itself: text mode must not need one, and a stray
    // loopback probe (default port 4655) must fail closed locally.
    let empty_state_dir = TempDir::new().expect("empty state dir");
    let trap = EgressTrap::start().await;
    let mut cmd = local_tier_cmd(home.path(), project.path(), empty_state_dir.path());
    trap.wire(&mut cmd);
    cmd.arg("search").arg("greet").arg("--only-text");
    cmd.assert().success();

    trap.assert_clean().await;
}

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
    // `--only-code` keeps this to the one code-prefix embed the test is about.
    cmd.arg("search").arg("greet").arg("--only-code");
    cmd.assert().success();

    assert!(
        served(&inference, "/search").await > 0,
        "the query was never embedded against the loopback server, so this \
         proves only that a search which did nothing sent nothing"
    );
    trap.assert_clean().await;
}

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

#[tokio::test]
async fn event_recording_zero_egress() {
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

    // A full caller declaration so the recording path runs rather than the undeclared no-op.
    let mut add_cmd = local_tier_cmd(home.path(), project.path(), state_dir.path());
    trap.wire(&mut add_cmd);
    add_cmd
        .env("INKENTRY_TRIGGER", "explicit")
        .env("INKENTRY_ACTOR", "agent")
        .env("INKENTRY_SESSION_REF", "egress-test-session")
        .env("INKENTRY_TOOL", "claude-code")
        .env("INKENTRY_MODEL", "claude-sonnet-5")
        .arg("memory")
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("egress test note")
        .arg("--body")
        .arg("written by egress_containment.rs");
    add_cmd.assert().success();

    let mut context_cmd = local_tier_cmd(home.path(), project.path(), state_dir.path());
    trap.wire(&mut context_cmd);
    context_cmd
        .env("INKENTRY_TRIGGER", "hook")
        .env("INKENTRY_ACTOR", "agent")
        .env("INKENTRY_SESSION_REF", "egress-test-session")
        .arg("context");
    context_cmd.assert().success();

    trap.assert_clean().await;
}

#[tokio::test]
async fn metrics_snapshot_zero_egress() {
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

    let mut add_cmd = local_tier_cmd(home.path(), project.path(), state_dir.path());
    add_cmd
        .arg("memory")
        .arg("add")
        .arg("--kind")
        .arg("decision")
        .arg("--title")
        .arg("egress test decision")
        .arg("--body")
        .arg("written by egress_containment.rs");
    add_cmd.assert().success();

    let trap = EgressTrap::start().await;

    let mut snapshot_cmd = local_tier_cmd(home.path(), project.path(), state_dir.path());
    trap.wire(&mut snapshot_cmd);
    snapshot_cmd.arg("metrics").arg("snapshot").arg("--json");
    snapshot_cmd.assert().success();

    trap.assert_clean().await;
}

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
    // The QA query embed goes to the loopback server; the note KNN runs locally.
    cmd.arg("search").arg("egress").arg("--only-memory");
    cmd.assert().success();

    assert!(
        served(&inference, "/index/embed").await > embeds_before,
        "the search embedded no query against the loopback server, so a clean \
         trap says only that nothing ran"
    );
    trap.assert_clean().await;
}

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

    // Index offline: parsing extracts the call graph without embedding, which is all
    // `plumbing graph-edges` needs.
    local_tier_cmd(home.path(), project.path(), state_dir.path())
        .arg("index")
        .arg(".")
        .assert()
        .success();

    let trap = EgressTrap::start().await;
    let mut cmd = local_tier_cmd(home.path(), project.path(), state_dir.path());
    trap.wire(&mut cmd);
    cmd.args(["plumbing", "graph-edges", "--symbol", "greet"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("caller.rs"));

    trap.assert_clean().await;
}

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

    // `publish-notes` is excluded: it pushes `refs/notes/inkentry` to a git remote, which is
    // expected egress rather than a local-tier read.
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
        // Exit code varies by subcommand (`ls-files` exits 1 on empty), so only assert that it
        // still parses: renaming one would otherwise leave this green having run nothing.
        let out = cmd.output().expect("run plumbing subcommand");
        assert_ran(&out, &args.join(" "));
    }

    // `knn` takes its query vector pre-embedded on stdin and never calls the inference
    // server, so it gets its own invocation.
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

    // A discoverable loopback server was available to every command and none used it, which
    // is not the same claim as "no server was reachable".
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

    // `plumbing` hands `embed_cmd` the raw `Config` without the tier-probe bridge other
    // inference commands go through, so loopback auto-discovery alone does not reach it; only an
    // explicit `server_url` under `cloud_first` does (still loopback here, so the egress claim
    // is unchanged). `write_project_server_config` overwrites config.toml, so the `project_id`
    // from `init --name` must be passed back: it is read verbatim, and losing it gives a 404,
    // not an egress leak.
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
    // `plumbing embed` reads lines from stdin (`--query` only picks the instruction prefix).
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

#[test]
fn update_check_unimplemented_tripwire() {
    // Tripwire, not a behavioral test: fails the moment update-check code lands, the cue to
    // replace it with real coverage. Only production `src/` is walked, since this file names
    // the identifiers itself.
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

#[test]
fn embed_hub_unreachable_from_cli_binary() {
    // `cargo tree --edges normal` is the production graph the shipped binary links; `hf-hub`
    // lives only behind inkentry-server's optional `embed-llama` feature, so its absence is a
    // structural guarantee rather than a runtime sample.
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
         embed-llama feature, never linked into the CLI binary local-tier commands run in.\n{tree}",
    );
}

// A harness that only asserts "clean" cannot detect a violation: this drives a real
// `reqwest::Client` at a rogue non-loopback host under the same proxy-env wiring
// `EgressTrap::wire` applies, and asserts the trap names it. `#[serial]` because it mutates
// process-global env instead of scoping env to a spawned child.
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
