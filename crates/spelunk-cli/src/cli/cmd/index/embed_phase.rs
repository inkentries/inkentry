use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Serialize;

use super::super::ui::is_tty;
use crate::{
    capability::{ServerLimits, Tier},
    config::Config,
    storage::Database,
};

/// Hard ceiling on chunks per request — the server enforces 256 (returns 413 if
/// exceeded), so both the user-supplied `--batch-size` and the calibrated batch
/// size are clamped to this. The server's candle embedder (F2LLM-v2-330M) runs
/// inference in padded sub-batches of EMBED_BATCH_SIZE=8, so e.g. 64 chunks = 8
/// forward passes.
const MAX_BATCH: usize = 256;

/// Default ceiling on the calibrated batch size when the user has not set
/// `--batch-size` (i.e. it is still 0): the server's own hard limit, so
/// calibration is free to grow the batch as large as the measured throughput
/// justifies. `--batch-size` exists to let a user *lower* this ceiling (e.g.
/// memory-constrained hardware), not to hand-pick a fixed size (see
/// `resolve_batch_ceiling`).
const DEFAULT_BATCH_CEILING: usize = MAX_BATCH;

/// Size of the very first request: a single chunk. We deliberately do not
/// guess a batch size before we have ANY timing data — a full batch's cold
/// start (model load, first-request JIT, etc.) used to be exactly what made
/// the old fixed-timeout approach unsafe on slow hardware. A batch of 1 gives
/// an initial per-entry estimate almost immediately, and also makes the
/// progress bar move right away (spelunk-oss^73) instead of only after a full
/// batch round-trips.
const CALIBRATION_BATCH_1: usize = 1;

/// Size of the second request, used to refine the estimate from
/// `CALIBRATION_BATCH_1` (which is dominated by one-off cold-start costs: model
/// warm-up, first-connection overhead, etc.) before committing to a steady-
/// state batch size. Only run when enough chunks remain (see
/// `next_batch_size` / calibration bookkeeping in `run_embed_phase`).
const CALIBRATION_BATCH_2: usize = 4;

/// Target wall-clock time we aim to keep each *steady-state* (post-calibration)
/// batch under. Batch size is derived by dividing this budget by the measured
/// per-entry rate: slow hardware (e.g. ~60 s/entry) gets a small batch (~4), fast
/// hardware (e.g. ~1 s/entry) gets a large one (up to `MAX_BATCH`), per the
/// founder's calibration approach for spelunk-oss^74/^71 — we time a couple of
/// small batches up front and size subsequent ones from the observed rate,
/// rather than always sending a fixed-size batch and reacting after the fact.
const TARGET_BATCH_SECONDS: u64 = 240;

/// Floor for a calibrated per-request timeout. Even on very fast hardware we
/// never drop below this, to absorb transient latency spikes (a slow DNS
/// lookup, a GC pause on the server, etc.).
const MIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Ceiling for a calibrated per-request timeout, so a pathologically slow
/// sample can't derive an effectively unbounded deadline.
const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(1800);

/// Safety multiple applied to a batch's *expected* duration (at the current
/// measured per-entry rate) when deriving that batch's request timeout: give
/// it this much headroom over the estimate so normal jitter never trips it.
const TIMEOUT_SAFETY_FACTOR: u32 = 4;

/// Resolve the effective ceiling the calibrated batch size may grow to, from
/// the user-supplied `--batch-size` flag: 0 falls back to the default ceiling,
/// and anything above the server ceiling is clamped to `MAX_BATCH`. Unlike the
/// old fixed batch size, this is now only an upper bound — the actual
/// per-request size is calibrated from measured throughput and may end up
/// smaller (see `next_batch_size`).
///
/// `server_max_batch_chunks` (from `ServerLimits::max_batch_chunks`, when the
/// server advertises it) additionally clamps the ceiling — a client should
/// never plan around a batch count the specific server it's talking to won't
/// even accept (`413`), independent of the client's own `MAX_BATCH` guess.
fn resolve_batch_ceiling(requested: usize, server_max_batch_chunks: Option<usize>) -> usize {
    let ceiling = if requested == 0 {
        DEFAULT_BATCH_CEILING
    } else {
        requested.min(MAX_BATCH)
    };
    match server_max_batch_chunks {
        Some(server_max) => ceiling.min(server_max),
        None => ceiling,
    }
}

/// Legacy per-request budget assumed when talking to a server that pre-dates
/// the `/v1/health` `limits` field (spelunk-oss^71/^73/^74) — i.e. the old
/// blanket `TimeoutLayer` budget with no `/index/embed` exemption. This is
/// the exact version-skew case that produced the field failure this fix
/// addresses: a CLI calibrating toward `TARGET_BATCH_SECONDS` (240s) talking
/// to an old server that kills every request at 30s regardless.
const LEGACY_SERVER_REQUEST_BUDGET_SECS: u64 = 30;

/// Fraction of the server's advertised (or assumed-legacy) per-request budget
/// that a calibrated batch should target, leaving headroom for jitter/variance
/// between the calibration sample and the batch actually sent — the server
/// will 408 at the hard budget regardless of how well-intentioned the
/// client's estimate was.
const SERVER_BUDGET_TARGET_FRACTION: f64 = 2.0 / 3.0;

/// Resolve the effective target batch duration (seconds), clamped to fit
/// comfortably inside the server's own advertised `/index/embed` request
/// budget when known — this REPLACES the fixed `TARGET_BATCH_SECONDS` as the
/// primary mechanism for staying under the server's actual budget (the
/// 408-triggered shrink in `run_embed_phase` is the fallback, for a server
/// that misreports or whose effective budget changes under load).
///
/// - Server advertises `limits` (current build, carries the
///   `/index/embed`-specific exemption from this same fix): target is
///   `TARGET_BATCH_SECONDS`, clamped down to `SERVER_BUDGET_TARGET_FRACTION ×
///   embed_request_timeout_secs` if that's smaller (a self-hosted deployment
///   could in principle configure a smaller budget than the CLI's default
///   target).
/// - Server does NOT advertise `limits` (pre-dates this fix, still enforces
///   the old blanket 30s budget with no exemption): target is
///   `SERVER_BUDGET_TARGET_FRACTION × LEGACY_SERVER_REQUEST_BUDGET_SECS` —
///   the version-skew case (instruction 5 / founder directive): a new CLI
///   talking to an old, long-running server must still make progress, just
///   with smaller batches and (at the call site) a warning.
fn resolve_target_batch_seconds(server_limits: Option<ServerLimits>) -> u64 {
    let budget_secs = server_limits
        .map(|l| l.embed_request_timeout_secs)
        .unwrap_or(LEGACY_SERVER_REQUEST_BUDGET_SECS);
    let safe_budget = (budget_secs as f64 * SERVER_BUDGET_TARGET_FRACTION).floor() as u64;
    TARGET_BATCH_SECONDS.min(safe_budget.max(1))
}

/// Max multiple of the *previous* batch's size that the next calibrated batch
/// is allowed to grow to in one step (spelunk-oss^71/^73/^74 field-failure
/// follow-up, PR #513 review). Without this, a single fast sample right after
/// a slow one could derive a batch many times larger than anything actually
/// measured — e.g. observed in the field: calibration batch 2 (4 chunks)
/// landing unusually fast produced a *raw* per-entry rate that alone implied a
/// batch of 200, an ~50x jump from the 4-chunk sample that produced it, with
/// no batch of intermediate size ever having been measured. Capping growth to
/// `GROWTH_FACTOR`x per step means the size ramps up over a few batches
/// instead of leaping straight to a value nothing has verified is safe.
const GROWTH_FACTOR: usize = 8;

