//! Sync support for the local memory store (ADR-037 D2).
//!
//! Identity model. Two stable identifiers bridge the local and cloud stores:
//!
//! * `uuid` — the entry's local identity (a fresh UUIDv7, Founder decision §3).
//!   Pushed to the cloud as `external_id`; the cloud-api batch endpoint dedupes
//!   on it, so re-pushing the same entry is idempotent (skipped server-side).
//! * `remote_id` — the cloud-minted entry id. cloud-api mints its own UUIDv7
//!   `id`, independent of our `external_id`. We record it on push (from the 207
//!   batch result) and on pull. **Pull dedupes on `remote_id`**, so an entry
//!   that originated locally is never re-inserted when it returns on the `since`
//!   feed.
//!
//! Every method here keys on these stable ids, never the machine-local
//! autoincrement `id`, which is what makes a re-run of `sync` a no-op.

use anyhow::Result;
use rusqlite::OptionalExtension;
use uuid::Uuid;

use super::MemoryStore;

/// A local note prepared for push to the cloud (ADR-037 D2/D3).
///
/// Carries the stable `uuid` (used as the cloud `external_id` / idempotency key)
/// and **text only** — no embedding vector. The server backfills the embedding
/// with its configured model (ADR-010/ADR-020 conformance); shipping a local
/// vector would reintroduce the embedding-space mismatch ADR-020 removed.
#[derive(Debug, Clone)]
pub struct SyncRow {
    /// Local autoincrement id (for recording the cloud id after a push).
    pub local_id: i64,
    /// Stable local identity (UUIDv7) → cloud `external_id`.
    pub uuid: String,
    /// Cloud-minted id, once known (set after a prior push/pull).
    pub remote_id: Option<String>,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub source_ref: Option<String>,
    /// Whether this entry is archived/tombstoned locally (drives cloud DELETE).
    pub archived: bool,
}

impl MemoryStore {
    /// Assign a fresh UUIDv7 to `note_id` if it lacks one; return the entry's
    /// UUID. Idempotent. This is the Founder-decided backfill (§3) — a *fresh*
    /// UUIDv7 minted on first sync, not a content-derived UUIDv5 — so identity is
    /// uniform with cloud-api's UUIDv7 default (ADR-032).
    pub fn ensure_uuid(&self, note_id: i64) -> Result<String> {
        if let Some(existing) = self.uuid_for(note_id)? {
            return Ok(existing);
        }
        let uuid = Uuid::now_v7().to_string();
        self.conn.execute(
            "UPDATE notes SET uuid = ?1 WHERE id = ?2 AND uuid IS NULL",
            rusqlite::params![uuid, note_id],
        )?;
        Ok(self.uuid_for(note_id)?.unwrap_or(uuid))
    }

