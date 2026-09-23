//! State-source metric computation (ADR-098 D3), against a project's
//! `memory.db` and, when available, its git history. Every value here is
//! reproducible from a commit and a repository — no events, no evals.

use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::storage::memory::{MemoryEdge, MemoryStore, Note, NoteId};
use crate::storage::note_kind::NOTE_KINDS;

/// harvest.rs's existing near-duplicate threshold (`crates/inkentry-cli/src/
/// cli/cmd/memory/harvest.rs::DEDUP_THRESHOLD`), reused here rather than
/// re-derived so the two never drift apart.
const NEAR_DUPLICATE_THRESHOLD: f64 = 0.15;

/// Kinds `cmp.review_items_per_day` counts (ADR-098 D3).
const REVIEW_KINDS: &[&str] = &["decision", "requirement", "question", "antipattern"];

/// Per-kind section limits `cmp.tokens_context_estimate` sums over, mirroring
/// `inkentry-cli`'s `cli/cmd/context.rs::SECTIONS` defaults (handoff 3,
/// question 10, decision 10, requirement 10). Kept in sync by hand: a change
/// to those defaults should update this list too.
const CONTEXT_SECTIONS: &[(&str, usize)] = &[
    ("handoff", 3),
    ("question", 10),
    ("decision", 10),
    ("requirement", 10),
];

/// A ratio with its numerator and denominator carried alongside the computed
/// value (ADR-098: "every rate carries its numerator and denominator").
/// `value` is `None` when `denominator` is 0, never a fabricated `0.0`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Rate {
    pub numerator: u64,
    pub denominator: u64,
    pub value: Option<f64>,
}

impl Rate {
    pub fn new(numerator: u64, denominator: u64) -> Self {
        let value = (denominator > 0).then(|| numerator as f64 / denominator as f64);
        Self {
            numerator,
            denominator,
            value,
        }
    }
}

/// A median duration in seconds, with the sample size it was computed over.
/// `median_seconds` is `None` on an empty sample.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct MedianSeconds {
    pub sample_size: u64,
    pub median_seconds: Option<i64>,
}

/// [`Rate`]'s shape plus the count of active entries excluded from both the
/// numerator and the denominator for lacking a stored embedding (ADR-098:
/// "entries without a vector are excluded ... and the count of excluded
/// entries is reported").
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct NearDuplicateRate {
    pub numerator: u64,
    pub denominator: u64,
    pub value: Option<f64>,
    pub excluded_without_vector: u64,
}

/// The four origin buckets `rec.entries.by_origin` always reports, present
/// with a 0 count when the store holds none of it — the same "key set never
/// depends on what happens to be stored" rule `total`/`active`/`in_window`
/// already follow for kind.
const ORIGIN_BUCKETS: &[&str] = &["human", "agent", "harvest", "unknown"];

/// `rec.entries` (ADR-098 D3). Every canonical kind is present in
/// `total`/`active`/`in_window` with a 0 count when the store holds none of
/// it, so the key set never depends on what happens to be stored;
/// `by_origin` (D6) follows the same rule over the four origin buckets.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntryCounts {
    pub total: BTreeMap<String, u64>,
    pub active: BTreeMap<String, u64>,
    pub in_window: BTreeMap<String, u64>,
    /// Active entries by `origin.actor_kind`, or `unknown` for an entry with
    /// no origin recorded (an entry written before D6, or with no caller
    /// declaration).
    pub by_origin: BTreeMap<String, u64>,
}

/// Git facts a metrics window needs, gathered once by [`super::build_snapshot`]
/// and handed in rather than re-fetched per metric.
pub struct GitWindowFacts {
    /// Full shas of every commit reachable from HEAD whose committer time
    /// falls inside the window.
    pub commit_shas: Vec<String>,
    /// Sum of lines added + removed across those same commits.
    pub lines_changed_in_window: u64,
    /// Every commit sha that at least one memory entry's git-notes anchor
    /// names, anywhere in the ref (not window-filtered: membership is
    /// checked against `commit_shas`, which is already the window).
    pub anchored_commit_shas: HashSet<String>,
}

