use anyhow::{Context, Result};
use async_trait::async_trait;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use std::collections::HashSet;

use super::backend::{MemoryBackend, NoteInput};
use super::memory::{MemoryEdge, Note, NoteId};
use crate::embeddings::{PUSHED_VECTOR_PRECISION, blob_to_vec, pushed_vector_model_tag};

mod cloud_api;
mod peer;
mod retry;
mod sync;
mod wire_types;
pub use cloud_api::CloudApiMemoryBackend;
pub(super) use peer::{PeerDialect, detect_dialect};
pub use sync::{
    BatchItemResult, BatchPushItem, BatchPushResult, CloudSyncClient, EdgePushResult, RemoteEntry,
    SincePage, SyncEdgePush,
};
pub use wire_types::ConflictInfo;
use wire_types::*;

/// Characters that must be percent-encoded inside a single URL **path segment**.
///
/// `derive_project_id` produces slugs that contain `/` (`local/<blake3-hex>`,
/// `github.com/owner/repo`). Inserted raw into `/v1/projects/{project_id}/…`
/// the slashes split the segment and break axum routing (→ 404). We percent-encode
/// the slug so the whole slug occupies exactly one captured `{project_id}` segment;
/// axum percent-decodes it back to the original slug server-side, so the
/// persistence key (`projects.slug`, UNIQUE) is unchanged. See inkentry decision #106.
///
/// Mirrors `PROJECT_ID_SEGMENT` / `encode_project_id` in
/// `inkentry-cli/src/server_client.rs` — duplicated here because inkentry-core
/// cannot depend on inkentry-cli.
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
pub(super) fn encode_project_id(project_id: &str) -> String {
    utf8_percent_encode(project_id, PROJECT_ID_SEGMENT).to_string()
}

/// Percent-encode a note id for safe use as a single URL path segment.
///
/// Ids are opaque tokens supplied by the caller, so an id containing `/` or
/// `%` would otherwise re-shape the request path rather than address an entry.
/// A no-op for the ids either peer actually mints (decimal integers, UUIDs).
fn encode_path_segment(id: &NoteId) -> String {
    utf8_percent_encode(id.as_str(), PROJECT_ID_SEGMENT).to_string()
}

/// HTTP client for the inkentry-server REST API.
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
        // the segment and break axum routing → 404. See inkentry decision #106.
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

/// The command that fixes a rejected credential, or nothing for a status that
/// is not about credentials.
///
/// A 401/403 from a self-hosted server is a missing per-origin key more often
/// than anything else, and nothing migrates one into place on the user's
/// behalf any more (ADR-088 D3), so the error is where they learn the command.
pub fn credential_hint(status: reqwest::StatusCode, base_url: &str) -> String {
    if status != reqwest::StatusCode::UNAUTHORIZED && status != reqwest::StatusCode::FORBIDDEN {
        return String::new();
    }
    format!(
        " Store a key for this server with `inkentry auth set-key --server {base_url}`, or run \
         `inkentry login` if it is inkentry cloud."
    )
}

/// Whether the request died before a connection to the server existed at all:
/// refused, unresolvable, or a connect attempt that ran out of time.
///
/// A request that timed out *after* the server accepted the connection is
/// deliberately not this. That is a slow server rather than an absent one, and
/// the two want different words.
fn is_unreachable(err: &reqwest::Error) -> bool {
    err.is_connect()
}

/// Why the connection never came up, in the few words that change what the
/// reader does next: nothing listening on that port, versus a connect that
/// never drew any answer at all (a dropped SYN, which is how a filtering
/// firewall looks from this side).
fn connect_detail(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        return "connect timed out";
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::ConnectionRefused
        {
            return "connection refused";
        }
        source = std::error::Error::source(err);
    }
    "could not connect"
}

/// Headline for a transport failure against a server that is not there.
///
/// Naming the mode is true by construction wherever this is reached:
/// [`open_memory_backend`](super::open_memory_backend) builds
/// [`RemoteMemoryBackend`] and [`CloudApiMemoryBackend`] only under
/// [`SyncMode::CloudFirst`](crate::config::SyncMode::CloudFirst), the one mode
/// that moves the store of record off this machine and therefore has no local
/// copy it could serve instead.
fn unreachable_message(base_url: &str, err: &reqwest::Error) -> String {
    format!(
        "team server unreachable at {base_url} ({}); mode is cloud_first, which does not fall \
         back to the local store",
        connect_detail(err)
    )
}

