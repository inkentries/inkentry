// Resolving a memory handle against a self-hosted team server.
//
// The team listing pages by offset, so the CLI walks the whole project to
// resolve a quoted handle: it reads every entry, however old, not just one
// page. A handle held anywhere resolves to its entry; one held nowhere is a
// plain not-found, never the old "looked only so far" hedge.

use crate::plumbing_helpers;
use plumbing_helpers::{TEAM_PROJECT_SLUG, inkentry_bin_in, mount_team_health, write_team_config};

use std::collections::HashMap;
use tempfile::TempDir;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn team_note(n: usize, title: &str) -> serde_json::Value {
    serde_json::json!({
        "id": format!("{n:08x}-0000-7000-8000-000000000000"),
        "kind": "decision",
        "title": title,
        "body": "b",
        "tags": [],
        "linked_files": [],
        "created_at": 1_700_000_000,
        "status": "active",
        "superseded_by": null,
    })
}

fn entity_id_of(title: &str) -> String {
    inkentry_core::storage::entity_id("decision", title, "b")
}

// A team memory store the CLI can walk. The list route honours `limit`/`offset`
// and caps a page at 500 as the real server does, so the walk sees the same
// short-page arithmetic and a page past the end is empty; the per-entry route
// serves an entry by its `id`, or 404s. One responder, so there is no
// mock-ordering to reason about.
struct TeamStore {
    entries: Vec<serde_json::Value>,
}

impl Respond for TeamStore {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let list_path = format!("/v1/projects/{TEAM_PROJECT_SLUG}/memory");
        let path = request.url.path().to_string();
        if path == list_path {
            let q: HashMap<String, String> = request.url.query_pairs().into_owned().collect();
            let offset: usize = q.get("offset").and_then(|s| s.parse().ok()).unwrap_or(0);
            let limit: usize = q.get("limit").and_then(|s| s.parse().ok()).unwrap_or(20);
            let page = limit.min(500);
            let slice: Vec<_> = self
                .entries
                .iter()
                .skip(offset)
                .take(page)
                .cloned()
                .collect();
            return ResponseTemplate::new(200).set_body_json(slice);
        }
        if let Some(id) = path.strip_prefix(&format!("{list_path}/"))
            && let Some(entry) = self.entries.iter().find(|e| e["id"] == id)
        {
            return ResponseTemplate::new(200).set_body_json(entry);
        }
        ResponseTemplate::new(404).set_body_json(serde_json::json!({}))
    }
}

async fn show_against(entries: Vec<serde_json::Value>, token: &str) -> std::process::Output {
    let server = MockServer::start().await;
    mount_team_health(&server).await;
    Mock::given(method("GET"))
        .and(path_regex(format!(
            r"^/v1/projects/{TEAM_PROJECT_SLUG}/memory"
        )))
        .respond_with(TeamStore { entries })
        .mount(&server)
        .await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let config_path = write_team_config(proj.path(), &server.uri());

    inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .env("INKENTRY_MODE", "cloud_first")
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "show", token])
        .output()
        .expect("run memory show")
}

// More entries than one 500-entry page, so a handle only on a later page proves
// the walk pages past the first.
const PAGES_WORTH: usize = 1_200;

fn filler(count: usize) -> Vec<serde_json::Value> {
    (0..count)
        .map(|n| team_note(n, &format!("entry {n}")))
        .collect()
}

#[tokio::test]
async fn a_handle_on_a_later_page_resolves() {
    let target = "the entry to find";
    let mut entries = filler(PAGES_WORTH);
    // Index 900 sits past the first page (0..500), so only a walk that requested
    // a non-zero offset ever reads it.
    entries[900] = team_note(900, target);

    let out = show_against(entries, &entity_id_of(target)).await;
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "a handle on a later page must resolve: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(target),
        "the resolved entry is the target: {stdout}"
    );
}

#[tokio::test]
async fn an_absent_handle_on_a_large_store_is_a_plain_not_found() {
    let absent = entity_id_of("never on this server");

    let out = show_against(filler(PAGES_WORTH), &absent).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("No memory entry with id"),
        "an exhausted walk that found nothing is a definite miss: {stderr}"
    );
    assert!(
        !stderr.contains("most recent memory entries"),
        "the bounded-lookup hedge is gone for a team server: {stderr}"
    );
}

#[tokio::test]
async fn an_ambiguous_handle_across_pages_is_refused() {
    // Two entries with identical content share one entity id; seeded on
    // different pages, so catching the clash requires having walked both.
    let twin = "same content twice";
    let mut entries = filler(PAGES_WORTH);
    entries[100] = team_note(100, twin);
    entries[900] = team_note(900, twin);

    let out = show_against(entries, &entity_id_of(twin)).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("matches 2 memory entries"),
        "an ambiguous handle spanning two pages is refused, naming the count: {stderr}"
    );
}
