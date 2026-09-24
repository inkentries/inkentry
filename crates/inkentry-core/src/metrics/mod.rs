//! ADR-098 metrics snapshot: `inkentry metrics snapshot` and the summary
//! `inkentry status` prints alongside it.
//!
//! Two of the three sources (D1) are implemented here. **State** is
//! reproducible from a commit and a repository (`memory.db`, `refs/notes/
//! inkentry`, `git log`), with no instrumentation. **Events** are
//! observations, computed from the local `events` table (D5) a command
//! records into after its own response is written — not reproducible from a
//! commit alone, since they describe what actually happened on this machine.
//! The **eval** source is never computed by the CLI (D7) and never appears
//! here at all.
//!
//! [`build_snapshot`]'s state block is deterministic: given the same
//! repository state it returns byte-identical data, with no wall clock, no
//! network and no model calls — the window closes at the later of HEAD's
//! committer time and the newest memory entry's `created_at`. The events
//! block is deterministic given the same `events` rows, which is the whole
//! of what determinism can mean for an observation.

mod events;
mod git;
mod state;

pub use events::{CallCounts, EventsMetrics, compute_events_metrics};
pub use state::{
    EntryCounts, GitWindowFacts, MedianSeconds, NearDuplicateRate, Rate, StateMetrics,
    StatusMetricsSummary, compute_state_metrics, compute_status_metrics_summary,
};

use anyhow::Result;
use serde::Serialize;
use std::path::Path;

use crate::storage::git_notes::GitNotesBackend;
use crate::storage::memory::MemoryStore;

/// `inkentry.metrics/1`'s schema id, carried verbatim in every snapshot.
pub const SCHEMA: &str = "inkentry.metrics/1";

/// Default metrics window, in days, used by both `inkentry metrics snapshot`
/// and the summary in `inkentry status`.
pub const DEFAULT_WINDOW_DAYS: u32 = 30;

#[derive(Debug, Clone, Serialize)]
pub struct CommitHeader {
    pub sha: String,
    pub time: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbedderHeader {
    pub model_id: &'static str,
    pub dim: usize,
    pub precision: &'static str,
}

/// D2's per-snapshot header: everything a chart needs to know whether two
/// snapshots are comparable (same embedder, same window) before it joins
/// them.
#[derive(Debug, Clone, Serialize)]
pub struct Header {
    pub project: String,
    /// `None` when the project is not a git repository (or has no commits
    /// yet) — omitted rather than a fabricated commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitHeader>,
    pub inkentry_version: String,
    pub embedder: EmbedderHeader,
    pub window_days: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub schema: &'static str,
    pub header: Header,
    pub state: StateMetrics,
    pub events: EventsMetrics,
}

/// The events source's own window: always 7 days (D3's `use.*`/`auto.*`
/// formulas are usage-recency questions, independent of the state block's
/// `--window-days`), ending at the same `window_end` the state block anchors
/// to (D7: both blocks describe one snapshot).
pub const EVENTS_WINDOW_DAYS: u32 = 7;

/// The window closes at the later of HEAD's committer time and the newest
/// entry, never at the wall clock: the document stays a function of the
/// repository and the store, and an entry recorded since the last commit is
/// still counted.
fn window_end(store: &MemoryStore, head_time: Option<i64>) -> Result<i64> {
    let newest_entry = store.newest_created_at()?;
    Ok(head_time.into_iter().chain(newest_entry).max().unwrap_or(0))
}

/// Assemble a full snapshot for the memory store at `mem_path`, rooted at
/// `project_root`.
///
/// `project_id` and `inkentry_version` are passed in rather than derived
/// here: `project_id` may be an explicit config override
/// (`Config::resolve_project_id`), and the version this build reports must be
/// `inkentry-cli`'s own `CARGO_PKG_VERSION`, not `inkentry-core`'s.
pub async fn build_snapshot(
    store: &MemoryStore,
    project_root: &Path,
    project_id: String,
    inkentry_version: String,
    window_days: u32,
) -> Result<Snapshot> {
    let head = git::head_commit(project_root).await;

    let window_end = window_end(store, head.as_ref().map(|(_, time)| *time))?;
    let window_start = window_end - i64::from(window_days) * 86_400;

    let git_facts = match &head {
        Some(_) => {
            let commits = git::commits_in_window(project_root, window_start, window_end).await?;
            let anchored_commit_shas = GitNotesBackend::with_root(project_root.to_path_buf())
                .anchored_commit_shas()
                .await
                .unwrap_or_default();
            Some(GitWindowFacts {
                lines_changed_in_window: commits.iter().map(|c| c.lines_changed).sum(),
                commit_shas: commits.into_iter().map(|c| c.sha).collect(),
                anchored_commit_shas,
            })
        }
        None => None,
    };

    let state = compute_state_metrics(
        store,
        window_start,
        window_end,
        window_days,
        git_facts.as_ref(),
    )?;

    let events_window_start = window_end - i64::from(EVENTS_WINDOW_DAYS) * 86_400;
    let event_rows = store.events_in_window(events_window_start, window_end)?;
    let events = compute_events_metrics(&event_rows, EVENTS_WINDOW_DAYS);

    Ok(Snapshot {
        schema: SCHEMA,
        header: Header {
            project: project_id,
            commit: head.map(|(sha, time)| CommitHeader { sha, time }),
            inkentry_version,
            embedder: EmbedderHeader {
                model_id: crate::embeddings::MODEL_ID,
                dim: crate::embeddings::EMBEDDING_DIM,
                precision: crate::embeddings::PUSHED_VECTOR_PRECISION,
            },
            window_days,
        },
        state,
        events,
    })
}

/// Assemble [`StatusMetricsSummary`] for `inkentry status`'s compact metrics
/// section: the window mechanics mirror [`build_snapshot`] (same anchoring,
/// same default window), but the git-derived pieces stay to the cheap,
/// `--numstat`-free commit listing, and there is no near-duplicate scan or
/// context-token estimate.
pub async fn build_status_summary(
    store: &MemoryStore,
    project_root: &Path,
    window_days: u32,
) -> Result<StatusMetricsSummary> {
    let head = git::head_commit(project_root).await;

    let window_end = window_end(store, head.as_ref().map(|(_, time)| *time))?;
    let window_start = window_end - i64::from(window_days) * 86_400;

    let (commit_shas, anchored_commit_shas) = match &head {
        Some(_) => {
            let shas = git::commit_shas_in_window(project_root, window_start, window_end).await?;
            let anchored = GitNotesBackend::with_root(project_root.to_path_buf())
                .anchored_commit_shas()
                .await
                .unwrap_or_default();
            (Some(shas), Some(anchored))
        }
        None => (None, None),
    };

    let events_window_start = window_end - i64::from(EVENTS_WINDOW_DAYS) * 86_400;
    let event_rows = store.events_in_window(events_window_start, window_end)?;
    let events = compute_events_metrics(&event_rows, EVENTS_WINDOW_DAYS);

    compute_status_metrics_summary(
        store,
        window_start,
        window_end,
        window_days,
        commit_shas.as_deref(),
        anchored_commit_shas.as_ref(),
        events,
    )
}
