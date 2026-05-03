use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::Command;

use super::backend::{MemoryBackend, NoteInput};
use super::memory::{MemoryEdge, Note};

/// Hard cap on entries returned by `list()`.
///
/// Each entry requires one `git notes show` subprocess call (~13 ms).
/// Without a guard, `list(5000)` would take ~65 seconds.
/// Callers needing unbounded listing should use `--backend sqlite`.
const GIT_NOTES_MAX_LIST: usize = 500;

/// Serialised form stored inside a git note blob.
#[derive(Debug, Serialize, Deserialize)]
struct NoteRecord {
    id: i64,
    kind: String,
    title: String,
    body: String,
    tags: Vec<String>,
    linked_files: Vec<String>,
    created_at: i64,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    invalid_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by: Option<i64>,
}

/// Memory backend backed by `git notes` in the `refs/notes/spelunk` namespace.
///
/// Each memory entry is attached to the `HEAD` commit at write time as a single
/// JSON object.  Multiple entries accumulate across commits as the user works.
///
/// # Concurrency warning
/// `add` uses `git notes add -f` (force-replace).  If two processes add a note
/// to the same `HEAD` simultaneously the second write silently overwrites the
/// first.  For multi-agent workflows use the sqlite backend (the default).
/// See issue #185 for the full analysis.
///
/// # Unsupported methods
/// Semantic search (`search`, `search_hybrid`, `search_timeline`, `search_text`),
/// graph edges (`add_edge`, `get_edges`), `supersede`, `harvested_shas`, and
/// `has_source_ref` all return `Err` with a clear message rather than silently
/// returning empty results.
pub struct GitNotesBackend {
    git_root: Option<std::path::PathBuf>,
}

impl Default for GitNotesBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl GitNotesBackend {
    pub fn new() -> Self {
        Self { git_root: None }
    }

    /// Create a backend pinned to `root` — useful for testing with a temporary repo.
    pub fn with_root(root: std::path::PathBuf) -> Self {
        Self {
            git_root: Some(root),
        }
    }

    fn git(&self) -> Command {
        let mut cmd = Command::new("git");
        if let Some(ref root) = self.git_root {
            cmd.current_dir(root);
        }
        cmd
    }

