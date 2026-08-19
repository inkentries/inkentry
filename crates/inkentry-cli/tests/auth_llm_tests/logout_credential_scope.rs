// Credential-store scoping across `inkentry logout` and `inkentry auth
// remove-key`: each form must touch exactly one credential store and leave the
// other intact.
//
// The cloud token pair lives in the `[auth]` table of
// `~/.config/inkentry/config.toml`; per-origin self-hosted server keys live in
// the secret store (here the file store, pinned via `INKENTRY_SECRET_STORE=file`
// by `inkentry_bin_in`). These tests seed BOTH stores, run one form, and assert
// which store changed and which survived: the assertion the older
// server-key-only tests never made, which let a server-key removal silently
// wipe the cloud pair.
//
// Each assertion spawns the real binary against an isolated `HOME` /
// `INKENTRY_CONFIG_DIR`, so nothing here reaches the developer's real config or
// the OS keychain.

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use predicates::prelude::*;
use tempfile::TempDir;

// Pipe `key` to `inkentry auth set-key --server <server>` over stdin (the only
// supported way to set a per-origin key). Writes the secret store, not config.toml.
fn set_key(home: &std::path::Path, server: &str, key: &str) {
    inkentry_bin_in(home)
        .arg("auth")
        .arg("set-key")
        .arg("--server")
        .arg(server)
        .write_stdin(format!("{key}\n"))
        .assert()
        .success();
}

// Seed a complete cloud `[auth]` token pair into `config.toml` directly: the
// same on-disk shape `inkentry login` writes. `INKENTRY_CONFIG_DIR` (set by
// `inkentry_bin_in`) resolves to `<home>/.config/inkentry`, so this is exactly
// where the CLI reads and (on bare logout) rewrites it.
fn seed_cloud_auth(home: &std::path::Path) {
    let dir = home.join(".config").join("inkentry");
    std::fs::create_dir_all(&dir).expect("create config dir");
    std::fs::write(
        dir.join("config.toml"),
        "[auth]\n\
         access_token = \"at-cloud-secret\"\n\
         refresh_token = \"rt-cloud-secret\"\n\
         expires_at = 4000000000\n\
         org_id = \"org_test\"\n",
    )
    .expect("seed [auth] into config.toml");
}

// Current text of the seeded `config.toml` (empty string once the file has been
// rewritten with no `[auth]` table).
fn config_toml(home: &std::path::Path) -> String {
    std::fs::read_to_string(home.join(".config").join("inkentry").join("config.toml"))
        .unwrap_or_default()
}

// Bare `logout` (no flags): clears only the cloud `[auth]` pair. Every stored
// server key must survive.
#[test]
fn bare_logout_clears_cloud_pair_only_and_keeps_server_keys() {
    let home = TempDir::new().unwrap();
    seed_cloud_auth(home.path());
    set_key(home.path(), "https://a.example:4655", "sk-a");
    set_key(home.path(), "https://b.example:4655", "sk-b");

    inkentry_bin_in(home.path())
        .arg("logout")
        .assert()
        .success();

    // Cloud pair removed.
    let cfg = config_toml(home.path());
    assert!(
        !cfg.contains("access_token") && !cfg.contains("refresh_token"),
        "cloud [auth] pair must be cleared by bare `logout`, config.toml was:\n{cfg}"
    );

    // Both server keys survive.
    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .stdout(predicate::str::contains("a.example"))
        .stdout(predicate::str::contains("b.example"));
}

// ADR-090 D6: `auth remove-key --all-servers` replaces `logout --servers`,
// and inherits the scoping obligation: it clears every server key and leaves
// the cloud `[auth]` pair intact.
#[test]
fn remove_key_all_servers_clears_every_server_key_and_keeps_cloud_pair() {
    let home = TempDir::new().unwrap();
    seed_cloud_auth(home.path());
    set_key(home.path(), "https://a.example:4655", "sk-a");
    set_key(home.path(), "https://b.example:4655", "sk-b");

    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("remove-key")
        .arg("--all-servers")
        .assert()
        .success();

    let cfg = config_toml(home.path());
    assert!(
        cfg.contains("access_token") && cfg.contains("refresh_token"),
        "cloud [auth] pair must survive `auth remove-key --all-servers`, config.toml was:\n{cfg}"
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
fn remove_key_server_clears_that_origin_only_and_keeps_cloud_pair() {
    let home = TempDir::new().unwrap();
    seed_cloud_auth(home.path());
    set_key(home.path(), "https://a.example:4655", "sk-a");
    set_key(home.path(), "https://b.example:4655", "sk-b");

    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("remove-key")
        .arg("--server")
        .arg("https://a.example:4655")
        .assert()
        .success();

    let cfg = config_toml(home.path());
    assert!(
        cfg.contains("access_token") && cfg.contains("refresh_token"),
        "cloud [auth] pair must survive `auth remove-key --server`, config.toml was:\n{cfg}"
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
