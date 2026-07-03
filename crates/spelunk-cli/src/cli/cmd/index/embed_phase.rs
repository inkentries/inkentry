use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar};
use serde::Serialize;

use super::super::ui::{is_tty, progress_style};
use crate::{capability::Tier, config::Config, storage::Database};

/// Hard ceiling on chunks per request — the server enforces 256 (returns 413 if
/// exceeded), so the user-supplied `--batch-size` is clamped to this. The
/// server's candle embedder (F2LLM-v2-330M) runs inference in padded sub-batches
/// of EMBED_BATCH_SIZE=8, so e.g. 64 chunks = 8 forward passes.
const MAX_BATCH: usize = 256;

/// Default request batch size when the flag is left at its baseline. Kept well
/// below MAX_BATCH so each HTTP call completes within the request timeout.
const DEFAULT_BATCH: usize = 64;

/// Baseline per-request timeout used for the FIRST batch, before we have any
/// measurement of this machine's embedding speed. After the first batch lands we
/// switch to a timeout scaled to the observed per-chunk rate (see
/// `scaled_timeout`), so slow hardware degrades gracefully instead of eating a
/// fixed deadline with nothing persisted (spelunk-oss^71).
const FIRST_BATCH_TIMEOUT: Duration = Duration::from_secs(600);

/// Floor for a scaled per-request timeout. Even on a very fast machine we never
/// drop below this, to absorb transient latency spikes.
const MIN_SCALED_TIMEOUT: Duration = Duration::from_secs(120);

/// Ceiling for a scaled per-request timeout, so a pathologically slow first
/// sample can't derive an effectively unbounded deadline.
const MAX_SCALED_TIMEOUT: Duration = Duration::from_secs(1800);

/// Safety multiple applied to the observed per-chunk time when scaling the
/// timeout for later batches: give each request this much headroom over the
/// measured rate so normal jitter never trips it.
const TIMEOUT_SAFETY_FACTOR: u32 = 4;

/// Resolve the effective per-request batch size from the user-supplied
/// `--batch-size` flag: 0 falls back to the default, and anything above the
/// server ceiling is clamped to `MAX_BATCH`.
fn resolve_batch_size(requested: usize) -> usize {
    if requested == 0 {
        DEFAULT_BATCH
    } else {
        requested.min(MAX_BATCH)
    }
}

/// Derive a per-request timeout for subsequent batches from the FIRST batch's
/// observed wall-clock time.
///
/// The first batch of `first_batch_len` chunks took `first_batch_elapsed`; that
/// includes one-off model cold-start, so it is a deliberately pessimistic per-
/// chunk sample. We budget `TIMEOUT_SAFETY_FACTOR ×` the observed per-chunk time
/// for a full `batch_size`-chunk request, then clamp into
/// `[MIN_SCALED_TIMEOUT, MAX_SCALED_TIMEOUT]`. On slow hardware this stretches
/// the deadline so a genuinely slow-but-progressing request is not killed
/// mid-flight and its whole batch discarded (spelunk-oss^71).
fn scaled_timeout(
    first_batch_elapsed: Duration,
    first_batch_len: usize,
    batch_size: usize,
) -> Duration {
    let per_chunk = first_batch_elapsed
        .checked_div(first_batch_len.max(1) as u32)
        .unwrap_or(FIRST_BATCH_TIMEOUT);
    let budget = per_chunk
        .saturating_mul(batch_size.max(1) as u32)
        .saturating_mul(TIMEOUT_SAFETY_FACTOR);
    budget.clamp(MIN_SCALED_TIMEOUT, MAX_SCALED_TIMEOUT)
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

    let batch_size = resolve_batch_size(batch_size);

    // Use `resolve_project_id` so that loopback auto-discovered servers (where
    // `cfg.project_id` may be absent) derive the id from the project root path,
    // matching `Config::resolve_project_id` behaviour (see spelunk#307).
    let project_id_owned = cfg.resolve_project_id(project_root);
    let project_id = project_id_owned.as_str();

    // No client-wide timeout: we apply a PER-REQUEST timeout below, starting
    // pessimistic (`FIRST_BATCH_TIMEOUT`) and then scaling to this machine's
    // observed embedding speed after the first batch. A single fixed client
    // deadline is what let a slow first batch expire with nothing persisted
    // (spelunk-oss^71).
    let client = reqwest::Client::builder()
        .build()
        .context("building HTTP client for embed phase")?;

    let total = chunk_ids_and_texts.len() as u64;
    let bar = if is_tty() && !crate::utils::is_agent_mode() {
        let b = mp.add(ProgressBar::new(total));
        b.set_style(progress_style("Embedding"));
        b
    } else {
        ProgressBar::hidden()
    };

    // Draw the bar immediately, before the first (possibly long) request blocks,
    // so the embedding phase shows visible movement within ~1 s of parsing
    // finishing instead of only after the first batch round-trip returns
    // (spelunk-oss^73). The steady tick animates the spinner while a request is
    // in flight so a single slow batch never looks frozen.
    bar.set_message("awaiting first batch\u{2026}");
    bar.enable_steady_tick(std::time::Duration::from_millis(120));
    bar.tick();

    let num_batches = total.div_ceil(batch_size as u64).max(1);
    let mut embedded = 0u64;
    // Per-request timeout: pessimistic for batch 1, then scaled to the observed
    // per-chunk rate for every later batch (spelunk-oss^71).
    let mut request_timeout = FIRST_BATCH_TIMEOUT;

    for (batch_idx, batch) in chunk_ids_and_texts.chunks(batch_size).enumerate() {
        // Show which batch is in flight so a single slow request reads as
        // "waiting on batch N/M", not a frozen bar (spelunk-oss^73).
        bar.set_message(format!(
            "sent batch {}/{num_batches}, awaiting response\u{2026}",
            batch_idx + 1,
        ));

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
                    "batch {}/{num_batches} failed after {embedded}/{total} embedded; \
                     re-run `spelunk index` to finish the rest",
                    batch_idx + 1,
                ));
                eprintln!(
                    "Embedding stopped at batch {}/{num_batches} ({embedded}/{total} chunks \
                     embedded and saved): {e:#}",
                    batch_idx + 1,
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

        // After the first batch, scale the per-request timeout to this
        // machine's measured speed so later (and slower) batches degrade
        // gracefully instead of inheriting the fixed pessimistic deadline
        // (spelunk-oss^71).
        if batch_idx == 0 {
            request_timeout = scaled_timeout(started.elapsed(), batch.len(), batch_size);
        }
    }

    bar.finish_with_message(format!("{embedded} chunks embedded"));
    Ok(embedded)
}

