use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

#[test]
fn status_migrates_a_partial_auth_block() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("lib.rs"), "pub fn hi() {}\n").unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = temp.path().join("config.toml");

    // The secret store is pinned to a temp file so migration never reaches the
    // real keychain or config dir.
    fs::write(
        &config_path,
        format!("db_path = {:?}\n", db_path.display().to_string()),
    )
    .unwrap();
    inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
        .env("INKENTRY_SECRET_STORE", "file")
        .env("INKENTRY_CONFIG_DIR", temp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    fs::write(
        &config_path,
        format!(
            "db_path = {db:?}\n\
             \n\
             [auth]\n\
             access_token = \"at\"\n\
             refresh_token = \"rt\"\n",
            db = db_path.display().to_string(),
        ),
    )
    .unwrap();

    inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
        .env("INKENTRY_SECRET_STORE", "file")
        .env("INKENTRY_CONFIG_DIR", temp.path())
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stderr(predicate::str::contains("parsing config.toml").not());

    let cfg = fs::read_to_string(&config_path).unwrap();
    assert!(
        !cfg.contains("[auth]") && !cfg.contains("access_token"),
        "migration must strip the [auth] table:\n{cfg}"
    );
    let secrets = fs::read_to_string(temp.path().join("secrets.toml")).unwrap_or_default();
    assert!(
        secrets.contains("org_tokens") && secrets.contains("\"at\""),
        "migration must write the session into the secret store:\n{secrets}"
    );
}