/// The `state` block of an `inkentry.metrics/1` snapshot (ADR-098 D3, D7).
#[derive(Debug, Clone, Serialize)]
pub struct StateMetrics {
    #[serde(rename = "rec.entries")]
    pub rec_entries: EntryCounts,
    #[serde(
        rename = "rec.commit_coverage",
        skip_serializing_if = "Option::is_none"
    )]
    pub rec_commit_coverage: Option<Rate>,
    #[serde(rename = "rec.supersede_rate")]
    pub rec_supersede_rate: Rate,
    #[serde(rename = "rec.time_to_supersede_p50")]
    pub rec_time_to_supersede_p50: MedianSeconds,
    #[serde(rename = "rec.open_question_age_p50")]
    pub rec_open_question_age_p50: MedianSeconds,
    #[serde(rename = "rec.near_duplicate_rate")]
    pub rec_near_duplicate_rate: NearDuplicateRate,
    #[serde(rename = "rec.unresolved_conflicts")]
    pub rec_unresolved_conflicts: u64,
    #[serde(
        rename = "cmp.lines_per_decision",
        skip_serializing_if = "Option::is_none"
    )]
    pub cmp_lines_per_decision: Option<Rate>,
    #[serde(rename = "cmp.review_items_per_day")]
    pub cmp_review_items_per_day: Rate,
    #[serde(rename = "cmp.tokens_context_estimate")]
    pub cmp_tokens_context_estimate: u64,
}

/// The cheap subset of [`StateMetrics`] `inkentry status` prints and carries
/// under its `metrics` JSON field: the pieces computable without a full
/// embedding scan (`rec.near_duplicate_rate`) or a `--numstat` walk of the
/// whole window (`cmp.lines_per_decision`), since `status` runs often and
/// must stay cheap. `cmp.tokens_context_estimate` is left out too — it is not
/// part of the compact section this summary backs.
#[derive(Debug, Clone, Serialize)]
pub struct StatusMetricsSummary {
    pub window_days: u32,
    #[serde(rename = "rec.entries_in_window")]
    pub rec_entries_in_window: BTreeMap<String, u64>,
    #[serde(
        rename = "rec.commit_coverage",
        skip_serializing_if = "Option::is_none"
    )]
    pub rec_commit_coverage: Option<Rate>,
    #[serde(rename = "rec.supersede_rate")]
    pub rec_supersede_rate: Rate,
    #[serde(rename = "rec.time_to_supersede_p50")]
    pub rec_time_to_supersede_p50: MedianSeconds,
    #[serde(rename = "rec.open_question_age_p50")]
    pub rec_open_question_age_p50: MedianSeconds,
    #[serde(rename = "cmp.review_items_per_day")]
    pub cmp_review_items_per_day: Rate,
    #[serde(rename = "rec.unresolved_conflicts")]
    pub rec_unresolved_conflicts: u64,
    /// ADR-098 D3/D5: the compact events subset `inkentry status` shows under
    /// "use, last 7 days" (explicit vs hook columns, automation rate). Always
    /// the fixed [`super::EVENTS_WINDOW_DAYS`] window, independent of
    /// `window_days` above (which only governs the state fields).
    pub events: super::EventsMetrics,
}

