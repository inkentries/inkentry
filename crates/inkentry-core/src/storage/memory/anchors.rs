//! ADR-099 D1: `pending_anchors`, the local working state a `memory add`
//! leaves behind until a commit claims it, plus D3a's `patch_id_cache` seen-set.
//!
//! Neither table is part of the git-notes carrier and neither syncs: a
//! pending row records where an entry was written, not what it says, and the
//! patch-id cache is pure local bookkeeping for a reconciliation pass that
//! runs again on every machine that runs it.

use anyhow::Result;

use super::MemoryStore;

/// One row of `pending_anchors`: a `memory add` entry waiting for a commit to
/// claim it (ADR-099 D1/D2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAnchor {
    pub entity_id: String,
    /// Absolute `git rev-parse --git-dir` of the worktree the write ran in.
    pub worktree: String,
    /// HEAD's sha at write time.
    pub head_at_write: String,
    pub created_at: i64,
}

fn row_to_pending_anchor(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingAnchor> {
    Ok(PendingAnchor {
        entity_id: row.get(0)?,
        worktree: row.get(1)?,
        head_at_write: row.get(2)?,
        created_at: row.get(3)?,
    })
}

impl MemoryStore {
    /// Record a pending anchor for `entity_id` (D1). `INSERT OR IGNORE`: a
    /// second `memory add` write for the same content within one process is
    /// not expected, and leaving the earliest write's row in place rather than
    /// clobbering it is the safer default if it ever happens.
    pub fn record_pending_anchor(
        &self,
        entity_id: &str,
        worktree: &str,
        head_at_write: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO pending_anchors (entity_id, worktree, head_at_write, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                entity_id,
                worktree,
                head_at_write,
                crate::storage::note_record::now_secs()
            ],
        )?;
        Ok(())
    }

    /// Count of pending rows — the only shape this local table takes outside
    /// this store (ADR-099 Security implications: "never exported by dump
    /// except as a count").
    pub fn pending_anchor_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM pending_anchors", [], |r| r.get(0))?)
    }

    /// Every pending row written from `worktree` — D2's first claim condition,
    /// applied before the caller checks ancestry.
    pub fn pending_anchors_in_worktree(&self, worktree: &str) -> Result<Vec<PendingAnchor>> {
        let mut stmt = self.conn.prepare(
            "SELECT entity_id, worktree, head_at_write, created_at FROM pending_anchors \
             WHERE worktree = ?1",
        )?;
        Ok(stmt
            .query_map(rusqlite::params![worktree], row_to_pending_anchor)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every pending row, for `status`'s D4 unanchored report and D3a's
    /// reconciliation pass.
    pub fn all_pending_anchors(&self) -> Result<Vec<PendingAnchor>> {
        let mut stmt = self.conn.prepare(
            "SELECT entity_id, worktree, head_at_write, created_at FROM pending_anchors",
        )?;
        Ok(stmt
            .query_map([], row_to_pending_anchor)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Remove a pending row once its entity has been claimed (D2) or
    /// anchored by hand (D4's `--commit`/`memory anchor <id>` escape hatches).
    pub fn remove_pending_anchor(&self, entity_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM pending_anchors WHERE entity_id = ?1",
            rusqlite::params![entity_id],
        )?;
        Ok(())
    }

    /// D3a: point a still-pending row at the commit that replaced its
    /// `head_at_write` after a rebase, so the ordinary D2 ancestry test can
    /// claim it against the replacement on the next commit in that worktree.
    pub fn reassign_pending_anchor_head(
        &self,
        entity_id: &str,
        new_head_at_write: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_anchors SET head_at_write = ?1 WHERE entity_id = ?2",
            rusqlite::params![new_head_at_write, entity_id],
        )?;
        Ok(())
    }

    /// The cached `git patch-id --stable` for `commit_sha`, and whether it has
    /// been computed before at all: `Ok(None)` means "not seen", `Ok(Some(None))`
    /// means "seen, and it is a merge with no patch-id" (D3a).
    pub fn cached_patch_id(&self, commit_sha: &str) -> Result<Option<Option<String>>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT patch_id FROM patch_id_cache WHERE commit_sha = ?1",
                rusqlite::params![commit_sha],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?)
    }

    /// Record `commit_sha`'s patch-id (or `None` for a merge) so a later pass
    /// does not recompute it (D3a: "cost one patch-id per commit not seen
    /// before").
    pub fn cache_patch_id(&self, commit_sha: &str, patch_id: Option<&str>) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO patch_id_cache (commit_sha, patch_id) VALUES (?1, ?2)",
            rusqlite::params![commit_sha, patch_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    fn register_sqlite_vec() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });
    }

    fn open_store() -> MemoryStore {
        register_sqlite_vec();
        MemoryStore::open(std::path::Path::new(":memory:"))
            .expect("failed to open in-memory MemoryStore")
    }

    #[test]
    fn a_recorded_pending_anchor_is_counted_and_listed_by_worktree() {
        let store = open_store();
        store
            .record_pending_anchor("e1", "/repo/.git", "abc123")
            .unwrap();
        store
            .record_pending_anchor("e2", "/other/.git", "def456")
            .unwrap();

        assert_eq!(store.pending_anchor_count().unwrap(), 2);
        let in_repo = store.pending_anchors_in_worktree("/repo/.git").unwrap();
        assert_eq!(in_repo.len(), 1);
        assert_eq!(in_repo[0].entity_id, "e1");
        assert_eq!(in_repo[0].head_at_write, "abc123");
    }

    #[test]
    fn a_second_write_for_the_same_entity_leaves_the_first_pending_row_in_place() {
        let store = open_store();
        store
            .record_pending_anchor("e1", "/repo/.git", "first")
            .unwrap();
        store
            .record_pending_anchor("e1", "/repo/.git", "second")
            .unwrap();

        let rows = store.all_pending_anchors().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].head_at_write, "first");
    }

    #[test]
    fn removing_a_pending_anchor_drops_it_and_only_it() {
        let store = open_store();
        store
            .record_pending_anchor("e1", "/repo/.git", "abc")
            .unwrap();
        store
            .record_pending_anchor("e2", "/repo/.git", "def")
            .unwrap();

        store.remove_pending_anchor("e1").unwrap();

        let rows = store.all_pending_anchors().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entity_id, "e2");
    }

    #[test]
    fn reassigning_a_pending_anchor_head_updates_only_that_column() {
        let store = open_store();
        store
            .record_pending_anchor("e1", "/repo/.git", "old-head")
            .unwrap();

        store
            .reassign_pending_anchor_head("e1", "new-head")
            .unwrap();

        let rows = store.all_pending_anchors().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].head_at_write, "new-head");
        assert_eq!(rows[0].worktree, "/repo/.git");
    }

    #[test]
    fn patch_id_cache_distinguishes_unseen_from_seen_merge() {
        let store = open_store();
        assert_eq!(store.cached_patch_id("sha1").unwrap(), None);

        store.cache_patch_id("sha1", Some("patchid1")).unwrap();
        assert_eq!(
            store.cached_patch_id("sha1").unwrap(),
            Some(Some("patchid1".to_string()))
        );

        store.cache_patch_id("sha2", None).unwrap();
        assert_eq!(store.cached_patch_id("sha2").unwrap(), Some(None));
    }
}
