//! ADR-099: `inkentry memory anchor`, the post-commit hook's claim command
//! (D2), the `--commit`/`memory anchor <id>` escape hatches (D4), and D3a's
//! hook-free rewrite reconciliation.
//!
//! Every entry point here is best-effort by design: a commit must never fail
//! because this plumbing stumbled, so every internal error is swallowed
//! rather than returned. Nothing is ever printed to stdout — the hook
//! redirects it away, but the command holds to that on its own too.

use std::path::Path;

use anyhow::Result;

use super::MemoryAnchorArgs;
use crate::storage::memory::Note;
use crate::storage::{
    GitNotesBackend, MemoryStore, append_anchor_record, commit_patch_id,
    commits_reachable_from_any_ref, is_ancestor, resolve_source_ref,
};

pub(super) async fn memory_anchor(args: MemoryAnchorArgs, mem_path: &Path) -> Result<()> {
    // Never touched: no project, or the project has never been `init`ed, is
    // simply nothing to do (ADR-099: "nothing else in this ADR applies").
    if !mem_path.exists() {
        return Ok(());
    }
    let Ok(git_root) = std::env::current_dir() else {
        return Ok(());
    };
    let Ok(store) = MemoryStore::open(mem_path) else {
        return Ok(());
    };

    let Some(target) = resolve_target_commit(&git_root, &args.commit).await else {
        return Ok(());
    };

    if args.ids.is_empty() {
        claim_pending(&store, &git_root, &target).await;
    } else {
        for id in &args.ids {
            let Some(note) = resolve_note(&store, id) else {
                continue;
            };
            anchor_note_now(&store, &git_root, &note, &target).await;
        }
    }
    Ok(())
}

/// The same handle resolution `resolve_note` (`resolve.rs`) does over a
/// `dyn MemoryBackend` — the exact id first, then an unambiguous `entity_id`
/// prefix — reimplemented over a raw `MemoryStore` since this plumbing
/// command never goes through `open_memory_backend`. An ambiguous or unknown
/// handle is silently skipped rather than guessed (D4: this runs from a hook,
/// and an id a caller can type by hand is inherently exact enough to trust).
fn resolve_note(store: &MemoryStore, token: &crate::storage::NoteId) -> Option<Note> {
    if let Ok(Some(note)) = store.get(token) {
        return Some(note);
    }
    if !inkentry_core::storage::is_entity_id_lookup(token.as_str()) {
        return None;
    }
    let mut matches = store.note_ids_for_entity_id_prefix(token.as_str()).ok()?;
    (matches.len() == 1)
        .then(|| matches.pop())
        .flatten()
        .and_then(|id| store.get(&id).ok().flatten())
}

/// `git rev-parse --verify <ref>^{commit}`: the exact sha `--commit` names,
/// whether it was given as `HEAD`, a branch, or a raw sha. `None` on any
/// failure (unborn HEAD, an unresolvable ref) — the caller does nothing.
async fn resolve_target_commit(git_root: &Path, commit_ref: &str) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .current_dir(git_root)
        .args(["rev-parse", "--verify", &format!("{commit_ref}^{{commit}}")])
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// D2: claim every pending row written in this worktree whose `head_at_write`
/// grew into `target` — by ordinary ancestry against `target`'s first parent,
/// or, for `commit --amend`, by equalling the commit `HEAD@{1}` names (the one
/// `target` replaced). Never by recency, branch, or session (ADR-099
/// rationale table).
async fn claim_pending(store: &MemoryStore, git_root: &Path, target: &str) {
    let Some(worktree) = inkentry_core::utils::current_worktree_git_dir(git_root) else {
        return;
    };
    let worktree = worktree.display().to_string();
    let Ok(pending) = store.pending_anchors_in_worktree(&worktree) else {
        return;
    };
    if pending.is_empty() {
        return;
    }

    let first_parent = rev_parse(git_root, &format!("{target}~1")).await;
    let amend_replaced = rev_parse(git_root, "HEAD@{1}").await;

    for row in pending {
        let claims = match &first_parent {
            Some(fp) if is_ancestor(Some(git_root), &row.head_at_write, fp).await => true,
            _ => amend_replaced.as_deref() == Some(row.head_at_write.as_str()),
        };
        if !claims {
            continue;
        }
        let Ok(Some(note)) = store.get_by_entity_id(&row.entity_id) else {
            continue;
        };
        anchor_note_now(store, git_root, &note, target).await;
    }
}

async fn rev_parse(git_root: &Path, rev: &str) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .current_dir(git_root)
        .args(["rev-parse", "--verify", rev])
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Write the anchor record (D3), resolve `source_ref` across every anchor the
/// entity now carries (D3: "the earliest one that is reachable from a ref,
/// falling back to the earliest"), and drop the pending row if there was one
/// (D2/D4 both end here).
pub(super) async fn anchor_note_now(
    store: &MemoryStore,
    git_root: &Path,
    note: &Note,
    target: &str,
) {
    let patch_id = commit_patch_id(Some(git_root), target).await.ok().flatten();
    if append_anchor_record(Some(git_root), target, note, patch_id)
        .await
        .is_err()
    {
        return;
    }

    let backend = GitNotesBackend::with_root(git_root.to_path_buf());
    let Ok(anchors) = backend.anchors_for_entity(&note.entity_id).await else {
        return;
    };
    let mut reachable = std::collections::HashMap::with_capacity(anchors.len());
    for (commit, _) in &anchors {
        reachable.insert(
            commit.clone(),
            is_ancestor(Some(git_root), commit, "HEAD").await,
        );
    }
    if let Some(resolved) =
        resolve_source_ref(&anchors, |c| reachable.get(c).copied().unwrap_or(false))
    {
        let _ = store.set_source_ref(&note.id, &resolved);
    }
    let _ = store.remove_pending_anchor(&note.entity_id);
}

