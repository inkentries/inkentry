//! Generic forward-only `PRAGMA user_version` migration ladder, shared by
//! every local store that stamps one. `memory.db` is the first caller
//! (`storage::memory::migrate`); a store with its own registry and
//! label reuses this runner rather than growing a second copy of it.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// One step in a ladder: the schema version it produces, and the body that
/// gets a store there from the version immediately below. Runs inside the
/// step's own transaction, alongside its `PRAGMA user_version` stamp.
pub(crate) type MigrationStep = fn(&Connection) -> Result<()>;

/// Applies every step in `steps` whose version is greater than
/// `found_version` and at most `target_version`, in order. Each step commits
/// atomically with its `PRAGMA user_version` stamp, so a crash or a failing
/// step leaves the store at the last version fully applied rather than
/// half-migrated. Runs each step under `BEGIN IMMEDIATE`: a concurrent
/// opener of the same file serialises on the write lock and, on its turn,
/// finds the version already advanced past the step and moves on without
/// re-running it.
///
/// `label` names the store in the one stderr line this prints when it
/// actually migrates something (e.g. `"memory.db"`); nothing is printed when
/// every step is already applied, and nothing ever goes to stdout.
pub(crate) fn apply_ladder(
    conn: &Connection,
    found_version: i32,
    target_version: i32,
    steps: &[(i32, MigrationStep)],
    label: &str,
) -> Result<()> {
    let mut at_version = found_version;

    for &(version, step) in steps {
        if version <= found_version {
            continue;
        }
        if version > target_version {
            break;
        }
        conn.execute_batch("BEGIN IMMEDIATE")
            .with_context(|| format!("locking {label} to apply migration step {version}"))?;
        let result = (|| -> Result<()> {
            // Re-read under the write lock: a concurrent opener may have
            // already applied this step while we were waiting for it.
            let current: i32 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .context("reading user_version")?;
            if current >= version {
                return Ok(());
            }
            step(conn).context("running migration step body")?;
            conn.execute_batch(&format!("PRAGMA user_version = {version}"))
                .context("stamping schema version")?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                conn.execute_batch("COMMIT")
                    .with_context(|| format!("committing {label} migration step {version}"))?;
                at_version = version;
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e.context(format!(
                    "migrating {label} to schema version {version} failed; \
                     store left at schema version {at_version}"
                )));
            }
        }
    }

    if at_version != found_version {
        eprintln!("{label}: migrated schema version {found_version} to {at_version}");
    }
    // A registry that stops short would otherwise hand back a store at an
    // older shape than the caller is about to query.
    anyhow::ensure!(
        at_version == target_version,
        "{label} is at schema version {at_version} but this build needs {target_version}, \
         and no migration step covers the gap"
    );
    Ok(())
}