/// Compute [`StatusMetricsSummary`] for the window `[window_start,
/// window_end]`. `commit_shas_in_window`/`anchored_commit_shas` are `None`
/// together exactly when [`compute_state_metrics`]'s `git` is `None` (no git
/// repository); passed in rather than fetched here so the caller can use the
/// cheaper, `--numstat`-free commit listing ([`super::git::commit_shas_in_window`]).
#[allow(clippy::too_many_arguments)]
pub fn compute_status_metrics_summary(
    store: &MemoryStore,
    window_start: i64,
    window_end: i64,
    window_days: u32,
    commit_shas_in_window: Option<&[String]>,
    anchored_commit_shas: Option<&HashSet<String>>,
    events: super::EventsMetrics,
) -> Result<StatusMetricsSummary> {
    let all_notes = store.all_notes_for_dedup()?;
    let created_at_by_id: HashMap<&NoteId, i64> =
        all_notes.iter().map(|n| (&n.id, n.created_at)).collect();
    let kind_by_id: HashMap<&NoteId, &str> =
        all_notes.iter().map(|n| (&n.id, n.kind.as_str())).collect();

    let rec_entries_in_window = entry_counts(&all_notes, window_start, window_end).in_window;

    let supersede_edges = store.edges_of_kind("supersedes")?;
    let relates_to_edges = store.edges_of_kind("relates_to")?;

    let rec_supersede_rate = supersede_rate(&all_notes, &supersede_edges, window_start, window_end);
    let rec_time_to_supersede_p50 = time_to_supersede_p50(
        &supersede_edges,
        &created_at_by_id,
        window_start,
        window_end,
    );
    let rec_open_question_age_p50 =
        open_question_age_p50(&all_notes, &relates_to_edges, &kind_by_id, window_end);

    let rec_commit_coverage = match (commit_shas_in_window, anchored_commit_shas) {
        (Some(shas), Some(anchored)) => Some(commit_coverage(store, shas, anchored)?),
        _ => None,
    };

    let review_items = all_notes
        .iter()
        .filter(|n| {
            REVIEW_KINDS.contains(&n.kind.as_str())
                && in_window(n.created_at, window_start, window_end)
        })
        .count() as u64;
    let cmp_review_items_per_day = Rate::new(review_items, u64::from(window_days));

    // `rec.unresolved_conflicts` is a structural, non-windowed count, so it
    // needs the full status map even in the cheap path.
    let status_by_id: HashMap<&NoteId, &str> = all_notes
        .iter()
        .map(|n| (&n.id, n.status.as_str()))
        .collect();
    let contradicts_edges = store.edges_of_kind("contradicts")?;
    let rec_unresolved_conflicts = unresolved_conflicts(&contradicts_edges, &status_by_id);

    Ok(StatusMetricsSummary {
        window_days,
        rec_entries_in_window,
        rec_commit_coverage,
        rec_supersede_rate,
        rec_time_to_supersede_p50,
        rec_open_question_age_p50,
        cmp_review_items_per_day,
        rec_unresolved_conflicts,
        events,
    })
}