/// Context for a send that failed at the transport layer.
///
/// `op` names the request as it always has. An unreachable server additionally
/// gets the diagnosis as the *headline*, because a raw transport error printed
/// under a URL reads as a malfunction rather than as "that server is not
/// answering", which is the one thing the reader needs to know.
pub(super) fn transport_error(err: reqwest::Error, base_url: &str, op: &str) -> anyhow::Error {
    let headline = is_unreachable(&err).then(|| unreachable_message(base_url, &err));
    let err = anyhow::Error::new(err).context(op.to_string());
    match headline {
        Some(headline) => err.context(headline),
        None => err,
    }
}

/// [`transport_error`] for the retrying send path, whose transport failure
/// arrives already wrapped in its route label.
pub(super) fn unreachable_headline(err: anyhow::Error, base_url: &str) -> anyhow::Error {
    match err.downcast_ref::<reqwest::Error>() {
        Some(source) if is_unreachable(source) => {
            let headline = unreachable_message(base_url, source);
            err.context(headline)
        }
        _ => err,
    }
}

/// [`reqwest::Response::error_for_status`] plus [`credential_hint`].
pub(super) trait CheckedResponse: Sized {
    fn checked(self, base_url: &str) -> Result<Self>;
}

impl CheckedResponse for reqwest::Response {
    fn checked(self, base_url: &str) -> Result<Self> {
        let status = self.status();
        let hint = credential_hint(status, base_url);
        if !hint.is_empty() {
            anyhow::bail!("{base_url} rejected the credential ({status}).{hint}");
        }
        Ok(self.error_for_status()?)
    }
}

// ── Trait implementation ──────────────────────────────────────────────────────

#[async_trait]
impl MemoryBackend for RemoteMemoryBackend {
    async fn add(&self, input: NoteInput) -> Result<(NoteId, bool)> {
        let vector = input.embedding.as_deref().map(blob_to_vec);
        // The tags only mean anything alongside a vector, and the accept side
        // refuses a vector that arrives without them.
        let (vector_model, vector_precision) = match vector {
            Some(_) => (
                Some(pushed_vector_model_tag().to_string()),
                Some(PUSHED_VECTOR_PRECISION.to_string()),
            ),
            None => (None, None),
        };
        let body = AddNoteRequest {
            kind: input.kind,
            title: input.title,
            body: input.body,
            tags: input.tags,
            linked_files: input.linked_files,
            vector,
            vector_model,
            vector_precision,
            source_ref: input.source_ref,
            valid_at: input.valid_at,
        };
        // A write with no client vector makes the server embed, so it runs
        // under the server's embed admission queue and can be shed with a
        // transient 429 rather than queued.
        let url = self.url("memory");
        let http_resp =
            retry::send_retrying_while_shed(&retry::RetryPolicy::default(), "POST /memory", || {
                self.authed(self.client.post(&url)).json(&body).send()
            })
            .await
            .map_err(|e| unreachable_headline(e, &self.base_url))?;

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
            // server.db doesn't enforce this amendment's promoted index, so
            // there is nothing for this backend to detect as a reuse.
            return Ok((resp.id, true));
        }