/// Asserts `steps` is contiguous and strictly increasing, starting at
/// `first` and ending at `last`. A gap or a duplicate would silently skip or
/// double-stamp a version. Trivially true when `steps` is empty and `first >
/// last` (no version between them to fill yet). Test-only: production never
/// needs to re-check its own registry at runtime, only a test needs to hold
/// it to this.
#[cfg(test)]
pub(crate) fn assert_contiguous(steps: &[(i32, MigrationStep)], first: i32, last: i32) {
    if steps.is_empty() {
        assert!(
            first > last,
            "an empty ladder is only valid while there is no version between {first} and \
             {last} to fill"
        );
        return;
    }
    let versions: Vec<i32> = steps.iter().map(|(v, _)| *v).collect();
    let expected: Vec<i32> = (first..=last).collect();
    assert_eq!(
        versions, expected,
        "migration steps must be contiguous from {first} to {last} with no gaps or reordering"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;
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

    fn conn_at_version(version: i32) -> Connection {
        register_sqlite_vec();
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(&format!("PRAGMA user_version = {version}"))
            .expect("stamp");
        conn
    }

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            rusqlite::params![name],
            |_| Ok(()),
        )
        .optional()
        .expect("probing table")
        .is_some()
    }

    fn create_a(conn: &Connection) -> Result<()> {
        conn.execute_batch("CREATE TABLE step_a (id INTEGER)")?;
        Ok(())
    }

    fn create_b(conn: &Connection) -> Result<()> {
        // Depends on step_a already existing, to prove ordering.
        conn.execute_batch("INSERT INTO step_a (id) VALUES (1)")?;
        conn.execute_batch("CREATE TABLE step_b (id INTEGER)")?;
        Ok(())
    }

    fn create_d(conn: &Connection) -> Result<()> {
        conn.execute_batch("CREATE TABLE step_d (id INTEGER)")?;
        Ok(())
    }

    #[test]
    fn eleven_to_thirteen_applies_steps_in_order_and_stamps_each() {
        let conn = conn_at_version(11);
        let steps: &[(i32, MigrationStep)] = &[(12, create_a), (13, create_b)];

        apply_ladder(&conn, 11, 13, steps, "test.db").expect("ladder");

        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 13);
        assert!(table_exists(&conn, "step_a"));
        assert!(table_exists(&conn, "step_b"));
        let seeded: i64 = conn
            .query_row("SELECT count(*) FROM step_a", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            seeded, 1,
            "step_b's insert into step_a only succeeds if step_a ran first"
        );
    }

    #[test]
    fn a_failing_step_leaves_the_store_at_the_last_completed_version() {
        fn create_c_then_fail(conn: &Connection) -> Result<()> {
            conn.execute_batch("CREATE TABLE step_c_partial (id INTEGER)")?;
            anyhow::bail!("synthetic failure for the rollback test")
        }

        let conn = conn_at_version(11);
        let steps: &[(i32, MigrationStep)] =
            &[(12, create_a), (13, create_c_then_fail), (14, create_d)];

        let err = apply_ladder(&conn, 11, 14, steps, "test.db")
            .expect_err("a failing step must surface as an error");
        assert!(
            err.to_string().contains("13") && err.to_string().contains("12"),
            "the error must name the failing step and the version the store was left at: {err}"
        );

        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, 12,
            "the earlier, successful step must stay committed"
        );
        assert!(table_exists(&conn, "step_a"));
        assert!(
            !table_exists(&conn, "step_c_partial"),
            "the failing step's own writes must be rolled back, not left partially applied"
        );
    }

    #[test]
    fn reopening_after_a_fixed_release_resumes_from_the_last_completed_version() {
        fn create_c_then_fail(conn: &Connection) -> Result<()> {
            conn.execute_batch("CREATE TABLE step_c_partial (id INTEGER)")?;
            anyhow::bail!("synthetic failure for the rollback test")
        }
        fn create_c_fixed(conn: &Connection) -> Result<()> {
            conn.execute_batch("CREATE TABLE step_c (id INTEGER)")?;
            Ok(())
        }

        let conn = conn_at_version(11);
        let broken: &[(i32, MigrationStep)] =
            &[(12, create_a), (13, create_c_then_fail), (14, create_d)];
        apply_ladder(&conn, 11, 14, broken, "test.db").expect_err("first open must fail");

        // A later build ships a fixed step 13; the next open passes the
        // ladder it would actually run, which never repeats step 12.
        let fixed: &[(i32, MigrationStep)] =
            &[(12, create_a), (13, create_c_fixed), (14, create_d)];
        apply_ladder(&conn, 12, 14, fixed, "test.db").expect("resumed ladder must succeed");

        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 14);
        assert!(table_exists(&conn, "step_c"));
        assert!(!table_exists(&conn, "step_c_partial"));
    }

    #[test]
    fn a_step_already_applied_by_a_concurrent_opener_is_skipped() {
        // Simulate a concurrent winner having already stamped 12 before this
        // call's per-step check runs.
        let conn = conn_at_version(12);
        let steps: &[(i32, MigrationStep)] = &[(12, create_a), (13, create_d)];

        apply_ladder(&conn, 12, 13, steps, "test.db").expect("ladder");

        assert!(
            !table_exists(&conn, "step_a"),
            "a step whose version is already stamped must not run its body again"
        );
        assert!(table_exists(&conn, "step_d"));
    }

    #[test]
    fn a_registry_that_stops_short_of_the_target_is_an_error() {
        let conn = conn_at_version(11);
        let steps: &[(i32, MigrationStep)] = &[(12, create_a)];
        let err = apply_ladder(&conn, 11, 13, steps, "test.db").expect_err("gap to 13");
        assert!(format!("{err:#}").contains("no migration step covers the gap"));
        assert!(
            table_exists(&conn, "step_a"),
            "the step that exists still applies"
        );
    }

    #[test]
    fn a_contiguous_registry_is_accepted() {
        let steps: &[(i32, MigrationStep)] = &[(12, create_a), (13, create_b)];
        assert_contiguous(steps, 12, 13);
    }

    #[test]
    fn an_empty_registry_is_accepted_only_with_nothing_to_fill() {
        let steps: &[(i32, MigrationStep)] = &[];
        assert_contiguous(steps, 12, 11);
    }

    #[test]
    #[should_panic(expected = "contiguous")]
    fn a_registry_with_a_gap_is_rejected() {
        let steps: &[(i32, MigrationStep)] = &[(12, create_a), (14, create_b)];
        assert_contiguous(steps, 12, 14);
    }
}
