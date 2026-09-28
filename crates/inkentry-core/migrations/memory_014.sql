-- Schema version 14 (ADR-099 D1): pending_anchors, local working state
-- mapping a `memory add` write to the commit it was written on, before the
-- post-commit hook (`inkentry memory anchor --commit HEAD`) claims it.
--
-- `worktree` is the only thing separating one agent's pending entries from
-- another's, since linked worktrees share one memory.db (ADR-099 Context).
-- Never carried by the git-notes carrier, never synced, and never exported by
-- a dump except as a count (Security implications).
CREATE TABLE pending_anchors (
    entity_id     TEXT    NOT NULL PRIMARY KEY,
    worktree      TEXT    NOT NULL,
    head_at_write TEXT    NOT NULL,
    created_at    INTEGER NOT NULL
);
CREATE INDEX idx_pending_anchors_worktree ON pending_anchors(worktree);

-- D3a's hook-free rewrite reconciliation costs one `git patch-id --stable`
-- per commit not already checked; this is that seen-set, so a repeated
-- `inkentry index` does not recompute it for a commit it already looked at.
-- NULL `patch_id` means a merge commit, which carries none.
CREATE TABLE patch_id_cache (
    commit_sha TEXT NOT NULL PRIMARY KEY,
    patch_id   TEXT
);
