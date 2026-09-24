//! Events-source metric computation (ADR-098 D3), against the local
//! `events` table (D5). Unlike the state source, these are observations, not
//! reproducible from a commit and a repository — they describe what actually
//! happened on this machine.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use crate::storage::memory::EventRow;

use super::state::{Rate, median};

/// Commands the events snapshot breaks out individually (ADR-098 D3's mock:
/// `calls.{context,search,memory.add}`). Every other recorded command
/// (`memory.supersede`, `harvest`, `sync`, `memory.list`, `memory.show`)
/// still counts toward the read/write automation rates and the actor/latency/
/// token aggregates below, just not its own named bucket.
const NAMED_CALLS: &[&str] = &["context", "search", "memory.add"];

/// Commands `auto.read_rate` divides over: the two retrieval entry points.
const READ_COMMANDS: &[&str] = &["search", "context"];
/// Commands `auto.write_rate` divides over: the two entries that mutate
/// memory directly (not `harvest`, which is its own actor kind, not a
/// caller-triggered read/write in the D3 sense).
const WRITE_COMMANDS: &[&str] = &["memory.add", "memory.supersede"];

/// The four actor buckets `by_actor` always reports, present with a 0 count
/// when nothing recorded under it.
const ACTOR_BUCKETS: &[&str] = &["human", "agent", "harvest", "unknown"];

/// Per-command call counts, split by declared trigger (ADR-098 D5:
/// `explicit` | `hook` | `unknown`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct CallCounts {
    pub total: u64,
    pub explicit: u64,
    pub hook: u64,
    pub unknown: u64,
}

impl CallCounts {
    fn from_rows<'a>(rows: impl Iterator<Item = &'a EventRow>) -> Self {
        let mut c = CallCounts::default();
        for r in rows {
            c.total += 1;
            match r.trigger.as_str() {
                "explicit" => c.explicit += 1,
                "hook" => c.hook += 1,
                _ => c.unknown += 1,
            }
        }
        c
    }
}

/// The `events` block of an `inkentry.metrics/1` snapshot (ADR-098 D3, D7).
///
/// `use.acted_on_rate` and `use.recall_miss_rate` (D3) are not computed here:
/// `acted_on_rate` needs a session-end boundary this schema does not record
/// (an event has no "session closed" signal, only a hashed `session_ref`
/// grouping), and `recall_miss_rate` is state-derived (it needs no events at
/// all) and already has a natural home in a later `StateMetrics` addition,
/// not this block. Both are named in the ADR so the gap is visible rather
/// than silently absent.
#[derive(Debug, Clone, Serialize)]
pub struct EventsMetrics {
    pub window_days: u32,
    #[serde(rename = "use.sessions_with_context")]
    pub use_sessions_with_context: Rate,
    #[serde(rename = "use.search_hit_rate")]
    pub use_search_hit_rate: Rate,
    #[serde(rename = "use.search_before_write")]
    pub use_search_before_write: Rate,
    #[serde(rename = "auto.read_rate")]
    pub auto_read_rate: Rate,
    #[serde(rename = "auto.write_rate")]
    pub auto_write_rate: Rate,
    /// `calls.{context,search,memory.add}` (see [`NAMED_CALLS`]).
    pub calls: BTreeMap<String, CallCounts>,
    /// Every recorded event in the window, by `actor_kind`.
    pub by_actor: BTreeMap<String, u64>,
    pub latency_ms_p50: Option<i64>,
    pub tokens_out_p50: Option<i64>,
}

