use anyhow::{Result, anyhow};
use std::collections::HashSet;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::memory::Note;
use super::note_record::{NoteRecord, record_to_note};

mod backend_impl;
mod lock;

pub use lock::{NotesLock, lock_notes};

// ── Carry config: surviving history rewrites ─────────────────────────────────

/// The ref spelunk stores memory notes on.
const SPELUNK_NOTES_REF: &str = "refs/notes/spelunk";

/// The tracking ref `git fetch` populates, per the refspec `spelunk init`
/// configures. Fetching straight onto [`SPELUNK_NOTES_REF`] would force-update
/// it and silently destroy local unpushed notes (ADR-069 D4).
const SPELUNK_TRACKING_REF: &str = "refs/notes/origin/spelunk";

/// The namespace git is willing to rewrite notes in.
const NOTES_NAMESPACE: &str = "refs/notes/";

/// What [`ensure_notes_rewrite_ref`] found or did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteRefStatus {
    /// This call added the setting; announce it once.
    Configured,
    /// Already named by an existing value (exactly, or via a glob).
    AlreadyCovered,
    /// Could not be set; the reason is logged. Entries stay at risk.
    Failed,
}

/// Point `notes.rewriteRef` at spelunk's notes ref in this repo.
///
/// Gotcha: git carries a note onto a rewritten commit (`commit --amend`,
/// `rebase`) only if `notes.rewriteRef` names the ref, and it has **no**
/// built-in default, so an unconfigured repo silently orphans every entry.
/// Pre-`init` git notes is the sole store, making that total loss.
///
/// `notes.rewriteMode` is deliberately left alone: its `concatenate` default
/// keeps every JSON line, whereas `overwrite` and `ignore` each drop one side
/// of a squashed pair, causing the loss this is meant to prevent.
///
/// Never returns an error: the write it guards may be an entry's only copy, so
/// a config failure must not sink it.
pub async fn ensure_notes_rewrite_ref(git_root: Option<&std::path::Path>) -> RewriteRefStatus {
    // Reads local, global and system scopes, so a user who set this themselves
    // anywhere is left alone. Absent (exit 1) means unset, not an error.
    let existing = run_git(git_root, &["config", "--get-all", "notes.rewriteRef"])
        .await
        .unwrap_or_default();
    if existing.lines().any(rewrite_ref_covers_spelunk) {
        return RewriteRefStatus::AlreadyCovered;
    }

    // Multi-valued: `--add` composes with any value the user already has, and
    // writes to the repo-local config (never global).
    match run_git(
        git_root,
        &["config", "--add", "notes.rewriteRef", SPELUNK_NOTES_REF],
    )
    .await
    {
        Ok(_) => RewriteRefStatus::Configured,
        Err(e) => {
            tracing::warn!(
                "could not set notes.rewriteRef ({e}); memory will not survive \
                 `git commit --amend` or `git rebase`"
            );
            RewriteRefStatus::Failed
        }
    }
}

/// Whether an existing `notes.rewriteRef` value already names spelunk's ref.
///
/// Values may be globs. git refuses to rewrite notes outside `refs/notes/`, so
/// a glob only counts while it stays inside that namespace: `refs/notes/*`
/// covers us, `refs/*` does not. A false negative only re-adds the exact ref,
/// which stays correct, so matching a trailing `*` is enough.
fn rewrite_ref_covers_spelunk(value: &str) -> bool {
    let value = value.trim();
    if value == SPELUNK_NOTES_REF {
        return true;
    }
    value.strip_suffix('*').is_some_and(|prefix| {
        prefix.starts_with(NOTES_NAMESPACE) && SPELUNK_NOTES_REF.starts_with(prefix)
    })
}

// ── Write-through helper (free function) ─────────────────────────────────────

