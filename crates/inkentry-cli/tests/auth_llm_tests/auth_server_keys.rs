use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use predicates::prelude::*;
use tempfile::TempDir;

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

#[test]
fn set_key_then_list_servers_shows_the_origin_not_the_secret() {
    let home = TempDir::new().unwrap();
    set_key(
        home.path(),
        "https://team.example:4655/ignored/path",
        "sk-team-secret",
    );

    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .stdout(predicate::str::contains("https://team.example:4655"))
        .stdout(predicate::str::contains("sk-team-secret").not());
}

#[test]
fn list_servers_with_nothing_stored_says_so() {
    let home = TempDir::new().unwrap();
    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .stdout(predicate::str::contains("No server keys stored"));
}

#[test]
fn set_key_rejects_empty_stdin() {
    let home = TempDir::new().unwrap();
    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("set-key")
        .arg("--server")
        .arg("https://team.example:4655")
        .write_stdin("")
        .assert()
        .failure();
}

#[test]
fn set_key_normalizes_origin_so_a_second_call_overwrites_not_duplicates() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://team.example:4655/a/b?x=1", "sk-1");
    set_key(home.path(), "https://team.example:4655/", "sk-2");

    let out = inkentry_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(
        text.lines().filter(|l| l.contains("team.example")).count(),
        1,
        "two URL forms of the same origin must collapse to one entry, got:\n{text}"
    );
}

#[test]
fn bare_logout_does_not_clear_stored_server_keys() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://team.example:4655", "sk-team-secret");

    inkentry_bin_in(home.path())
        .arg("logout")
        .assert()
        .success();

    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .stdout(predicate::str::contains("https://team.example:4655"));
}

#[test]
fn logout_no_longer_accepts_the_server_key_flags() {
    let home = TempDir::new().unwrap();
    inkentry_bin_in(home.path())
        .arg("logout")
        .arg("--servers")
        .assert()
        .failure();
    inkentry_bin_in(home.path())
        .arg("logout")
        .arg("--server")
        .arg("https://a.example:4655")
        .assert()
        .failure();
}

#[test]
fn bare_logout_names_auth_remove_key_when_server_keys_remain() {
    let home = TempDir::new().unwrap();
    set_key(home.path(), "https://team.example:4655", "sk-team-secret");

    inkentry_bin_in(home.path())
        .arg("logout")
        .assert()
        .success()
        .stdout(predicate::str::contains("inkentry auth remove-key"))
        .stdout(predicate::str::contains("logout --servers").not());
}