/// Choose the next steady-state batch size from the measured per-entry
/// duration, so a batch takes roughly `target_seconds` wall-clock: slow
/// hardware (large `per_entry`) gets a small batch, fast hardware gets a large
/// one. Clamped to `[1, ceiling]` — `ceiling` is the smaller of `MAX_BATCH` (the
/// server's hard 413 limit), the user's `--batch-size` (if they set one), and
/// the server's own advertised `max_batch_chunks` (if known) — and
/// additionally clamped to at most `GROWTH_FACTOR × previous_batch_size`,
/// so one sample can never produce a many-times-larger leap in a single step
/// (see `GROWTH_FACTOR`).
///
/// `target_seconds` is normally `TARGET_BATCH_SECONDS`, but is clamped down by
/// `resolve_target_batch_seconds` to fit the server's advertised (or
/// assumed-legacy) `/index/embed` request budget — see that function.
///
/// Examples from the calibration this replaces the old "time a fixed 64-batch"
/// approach with: ~60 s/entry ⇒ batch 4; ~1 s/entry ⇒ batch up to
/// `GROWTH_FACTOR × previous_batch_size` (not necessarily the full 256 ceiling
/// — growth is capped per step, see `GROWTH_FACTOR`).
fn next_batch_size(
    per_entry: Duration,
    ceiling: usize,
    previous_batch_size: usize,
    target_seconds: u64,
) -> usize {
    let growth_cap = previous_batch_size
        .max(1)
        .saturating_mul(GROWTH_FACTOR)
        .min(ceiling.max(1));

    if per_entry.is_zero() {
        return growth_cap;
    }
    let target = Duration::from_secs(target_seconds).as_secs_f64();
    let size = (target / per_entry.as_secs_f64()).round();
    if size < 1.0 {
        1
    } else if size >= growth_cap as f64 {
        growth_cap
    } else {
        size as usize
    }
}

/// Derive the per-request timeout for a batch of `batch_size` entries from the
/// current measured per-entry rate: `TIMEOUT_SAFETY_FACTOR ×` the batch's
/// expected duration at that rate, clamped into
/// `[MIN_REQUEST_TIMEOUT, MAX_REQUEST_TIMEOUT]`. Because batch size itself is
/// derived from the same rate to target `TARGET_BATCH_SECONDS`, this timeout
/// tracks the batch size rather than a single fixed deadline — a genuinely
/// slow-but-progressing request is not killed mid-flight and its whole batch
/// discarded (spelunk-oss^71).
fn batch_timeout(per_entry: Duration, batch_size: usize) -> Duration {
    let expected = per_entry.saturating_mul(batch_size.max(1) as u32);
    let budget = expected.saturating_mul(TIMEOUT_SAFETY_FACTOR);
    budget.clamp(MIN_REQUEST_TIMEOUT, MAX_REQUEST_TIMEOUT)
}

/// Timeout used for the very first request (a single chunk), before we have
/// any measurement of this machine's embedding speed at all. Pessimistic
/// because it must also absorb one-off model cold-start.
const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Blend weight given to `CALIBRATION_BATCH_1`'s sample (a single chunk) the
/// moment a second, larger sample arrives. Deliberately small: batch 1 exists
/// only to get the progress bar moving and produce *some* number before batch
/// 2 lands (spelunk-oss^73) — its 1-entry sample is dominated by one-off
/// per-request overhead (connection setup, first-request scheduling, and on
/// some hardware genuine model warm-up) that does not repeat on every
/// subsequent batch, so it must not carry the same weight as a real,
/// multi-entry measurement. A founder field report (2026-07-05, PR #513)
/// showed a 50/50 blend of a cold ~25s batch-1 sample with a warm ~1.2s/entry
/// batch-2 sample producing a ~13s/entry *displayed* rate while the batch-size
/// decision (see `next_batch_size`) used a *different*, unblended sample —
/// two disagreeing estimates from what should be one authoritative number.
/// De-weighting the batch-1 sample here (instead of a straight 50/50) means
/// the blended estimate — the single source both the batch-size/timeout
/// decision AND the displayed rate now read from — converges to the real,
/// measured rate almost immediately rather than staying skewed by a sample
/// that was never representative of steady state.
const CALIBRATION_BATCH_1_WEIGHT: f64 = 0.1;

/// Running estimate of this run's embedding throughput, refined after every
/// batch so a rate that drifts mid-run (e.g. thermal throttling, another
/// process contending for the GPU) is picked up rather than locked in from the
/// first sample alone.
///
/// This is the SINGLE authoritative rate source: `next_batch_size`,
/// `batch_timeout`, and the progress-bar status message all read
/// `per_entry()` from the same `RateEstimate` instance, so they can never
/// disagree the way the batch-size decision and the *displayed* rate
/// diverged in the field (see `CALIBRATION_BATCH_1_WEIGHT`). `indicatif`'s own
/// `{eta}` template token still runs its own internal estimator (see
/// `embed_progress_style`) — that is display-only smoothing for the progress
/// bar widget itself and is unrelated to any decision this struct feeds.
struct RateEstimate {
    /// Exponentially-weighted per-entry duration. `None` until the first batch
    /// lands.
    per_entry: Option<Duration>,
    /// Number of batches folded in so far, so `update` can tell "this is the
    /// batch-1 cold sample being immediately superseded" (samples_seen == 1)
    /// apart from every later, steady-state blend (50/50, same as before).
    samples_seen: u32,
}

impl RateEstimate {
    fn new() -> Self {
        Self {
            per_entry: None,
            samples_seen: 0,
        }
    }

    /// Fold in a newly-observed batch: `elapsed` wall-clock time for
    /// `entries` chunks. The first observation seeds the estimate outright;
    /// the second observation (superseding the batch-1 cold sample) blends
    /// with only `CALIBRATION_BATCH_1_WEIGHT` given to that first sample
    /// instead of an even split, since it is dominated by one-off overhead
    /// that isn't representative of steady state (see
    /// `CALIBRATION_BATCH_1_WEIGHT`). From the third observation onward,
    /// later samples are blended 50/50 with the running estimate — recent
    /// batches matter more than stale ones, so a hardware/network rate change
    /// mid-run (thermal throttling, contention) is reflected within a couple
    /// of batches instead of being permanently anchored to an early sample.
    fn update(&mut self, elapsed: Duration, entries: usize) {
        if entries == 0 {
            return;
        }
        let sample = elapsed.div_f64(entries as f64);
        self.per_entry = Some(match self.per_entry {
            None => sample,
            Some(prev) if self.samples_seen == 1 => {
                // Superseding the batch-1 cold sample: give it only
                // `CALIBRATION_BATCH_1_WEIGHT` instead of 50/50.
                let w = CALIBRATION_BATCH_1_WEIGHT;
                let blended = prev.as_secs_f64() * w + sample.as_secs_f64() * (1.0 - w);
                Duration::from_secs_f64(blended)
            }
            Some(prev) => {
                // Steady-state blend: a simple 50/50 exponential moving
                // average.
                let blended = (prev.as_secs_f64() + sample.as_secs_f64()) / 2.0;
                Duration::from_secs_f64(blended)
            }
        });
        self.samples_seen += 1;
    }

    /// Current best estimate, or `None` before the first batch has landed.
    fn per_entry(&self) -> Option<Duration> {
        self.per_entry
    }
}

