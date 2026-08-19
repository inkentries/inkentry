// `inkentry auth remove-key` (ADR-090): the removal half of the credential
// surface, spelled and discoverable next to the `set-key` that installed it.
//
// Drives the real binary against an isolated `HOME` (via `inkentry_bin_in`,
// `INKENTRY_SECRET_STORE=file`) so nothing here touches the developer's real
// `~/.config/inkentry` or the OS keychain, and so a key set by one process
// survives into the next assertion's separate spawn.

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use assert_cmd::assert::Assert;
use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn set_key(home: &Path, server: &str, key: &str) {
    inkentry_bin_in(home)
        .arg("auth")
        .arg("set-key")
        .arg("--server")
        .arg(server)
        .write_stdin(format!("{key}\n"))
        .assert()
        .success();
}

fn set_llm_key(home: &Path, key: &str) {
    inkentry_bin_in(home)
        .arg("auth")
        .arg("set-key")
        .arg("--llm")
        .write_stdin(format!("{key}\n"))
        .assert()
        .success();
}

fn remove_server_key(home: &Path, server: &str) -> Assert {
    inkentry_bin_in(home)
        .arg("auth")
        .arg("remove-key")
        .arg("--server")
        .arg(server)
        .assert()
}

fn list_servers(home: &Path) -> String {
    let out = inkentry_bin_in(home)
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

// Everything the file secret store holds, verbatim. D5's end state is a
// property of the stored blob, not of what `list-servers` chooses to print.
fn secrets_toml(home: &Path) -> String {
    std::fs::read_to_string(home.join(".config").join("inkentry").join("secrets.toml"))
        .unwrap_or_default()
}

// ── 1. one origin removed, the rest intact and still usable ────────────────

#[test]
fn remove_key_clears_one_origin_and_leaves_the_others_listed() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://a.example:4655", "sk-a");
    set_key(home.path(), "https://b.example:4655", "sk-b");
    set_key(home.path(), "https://c.example:4655", "sk-c");

    remove_server_key(home.path(), "https://b.example:4655").success();

    let text = list_servers(home.path());
    assert!(
        !text.contains("b.example"),
        "removed origin still listed:\n{text}"
    );
    assert!(
        text.contains("a.example"),
        "untouched origin missing:\n{text}"
    );
    assert!(
        text.contains("c.example"),
        "untouched origin missing:\n{text}"
    );
}

// "Intact" is not the same as "usable": a surviving map entry only matters if
// the surviving origin still authenticates. This checks the `Authorization`
// header a real request carries after the removal, one origin removed and one
// left, rather than trusting `list-servers`.
#[tokio::test]
async fn the_surviving_origins_key_still_reaches_the_wire_after_the_other_is_removed() {
    let removed_server = MockServer::start().await;
    let kept_server = MockServer::start().await;
    for server in [&removed_server, &kept_server] {
        Mock::given(method("GET"))
            .and(path_regex(r"^/v1/health$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "version": "test",
                "capabilities": ["memory"],
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/v1/projects/.+/memory/since$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"entries": []})),
            )
            .mount(server)
            .await;
    }

    let home = TempDir::new().unwrap();
    let cfg_dir = TempDir::new().unwrap();
    set_key(home.path(), &removed_server.uri(), "sk-removed-secret");
    set_key(home.path(), &kept_server.uri(), "sk-kept-secret");

    remove_server_key(home.path(), &removed_server.uri()).success();

    let config_path = cfg_dir.path().join("empty.toml");
    std::fs::write(&config_path, "").unwrap();
    let index_db = cfg_dir.path().join("index.db");
    for server in [&removed_server, &kept_server] {
        inkentry_bin_in(home.path())
            .env_remove("INKENTRY_SERVER_KEY")
            .env("INKENTRY_SERVER_URL", server.uri())
            .env("INKENTRY_PROJECT_ID", "test-org/test-project")
            .arg("--config")
            .arg(&config_path)
            .arg("plumbing")
            .arg("--db")
            .arg(&index_db)
            .arg("pull")
            .output()
            .unwrap();
    }

    let removed_requests = removed_server.received_requests().await.unwrap();
    let removed_since = removed_requests
        .iter()
        .find(|r| r.url.path().ends_with("/memory/since"))
        .expect("the removed origin received a /memory/since request");
    assert!(
        removed_since.headers.get("authorization").is_none(),
        "the removed origin must resolve to no bearer at all"
    );

    let kept_requests = kept_server.received_requests().await.unwrap();
    let kept_since = kept_requests
        .iter()
        .find(|r| r.url.path().ends_with("/memory/since"))
        .expect("the kept origin received a /memory/since request");
    assert_eq!(
        kept_since
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap()),
        Some("Bearer sk-kept-secret"),
        "removing one origin's key must leave the other's usable"
    );
}

