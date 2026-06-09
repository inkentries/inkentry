use anyhow::{Context, Result};
use async_trait::async_trait;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use super::backend::{MemoryBackend, NoteInput};
use super::memory::{MemoryEdge, Note};
use crate::embeddings::blob_to_vec;

/// Characters that must be percent-encoded inside a single URL **path segment**.
///
/// `derive_project_id` produces slugs that contain `/` (`local/<blake3-hex>`,
/// `github.com/owner/repo`). Inserted raw into `/v1/projects/{project_id}/…`
/// the slashes split the segment and break axum routing (→ 404). We percent-encode
/// the slug so the whole slug occupies exactly one captured `{project_id}` segment;
/// axum percent-decodes it back to the original slug server-side, so the
/// persistence key (`projects.slug`, UNIQUE) is unchanged. See spelunk decision #106.
///
/// Mirrors `PROJECT_ID_SEGMENT` / `encode_project_id` in
/// `spelunk-cli/src/server_client.rs` — duplicated here because spelunk-core
/// cannot depend on spelunk-cli.
const PROJECT_ID_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/')
    .add(b'%');

/// Percent-encode a `project_id` slug for safe use as a single URL path segment.
///
/// Only the segment is encoded (not the surrounding URL); `/` → `%2F` etc.
fn encode_project_id(project_id: &str) -> String {
    utf8_percent_encode(project_id, PROJECT_ID_SEGMENT).to_string()
}

/// HTTP client for the spelunk-server REST API.
///
/// All routes are scoped under `/v1/projects/{project_id}/`.
pub struct RemoteMemoryBackend {
    pub client: reqwest::Client,
    pub base_url: String,
    pub project_id: String,
    pub api_key: Option<String>,
}

impl RemoteMemoryBackend {
    fn url(&self, path: &str) -> String {
        // Percent-encode the project_id path segment: slugs contain `/`
        // (`local/<hex>`, `github.com/owner/repo`) which would otherwise split
        // the segment and break axum routing → 404. See spelunk decision #106.
        format!(
            "{}/v1/projects/{}/{}",
            self.base_url.trim_end_matches('/'),
            encode_project_id(&self.project_id),
            path
        )
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(key) = &self.api_key {
            req.header("Authorization", format!("Bearer {key}"))
        } else {
            req
        }
    }
}

// ── Wire types (match server JSON schema) ─────────────────────────────────────

#[derive(Serialize)]
struct AddNoteRequest {
    kind: String,
    title: String,
    body: String,
    tags: Vec<String>,
    linked_files: Vec<String>,
    embedding: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_at: Option<i64>,
}

#[derive(Deserialize)]
struct AddNoteResponse {
    id: i64,
    #[serde(default)]
    conflicts: Vec<ConflictInfo>,
}

/// Conflict information returned by the server when a new note is semantically
/// close to an existing active entry (HTTP 409).
#[derive(Debug, Deserialize, Clone)]
pub struct ConflictInfo {
    pub id: i64,
    pub title: String,
    pub similarity: f32,
}

#[derive(Deserialize)]
struct NoteResponse {
    id: i64,
    kind: String,
    title: String,
    body: String,
    tags: Vec<String>,
    linked_files: Vec<String>,
    created_at: i64,
    status: String,
    superseded_by: Option<i64>,
    #[serde(default)]
    source_ref: Option<String>,
    #[serde(default)]
    valid_at: Option<i64>,
    #[serde(default)]
    invalid_at: Option<i64>,
    #[serde(default)]
    distance: Option<f64>,
}

impl From<NoteResponse> for Note {
    fn from(r: NoteResponse) -> Self {
        Note {
            id: r.id,
            kind: r.kind,
            title: r.title,
            body: r.body,
            tags: r.tags,
            linked_files: r.linked_files,
            created_at: r.created_at,
            status: r.status,
            superseded_by: r.superseded_by,
            source_ref: r.source_ref,
            valid_at: r.valid_at,
            invalid_at: r.invalid_at,
            distance: r.distance,
            score: None,
        }
    }
}

/// Matches `spelunk-server::handlers::SearchRequest` — the server embeds
/// `query` server-side (it has no way to accept a pre-computed client-side
/// embedding), so the remote backend must always send the raw query text.
#[derive(Serialize)]
struct SearchRequest {
    query: String,
    limit: usize,
}

#[derive(Serialize)]
struct SupersedeRequest {
    new_id: i64,
}