/// Progress style for the embed phase, including indicatif's built-in `{eta}`
/// token. The ETA is driven by `bar.inc(1)` per embedded chunk and uses
/// indicatif's double-exponentially-smoothed rate estimator; starting
/// calibration with a batch of 1 (rather than a full batch) means the first
/// data point lands almost immediately, so the ETA and the bar itself become
/// visible right away instead of only after a full batch completes
/// (spelunk-oss^73/^74). A steady tick keeps the spinner and ETA moving even
/// while a request is in flight, so a slow batch never looks frozen.
fn embed_progress_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.cyan} Embedding [{bar:38.cyan/blue}] {pos}/{len}  ETA {eta}  {wide_msg}",
    )
    .unwrap()
    .progress_chars("=>-")
}

#[derive(Serialize)]
struct EmbedRequest {
    chunks: Vec<ReqChunk>,
}

#[derive(Serialize)]
struct ReqChunk {
    chunk_id: String,
    content: String,
}

/// Report an unrecoverable embed-phase failure: abandon the progress bar with
/// a summary, and print an actionable message to stderr. Does NOT return an
/// `Err` — callers report the count embedded so far via `Ok(embedded)` (see
/// `run_embed_phase`'s doc comment on why an `Err` here would be wrong).
///
/// The message names the server request budget and a possible server-version
/// mismatch as likely causes when the error text itself doesn't already make
/// that obvious (a 408/timeout error already says "request budget" — see
/// `embed_one_batch`'s `EmbedBatchError::BudgetExceeded` construction — but a
/// user may not connect that to "maybe my server is out of date", especially
/// since a modern server importing this same fix should never actually reach
/// this path for a reasonably-sized batch).
fn report_embed_failure(
    bar: &ProgressBar,
    embedded: u64,
    total: u64,
    server_url: &str,
    err: anyhow::Error,
) {
    bar.abandon_with_message(format!(
        "batch failed after {embedded}/{total} embedded; re-run `spelunk index` to finish the rest",
    ));
    eprintln!("Embedding stopped after {embedded}/{total} chunks embedded and saved: {err:#}");
    eprintln!(
        "Re-run `spelunk index` to embed the remaining {} chunk(s); already-embedded chunks \
         are skipped.",
        total - embedded,
    );
    eprintln!(
        "If this keeps happening: the spelunk-server at {server_url} may be enforcing a \
         smaller request budget than this batch needs, or may be running an older build \
         that predates the long-running-embed fix (upgrading the server, or running \
         `spelunk server stop && spelunk server start` to pick up a newer build, may help)."
    );
}

