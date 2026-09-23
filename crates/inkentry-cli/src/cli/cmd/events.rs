//! ADR-098 D5: the one place every command that records an event builds its
//! `EventFields` and calls the storage-layer recorder.
//!
//! Every call here is best-effort by construction (`record_event_at` never
//! errors outward) and happens after a command's response is already
//! written, so a dropped event can never change a command's exit status or
//! output.

use std::path::Path;
use std::time::Instant;

use inkentry_core::storage::memory::{EventFields, record_event_at};

use crate::config::{Config, SyncMode};

/// `surface` for every event this binary records. The CLI is the only
/// surface today; `mcp`/`rest`/`webui` (D5) belong to the processes that
/// implement them.
const SURFACE: &str = "cli";

/// Whether this invocation may record at all: local store only (ADR-098 D5).
/// `false` under an explicit `--backend git-notes` (no `memory.db` is even
/// opened on that path) and under `cloud_first` with a `server_url` set,
/// which routes memory CRUD straight to a remote store and never touches
/// `mem_path` (`storage::open_memory_backend`'s own routing rule, mirrored
/// here rather than re-derived from an already-open backend, since several
/// call sites never open one directly).
fn is_local_store(cfg: &Config, backend_override: Option<&str>) -> bool {
    if backend_override == Some("git-notes") {
        return false;
    }
    !(cfg.resolve_mode() == SyncMode::CloudFirst && cfg.server_url.is_some())
}

/// Record one `events` row for `command`, or silently do nothing when there
/// is nothing to record against: no local `memory.db` yet (the file must
/// already exist — this never creates one, since a read-only command must
/// never leave a schema-less stray file behind), an explicit git-notes
/// backend, or a remote-primary project (D5: "record for the local store
/// only").
///
/// `returned_ids` are `entity_id`s only — never titles, paths or query text
/// (D5) — and are comma-joined; an empty slice omits the column entirely
/// rather than storing an empty string.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record(
    cfg: &Config,
    mem_path: &Path,
    backend_override: Option<&str>,
    command: &str,
    code_results: Option<i64>,
    memory_results: Option<i64>,
    returned_ids: &[String],
    tokens_out: Option<i64>,
    started: Instant,
    ok: bool,
) {
    if !mem_path.exists() || !is_local_store(cfg, backend_override) {
        return;
    }
    let returned_ids_joined = (!returned_ids.is_empty()).then(|| returned_ids.join(","));
    let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    record_event_at(
        mem_path,
        EventFields {
            command,
            surface: SURFACE,
            trigger: cfg.caller.trigger.as_str(),
            actor_kind: cfg.caller.actor.as_str(),
            session_ref: cfg.caller.session_ref.as_deref(),
            code_results,
            memory_results,
            returned_ids: returned_ids_joined.as_deref(),
            tokens_out,
            latency_ms: Some(latency_ms),
            ok,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_first_with_a_server_url_is_not_the_local_store() {
        let cfg = Config {
            server_url: Some("https://team.example".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };
        assert!(!is_local_store(&cfg, None));
    }

    #[test]
    fn an_explicit_git_notes_backend_is_not_the_local_store() {
        assert!(!is_local_store(&Config::default(), Some("git-notes")));
    }

    #[test]
    fn the_solo_default_is_the_local_store() {
        assert!(is_local_store(&Config::default(), None));
    }

    #[test]
    fn local_first_with_a_server_url_is_still_the_local_store() {
        // local_first keeps reads/writes local; the server is a converging
        // replica, not the store of record.
        let cfg = Config {
            server_url: Some("https://team.example".to_string()),
            mode: Some(SyncMode::LocalFirst),
            ..Default::default()
        };
        assert!(is_local_store(&cfg, None));
    }
}