    async fn run(&self, args: &[&str]) -> Result<String> {
        let out = self.git().args(args).output().await?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(anyhow!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    async fn head_sha(&self) -> Result<String> {
        Ok(self.run(&["rev-parse", "HEAD"]).await?.trim().to_string())
    }

    /// Return (commit-sha, commit-timestamp) pairs for commits that have a
    /// spelunk note, in reverse-chronological (newest first) order.
    async fn noted_commits(&self) -> Result<Vec<(String, i64)>> {
        // `git notes --ref=spelunk list` → "<note-blob-sha> <commit-sha>"
        let list_out = self
            .git()
            .args(["notes", "--ref=spelunk", "list"])
            .output()
            .await?;

        if !list_out.status.success() {
            return Ok(vec![]);
        }

        let noted: HashSet<String> = String::from_utf8_lossy(&list_out.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().nth(1).map(str::to_owned))
            .collect();

        if noted.is_empty() {
            return Ok(vec![]);
        }

        // Walk git log in reverse-chronological order to get commit timestamps.
        let log_out = self.git().args(["log", "--format=%H %at"]).output().await?;

        if !log_out.status.success() {
            return Ok(vec![]);
        }

        let pairs = String::from_utf8_lossy(&log_out.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let sha = parts.next()?.to_owned();
                let ts: i64 = parts.next()?.parse().ok()?;
                noted.contains(&sha).then_some((sha, ts))
            })
            .collect();

        Ok(pairs)
    }

    async fn read_record(&self, commit_sha: &str) -> Result<Option<NoteRecord>> {
        let out = self
            .git()
            .args(["notes", "--ref=spelunk", "show", commit_sha])
            .output()
            .await?;

        if !out.status.success() {
            return Ok(None);
        }

        let json = String::from_utf8_lossy(&out.stdout);
        let record: NoteRecord = serde_json::from_str(json.trim())
            .map_err(|e| anyhow!("parsing spelunk note on {commit_sha}: {e}"))?;
        Ok(Some(record))
    }

    async fn collect(
        &self,
        kind_filter: Option<&str>,
        include_archived: bool,
        as_of: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Note>> {
        let commits = self.noted_commits().await?;
        let mut notes = Vec::new();

        for (sha, _) in commits {
            if notes.len() >= limit {
                break;
            }
            if let Some(record) = self.read_record(&sha).await? {
                if kind_filter.is_some_and(|k| record.kind != k) {
                    continue;
                }
                if !include_archived && record.status == "archived" {
                    continue;
                }
                if let Some(ts) = as_of {
                    let effective = record.valid_at.unwrap_or(record.created_at);
                    if effective > ts {
                        continue;
                    }
                    if record.invalid_at.is_some_and(|ia| ia <= ts) {
                        continue;
                    }
                }
                notes.push(record_to_note(record));
            }
        }

        Ok(notes)
    }
}

fn record_to_note(r: NoteRecord) -> Note {
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
        distance: None,
        score: None,
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[async_trait]
impl MemoryBackend for GitNotesBackend {
    async fn add(&self, input: NoteInput) -> Result<i64> {
        let id = now_millis();
        let record = NoteRecord {
            id,
            kind: input.kind,
            title: input.title,
            body: input.body,
            tags: input.tags,
            linked_files: input.linked_files,
            created_at: now_secs(),
            status: "active".to_string(),
            source_ref: input.source_ref,
            valid_at: input.valid_at,
            invalid_at: None,
            superseded_by: None,
        };

        let json = serde_json::to_string(&record)?;
        let head = self.head_sha().await?;

        let status = self
            .git()
            .args(["notes", "--ref=spelunk", "add", "-f", "-m", &json, &head])
            .status()
            .await?;

        if !status.success() {
            return Err(anyhow!("git notes add failed"));
        }

        Ok(id)
    }

    async fn list(
        &self,
        kind_filter: Option<&str>,
        limit: usize,
        include_archived: bool,
        as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        let effective_limit = limit.min(GIT_NOTES_MAX_LIST);
        if limit > GIT_NOTES_MAX_LIST {
            tracing::warn!(
                "GitNotesBackend::list: caller requested {} entries; capped at {} to prevent \
                 O(n) subprocess hang. Use --backend sqlite for unbounded listing.",
                limit,
                GIT_NOTES_MAX_LIST
            );
        }
        self.collect(kind_filter, include_archived, as_of, effective_limit)
            .await
    }

    async fn list_by_source_ref(
        &self,
        _source_ref_prefix: &str,
        _limit: usize,
        _include_archived: bool,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        Err(crate::error::SpelunkError::BackendUnsupported("list_by_source_ref".into()).into())
    }

    async fn get(&self, id: i64) -> Result<Option<Note>> {
        for (sha, _) in self.noted_commits().await? {
            if let Some(record) = self.read_record(&sha).await?
                && record.id == id
            {
                return Ok(Some(record_to_note(record)));
            }
        }
        Ok(None)
    }

    async fn count(&self) -> Result<i64> {
        Ok(self.noted_commits().await?.len() as i64)
    }

    async fn archive(&self, id: i64) -> Result<bool> {
        for (sha, _) in self.noted_commits().await? {
            if let Some(mut record) = self.read_record(&sha).await?
                && record.id == id
            {
                record.status = "archived".to_string();
                let json = serde_json::to_string(&record)?;
                let status = self
                    .git()
                    .args(["notes", "--ref=spelunk", "add", "-f", "-m", &json, &sha])
                    .status()
                    .await?;
                return Ok(status.success());
            }
        }
        Ok(false)
    }

    // ── Unsupported ──────────────────────────────────────────────────────────

    async fn search_timeline(&self, _query_blob: &[u8], _limit: usize) -> Result<Vec<Note>> {
        Err(crate::error::SpelunkError::BackendUnsupported("search_timeline".into()).into())
    }

    async fn search(
        &self,
        _query_blob: &[u8],
        _limit: usize,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        Err(crate::error::SpelunkError::BackendUnsupported("search".into()).into())
    }

    async fn search_text(
        &self,
        _query: &str,
        _limit: usize,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        Err(crate::error::SpelunkError::BackendUnsupported("search_text".into()).into())
    }

    async fn search_hybrid(
        &self,
        _query_blob: &[u8],
        _query: &str,
        _limit: usize,
        _as_of: Option<i64>,
    ) -> Result<Vec<Note>> {
        Err(crate::error::SpelunkError::BackendUnsupported("search_hybrid".into()).into())
    }

    async fn supersede(&self, _old_id: i64, _new_id: i64) -> Result<bool> {
        Err(crate::error::SpelunkError::BackendUnsupported("supersede".into()).into())
    }

    async fn harvested_shas(&self) -> Result<HashSet<String>> {
        Err(crate::error::SpelunkError::BackendUnsupported("harvested_shas".into()).into())
    }

    async fn has_source_ref(&self, _sha: &str) -> Result<bool> {
        Err(crate::error::SpelunkError::BackendUnsupported("has_source_ref".into()).into())
    }

    async fn add_edge(&self, _from_id: i64, _to_id: i64, _kind: &str) -> Result<()> {
        Err(crate::error::SpelunkError::BackendUnsupported("add_edge".into()).into())
    }

    async fn get_edges(&self, _id: i64) -> Result<(Vec<MemoryEdge>, Vec<MemoryEdge>)> {
        Err(crate::error::SpelunkError::BackendUnsupported("get_edges".into()).into())
    }
}
