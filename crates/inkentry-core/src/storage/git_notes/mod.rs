use anyhow::{Context, Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::entity_id::note_entity_id;
use super::memory::Note;
use super::note_record::{CarriedEdge, NoteRecord, now_millis, now_secs, record_to_note};
use fold::fold_records;

mod backend_impl;
mod fold;
mod lock;
mod publish;
mod refs;

pub use lock::{LOCK_WAIT_BUDGET, LockAttempt, NotesLock, lock_notes};
pub use publish::{PublishOutcome, SkipReason, publish_notes};
pub use refs::NotesRefs;

const INKENTRY_NOTES_REF: &str = "refs/notes/inkentry";

// Populated by git fetch (refspec set by inkentry init). Fetching directly
// onto INKENTRY_NOTES_REF would force-update it and destroy local unpushed
// notes.
const INKENTRY_TRACKING_REF: &str = "refs/notes/origin/inkentry";

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

/// Points `notes.rewriteRef` at inkentry's notes ref in this repo.
///
/// Without this, git only carries a note onto a rewritten commit (`commit
/// --amend`, `rebase`) if `notes.rewriteRef` names it; there is no built-in
/// default, so an unconfigured repo silently orphans every entry.
///
/// `notes.rewriteMode` is left alone: `concatenate` (the default) keeps every
/// JSON line, while `overwrite`/`ignore` would drop one side of a squashed
/// pair.
///
/// Never returns an error: a config failure must not sink the write it guards.
pub async fn ensure_notes_rewrite_ref(git_root: Option<&std::path::Path>) -> RewriteRefStatus {
    // Reads all config scopes; exit 1 means unset, not an error.
    let existing = run_git(git_root, &["config", "--get-all", "notes.rewriteRef"])
        .await
        .unwrap_or_default();
    if existing.lines().any(rewrite_ref_covers_inkentry) {
        return RewriteRefStatus::AlreadyCovered;
    }

    // `--add` composes with any value the user already has, and writes to the
    // repo-local config, never global.
    match run_git(
        git_root,
        &["config", "--add", "notes.rewriteRef", INKENTRY_NOTES_REF],
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

// Values may be globs; git only rewrites notes under refs/notes/, so a glob
// counts only if it stays within that namespace (refs/notes/* covers us,
// refs/* does not). A false negative just re-adds the exact ref, so matching
// a trailing `*` is sufficient.
fn rewrite_ref_covers_inkentry(value: &str) -> bool {
    let value = value.trim();
    if value == INKENTRY_NOTES_REF {
        return true;
    }
    value.strip_suffix('*').is_some_and(|prefix| {
        prefix.starts_with(NOTES_NAMESPACE) && INKENTRY_NOTES_REF.starts_with(prefix)
    })
}

// How a writer holds, or legitimately does not hold, the notes lock. Unlocked
// is a returned value (not just a `tracing::warn!`) so a caller can surface a
// degradation that would otherwise be invisible without RUST_LOG.
#[must_use]
enum WriterLock {
    // Held until dropped.
    Held { _guard: NotesLock },
    // The lock cannot exist here; the write proceeds unserialized.
    Unlocked { path: PathBuf, reason: String },
}

// Holds the notes lock for a writer, or fails, except where the lock cannot
// exist at all, which degrades to unlocked (logged) rather than failing.
// `Err` is either contention (someone else holds it) or a failed path
// resolution.
async fn writer_lock(git_root: Option<&std::path::Path>) -> Result<WriterLock> {
    match lock_notes(git_root).await? {
        LockAttempt::Acquired(guard) => Ok(WriterLock::Held { _guard: guard }),
        LockAttempt::Contended { path } => Err(anyhow!(
            "the git notes lock ({}) stayed held by other writers for over {:?}; \
             not writing without it, because an unserialized write can silently \
             erase a concurrent writer's entry. Retry the command (many \
             concurrent writers can exceed the wait legitimately); if it \
             persists with nothing else running, a inkentry or git process is \
             stuck holding the lock (it frees itself when that process exits)",
            path.display(),
            lock::LOCK_WAIT_BUDGET,
        )),
        LockAttempt::Unusable { path, reason } => {
            tracing::warn!(
                "git notes lock {} unusable ({reason}); writing without \
                 serialization, so a concurrent memory write could be lost",
                path.display()
            );
            Ok(WriterLock::Unlocked { path, reason })
        }
    }
}

const NOTE_READ_ATTEMPTS: u32 = 4;

const NOTE_READ_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

// Retries only genuine read failures, never "no note found": since the read
// has no side effects, retrying can't double-apply anything, and a
// persistent failure still reaches the caller as Err rather than being
// mistaken for an empty note.
async fn read_note_body_with_retry(
    git_root: Option<&std::path::Path>,
    object: &str,
) -> Result<Option<String>> {
    let mut last_err = None;
    for attempt in 1..=NOTE_READ_ATTEMPTS {
        match read_note_body(git_root, object).await {
            Ok(body) => return Ok(body),
            Err(e) => {
                tracing::warn!(
                    "reading existing note failed (attempt {attempt}/{NOTE_READ_ATTEMPTS}): {e}"
                );
                last_err = Some(e);
                if attempt < NOTE_READ_ATTEMPTS {
                    tokio::time::sleep(NOTE_READ_BACKOFF * attempt).await;
                }
            }
        }
    }
    Err(last_err.expect("at least one attempt ran"))
}

// Distinguishes "no note" (None) from a failed read (Err): a writer that
// mistakes a failed read for "no note yet" would rewrite the note as just
// its own line, erasing every sibling entry. Matches on the exit code ("no
// note found" exits 1; infrastructure failures die with 128), not the
// message, which is localized.
async fn read_note_body(
    git_root: Option<&std::path::Path>,
    object: &str,
) -> Result<Option<String>> {
    let mut cmd = Command::new("git");
    if let Some(d) = git_root {
        cmd.current_dir(d);
    }
    let out = cmd
        .args(["notes", "--ref=inkentry", "show", "--", object])
        .output()
        .await?;

    if out.status.success() {
        return Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()));
    }
    if out.status.code() == Some(1) {
        return Ok(None);
    }
    Err(anyhow!(
        "git notes show -- {object}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// What [`append_to_git_notes`] did, beyond writing the entry.
#[derive(Debug)]
pub struct AppendOutcome {
    /// The carry-config status ensured along the way; a caller announces it once.
    pub rewrite_ref: RewriteRefStatus,
    /// Set when the write proceeded without the notes lock (the one
    /// filesystem where the lock cannot exist). The caller should show this
    /// to the user: it is the only channel that works without `RUST_LOG`.
    pub lock_degradation: Option<String>,
}

/// Appends a `NoteRecord` as a JSON line to `refs/notes/inkentry` on HEAD.
///
/// Read-modify-write with append semantics: the existing blob's lines
/// (inkentry records and foreign content alike) are preserved verbatim, the
/// new record is appended as one JSON line, and the result is written back
/// with `git notes add -f`.
///
/// Serialized end to end by [`lock_notes`]; without it a concurrent writer
/// can read the same body and silently drop this entry on write-back. A
/// contended lock is an `Err` and nothing is written; only a lock that cannot
/// exist on this filesystem degrades to an unlocked write, reported in
/// [`AppendOutcome::lock_degradation`].
///
/// # Arguments
/// * `git_root` — directory passed to `git -C`; `None` uses the process CWD.
/// * `record` — the entry to append.
pub async fn append_to_git_notes(
    git_root: Option<&std::path::Path>,
    record: &NoteRecord,
) -> Result<AppendOutcome> {
    let head = run_git(git_root, &["rev-parse", "HEAD"])
        .await
        .map(|s| s.trim().to_string())?;
    append_to_git_notes_at(git_root, &head, record).await
}

// append_to_git_notes against an explicit target object rather than HEAD —
// the one other place a note attaches to a specific commit (an anchor record
// attaches to the claimed commit, resolved independently of the process's
// own HEAD).
async fn append_to_git_notes_at(
    git_root: Option<&std::path::Path>,
    target: &str,
    record: &NoteRecord,
) -> Result<AppendOutcome> {
    // Touches `git config` only, never the notes ref, so it stays outside the
    // lock: serializing it would widen the guarded section for nothing.
    let rewrite_ref = ensure_notes_rewrite_ref(git_root).await;

    // Guards all four steps below. Bind the whole enum: `Held`'s guard must
    // live to the end of the function.
    let lock = writer_lock(git_root).await?;
    let lock_degradation = match &lock {
        WriterLock::Held { .. } => None,
        WriterLock::Unlocked { path, reason } => Some(format!(
            "wrote to git notes without the cross-process lock (lock file {} \
             unusable: {reason}); concurrent memory writes in this repo can \
             lose entries",
            path.display()
        )),
    };

    let existing = read_note_body_with_retry(git_root, target)
        .await
        .context("could not read the existing note, so not overwriting it")?;

    let new_line = serde_json::to_string(record)?;

    let combined = match existing {
        Some(body) if !body.trim().is_empty() => {
            format!("{}\n{}", body.trim_end_matches('\n'), new_line)
        }
        _ => new_line,
    };

    // Passed via stdin (`-F -`) rather than `-m` argv: keeps note content off
    // argv/`ps` visibility and avoids it being misparsed as an option. `--`
    // guards the trailing object (target sha) the same way, though target is
    // always a rev-parse-verified sha here.
    run_git_with_stdin(
        git_root,
        &[
            "notes",
            "--ref=inkentry",
            "add",
            "-f",
            "-F",
            "-",
            "--",
            target,
        ],
        &combined,
    )
    .await?;

    Ok(AppendOutcome {
        rewrite_ref,
        lock_degradation,
    })
}

/// What [`append_new_to_git_notes`] did.
#[derive(Debug)]
pub struct BatchAppendOutcome {
    /// The carry-config status ensured along the way; a caller announces it once.
    pub rewrite_ref: RewriteRefStatus,
    /// Set when the write proceeded without the notes lock, exactly as
    /// [`AppendOutcome::lock_degradation`].
    pub lock_degradation: Option<String>,
    /// Records appended to HEAD's note.
    pub written: usize,
    /// Records whose entity was already on the ref, so nothing was appended
    /// for them.
    pub already_carried: usize,
}

/// Appends every record in `records` not already on `refs/notes/inkentry`, as
/// JSON lines on HEAD's note, in one read-modify-write under a single lock.
///
/// Differs from calling [`append_to_git_notes`] per record in two ways: it
/// takes the lock once for the whole batch instead of once per record, and it
/// skips any record whose entity is already on the ref (checked by entity id,
/// not by presence in the local store, so a re-import does not duplicate the
/// entry on every run).
///
/// A ref that cannot be read is an `Err` and nothing is written: an unreadable
/// ref cannot be told apart from an empty one, so treating it as empty risks
/// writing a duplicate.
pub async fn append_new_to_git_notes(
    git_root: Option<&std::path::Path>,
    records: &[NoteRecord],
) -> Result<BatchAppendOutcome> {
    let rewrite_ref = ensure_notes_rewrite_ref(git_root).await;

    let lock = writer_lock(git_root).await?;
    let lock_degradation = match &lock {
        WriterLock::Held { .. } => None,
        WriterLock::Unlocked { path, reason } => Some(format!(
            "wrote to git notes without the cross-process lock (lock file {} \
             unusable: {reason}); concurrent memory writes in this repo can \
             lose entries",
            path.display()
        )),
    };

    let backend = match git_root {
        Some(root) => GitNotesBackend::with_root(root.to_path_buf()),
        None => GitNotesBackend::new(),
    };
    // Every reachable note, not just HEAD's — an entry may have been written
    // on any commit. Reads take no lock, so this can't re-enter the one held
    // above.
    let carried: HashSet<String> = backend
        .folded_records()
        .await
        .context("could not read the entries already on the notes ref, so not writing to it")?
        .iter()
        .map(NoteRecord::resolve_entity_id)
        .collect();

    let fresh: Vec<&NoteRecord> = records
        .iter()
        .filter(|r| !carried.contains(&r.resolve_entity_id()))
        .collect();
    let already_carried = records.len() - fresh.len();
    if fresh.is_empty() {
        return Ok(BatchAppendOutcome {
            rewrite_ref,
            lock_degradation,
            written: 0,
            already_carried,
        });
    }

    let head = run_git(git_root, &["rev-parse", "HEAD"])
        .await
        .map(|s| s.trim().to_string())?;

    let existing = read_note_body_with_retry(git_root, &head)
        .await
        .context("could not read the existing note, so not overwriting it")?;

    let appended = fresh
        .iter()
        .map(serde_json::to_string)
        .collect::<serde_json::Result<Vec<_>>>()?
        .join("\n");

    let combined = match existing {
        Some(body) if !body.trim().is_empty() => {
            format!("{}\n{}", body.trim_end_matches('\n'), appended)
        }
        _ => appended,
    };

    // Body over stdin and `--` before the object, for the reasons
    // `append_to_git_notes` sets out at its own write.
    run_git_with_stdin(
        git_root,
        &[
            "notes",
            "--ref=inkentry",
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

    Ok(BatchAppendOutcome {
        rewrite_ref,
        lock_degradation,
        written: fresh.len(),
        already_carried,
    })
}

/// Appends a state-update record for an entity already on the carrier:
/// `base` supplies its content unchanged, while `status`, `invalid_at` and
/// `superseded_by_entity_id` override its mutable state.
///
/// Never rewrites the entity's existing line(s) in place — doing so can leave
/// a second machine with both the rewritten and its own stale original line
/// after a notes merge, so the entity would appear twice with conflicting
/// status. Appending a new line and folding same-`entity_id` copies at read
/// time ([`fold_records`]) converges regardless of merge order instead, since
/// the fold's archival rule is monotonic.
///
/// This call always changes mutable state, so it must never be skipped as a
/// no-op the way an unchanged re-record is: nothing here suppresses a
/// same-`entity_id` append, and a guard that did would silently swallow every
/// state update this function writes.
pub async fn append_state_update(
    git_root: Option<&std::path::Path>,
    base: &Note,
    status: &str,
    invalid_at: Option<i64>,
    superseded_by_entity_id: Option<String>,
) -> Result<AppendOutcome> {
    let record = entity_update_record(
        base,
        status,
        invalid_at,
        superseded_by_entity_id,
        vec![],
        None,
        None,
    );
    append_to_git_notes(git_root, &record).await
}

/// Appends a record carrying `edges` as outgoing edges of `base`'s entity,
/// its mutable state left unchanged.
///
/// Same append-only shape as [`append_state_update`]: the fold unions edge
/// lists across copies, so an edge recorded here converges regardless of
/// merge order. `supersedes` is never passed here; it travels as
/// `superseded_by_entity_id`.
pub async fn append_edges(
    git_root: Option<&std::path::Path>,
    base: &Note,
    edges: Vec<CarriedEdge>,
) -> Result<AppendOutcome> {
    let record = entity_update_record(base, &base.status, base.invalid_at, None, edges, None, None);
    append_to_git_notes(git_root, &record).await
}

/// Appends an anchor record for `base`'s entity to `target_commit` (the
/// claimed commit — not necessarily `HEAD`, unlike every other append in this
/// module). Carries `base`'s content unchanged, like [`append_state_update`],
/// so it loses nothing even if this entity's own fold group picked it as the
/// base.
pub async fn append_anchor_record(
    git_root: Option<&std::path::Path>,
    target_commit: &str,
    base: &Note,
    patch_id: Option<String>,
) -> Result<AppendOutcome> {
    let record = entity_update_record(
        base,
        &base.status,
        base.invalid_at,
        None,
        vec![],
        Some("anchor".to_string()),
        patch_id,
    );
    append_to_git_notes_at(git_root, target_commit, &record).await
}

// The record an entity update appends: base's content unchanged, keyed by
// its entity_id, with the mutable state the caller supplies.
#[allow(clippy::too_many_arguments)]
fn entity_update_record(
    base: &Note,
    status: &str,
    invalid_at: Option<i64>,
    superseded_by_entity_id: Option<String>,
    edges: Vec<CarriedEdge>,
    op: Option<String>,
    patch_id: Option<String>,
) -> NoteRecord {
    NoteRecord {
        schema_version: 1,
        id: now_millis(),
        kind: base.kind.clone(),
        title: base.title.clone(),
        body: base.body.clone(),
        tags: base.tags.clone(),
        linked_files: base.linked_files.clone(),
        created_at: now_secs(),
        status: status.to_string(),
        source_ref: base.source_ref.clone(),
        valid_at: base.valid_at,
        invalid_at,
        // Machine-local rowid link: never populated here; this path keys
        // entities by entity_id only.
        superseded_by: None,
        remote_id: None,
        entity_id: Some(note_entity_id(base)),
        superseded_by_entity_id,
        edges,
        // Carried forward defensively: the fold never picks a state-update
        // record as its group's base, so this value is not what a reader
        // ultimately sees.
        origin: base.origin.clone(),
        op,
        patch_id,
    }
}

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

/// Merges fetched teammate notes into the working ref so `memory list` /
/// `context` can see them.
///
/// Does no network: it merges only what the user's own `git fetch` already
/// wrote, so reads still work with the remote unreachable.
///
/// Never fails the caller: a read must not break because the merge could not
/// run. A missing tracking ref is nothing to do (the un-fetched, solo case),
/// and an unavailable lock skips the merge rather than making the caller wait.
pub async fn merge_tracking_notes(git_root: Option<&std::path::Path>) -> NotesMergeOutcome {
    // Without this, a concurrent append_to_git_notes read-modify-write could
    // silently overwrite the merged entries. Unlike a writer, every
    // non-acquired outcome just skips: the union is idempotent, so the next
    // read catches up, and a read must never fail over the lock.
    let _lock = match lock_notes(git_root).await {
        Ok(LockAttempt::Acquired(guard)) => guard,
        Ok(LockAttempt::Contended { .. }) | Ok(LockAttempt::Unusable { .. }) => {
            return NotesMergeOutcome::LockUnavailable;
        }
        Err(e) => {
            tracing::debug!("notes merge skipped, lock path unresolved: {e}");
            return NotesMergeOutcome::LockUnavailable;
        }
    };

    // `-s` is explicit on every call: the `notes.mergeStrategy` default is
    // `manual`, which exits 1 and leaves a stuck `.git/NOTES_MERGE_WORKTREE`.
    // The user's own setting is never written.
    match run_git(
        git_root,
        &[
            "notes",
            "--ref=inkentry",
            "merge",
            "-s",
            "cat_sort_uniq",
            INKENTRY_TRACKING_REF,
        ],
    )
    .await
    {
        Ok(_) => NotesMergeOutcome::Merged,
        Err(e) => {
            tracing::debug!("notes merge from {INKENTRY_TRACKING_REF} skipped: {e}");
            NotesMergeOutcome::Skipped
        }
    }
}

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

// Writes stdin_data to the subprocess's stdin, so callers can pass note
// bodies via `-F -` without putting them on argv.
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

/// Resolves "earliest reachable, falling back to earliest" over a set of
/// `(commit, created_at)` anchors — see
/// [`GitNotesBackend::anchors_for_entity`].
pub fn resolve_source_ref(
    anchors: &[(String, i64)],
    reachable: impl Fn(&str) -> bool,
) -> Option<String> {
    fold::resolve_source_ref(anchors, reachable)
}

/// Whether `ancestor` is `descendant` itself or one of its ancestors along
/// `git`'s ordinary (all-parents) DAG. `false` on any git failure (an unknown
/// object, for instance) rather than propagating: every caller uses this as
/// one input to a best-effort claim or reconciliation decision, never as the
/// sole gate on a write that must fail loudly.
pub async fn is_ancestor(
    git_root: Option<&std::path::Path>,
    ancestor: &str,
    descendant: &str,
) -> bool {
    let mut cmd = Command::new("git");
    if let Some(d) = git_root {
        cmd.current_dir(d);
    }
    cmd.args(["merge-base", "--is-ancestor", ancestor, descendant]);
    matches!(cmd.status().await, Ok(status) if status.success())
}

/// Every commit reachable from a local branch, tag or remote-tracking branch.
/// Deliberately excludes `refs/notes/*`, whose own commit-shaped history is
/// not the code DAG this check cares about.
pub async fn commits_reachable_from_any_ref(
    git_root: Option<&std::path::Path>,
) -> Result<HashSet<String>> {
    match run_git(
        git_root,
        &["rev-list", "--branches", "--tags", "--remotes", "HEAD"],
    )
    .await
    {
        Ok(out) => Ok(out.lines().map(str::trim).map(str::to_string).collect()),
        // No branches/tags/remotes and an unborn HEAD both exit non-zero here;
        // either way there is nothing reachable to report.
        Err(_) => Ok(HashSet::new()),
    }
}

/// `git patch-id --stable` of `sha`'s diff, or `None` for a merge commit. A
/// root commit (no parent) still gets one, diffed against the empty tree,
/// exactly as `git show`/`git patch-id` already do for it.
pub async fn commit_patch_id(
    git_root: Option<&std::path::Path>,
    sha: &str,
) -> Result<Option<String>> {
    let parents = run_git(git_root, &["rev-list", "--parents", "-1", sha]).await?;
    let parent_count = parents.split_whitespace().count().saturating_sub(1);
    if parent_count > 1 {
        return Ok(None);
    }

    let diff = run_git(git_root, &["show", "--no-color", "-p", sha]).await?;

    let mut cmd = Command::new("git");
    if let Some(d) = git_root {
        cmd.current_dir(d);
    }
    cmd.args(["patch-id", "--stable"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open stdin for git patch-id"))?;
    let writer = async move {
        stdin.write_all(diff.as_bytes()).await?;
        stdin.shutdown().await
    };
    let (write_res, out) = tokio::join!(writer, child.wait_with_output());
    let out = out?;
    write_res?;
    if !out.status.success() {
        anyhow::bail!(
            "git patch-id --stable: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Output is "<patch-id> <commit-sha>"; an empty diff (a genuinely empty
    // commit) prints nothing.
    Ok(stdout.split_whitespace().next().map(str::to_string))
}

// Hard cap on entries returned by list(); bounds output only, not work: the
// fold still reads every reachable note blob regardless. Use --backend
// sqlite for unbounded listing.
const GIT_NOTES_MAX_LIST: usize = 500;

/// Memory backend backed by `git notes` in the `refs/notes/inkentry` namespace.
///
/// The note on a commit is JSON Lines: one `NoteRecord` per line, possibly
/// interleaved with foreign content (prose, other tools' lines). Reads skip
/// foreign lines; writes preserve them and every sibling record verbatim.
/// Multiple entries accumulate within a commit's note and across commits.
///
/// # Concurrency
/// `add` and `archive` both do read-modify-write and rewrite the note with
/// `git notes add -f`, appending a new JSON line rather than mutating an
/// existing one. Each is serialized by [`lock_notes`], keyed on the git
/// common dir so worktrees sharing one notes ref contend on one lock.
///
/// # Unsupported methods
/// Semantic search (`search`, `search_hybrid`, `search_timeline`, `search_text`),
/// `get_edges`, `supersede`, `harvested_shas`, and `has_source_ref` all return
/// `Err` with a clear message rather than silently returning empty results.
/// `add_edge` is supported for `relates_to` and `contradicts` (the edge rides
/// an appended record); reading edges back is a store's job after import.
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

    // Exposes the root to the free-function carrier helpers
    // (append_state_update et al.), which take Option<&Path> since they're
    // shared with the SQLite-primary write-through path that has no
    // GitNotesBackend to borrow.
    fn git_root(&self) -> Option<&std::path::Path> {
        self.git_root.as_deref()
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

    // Writes the note body via `git notes add -f -F - -- <object>`, over
    // stdin: keeps note content (which may be arbitrary user/LLM text) off
    // argv, and `--` stops object from being parsed as an option.
    async fn add_note_stdin(&self, object: &str, body: &str) -> Result<()> {
        let mut cmd = self.git();
        cmd.args([
            "notes",
            "--ref=inkentry",
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

    // (commit_sha, note_blob_sha) for every commit reachable from HEAD that
    // carries a note, newest first. Only HEAD-reachable commits are listed:
    // memory travels with the code that carries it, so a teammate's note on
    // a fetched-but-unmerged commit stays invisible until merged.
    async fn noted_commits(&self) -> Result<Vec<(String, String)>> {
        // `git notes --ref=inkentry list` → "<note-blob-sha> <commit-sha>"
        let list_out = self
            .git()
            .args(["notes", "--ref=inkentry", "list"])
            .output()
            .await?;

        if !list_out.status.success() {
            return Ok(vec![]);
        }

        let listing = String::from_utf8_lossy(&list_out.stdout);
        let noted: HashMap<&str, &str> = listing
            .lines()
            .filter_map(|l| {
                let mut parts = l.split_whitespace();
                let blob = parts.next()?;
                let commit = parts.next()?;
                Some((commit, blob))
            })
            .collect();

        if noted.is_empty() {
            return Ok(vec![]);
        }

        let log_out = self.git().args(["log", "--format=%H"]).output().await?;

        if !log_out.status.success() {
            return Ok(vec![]);
        }

        let pairs = String::from_utf8_lossy(&log_out.stdout)
            .lines()
            .filter_map(|line| {
                let commit = line.trim();
                noted
                    .get(commit)
                    .map(|blob| (commit.to_owned(), (*blob).to_owned()))
            })
            .collect();

        Ok(pairs)
    }

    // (commit_sha, note_blob_sha) for every noted commit, reachable or not —
    // unlike noted_commits, which filters to HEAD's history. The
    // reconciliation pass needs exactly the commits that filter would drop:
    // an anchor whose commit a rebase orphaned.
    async fn all_noted_commits(&self) -> Result<Vec<(String, String)>> {
        let list_out = self
            .git()
            .args(["notes", "--ref=inkentry", "list"])
            .output()
            .await?;
        if !list_out.status.success() {
            return Ok(vec![]);
        }
        Ok(String::from_utf8_lossy(&list_out.stdout)
            .lines()
            .filter_map(|l| {
                let mut parts = l.split_whitespace();
                let blob = parts.next()?;
                let commit = parts.next()?;
                Some((commit.to_string(), blob.to_string()))
            })
            .collect())
    }

    /// Every record on the ref paired with its attachment commit, reachable
    /// or not.
    pub async fn all_noted_records(&self) -> Result<Vec<(String, NoteRecord)>> {
        let noted = self.all_noted_commits().await?;
        let blob_shas: Vec<String> = noted.iter().map(|(_, blob)| blob.clone()).collect();
        let blobs = self.read_note_blobs(&blob_shas).await?;

        let mut out = Vec::new();
        for ((commit, _blob), body) in noted.iter().zip(blobs.iter()) {
            for record in parse_records(body)? {
                out.push((commit.clone(), record));
            }
        }
        Ok(out)
    }

    // Note blob shas only, for the lenient batch read folded_records uses:
    // listing/lookup reads must not break because one historical note is
    // unreadable.
    async fn noted_blobs(&self) -> Result<Vec<String>> {
        Ok(self
            .noted_commits()
            .await?
            .into_iter()
            .map(|(_, blob)| blob)
            .collect())
    }

    // Reads every listed note blob in one `git cat-file --batch`, in request
    // order. The fold needs every reachable blob, so a per-commit `git notes
    // show` would cost one subprocess each (~13ms); write paths keep `show`
    // since they read exactly one note.
    async fn read_note_blobs(&self, blob_shas: &[String]) -> Result<Vec<String>> {
        if blob_shas.is_empty() {
            return Ok(vec![]);
        }

        let mut cmd = self.git();
        cmd.args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open stdin for git cat-file --batch"))?;

        let mut request = blob_shas.join("\n");
        request.push('\n');

        // Write and drain concurrently: git blocks once the stdout pipe fills,
        // so writing the whole request first would deadlock on a big enough repo.
        let writer = async move {
            stdin.write_all(request.as_bytes()).await?;
            stdin.shutdown().await
        };
        let (write_res, out) = tokio::join!(writer, child.wait_with_output());

        let out = out?;
        if !out.status.success() {
            return Err(anyhow!(
                "git cat-file --batch: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        write_res?;

        parse_cat_file_batch(&out.stdout)
    }

    // Raw note blob for commit_sha (empty string if no note). A failed read
    // is Err, never an empty blob: append_record writes back what this
    // returns, so conflating the two would turn a transient git failure into
    // a wiped note.
    async fn read_note_blob(&self, commit_sha: &str) -> Result<String> {
        Ok(
            read_note_body_with_retry(self.git_root.as_deref(), commit_sha)
                .await?
                .unwrap_or_default(),
        )
    }

    // Appends record as a new JSON line to object's note, preserving every
    // existing line byte-for-byte.
    async fn append_record(&self, object: &str, record: &NoteRecord) -> Result<()> {
        // git notes is the primary store on this path (`--backend git-notes`),
        // so an unconfigured carry ref orphans the only copy. Status is dropped:
        // this path has no command output to announce on.
        ensure_notes_rewrite_ref(self.git_root.as_deref()).await;

        let _lock = writer_lock(self.git_root.as_deref()).await?;

        let existing = self.read_note_blob(object).await?;
        let new_line = serde_json::to_string(record)?;
        let combined = if existing.trim().is_empty() {
            new_line
        } else {
            format!("{}\n{}", existing.trim_end_matches('\n'), new_line)
        };
        self.add_note_stdin(object, &combined).await
    }

    // Every entry on the ref, folded to one record per entity, no filtering
    // or limit truncation — the shared basis so collect()'s listing and
    // get()'s lookup see the same folded state: a record's
    // status/superseded_by must reflect every state-update appended for its
    // entity, not just whichever raw line carries its original id.
    //
    // The only site that sees every commit's records, so the only site that
    // can fold an entity's copies together.
    async fn folded_records(&self) -> Result<Vec<NoteRecord>> {
        let blob_shas = self.noted_blobs().await?;

        let mut records = Vec::new();
        for blob in self.read_note_blobs(&blob_shas).await? {
            records.extend(parse_records(&blob)?);
        }

        // Fold before every filter below. Dropping an archived copy first would
        // leave a surviving active copy to resurrect the entity.
        Ok(fold_records(records))
    }

    /// Every outgoing edge on the ref as `(source entity_id, edge)` pairs, read
    /// off the folded records so an edge appended on any copy of an entity
    /// counts once. This is the whole of what the carrier offers about the
    /// graph: it is reconstructed by resolving these against a store's rows,
    /// never queried from the ref.
    pub async fn carried_edges(&self) -> Result<Vec<(String, CarriedEdge)>> {
        Ok(self
            .folded_records()
            .await?
            .into_iter()
            .flat_map(|record| {
                let from = record.resolve_entity_id();
                record
                    .edges
                    .into_iter()
                    .map(move |edge| (from.clone(), edge))
            })
            .collect())
    }

    /// Every supersede on the ref as `(OLD/subject entity_id, NEW/successor
    /// entity_id)` pairs. The carrier records a supersede on the subject's
    /// `superseded_by_entity_id` rather than in the `edges` list (which
    /// [`carried_edges`] reads), so this is its own projection; a clone rebuilds
    /// the `supersedes` row by resolving both ends against its rows. Read off the
    /// folded records so a supersede appended on any copy of an entity counts
    /// once.
    pub async fn carried_supersede_edges(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .folded_records()
            .await?
            .into_iter()
            .filter_map(|record| {
                record
                    .superseded_by_entity_id
                    .as_deref()
                    .map(|successor| (record.resolve_entity_id(), successor.to_string()))
            })
            .collect())
    }

    // Every record on the ref, each paired with its anchor commit, newest
    // first. Unlike folded_records this keeps per-commit provenance and does
    // not fold; callers fold or anchor as they need.
    //
    // noted_commits and read_note_blobs share one order (the latter reads
    // the former's blob shas in request order), so zipping them attributes
    // each blob's records to the right commit.
    async fn records_with_commit(&self) -> Result<Vec<(String, NoteRecord)>> {
        let noted = self.noted_commits().await?;
        let blob_shas: Vec<String> = noted.iter().map(|(_, blob)| blob.clone()).collect();
        let blobs = self.read_note_blobs(&blob_shas).await?;

        let mut out = Vec::new();
        for ((commit, _blob), body) in noted.iter().zip(blobs.iter()) {
            for record in parse_records(body)? {
                out.push((commit.clone(), record));
            }
        }
        Ok(out)
    }

    /// The `entity_id`s of every entry whose memory note is anchored to a commit
    /// whose sha begins with `sha_prefix`.
    ///
    /// The anchor — the git-notes attachment (commit → note object) — is the
    /// only place a `memory add` entry records which commit it belongs to: its
    /// SQLite `source_ref` column stays NULL, so a `source_ref` column query
    /// can never surface it. Prefix matching mirrors that column's `LIKE
    /// 'prefix%'` semantics — a plain string prefix over the full commit sha
    /// — rather than git's own abbreviated-object resolution, so an
    /// ambiguous-to-git prefix still matches every noted commit it is a
    /// prefix of.
    pub async fn entity_ids_anchored_to(&self, sha_prefix: &str) -> Result<Vec<String>> {
        let records = self.records_with_commit().await?;
        Ok(fold::all_anchor_commits(&records)
            .into_iter()
            .filter(|(_entity, commits)| commits.iter().any(|c| c.starts_with(sha_prefix)))
            .map(|(entity, _commits)| entity)
            .collect())
    }

    /// Every explicit `op: "anchor"` commit claimed for `entity_id`, each
    /// paired with the `created_at` of the record that claimed it. Excludes
    /// the base write-time attachment.
    pub async fn anchors_for_entity(&self, entity_id: &str) -> Result<Vec<(String, i64)>> {
        let records = self.records_with_commit().await?;
        Ok(fold::anchors_for_entity(&records, entity_id))
    }

    /// The distinct set of full commit shas that at least one entry's memory
    /// note is anchored to — the write-time attachment plus every commit
    /// carrying an explicit `op: "anchor"` record, so a caller counts an
    /// entry anchored later, not only at `memory add` time.
    ///
    /// Same resolution as [`entity_ids_anchored_to`], read from the commit
    /// side instead of the entity side: a caller needing "is this commit
    /// covered" for every commit in a window would otherwise re-walk the
    /// whole notes ref by calling `entity_ids_anchored_to` per commit. One
    /// pass here, then the caller checks membership.
    pub async fn anchored_commit_shas(&self) -> Result<HashSet<String>> {
        let records = self.records_with_commit().await?;
        Ok(fold::all_anchor_commits(&records)
            .into_values()
            .flatten()
            .collect())
    }

    // Note-anchored entries whose anchor commit begins with sha_prefix, as
    // folded Notes. The git-notes analogue of the SQLite source_ref filter,
    // used when git notes is the primary store; the SQLite-primary path
    // resolves the same anchors via entity_ids_anchored_to and reads the
    // rows back instead.
    async fn list_anchored_to(
        &self,
        sha_prefix: &str,
        include_archived: bool,
        as_of: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Note>> {
        let records = self.records_with_commit().await?;
        let anchors = fold::all_anchor_commits(&records);
        // Fold across every commit so an entry's `status` reflects a
        // state-update appended on a later commit, not just its original line.
        let mut folded = fold_records(records.into_iter().map(|(_, r)| r).collect());

        folded.retain(|record| {
            anchors
                .get(&record.resolve_entity_id())
                .is_some_and(|commits| commits.iter().any(|c| c.starts_with(sha_prefix)))
                && record_in_window(record, include_archived, as_of)
        });

        // Match `collect`'s ordering and newest-wins truncation exactly.
        folded.sort_by_key(|r| r.created_at);
        if folded.len() > limit {
            folded.drain(..folded.len() - limit);
        }
        Ok(folded.into_iter().map(record_to_note).collect())
    }

    async fn collect(
        &self,
        kind_filter: Option<&str>,
        include_archived: bool,
        as_of: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Note>> {
        let mut folded = self.folded_records().await?;

        folded.retain(|record| {
            if kind_filter.is_some_and(|k| record.kind != k) {
                return false;
            }
            record_in_window(record, include_archived, as_of)
        });

        // Stable over first-encounter order, so ties keep blob order.
        folded.sort_by_key(|r| r.created_at);
        if folded.len() > limit {
            // Keep the newest, as the sqlite backend's `ORDER BY created_at
            // DESC LIMIT` does. Folding first is what makes this exact.
            folded.drain(..folded.len() - limit);
        }

        Ok(folded.into_iter().map(record_to_note).collect())
    }
}

// Whether a folded record survives the archived / point-in-time gate shared
// by collect() and list_anchored_to(). kind filtering (collect only) stays
// with the caller.
//
// A point-in-time (as_of) query is governed entirely by the temporal window,
// independent of archived status: an entry archived or superseded after T
// was live at T and must be returned, so the archived gate is skipped
// whenever as_of is set; include_archived then only affects the
// current-view listing.
fn record_in_window(record: &NoteRecord, include_archived: bool, as_of: Option<i64>) -> bool {
    if let Some(ts) = as_of {
        let effective = record.valid_at.unwrap_or(record.created_at);
        if effective > ts {
            return false;
        }
        if record.invalid_at.is_some_and(|ia| ia <= ts) {
            return false;
        }
        return true;
    }
    if !include_archived && record.status == "archived" {
        return false;
    }
    true
}

// Permissively parses the inkentry records from one note blob (JSON Lines
// interleaved with foreign content). Foreign lines are skipped; only a
// newer, incompatible schema_version returns an error.
fn parse_records(blob: &str) -> Result<Vec<NoteRecord>> {
    let mut records = Vec::new();
    for line in blob.lines() {
        match parse_inkentry_line(line) {
            Some(record) => {
                if record.schema_version > 1 {
                    return Err(anyhow::Error::new(
                        crate::error::InkentryError::SchemaMismatch {
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
    Ok(records)
}

// Splits `git cat-file --batch` output into one body per requested object.
// Each record is "<sha> <type> <size>\n<size bytes>\n"; the size header is
// the only safe delimiter since a note body has newlines of its own.
fn parse_cat_file_batch(out: &[u8]) -> Result<Vec<String>> {
    let mut bodies = Vec::new();
    let mut rest = out;

    while !rest.is_empty() {
        let nl = rest
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| anyhow!("git cat-file --batch: header with no newline"))?;
        let header = String::from_utf8_lossy(&rest[..nl]).into_owned();
        rest = &rest[nl + 1..];

        let fields: Vec<&str> = header.split(' ').collect();
        match fields.as_slice() {
            // "<sha> missing" / "<sha> ambiguous": no body follows. A read must
            // not break on one unreadable note.
            [_, _] => bodies.push(String::new()),
            [_, _, size] => {
                let size: usize = size
                    .parse()
                    .map_err(|_| anyhow!("git cat-file --batch: bad size in {header:?}"))?;
                if rest.len() < size + 1 {
                    return Err(anyhow!("git cat-file --batch: truncated body"));
                }
                bodies.push(String::from_utf8_lossy(&rest[..size]).into_owned());
                rest = &rest[size + 1..];
            }
            _ => {
                return Err(anyhow!(
                    "git cat-file --batch: unexpected header {header:?}"
                ));
            }
        }
    }

    Ok(bodies)
}

// Classifies one line of a note blob: Some(record) if it parses as a JSON
// object deserializing into NoteRecord; non-JSON, non-object, blank, and
// prose lines are foreign (None).
fn parse_inkentry_line(line: &str) -> Option<NoteRecord> {
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

// The framing `git cat-file --batch` emits, pinned against git 2.55.
#[cfg(test)]
mod cat_file_batch {
    use super::*;

    // One object: `<sha> <type> <size>\n<body>\n`.
    fn framed(sha: &str, body: &str) -> String {
        format!("{sha} blob {}\n{body}\n", body.len())
    }

    #[test]
    fn empty_output_yields_no_bodies() {
        assert!(parse_cat_file_batch(b"").expect("parse").is_empty());
    }

    // A note body holds newlines of its own, so only the size header can
    // delimit it: a body line that mimics a header must not split it.
    #[test]
    fn a_body_that_mimics_a_header_is_not_split() {
        let body = "line one\ndeadbeef blob 99\nline three";

        assert_eq!(
            parse_cat_file_batch(framed("aaa", body).as_bytes()).expect("parse"),
            vec![body]
        );
    }

    // One unreadable note must not fail the whole read: git reports
    // `<sha> missing` with no body and still exits 0.
    #[test]
    fn a_missing_object_yields_an_empty_body_and_the_batch_survives() {
        let out = format!(
            "{}bbb missing\n{}",
            framed("aaa", "first"),
            framed("ccc", "third")
        );

        assert_eq!(
            parse_cat_file_batch(out.as_bytes()).expect("parse"),
            vec!["first", "", "third"]
        );
    }

    // `git notes add --allow-empty` writes the empty blob. Git still emits
    // the body's trailing newline, so a zero-length body must not read as
    // truncated.
    #[test]
    fn an_empty_blob_parses_as_an_empty_body() {
        assert_eq!(
            parse_cat_file_batch(b"aaa blob 0\n\n").expect("parse"),
            vec![""]
        );
    }

    #[test]
    fn a_body_shorter_than_its_header_claims_is_an_error() {
        assert!(parse_cat_file_batch(b"aaa blob 99\nshort\n").is_err());
    }

    // Consumed in request order, so a body is never attributed to the wrong
    // note.
    #[test]
    fn bodies_come_back_in_request_order() {
        let out = format!("{}{}", framed("aaa", "one"), framed("bbb", "two"));

        assert_eq!(
            parse_cat_file_batch(out.as_bytes()).expect("parse"),
            vec!["one", "two"]
        );
    }

    fn repo_with_one_note() -> (tempfile::TempDir, String) {
        crate::test_support::isolate_git_config();
        let dir = tempfile::TempDir::new().expect("tempdir");
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            out
        };

        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "T"]);
        std::fs::write(dir.path().join("README.md"), "x").expect("write");
        run(&["add", "."]);
        run(&["commit", "--no-gpg-sign", "-m", "first"]);
        run(&["notes", "--ref=inkentry", "add", "-m", "one\ntwo", "HEAD"]);

        let listing =
            String::from_utf8(run(&["notes", "--ref=inkentry", "list"]).stdout).expect("utf8");
        let blob = listing
            .split_whitespace()
            .next()
            .expect("a note blob sha")
            .to_string();

        (dir, blob)
    }

    // The batch must drain stdout while writing stdin: git stops reading
    // once its stdout pipe fills, and the fold reads every reachable blob,
    // so GIT_NOTES_MAX_LIST doesn't bound the request size. 5000 shas is
    // ~205KiB in, ~340KiB out — past the 64KiB pipe buffer both ways, so a
    // regression here hangs instead of failing loudly.
    #[tokio::test]
    async fn a_request_past_the_pipe_buffer_does_not_deadlock() {
        let (dir, blob) = repo_with_one_note();
        let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

        let bodies = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            backend.read_note_blobs(&vec![blob; 5000]),
        )
        .await
        .expect("deadlocked: stdout must drain while stdin is written")
        .expect("read");

        assert_eq!(bodies.len(), 5000, "one body per requested sha");
        assert!(
            bodies.iter().all(|b| b == "one\ntwo\n"),
            "every body must survive the batch intact"
        );
    }
}
