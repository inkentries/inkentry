// Integration coverage for tolerant migration of a partial legacy `[auth]`
// block (ADR-074).
//
// Hand-editing the config is a documented workflow and a login without an org
// leaves `org_id` empty, so a legacy `[auth]` table missing a field must
// migrate into the secret store rather than brick commands that need no
// credentials. Before this tolerance, `Config::load` failed on such a table and
// every command exited non-zero with a bare `Error: parsing config.toml`.

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

// `inkentry status` runs (exit 0) with a legacy `[auth]` table missing `org_id`
// and `expires_at`, migrating it into the secret store and stripping it from the
// config, instead of failing with a config parse error.
#[test]
fn status_migrates_a_partial_auth_block() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("lib.rs"), "pub fn hi() {}\n").unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = temp.path().join("config.toml");

    // Build the index first with a clean config so setup is not what we test.
    // `INKENTRY_NO_SERVER=1` forces offline: no embedding server is needed. The
    // secret store is pinned to a temp file store so migration never reaches the
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

    // Now rewrite the same global config with an `[auth]` table that is missing
    // `org_id` and `expires_at` — the exact shape a login-without-org or a
    // hand-trimmed file produces.
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

    // Migration moved the session into the file store and stripped `[auth]`.
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
