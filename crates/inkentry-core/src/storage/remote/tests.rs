use super::*;

fn backend(project_id: &str) -> RemoteMemoryBackend {
    RemoteMemoryBackend {
        client: reqwest::Client::new(),
        base_url: "http://127.0.0.1:4655".to_string(),
        project_id: project_id.to_string(),
        bearer: Bearer::fixed(None),
    }
}

// A slug can contain `/`, so it must be percent-encoded or axum splits the
// path on it and routes nothing into `{project_id}` (404).
#[test]
fn url_percent_encodes_local_fallback_slug() {
    let b = backend("local/9f2a8b3c4d5e6f70");
    assert_eq!(
        b.url("memory/search"),
        "http://127.0.0.1:4655/v1/projects/local%2F9f2a8b3c4d5e6f70/memory/search"
    );
}

#[test]
fn url_percent_encodes_github_remote_slug() {
    let b = backend("github.com/inkentries/inkentry");
    assert_eq!(
        b.url("memory"),
        "http://127.0.0.1:4655/v1/projects/github.com%2Finkentries%2Finkentry/memory"
    );
}

// The slug is the persistence key (`projects.slug` UNIQUE), so
// percent-decoding the encoded segment must yield it back exactly.
#[test]
fn encode_project_id_round_trips_through_percent_decode() {
    for slug in ["local/9f2a8b3c4d5e6f70", "github.com/inkentries/inkentry"] {
        let encoded = encode_project_id(slug);
        let decoded = percent_encoding::percent_decode_str(&encoded)
            .decode_utf8()
            .expect("valid UTF-8 after percent-decoding");
        assert_eq!(decoded, slug, "round-trip mismatch for slug {slug:?}");
    }
}

#[test]
fn url_leaves_simple_slug_unchanged() {
    let b = backend("my-project");
    assert_eq!(
        b.url("memory"),
        "http://127.0.0.1:4655/v1/projects/my-project/memory"
    );
}

// The real server embeds the query itself, so `POST .../memory/search`
// requires `{"query": <text>, "limit": <n>}`; the client must send that
// shape rather than a precomputed embedding.
#[tokio::test]
async fn search_sends_query_text_not_precomputed_embedding() {
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/projects/local%2Fabc123/memory/search"))
        .and(body_partial_json(
            serde_json::json!({ "query": "timezone" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let backend = RemoteMemoryBackend {
        client: reqwest::Client::new(),
        base_url: server.uri(),
        project_id: "local/abc123".to_string(),
        bearer: Bearer::fixed(None),
    };

    // `MemoryBackend::search` takes both a pre-computed query embedding blob
    // (used by local backends for KNN) and the raw query text; the remote
    // backend, which has no local embedder, ignores the blob and sends the
    // query text on the wire.
    let query_blob = crate::embeddings::vec_to_blob(&[0.1_f32, 0.2, 0.3]);
    let result = backend.search(&query_blob, "timezone", 3, None).await;

    assert!(
        result.is_ok(),
        "expected the server to accept the request body and return results, \
         got: {:?}\n\n\
         If this failed with a 422-shaped error, the client is still \
         sending `{{\"embedding\": [...], \"limit\": N}}` instead of the \
         `{{\"query\": \"<text>\", \"limit\": N}}` shape the real \
         inkentry-server requires — see spelunk-cloud/spelunk issue for \
         'memory search returns 422 against a real server (query/embedding \
         payload mismatch)'.",
        result.err().map(|e| e.to_string())
    );
}

// `GET /memory` only recognises `kind`, `limit`, `archived`, `offset`; axum's
// `Query` extractor silently drops anything else, including `as_of` and
// `source_ref` below. That makes `has_source_ref` decide whether a commit was
// already harvested purely from whether the (unfiltered) list came back
// non-empty, so the server answers "yes" for every commit once the project
// holds any memory at all.
#[tokio::test]
async fn list_sends_query_parameters_the_oss_server_silently_drops() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/local%2Fabc123/memory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let backend = RemoteMemoryBackend {
        client: reqwest::Client::new(),
        base_url: server.uri(),
        project_id: "local/abc123".to_string(),
        bearer: Bearer::fixed(None),
    };

    backend
        .list(None, 5, false, Some(1_700_000_000))
        .await
        .expect("list must reach the mock");
    backend
        .list_by_source_ref("deadbeefcafe", 1, true, None)
        .await
        .expect("list_by_source_ref must reach the mock");

    let queries: Vec<String> = server
        .received_requests()
        .await
        .expect("mock server records requests")
        .iter()
        .map(|r| r.url.query().unwrap_or_default().to_string())
        .collect();

    let accepted_by_the_server = ["kind", "limit", "archived"];
    let sent: Vec<&str> = queries
        .iter()
        .flat_map(|q| q.split('&'))
        .filter_map(|pair| pair.split('=').next())
        .filter(|name| !accepted_by_the_server.contains(name))
        .collect();

    assert!(
        sent.contains(&"as_of"),
        "expected `list` to still be sending the unsupported `as_of` parameter; \
         if it stopped, delete this test. Sent: {sent:?}"
    );
    assert!(
        sent.contains(&"source_ref"),
        "expected `list_by_source_ref` to still be sending the unsupported \
         `source_ref` parameter; if it stopped, delete this test. Sent: {sent:?}"
    );
}

// The read endpoints must accept both shapes a team server can send: the
// object envelope (`{entries, total}` / `{shas}`), and the bare array an
// older server still returns. Accepting both keeps a newer CLI working
// against an older team server.

fn note_json(title: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e01",
        "kind": "decision",
        "title": title,
        "body": "b",
        "tags": [],
        "linked_files": [],
        "created_at": 0,
        "status": "active",
        "superseded_by": null,
    })
}

fn backend_at(uri: String) -> RemoteMemoryBackend {
    RemoteMemoryBackend {
        client: reqwest::Client::new(),
        base_url: uri,
        project_id: "proj".to_string(),
        bearer: Bearer::fixed(None),
    }
}

#[tokio::test]
async fn list_accepts_object_envelope_from_newer_server() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [note_json("A")],
            "total": 1,
        })))
        .mount(&server)
        .await;

    let notes = backend_at(server.uri())
        .list(None, 10, false, None)
        .await
        .expect("list must parse the object envelope");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "A");
}

