use anyhow::{Context, Result};
use async_trait::async_trait;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use std::collections::HashSet;

use super::backend::{AddOutcome, EntityIdLookup, MemoryBackend, NoteInput, Resolution};
use super::memory::{MemoryEdge, Note, NoteId};
use crate::embeddings::{PUSHED_VECTOR_PRECISION, blob_to_vec, pushed_vector_model_tag};

mod cloud_api;
mod peer;
mod retry;
mod session;
mod sync;
mod wire_types;
pub use cloud_api::CloudApiMemoryBackend;
pub(super) use peer::{PeerDialect, detect_dialect};
pub(super) use session::installed_refresher;
pub use session::{Bearer, SessionRefresher, install_session_refresher};
pub use sync::{
    BatchItemResult, BatchPushItem, BatchPushResult, CloudSyncClient, EdgePushResult, RemoteEntry,
    SincePage, SyncEdgePush,
};
pub use wire_types::ConflictInfo;
use wire_types::*;

// Characters that must be percent-encoded inside a single URL path segment.
//
// `project_id` slugs can contain `/` (`local/<blake3-hex>`,
// `github.com/owner/repo`); inserted raw into `/v1/projects/{project_id}/…`
// the slashes split the segment and break axum routing. Percent-encoding
// keeps the whole slug in one captured segment; axum decodes it back
// server-side, so the persisted slug is unchanged.
//
// Mirrors `PROJECT_ID_SEGMENT`/`encode_project_id` in
// `inkentry-cli/src/server_client.rs` — duplicated here because inkentry-core
// cannot depend on inkentry-cli.
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

// Percent-encodes a `project_id` slug for safe use as a single URL path
// segment; only the segment is encoded, not the surrounding URL.
pub(super) fn encode_project_id(project_id: &str) -> String {
    utf8_percent_encode(project_id, PROJECT_ID_SEGMENT).to_string()
}

// Percent-encodes a note id for safe use as a single URL path segment. Ids
// are opaque caller-supplied tokens, so one containing `/` or `%` would
// otherwise re-shape the request path. A no-op for the ids either peer
// actually mints (decimal integers, UUIDs).
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
    pub bearer: Bearer,
}

impl RemoteMemoryBackend {
    fn url(&self, path: &str) -> String {
        format!(
            "{}/v1/projects/{}/{}",
            self.base_url.trim_end_matches('/'),
            encode_project_id(&self.project_id),
            path
        )
    }

    // Sends an authenticated request, classifying any transport failure once.
    // When a connection to this origin already failed earlier in this
    // process, the attempt is skipped and the same failure reported
    // immediately: a latency shortcut, never a different outcome.
    async fn send(&self, req: reqwest::RequestBuilder, op: &str) -> Result<reqwest::Response> {
        session::send_request(&self.bearer, &self.base_url, req, op).await
    }
}

/// The command that fixes a rejected credential, or nothing for a status that
/// is not about credentials.
///
/// A 401/403 from a self-hosted server is a missing per-origin key more often
/// than anything else, and nothing migrates one into place on the user's
/// behalf, so the error is where they learn the command.
///
/// Callers must have renewed an expired cloud session before reaching this
/// (the memory backends do so in `Bearer::send`); otherwise `inkentry login`
/// is advised for a session a refresh would have revived.
pub fn credential_hint(status: reqwest::StatusCode, base_url: &str) -> String {
    if status != reqwest::StatusCode::UNAUTHORIZED && status != reqwest::StatusCode::FORBIDDEN {
        return String::new();
    }
    format!(
        " Store a key for this server with `inkentry auth set-key --server {base_url}`, or run \
         `inkentry login` if it is inkentry cloud."
    )
}

// How a request failed before it ever carried a reply.
enum ConnectFailure {
    // Nothing answered: refused, unresolvable, or a connect that ran out of
    // time.
    Unreachable,
    // Something answered and then the TLS handshake failed. Carries the short
    // cause, which is what tells the operator which certificate to fix.
    Tls(String),
}

