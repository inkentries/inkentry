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
mod stats;

pub use backend::{LocalMemoryBackend, MemoryBackend, NoteInput};
pub use conventions::{ConventionRow, RawChunkRow, has_doc_prefix};
pub use db::Database;
pub use files::FileRecord;
pub use git_notes::{GitNotesBackend, append_to_git_notes};
pub use graph::GraphEdge;
pub use memory::{MemoryEdge, MemoryStore};
pub use note_record::{NoteRecord, now_millis, now_secs};
pub use remote::RemoteMemoryBackend;
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
/// Selection rule (ADR-004 — one canonical store per project):
/// 1. `backend_override = Some("git-notes")` → `GitNotesBackend`
/// 2. **Explicit** `server_url` in config (team/remote server) →
///    `RemoteMemoryBackend` (the team-memory tier: memory lives on the shared
///    server by the user's deliberate configuration).
/// 3. Otherwise → local SQLite `memory.db` at `mem_path`.
///
/// This function intentionally keys only on `cfg.server_url`. An auto-discovered
/// loopback server is inference-only and routes through `cfg.inference_url`
/// instead (see `Tier::effective_config`), so it never diverts memory CRUD away
/// from the project's local `memory.db`.
pub fn open_memory_backend(
    cfg: &crate::config::Config,
    mem_path: &Path,
    backend_override: Option<&str>,
) -> Result<Box<dyn MemoryBackend + Send>> {
    if backend_override == Some("git-notes") {
        return Ok(Box::new(GitNotesBackend::new()));
    }
    if let Some(url) = &cfg.server_url {
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
