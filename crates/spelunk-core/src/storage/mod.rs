pub mod backend;
pub mod db;
pub mod git_notes;
pub mod memory;
pub mod note_record;
pub mod remote;

// Storage sub-modules: each holds impl blocks for Database or standalone types.
mod chunks;
mod conventions;
mod files;
mod graph;
mod search;
mod snapshots;
mod specs;
mod sql;
mod stats;

pub use backend::{LocalMemoryBackend, MemoryBackend, NoteInput};
pub use conventions::{ConventionRow, RawChunkRow, has_doc_prefix};
pub use db::Database;
pub use files::FileRecord;
pub use git_notes::{GitNotesBackend, append_to_git_notes};
pub use graph::GraphEdge;
pub use memory::{MemoryEdge, MemoryStore, SyncRow};
pub use note_record::{NoteRecord, now_millis, now_secs};
pub use remote::{
    BatchItemResult, BatchPushItem, BatchPushResult, CloudSyncClient, RemoteEntry,
    RemoteMemoryBackend, resolve_cloud_project_uuid,
};
pub use snapshots::{Snapshot, SymbolVersion};
pub use specs::{SpecRecord, StaleSpec};
pub use stats::{DriftCandidate, IndexStats, LanguageStat, StalenessReport, record_usage_at};

use anyhow::Result;
use std::path::Path;

/// Escape a user-supplied string for use in a SQLite LIKE pattern.
///
/// SQLite's LIKE operator treats `%`, `_`, and the chosen escape character as
/// special. If the caller appends or prepends wildcards around an
/// otherwise-literal value (e.g. `'%' || ?1` for suffix matching), any `%` or
/// `_` that appears inside the user's string would be misinterpreted as
/// additional wildcards, causing over-matching.
///
/// This function escapes `\`, `%`, and `_` with a backslash so that
/// `LIKE … ESCAPE '\'` treats them as literal characters.
///
/// # Example
/// ```ignore
/// let pat = format!("%{}", escape_like(user_path));
/// stmt.query(rusqlite::params![pat])?;
/// // SQL: WHERE path LIKE ?1 ESCAPE '\'
/// ```
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

/// Open the appropriate memory backend.
///
/// Selection rule (ADR-004 — one canonical store per project; ADR-037 D1 — the
/// resolved sync mode replaces the implicit "is `server_url` set" branch):
/// 1. `backend_override = Some("git-notes")` → `GitNotesBackend`.
/// 2. [`SyncMode::CloudFirst`](crate::config::SyncMode::CloudFirst) **and** an
///    explicit `server_url` → `RemoteMemoryBackend`. `cloud_first` is the
///    debug/override tier: reads/writes go straight to the cloud.
/// 3. Otherwise → local SQLite `memory.db` at `mem_path`. This covers
///    [`SyncMode::Offline`] (provable no-cloud, even when `server_url` is set;
///    the `SPELUNK_NO_SERVER=1` kill-switch resolves here) and the default
///    [`SyncMode::LocalFirst`], where reads and writes stay local and the cloud
///    replica is converged explicitly by `spelunk sync` (ADR-037 D2).
///
/// This function keys on the resolved mode plus `cfg.server_url`. An
/// auto-discovered loopback server is inference-only and routes through
/// `cfg.inference_url` instead (see `Tier::effective_config`), so it never
/// diverts memory CRUD away from the project's local `memory.db`.
pub fn open_memory_backend(
    cfg: &crate::config::Config,
    mem_path: &Path,
    backend_override: Option<&str>,
) -> Result<Box<dyn MemoryBackend + Send>> {
    use crate::config::SyncMode;

    if backend_override == Some("git-notes") {
        return Ok(Box::new(GitNotesBackend::new()));
    }
    // Only `cloud_first` (debug/override) routes memory CRUD straight to the
    // cloud; `offline` and `local_first` resolve to the local store.
    let route_remote = cfg.resolve_mode() == SyncMode::CloudFirst;
    if let Some(url) = cfg.server_url.as_ref().filter(|_| route_remote) {
        let project_id = cfg.project_id.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "server_url is set ({url}) but project_id is missing.\n\
                 Set `project_id` in your spelunk config (e.g. ~/.config/spelunk/config.toml \
                 or .spelunk/config.toml), or set the SPELUNK_PROJECT_ID environment variable, \
                 so memory operations can be keyed to a project on the server."
            )
        })?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Box::new(RemoteMemoryBackend {
            client,
            base_url: url.clone(),
            project_id,
            api_key: cfg.server_key.clone(),
        }))
    } else {
        Ok(Box::new(LocalMemoryBackend::new(MemoryStore::open(
            mem_path,
        )?)))
    }
}

// ── Tests for escape_like ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::escape_like;

    // Bug #406 — unit tests for the LIKE-metacharacter escape helper.

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
        // The backslash escape character itself must be doubled.
        assert_eq!(escape_like("foo\\bar"), "foo\\\\bar");
    }

    #[test]
    fn plain_path_is_unchanged() {
        assert_eq!(escape_like("normal/path/file.rs"), "normal/path/file.rs");
    }

    #[test]
    fn all_three_metacharacters_combined() {
        // "a%b_c\d" → "a\%b\_c\\d"
        assert_eq!(escape_like("a%b_c\\d"), "a\\%b\\_c\\\\d");
    }

    #[test]
    fn empty_string_stays_empty() {
        assert_eq!(escape_like(""), "");
    }
}

// ── ADR-037 D1: backend selection honours the resolved sync mode ──────────────

#[cfg(test)]
mod backend_selection_tests {
    use super::open_memory_backend;
    use crate::config::{Config, SyncMode};
    use std::sync::OnceLock;

    fn register_sqlite_vec() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });
    }

    fn clear_env() {
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
    }

    #[test]
    #[serial_test::serial]
    fn offline_mode_routes_local_even_with_server_url() {
        clear_env();
        register_sqlite_vec();
        let cfg = Config {
            server_url: Some("http://team.example.com:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(SyncMode::Offline),
            ..Default::default()
        };
        let be = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None).unwrap();
        assert_eq!(
            be.backend_kind(),
            "sqlite",
            "offline must keep memory local even when server_url is set"
        );
    }

    #[test]
    #[serial_test::serial]
    fn local_first_mode_routes_local() {
        clear_env();
        register_sqlite_vec();
        let cfg = Config {
            server_url: Some("http://team.example.com:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(SyncMode::LocalFirst),
            ..Default::default()
        };
        let be = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None).unwrap();
        assert_eq!(be.backend_kind(), "sqlite");
    }

    #[test]
    #[serial_test::serial]
    fn cloud_first_mode_routes_remote() {
        clear_env();
        let cfg = Config {
            server_url: Some("http://team.example.com:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };
        let be = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None).unwrap();
        assert_eq!(
            be.backend_kind(),
            "remote",
            "cloud_first (debug/override) routes memory CRUD to the cloud"
        );
    }

    #[test]
    #[serial_test::serial]
    fn no_server_kill_switch_forces_local() {
        register_sqlite_vec();
        let cfg = Config {
            server_url: Some("http://team.example.com:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };
        unsafe { std::env::set_var("SPELUNK_NO_SERVER", "1") };
        let be = open_memory_backend(&cfg, std::path::Path::new(":memory:"), None).unwrap();
        assert_eq!(
            be.backend_kind(),
            "sqlite",
            "SPELUNK_NO_SERVER=1 forces offline → local backend"
        );
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
    }
}