    /// Return the UUID for a local note id, if assigned.
    pub fn uuid_for(&self, note_id: i64) -> Result<Option<String>> {
        let uuid: Option<String> = self
            .conn
            .query_row(
                "SELECT uuid FROM notes WHERE id = ?1",
                rusqlite::params![note_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(uuid)
    }

    /// Record the cloud-minted id for a local note (after a successful push or
    /// when first seen on pull). Idempotent.
    pub fn set_remote_id(&self, note_id: i64, remote_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE notes SET remote_id = ?1 WHERE id = ?2 AND remote_id IS NULL",
            rusqlite::params![remote_id, note_id],
        )?;
        Ok(())
    }

    /// Whether any local note already carries this cloud `remote_id`.
    pub fn has_remote_id(&self, remote_id: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE remote_id = ?1",
            rusqlite::params![remote_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Collect local notes to push, backfilling a fresh UUIDv7 on any that lack
    /// one. Returns text-only rows (no vectors) ordered oldest-first.
    ///
    /// `include_archived` mirrors the caller's flag; archived rows are still
    /// returned (as tombstones) when requested so deletes propagate (ADR-037 D2).
    pub fn rows_for_sync(&self, include_archived: bool) -> Result<Vec<SyncRow>> {
        let status_clause = if include_archived {
            ""
        } else {
            "WHERE status = 'active'"
        };
        // Read candidate ids first (immutable borrow), then mint UUIDs (mutating),
        // then build rows — avoids holding a statement across the UUID UPDATE.
        let ids: Vec<i64> = {
            let sql =
                format!("SELECT id FROM notes {status_clause} ORDER BY created_at ASC, id ASC");
            let mut stmt = self.conn.prepare(&sql)?;
            stmt.query_map([], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            self.ensure_uuid(id)?;
            if let Some(row) = self.sync_row(id)? {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// Build a [`SyncRow`] for a single note id (UUID must already be assigned).
    fn sync_row(&self, note_id: i64) -> Result<Option<SyncRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, uuid, remote_id, kind, title, body, source_ref, status \
                 FROM notes WHERE id = ?1",
                rusqlite::params![note_id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?;

        let Some((local_id, uuid, remote_id, kind, title, body, source_ref, status)) = row else {
            return Ok(None);
        };
        let Some(uuid) = uuid else { return Ok(None) };

        Ok(Some(SyncRow {
            local_id,
            uuid,
            remote_id,
            kind,
            title,
            body,
            source_ref,
            archived: status == "archived",
        }))
    }

    /// Idempotently apply a note pulled from the cloud, keyed by `remote_id`
    /// (the cloud-minted id).
    ///
    /// - If a local note already carries this `remote_id` (we pushed it, or we
    ///   pulled it before), reconcile lifecycle only — a cloud tombstone archives
    ///   the local copy. Content is append-only and never mutated (ADR-005).
    /// - Otherwise insert a new local row carrying `remote_id`. Add-Wins /
    ///   keep-both: pulled entries are added, never overwriting local ones.
    ///
    /// Returns `true` when a new row was inserted, `false` otherwise. Re-running
    /// with the same input is a no-op — the source of `sync`'s idempotency.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_remote_note(
        &self,
        remote_id: &str,
        kind: &str,
        title: &str,
        body: &str,
        source_ref: Option<&str>,
        created_at: i64,
        archived: bool,
    ) -> Result<bool> {
        if let Some(existing_id) = self.note_id_for_remote_id(remote_id)? {
            if archived {
                // Never un-archive (Add-Wins keeps the archive).
                self.archive(existing_id)?;
            }
            return Ok(false);
        }

        let status = if archived { "archived" } else { "active" };
        // A pulled entry gets a fresh local `uuid` too, so a later push of this
        // store still has a stable external_id for it.
        let uuid = Uuid::now_v7().to_string();
        self.conn.execute(
            "INSERT INTO notes (uuid, remote_id, kind, title, body, source_ref, status, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![uuid, remote_id, kind, title, body, source_ref, status, created_at],
        )?;
        Ok(true)
    }

    /// Local note id carrying the given cloud `remote_id`, if any.
    pub fn note_id_for_remote_id(&self, remote_id: &str) -> Result<Option<i64>> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM notes WHERE remote_id = ?1",
                rusqlite::params![remote_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Read the per-project pull watermark (ISO 8601), if stored.
    pub fn last_synced(&self, project_id: &str) -> Result<Option<String>> {
        let ts: Option<String> = self
            .conn
            .query_row(
                "SELECT last_synced FROM sync_state WHERE project_id = ?1",
                rusqlite::params![project_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(ts)
    }

    /// Persist the per-project pull watermark (ISO 8601).
    pub fn set_last_synced(&self, project_id: &str, watermark: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_state (project_id, last_synced, updated_at) \
             VALUES (?1, ?2, unixepoch()) \
             ON CONFLICT(project_id) DO UPDATE SET \
                 last_synced = excluded.last_synced, updated_at = unixepoch()",
            rusqlite::params![project_id, watermark],
        )?;
        Ok(())
    }

    /// Persist the per-project push watermark (ISO 8601).
    pub fn set_last_pushed(&self, project_id: &str, watermark: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_state (project_id, last_pushed, updated_at) \
             VALUES (?1, ?2, unixepoch()) \
             ON CONFLICT(project_id) DO UPDATE SET \
                 last_pushed = excluded.last_pushed, updated_at = unixepoch()",
            rusqlite::params![project_id, watermark],
        )?;
        Ok(())
    }
}