// Classifies a transport failure, or `None` when the request reached the
// server and failed after that (a slow server, not an absent one).
fn classify(err: &reqwest::Error) -> Option<ConnectFailure> {
    if !err.is_connect() {
        return None;
    }
    // `reqwest` reports a failed TLS handshake as a connect error, but the
    // connection demonstrably succeeded: the server accepted it and the
    // handshake is what failed. Reporting that as unreachable sends the
    // operator to restart a server that is already running, when the fix is a
    // certificate or a trust anchor.
    match crate::config::find_rustls_cause(err) {
        Some(cause) => Some(ConnectFailure::Tls(cause)),
        None => Some(ConnectFailure::Unreachable),
    }
}

// Why the connection never came up, in the few words that change what the
// reader does next: nothing listening on that port, versus a connect that
// never drew any answer at all (a dropped SYN, as a filtering firewall looks
// from this side).
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

// The clause naming the mode, appended to every headline here. True by
// construction: `open_memory_backend` builds these backends only under
// `cloud_first`, the one mode with no local copy to fall back to.
const NO_FALLBACK: &str = "mode is cloud_first, which does not fall back to the local store";

fn unreachable_message(base_url: &str, detail: &str) -> String {
    format!("team server unreachable at {base_url} ({detail}); {NO_FALLBACK}")
}

// Deliberately says the server is running: the whole point of separating this
// from the unreachable wording is that restarting the server cannot fix it.
fn tls_message(base_url: &str, cause: &str) -> String {
    format!(
        "TLS handshake with the team server at {base_url} failed ({cause}); it accepted the \
         connection, so the server is running and this is a certificate problem rather than an \
         outage. Trust the CA that signed it with `server_ca` in your inkentry config, or \
         INKENTRY_SERVER_CA. {NO_FALLBACK}"
    )
}

// Context for a send that failed at the transport layer. A connect-stage
// failure gets the diagnosis as the headline, because a raw transport error
// printed under a URL reads as a malfunction rather than "that server is not
// answering" or "that certificate is not trusted".
pub(super) fn transport_error(err: reqwest::Error, base_url: &str, op: &str) -> anyhow::Error {
    let headline = match classify(&err) {
        Some(ConnectFailure::Unreachable) => {
            // Only a genuinely absent server is memoised. A TLS failure must
            // not be, or the next attempt would skip its handshake and report
            // the wrong diagnosis.
            crate::reachability::record_connect_failure(base_url);
            Some(unreachable_message(base_url, connect_detail(&err)))
        }
        Some(ConnectFailure::Tls(cause)) => Some(tls_message(base_url, &cause)),
        None => None,
    };
    let err = anyhow::Error::new(err).context(op.to_string());
    match headline {
        Some(headline) => err.context(headline),
        None => err,
    }
}

// `transport_error` for the retrying send path, whose transport failure
// arrives already wrapped in its route label.
pub(super) fn unreachable_headline(err: anyhow::Error, base_url: &str) -> anyhow::Error {
    let headline = match err.downcast_ref::<reqwest::Error>().and_then(classify) {
        Some(ConnectFailure::Unreachable) => {
            let source = err
                .downcast_ref::<reqwest::Error>()
                .expect("just matched on it");
            crate::reachability::record_connect_failure(base_url);
            Some(unreachable_message(base_url, connect_detail(source)))
        }
        Some(ConnectFailure::Tls(cause)) => Some(tls_message(base_url, &cause)),
        None => None,
    };
    match headline {
        Some(headline) => err.context(headline),
        None => err,
    }
}

// The error a request reports when it is skipped because a connection to the
// same origin already failed in this process — the same conclusion an
// attempt would have reached, reported sooner, never a fallback.
pub(super) fn already_unreachable(base_url: &str, op: &str) -> anyhow::Error {
    anyhow::anyhow!("{op}").context(unreachable_message(
        base_url,
        "a connection attempt earlier in this command already failed",
    ))
}

// `reqwest::Response::error_for_status` plus `credential_hint`.
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

#[async_trait]
impl MemoryBackend for RemoteMemoryBackend {
    async fn add(&self, input: NoteInput) -> Result<(NoteId, bool)> {
        match self.add_with_reconcile(input, false, &[]).await? {
            AddOutcome::Created { id, created, .. } => Ok((id, created)),
            AddOutcome::Blocked { .. } => {
                anyhow::bail!("server refused the write although reconciliation was not requested")
            }
        }
    }