/// Send one embed batch and return the raw little-endian f32 response bytes:
/// one `EMBEDDING_DIM`-float vector per request chunk, in request order.
///
/// Applies a per-request `timeout` (see `scaled_timeout`) and validates the
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
    fn resolve_batch_size_passes_through_valid_values() {
        // A user-supplied value within range is used verbatim — this is the
        // value that reaches `chunk_ids_and_texts.chunks(batch_size)` and so
        // actually controls the per-request batch size.
        assert_eq!(resolve_batch_size(1), 1);
        assert_eq!(resolve_batch_size(32), 32);
        assert_eq!(resolve_batch_size(64), 64);
        assert_eq!(resolve_batch_size(200), 200);
        assert_eq!(resolve_batch_size(MAX_BATCH), MAX_BATCH);
    }

    #[test]
    fn resolve_batch_size_falls_back_to_default_for_zero() {
        assert_eq!(resolve_batch_size(0), DEFAULT_BATCH);
        assert_eq!(DEFAULT_BATCH, 64);
    }

    #[test]
    fn resolve_batch_size_clamps_above_server_ceiling() {
        assert_eq!(resolve_batch_size(MAX_BATCH + 1), MAX_BATCH);
        assert_eq!(resolve_batch_size(10_000), MAX_BATCH);
    }

    // ── scaled_timeout: derive later-batch deadline from first-batch timing ────
    // (spelunk-oss^71)

    #[test]
    fn scaled_timeout_stretches_for_slow_hardware() {
        // First batch of 64 chunks took 200 s ⇒ ~3.125 s/chunk. A full 64-chunk
        // batch at that rate is 200 s; with the 4× safety factor the scaled
        // deadline is ~800 s, well above both the floor and the fixed 600 s
        // that previously killed slow first batches with nothing persisted.
        let t = scaled_timeout(Duration::from_secs(200), 64, 64);
        assert!(
            t > Duration::from_secs(600),
            "slow hardware must get a longer deadline than the old fixed 600 s, got {t:?}"
        );
        assert!(t <= MAX_SCALED_TIMEOUT, "never exceeds the ceiling");
    }

    #[test]
    fn scaled_timeout_clamps_to_floor_for_fast_hardware() {
        // First batch of 64 chunks in 1 s ⇒ a scaled budget far below the
        // floor; the floor must win so transient spikes are still absorbed.
        let t = scaled_timeout(Duration::from_secs(1), 64, 64);
        assert_eq!(t, MIN_SCALED_TIMEOUT);
    }

    #[test]
    fn scaled_timeout_clamps_to_ceiling_for_pathologically_slow_sample() {
        // An absurdly slow first sample must not derive an effectively
        // unbounded deadline.
        let t = scaled_timeout(Duration::from_secs(100_000), 1, MAX_BATCH);
        assert_eq!(t, MAX_SCALED_TIMEOUT);
    }

    #[test]
    fn scaled_timeout_handles_empty_first_batch_without_panicking() {
        // Degenerate inputs (zero-length batch / zero batch size) must not
        // divide-by-zero or overflow; the clamp still yields a valid duration.
        let t = scaled_timeout(Duration::from_secs(10), 0, 0);
        assert!(t >= MIN_SCALED_TIMEOUT && t <= MAX_SCALED_TIMEOUT);
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
        // 3 batches of 2 chunks each. The first two requests succeed; the third
        // returns 500. The run must persist the first 4 embeddings, NOT error,
        // and report only 4 of 6 embedded, proving a mid-run failure never
        // discards the batches the server already computed (spelunk-oss^71).
        let mock = MockServer::start().await;
        // First two POSTs succeed (mounts are matched newest-first, and
        // `up_to_n_times` bounds this one to the first two calls).
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        // Everything after that fails.
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
            2, // batch_size
            &mp,
        )
        .await
        .expect("a failing batch must NOT return Err; it stops gracefully");

        assert_eq!(
            embedded, 4,
            "the two successful batches (4 chunks) must be reported as embedded"
        );
        assert_eq!(
            db.stats().unwrap().embedding_count,
            4,
            "the 4 embeddings from the successful batches must be persisted in the DB, \
             not rolled back when the third batch failed"
        );
    }

    #[tokio::test]
    async fn all_batches_success_embeds_everything() {
        // Control case: when every batch succeeds, all chunks are embedded and
        // persisted (guards against the failure path over-triggering).
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(5);
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
            2,
            &mp,
        )
        .await
        .expect("all-success run");

        assert_eq!(embedded, 5);
        assert_eq!(db.stats().unwrap().embedding_count, 5);
    }
}