        let resp = http_resp
            .checked(&self.base_url)
            .context("server returned error for POST /memory")?
            .json::<AddNoteResponse>()
            .await
            .context("parsing POST /memory response")?;
        // Server-minted cross-machine id (ADR-059 D2). No local store to persist
        // into on this backend; surface it for diagnostics.
        if let Some(remote_id) = &resp.remote_id {
            tracing::debug!(remote_id, "server assigned remote_id for new memory entry");
        }
        Ok((resp.id, true))
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
    /// server-side (see `inkentry-server::handlers::search_notes`). The
    /// pre-computed `query_blob` is what local backends use for KNN; the
    /// remote backend ignores it and sends the raw query text instead, or the
    /// server's required `query: String` field is missing and axum rejects
    /// the request with 422 before the handler ever runs (spelunk-cloud/spelunk#359).
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
            .map_err(|e| transport_error(e, &self.base_url, "POST /memory/search"))?
            .checked(&self.base_url)
            .context("server returned error for POST /memory/search")?
            .json::<NoteListPayload>()
            .await
            .context("parsing search response")?;
        Ok(resp.into_notes().into_iter().map(Into::into).collect())
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
            .map_err(|e| transport_error(e, &self.base_url, "GET /memory"))?
            .checked(&self.base_url)
            .context("server returned error for GET /memory")?
            .json::<NoteListPayload>()
            .await
            .context("parsing list response")?;
        Ok(resp.into_notes().into_iter().map(Into::into).collect())
    }

    async fn get(&self, id: NoteId) -> Result<Option<Note>> {
        let resp = self
            .authed(
                self.client
                    .get(self.url(&format!("memory/{}", encode_path_segment(&id)))),
            )
            .send()
            .await
            .map_err(|e| transport_error(e, &self.base_url, "GET /memory/{id}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let note = resp
            .checked(&self.base_url)
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
            .map_err(|e| transport_error(e, &self.base_url, "GET /stats"))?
            .checked(&self.base_url)
            .context("server returned error for GET /stats")?
            .json::<CountResponse>()
            .await
            .context("parsing stats response")?;
        Ok(resp.count)
    }

    async fn archive(&self, id: NoteId) -> Result<bool> {
        let resp = self
            .authed(
                self.client
                    .post(self.url(&format!("memory/{}/archive", encode_path_segment(&id)))),
            )
            .send()
            .await
            .map_err(|e| transport_error(e, &self.base_url, "POST /memory/{id}/archive"))?
            .checked(&self.base_url)
            .context("server returned error for POST /memory/{id}/archive")?
            .json::<BoolResponse>()
            .await
            .context("parsing archive response")?;
        Ok(resp.changed)
    }

    async fn supersede(&self, old_id: NoteId, new_id: NoteId) -> Result<bool> {
        let body = SupersedeRequest { new_id };
        let resp = self
            .authed(self.client.post(self.url(&format!(
                "memory/{}/supersede",
                encode_path_segment(&old_id)
            ))))
            .json(&body)
            .send()
            .await
            .map_err(|e| transport_error(e, &self.base_url, "POST /memory/{id}/supersede"))?
            .checked(&self.base_url)
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
            .map_err(|e| transport_error(e, &self.base_url, "GET /memory (source_ref filter)"))?
            .checked(&self.base_url)
            .context("server returned error for GET /memory")?
            .json::<NoteListPayload>()
            .await
            .context("parsing list response")?;
        Ok(resp.into_notes().into_iter().map(Into::into).collect())
    }

    async fn harvested_shas(&self) -> Result<HashSet<String>> {
        let resp = self
            .authed(self.client.get(self.url("memory/harvested-shas")))
            .send()
            .await
            .map_err(|e| transport_error(e, &self.base_url, "GET /memory/harvested-shas"))?
            .checked(&self.base_url)
            .context("server returned error for GET /memory/harvested-shas")?
            .json::<HarvestedShasPayload>()
            .await
            .context("parsing harvested-shas response")?;
        Ok(resp.into_shas().into_iter().collect())
    }

    async fn has_source_ref(&self, sha: &str) -> Result<bool> {
        // Reuse the list endpoint with the full SHA as prefix; if any results come back,
        // this commit has been harvested.
        let notes = self.list_by_source_ref(sha, 1, true, None).await?;
        Ok(!notes.is_empty())
    }

    /// Remote backend: edge mutations are not supported — no-op.
    async fn add_edge(&self, _from_id: &NoteId, _to_id: &NoteId, _kind: &str) -> Result<()> {
        Ok(())
    }

    /// Remote backend: edge queries are not supported — returns empty lists.
    async fn get_edges(&self, _id: &NoteId) -> Result<(Vec<MemoryEdge>, Vec<MemoryEdge>)> {
        Ok((vec![], vec![]))
    }

    fn backend_kind(&self) -> &'static str {
        "remote"
    }
}

#[cfg(test)]
mod tests;