#[derive(Deserialize)]
struct BoolResponse {
    changed: bool,
}

#[derive(Deserialize)]
struct CountResponse {
    count: i64,
}

// ── Trait implementation ──────────────────────────────────────────────────────

#[async_trait]
impl MemoryBackend for RemoteMemoryBackend {
    async fn add(&self, input: NoteInput) -> Result<i64> {
        let embedding = input.embedding.as_deref().map(blob_to_vec);
        let body = AddNoteRequest {
            kind: input.kind,
            title: input.title,
            body: input.body,
            tags: input.tags,
            linked_files: input.linked_files,
            embedding,
            source_ref: input.source_ref,
            valid_at: input.valid_at,
        };
        let http_resp = self
            .authed(self.client.post(self.url("memory")))
            .json(&body)
            .send()
            .await
            .context("POST /memory")?;

        let status = http_resp.status();

        // 409 means "stored but conflicting" — treat as success but emit a warning.
        if status == reqwest::StatusCode::CONFLICT {
            let resp = http_resp
                .json::<AddNoteResponse>()
                .await
                .context("parsing POST /memory 409 response")?;
            if !resp.conflicts.is_empty() {
                eprintln!("warning: memory entry conflicts with existing entries:");
                for c in &resp.conflicts {
                    eprintln!(
                        "  · #{} \"{}\" (similarity: {:.2})",
                        c.id, c.title, c.similarity
                    );
                }
            }
            return Ok(resp.id);
        }

        let resp = http_resp
            .error_for_status()
            .context("server returned error for POST /memory")?
            .json::<AddNoteResponse>()
            .await
            .context("parsing POST /memory response")?;
        Ok(resp.id)
    }

    /// Remote backend: timeline search falls back to regular semantic search.
    async fn search_timeline(
        &self,
        query_blob: &[u8],
        query: &str,
        limit: usize,
    ) -> Result<Vec<Note>> {
        self.search(query_blob, query, limit, None).await
    }

    /// The server has no native embedder client-side hook — it embeds `query`
    /// server-side (see `spelunk-server::handlers::search_notes`). The
    /// pre-computed `query_blob` is what local backends use for KNN; the
    /// remote backend ignores it and sends the raw query text instead, or the
    /// server's required `query: String` field is missing and axum rejects
    /// the request with 422 before the handler ever runs (spelunk#359).
    async fn search(
        &self,
        _query_blob: &[u8],
        query: &str,
        limit: usize,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        let body = SearchRequest {
            query: query.to_string(),
            limit,
        };
        let resp = self
            .authed(self.client.post(self.url("memory/search")))
            .json(&body)
            .send()
            .await
            .context("POST /memory/search")?
            .error_for_status()
            .context("server returned error for POST /memory/search")?
            .json::<Vec<NoteResponse>>()
            .await
            .context("parsing search response")?;
        Ok(resp.into_iter().map(Into::into).collect())
    }

    /// Remote backend: BM25 text search is not supported — falls back to semantic search.
    async fn search_text(
        &self,
        _query: &str,
        _limit: usize,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        anyhow::bail!(
            "BM25 text search is not supported by the remote memory backend. \
             Use --mode semantic or omit --mode to use the default hybrid mode."
        )
    }

    /// Remote backend: hybrid search falls back to semantic search
    /// (server-side FTS is not available in this client).
    async fn search_hybrid(
        &self,
        query_blob: &[u8],
        query: &str,
        limit: usize,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        self.search(query_blob, query, limit, as_of).await
    }

    async fn list(
        &self,
        kind_filter: Option<&str>,
        limit: usize,
        include_archived: bool,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        let mut req = self.client.get(self.url("memory")).query(&[
            ("limit", limit.to_string().as_str()),
            ("archived", if include_archived { "true" } else { "false" }),
        ]);
        if let Some(kind) = kind_filter {
            req = req.query(&[("kind", kind)]);
        }
        if let Some(ts) = as_of {
            req = req.query(&[("as_of", ts.to_string().as_str())]);
        }
        let resp = self
            .authed(req)
            .send()
            .await
            .context("GET /memory")?
            .error_for_status()
            .context("server returned error for GET /memory")?
            .json::<Vec<NoteResponse>>()
            .await
            .context("parsing list response")?;
        Ok(resp.into_iter().map(Into::into).collect())
    }

