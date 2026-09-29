pub mod backend;
pub mod db;
pub mod entity_id;
pub mod git_notes;
pub mod memory;
pub mod note_kind;
pub mod note_record;
pub mod origin;
pub mod remote;

mod chunks;
mod conventions;
mod files;
mod graph;
mod index_migrate;
mod migration_ladder;
mod search;
mod specs;
mod sql;
mod stats;

pub use backend::{
    AddOutcome, EntityIdLookup, LocalMemoryBackend, MemoryBackend, NoteInput, Resolution,
};
pub(crate) use chunks::chunk_subwords;
pub use conventions::{ConventionRow, RawChunkRow, has_doc_prefix};
pub use db::{Database, GRAPH_EDGES_REEXTRACT};
pub use entity_id::{
    ENTITY_ID_HANDLE_LEN, ENTITY_ID_MIN_PREFIX_LEN, entity_id, entity_id_handle,
    is_entity_id_lookup, note_entity_id,
};
pub use files::FileRecord;
pub use git_notes::{
    AppendOutcome, BatchAppendOutcome, GitNotesBackend, LOCK_WAIT_BUDGET, LockAttempt, NotesLock,
    NotesMergeOutcome, NotesRefs, PublishOutcome, RewriteRefStatus, SkipReason,
    append_anchor_record, append_edges, append_new_to_git_notes, append_state_update,
    append_to_git_notes, commit_patch_id, commits_reachable_from_any_ref, ensure_notes_rewrite_ref,
    is_ancestor, lock_notes, merge_tracking_notes, publish_notes, resolve_source_ref,
};
pub use graph::GraphEdge;
pub use memory::{
    Candidate, CandidateBand, CandidateHit, CarriedEdgeImport, DedupeSummary, FileState,
    MAX_CANDIDATES_PER_BAND, MEMORY_DUPLICATE_DISTANCE, MEMORY_MAX_QA_DISTANCE, MemoryEdge,
    MemoryStore, NoteId, NotesImportMarker, PendingAnchor, ResolvedFileLink, SyncEdge, SyncRow,
    classify_candidates, normalize_relative_path, normalize_tag, resolve_file_link,
    unresolvable_id_message, uuid_v7_at,
};
pub use note_kind::{NOTE_KINDS, is_valid_note_kind, parse_note_kind};
pub use note_record::{CarriedEdge, NoteRecord, carrier_token, now_millis, now_secs};
pub use origin::Origin;
pub use remote::{
    BatchItemResult, BatchPushItem, BatchPushResult, CloudSyncClient, EdgePushResult, RemoteEntry,
    RemoteMemoryBackend, SessionRefresher, SincePage, SyncEdgePush, credential_hint,
    install_session_refresher,
};
pub use specs::{SpecRecord, StaleSpec};
pub use stats::{DriftCandidate, EmbedTokenStats, IndexStats, LanguageStat, StalenessReport};

use anyhow::Result;
use std::path::Path;

// Caps a freshly-opened connection's page count when
// `INKENTRY_TEST_MAX_PAGE_COUNT` is set, so the crash-safety integration
// suite can force a deterministic `SQLITE_FULL` on the next write without a
// size-capped filesystem or a custom VFS. `max_page_count` is a
// per-connection setting, not persisted to the file, so this must run on
// every `open`. A no-op for every real user.
pub(crate) fn apply_test_page_cap(conn: &rusqlite::Connection) -> Result<()> {
    if let Ok(raw) = std::env::var("INKENTRY_TEST_MAX_PAGE_COUNT")
        && let Ok(n) = raw.parse::<i64>()
    {
        conn.execute_batch(&format!("PRAGMA max_page_count = {n};"))?;
    }
    Ok(())
}