#[tokio::test]
async fn list_accepts_bare_array_from_older_server() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([note_json("A")])))
        .mount(&server)
        .await;

    let notes = backend_at(server.uri())
        .list(None, 10, false, None)
        .await
        .expect("list must still parse a legacy bare array");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "A");
}

#[tokio::test]
async fn search_accepts_object_envelope_from_newer_server() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [note_json("hit")],
            "total": 1,
        })))
        .mount(&server)
        .await;

    let query_blob = crate::embeddings::vec_to_blob(&[0.1_f32, 0.2, 0.3]);
    let notes = backend_at(server.uri())
        .search(&query_blob, "q", 5, None)
        .await
        .expect("search must parse the object envelope");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "hit");
}

#[tokio::test]
async fn harvested_shas_accepts_both_shapes() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let enveloped = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/harvested-shas"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "shas": ["abc"] })),
        )
        .mount(&enveloped)
        .await;
    let shas = backend_at(enveloped.uri())
        .harvested_shas()
        .await
        .expect("harvested_shas must parse the object envelope");
    assert!(shas.contains("abc"), "got: {shas:?}");

    let bare = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/harvested-shas"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!(["def"])))
        .mount(&bare)
        .await;
    let shas = backend_at(bare.uri())
        .harvested_shas()
        .await
        .expect("harvested_shas must still parse a legacy bare array");
    assert!(shas.contains("def"), "got: {shas:?}");
}

#[tokio::test]
async fn add_retries_a_shed_429_instead_of_failing_the_write() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // A full server-side embed admission queue sheds `POST /memory` with a
    // 429 that clears once the in-flight embed finishes; `memory add` must
    // ride that out rather than reporting a failed write. `Retry-After: 0`
    // keeps the test's real sleep at zero.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e01",
            "conflicts": [],
        })))
        .mount(&server)
        .await;

    let (id, _) = backend_at(server.uri())
        .add(NoteInput {
            kind: "decision".into(),
            title: "t".into(),
            body: "b".into(),
            tags: vec![],
            linked_files: vec![],
            embedding: None,
            source_ref: None,
            valid_at: None,
            supersedes: None,
            origin: None,
        })
        .await
        .expect("a transient 429 must not fail the write");
    assert_eq!(id.as_str(), "0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e01");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