/// Append a `NoteRecord` as a JSON line to `refs/notes/spelunk` on HEAD.
///
/// Read-modify-write with append semantics: the existing blob is read and its
/// lines (spelunk records and foreign content alike) are preserved verbatim;
/// the new record is appended as one JSON line; the combined text is written
/// back with `git notes add -f`.
///
/// Serialized end to end by [`lock_notes`]; without it a concurrent writer
/// reads the same body and silently drops this entry on write-back (#185).
///
/// Errors are intentionally non-fatal: the caller should log `tracing::warn!`
/// and continue.  On success it returns the [`RewriteRefStatus`] of the carry
/// config ensured along the way, so a CLI caller can announce it once.
///
/// # Arguments
/// * `git_root` — directory passed to `git -C`; `None` uses the process CWD.
/// * `record` — the entry to append.
pub async fn append_to_git_notes(
    git_root: Option<&std::path::Path>,
    record: &NoteRecord,
) -> Result<RewriteRefStatus> {
    // Touches `git config` only, never the notes ref, so it stays outside the
    // lock: serializing it would widen the guarded section for nothing.
    let rewrite_ref = ensure_notes_rewrite_ref(git_root).await;

    // Guard all four steps. Contention must never fail the caller's write, so
    // an unavailable lock degrades to the pre-#185 unserialized behaviour.
    let _lock = lock_notes(git_root).await;

    // ── 1. Get HEAD sha ───────────────────────────────────────────────────────
    let head = run_git(git_root, &["rev-parse", "HEAD"])
        .await
        .map(|s| s.trim().to_string())?;

    // ── 2. Read existing note (may not exist) ─────────────────────────────────
    let existing = run_git(git_root, &["notes", "--ref=spelunk", "show", "--", &head])
        .await
        .unwrap_or_default();

    // ── 3. Append new entry ───────────────────────────────────────────────────
    let new_line = serde_json::to_string(record)?;

    let combined = if existing.trim().is_empty() {
        new_line
    } else {
        format!("{}\n{}", existing.trim_end_matches('\n'), new_line)
    };

    // ── 4. Write back ─────────────────────────────────────────────────────────
    // The note body is passed via stdin (`-F -`) rather than as a `-m` argv
    // value: this keeps arbitrary/attacker-influenced note content off the
    // process argv (and therefore out of `ps`/process-list visibility) and
    // means the body can never be misparsed as an option, regardless of its
    // contents. `--` guards the trailing `<object>` (HEAD sha) so it can't be
    // interpreted as an option either, even though `head` is always a
    // `rev-parse`-verified sha here.
    run_git_with_stdin(
        git_root,
        &[
            "notes",
            "--ref=spelunk",
            "add",
            "-f",
            "-F",
            "-",
            "--",
            &head,
        ],
        &combined,
    )
    .await?;

    Ok(rewrite_ref)
}

// ── Read-path merge: making fetched notes visible ────────────────────────────

/// What [`merge_tracking_notes`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotesMergeOutcome {
    /// The merge ran; any fetched entries are now on the working ref.
    Merged,
    /// Nothing to merge, or the merge failed. The caller reads regardless.
    Skipped,
    /// The lock was unavailable, so the merge was skipped. The union is
    /// idempotent, so the next read catches up.
    LockUnavailable,
}

/// Merge fetched teammate notes ([`SPELUNK_TRACKING_REF`]) into the working ref
/// so `memory list` / `context` can see them.
///
/// Does **no** network. It merges only what the user's own `git fetch` already
/// wrote, which is what lets reads work with the remote unreachable and keeps
/// egress off a path the user never pointed at a remote (ADR-069 D5).
///
/// Never fails the caller: a read must not break because the merge could not
/// run. A missing tracking ref is nothing to do (git exits 128 when both refs
/// are empty, which is the un-fetched solo case), and an unavailable lock skips
/// the merge rather than waiting the caller out.
pub async fn merge_tracking_notes(git_root: Option<&std::path::Path>) -> NotesMergeOutcome {
    // Without this, a concurrent `append_to_git_notes` read-modify-write
    // silently overwrites the merged entries (#185 / ADR-069 D6).
    let Some(_lock) = lock_notes(git_root).await else {
        return NotesMergeOutcome::LockUnavailable;
    };

    // `-s` is explicit on every call: the `notes.mergeStrategy` default is
    // `manual`, which exits 1 and leaves a stuck `.git/NOTES_MERGE_WORKTREE`.
    // The user's own setting is never written.
    match run_git(
        git_root,
        &[
            "notes",
            "--ref=spelunk",
            "merge",
            "-s",
            "cat_sort_uniq",
            SPELUNK_TRACKING_REF,
        ],
    )
    .await
    {
        Ok(_) => NotesMergeOutcome::Merged,
        Err(e) => {
            tracing::debug!("notes merge from {SPELUNK_TRACKING_REF} skipped: {e}");
            NotesMergeOutcome::Skipped
        }
    }
}

/// Run a git subprocess, optionally in `dir`, and return stdout as a `String`.
/// Returns `Err` if the process fails.
async fn run_git(dir: Option<&std::path::Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd.args(args).output().await?;
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

/// Run a git subprocess, optionally in `dir`, writing `stdin_data` to its
/// stdin and returning stdout as a `String`. Used with `-F -` invocations so
/// note bodies never appear on argv.
async fn run_git_with_stdin(
    dir: Option<&std::path::Path>,
    args: &[&str],
    stdin_data: &str,
) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open stdin for git {}", args.join(" ")))?;
        stdin.write_all(stdin_data.as_bytes()).await?;
        // Drop closes stdin so git sees EOF.
    }
    let out = child.wait_with_output().await?;
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

/// Hard cap on entries returned by `list()`.
///
/// Each entry requires one `git notes show` subprocess call (~13 ms).
/// Without a guard, `list(5000)` would take ~65 seconds.
/// Callers needing unbounded listing should use `--backend sqlite`.
const GIT_NOTES_MAX_LIST: usize = 500;