/// Compute [`EventsMetrics`] over `rows`, which must already be the events
/// whose `at` falls in the target window (`MemoryStore::events_in_window`),
/// ordered oldest first — the order [`Self`]'s session-relative formulas
/// (`use.sessions_with_context`, `use.search_before_write`) depend on.
pub fn compute_events_metrics(rows: &[EventRow], window_days: u32) -> EventsMetrics {
    let sessions = group_by_session(rows);

    let use_sessions_with_context = {
        let denominator = sessions.len() as u64;
        let numerator = sessions
            .values()
            .filter(|rows| rows.iter().take(3).any(|r| r.command == "context"))
            .count() as u64;
        Rate::new(numerator, denominator)
    };

    let use_search_hit_rate = {
        let searches: Vec<&EventRow> = rows.iter().filter(|r| r.command == "search").collect();
        let numerator = searches
            .iter()
            .filter(|r| r.code_results.unwrap_or(0) + r.memory_results.unwrap_or(0) > 0)
            .count() as u64;
        Rate::new(numerator, searches.len() as u64)
    };

    let use_search_before_write = {
        let adds: Vec<&EventRow> = rows.iter().filter(|r| r.command == "memory.add").collect();
        let numerator = adds
            .iter()
            .filter(|add| {
                add.session_ref.as_deref().is_some_and(|session| {
                    sessions.get(session).is_some_and(|session_rows| {
                        session_rows.iter().any(|r| {
                            matches!(r.command.as_str(), "search" | "context") && r.at < add.at
                        })
                    })
                })
            })
            .count() as u64;
        Rate::new(numerator, adds.len() as u64)
    };

    let auto_read_rate = automation_rate(rows, READ_COMMANDS);
    let auto_write_rate = automation_rate(rows, WRITE_COMMANDS);

    let calls: BTreeMap<String, CallCounts> = NAMED_CALLS
        .iter()
        .map(|&cmd| {
            (
                cmd.to_string(),
                CallCounts::from_rows(rows.iter().filter(|r| r.command == cmd)),
            )
        })
        .collect();

    let mut by_actor: BTreeMap<String, u64> =
        ACTOR_BUCKETS.iter().map(|k| (k.to_string(), 0)).collect();
    for r in rows {
        *by_actor.entry(r.actor_kind.clone()).or_insert(0) += 1;
    }

    let latency_ms_p50 = median(rows.iter().filter_map(|r| r.latency_ms).collect());
    let tokens_out_p50 = median(rows.iter().filter_map(|r| r.tokens_out).collect());

    EventsMetrics {
        window_days,
        use_sessions_with_context,
        use_search_hit_rate,
        use_search_before_write,
        auto_read_rate,
        auto_write_rate,
        calls,
        by_actor,
        latency_ms_p50,
        tokens_out_p50,
    }
}

/// `numerator` = events among `commands` whose `trigger` is `hook`;
/// `denominator` = every event among `commands`.
fn automation_rate(rows: &[EventRow], commands: &[&str]) -> Rate {
    let matching: Vec<&EventRow> = rows
        .iter()
        .filter(|r| commands.contains(&r.command.as_str()))
        .collect();
    let numerator = matching.iter().filter(|r| r.trigger == "hook").count() as u64;
    Rate::new(numerator, matching.len() as u64)
}

