// Credential-store scoping across `inkentry logout`, `inkentry logout --org`,
// and `inkentry auth remove-key`: each form must touch exactly the intended
// credential(s) and leave the others intact.
//
// Since ADR-074 the cloud session no longer lives in `config.toml`: it is one
// entry per organization in the secret store (here the file store, pinned via
// `INKENTRY_SECRET_STORE=file` by `inkentry_bin_in`), keyed by WorkOS org id.
// Per-origin self-hosted server keys live in the same store under their own
// entry. These tests seed both, run one form, and assert which entries changed
// and which survived — the scoping the older server-key-only tests never made,
// which let a server-key removal silently wipe the cloud pair.
//
// Each assertion spawns the real binary against an isolated `HOME` /
// `INKENTRY_CONFIG_DIR`, so nothing here reaches the developer's real config or
// the OS keychain.

use std::path::{Path, PathBuf};

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use predicates::prelude::*;
use tempfile::TempDir;

// Pipe `key` to `inkentry auth set-key --server <server>` over stdin (the only
// supported way to set a per-origin key). Writes the secret store.
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

fn secrets_path(home: &Path) -> PathBuf {
    home.join(".config").join("inkentry").join("secrets.toml")
}

// Seed two cached org sessions (org_a active) directly into the file store, the
// post-login shape `inkentry login` / `org switch` produce. Written as a TOML
// literal string so the JSON's double quotes need no escaping; any existing
// entries (e.g. server keys) are preserved.
fn seed_org_sessions(home: &Path) {
    let dir = home.join(".config").join("inkentry");
    std::fs::create_dir_all(&dir).expect("create config dir");
    let payload = serde_json::json!({
        "active": "org_a",
        "orgs": {
            "org_a": {
                "access_token": "at-a-secret",
                "refresh_token": "rt-a-secret",
                "expires_at": 4_000_000_000_i64,
                "cloud_origin": "https://api.inkentry.com",
                "slug": "acme",
            },
            "org_b": {
                "access_token": "at-b-secret",
                "refresh_token": "rt-b-secret",
                "expires_at": 4_000_000_000_i64,
                "cloud_origin": "https://api.inkentry.com",
                "slug": "beta",
            },
        }
    })
    .to_string();
    let path = secrets_path(home);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, format!("org_tokens = '{payload}'\n{existing}"))
        .expect("seed org_tokens into secrets.toml");
}

fn secrets_toml(home: &Path) -> String {
    std::fs::read_to_string(secrets_path(home)).unwrap_or_default()
}

// `org list` prints each cached org and marks the active one, and never any
// token material.
#[test]
fn org_list_shows_cached_orgs_and_never_token_material() {
    let home = TempDir::new().unwrap();
    seed_org_sessions(home.path());

    let out = inkentry_bin_in(home.path())
        .arg("org")
        .arg("list")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();

    assert!(
        text.contains("acme"),
        "org list must show org_a's slug:\n{text}"
    );
    assert!(
        text.contains("beta"),
        "org list must show org_b's slug:\n{text}"
    );
    assert!(
        text.contains('*'),
        "org list must mark the active org:\n{text}"
    );
    for secret in ["at-a-secret", "rt-a-secret", "at-b-secret", "rt-b-secret"] {
        assert!(
            !text.contains(secret),
            "org list leaked token material ({secret}):\n{text}"
        );
    }
}

// `logout --org <target>` clears only that org's session, leaving every other
// cached org and every server key intact.
#[test]
fn logout_one_org_clears_only_that_session() {
    let home = TempDir::new().unwrap();
    seed_org_sessions(home.path());
    set_key(home.path(), "https://a.example:4655", "sk-a");

    inkentry_bin_in(home.path())
        .arg("logout")
        .arg("--org")
        .arg("org_a")
        .assert()
        .success();

    let secrets = secrets_toml(home.path());
    assert!(
        !secrets.contains("at-a-secret"),
        "the targeted org's session must be cleared:\n{secrets}"
    );
    assert!(
        secrets.contains("at-b-secret"),
        "a sibling org's session must survive:\n{secrets}"
    );
    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .stdout(predicate::str::contains("a.example"));
}

// Bare `logout` clears every cached org session. Every stored server key must
// survive.
#[test]
fn bare_logout_clears_all_org_sessions_and_keeps_server_keys() {
    let home = TempDir::new().unwrap();
    seed_org_sessions(home.path());
    set_key(home.path(), "https://a.example:4655", "sk-a");
    set_key(home.path(), "https://b.example:4655", "sk-b");

    inkentry_bin_in(home.path())
        .arg("logout")
        .assert()
        .success();

    let secrets = secrets_toml(home.path());
    assert!(
        !secrets.contains("at-a-secret") && !secrets.contains("at-b-secret"),
        "every cached cloud session must be cleared by bare `logout`:\n{secrets}"
    );

    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .stdout(predicate::str::contains("a.example"))
        .stdout(predicate::str::contains("b.example"));
}

// ADR-090 D6: `auth remove-key --all-servers` clears every server key and
// leaves the cloud sessions intact.
#[test]
fn remove_key_all_servers_clears_every_server_key_and_keeps_cloud_sessions() {
    let home = TempDir::new().unwrap();
    seed_org_sessions(home.path());
    set_key(home.path(), "https://a.example:4655", "sk-a");
    set_key(home.path(), "https://b.example:4655", "sk-b");

    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("remove-key")
        .arg("--all-servers")
        .assert()
        .success();

    let secrets = secrets_toml(home.path());
    assert!(
        secrets.contains("at-a-secret") && secrets.contains("at-b-secret"),
        "cloud sessions must survive `auth remove-key --all-servers`:\n{secrets}"
    );

    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .stdout(predicate::str::contains("No server keys stored"));
}

// The same obligation for the single-origin form.
#[test]
fn remove_key_server_clears_that_origin_only_and_keeps_cloud_sessions() {
    let home = TempDir::new().unwrap();
    seed_org_sessions(home.path());
    set_key(home.path(), "https://a.example:4655", "sk-a");
    set_key(home.path(), "https://b.example:4655", "sk-b");

    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("remove-key")
        .arg("--server")
        .arg("https://a.example:4655")
        .assert()
        .success();

    let secrets = secrets_toml(home.path());
    assert!(
        secrets.contains("at-a-secret") && secrets.contains("at-b-secret"),
        "cloud sessions must survive `auth remove-key --server`:\n{secrets}"
    );

    let out = inkentry_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        !text.contains("a.example"),
        "removed origin still listed:\n{text}"
    );
    assert!(
        text.contains("b.example"),
        "untouched origin missing:\n{text}"
    );
}