/// Memory backend backed by `git notes` in the `refs/notes/spelunk` namespace.
///
/// The note on a commit is JSON Lines: one `NoteRecord` per line, possibly
/// interleaved with foreign content (prose, other tools' lines). Reads skip
/// foreign lines; writes preserve them and every sibling record verbatim.
/// Multiple entries accumulate within a commit's note and across commits.
///
/// # Concurrency
/// `add`/`archive` do read-modify-write and rewrite the note with
/// `git notes add -f`. Each is serialized by [`lock_notes`], which is keyed on
/// the git **common** dir so that worktrees sharing one notes ref contend on
/// one lock (#185).
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

    /// Write a spelunk note body to `object` via `git notes add -f -F - --
    /// <object>`, passing `body` over stdin. Keeps note content (which may
    /// contain arbitrary user/LLM text) off argv, and the `--` separator
    /// stops `object` from being parsed as an option.
    async fn add_note_stdin(&self, object: &str, body: &str) -> Result<()> {
        let mut cmd = self.git();
        cmd.args([
            "notes",
            "--ref=spelunk",
            "add",
            "-f",
            "-F",
            "-",
            "--",
            object,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow!("failed to open stdin for git notes add"))?;
            stdin.write_all(body.as_bytes()).await?;
        }
        let out = child.wait_with_output().await?;
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!(
                "git notes add failed: {}",
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

    /// Read the raw note blob for `commit_sha` (empty string if no note).
    async fn read_note_blob(&self, commit_sha: &str) -> Result<String> {
        let out = self
            .git()
            .args(["notes", "--ref=spelunk", "show", "--", commit_sha])
            .output()
            .await?;
        if !out.status.success() {
            return Ok(String::new());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Permissively parse the spelunk records from a commit's note blob.
    ///
    /// The blob is JSON Lines interleaved with foreign content (prose, other
    /// tools' lines). Foreign lines are skipped without error; only a record
    /// from a newer, incompatible `schema_version` returns an error.
    async fn read_records(&self, commit_sha: &str) -> Result<Vec<NoteRecord>> {
        let blob = self.read_note_blob(commit_sha).await?;
        let mut records = Vec::new();
        for line in blob.lines() {
            match parse_spelunk_line(line) {
                Some(record) => {
                    if record.schema_version > 1 {
                        return Err(anyhow::Error::new(
                            crate::error::SpelunkError::SchemaMismatch {
                                found: record.schema_version,
                                max_known: 1,
                            },
                        ));
                    }
                    records.push(record);
                }
                None => continue, // foreign line: skip, never error
            }
        }
        // `cat_sort_uniq` unions lines lexicographically, so after a merge blob
        // order is not chronological (ADR-069 D2). Stable: ties keep blob order.
        records.sort_by_key(|r| r.created_at);
        Ok(records)
    }

    /// Append `record` as a new JSON line to `object`'s note, preserving every
    /// existing line (spelunk records and foreign content) byte-for-byte.
    async fn append_record(&self, object: &str, record: &NoteRecord) -> Result<()> {
        // git notes is the primary store on this path (`--backend git-notes`),
        // so an unconfigured carry ref orphans the only copy. Status is dropped:
        // this path has no command output to announce on.
        ensure_notes_rewrite_ref(self.git_root.as_deref()).await;

        let _lock = lock_notes(self.git_root.as_deref()).await;

        let existing = self.read_note_blob(object).await?;
        let new_line = serde_json::to_string(record)?;
        let combined = if existing.trim().is_empty() {
            new_line
        } else {
            format!("{}\n{}", existing.trim_end_matches('\n'), new_line)
        };
        self.add_note_stdin(object, &combined).await
    }

    /// Set `status = "archived"` on the single spelunk record whose `id` matches
    /// `id` within `object`'s note. Every other line (sibling records and
    /// foreign content) is re-emitted unchanged in its original position; only
    /// the matched record's line is re-serialized. Returns whether a match was
    /// rewritten.
    async fn archive_record(&self, object: &str, id: i64) -> Result<bool> {
        let _lock = lock_notes(self.git_root.as_deref()).await;

        let blob = self.read_note_blob(object).await?;
        let mut out_lines: Vec<String> = Vec::new();
        let mut changed = false;
        for line in blob.lines() {
            match parse_spelunk_line(line) {
                Some(mut record) if !changed && record.id == id => {
                    record.status = "archived".to_string();
                    out_lines.push(serde_json::to_string(&record)?);
                    changed = true;
                }
                // Foreign lines and untargeted records: re-emit verbatim.
                _ => out_lines.push(line.to_string()),
            }
        }
        if changed {
            self.add_note_stdin(object, &out_lines.join("\n")).await?;
        }
        Ok(changed)
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

        'outer: for (sha, _) in commits {
            for record in self.read_records(&sha).await? {
                if notes.len() >= limit {
                    break 'outer;
                }
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

/// Classify one line of a note blob: `Some(record)` if it parses as a JSON
/// *object* deserializing into `NoteRecord`. Non-JSON, non-object JSON, blank,
/// and prose lines are foreign (`None`).
fn parse_spelunk_line(line: &str) -> Option<NoteRecord> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Must be a JSON object; arrays/strings/numbers/null are foreign.
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    if !value.is_object() {
        return None;
    }
    serde_json::from_value(value).ok()
}