/// Compute every state metric for the window `[window_start, window_end]`
/// (both inclusive, unix seconds). `git` is `None` when `project_root` is not
/// a git repository (or has no commits yet), in which case every
/// commit-derived metric is omitted from the result rather than reported as
/// zero (ADR-098: "a project that is not a git repository gets no
/// commit-based metrics").
pub fn compute_state_metrics(
    store: &MemoryStore,
    window_start: i64,
    window_end: i64,
    window_days: u32,
    git: Option<&GitWindowFacts>,
) -> Result<StateMetrics> {
    let all_notes = store.all_notes_for_dedup()?;
    let created_at_by_id: HashMap<&NoteId, i64> =
        all_notes.iter().map(|n| (&n.id, n.created_at)).collect();
    let kind_by_id: HashMap<&NoteId, &str> =
        all_notes.iter().map(|n| (&n.id, n.kind.as_str())).collect();
    let status_by_id: HashMap<&NoteId, &str> = all_notes
        .iter()
        .map(|n| (&n.id, n.status.as_str()))
        .collect();

    let rec_entries = entry_counts(&all_notes, window_start, window_end);

    let supersede_edges = store.edges_of_kind("supersedes")?;
    let contradicts_edges = store.edges_of_kind("contradicts")?;
    let relates_to_edges = store.edges_of_kind("relates_to")?;

    let rec_supersede_rate = supersede_rate(&all_notes, &supersede_edges, window_start, window_end);
    let rec_time_to_supersede_p50 = time_to_supersede_p50(
        &supersede_edges,
        &created_at_by_id,
        window_start,
        window_end,
    );
    let rec_open_question_age_p50 =
        open_question_age_p50(&all_notes, &relates_to_edges, &kind_by_id, window_end);
    let rec_near_duplicate_rate = near_duplicate_rate(store, &all_notes)?;
    let rec_unresolved_conflicts = unresolved_conflicts(&contradicts_edges, &status_by_id);

    let decisions_in_window = all_notes
        .iter()
        .filter(|n| n.kind == "decision" && in_window(n.created_at, window_start, window_end))
        .count() as u64;

    let rec_commit_coverage = git
        .map(|g| commit_coverage(store, &g.commit_shas, &g.anchored_commit_shas))
        .transpose()?;
    let cmp_lines_per_decision =
        git.map(|g| Rate::new(g.lines_changed_in_window, decisions_in_window));

    let review_items = all_notes
        .iter()
        .filter(|n| {
            REVIEW_KINDS.contains(&n.kind.as_str())
                && in_window(n.created_at, window_start, window_end)
        })
        .count() as u64;
    let cmp_review_items_per_day = Rate::new(review_items, u64::from(window_days));

    let cmp_tokens_context_estimate = context_tokens_estimate(store)?;

    Ok(StateMetrics {
        rec_entries,
        rec_commit_coverage,
        rec_supersede_rate,
        rec_time_to_supersede_p50,
        rec_open_question_age_p50,
        rec_near_duplicate_rate,
        rec_unresolved_conflicts,
        cmp_lines_per_decision,
        cmp_review_items_per_day,
        cmp_tokens_context_estimate,
    })
}

fn in_window(ts: i64, start: i64, end: i64) -> bool {
    ts >= start && ts <= end
}

/// Whether `note` was valid (created and not yet superseded/archived-with-a-
/// timestamp) at `ts`. Mirrors the `as_of` window `MemoryStore::list_filtered`
/// already applies (`COALESCE(valid_at, created_at) <= ts AND (invalid_at IS
/// NULL OR invalid_at > ts)`), read here off an already-fetched `Note` rather
/// than re-queried, since the caller needs it as a filter over an in-memory set.
fn active_at(note: &Note, ts: i64) -> bool {
    let starts_at = note.valid_at.unwrap_or(note.created_at);
    starts_at <= ts && note.invalid_at.is_none_or(|inv| inv > ts)
}

fn entry_counts(all_notes: &[Note], window_start: i64, window_end: i64) -> EntryCounts {
    let mut total: BTreeMap<String, u64> = NOTE_KINDS.iter().map(|k| (k.to_string(), 0)).collect();
    let mut active = total.clone();
    let mut in_window_counts = total.clone();
    let mut by_origin: BTreeMap<String, u64> =
        ORIGIN_BUCKETS.iter().map(|k| (k.to_string(), 0)).collect();
    for n in all_notes {
        *total.entry(n.kind.clone()).or_insert(0) += 1;
        if n.status == "active" {
            *active.entry(n.kind.clone()).or_insert(0) += 1;
            let bucket = n
                .origin
                .as_ref()
                .map(|o| o.actor_kind.as_str())
                .unwrap_or("unknown");
            *by_origin.entry(bucket.to_string()).or_insert(0) += 1;
        }
        if in_window(n.created_at, window_start, window_end) {
            *in_window_counts.entry(n.kind.clone()).or_insert(0) += 1;
        }
    }
    EntryCounts {
        total,
        active,
        in_window: in_window_counts,
        by_origin,
    }
}

/// `rec.supersede_rate` = entries superseded in the window (any kind, via the
/// `supersedes` edge's own `created_at`) divided by active *decisions* at
/// window start. ADR-098 D3 states the numerator as "entries superseded" and
/// the denominator as "active decisions" without restricting the numerator's
/// kind to `decision` too; taken literally here.
fn supersede_rate(
    all_notes: &[Note],
    supersede_edges: &[MemoryEdge],
    window_start: i64,
    window_end: i64,
) -> Rate {
    let numerator = supersede_edges
        .iter()
        .filter(|e| in_window(e.created_at, window_start, window_end))
        .count() as u64;
    let denominator = all_notes
        .iter()
        .filter(|n| n.kind == "decision" && active_at(n, window_start))
        .count() as u64;
    Rate::new(numerator, denominator)
}

