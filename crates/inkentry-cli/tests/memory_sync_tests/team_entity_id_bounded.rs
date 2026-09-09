// What a team server says when it cannot look far enough back.
//
// The team listing takes a limit but no offset or cursor, so a handle lookup
// reads one page. When that page comes back full the store holds more, and the
// entries beyond it are the oldest ones, since the listing is newest first.
// Those are exactly the entries a long-lived document quotes, so answering
// "no such entry" there would deny one that was never looked for.

use crate::plumbing_helpers;
use plumbing_helpers::{TEAM_PROJECT_SLUG, inkentry_bin_in, mount_team_health, write_team_config};

use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Matches the client's own bound, so a listing of this size is indistinguishable
// from one that was cut off.
const SCAN_LIMIT: usize = 1_000;

fn team_note(n: usize) -> serde_json::Value {
    serde_json::json!({
        "id": format!("{n:08x}-0000-7000-8000-000000000000"),
        "kind": "decision",
        "title": format!("entry {n}"),
        "body": "b",
        "tags": [],
        "linked_files": [],
        "created_at": 1_700_000_000,
        "status": "active",
        "superseded_by": null,
    })
}

async fn mount_listing(server: &MockServer, count: usize) {
    let entries: Vec<serde_json::Value> = (0..count).map(team_note).collect();
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{TEAM_PROJECT_SLUG}/memory")))
        .respond_with(ResponseTemplate::new(200).set_body_json(entries))
        .mount(server)
        .await;
    // The handle is not a server id, so the per-entry route never resolves it.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({})))
        .mount(server)
        .await;
}

async fn show_absent_handle(count: usize) -> (bool, String) {
    let server = MockServer::start().await;
    mount_team_health(&server).await;
    mount_listing(&server, count).await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let config_path = write_team_config(proj.path(), &server.uri());

    // A well-formed entity id the mounted listing does not contain.
    let absent = inkentry_core::storage::entity_id("decision", "never on this server", "b");
    let out = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .env("INKENTRY_MODE", "cloud_first")
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "show", &absent])
        .output()
        .expect("run memory show");

    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[tokio::test]
async fn a_truncated_listing_is_not_reported_as_a_missing_entry() {
    let (ok, stderr) = show_absent_handle(SCAN_LIMIT).await;

    assert!(!ok, "the lookup did not succeed, so it must not exit 0");
    assert!(
        !stderr.contains("No memory entry with id"),
        "a bounded read must not be stated as a definite absence: {stderr}"
    );
    assert!(
        stderr.contains("not among the 1000 most recent memory entries"),
        "it must say how far it looked: {stderr}"
    );
    assert!(
        stderr.contains("--limit"),
        "and how to look further back: {stderr}"
    );
}

// The other half of the same rule: when the listing ends inside the page, the
// whole store was read and a miss is a fact worth stating plainly.
#[tokio::test]
async fn a_complete_listing_still_gives_the_plain_not_found() {
    let (ok, stderr) = show_absent_handle(3).await;

    assert!(!ok);
    assert!(
        stderr.contains("No memory entry with id"),
        "an exhaustive read that found nothing is a definite miss: {stderr}"
    );
    assert!(!stderr.contains("most recent memory entries"), "{stderr}");
}
