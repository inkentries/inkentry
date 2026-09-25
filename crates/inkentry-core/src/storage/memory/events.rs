//! ADR-098 D5: the local `events` table.
//!
//! A best-effort record of command invocations, kept for the metrics events
//! source (`inkentry_core::metrics`). Never part of the git-notes carrier and
//! never read by a sync path — it is local working state in the same sense
//! as any other projection detail, and `inkentry metrics clear` empties it.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rusqlite::Connection;

use super::MemoryStore;

/// How long [`record_event_at`] waits on a lock held by another writer before
/// giving up. Short and deliberately not zero: `memory.db` is shared by every
/// agent and linked worktree, and a read command must never wait long on a
/// writer to record that it ran, but the busiest real contention (two
/// commands finishing within the same instant) clears in well under this.
const RECORD_BUSY_TIMEOUT_MS: u32 = 200;

/// One row to record, gathered by a command after its response is written.
/// Every optional field is `None` when the command has nothing to report for
/// it (a command with no code corpus never has `code_results`, for example).
pub struct EventFields<'a> {
    pub command: &'a str,
    pub surface: &'a str,
    pub trigger: &'a str,
    pub actor_kind: &'a str,
    pub session_ref: Option<&'a str>,
    pub code_results: Option<i64>,
    pub memory_results: Option<i64>,
    /// Comma-joined `entity_id`s of the memory entries returned (search,
    /// context) or written (add) — never titles, paths or query text (D5).
    pub returned_ids: Option<&'a str>,
    pub tokens_out: Option<i64>,
    pub latency_ms: Option<i64>,
    pub ok: bool,
}

/// A stored `events` row, as the metrics computations read it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub at: i64,
    pub command: String,
    pub surface: String,
    pub trigger: String,
    pub actor_kind: String,
    pub session_ref: Option<String>,
    pub code_results: Option<i64>,
    pub memory_results: Option<i64>,
    pub returned_ids: Option<String>,
    pub tokens_out: Option<i64>,
    pub latency_ms: Option<i64>,
    pub ok: bool,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Best-effort insert into `events` at `db_path` (the project's `memory.db`).
///
/// Opens its own short-lived connection with a short busy timeout rather than
/// sharing a caller's, so this is the one place the "never make a read command
/// wait to record that it ran" rule (D5) lives — every recording call site
/// funnels through here instead of re-implementing the timeout. Any failure —
/// no file at `db_path`, a lock still held past the timeout, a schema this
/// build predates — silently drops the event; a command's exit status and
/// output are never affected by this call, and nothing is ever logged, since
/// a dropped event is an expected outcome of contention, not a fault.
pub fn record_event_at(db_path: &Path, fields: EventFields) {
    let Ok(conn) = Connection::open(db_path) else {
        return;
    };
    if conn
        .execute_batch(&format!("PRAGMA busy_timeout = {RECORD_BUSY_TIMEOUT_MS}"))
        .is_err()
    {
        return;
    }
    let _ = conn.execute(
        "INSERT INTO events \
         (at, command, surface, trigger, actor_kind, session_ref, code_results, \
          memory_results, returned_ids, tokens_out, latency_ms, ok) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            now_secs(),
            fields.command,
            fields.surface,
            fields.trigger,
            fields.actor_kind,
            fields.session_ref,
            fields.code_results,
            fields.memory_results,
            fields.returned_ids,
            fields.tokens_out,
            fields.latency_ms,
            fields.ok as i64,
        ],
    );
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        at: row.get(0)?,
        command: row.get(1)?,
        surface: row.get(2)?,
        trigger: row.get(3)?,
        actor_kind: row.get(4)?,
        session_ref: row.get(5)?,
        code_results: row.get(6)?,
        memory_results: row.get(7)?,
        returned_ids: row.get(8)?,
        tokens_out: row.get(9)?,
        latency_ms: row.get(10)?,
        ok: row.get::<_, i64>(11)? != 0,
    })
}

const EVENT_COLUMNS: &str = "at, command, surface, trigger, actor_kind, session_ref, \
     code_results, memory_results, returned_ids, tokens_out, latency_ms, ok";