    async fn add_with_reconcile(
        &self,
        input: NoteInput,
        reconcile: bool,
        resolutions: &[Resolution],
    ) -> Result<AddOutcome> {
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
        let (origin_actor_kind, origin_tool, origin_model) = match &input.origin {
            Some(o) => (
                Some(o.actor_kind.as_str().to_string()),
                o.tool.clone(),
                o.model.clone(),
            ),
            None => (None, None, None),
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
            origin_actor_kind,
            origin_tool,
            origin_model,
            // ADR-100 D4: a server that predates this field simply ignores
            // it and answers with the pre-existing 409 shape, which
            // `AddNoteResponse::stored`'s default reads correctly.
            reconcile: reconcile.then_some("block"),
            resolutions: resolutions.iter().map(ResolutionWire::from).collect(),
        };
        // A write with no client vector makes the server embed, so it runs
        // under the server's embed admission queue and can be shed with a
        // transient 429 rather than queued.
        let url = self.url("memory");
        if crate::reachability::connect_already_failed(&self.base_url) {
            return Err(already_unreachable(&self.base_url, "POST /memory"));
        }
        let (url, body) = (&url, &body);
        let http_resp = self
            .bearer
            .send(|token| async move {
                retry::send_retrying_while_shed(
                    &retry::RetryPolicy::default(),
                    "POST /memory",
                    || {
                        session::authorize(self.client.post(url), token.as_deref())
                            .json(body)
                            .send()
                    },
                )
                .await
            })
            .await
            .map_err(|e| unreachable_headline(e, &self.base_url))?;

        let status = http_resp.status();

        if status == reqwest::StatusCode::CONFLICT {
            let resp = http_resp
                .json::<AddNoteResponse>()
                .await
                .context("parsing POST /memory 409 response")?;

            if !resp.stored {
                // ADR-100 D2: a duplicate-band candidate with no resolution.
                // Nothing was written.
                return Ok(AddOutcome::Blocked {
                    candidates: resp.candidates.into_iter().map(Into::into).collect(),
                });
            }

            // Pre-ADR-100 server: stored but conflicting — treat as success
            // but emit the legacy warning.
            if !resp.conflicts.is_empty() {
                eprintln!("warning: memory entry conflicts with existing entries:");
                for c in &resp.conflicts {
                    eprintln!(
                        "  · #{} \"{}\" (similarity: {:.2})",
                        c.id, c.title, c.similarity
                    );
                }
            }
            let id = resp
                .id
                .context("server reported the entry as stored but sent no id")?;
            // server.db doesn't enforce this amendment's promoted index, so
            // there is nothing for this backend to detect as a reuse.
            return Ok(AddOutcome::Created {
                id,
                created: true,
                candidates: Vec::new(),
                related: Vec::new(),
            });
        }

        let resp = http_resp
            .checked(&self.base_url)
            .context("server returned error for POST /memory")?
            .json::<AddNoteResponse>()
            .await
            .context("parsing POST /memory response")?;
        // Server-minted cross-machine id; no local store to persist into on
        // this backend, so surface it for diagnostics.
        if let Some(remote_id) = &resp.remote_id {
            tracing::debug!(remote_id, "server assigned remote_id for new memory entry");
        }
        let id = resp
            .id
            .context("server reported the entry as stored but sent no id")?;
        Ok(AddOutcome::Created {
            id,
            created: true,
            candidates: resp.candidates.into_iter().map(Into::into).collect(),
            related: resp.related.into_iter().map(Into::into).collect(),
        })
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

    /// The server embeds `query` server-side. The pre-computed `query_blob` is
    /// what local backends use for KNN; the remote backend ignores it and
    /// sends the raw query text instead.
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
            .send(
                self.client.post(self.url("memory/search")).json(&body),
                "POST /memory/search",
            )
            .await?
            .checked(&self.base_url)
            .context("server returned error for POST /memory/search")?
            .json::<NoteListPayload>()
            .await
            .context("parsing search response")?;
        Ok(resp.into_notes().into_iter().map(Into::into).collect())
    }