/// D4 escape hatch for `memory add --commit`: anchor the just-written entry
/// to a commit immediately, no ancestry test, and skip recording a pending
/// row for it in the first place.
pub(crate) async fn anchor_now(
    mem_path: &Path,
    target: &str,
    id: &crate::storage::NoteId,
) -> Result<()> {
    let git_root = std::env::current_dir()?;
    let store = MemoryStore::open(mem_path)?;
    let Some(note) = store.get(id)? else {
        anyhow::bail!("entry {id} vanished before it could be anchored");
    };
    let Some(target_sha) = resolve_target_commit(&git_root, target).await else {
        anyhow::bail!("could not resolve '{target}' to a commit");
    };
    anchor_note_now(&store, &git_root, &note, &target_sha).await;
    Ok(())
}

/// D1: record where `entity_id` was written, for the post-commit hook (or a
/// later `inkentry index` reconciliation pass) to claim. A no-op outside a
/// git repository or with HEAD unborn — "nothing else in this ADR applies to
/// such a project" — and best-effort otherwise: a failure here must not fail
/// the entry it describes, which is already durably stored by the time this
/// runs.
pub(crate) async fn record_pending(mem_path: &Path, entity_id: &str) -> Result<()> {
    let git_root = std::env::current_dir()?;
    let Some(worktree) = inkentry_core::utils::current_worktree_git_dir(&git_root) else {
        return Ok(());
    };
    let Some(head) = rev_parse(&git_root, "HEAD").await else {
        return Ok(());
    };
    let store = MemoryStore::open(mem_path)?;
    store.record_pending_anchor(entity_id, &worktree.display().to_string(), &head)?;
    Ok(())
}

// ── D3a: hook-free rewrite reconciliation ───────────────────────────────────

/// Run from `inkentry index` (the pass that already runs after local history
/// moves — see `crates/inkentry-cli/src/cli/cmd/index/mod.rs`). Best-effort
/// and silent: a failure here must never fail an index run.
///
/// For every noted commit no longer reachable from any ref, looks for exactly
/// one reachable commit with the same patch-id and adds a second anchor there
/// (the old one is kept). For every pending row whose `head_at_write` is
/// unreachable, the same lookup repoints it at the replacement so the next
/// ordinary commit in that worktree can claim it.
pub(crate) async fn reconcile_anchors(mem_path: &Path, project_root: &Path) -> Result<()> {
    if !mem_path.exists() {
        return Ok(());
    }
    let store = MemoryStore::open(mem_path)?;
    let backend = GitNotesBackend::with_root(project_root.to_path_buf());
    let reachable = commits_reachable_from_any_ref(Some(project_root)).await?;

    reconcile_orphaned_anchors(&store, &backend, project_root, &reachable).await?;
    reconcile_pending_rows(&store, project_root, &reachable).await?;
    Ok(())
}

async fn patch_id_of(store: &MemoryStore, git_root: &Path, sha: &str) -> Option<String> {
    if let Ok(Some(cached)) = store.cached_patch_id(sha) {
        return cached;
    }
    let computed = commit_patch_id(Some(git_root), sha).await.ok().flatten();
    let _ = store.cache_patch_id(sha, computed.as_deref());
    computed
}

/// One reachable commit sharing `patch_id`, or `None` if there is not exactly
/// one (ambiguous or no match is always a safe, sanctioned outcome — ADR-099
/// D3a).
async fn find_unique_match(
    store: &MemoryStore,
    git_root: &Path,
    patch_id: &str,
    reachable: &std::collections::HashSet<String>,
) -> Option<String> {
    let mut found: Option<String> = None;
    for candidate in reachable {
        if patch_id_of(store, git_root, candidate).await.as_deref() == Some(patch_id) {
            if found.is_some() {
                return None; // ambiguous
            }
            found = Some(candidate.clone());
        }
    }
    found
}

async fn reconcile_orphaned_anchors(
    store: &MemoryStore,
    backend: &GitNotesBackend,
    project_root: &Path,
    reachable: &std::collections::HashSet<String>,
) -> Result<()> {
    let all = backend.all_noted_records().await?;
    for (commit, record) in &all {
        if reachable.contains(commit) || record.op.as_deref() != Some("anchor") {
            continue;
        }
        let Some(patch_id) = record.patch_id.clone() else {
            continue; // merge, or an older anchor with no stored patch-id
        };
        let Some(replacement) = find_unique_match(store, project_root, &patch_id, reachable).await
        else {
            continue;
        };
        let entity_id = record.resolve_entity_id();
        let Ok(Some(note)) = store.get_by_entity_id(&entity_id) else {
            continue;
        };
        anchor_note_now(store, project_root, &note, &replacement).await;
    }
    Ok(())
}

async fn reconcile_pending_rows(
    store: &MemoryStore,
    project_root: &Path,
    reachable: &std::collections::HashSet<String>,
) -> Result<()> {
    for row in store.all_pending_anchors().unwrap_or_default() {
        if reachable.contains(&row.head_at_write) {
            continue;
        }
        let Some(patch_id) = patch_id_of(store, project_root, &row.head_at_write).await else {
            continue;
        };
        let Some(replacement) = find_unique_match(store, project_root, &patch_id, reachable).await
        else {
            continue;
        };
        let _ = store.reassign_pending_anchor_head(&row.entity_id, &replacement);
    }
    Ok(())
}