    async fn get(&self, id: i64) -> Result<Option<Note>> {
        let resp = self
            .authed(self.client.get(self.url(&format!("memory/{id}"))))
            .send()
            .await
            .context("GET /memory/{id}")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let note = resp
            .error_for_status()
            .context("server returned error for GET /memory/{id}")?
            .json::<NoteResponse>()
            .await
            .context("parsing get response")?;
        Ok(Some(note.into()))
    }

    async fn count(&self) -> Result<i64> {
        let resp = self
            .authed(self.client.get(self.url("stats")))
            .send()
            .await
            .context("GET /stats")?
            .error_for_status()
            .context("server returned error for GET /stats")?
            .json::<CountResponse>()
            .await
            .context("parsing stats response")?;
        Ok(resp.count)
    }

    async fn archive(&self, id: i64) -> Result<bool> {
        let resp = self
            .authed(self.client.post(self.url(&format!("memory/{id}/archive"))))
            .send()
            .await
            .context("POST /memory/{id}/archive")?
            .error_for_status()
            .context("server returned error for POST /memory/{id}/archive")?
            .json::<BoolResponse>()
            .await
            .context("parsing archive response")?;
        Ok(resp.changed)
    }

    async fn supersede(&self, old_id: i64, new_id: i64) -> Result<bool> {
        let body = SupersedeRequest { new_id };
        let resp = self
            .authed(
                self.client
                    .post(self.url(&format!("memory/{old_id}/supersede"))),
            )
            .json(&body)
            .send()
            .await
            .context("POST /memory/{id}/supersede")?
            .error_for_status()
            .context("server returned error for POST /memory/{id}/supersede")?
            .json::<BoolResponse>()
            .await
            .context("parsing supersede response")?;
        Ok(resp.changed)
    }

    async fn list_by_source_ref(
        &self,
        source_ref_prefix: &str,
        limit: usize,
        include_archived: bool,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        let req = self.client.get(self.url("memory")).query(&[
            ("limit", limit.to_string().as_str()),
            ("archived", if include_archived { "true" } else { "false" }),
            ("source_ref", source_ref_prefix),
        ]);
        let resp = self
            .authed(req)
            .send()
            .await
            .context("GET /memory (source_ref filter)")?
            .error_for_status()
            .context("server returned error for GET /memory")?
            .json::<Vec<NoteResponse>>()
            .await
            .context("parsing list response")?;
        Ok(resp.into_iter().map(Into::into).collect())
    }

    async fn harvested_shas(&self) -> Result<HashSet<String>> {
        let resp = self
            .authed(self.client.get(self.url("memory/harvested-shas")))
            .send()
            .await
            .context("GET /memory/harvested-shas")?
            .error_for_status()
            .context("server returned error for GET /memory/harvested-shas")?
            .json::<Vec<String>>()
            .await
            .context("parsing harvested-shas response")?;
        Ok(resp.into_iter().collect())
    }

    async fn has_source_ref(&self, sha: &str) -> Result<bool> {
        // Reuse the list endpoint with the full SHA as prefix; if any results come back,
        // this commit has been harvested.
        let notes = self.list_by_source_ref(sha, 1, true, None).await?;
        Ok(!notes.is_empty())
    }

    /// Remote backend: edge mutations are not supported — no-op.
    async fn add_edge(&self, _from_id: i64, _to_id: i64, _kind: &str) -> Result<()> {
        Ok(())
    }

    /// Remote backend: edge queries are not supported — returns empty lists.
    async fn get_edges(&self, _id: i64) -> Result<(Vec<MemoryEdge>, Vec<MemoryEdge>)> {
        Ok((vec![], vec![]))
    }

    fn backend_kind(&self) -> &'static str {
        "remote"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(project_id: &str) -> RemoteMemoryBackend {
        RemoteMemoryBackend {
            client: reqwest::Client::new(),
            base_url: "http://127.0.0.1:7777".to_string(),
            project_id: project_id.to_string(),
            api_key: None,
        }
    }

    /// `derive_local_fallback` / `normalise_git_url` slugs contain `/`; the
    /// segment must be percent-encoded so axum routes the whole slug into
    /// `{project_id}` instead of splitting on `/` (→ 404). See spelunk
    /// decision #106 (mirrors IMP-1's fix in spelunk-cli/server_client.rs).
    #[test]
    fn url_percent_encodes_local_fallback_slug() {
        let b = backend("local/9f2a8b3c4d5e6f70");
        assert_eq!(
            b.url("memory/search"),
            "http://127.0.0.1:7777/v1/projects/local%2F9f2a8b3c4d5e6f70/memory/search"
        );
    }

