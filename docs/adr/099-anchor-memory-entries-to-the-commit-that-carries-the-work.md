# ADR-099: Anchor memory entries to the commit that carries the work, claimed per worktree and never guessed

**Date:** 2026-09-17
**Deciders:** Founder (Johan); Architect
**Relationship to prior ADRs:** builds on the git-notes carrier
([ADR-068](068-zero-setup-onboarding-git-notes-memory-fallback.md)), the
`notes.rewriteRef` carry and tracking refspec
([ADR-069](069-git-notes-sharing-pre-push-hook-and-tracking-refspec.md)), and
the carrier's state-update records
([ADR-086](086-carrier-representation-for-memory-edges.md)). Identity is
untouched ([ADR-078](078-uuidv7-memory-entry-identity.md),
[ADR-093](093-entity-id-is-the-portable-handle-for-memory-entries.md)): the
anchor is not part of `entity_id`. Supplies `rec.commit_coverage` and the
`memory-commit/v1` eval set of
[ADR-098](098-metrics-and-evaluation-indexed-by-commit.md). The local table it
adds is an ordinary `memory.db` migration step (#292).

## Context

An entry written with `inkentry memory add` is not tied to the commit that
contains the work it describes.

- `memory add` always passes `source_ref: None`
  (`crates/inkentry-cli/src/cli/cmd/memory/add.rs`). Only `harvest` sets it. On
  this repository's own store, 181 of 195 entries have a `source_ref`, and all
  of them came from harvest.
- The carrier attaches the entry's note to `HEAD` at write time. An agent
  records a decision while working, before committing, so that `HEAD` is the
  **parent** of the commit the decision belongs to. `memory list --source-ref`
  resolves anchors through that attachment
  (`storage/git_notes/backend_impl.rs`, `list_by_source_ref`), so it answers
  "what was written while standing on this commit", not "what does this commit
  contain".
- Linked worktrees share the main worktree's `.inkentry/`
  (`config/paths.rs`, `resolve_main_worktree_root`). One `memory.db` therefore
  serves every worktree and every agent working in them at once.

Three planned features need the real mapping: a pull-request comment listing
the decisions made in the PR's commits, "the state of memory as of this commit",
and the commit-based eval labels in ADR-098.

The obvious implementation is a post-commit step that stamps every entry
lacking a commit with the new SHA. With a shared store and several agents in
several worktrees, that assigns one agent's decisions to another agent's
commit. It is wrong exactly when the product is used as intended.

A post-commit hook already exists (`cli/cmd/hooks.rs`): it runs
`inkentry index --detach` and `inkentry harvest --git-range HEAD~1..HEAD
--detach`. Git does not clone hooks, so it is present only where someone
installed it.

## Decision

### D1 - an entry records where it was written, locally

On `memory add`, inkentry records a **pending anchor** in a local table in
`memory.db`. It is local working state: it never enters the carrier and never
syncs.

```sql
CREATE TABLE pending_anchors (
    entity_id     TEXT    NOT NULL PRIMARY KEY,
    worktree      TEXT    NOT NULL,  -- absolute `git rev-parse --git-dir` of the worktree the write ran in
    head_at_write TEXT    NOT NULL,  -- HEAD sha at write time
    created_at    INTEGER NOT NULL
);
```

The table lives in `memory.db`, which every linked worktree of a repository
shares. That sharing is the reason the `worktree` column exists: it is the
only thing that separates one agent's pending entries from another's. Nothing
else about the moment of writing is stored. In particular the branch name is
not: a branch is a movable label, and the label at write time says nothing
reliable about where the work will be committed (D2).

A write outside a git repository, or pre-`init` on the git-notes-only path,
records no pending anchor and behaves as today.

### D2 - the post-commit hook claims anchors for its own worktree only

The post-commit hook gains one line, `inkentry memory anchor --commit HEAD`,
run before the detached index and harvest. It is plumbing, makes no network
call and never fails the commit. It claims a pending row when **both** of these hold:

1. **Same worktree.** `worktree` equals the git-dir of the worktree the commit
   was made in.
2. **The commit grew out of where the entry was written.** `head_at_write` is
   the new commit's first parent or an ancestor of it. For `commit --amend`,
   where the new commit replaces `head_at_write` rather than descending from
   it, the commit being replaced (`HEAD@{1}`) is accepted in place of the
   parent.

Rows failing either are left pending. Nothing is ever assigned by recency
alone.

Two things are deliberately **not** conditions, because each rejects an
ordinary workflow:

- **Branch.** Make changes on `main`, record a decision, realise at commit time
  that this belongs on a branch, `git switch -c feature`, commit. The entry was
  written "on main" and committed "on feature", and it plainly belongs to that
  commit. Ancestry handles it correctly: `head_at_write` is the tip of `main`,
  which is the new commit's parent. Ancestry also handles the case a branch
  check was meant for: switch the same worktree to an unrelated branch and
  commit there, and `head_at_write` is not an ancestor, so nothing is claimed.
- **Session.** Two agents in *different* worktrees are separated by condition
  1. Two sessions in the *same* worktree share one working tree and one index,
  so a commit made there carries whatever both of them changed; an entry from
  the session that did not run `git commit` still belongs to it. The common
  case is sequential: one session records decisions and stops, a later session
  or a person commits the work.

Commit time is not compared either. At hook time every pending row already
predates the commit, and committer dates can be set by hand.

### D3 - the anchor is a note attachment, so history rewrites carry it

Claiming writes a state-update record `{entity_id, op: "anchor"}` attached to
the **claimed commit**, and sets `source_ref` on the local row as the projection
of that attachment. This keeps the rule the carrier already follows: the anchor
commit is the attachment, not a field inside the record. Because
`notes.rewriteRef` names the inkentry ref (ADR-069), `commit --amend` and
`rebase` carry the anchor onto the rewritten commit with no new mechanism. On
import, an `anchor` record sets `source_ref` for its `entity_id`; the original
write-time attachment remains and keeps its meaning of "written while standing
here".

An entry can carry more than one anchor (a decision whose work spans commits is
claimed by the first; `memory anchor --commit <sha> <id>` adds others by hand).
`source_ref` holds the earliest.

### D4 - unclaimed stays unclaimed, and is visible

Pending rows whose worktree no longer exists, or that are older than 14 days,
are reported by `inkentry status` as unanchored and are not assigned. An entry
about a discussion that produced no commit is legitimately unanchored. The
escape hatches are explicit: `memory add --commit <sha>` and
`memory anchor --commit <sha> <id>`.

### D5 - servers accept a later anchor

`source_ref` already travels on `AddNoteRequest`
(`storage/remote/wire_types.rs`). An entry can now be synced before it is
anchored, so the sync path sends the anchor as an update when it is claimed.
inkentry-server gains the update.

## Rationale

| Option | Considered | Rejected because |
|---|---|---|
| Stamp every unanchored entry with the new SHA in post-commit | Trivial | Misattributes across agents and worktrees, which share one `memory.db`. Wrong in the product's main use case |
| Use `HEAD` at write time as the anchor (today's attachment) | No new state | It is the parent of the work commit, and for a long session many commits behind it |
| Match by time window only | No worktree bookkeeping | Two agents committing within the same minutes are indistinguishable |
| Also require the branch at write time to equal the branch committed on | Feels like extra safety | Rejects "started on `main`, created the branch at commit time", which is routine. Ancestry gives the safety without the false negative |
| Also require the writing session to equal the committing session | Separates agents sharing a directory | Sessions sharing a worktree share a working tree, so the commit carries both; and the writer and the committer are often different sessions, or an agent and a person |
| A commit-message trailer listing entry ids, written by a `prepare-commit-msg` hook | Survives every rewrite and is visible in `git log` | Puts tool bookkeeping into every commit message, and a second hook in the commit path is a second thing that can break a commit. The note attachment survives rewrites already |
| Store the anchor as a field in the record | Simple to read | The record is immutable and content-addressed by design; the carrier already expresses anchors as attachments, and a field would not follow an amend |
| Require the agent to pass `--commit` | Exact | The commit does not exist yet when the decision is made, and a step someone must remember is the failure this product exists to remove |

## Consequences

**Easier**

- "Decisions made in this PR" is a lookup over the PR's commit SHAs.
- `search --as-of <sha>` needs only SHA to commit-time resolution; the temporal
  filter already exists.
- `memory-commit/v1` gains labels from agent-written entries rather than from
  harvest alone.

**Harder**

- The hook is now load-bearing for a data property, and hooks are per-clone.
  Without it entries stay pending until someone runs `memory anchor`. The
  agent hooks work should run the same plumbing after an agent commits.
- Squash merges discard the anchored commits from `main`. The anchor remains
  valid against the PR's commits, which is what the PR comment needs, but an
  as-of query over `main` will not see it until the squash commit is anchored.
  Deferred; see below.
- One more local table to keep consistent with worktree removal.

**Known gaps**

- An entry still pending when its branch is rebased keeps a `head_at_write`
  that is no longer an ancestor of anything, and falls to unanchored (D4).
  `post-rewrite` could remap it; not worth the machinery until it is seen to
  matter.
- The first commit after an entry is written claims it, even if that commit is
  an unrelated fix made first. Once entries carry linked files (ADR-101),
  preferring a commit that touches one of them would sharpen this.

**Revisit if**

- The share of `memory add` entries still unanchored after a week exceeds a
  fifth: the claim rule is too strict or the hook is not installed widely.
- Squash-merge repositories are common among users: add a rule that anchors a
  PR's entries to its squash commit at merge time (the merge event carries the
  mapping).

## Security implications

- `pending_anchors.worktree` is an absolute local path. The table is local,
  never synced, never exported by `dump` except as a count.
- `memory anchor` runs inside a git hook with the user's privileges. It takes
  no input from the commit message or file contents, only SHAs from git, and
  makes no network call (`egress_containment.rs` gains a case).

## Measured by

`rec.commit_coverage` rises for repositories using agents. New state metric
`rec.unanchored_rate`: `memory add` entries older than 14 days with no anchor,
divided by `memory add` entries older than 14 days. Label count of
`memory-commit/v1` on inkentry's own history.
