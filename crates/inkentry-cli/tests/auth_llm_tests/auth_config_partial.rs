// Integration coverage for the partial-`[auth]`-block tolerance fix.
//
// `[auth]` is login-managed, but `--org` is a documented optional scoping flag
// and hand-editing the config is a documented workflow. A global config whose
// `[auth]` table is missing a field (e.g. `org_id`, left out by a login without
// an org, or trimmed by hand) must not brick commands that need no credentials.
// Before the fix, `Config::load` failed on such a table and every command
// exited non-zero with a bare `Error: parsing config.toml`.

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

// `inkentry status` runs (exit 0) with an `[auth]` table missing `org_id` and
// `expires_at`, instead of failing with a config parse error.
#[test]
fn status_runs_with_partial_auth_block() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("lib.rs"), "pub fn hi() {}\n").unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = temp.path().join("config.toml");

    // Build the index first with a clean config so setup is not what we test.
    // `INKENTRY_NO_SERVER=1` forces offline: no embedding server is needed.
    fs::write(
        &config_path,
        format!("db_path = {:?}\n", db_path.display().to_string()),
    )
    .unwrap();
    inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
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
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stderr(predicate::str::contains("parsing config.toml").not());
}