/// `rec.time_to_supersede_p50`: median of `superseder.created_at -
/// superseded.created_at` over `supersedes` edges created in the window.
/// `supersedes` edges are stored `(from_id = superseder, to_id = superseded)`
/// (see `storage::memory::edges::add_note_superseding`/`supersede`).
fn time_to_supersede_p50(
    supersede_edges: &[MemoryEdge],
    created_at_by_id: &HashMap<&NoteId, i64>,
    window_start: i64,
    window_end: i64,
) -> MedianSeconds {
    let deltas: Vec<i64> = supersede_edges
        .iter()
        .filter(|e| in_window(e.created_at, window_start, window_end))
        .filter_map(|e| {
            let superseder_at = created_at_by_id.get(&e.from_id)?;
            let superseded_at = created_at_by_id.get(&e.to_id)?;
            Some((superseder_at - superseded_at).max(0))
        })
        .collect();
    MedianSeconds {
        sample_size: deltas.len() as u64,
        median_seconds: median(deltas),
    }
}

/// `rec.open_question_age_p50`, as closely as this schema can express it.
///
/// ADR-098 defines an open question as `kind='question'` with no `answer`
/// related to it, but the schema has no edge kind dedicated to "this answers
/// that question" — only the generic `relates_to`. The closest honest
/// definition: a `question` is open unless a `relates_to` edge (either
/// direction) connects it to an entry of kind `answer`. Restricted to
/// currently-active questions, since an archived/superseded one is no longer
/// part of the open review surface.
fn open_question_age_p50(
    all_notes: &[Note],
    relates_to_edges: &[MemoryEdge],
    kind_by_id: &HashMap<&NoteId, &str>,
    window_end: i64,
) -> MedianSeconds {
    let mut answered: HashSet<&NoteId> = HashSet::new();
    for e in relates_to_edges {
        if kind_by_id.get(&e.to_id) == Some(&"answer") {
            answered.insert(&e.from_id);
        }
        if kind_by_id.get(&e.from_id) == Some(&"answer") {
            answered.insert(&e.to_id);
        }
    }
    let ages: Vec<i64> = all_notes
        .iter()
        .filter(|n| n.kind == "question" && n.status == "active" && !answered.contains(&n.id))
        .map(|n| (window_end - n.created_at).max(0))
        .collect();
    MedianSeconds {
        sample_size: ages.len() as u64,
        median_seconds: median(ages),
    }
}

/// `rec.near_duplicate_rate`: active entries having another active entry
/// within cosine distance 0.15, using only stored embeddings (no re-embed, no
/// network). Reuses [`MemoryStore::search`] — the same vector KNN path
/// `inkentry harvest`'s own dedup check already runs distances through — so
/// this shares its distance semantics rather than defining a second one.
fn near_duplicate_rate(store: &MemoryStore, all_notes: &[Note]) -> Result<NearDuplicateRate> {
    let mut excluded_without_vector = 0u64;
    let mut with_vector: Vec<(&Note, Vec<u8>)> = Vec::new();
    for n in all_notes.iter().filter(|n| n.status == "active") {
        match store.get_embedding(&n.id)? {
            Some(blob) => with_vector.push((n, blob)),
            None => excluded_without_vector += 1,
        }
    }

    let denominator = with_vector.len() as u64;
    let mut numerator = 0u64;
    for (note, blob) in &with_vector {
        // k=5 rather than 2: guards against ties at distance 0 (two notes
        // with byte-identical embeddings) still surfacing a genuine *other*
        // neighbour within the window this scans.
        let neighbors = store.search(blob, 5, None)?;
        let has_near_duplicate = neighbors.iter().any(|nb| {
            nb.id != note.id && nb.distance.unwrap_or(f64::MAX) < NEAR_DUPLICATE_THRESHOLD
        });
        if has_near_duplicate {
            numerator += 1;
        }
    }

    let value = (denominator > 0).then(|| numerator as f64 / denominator as f64);
    Ok(NearDuplicateRate {
        numerator,
        denominator,
        value,
        excluded_without_vector,
    })
}

