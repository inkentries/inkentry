// Each logout / remove-key form must touch exactly the intended credentials and
// leave the others intact. Cloud sessions live one per org id in the secret
// store; server keys live under their own entry.

use std::path::{Path, PathBuf};

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use predicates::prelude::*;
use tempfile::TempDir;

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

// Written as a TOML literal string so the JSON's double quotes need no
// escaping; existing entries (e.g. server keys) are preserved.
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