    #[test]
    fn url_percent_encodes_github_remote_slug() {
        let b = backend("github.com/spelunk-cloud/spelunk");
        assert_eq!(
            b.url("memory"),
            "http://127.0.0.1:7777/v1/projects/github.com%2Fspelunk-cloud%2Fspelunk/memory"
        );
    }

    /// Round-trip: percent-decoding the encoded segment must yield the
    /// original slug, since the slug is the persistence key
    /// (`projects.slug` UNIQUE) and must reach `require_project`/
    /// `upsert_project` exactly as `derive_project_id` produced it.
    #[test]
    fn encode_project_id_round_trips_through_percent_decode() {
        for slug in ["local/9f2a8b3c4d5e6f70", "github.com/spelunk-cloud/spelunk"] {
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
            "http://127.0.0.1:7777/v1/projects/my-project/memory"
        );
    }

    /// Regression test for the v0.8.0 IMP-3 retest sweep (spelunk-cloud/spelunk
    /// agent-comms/handoffs/qa-v080-test-plan.md, Fix 3).
    ///
    /// `POST /v1/projects/{id}/memory/search` on the real server expects
    /// `{"query": <text>, "limit": <n>}` — the server embeds the query itself
    /// (see `spelunk_server::handlers::SearchRequest` /
    /// `spelunk-server/src/handlers.rs::search_notes`, which calls
    /// `body.query` through its embedder).
    ///
    /// `RemoteMemoryBackend::search` instead serialises a pre-computed
    /// `{"embedding": [f32...], "limit": <n>}` body (see `SearchRequest` in
    /// this file). Because the server's `query` field is a required `String`
    /// (no `#[serde(default)]`), axum's `Json<SearchRequest>` extractor
    /// rejects the mismatched body with `422 Unprocessable Entity` *before*
    /// `search_notes` ever runs — so `memory search` / `memory timeline`
    /// (which both funnel through `RemoteMemoryBackend::search`) always fail
    /// with a 422 against a real spelunk-server, never returning results.
    ///
    /// This was masked pre-IMP-3 because `memory search`/`timeline` short-
    /// circuited on `cfg.server_url.is_none()` with a "requires
    /// spelunk-server" error before ever issuing the HTTP request — IMP-3
    /// fixed that gating (so loopback auto-discovered servers are honoured),
    /// which is what newly exposes this pre-existing client/server payload
    /// mismatch end-to-end.
    ///
    /// This test asserts the wire body sent by the client is shaped the way
    /// the real server's `SearchRequest` requires (`query` + `limit`, no
    /// `embedding` field). It currently FAILS — the client sends `embedding`
    /// instead of `query` — capturing the bug for the implementer to fix
    /// (either by changing `RemoteMemoryBackend::SearchRequest` to send
    /// `{query, limit}` and dropping the local KNN step, or by adding an
    /// `embedding`-accepting variant server-side; that decision belongs to
    /// the implementer / architect, not this test).
    #[tokio::test]
    async fn search_sends_query_text_not_precomputed_embedding() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // Mirrors the real server's contract: a body containing a `query`
        // string field (and NOT requiring `embedding`) is what
        // `spelunk-server::handlers::search_notes` actually accepts.
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
            api_key: None,
        };

        // `MemoryBackend::search` takes both a pre-computed query embedding
        // blob (used by local backends for KNN) *and* the raw query text
        // (used by the remote backend, which has no local embedder and must
        // let the server embed server-side — see spelunk#359). The remote
        // backend ignores `query_blob` and sends `query` on the wire.
        let query_blob = crate::embeddings::vec_to_blob(&[0.1_f32, 0.2, 0.3]);
        let result = backend.search(&query_blob, "timezone", 3, None).await;

        assert!(
            result.is_ok(),
            "expected the server to accept the request body and return results, \
             got: {:?}\n\n\
             If this failed with a 422-shaped error, the client is still \
             sending `{{\"embedding\": [...], \"limit\": N}}` instead of the \
             `{{\"query\": \"<text>\", \"limit\": N}}` shape the real \
             spelunk-server requires — see spelunk-cloud/spelunk issue for \
             'memory search returns 422 against a real server (query/embedding \
             payload mismatch)'.",
            result.err().map(|e| e.to_string())
        );
    }
}