// ── 2. the last key takes its stored entry with it ─────────────────────────

#[test]
fn removing_the_last_server_key_leaves_no_stored_entry_behind() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://only.example:4655", "sk-only");
    assert!(secrets_toml(home.path()).contains("server_keys"));

    remove_server_key(home.path(), "https://only.example:4655").success();

    let stored = secrets_toml(home.path());
    assert!(
        !stored.contains("server_keys"),
        "an emptied map must delete its entry, not store an empty object; secrets.toml was:\n{stored}"
    );
}

#[test]
fn remove_key_all_servers_leaves_no_stored_entry_behind() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://a.example:4655", "sk-a");
    set_key(home.path(), "https://b.example:4655", "sk-b");

    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("remove-key")
        .arg("--all-servers")
        .assert()
        .success();

    let stored = secrets_toml(home.path());
    assert!(
        !stored.contains("server_keys"),
        "`--all-servers` must delete the entry too; secrets.toml was:\n{stored}"
    );
    assert!(list_servers(home.path()).contains("No server keys stored"));
}

// `--all-servers` is spelled for what it clears: an LLM key is not a server
// key and must survive it.
#[test]
fn remove_key_all_servers_does_not_touch_the_llm_key() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://a.example:4655", "sk-a");
    set_llm_key(home.path(), "sk-llm-secret");

    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("remove-key")
        .arg("--all-servers")
        .assert()
        .success();

    assert!(
        secrets_toml(home.path()).contains("llm_key"),
        "`--all-servers` must leave the LLM credential alone"
    );
}

// ── 3. set-key and remove-key agree on the origin, form for form ───────────

#[test]
fn every_url_form_set_key_accepts_is_matched_by_remove_key() {
    let forms = [
        "https://team.example:4655",
        "https://team.example:4655/",
        "https://team.example:4655/a/b?x=1",
        "https://TEAM.Example:4655",
    ];
    for set_form in forms {
        for remove_form in forms {
            let home = TempDir::new().unwrap();
            set_key(home.path(), set_form, "sk-team-secret");

            remove_server_key(home.path(), remove_form)
                .success()
                .stdout(predicate::str::contains("Removed"));

            let text = list_servers(home.path());
            assert!(
                text.contains("No server keys stored"),
                "set as {set_form:?} then removed as {remove_form:?} left the key in place:\n{text}"
            );
        }
    }
}

#[test]
fn the_default_port_form_and_the_bare_form_are_the_same_origin_to_both_commands() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://port.example:443", "sk-port");

    remove_server_key(home.path(), "https://port.example")
        .success()
        .stdout(predicate::str::contains("Removed"));
    assert!(list_servers(home.path()).contains("No server keys stored"));
}

#[test]
fn a_different_port_is_a_different_origin_and_is_not_removed() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://port.example:8443", "sk-port");

    remove_server_key(home.path(), "https://port.example:9443")
        .success()
        .stdout(predicate::str::contains("Removed").not());
    assert!(list_servers(home.path()).contains("port.example:8443"));
}

// ── 4. absence is idempotent and is not reported as a removal ──────────────

#[test]
fn removing_an_absent_server_key_exits_zero_and_claims_no_removal() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://kept.example:4655", "sk-kept");

    for _ in 0..2 {
        remove_server_key(home.path(), "https://typo.example:4655")
            .success()
            .stdout(predicate::str::contains("No server key"))
            .stdout(predicate::str::contains("Removed").not());
    }

    assert!(list_servers(home.path()).contains("kept.example"));
}

#[test]
fn removing_the_same_server_key_twice_reports_a_removal_only_the_first_time() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://team.example:4655", "sk-team");

    remove_server_key(home.path(), "https://team.example:4655")
        .success()
        .stdout(predicate::str::contains("Removed"));
    remove_server_key(home.path(), "https://team.example:4655")
        .success()
        .stdout(predicate::str::contains("Removed").not());
}

#[test]
fn removing_an_absent_llm_key_exits_zero_and_claims_no_removal() {
    let home = TempDir::new().unwrap();
    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("remove-key")
        .arg("--llm")
        .assert()
        .success()
        .stdout(predicate::str::contains("No LLM key"))
        .stdout(predicate::str::contains("Removed").not());
}