// A rejected credential must name the fix rather than surface a bare 401.
#[tokio::test]
async fn a_rejected_credential_names_the_set_key_command() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/stats"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    // `{:#}` is how the CLI renders an error (main.rs), so it is what the user
    // actually reads.
    let err = format!(
        "{:#}",
        backend_at(server.uri())
            .count()
            .await
            .expect_err("a 401 must fail the call")
    );
    assert!(
        err.contains(&format!("inkentry auth set-key --server {}", server.uri())),
        "must name the fix, got: {err}"
    );
}

// A loopback address with nothing listening: bind an ephemeral port, read it
// back, then drop the listener. Connecting there is refused immediately, so a
// test exercises the refused path without spending wall-clock time.
fn closed_loopback_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().expect("read the bound port").port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

fn note_input(title: &str) -> NoteInput {
    NoteInput {
        kind: "note".to_string(),
        title: title.to_string(),
        body: "b".to_string(),
        tags: vec![],
        linked_files: vec![],
        embedding: None,
        source_ref: None,
        valid_at: None,
        supersedes: None,
        origin: None,
    }
}

#[tokio::test]
async fn a_refused_connection_on_the_write_path_names_the_server_unreachable() {
    let base_url = closed_loopback_url();
    let err = backend_at(base_url.clone())
        .add(note_input("t"))
        .await
        .expect_err("a write to a closed port must fail");

    // The headline is what a user reads first, so the diagnosis belongs there
    // rather than under the route label.
    assert_eq!(
        err.to_string(),
        format!(
            "team server unreachable at {base_url} (connection refused); mode is cloud_first, \
             which does not fall back to the local store"
        )
    );
    // The route and the transport error stay in the chain for diagnosis.
    assert!(
        format!("{err:#}").contains("POST /memory"),
        "the chain must still name the failed request, got: {err:#}"
    );
}

#[tokio::test]
async fn a_refused_connection_on_the_read_path_names_the_server_unreachable() {
    let base_url = closed_loopback_url();
    let err = backend_at(base_url.clone())
        .list(None, 10, false, None)
        .await
        .expect_err("a read from a closed port must fail");

    assert_eq!(
        err.to_string(),
        format!(
            "team server unreachable at {base_url} (connection refused); mode is cloud_first, \
             which does not fall back to the local store"
        )
    );
    assert!(
        format!("{err:#}").contains("GET /memory"),
        "the chain must still name the failed request, got: {err:#}"
    );
}

#[tokio::test]
async fn a_slow_but_connected_server_is_not_reported_as_unreachable() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // The server accepts the connection and then dawdles past the request
    // budget. That is a slow server, not an absent one, so it must keep the
    // wording it had and never claim the server is unreachable.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"notes": []}))
                .set_delay(std::time::Duration::from_millis(400)),
        )
        .mount(&server)
        .await;

    let backend = RemoteMemoryBackend {
        client: reqwest::Client::builder()
            .connect_timeout(crate::config::REMOTE_CONNECT_TIMEOUT)
            .timeout(std::time::Duration::from_millis(50))
            .build()
            .expect("build a client with a collapsed request budget"),
        base_url: server.uri(),
        project_id: "proj".to_string(),
        bearer: Bearer::fixed(None),
    };

    let err = backend
        .list(None, 10, false, None)
        .await
        .expect_err("a response delayed past the request budget must fail");
    assert_eq!(err.to_string(), "GET /memory");
    let chain = format!("{err:#}");
    assert!(
        !chain.contains("unreachable"),
        "a connected server must never be reported as unreachable, got: {chain}"
    );
}

// The team server's listing pages by offset, so this backend walks it to
// exhaustion to resolve a handle, reading every page until one comes back empty.

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

// Serves the listing the way the team server does: honouring `offset`, and
// capping a page far below what the client asks — as the real server caps at
// 500 — so a handful of entries still spans several pages and a page past the
// end is empty, which is what ends the client's walk.
struct PagedTeamListing {
    entries: Vec<serde_json::Value>,
}