impl MemoryStore {
    /// Every event whose `at` falls in `[window_start, window_end]` (both
    /// inclusive), oldest first — the metrics events source's whole input.
    pub fn events_in_window(&self, window_start: i64, window_end: i64) -> Result<Vec<EventRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE at >= ?1 AND at <= ?2 ORDER BY at ASC"
        ))?;
        let rows = stmt.query_map(rusqlite::params![window_start, window_end], row_to_event)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// The `at` of the most recently recorded event. `None` when no event has
    /// been recorded.
    pub fn newest_event_at(&self) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT MAX(at) FROM events", [], |r| r.get(0))?)
    }

    /// `(command, count)` for every event since `cutoff` (inclusive), grouped
    /// by command and ordered by count descending — `inkentry status`'s 7-day
    /// usage summary, now read from `events` (D5) rather than `index.db`'s
    /// `usage` table.
    pub fn events_command_counts_since(&self, cutoff: i64) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT command, COUNT(*) FROM events \
             WHERE at >= ?1 \
             GROUP BY command \
             ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![cutoff], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Empty the `events` table — `inkentry metrics clear`. Nothing else in
    /// `memory.db` is touched: the table is the whole of the local event log
    /// (D5), and clearing it is the whole of the privacy story a separate
    /// file would otherwise have given.
    pub fn clear_events(&self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM events", [])?)
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

    fn open_store() -> (tempfile::TempDir, MemoryStore) {
        register_sqlite_vec();
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("memory.db");
        let store = MemoryStore::open(&path).expect("open");
        (tmp, store)
    }

    fn fields(command: &str, at_offset: i64) -> EventFields<'_> {
        let _ = at_offset;
        EventFields {
            command,
            surface: "cli",
            trigger: "explicit",
            actor_kind: "human",
            session_ref: None,
            code_results: Some(2),
            memory_results: Some(1),
            returned_ids: Some("abc,def"),
            tokens_out: Some(120),
            latency_ms: Some(15),
            ok: true,
        }
    }

    #[test]
    fn recording_then_reading_back_round_trips_every_field() {
        let (tmp, store) = open_store();
        let path = tmp.path().join("memory.db");
        record_event_at(&path, fields("search", 0));

        let rows = store.events_in_window(0, i64::MAX).expect("read");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.command, "search");
        assert_eq!(row.surface, "cli");
        assert_eq!(row.trigger, "explicit");
        assert_eq!(row.actor_kind, "human");
        assert_eq!(row.code_results, Some(2));
        assert_eq!(row.memory_results, Some(1));
        assert_eq!(row.returned_ids.as_deref(), Some("abc,def"));
        assert_eq!(row.tokens_out, Some(120));
        assert_eq!(row.latency_ms, Some(15));
        assert!(row.ok);
    }

    #[test]
    fn recording_against_a_missing_file_is_silently_dropped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("does-not-exist").join("memory.db");
        // No parent directory, no schema: this must not panic and must not
        // create anything.
        record_event_at(&path, fields("search", 0));
        assert!(!path.exists());
    }

    #[test]
    fn events_in_window_excludes_rows_outside_the_window() {
        let (tmp, store) = open_store();
        let path = tmp.path().join("memory.db");
        record_event_at(&path, fields("search", 0));

        let rows = store
            .events_in_window(1_000_000_000_000, 2_000_000_000_000)
            .unwrap();
        assert!(
            rows.is_empty(),
            "an event before the window must not appear in it"
        );
    }

    #[test]
    fn command_counts_since_groups_and_orders_by_count_descending() {
        let (tmp, store) = open_store();
        let path = tmp.path().join("memory.db");
        record_event_at(&path, fields("search", 0));
        record_event_at(&path, fields("search", 0));
        record_event_at(&path, fields("context", 0));

        let counts = store.events_command_counts_since(0).unwrap();
        assert_eq!(
            counts,
            vec![("search".to_string(), 2), ("context".to_string(), 1)]
        );
    }

    #[test]
    fn command_counts_since_excludes_rows_before_the_cutoff() {
        let (tmp, store) = open_store();
        let path = tmp.path().join("memory.db");
        record_event_at(&path, fields("search", 0));

        let counts = store.events_command_counts_since(4_000_000_000).unwrap();
        assert!(counts.is_empty());
    }

    #[test]
    fn clear_events_empties_the_table_and_reports_how_many() {
        let (tmp, store) = open_store();
        let path = tmp.path().join("memory.db");
        record_event_at(&path, fields("search", 0));
        record_event_at(&path, fields("context", 0));

        let cleared = store.clear_events().unwrap();
        assert_eq!(cleared, 2);
        assert!(store.events_in_window(0, i64::MAX).unwrap().is_empty());
    }

    #[test]
    fn a_locked_memory_db_makes_recording_drop_the_event_not_hang() {
        let (tmp, store) = open_store();
        let path = tmp.path().join("memory.db");

        let locker = rusqlite::Connection::open(&path).expect("second connection");
        locker
            .execute_batch("BEGIN IMMEDIATE; CREATE TABLE lock_probe (id INTEGER);")
            .expect("acquire the write lock");

        // Must return promptly (bounded by RECORD_BUSY_TIMEOUT_MS) rather than
        // waiting indefinitely, and must not panic.
        record_event_at(&path, fields("search", 0));
        drop(locker);

        assert!(store.events_in_window(0, i64::MAX).unwrap().is_empty());
    }
}