#[test]
fn removing_an_absent_set_of_server_keys_exits_zero_and_claims_no_removal() {
    let home = TempDir::new().unwrap();
    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("remove-key")
        .arg("--all-servers")
        .assert()
        .success()
        .stdout(predicate::str::contains("No server keys"))
        .stdout(predicate::str::contains("Removed").not());
}

// ── the `--llm` capability ADR-090 D2 adds ─────────────────────────────────

#[test]
fn an_llm_key_can_be_set_and_then_removed() {
    let home = TempDir::new().unwrap();
    set_llm_key(home.path(), "sk-llm-secret");
    assert!(secrets_toml(home.path()).contains("llm_key"));

    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("remove-key")
        .arg("--llm")
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed"));

    let stored = secrets_toml(home.path());
    assert!(
        !stored.contains("llm_key"),
        "the LLM credential must be gone; secrets.toml was:\n{stored}"
    );
}

#[test]
fn removing_the_llm_key_does_not_touch_the_server_keys() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://a.example:4655", "sk-a");
    set_llm_key(home.path(), "sk-llm-secret");

    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("remove-key")
        .arg("--llm")
        .assert()
        .success();

    assert!(list_servers(home.path()).contains("a.example"));
}

// ── the flag group mirrors `set-key`'s ─────────────────────────────────────

#[test]
fn remove_key_requires_one_of_the_three_flags() {
    let home = TempDir::new().unwrap();
    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("remove-key")
        .assert()
        .failure();
}

#[test]
fn the_three_remove_key_flags_are_mutually_exclusive() {
    let home = TempDir::new().unwrap();
    let pairs = [
        vec!["--server", "https://a.example", "--llm"],
        vec!["--server", "https://a.example", "--all-servers"],
        vec!["--llm", "--all-servers"],
    ];
    for args in pairs {
        inkentry_bin_in(home.path())
            .arg("auth")
            .arg("remove-key")
            .args(&args)
            .assert()
            .failure();
    }
}

// The bulk flag is `--all-servers`, never a bare `--all`: in a command that
// can also address the LLM key, "all" does not say what it clears.
#[test]
fn remove_key_has_no_bare_all_flag() {
    let home = TempDir::new().unwrap();
    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("remove-key")
        .arg("--all")
        .assert()
        .failure();
}

#[test]
fn auth_help_lists_remove_key_alongside_set_key() {
    let home = TempDir::new().unwrap();
    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("set-key"))
        .stdout(predicate::str::contains("remove-key"))
        .stdout(predicate::str::contains("list-servers"));
}

// ── 5. no command in the family prints key material ────────────────────────

#[test]
fn no_command_in_the_family_prints_key_material_on_any_path() {
    let home = TempDir::new().unwrap();
    let server_secret = "sk-server-must-never-be-printed";
    let llm_secret = "sk-llm-must-never-be-printed";
    set_key(home.path(), "https://a.example:4655", server_secret);
    set_key(home.path(), "https://b.example:4655", server_secret);
    set_llm_key(home.path(), llm_secret);

    // Success paths, then the error paths: an unparsable URL, a missing flag,
    // two conflicting flags, and an empty stdin on the way in.
    let invocations: Vec<Vec<&str>> = vec![
        vec!["auth", "list-servers"],
        vec!["auth", "remove-key", "--server", "https://a.example:4655"],
        vec!["auth", "remove-key", "--server", "https://a.example:4655"],
        vec!["auth", "remove-key", "--server", "not a url"],
        vec!["auth", "remove-key"],
        vec!["auth", "remove-key", "--llm", "--all-servers"],
        vec!["auth", "remove-key", "--all"],
        vec!["auth", "set-key", "--server", "https://c.example:4655"],
        vec!["auth", "remove-key", "--llm"],
        vec!["auth", "remove-key", "--llm"],
        vec!["auth", "remove-key", "--all-servers"],
        vec!["auth", "remove-key", "--all-servers"],
        vec!["logout"],
    ];
    for args in invocations {
        let out = inkentry_bin_in(home.path())
            .args(&args)
            .write_stdin("")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        for secret in [server_secret, llm_secret] {
            assert!(
                !stdout.contains(secret) && !stderr.contains(secret),
                "`inkentry {}` printed key material:\nstdout:\n{stdout}\nstderr:\n{stderr}",
                args.join(" ")
            );
        }
    }
}