impl wiremock::Respond for PagedTeamListing {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let q: std::collections::HashMap<String, String> =
            request.url.query_pairs().into_owned().collect();
        let offset: usize = q.get("offset").and_then(|s| s.parse().ok()).unwrap_or(0);
        let page = 2;
        let slice: Vec<_> = self
            .entries
            .iter()
            .skip(offset)
            .take(page)
            .cloned()
            .collect();
        wiremock::ResponseTemplate::new(200).set_body_json(slice)
    }
}

async fn team_backend_listing(
    count: usize,
    last_title: &str,
) -> (wiremock::MockServer, RemoteMemoryBackend) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    let mut entries: Vec<serde_json::Value> = (0..count.saturating_sub(1))
        .map(|n| team_note(n, &format!("filler {n}")))
        .collect();
    entries.push(team_note(count, last_title));
    Mock::given(method("GET"))
        .and(path("/v1/projects/team/memory"))
        .respond_with(PagedTeamListing { entries })
        .mount(&server)
        .await;

    let backend = RemoteMemoryBackend {
        client: reqwest::Client::new(),
        base_url: server.uri(),
        project_id: "team".to_string(),
        bearer: Bearer::fixed(None),
    };
    (server, backend)
}

// A store larger than one page is walked to the end rather than cut off, so
// the team backend reports a complete result, never a bounded one: an absent
// handle is a definite empty result.
#[tokio::test]
async fn a_multi_page_store_is_walked_to_a_complete_result() {
    let (_server, backend) = team_backend_listing(5, "newest").await;

    let absent = crate::storage::entity_id("decision", "an entry this server never held", "b");
    let lookup = backend
        .note_ids_for_entity_id_prefix(&absent)
        .await
        .unwrap();

    assert_eq!(lookup, EntityIdLookup::Complete(vec![]));
}

// The target sits on the last page (the mock caps a page at 2, so five entries
// span three), so resolving it proves the walk advanced the offset past the
// first page instead of reading one page and stopping.
#[tokio::test]
async fn a_handle_on_a_later_page_resolves() {
    let (_server, backend) = team_backend_listing(5, "findable").await;

    let target = crate::storage::entity_id("decision", "findable", "b");
    let lookup = backend
        .note_ids_for_entity_id_prefix(&target[..12])
        .await
        .unwrap();

    assert_eq!(
        lookup,
        EntityIdLookup::Complete(vec![
            "00000005-0000-7000-8000-000000000000".parse().unwrap()
        ])
    );
}

// A team server one release behind has no `offset` on its list route and drops
// the unknown parameter, so it answers every request with the same page. Serves
// that: a fixed listing, ignoring offset entirely.
async fn team_backend_ignoring_offset(count: usize) -> (wiremock::MockServer, RemoteMemoryBackend) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let entries: Vec<serde_json::Value> = (0..count)
        .map(|n| team_note(n, &format!("filler {n}")))
        .collect();
    Mock::given(method("GET"))
        .and(path("/v1/projects/team/memory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(entries))
        .mount(&server)
        .await;

    let backend = RemoteMemoryBackend {
        client: reqwest::Client::new(),
        base_url: server.uri(),
        project_id: "team".to_string(),
        bearer: Bearer::fixed(None),
    };
    (server, backend)
}

// A peer that silently drops `offset` returns the same page forever; the walk
// must notice it is not advancing and report the read bounded rather than loop.
// That this test terminates at all is the core of what it proves.
#[tokio::test]
async fn a_peer_that_ignores_offset_is_reported_bounded_not_walked_forever() {
    let (_server, backend) = team_backend_ignoring_offset(10).await;

    let absent = crate::storage::entity_id("decision", "never held here", "b");
    let lookup = backend
        .note_ids_for_entity_id_prefix(&absent)
        .await
        .unwrap();

    match lookup {
        EntityIdLookup::Bounded { matches, examined } => {
            assert!(matches.is_empty(), "the absent handle matched nothing");
            assert_eq!(examined, 10, "it reports how far it actually read");
        }
        EntityIdLookup::Complete(_) => {
            panic!("a peer that never advances must not be reported as a complete read")
        }
    }
}
