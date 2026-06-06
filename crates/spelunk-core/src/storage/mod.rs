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

/// Open the appropriate memory backend.
///
/// Priority:
/// 1. `backend_override = Some("git-notes")` → `GitNotesBackend`
/// 2. `server_url` set in config → `RemoteMemoryBackend`
/// 3. Otherwise → local SQLite at `mem_path`
pub fn open_memory_backend(
    cfg: &crate::config::Config,
    mem_path: &Path,
    backend_override: Option<&str>,
) -> Result<Box<dyn MemoryBackend + Send>> {
    if backend_override == Some("git-notes") {
        return Ok(Box::new(GitNotesBackend::new()));
    }
    if let Some(url) = &cfg.server_url {
        let project_id = cfg.project_id.clone().expect(
            "project_id must be set when server_url is configured; \
             call Config::validate() before open_memory_backend()",
        );
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