    /// Remote backend: BM25 text search is not supported.
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
    /// (server-side FTS is not available in this client). The relevance gate
    /// is calibrated for the local SQLite backend's embedding space only, so
    /// `gate` is unused here.
    async fn search_hybrid(
        &self,
        query_blob: &[u8],
        query: &str,
        limit: usize,
        as_of: Option<i64>,
        _gate: bool,
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
            .send(req, "GET /memory")
            .await?
            .checked(&self.base_url)
            .context("server returned error for GET /memory")?
            .json::<NoteListPayload>()
            .await
            .context("parsing list response")?;
        Ok(resp.into_notes().into_iter().map(Into::into).collect())
    }

    async fn get(&self, id: NoteId) -> Result<Option<Note>> {
        let resp = self
            .send(
                self.client
                    .get(self.url(&format!("memory/{}", encode_path_segment(&id)))),
                "GET /memory/{id}",
            )
            .await?;
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

    /// The team server pages its listing by `offset`, so a handle resolves
    /// against the whole store rather than one page: walk it, collecting every
    /// match so an ambiguous prefix is still caught when its entries fall on
    /// different pages. A store read to its empty tail is `Complete`.
    ///
    /// Two guards keep the walk finite when a peer does not page as asked: a
    /// page that adds no entry not already seen means the offset is not
    /// advancing, so the read is reported `Bounded` rather than looped, and a
    /// hard page ceiling backstops that guard.
    async fn note_ids_for_entity_id_prefix(&self, prefix: &str) -> Result<EntityIdLookup> {
        // Far above any project-sized store; the no-progress guard is what
        // actually stops a non-paging peer, this only backstops it.
        const MAX_PAGES: usize = 1_000;
        let page = crate::storage::backend::ENTITY_ID_PAGE_SIZE;
        let mut matches = Vec::new();
        let mut seen: HashSet<NoteId> = HashSet::new();
        let mut offset = 0usize;
        for _ in 0..MAX_PAGES {
            let req = self.client.get(self.url("memory")).query(&[
                ("limit", page.to_string().as_str()),
                ("offset", offset.to_string().as_str()),
                ("archived", "true"),
            ]);
            let notes: Vec<Note> = self
                .send(req, "GET /memory")
                .await?
                .checked(&self.base_url)
                .context("server returned error for GET /memory")?
                .json::<NoteListPayload>()
                .await
                .context("parsing list response")?
                .into_notes()
                .into_iter()
                .map(Into::into)
                .collect();
            if notes.is_empty() {
                return Ok(EntityIdLookup::Complete(matches));
            }
            let drained = notes.len();
            let mut progressed = false;
            for note in notes {
                if seen.insert(note.id.clone()) {
                    progressed = true;
                    if note.entity_id.starts_with(prefix) {
                        matches.push(note.id);
                    }
                }
            }
            if !progressed {
                return Ok(EntityIdLookup::Bounded {
                    matches,
                    examined: seen.len(),
                });
            }
            offset += drained;
        }
        Ok(EntityIdLookup::Bounded {
            matches,
            examined: seen.len(),
        })
    }

    async fn count(&self) -> Result<i64> {
        let resp = self
            .send(self.client.get(self.url("stats")), "GET /stats")
            .await?
            .checked(&self.base_url)
            .context("server returned error for GET /stats")?
            .json::<CountResponse>()
            .await
            .context("parsing stats response")?;
        Ok(resp.count)
    }

    async fn archive(&self, id: NoteId) -> Result<bool> {
        let resp = self
            .send(
                self.client
                    .post(self.url(&format!("memory/{}/archive", encode_path_segment(&id)))),
                "POST /memory/{id}/archive",
            )
            .await?
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
            .send(
                self.client
                    .post(self.url(&format!(
                        "memory/{}/supersede",
                        encode_path_segment(&old_id)
                    )))
                    .json(&body),
                "POST /memory/{id}/supersede",
            )
            .await?
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
            .send(req, "GET /memory (source_ref filter)")
            .await?
            .checked(&self.base_url)
            .context("server returned error for GET /memory")?
            .json::<NoteListPayload>()
            .await
            .context("parsing list response")?;
        Ok(resp.into_notes().into_iter().map(Into::into).collect())
    }

    async fn harvested_shas(&self) -> Result<HashSet<String>> {
        let resp = self
            .send(
                self.client.get(self.url("memory/harvested-shas")),
                "GET /memory/harvested-shas",
            )
            .await?
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