/// Group rows by `session_ref`, preserving each session's relative order
/// (input is already sorted by `at`). Events with no `session_ref` join no
/// session and are excluded from every session-relative formula.
fn group_by_session(rows: &[EventRow]) -> HashMap<&str, Vec<&EventRow>> {
    let mut groups: HashMap<&str, Vec<&EventRow>> = HashMap::new();
    for r in rows {
        if let Some(session) = r.session_ref.as_deref() {
            groups.entry(session).or_default().push(r);
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(command: &str, trigger: &str, actor_kind: &str, at: i64) -> EventRow {
        EventRow {
            at,
            command: command.to_string(),
            surface: "cli".to_string(),
            trigger: trigger.to_string(),
            actor_kind: actor_kind.to_string(),
            session_ref: None,
            code_results: None,
            memory_results: None,
            returned_ids: None,
            tokens_out: None,
            latency_ms: None,
            ok: true,
        }
    }

    fn with_session(mut r: EventRow, session: &str) -> EventRow {
        r.session_ref = Some(session.to_string());
        r
    }

    #[test]
    fn empty_events_yield_null_rates_never_a_fabricated_zero() {
        let m = compute_events_metrics(&[], 7);
        assert_eq!(m.use_sessions_with_context.value, None);
        assert_eq!(m.use_search_hit_rate.value, None);
        assert_eq!(m.auto_read_rate.value, None);
        assert_eq!(m.latency_ms_p50, None);
    }

    #[test]
    fn sessions_with_context_counts_context_in_the_first_three_events() {
        let rows = vec![
            with_session(row("context", "explicit", "human", 1), "s1"),
            with_session(row("search", "explicit", "human", 2), "s1"),
            with_session(row("search", "explicit", "human", 1), "s2"),
            with_session(row("search", "explicit", "human", 2), "s2"),
            with_session(row("search", "explicit", "human", 3), "s2"),
            with_session(row("context", "explicit", "human", 4), "s2"),
        ];
        let m = compute_events_metrics(&rows, 7);
        assert_eq!(m.use_sessions_with_context.numerator, 1);
        assert_eq!(m.use_sessions_with_context.denominator, 2);
    }

    #[test]
    fn search_hit_rate_counts_nonzero_result_searches() {
        let mut hit = row("search", "explicit", "human", 1);
        hit.code_results = Some(2);
        hit.memory_results = Some(0);
        let mut miss = row("search", "explicit", "human", 2);
        miss.code_results = Some(0);
        miss.memory_results = Some(0);
        let m = compute_events_metrics(&[hit, miss], 7);
        assert_eq!(m.use_search_hit_rate.numerator, 1);
        assert_eq!(m.use_search_hit_rate.denominator, 2);
    }

    #[test]
    fn search_before_write_requires_the_same_session_and_earlier_timestamp() {
        let preceded = vec![
            with_session(row("search", "explicit", "human", 1), "s1"),
            with_session(row("memory.add", "explicit", "human", 2), "s1"),
        ];
        let m = compute_events_metrics(&preceded, 7);
        assert_eq!(m.use_search_before_write, Rate::new(1, 1));

        // Add with no session_ref: cannot be preceded by anything.
        let unlinked = vec![row("memory.add", "explicit", "human", 1)];
        let m = compute_events_metrics(&unlinked, 7);
        assert_eq!(m.use_search_before_write, Rate::new(0, 1));

        // Search AFTER the add in the same session does not count.
        let wrong_order = vec![
            with_session(row("memory.add", "explicit", "human", 1), "s1"),
            with_session(row("search", "explicit", "human", 2), "s1"),
        ];
        let m = compute_events_metrics(&wrong_order, 7);
        assert_eq!(m.use_search_before_write, Rate::new(0, 1));
    }

    #[test]
    fn automation_rates_split_hook_from_everything_else() {
        let rows = vec![
            row("search", "hook", "agent", 1),
            row("search", "explicit", "human", 2),
            row("context", "hook", "agent", 3),
            row("memory.add", "hook", "agent", 4),
            row("memory.supersede", "explicit", "human", 5),
        ];
        let m = compute_events_metrics(&rows, 7);
        assert_eq!(m.auto_read_rate, Rate::new(2, 3));
        assert_eq!(m.auto_write_rate, Rate::new(1, 2));
    }

    #[test]
    fn calls_breaks_out_only_the_three_named_commands_by_trigger() {
        let rows = vec![
            row("search", "explicit", "human", 1),
            row("search", "hook", "agent", 2),
            row("search", "sometimes-unrecognised", "human", 3),
            row("harvest", "explicit", "human", 4),
        ];
        let m = compute_events_metrics(&rows, 7);
        let search = &m.calls["search"];
        assert_eq!(search.total, 3);
        assert_eq!(search.explicit, 1);
        assert_eq!(search.hook, 1);
        assert_eq!(search.unknown, 1);
        assert_eq!(m.calls["context"].total, 0);
        assert_eq!(m.calls["memory.add"].total, 0);
        assert!(
            !m.calls.contains_key("harvest"),
            "harvest has no named bucket"
        );
    }

    #[test]
    fn by_actor_always_reports_all_four_buckets() {
        let rows = vec![row("search", "explicit", "human", 1)];
        let m = compute_events_metrics(&rows, 7);
        assert_eq!(m.by_actor["human"], 1);
        assert_eq!(m.by_actor["agent"], 0);
        assert_eq!(m.by_actor["harvest"], 0);
        assert_eq!(m.by_actor["unknown"], 0);
    }

    #[test]
    fn latency_and_tokens_medians_ignore_rows_with_no_value() {
        let mut rows = vec![
            row("search", "explicit", "human", 1),
            row("search", "explicit", "human", 2),
            row("search", "explicit", "human", 3),
        ];
        rows[0].latency_ms = Some(10);
        rows[1].latency_ms = Some(20);
        // rows[2] has no latency recorded and must not skew the median.
        rows[0].tokens_out = Some(100);
        let m = compute_events_metrics(&rows, 7);
        assert_eq!(m.latency_ms_p50, Some(15));
        assert_eq!(m.tokens_out_p50, Some(100));
    }
}
