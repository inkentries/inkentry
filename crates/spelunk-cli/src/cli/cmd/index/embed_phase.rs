use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Serialize;

use super::super::ui::is_tty;
use crate::{capability::Tier, config::Config, storage::Database};

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
fn resolve_batch_ceiling(requested: usize) -> usize {
    if requested == 0 {
        DEFAULT_BATCH_CEILING
    } else {
        requested.min(MAX_BATCH)
    }
}

/// Choose the next steady-state batch size from the measured per-entry
/// duration, so a batch takes roughly `TARGET_BATCH_SECONDS` wall-clock: slow
/// hardware (large `per_entry`) gets a small batch, fast hardware gets a large
/// one. Clamped to `[1, ceiling]` — `ceiling` is the smaller of `MAX_BATCH` (the
/// server's hard 413 limit) and the user's `--batch-size`, if they set one.
///
/// Examples from the calibration this replaces the old "time a fixed 64-batch"
/// approach with: ~60 s/entry ⇒ batch 4; ~1 s/entry ⇒ batch 256 (assuming the
/// ceiling allows it).
fn next_batch_size(per_entry: Duration, ceiling: usize) -> usize {
    if per_entry.is_zero() {
        return ceiling.max(1);
    }
    let target = Duration::from_secs(TARGET_BATCH_SECONDS).as_secs_f64();
    let size = (target / per_entry.as_secs_f64()).round();
    if size < 1.0 {
        1
    } else if size >= ceiling.max(1) as f64 {
        ceiling.max(1)
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

/// Running estimate of this run's embedding throughput, refined after every
/// batch so a rate that drifts mid-run (e.g. thermal throttling, another
/// process contending for the GPU) is picked up rather than locked in from the
/// first sample alone.
struct RateEstimate {
    /// Exponentially-weighted per-entry duration. `None` until the first batch
    /// lands.
    per_entry: Option<Duration>,
}

impl RateEstimate {
    fn new() -> Self {
        Self { per_entry: None }
    }

    /// Fold in a newly-observed batch: `elapsed` wall-clock time for
    /// `entries` chunks. The first observation seeds the estimate outright;
    /// later ones are blended in with an exponential weight so the estimate
    /// keeps tracking the current rate (letting later, larger batches
    /// dominate the noisier single-chunk calibration sample) without being
    /// destabilised by a single outlier batch.
    fn update(&mut self, elapsed: Duration, entries: usize) {
        if entries == 0 {
            return;
        }
        let sample = elapsed.div_f64(entries as f64);
        self.per_entry = Some(match self.per_entry {
            None => sample,
            // Blend 50/50 with the running estimate. This is a simple
            // exponential moving average: recent batches matter more than
            // stale ones, so a hardware/network rate change mid-run (thermal
            // throttling, contention) is reflected within a couple of
            // batches instead of being permanently anchored to the first
            // sample.
            Some(prev) => {
                let blended = (prev.as_secs_f64() + sample.as_secs_f64()) / 2.0;
                Duration::from_secs_f64(blended)
            }
        });
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

    // Ceiling the calibrated batch size may grow to. Unlike the old scheme,
    // this is no longer the size we always send — see `next_batch_size`.
    let ceiling = resolve_batch_ceiling(batch_size);

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
    let remaining = chunk_ids_and_texts.len();

    while cursor < remaining {
        batch_num += 1;
        let left = remaining - cursor;

        // Calibration phase: the first request is a single chunk, the second
        // is a small 4-chunk sample (both clamped to what's actually left, for
        // small indexes). Both exist purely to get real timing data before
        // committing to a steady-state batch size, per the founder's feedback
        // on spelunk-oss^71/^74 — we no longer time a full default-sized batch
        // and adapt afterward.
        let this_batch_size = match batch_num {
            1 => CALIBRATION_BATCH_1,
            2 => CALIBRATION_BATCH_2,
            _ => {
                let per_entry = rate
                    .per_entry()
                    .expect("rate is seeded after the first batch completes");
                next_batch_size(per_entry, ceiling)
            }
        }
        .clamp(1, left);

        let request_timeout = match rate.per_entry() {
            Some(per_entry) => batch_timeout(per_entry, this_batch_size),
            None => FIRST_REQUEST_TIMEOUT,
        };

        // Show which chunks are in flight so a single slow request reads as
        // progress against a known window, not a frozen bar (spelunk-oss^73).
        bar.set_message(format!(
            "sent {this_batch_size} chunk(s) ({}/{total} done so far), awaiting response\u{2026}",
            embedded,
        ));

        let batch = &chunk_ids_and_texts[cursor..cursor + this_batch_size];

        let req_chunks: Vec<ReqChunk> = batch
            .iter()
            .map(|(id, text)| ReqChunk {
                chunk_id: id.to_string(),
                content: text.clone(),
            })
            .collect();

        // Percent-encode the project_id path segment: slugs contain `/`
        // (`local/<hex>`, `github.com/owner/repo`) which would otherwise split
        // the segment and break axum routing → 404. See spelunk decision #106.
        let url = format!(
            "{}/v1/projects/{}/index/embed",
            server_url.trim_end_matches('/'),
            crate::server_client::encode_project_id(project_id),
        );

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

        let bytes = match outcome {
            Ok(bytes) => bytes,
            // A batch failing (timeout, reset, non-2xx, short read) must NOT
            // abort the whole run: embeddings already committed from prior
            // batches stay in the DB, and a re-run picks up the rest via the
            // missing-embedding backfill (spelunk-oss^72). Report and stop
            // rather than propagating an Err that would unwind the command
            // before `stats()` and discard the visible progress
            // (spelunk-oss^71).
            Err(e) => {
                bar.abandon_with_message(format!(
                    "batch failed after {embedded}/{total} embedded; \
                     re-run `spelunk index` to finish the rest",
                ));
                eprintln!(
                    "Embedding stopped after {embedded}/{total} chunks embedded and saved: {e:#}",
                );
                eprintln!(
                    "Re-run `spelunk index` to embed the remaining {} chunk(s); \
                     already-embedded chunks are skipped.",
                    total - embedded,
                );
                return Ok(embedded);
            }
        };

        let dim = spelunk_core::embeddings::EMBEDDING_DIM;
        let stride = dim * 4;

        for (i, (row_id, _text)) in batch.iter().enumerate() {
            let vector =
                spelunk_core::embeddings::blob_to_vec(&bytes[i * stride..(i + 1) * stride]);
            db.insert_embedding(*row_id, &vector)?;
            embedded += 1;
            bar.inc(1);
        }

        // Fold this batch's measured rate in and re-estimate: later batches'
        // sizes (and timeouts) track the current rate rather than being fixed
        // from a single early sample, so a rate that drifts mid-run is picked
        // up within a couple of batches (spelunk-oss^71/^74). The same
        // per-chunk `bar.inc(1)` cadence above also feeds the visible `{eta}`,
        // which indicatif smooths across batches so the tiny first
        // calibration batch's cold-start skew washes out quickly
        // (spelunk-oss^74).
        rate.update(started.elapsed(), batch.len());

        cursor += this_batch_size;
    }

    bar.finish_with_message(format!("{embedded} chunks embedded"));
    Ok(embedded)
}

/// Send one embed batch and return the raw little-endian f32 response bytes:
/// one `EMBEDDING_DIM`-float vector per request chunk, in request order.
///
/// Applies a per-request `timeout` (see `batch_timeout`) and validates the
/// response length before returning, so callers get a single fallible unit they
/// can treat as all-or-nothing for that batch (spelunk-oss^71).
async fn embed_one_batch(
    client: &reqwest::Client,
    url: &str,
    server_key: Option<&str>,
    body: EmbedRequest,
    batch_len: usize,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut req = client.post(url).timeout(timeout).json(&body);
    if let Some(k) = server_key {
        req = req.bearer_auth(k);
    }

    let bytes = req
        .send()
        .await
        .with_context(|| format!("calling {url}"))?
        .error_for_status()
        .context("server returned an error for index/embed")?
        .bytes()
        .await
        .context("reading index/embed response")?;

    let dim = spelunk_core::embeddings::EMBEDDING_DIM;
    let stride = dim * 4;
    let expected = batch_len * stride;
    anyhow::ensure!(
        bytes.len() == expected,
        "index/embed returned {} bytes, expected {expected} ({batch_len} × {dim}-dim f32)",
        bytes.len(),
    );
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_batch_ceiling_passes_through_valid_values() {
        // A user-supplied value within range is used verbatim as the ceiling
        // that calibration may grow the batch size up to.
        assert_eq!(resolve_batch_ceiling(1), 1);
        assert_eq!(resolve_batch_ceiling(32), 32);
        assert_eq!(resolve_batch_ceiling(64), 64);
        assert_eq!(resolve_batch_ceiling(200), 200);
        assert_eq!(resolve_batch_ceiling(MAX_BATCH), MAX_BATCH);
    }

    #[test]
    fn resolve_batch_ceiling_falls_back_to_default_for_zero() {
        // 0 means the user left `--batch-size` at its default: the ceiling is
        // the server's own hard limit, not some fixed pre-calibration size.
        assert_eq!(resolve_batch_ceiling(0), DEFAULT_BATCH_CEILING);
        assert_eq!(DEFAULT_BATCH_CEILING, MAX_BATCH);
    }

    #[test]
    fn resolve_batch_ceiling_clamps_above_server_ceiling() {
        assert_eq!(resolve_batch_ceiling(MAX_BATCH + 1), MAX_BATCH);
        assert_eq!(resolve_batch_ceiling(10_000), MAX_BATCH);
    }

    // ── next_batch_size: calibration-driven batch sizing ────────────────────
    // (spelunk-oss^71/^74, founder review on PR #513)

    #[test]
    fn next_batch_size_shrinks_for_slow_hardware() {
        // ~60 s/entry ⇒ a 4-min (240 s) budget fits ~4 entries per batch —
        // this is the founder's own worked example from the PR review.
        assert_eq!(next_batch_size(Duration::from_secs(60), 256), 4);
    }

    #[test]
    fn next_batch_size_grows_for_fast_hardware() {
        // ~1 s/entry ⇒ a 240 s budget fits 240 entries, clamped to the 256
        // ceiling only if it would exceed it; here it doesn't, so we get the
        // budget-derived value rather than always maxing out.
        assert_eq!(next_batch_size(Duration::from_secs(1), 256), 240);
    }

    #[test]
    fn next_batch_size_clamps_to_ceiling() {
        // A very fast rate would derive a batch far above the ceiling (e.g.
        // the server's hard 256-chunk / 413 limit, or a user-supplied
        // `--batch-size`); the ceiling wins.
        let t = next_batch_size(Duration::from_millis(1), 256);
        assert_eq!(t, 256);
        let t = next_batch_size(Duration::from_millis(1), 32);
        assert_eq!(t, 32);
    }

    #[test]
    fn next_batch_size_floors_at_one_for_extremely_slow_hardware() {
        // If a single entry alone blows the whole per-batch budget, we still
        // must send at least one entry per request.
        let t = next_batch_size(Duration::from_secs(10_000), 256);
        assert_eq!(t, 1);
    }

    #[test]
    fn next_batch_size_handles_zero_duration_without_panicking() {
        // A degenerate zero-duration sample (e.g. a clock quirk) must not
        // divide-by-zero; falls back to the ceiling since the rate is
        // unmeasurably fast.
        let t = next_batch_size(Duration::ZERO, 256);
        assert_eq!(t, 256);
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
    fn rate_estimate_blends_later_batches_rather_than_locking_to_first_sample() {
        // First (calibration) sample: 1 entry in 10 s ⇒ 10 s/entry, skewed by
        // one-off cold start. A later, larger batch at 1 s/entry should pull
        // the blended estimate down, not be ignored in favour of the stale
        // first sample.
        let mut r = RateEstimate::new();
        r.update(Duration::from_secs(10), 1);
        r.update(Duration::from_secs(4), 4); // 1 s/entry
        let blended = r.per_entry().unwrap();
        assert!(
            blended < Duration::from_secs(10),
            "the rate must move toward the newer, faster sample, got {blended:?}"
        );
        assert!(
            blended > Duration::from_secs(1),
            "a single fast batch should not instantly erase the earlier sample either, \
             got {blended:?}"
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

    use crate::capability::{Capabilities, EmbedderState};
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
        Tier::Server {
            url,
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
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
}