/// Send pending chunks to `spelunk-server` for embedding and write the returned
/// vectors into the local DB.
///
/// Returns the number of chunks successfully embedded.
///
/// Requires `Tier::Server`; returns `Ok(0)` immediately for `Tier::Offline`.
pub(super) async fn run_embed_phase(
    chunk_ids_and_texts: Vec<(i64, String)>,
    db: &Database,
    cfg: &Config,
    tier: &Tier,
    project_root: &std::path::Path,
    batch_size: usize,
    mp: &MultiProgress,
) -> Result<u64> {
    let (server_url, server_key) = match tier {
        Tier::Server { url, .. } => (url.clone(), cfg.server_key.clone()),
        Tier::Offline => return Ok(0),
    };
    let server_limits = tier.server_limits();

    // Ceiling the calibrated batch size may grow to. Unlike the old scheme,
    // this is no longer the size we always send — see `next_batch_size`. Also
    // clamped to what the server itself advertises (`max_batch_chunks`), when
    // known, so the client never plans around a batch count the specific
    // server it's talking to won't accept.
    let server_max_batch_chunks = server_limits.map(|l| l.max_batch_chunks);
    let ceiling = resolve_batch_ceiling(batch_size, server_max_batch_chunks);

    // Target batch duration, clamped to fit the server's advertised (or
    // assumed-legacy) `/index/embed` request budget — see
    // `resolve_target_batch_seconds`. This is the PRIMARY mechanism for
    // staying under the server's budget; the steady-state 408-triggered
    // shrink below is the fallback for a server that misreports or changes
    // its effective budget under load (spelunk-oss^71/^73/^74).
    let target_batch_seconds = resolve_target_batch_seconds(server_limits);
    if server_limits.is_none() {
        // Version-skew notice: this server pre-dates the `/v1/health`
        // `limits` field, so it may still enforce the old blanket 30s
        // `/index/embed` budget with no exemption (the exact field failure
        // this fix addresses). Calibrating toward a smaller target keeps the
        // run working — just with smaller, more frequent batches — instead of
        // repeatedly hitting 408 against an old server.
        eprintln!(
            "Note: spelunk-server at {server_url} did not report its /index/embed request \
             budget (older server build) — assuming a conservative {LEGACY_SERVER_REQUEST_BUDGET_SECS}s \
             legacy budget and targeting smaller batches accordingly. If this looks slower than \
             expected, consider upgrading the server."
        );
    }

    // Use `resolve_project_id` so that loopback auto-discovered servers (where
    // `cfg.project_id` may be absent) derive the id from the project root path,
    // matching `Config::resolve_project_id` behaviour (see spelunk#307).
    let project_id_owned = cfg.resolve_project_id(project_root);
    let project_id = project_id_owned.as_str();

    // No client-wide timeout: we apply a PER-REQUEST timeout below, derived
    // from the measured rate once we have one (starting pessimistic for the
    // very first, single-chunk request). A single fixed client deadline is
    // what let a slow first batch expire with nothing persisted
    // (spelunk-oss^71).
    let client = reqwest::Client::builder()
        .build()
        .context("building HTTP client for embed phase")?;

    let total = chunk_ids_and_texts.len() as u64;
    let bar = if is_tty() && !crate::utils::is_agent_mode() {
        let b = mp.add(ProgressBar::new(total));
        b.set_style(embed_progress_style());
        b
    } else {
        ProgressBar::hidden()
    };

    // Draw the bar immediately, before the first (small, fast) request even
    // fires, so the embedding phase shows visible movement within ~1 s of
    // parsing finishing instead of only after a full batch round-trip returns
    // (spelunk-oss^73). The steady tick animates the spinner while a request
    // is in flight so a single slow batch never looks frozen.
    bar.set_message("calibrating batch size\u{2026}");
    bar.enable_steady_tick(std::time::Duration::from_millis(120));
    bar.tick();

    let mut rate = RateEstimate::new();
    let mut embedded = 0u64;
    let mut cursor = 0usize;
    let mut batch_num = 0u64;
    let mut previous_batch_size = 1usize;
    let remaining = chunk_ids_and_texts.len();
    // Percent-encode the project_id path segment: slugs contain `/`
    // (`local/<hex>`, `github.com/owner/repo`) which would otherwise split
    // the segment and break axum routing → 404. See spelunk decision #106.
    let url = format!(
        "{}/v1/projects/{}/index/embed",
        server_url.trim_end_matches('/'),
        crate::server_client::encode_project_id(project_id),
    );

    while cursor < remaining {
        batch_num += 1;
        let left = remaining - cursor;

        // Calibration phase: the first request is a single chunk, the second
        // is a small 4-chunk sample (both clamped to what's actually left, for
        // small indexes). Both exist purely to get real timing data before
        // committing to a steady-state batch size, per the founder's feedback
        // on spelunk-oss^71/^74 — we no longer time a full default-sized batch
        // and adapt afterward.
        let mut this_batch_size = match batch_num {
            1 => CALIBRATION_BATCH_1,
            2 => CALIBRATION_BATCH_2,
            _ => {
                let per_entry = rate
                    .per_entry()
                    .expect("rate is seeded after the first batch completes");
                next_batch_size(
                    per_entry,
                    ceiling,
                    previous_batch_size,
                    target_batch_seconds,
                )
            }
        }
        .clamp(1, left);

        // Retry loop for THIS batch: a 408/timeout ("budget exceeded") is
        // treated as recoverable — either escalate patience (calibration
        // batch 1 only, where we have no rate estimate yet to size a smaller
        // request from) or shrink the batch and retry, rather than aborting
        // the whole run at 0 embedded (spelunk-oss^71/^73/^74 field-failure
        // follow-up). Any other failure (network error, 5xx, malformed
        // response) still aborts immediately as before — shrinking/retrying
        // is specifically a response to "the request was too big/slow for
        // the budget", not a generic retry-everything policy.
        let mut escalated_calibration_once = false;
        let bytes = 'retry: loop {
            let request_timeout = match rate.per_entry() {
                Some(per_entry) => batch_timeout(per_entry, this_batch_size),
                None if escalated_calibration_once => MAX_REQUEST_TIMEOUT,
                None => FIRST_REQUEST_TIMEOUT,
            };

            // Show which chunks are in flight so a single slow request reads as
            // progress against a known window, not a frozen bar (spelunk-oss^73).
            bar.set_message(format!(
                "sent {this_batch_size} chunk(s) ({embedded}/{total} done so far), \
                 awaiting response\u{2026}",
            ));

            let batch = &chunk_ids_and_texts[cursor..cursor + this_batch_size];

            let req_chunks: Vec<ReqChunk> = batch
                .iter()
                .map(|(id, text)| ReqChunk {
                    chunk_id: id.to_string(),
                    content: text.clone(),
                })
                .collect();

            let started = Instant::now();
            let outcome = embed_one_batch(
                &client,
                &url,
                server_key.as_deref(),
                EmbedRequest { chunks: req_chunks },
                batch.len(),
                request_timeout,
            )
            .await;

            match outcome {
                Ok(bytes) => {
                    // Fold this batch's measured rate in and re-estimate:
                    // later batches' sizes (and timeouts) track the current
                    // rate rather than being fixed from a single early
                    // sample, so a rate that drifts mid-run is picked up
                    // within a couple of batches (spelunk-oss^71/^74). The
                    // same per-chunk `bar.inc(1)` cadence below also feeds the
                    // visible `{eta}`, which indicatif smooths across batches
                    // so the tiny first calibration batch's cold-start skew
                    // washes out quickly (spelunk-oss^74).
                    rate.update(started.elapsed(), batch.len());
                    break 'retry bytes;
                }
                Err(EmbedBatchError::BudgetExceeded(e)) if this_batch_size == 1 => {
                    // Can't shrink below 1 chunk. For calibration batch 1
                    // specifically (no rate estimate yet), escalate patience
                    // once (FIRST_REQUEST_TIMEOUT → MAX_REQUEST_TIMEOUT) —
                    // this is the "retry the calibration batch once with
                    // escalated patience" behaviour: a single chunk that took
                    // >FIRST_REQUEST_TIMEOUT might still legitimately finish
                    // given the full 1800s ceiling (e.g. genuine cold-start
                    // cost on very slow hardware), and giving up at 0/total
                    // embedded on the very first request is the worst
                    // possible failure mode. Only escalate once; if it still
                    // fails at the max budget, this really is unrecoverable
                    // (or a persistently mis-configured/underpowered server)
                    // and we must stop.
                    if !escalated_calibration_once && rate.per_entry().is_none() {
                        escalated_calibration_once = true;
                        eprintln!(
                            "First embed request timed out (server request budget \
                             may be smaller than expected, or this server predates \
                             the long-running-embed fix) — retrying with more patience\u{2026}"
                        );
                        continue 'retry;
                    }
                    // A batch failing (even after the calibration escalation
                    // above) must NOT abort the whole run with an `Err`:
                    // embeddings already committed from prior batches stay in
                    // the DB, and a re-run picks up the rest via the
                    // missing-embedding backfill (spelunk-oss^72). Report and
                    // return the count embedded so far — propagating an `Err`
                    // here would unwind the command before `stats()` runs and
                    // discard the visible progress (spelunk-oss^71).
                    report_embed_failure(&bar, embedded, total, &server_url, e);
                    return Ok(embedded);
                }
                Err(EmbedBatchError::BudgetExceeded(e)) => {
                    // Steady-state (or post-calibration) batch exceeded the
                    // server's request budget: treat this as "the server's
                    // effective budget is smaller than this batch", not as a
                    // fatal error — shrink and retry rather than discarding
                    // all subsequent progress. Halve (floor 1) and also fold
                    // a pessimistic sample into the rate estimate so
                    // subsequent `next_batch_size` calls don't immediately
                    // re-derive the same too-large size.
                    let shrunk = (this_batch_size / 2).max(1);
                    if shrunk == this_batch_size {
                        // Already at the floor and still failing — no smaller
                        // batch to try; this is the batch-of-1 branch above,
                        // so unreachable in practice, but guards against an
                        // infinite loop if reached some other way.
                        report_embed_failure(&bar, embedded, total, &server_url, e);
                        return Ok(embedded);
                    }
                    tracing::warn!(
                        "index/embed batch of {this_batch_size} chunks exceeded the server's \
                         request budget (408) — shrinking to {shrunk} chunk(s) and retrying: {e:#}",
                    );
                    // A 408 tells us this batch's *expected* duration exceeded
                    // the server's budget: fold in a pessimistic per-entry
                    // sample (the request_timeout that just failed, spread
                    // over the batch) so the rate estimate reflects "at least
                    // this slow", pulling future `next_batch_size` calls down
                    // rather than immediately re-deriving the same too-large
                    // batch from a stale, too-optimistic estimate.
                    rate.update(request_timeout, this_batch_size);
                    this_batch_size = shrunk;
                    continue 'retry;
                }
                Err(EmbedBatchError::Other(e)) => {
                    // A batch failing for any other reason (connection reset,
                    // non-408 non-2xx, malformed response, …) must NOT abort
                    // the whole run: embeddings already committed from prior
                    // batches stay in the DB, and a re-run picks up the rest
                    // via the missing-embedding backfill (spelunk-oss^72).
                    // Report and stop rather than propagating an Err that
                    // would unwind the command before `stats()` and discard
                    // the visible progress (spelunk-oss^71).
                    report_embed_failure(&bar, embedded, total, &server_url, e);
                    return Ok(embedded);
                }
            }
        };

        let dim = spelunk_core::embeddings::EMBEDDING_DIM;
        let stride = dim * 4;
        let batch = &chunk_ids_and_texts[cursor..cursor + this_batch_size];

        for (i, (row_id, _text)) in batch.iter().enumerate() {
            let vector =
                spelunk_core::embeddings::blob_to_vec(&bytes[i * stride..(i + 1) * stride]);
            db.insert_embedding(*row_id, &vector)?;
            embedded += 1;
            bar.inc(1);
        }

        previous_batch_size = this_batch_size;
        cursor += this_batch_size;
    }

    bar.finish_with_message(format!("{embedded} chunks embedded"));
    Ok(embedded)
}