/// `rec.unresolved_conflicts`: `contradicts` edges whose both endpoints are
/// currently active (i.e. neither has since been superseded or archived).
fn unresolved_conflicts(
    contradicts_edges: &[MemoryEdge],
    status_by_id: &HashMap<&NoteId, &str>,
) -> u64 {
    contradicts_edges
        .iter()
        .filter(|e| {
            status_by_id.get(&e.from_id) == Some(&"active")
                && status_by_id.get(&e.to_id) == Some(&"active")
        })
        .count() as u64
}

/// `rec.commit_coverage`: commits in the window with at least one entry whose
/// `source_ref` names that commit, or whose git-notes anchor does. Takes the
/// commit list and the anchor set as plain slices/sets rather than
/// [`GitWindowFacts`] so `inkentry status`'s cheap summary can share this
/// without paying for `--numstat` (which `GitWindowFacts` also carries, for
/// `cmp.lines_per_decision`).
fn commit_coverage(
    store: &MemoryStore,
    commit_shas: &[String],
    anchored: &HashSet<String>,
) -> Result<Rate> {
    let harvested_shas = store.harvested_shas()?;
    let covered = commit_shas
        .iter()
        .filter(|sha| harvested_shas.contains(sha.as_str()) || anchored.contains(sha.as_str()))
        .count() as u64;
    Ok(Rate::new(covered, commit_shas.len() as u64))
}

/// `cmp.tokens_context_estimate`: token count of the entries `inkentry
/// context`'s default (no `--kind`, no `--limit`, no `--budget`) view would
/// print, using the existing chars/4 estimator
/// (`crate::search::tokens::estimate_tokens`) inkentry-cli's own `--budget`
/// packing already uses for the same notes. Named `_estimate` because that
/// heuristic is exactly what it is, not an exact tokenizer count. Omits the
/// cross-project dependency pass and the conventions section that porcelain
/// `context` also prints: both would pull in another project's index or the
/// local code index, which a memory-only snapshot has no business touching.
fn context_tokens_estimate(store: &MemoryStore) -> Result<u64> {
    let mut tokens = 0u64;
    for (kind, limit) in CONTEXT_SECTIONS {
        let notes = store.list(Some(*kind), *limit, false)?;
        for n in &notes {
            tokens += crate::search::tokens::estimate_tokens(&n.title) as u64;
            tokens += crate::search::tokens::estimate_tokens(&n.body) as u64;
        }
    }
    Ok(tokens)
}

/// Median of a set of non-negative durations. `None` on an empty input; for
/// an even-sized input this averages the two middle values (integer
/// division, which is exact for the odd case and merely rounds down by at
/// most half a second for the even one).
pub(super) fn median(mut values: Vec<i64>) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let n = values.len();
    Some(if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_is_none_on_zero_denominator_never_a_fabricated_zero() {
        let r = Rate::new(0, 0);
        assert_eq!(r.numerator, 0);
        assert_eq!(r.denominator, 0);
        assert_eq!(r.value, None);
    }

    #[test]
    fn rate_divides_numerator_by_denominator() {
        let r = Rate::new(3, 4);
        assert_eq!(r.value, Some(0.75));
    }

    #[test]
    fn median_of_empty_is_none() {
        assert_eq!(median(vec![]), None);
    }

    #[test]
    fn median_of_odd_length_is_the_middle_value() {
        assert_eq!(median(vec![5, 1, 3]), Some(3));
    }

    #[test]
    fn median_of_even_length_averages_the_two_middle_values() {
        assert_eq!(median(vec![10, 20, 30, 40]), Some(25));
    }
}
