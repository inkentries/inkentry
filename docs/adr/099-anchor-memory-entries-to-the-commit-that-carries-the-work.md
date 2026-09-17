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

An inkentry project does not have to be a git repository. When the project
has no repository, or the repository has no commit yet (`HEAD` is unborn),
`memory add` records no pending anchor and behaves exactly as today. Nothing
else in this ADR applies to such a project: there is no hook, entries are
never reported as unanchored, and the commit-based metrics (`rec.commit_coverage`,
`rec.unanchored_rate`) are absent from its snapshot rather than zero. The same
holds pre-`init` on the git-notes-only path.

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

### D3 - the anchor is a note attachment, and it records the commit's patch-id

Claiming writes a state-update record `{entity_id, op: "anchor", patch_id}` attached to
the **claimed commit**, and sets `source_ref` on the local row as the projection
of that attachment. This keeps the rule the carrier already follows: the anchor
commit is the attachment, not a field inside the record. Because
`notes.rewriteRef` names the inkentry ref (ADR-069), `commit --amend` and
`rebase` carry the anchor onto the rewritten commit with no new mechanism. On
import, an `anchor` record sets `source_ref` for its `entity_id`; the original
write-time attachment remains and keeps its meaning of "written while standing
here".

`patch_id` is `git patch-id --stable` of the claimed commit: a hash of the
commit's diff that does not depend on its parent, which is how git itself
recognises a commit that has been rebased or cherry-picked. It is stored in the
record, not recomputed later, because the original commit object may be gone
from a clone by the time it is needed. Merge commits have no patch-id and carry
none.

An entry can carry more than one anchor. `source_ref` holds the earliest one
that is reachable from a ref, falling back to the earliest.

### D3a - rewrites are followed by patch-id, without relying on a hook

Rebasing is a default workflow for many teams, and much of it happens where no
local hook can run: the hosting service's "rebase and merge" button creates new
commits server-side, with new SHAs, and carries no notes. `notes.rewriteRef`
and a `post-rewrite` hook only ever see rewrites made in the clone they are
installed in. So following a rewrite cannot depend on having observed it.

Instead, reconciliation is done after the fact, from content:

- **Claimed anchors.** When the commit an anchor is attached to is no longer
  reachable from any ref, inkentry looks for a reachable commit with the same
  `patch_id` and, if there is exactly one, writes an additional `anchor` record
  on it. The old anchor is kept: it is still true of the commits the work was
  reviewed as. This runs inside passes that already happen after history moves
  (`inkentry index`, `memory sync`), over commits not seen before, so it costs
  one patch-id per new commit.
- **Pending rows.** When `head_at_write` is no longer reachable from any ref,
  the same lookup finds the commit that replaced it, and D2's ancestry test
  runs against that. This covers "recorded a decision, rebased the branch, then
  committed" with no `post-rewrite` hook.

It is best effort by nature. A rebase that needed conflict resolution changes
the diff and therefore the patch-id, and no match is found. Two commits with
the same diff are ambiguous and are left alone. Neither case is guessed: the
entry keeps the anchor it has, or stays pending and ages out under D4.

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
| Follow rewrites with a `post-rewrite` hook | Exact, and git hands over the old-to-new mapping | Only sees rewrites made in a clone that has the hook. Server-side rebase and squash, and teammates' rebases, are invisible to it. Patch-id needs no observation of the rewrite, so the hook would add a second mechanism for a subset of cases |
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
- Squash merges are not recoverable by patch-id: several diffs become one. The
  anchor stays valid against the commits the work was reviewed as, which is
  what a per-PR listing needs, but an as-of query over `main` does not see the
  entry at the squash commit. See Known gaps.
- One more local table to keep consistent with worktree removal.

**Known gaps**

- Squash merges (above), and rebases whose conflicts changed the diff, lose the
  link to `main`. Closing the squash case needs the mapping from a pull
  request's commits to its squash commit, which only the merge event carries;
  a tree comparison (the squash commit's tree equals the branch tip's tree when
  the branch was up to date) is a possible hook-free heuristic, not adopted
  here.
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
