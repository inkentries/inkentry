use std::path::Path;
use std::time::Instant;

use inkentry_core::storage::memory::{EventFields, record_event_at};

use crate::config::{Config, SyncMode};

const SURFACE: &str = "cli";

// Mirrors `storage::open_memory_backend`'s routing rather than inspecting an
// open backend, since several call sites never open one.
fn is_local_store(cfg: &Config, backend_override: Option<&str>) -> bool {
    if backend_override == Some("git-notes") {
        return false;
    }
    !(cfg.resolve_mode() == SyncMode::CloudFirst && cfg.server_url.is_some())
}

// Never creates `memory.db`: a read-only command must not leave a schema-less
// stray file behind. `returned_ids` are entity ids only, never titles, paths or
// query text.
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
        let cfg = Config {
            server_url: Some("https://team.example".to_string()),
            mode: Some(SyncMode::LocalFirst),
            ..Default::default()
        };
        assert!(is_local_store(&cfg, None));
    }
}