/// An `embed_one_batch` failure, distinguishing "the request budget was too
/// small for this batch" (HTTP 408 from the server's `TimeoutLayer`, or a
/// client-side `reqwest` timeout expiring first) from every other failure
/// (connection refused, 5xx, malformed response, …). The distinction matters:
/// a 408/timeout is actionable and often recoverable by shrinking the batch
/// and/or escalating patience and retrying (see `run_embed_phase`'s
/// calibration retry and steady-state shrink-on-408 handling); other failures
/// are not something retrying the same batch smaller is expected to fix.
enum EmbedBatchError {
    /// Server returned `408 Request Timeout`, or the client-side `timeout`
    /// passed to this call elapsed first (`reqwest::Error::is_timeout()`).
    BudgetExceeded(anyhow::Error),
    /// Any other failure (network error, non-408 non-2xx status, malformed
    /// response body, …).
    Other(anyhow::Error),
}

/// Send one embed batch and return the raw little-endian f32 response bytes:
/// one `EMBEDDING_DIM`-float vector per request chunk, in request order.
///
/// Applies a per-request `timeout` (see `batch_timeout`) and validates the
/// response length before returning, so callers get a single fallible unit they
/// can treat as all-or-nothing for that batch (spelunk-oss^71). Distinguishes
/// a 408/timeout failure from every other kind — see [`EmbedBatchError`].
async fn embed_one_batch(
    client: &reqwest::Client,
    url: &str,
    server_key: Option<&str>,
    body: EmbedRequest,
    batch_len: usize,
    timeout: Duration,
) -> Result<Vec<u8>, EmbedBatchError> {
    let mut req = client.post(url).timeout(timeout).json(&body);
    if let Some(k) = server_key {
        req = req.bearer_auth(k);
    }

    let send_result = req.send().await;
    let resp = match send_result {
        Ok(resp) => resp,
        Err(e) if e.is_timeout() => {
            return Err(EmbedBatchError::BudgetExceeded(
                anyhow::Error::new(e).context(format!(
                    "calling {url} (client-side timeout of {timeout:?} elapsed)"
                )),
            ));
        }
        Err(e) => {
            return Err(EmbedBatchError::Other(
                anyhow::Error::new(e).context(format!("calling {url}")),
            ));
        }
    };

    if resp.status() == reqwest::StatusCode::REQUEST_TIMEOUT {
        return Err(EmbedBatchError::BudgetExceeded(anyhow::anyhow!(
            "server returned 408 Request Timeout for index/embed \
             (batch of {batch_len} chunk(s) exceeded the server's request budget)"
        )));
    }

    let resp = match resp.error_for_status() {
        Ok(resp) => resp,
        Err(e) => {
            return Err(EmbedBatchError::Other(
                anyhow::Error::new(e).context("server returned an error for index/embed"),
            ));
        }
    };

    let bytes = resp
        .bytes()
        .await
        .context("reading index/embed response")
        .map_err(EmbedBatchError::Other)?;

    let dim = spelunk_core::embeddings::EMBEDDING_DIM;
    let stride = dim * 4;
    let expected = batch_len * stride;
    if bytes.len() != expected {
        return Err(EmbedBatchError::Other(anyhow::anyhow!(
            "index/embed returned {} bytes, expected {expected} ({batch_len} × {dim}-dim f32)",
            bytes.len(),
        )));
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_batch_ceiling_passes_through_valid_values() {
        // A user-supplied value within range is used verbatim as the ceiling
        // that calibration may grow the batch size up to.
        assert_eq!(resolve_batch_ceiling(1, None), 1);
        assert_eq!(resolve_batch_ceiling(32, None), 32);
        assert_eq!(resolve_batch_ceiling(64, None), 64);
        assert_eq!(resolve_batch_ceiling(200, None), 200);
        assert_eq!(resolve_batch_ceiling(MAX_BATCH, None), MAX_BATCH);
    }

    #[test]
    fn resolve_batch_ceiling_falls_back_to_default_for_zero() {
        // 0 means the user left `--batch-size` at its default: the ceiling is
        // the server's own hard limit, not some fixed pre-calibration size.
        assert_eq!(resolve_batch_ceiling(0, None), DEFAULT_BATCH_CEILING);
        assert_eq!(DEFAULT_BATCH_CEILING, MAX_BATCH);
    }

    #[test]
    fn resolve_batch_ceiling_clamps_above_server_ceiling() {
        assert_eq!(resolve_batch_ceiling(MAX_BATCH + 1, None), MAX_BATCH);
        assert_eq!(resolve_batch_ceiling(10_000, None), MAX_BATCH);
    }

    #[test]
    fn resolve_batch_ceiling_clamps_to_server_advertised_max() {
        // A server that advertises a smaller max_batch_chunks than our own
        // MAX_BATCH guess must win — the client should never plan around a
        // batch count the specific server it's talking to won't accept
        // (spelunk-oss^71/^73/^74, founder directive on server-limits surface).
        assert_eq!(resolve_batch_ceiling(0, Some(32)), 32);
        assert_eq!(resolve_batch_ceiling(200, Some(32)), 32);
        // A server-advertised max ABOVE the user's/default ceiling doesn't
        // raise it — it's a min(), not a replacement.
        assert_eq!(resolve_batch_ceiling(16, Some(256)), 16);
    }

    // ── resolve_target_batch_seconds: server-limits-aware target clamping ───
    // (spelunk-oss^71/^73/^74, founder directive on version-skew handling)

    #[test]
    fn resolve_target_batch_seconds_uses_default_when_server_budget_is_generous() {
        // A server advertising the new EMBED_REQUEST_TIMEOUT (1800s) budget
        // comfortably fits the default 240s target — no clamping needed.
        let limits = ServerLimits {
            embed_request_timeout_secs: 1800,
            max_batch_chunks: 256,
            embedder_token_cap: None,
        };
        assert_eq!(
            resolve_target_batch_seconds(Some(limits)),
            TARGET_BATCH_SECONDS
        );
    }

    #[test]
    fn resolve_target_batch_seconds_clamps_down_for_small_server_budget() {
        // A server advertising a smaller budget than the default target
        // forces a smaller target, at SERVER_BUDGET_TARGET_FRACTION of that
        // budget (leaving headroom rather than targeting the hard edge).
        let limits = ServerLimits {
            embed_request_timeout_secs: 60,
            max_batch_chunks: 256,
            embedder_token_cap: None,
        };
        assert_eq!(resolve_target_batch_seconds(Some(limits)), 40); // 60 * 2/3
    }

    #[test]
    fn resolve_target_batch_seconds_assumes_legacy_budget_when_server_limits_absent() {
        // THE version-skew case: a server that pre-dates the `limits` field
        // still enforces the old blanket 30s budget with no /index/embed
        // exemption. Absent `limits` must NOT be read as "no limit" — it must
        // fall back to the conservative legacy assumption.
        assert_eq!(
            resolve_target_batch_seconds(None),
            20 // 30 * 2/3, floored
        );
    }

    // ── next_batch_size: calibration-driven batch sizing ────────────────────
    // (spelunk-oss^71/^74, founder review on PR #513)

    #[test]
    fn next_batch_size_shrinks_for_slow_hardware() {
        // ~60 s/entry ⇒ a 4-min (240 s) budget fits ~4 entries per batch —
        // this is the founder's own worked example from the PR review.
        // previous_batch_size=256 so the growth cap doesn't bind here.
        assert_eq!(
            next_batch_size(Duration::from_secs(60), 256, 256, TARGET_BATCH_SECONDS),
            4
        );
    }

    #[test]
    fn next_batch_size_grows_for_fast_hardware_but_respects_growth_cap() {
        // ~1 s/entry ⇒ a 240 s budget fits 240 entries, but growth from a
        // previous batch of 4 is capped to GROWTH_FACTOR (8) × 4 = 32 — this
        // is the fix for the field failure where a single fast sample after a
        // small calibration batch derived a ~50x-larger batch in one step.
        assert_eq!(
            next_batch_size(Duration::from_secs(1), 256, 4, TARGET_BATCH_SECONDS),
            32
        );
    }

    #[test]
    fn next_batch_size_reaches_ceiling_once_previous_batch_is_already_large() {
        // Once the previous batch was already large enough that
        // GROWTH_FACTOR × it exceeds the ceiling, the ceiling (not the growth
        // cap) is the binding constraint — growth isn't artificially
        // stalled forever once it has ramped up.
        assert_eq!(
            next_batch_size(Duration::from_secs(1), 256, 64, TARGET_BATCH_SECONDS),
            240 // budget-derived value, below both the 512 growth cap and the 256 ceiling
        );
    }

    #[test]
    fn next_batch_size_clamps_to_ceiling() {
        // A very fast rate would derive a batch far above the ceiling (e.g.
        // the server's hard 256-chunk / 413 limit, or a user-supplied
        // `--batch-size`); the ceiling wins even when the growth cap (from a
        // large previous batch) would otherwise allow more.
        let t = next_batch_size(Duration::from_millis(1), 256, 256, TARGET_BATCH_SECONDS);
        assert_eq!(t, 256);
        let t = next_batch_size(Duration::from_millis(1), 32, 32, TARGET_BATCH_SECONDS);
        assert_eq!(t, 32);
    }

    #[test]
    fn next_batch_size_floors_at_one_for_extremely_slow_hardware() {
        // If a single entry alone blows the whole per-batch budget, we still
        // must send at least one entry per request.
        let t = next_batch_size(Duration::from_secs(10_000), 256, 4, TARGET_BATCH_SECONDS);
        assert_eq!(t, 1);
    }

    #[test]
    fn next_batch_size_handles_zero_duration_without_panicking() {
        // A degenerate zero-duration sample (e.g. a clock quirk) must not
        // divide-by-zero; falls back to the growth cap since the rate is
        // unmeasurably fast (not directly to the ceiling — growth is still
        // capped per step even in this degenerate case).
        let t = next_batch_size(Duration::ZERO, 256, 4, TARGET_BATCH_SECONDS);
        assert_eq!(t, 32); // growth_cap = 4 * GROWTH_FACTOR(8)
    }

    #[test]
    fn next_batch_size_uses_smaller_clamped_target_when_passed() {
        // A caller passing a smaller target_seconds (e.g. because
        // resolve_target_batch_seconds clamped it down for a small-budget
        // server) must derive a proportionally smaller batch, not always
        // TARGET_BATCH_SECONDS.
        let t = next_batch_size(Duration::from_secs(1), 256, 256, 20);
        assert_eq!(t, 20);
    }

    // ── batch_timeout: derive a per-request deadline from the measured rate ──
    // (spelunk-oss^71)

    #[test]
    fn batch_timeout_scales_with_expected_batch_duration() {
        // At 60 s/entry, a batch of 4 is expected to take 240 s; with the 4x
        // safety factor that's 960 s, clamped to the 1800 s ceiling.
        let t = batch_timeout(Duration::from_secs(60), 4);
        assert_eq!(t, Duration::from_secs(960));
    }

    #[test]
    fn batch_timeout_clamps_to_floor_for_fast_hardware() {
        // At 1 s/entry, a batch of 4 is expected to take 4 s; even with the
        // 4x safety factor (16 s) that's far below the floor, which must win
        // so transient latency spikes are still absorbed.
        let t = batch_timeout(Duration::from_secs(1), 4);
        assert_eq!(t, MIN_REQUEST_TIMEOUT);
    }

    #[test]
    fn batch_timeout_clamps_to_ceiling_for_pathologically_slow_rate() {
        let t = batch_timeout(Duration::from_secs(100_000), 256);
        assert_eq!(t, MAX_REQUEST_TIMEOUT);
    }

    #[test]
    fn batch_timeout_never_panics_on_degenerate_inputs() {
        let t = batch_timeout(Duration::ZERO, 0);
        assert!(t >= MIN_REQUEST_TIMEOUT && t <= MAX_REQUEST_TIMEOUT);
    }

    // ── RateEstimate: continuously re-estimate the per-entry rate ───────────
    // (spelunk-oss^74 — "keep re-estimating the rate as batches complete")

    #[test]
    fn rate_estimate_seeds_from_first_observation() {
        let mut r = RateEstimate::new();
        assert!(r.per_entry().is_none());
        r.update(Duration::from_secs(2), 1);
        assert_eq!(r.per_entry(), Some(Duration::from_secs(2)));
    }

    #[test]
    fn rate_estimate_deweights_the_batch_1_cold_sample_on_second_observation() {
        // First (calibration batch 1) sample: 1 entry in 10 s ⇒ 10 s/entry,
        // skewed by one-off cold start. The second sample (calibration batch
        // 2, at 1 s/entry) must dominate the blend — only
        // CALIBRATION_BATCH_1_WEIGHT (0.1) of the cold sample survives, not an
        // even 50/50 split.
        let mut r = RateEstimate::new();
        r.update(Duration::from_secs(10), 1);
        r.update(Duration::from_secs(4), 4); // 1 s/entry
        let blended = r.per_entry().unwrap();
        // Exact expected value: 10*0.1 + 1*0.9 = 1.9s.
        assert!(
            (blended.as_secs_f64() - 1.9).abs() < 1e-9,
            "expected the de-weighted blend 10*0.1 + 1*0.9 = 1.9s, got {blended:?}"
        );
        assert!(
            blended < Duration::from_secs(10),
            "the rate must move toward the newer, faster sample, got {blended:?}"
        );
    }

    #[test]
    fn rate_estimate_third_sample_onward_blends_50_50() {
        // From the THIRD observation onward (i.e. once the batch-1 cold
        // sample has already been superseded), later samples blend evenly
        // with the running estimate — same behaviour as before this fix.
        let mut r = RateEstimate::new();
        r.update(Duration::from_secs(10), 1); // batch 1 (cold): 10s/entry
        r.update(Duration::from_secs(4), 4); // batch 2: 1s/entry -> blended 1.9s/entry
        r.update(Duration::from_secs(3), 1); // batch 3: 3s/entry -> 50/50 blend with 1.9
        let blended = r.per_entry().unwrap();
        let expected = (1.9 + 3.0) / 2.0;
        assert!(
            (blended.as_secs_f64() - expected).abs() < 1e-9,
            "expected a plain 50/50 blend from the third sample onward: {expected}, got {blended:?}"
        );
    }

    #[test]
    fn rate_estimate_reproduces_field_failure_scenario_with_fix() {
        // Reproduces the founder's field-report numbers (PR #513 review,
        // 2026-07-05): calibration batch 1 (1 chunk) took ~25s (cold);
        // calibration batch 2 (4 chunks) took ~4.8s (~1.2s/entry, warm).
        // Pre-fix, a straight 50/50 blend gave ~13.1s/entry, but
        // next_batch_size used a DIFFERENT (raw, unblended) sample and
        // derived a batch of 200 — an inconsistency between the displayed
        // rate and the batch-size decision, and a batch sized to run for
        // ~240s against a 30s server budget. Post-fix, the single shared
        // estimate (de-weighted + growth-capped) must derive something far
        // smaller and internally consistent.
        let mut r = RateEstimate::new();
        r.update(Duration::from_secs(25), 1); // batch 1: cold
        r.update(Duration::from_millis(4800), 4); // batch 2: 1.2s/entry warm
        let per_entry = r.per_entry().unwrap();
        // 25*0.1 + 1.2*0.9 = 3.58s/entry.
        assert!(
            (per_entry.as_secs_f64() - 3.58).abs() < 1e-9,
            "expected 3.58s/entry, got {per_entry:?}"
        );
        // The SAME estimate feeds next_batch_size (growth-capped from the
        // previous batch of 4) — this must land far below the old field
        // failure's 200-chunk leap.
        let batch_3_size = next_batch_size(per_entry, 256, 4, TARGET_BATCH_SECONDS);
        assert_eq!(
            batch_3_size, 32,
            "growth-capped (GROWTH_FACTOR=8 * previous batch of 4) at 32, not the raw \
             240/3.58≈67 the estimate alone would suggest, and nowhere near the field \
             failure's 200"
        );
        // And the resulting batch's expected duration must be far below the
        // field failure's ~240s (the old, un-capped 200-chunk batch's target
        // duration) — this test uses the uncapped TARGET_BATCH_SECONDS
        // directly (not `resolve_target_batch_seconds`, covered separately
        // above) so the growth cap alone is what's under test here.
        let expected_duration = per_entry.as_secs_f64() * batch_3_size as f64;
        assert!(
            expected_duration < 150.0,
            "batch 3's expected duration ({expected_duration:.1}s) must be far below the \
             field failure's ~240s (200 chunks @ ~1.2s/entry)"
        );
    }

    #[test]
    fn rate_estimate_ignores_zero_length_batches() {
        let mut r = RateEstimate::new();
        r.update(Duration::from_secs(1), 0);
        assert!(r.per_entry().is_none());
    }

    // ── embed_progress_style: the ETA-aware indicatif template must build ───────
    // (spelunk-oss^74)

    #[test]
    fn embed_progress_style_builds_with_eta_token() {
        // `embed_progress_style()` calls `ProgressStyle::with_template(..).unwrap()`
        // on a template string containing `{eta}`. A malformed template (e.g. a
        // typo'd token) would panic at that unwrap the first time the embed phase
        // runs. Building it here proves the ETA-aware style is well-formed and
        // wired up, and applying it to a bar exercises the same path the embed
        // phase takes when it calls `bar.set_style(embed_progress_style())`.
        let style = embed_progress_style();
        let bar = ProgressBar::hidden();
        bar.set_style(style);
        // Driving the bar the way the embed phase does (steady tick + per-chunk
        // inc) must not panic with the ETA template applied.
        bar.enable_steady_tick(Duration::from_millis(120));
        bar.set_length(10);
        bar.tick();
        bar.inc(1);
        bar.finish_and_clear();
    }

    // ── run_embed_phase: a mid-run batch failure must not discard earlier,
    //    already-committed embeddings (spelunk-oss^71) ─────────────────────────

    use std::sync::OnceLock;

    use crate::capability::{Capabilities, EmbedderState, ServerLimits};
    use spelunk_core::config::Config;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Register the sqlite-vec extension exactly once per test process so the
    /// in-memory DB can create the `vec0` embeddings table.
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

    /// One constant `EMBEDDING_DIM`-vector of little-endian f32 per request
    /// chunk, matching the server's wire format (response[i] → chunk[i]).
    struct OkEmbedResponder;
    impl wiremock::Respond for OkEmbedResponder {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            #[derive(serde::Deserialize)]
            struct ReqBody {
                chunks: Vec<serde_json::Value>,
            }
            let body: ReqBody =
                serde_json::from_slice(&request.body).unwrap_or(ReqBody { chunks: vec![] });
            let dim = spelunk_core::embeddings::EMBEDDING_DIM;
            let mut bytes = Vec::with_capacity(body.chunks.len() * dim * 4);
            for _ in &body.chunks {
                for _ in 0..dim {
                    bytes.extend_from_slice(&0.1f32.to_le_bytes());
                }
            }
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(bytes)
        }
    }

    /// Insert `n` chunks into a fresh in-memory DB and return it plus their ids.
    fn seed_chunks(n: usize) -> (Database, Vec<i64>) {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open in-memory DB");
        let file_id = db.upsert_file("src/lib.rs", Some("rust"), "hash0").unwrap();
        let ids = (0..n)
            .map(|i| {
                db.insert_chunk(
                    file_id,
                    "function",
                    Some(&format!("f{i}")),
                    i,
                    i + 1,
                    &format!("fn f{i}() {{}}"),
                    None,
                    1,
                )
                .unwrap()
            })
            .collect();
        (db, ids)
    }

    fn server_tier(url: String) -> Tier {
        server_tier_with_limits(url, None)
    }

    /// Same as [`server_tier`], but with `server_limits` set — used by tests
    /// that exercise the version-skew clamping (spelunk-oss^71/^73/^74).
    fn server_tier_with_limits(url: String, server_limits: Option<ServerLimits>) -> Tier {
        Tier::Server {
            url,
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits,
        }
    }

    #[tokio::test]
    async fn batch_failure_keeps_prior_batches_and_stops_gracefully() {
        // 6 chunks with a small ceiling so calibration quickly ramps toward a
        // multi-chunk steady-state batch; the mock server's third response
        // (whichever batch it lands on) fails with 500. The run must persist
        // every chunk embedded before the failure, NOT error, and report only
        // the successfully-embedded count — proving a mid-run failure never
        // discards batches the server already computed (spelunk-oss^71).
        let mock = MockServer::start().await;
        // The first two requests (calibration: 1 chunk, then up to 4 chunks)
        // succeed; everything after that fails, so the run stops partway
        // through a small index without ever reaching a "finished" state.
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(6);
        let chunk_ids_and_texts: Vec<(i64, String)> =
            ids.iter().map(|id| (*id, format!("text {id}"))).collect();

        let cfg = Config::default();
        let tier = server_tier(mock.uri());
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            4, // batch_size ceiling
            &mp,
        )
        .await
        .expect("a failing batch must NOT return Err; it stops gracefully");

        // Calibration sends batch 1 (1 chunk) then batch 2 (up to 4 chunks,
        // clamped to what's left); both succeed here, so exactly
        // 1 + min(4, 5) = 5 chunks land before the third request fails.
        assert_eq!(
            embedded, 5,
            "the two successful calibration batches (1 + 4 chunks) must be reported as embedded"
        );
        assert_eq!(
            db.stats().unwrap().embedding_count,
            5,
            "the 5 embeddings from the successful batches must be persisted in the DB, \
             not rolled back when the next batch failed"
        );
    }

    #[tokio::test]
    async fn all_batches_success_embeds_everything() {
        // Control case: when every batch succeeds, all chunks are embedded and
        // persisted (guards against the failure path over-triggering), across
        // calibration batches and into steady state.
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(50);
        let chunk_ids_and_texts: Vec<(i64, String)> =
            ids.iter().map(|id| (*id, format!("text {id}"))).collect();

        let cfg = Config::default();
        let tier = server_tier(mock.uri());
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            8,
            &mp,
        )
        .await
        .expect("all-success run");

        assert_eq!(embedded, 50);
        assert_eq!(db.stats().unwrap().embedding_count, 50);
    }

    #[tokio::test]
    async fn small_index_below_calibration_size_still_embeds_everything() {
        // An index with fewer chunks than even the first calibration batch
        // (or between the two) must not panic on slicing and must still embed
        // every chunk — regression guard for the `.min(left)` clamps.
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .mount(&mock)
            .await;

        for n in [1usize, 2, 3] {
            let (db, ids) = seed_chunks(n);
            let chunk_ids_and_texts: Vec<(i64, String)> =
                ids.iter().map(|id| (*id, format!("text {id}"))).collect();

            let cfg = Config::default();
            let tier = server_tier(mock.uri());
            let mp = MultiProgress::new();

            let embedded = run_embed_phase(
                chunk_ids_and_texts,
                &db,
                &cfg,
                &tier,
                std::path::Path::new("/tmp/proj"),
                64,
                &mp,
            )
            .await
            .unwrap_or_else(|e| panic!("n={n} must succeed: {e:#}"));

            assert_eq!(embedded, n as u64, "n={n}");
        }
    }

    #[tokio::test]
    async fn empty_queue_returns_immediately_without_any_request() {
        // Nothing to embed (e.g. a re-run where every chunk already has an
        // embedding) must not enter the batch loop at all — a regression
        // guard for the `while cursor < remaining` loop that replaced the old
        // fixed-size `.chunks()` iterator, which handled a zero-length slice
        // for free.
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let (db, _ids) = seed_chunks(0);
        let cfg = Config::default();
        let tier = server_tier(mock.uri());
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            Vec::new(),
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            64,
            &mp,
        )
        .await
        .expect("an empty queue must succeed trivially");

        assert_eq!(embedded, 0);
    }

    // ── 408/timeout retry-then-shrink behaviour (spelunk-oss^71/^73/^74,
    //    PR #513 field-failure fix) ──────────────────────────────────────────

    #[tokio::test]
    async fn calibration_batch_1_408_is_retried_and_succeeds() {
        // The very first request (calibration batch of 1) 408s once, then
        // succeeds on retry. This must NOT be treated as a fatal failure at
        // 0/total embedded — the whole point of the escalated-patience retry
        // is that a single calibration request timing out must not kill the
        // phase outright (the exact founder-reported field failure).
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(408))
            .up_to_n_times(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(3);
        let chunk_ids_and_texts: Vec<(i64, String)> =
            ids.iter().map(|id| (*id, format!("text {id}"))).collect();

        let cfg = Config::default();
        let tier = server_tier(mock.uri());
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            64,
            &mp,
        )
        .await
        .expect("a single 408 on calibration batch 1 must be retried, not fatal");

        assert_eq!(
            embedded, 3,
            "all chunks must be embedded once the retried calibration request succeeds"
        );
        assert_eq!(db.stats().unwrap().embedding_count, 3);
    }

    #[tokio::test]
    async fn calibration_batch_1_408_twice_gives_up_gracefully() {
        // If the retried calibration request ALSO 408s, the phase must still
        // return Ok(0) (not Err) — the caller (`run_embed_phases`/`index()`)
        // depends on this to still print stats and exit cleanly rather than
        // unwinding via `?` before `db.stats()` runs.
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(408))
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(3);
        let chunk_ids_and_texts: Vec<(i64, String)> =
            ids.iter().map(|id| (*id, format!("text {id}"))).collect();

        let cfg = Config::default();
        let tier = server_tier(mock.uri());
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            64,
            &mp,
        )
        .await
        .expect("must return Ok(embedded), never Err, even after exhausting the retry");

        assert_eq!(embedded, 0, "nothing embedded when both attempts 408");
        assert_eq!(db.stats().unwrap().embedding_count, 0);
    }

    #[tokio::test]
    async fn steady_state_408_shrinks_batch_and_retries_instead_of_aborting() {
        // A steady-state (post-calibration) batch that 408s must shrink and
        // retry rather than discarding all subsequent progress. Set up: 20
        // chunks, a large `--batch-size` so calibration ramps toward a big
        // batch quickly, and the mock 408s on any request >4 chunks — forcing
        // the shrink-and-retry path to run at least once, ending with
        // everything eventually embedded.
        let mock = MockServer::start().await;

        struct ShrinkUntilSmallResponder;
        impl wiremock::Respond for ShrinkUntilSmallResponder {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                #[derive(serde::Deserialize)]
                struct ReqBody {
                    chunks: Vec<serde_json::Value>,
                }
                let body: ReqBody =
                    serde_json::from_slice(&request.body).unwrap_or(ReqBody { chunks: vec![] });
                if body.chunks.len() > 4 {
                    return ResponseTemplate::new(408);
                }
                let dim = spelunk_core::embeddings::EMBEDDING_DIM;
                let mut bytes = Vec::with_capacity(body.chunks.len() * dim * 4);
                for _ in &body.chunks {
                    for _ in 0..dim {
                        bytes.extend_from_slice(&0.1f32.to_le_bytes());
                    }
                }
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/octet-stream")
                    .set_body_bytes(bytes)
            }
        }

        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ShrinkUntilSmallResponder)
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(20);
        let chunk_ids_and_texts: Vec<(i64, String)> =
            ids.iter().map(|id| (*id, format!("text {id}"))).collect();

        let cfg = Config::default();
        let tier = server_tier(mock.uri());
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            64, // ceiling well above the mock's 4-chunk cliff
            &mp,
        )
        .await
        .expect("steady-state 408s must shrink and retry, not abort");

        assert_eq!(
            embedded, 20,
            "every chunk must eventually be embedded once the batch size shrinks below \
             the mock's 4-chunk cliff"
        );
        assert_eq!(db.stats().unwrap().embedding_count, 20);
    }

    #[tokio::test]
    async fn server_advertised_limits_clamp_batch_size_below_default_ceiling() {
        // A server whose /v1/health advertises a small max_batch_chunks must
        // have that respected even when the user's --batch-size (here: 0,
        // i.e. "use the default") would otherwise allow much larger batches.
        // We prove this indirectly: mount a mock that 413s any batch above
        // the advertised limit, and confirm the run still succeeds (i.e. the
        // client never actually sent an oversized batch).
        let mock = MockServer::start().await;

        struct RejectAboveLimitResponder {
            limit: usize,
        }
        impl wiremock::Respond for RejectAboveLimitResponder {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                #[derive(serde::Deserialize)]
                struct ReqBody {
                    chunks: Vec<serde_json::Value>,
                }
                let body: ReqBody =
                    serde_json::from_slice(&request.body).unwrap_or(ReqBody { chunks: vec![] });
                if body.chunks.len() > self.limit {
                    return ResponseTemplate::new(413);
                }
                let dim = spelunk_core::embeddings::EMBEDDING_DIM;
                let mut bytes = Vec::with_capacity(body.chunks.len() * dim * 4);
                for _ in &body.chunks {
                    for _ in 0..dim {
                        bytes.extend_from_slice(&0.1f32.to_le_bytes());
                    }
                }
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/octet-stream")
                    .set_body_bytes(bytes)
            }
        }

        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(RejectAboveLimitResponder { limit: 8 })
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(30);
        let chunk_ids_and_texts: Vec<(i64, String)> =
            ids.iter().map(|id| (*id, format!("text {id}"))).collect();

        let cfg = Config::default();
        let limits = ServerLimits {
            embed_request_timeout_secs: 1800,
            max_batch_chunks: 8,
            embedder_token_cap: None,
        };
        let tier = server_tier_with_limits(mock.uri(), Some(limits));
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            0, // user did not set --batch-size: default ceiling would be MAX_BATCH (256)
            &mp,
        )
        .await
        .expect("batches must stay within the server-advertised max_batch_chunks");

        assert_eq!(
            embedded, 30,
            "every chunk must embed successfully — a 413 here would mean the client sent \
             a batch larger than the server-advertised max_batch_chunks"
        );
        assert_eq!(db.stats().unwrap().embedding_count, 30);
    }
}