// Blocks until killed, iff `INKENTRY_TEST_CRASH_POINT` names this exact
// point. Used by the crash-safety integration suite to land a real `SIGKILL`
// inside a chosen write window instead of racing wall-clock timing: the
// child prints a marker then blocks on a stdin read the harness never
// satisfies, so the harness can kill it with the process provably parked at
// that window. A no-op for every real user.
pub(crate) fn pause_for_crash_test(point: &str) {
    let Ok(target) = std::env::var("INKENTRY_TEST_CRASH_POINT") else {
        return;
    };
    if target != point {
        return;
    }
    println!("INKENTRY_TEST_CRASH_POINT_REACHED:{point}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut buf = [0u8; 1];
    let _ = std::io::Read::read(&mut std::io::stdin(), &mut buf);
}

// Escapes `\`, `%`, and `_` in a user-supplied string with a backslash, so a
// caller building a SQLite `LIKE … ESCAPE '\'` pattern around it (e.g. `'%' ||
// ?1` for suffix matching) doesn't have a literal `%` or `_` in the value
// misread as an additional wildcard.
pub(super) fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Open the appropriate memory backend: one canonical store per project.
///
/// 1. `backend_override = Some("git-notes")` → `GitNotesBackend`.
/// 2. [`SyncMode::CloudFirst`](crate::config::SyncMode::CloudFirst) **and** an
///    explicit `server_url` → `RemoteMemoryBackend`. `cloud_first` is the
///    server-authoritative tier: reads/writes go straight to the cloud, and an
///    unreachable server surfaces as an error (never a silent local read).
/// 3. Otherwise → local SQLite `memory.db` at `mem_path`. This covers
///    [`SyncMode::Offline`] (provable no-cloud, even when `server_url` is set;
///    the `INKENTRY_NO_SERVER=1` kill-switch resolves here) and the default
///    [`SyncMode::LocalFirst`], where reads and writes stay local and the cloud
///    replica is converged explicitly by `inkentry sync`.
///
/// This function keys on the resolved mode plus `cfg.server_url`. An
/// auto-discovered loopback server is inference-only and routes through
/// `cfg.inference_url` instead, so it never diverts memory CRUD away from
/// the project's local `memory.db`.
pub async fn open_memory_backend(
    cfg: &crate::config::Config,
    mem_path: &Path,
    backend_override: Option<&str>,
) -> Result<Box<dyn MemoryBackend + Send>> {
    use crate::config::SyncMode;

    if backend_override == Some("git-notes") {
        return Ok(Box::new(GitNotesBackend::new()));
    }
    // Only `cloud_first` (server-authoritative) routes memory CRUD straight to
    // the cloud; `offline` and `local_first` resolve to the local store.
    let route_remote = cfg.resolve_mode() == SyncMode::CloudFirst;
    if let Some(url) = cfg.server_url.as_ref().filter(|_| route_remote) {
        // The cloud-routing path attaches a bearer to every memory request;
        // reject a non-loopback plaintext `http://` `server_url` here, the
        // one choke point that catches it before any request is built. A
        // library returns the error rather than exiting; the CLI surfaces it.
        crate::config::validate_transport_url(url).map_err(anyhow::Error::msg)?;
        return open_remote_memory_backend(cfg, url).await;
    }
    Ok(Box::new(LocalMemoryBackend::new(MemoryStore::open(
        mem_path,
    )?)))
}

// Builds the cloud-routing memory backend (REST client) for an already
// transport-validated `url`, using the host's default secret store to
// resolve the bearer. Production reaches it only after `open_memory_backend`
// has enforced `validate_transport_url`, so a non-loopback plaintext `http://`
// url is rejected before any bearer is sent.
async fn open_remote_memory_backend(
    cfg: &crate::config::Config,
    url: &str,
) -> Result<Box<dyn MemoryBackend + Send>> {
    // Bearer resolved per-origin: `url` may be a self-hosted team server
    // (`cloud_first` routes any configured `server_url`, not only the cloud
    // one), and a cloud login must never leak to a self-hosted server, so the
    // origin decides which credential kind is consulted.
    let bearer = cfg.bearer_for(url)?;
    open_remote_memory_backend_with_bearer(cfg, url, bearer).await
}

// Same as `open_remote_memory_backend` but with an injected secret store, so
// tests can drive the cloud-routing seam without the real default store, and
// bypass the transport guard to reach a plaintext-http mock.
#[cfg(test)]
async fn open_remote_memory_backend_with_store(
    cfg: &crate::config::Config,
    url: &str,
    store: &dyn crate::config::secret_store::SecretStore,
) -> Result<Box<dyn MemoryBackend + Send>> {
    let bearer = cfg.bearer_for_with_store(url, store)?;
    open_remote_memory_backend_with_bearer(cfg, url, bearer).await
}

async fn open_remote_memory_backend_with_bearer(
    cfg: &crate::config::Config,
    url: &str,
    bearer: Option<String>,
) -> Result<Box<dyn MemoryBackend + Send>> {
    let project_id = cfg.project_id.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "server_url is set ({url}) but project_id is missing.\n\
             Set `project_id` in your inkentry config (e.g. ~/.config/inkentry/config.toml \
             or .inkentry/config.toml), or set the INKENTRY_PROJECT_ID environment variable, \
             so memory operations can be keyed to a project on the server."
        )
    })?;
    let client = crate::config::apply_server_ca(
        reqwest::Client::builder(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )?
    .connect_timeout(crate::config::REMOTE_CONNECT_TIMEOUT)
    .timeout(std::time::Duration::from_secs(30))
    .build()?;

    // `project_id` goes on the wire exactly as configured, slug or UUID; both
    // peers accept either.
    //
    // Which memory dialect that peer speaks is settled once, here, rather
    // than branched on inside every CRUD method. An uncertain probe resolves
    // to the team-server dialect.
    let bearer = match remote::installed_refresher() {
        Some(refresher) => remote::Bearer::renewable(bearer, refresher, cfg, url),
        None => remote::Bearer::fixed(bearer),
    };
    match remote::detect_dialect(&client, url).await {
        remote::PeerDialect::CloudApi => Ok(Box::new(remote::CloudApiMemoryBackend {
            client,
            base_url: url.to_string(),
            project_id,
            bearer,
        })),
        remote::PeerDialect::TeamServer => Ok(Box::new(RemoteMemoryBackend {
            client,
            base_url: url.to_string(),
            project_id,
            bearer,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::escape_like;

    #[test]
    fn percent_is_escaped() {
        assert_eq!(escape_like("foo%bar"), "foo\\%bar");
    }

    #[test]
    fn underscore_is_escaped() {
        assert_eq!(escape_like("foo_bar"), "foo\\_bar");
    }

    #[test]
    fn backslash_is_escaped_first() {
        assert_eq!(escape_like("foo\\bar"), "foo\\\\bar");
    }

    #[test]
    fn plain_path_is_unchanged() {
        assert_eq!(escape_like("normal/path/file.rs"), "normal/path/file.rs");
    }

    #[test]
    fn all_three_metacharacters_combined() {
        assert_eq!(escape_like("a%b_c\\d"), "a\\%b\\_c\\\\d");
    }

    #[test]
    fn empty_string_stays_empty() {
        assert_eq!(escape_like(""), "");
    }
}

#[cfg(test)]
mod backend_selection_tests;
