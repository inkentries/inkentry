use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Serialize;

use super::super::ui::is_tty;
use crate::{
    capability::{ServerLimits, Tier},
    config::Config,
    storage::{Database, credential_hint},
};

// Server rejects larger batches with 413.
const MAX_BATCH: usize = 256;

const DEFAULT_BATCH_CEILING: usize = MAX_BATCH;

// Single chunk first: an early rate estimate, and the bar moves before a full batch lands.
const CALIBRATION_BATCH_1: usize = 1;

const CALIBRATION_BATCH_2: usize = 4;

const TARGET_BATCH_SECONDS: u64 = 240;

const MIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(1800);

const TIMEOUT_SAFETY_FACTOR: u32 = 4;

// A connect failure says nothing about batch size or throughput, so retry the same size
// on this schedule; its length bounds the retries.
const CONNECT_FAILURE_BACKOFFS: [Duration; 5] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(45),
    Duration::from_secs(90),
    Duration::from_secs(180),
];

const DEFAULT_SATURATION_RETRY: Duration = Duration::from_secs(5);

const MAX_SATURATION_RETRIES: usize = 30;

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

// Budget of servers predating the `/v1/health` `limits` field: a blanket 30s timeout.
const LEGACY_SERVER_REQUEST_BUDGET_SECS: u64 = 30;

// Headroom for jitter between the calibration sample and the batch sent.
const SERVER_BUDGET_TARGET_FRACTION: f64 = 2.0 / 3.0;

fn resolve_target_batch_seconds(server_limits: Option<ServerLimits>) -> u64 {
    let budget_secs = server_limits
        .and_then(|l| l.embed_request_timeout_secs)
        .unwrap_or(LEGACY_SERVER_REQUEST_BUDGET_SECS);
    let safe_budget = (budget_secs as f64 * SERVER_BUDGET_TARGET_FRACTION).floor() as u64;
    TARGET_BATCH_SECONDS.min(safe_budget.max(1))
}

// Caps per-step growth so one fast sample can't jump to an unmeasured size.
const GROWTH_FACTOR: usize = 8;

// Sized by token sum, not chunk count: per-chunk cost grows ~4x through the id-ordered
// queue, so a chunk-count budget calibrated on early chunks over-fills a batch of late ones.
fn next_batch_len(
    per_token: Duration,
    token_tail: &[usize],
    ceiling: usize,
    previous_batch_size: usize,
    target_seconds: u64,
) -> usize {
    let growth_cap = previous_batch_size
        .max(1)
        .saturating_mul(GROWTH_FACTOR)
        .min(ceiling.max(1));

    if per_token.is_zero() {
        return growth_cap.min(token_tail.len().max(1));
    }
    let target_tokens = Duration::from_secs(target_seconds).as_secs_f64() / per_token.as_secs_f64();

    let mut len = 0usize;
    let mut tokens = 0f64;
    for &tc in token_tail.iter().take(growth_cap) {
        tokens += tc.max(1) as f64;
        if len > 0 && tokens > target_tokens {
            break;
        }
        len += 1;
    }
    len.max(1)
}

fn batch_timeout(per_token: Duration, batch_tokens: u64) -> Duration {
    let expected_secs = per_token.as_secs_f64() * batch_tokens.max(1) as f64;
    let budget_secs = (expected_secs * TIMEOUT_SAFETY_FACTOR as f64)
        .clamp(0.0, MAX_REQUEST_TIMEOUT.as_secs_f64());
    Duration::from_secs_f64(budget_secs).clamp(MIN_REQUEST_TIMEOUT, MAX_REQUEST_TIMEOUT)
}

// Pessimistic: absorbs one-off model cold start.
const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

// Small: the 1-entry sample is dominated by one-off per-request overhead.
const CALIBRATION_BATCH_1_WEIGHT: f64 = 0.1;

// Single rate source for batch sizing, timeouts and the ETA. Per token, not per chunk:
// chunk cost isn't stationary through the queue. The token estimate's bias cancels only
// against estimates from the same run, so never cache the rate across runs.
struct RateEstimate {
    per_token: Option<Duration>,
    samples_seen: u32,
}

impl RateEstimate {
    fn new() -> Self {
        Self {
            per_token: None,
            samples_seen: 0,
        }
    }

    fn update(&mut self, elapsed: Duration, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let sample = elapsed.div_f64(tokens as f64);
        self.per_token = Some(match self.per_token {
            None => sample,
            Some(prev) if self.samples_seen == 1 => {
                // Second sample: de-weight the cold batch-1 sample.
                let w = CALIBRATION_BATCH_1_WEIGHT;
                let blended = prev.as_secs_f64() * w + sample.as_secs_f64() * (1.0 - w);
                Duration::from_secs_f64(blended)
            }
            Some(prev) => {
                let blended = (prev.as_secs_f64() + sample.as_secs_f64()) / 2.0;
                Duration::from_secs_f64(blended)
            }
        });
        self.samples_seen += 1;
    }

    fn per_token(&self) -> Option<Duration> {
        self.per_token
    }
}

// Not indicatif's `{eta}`: batches land in bursts, which it reads as rate ~0 and
// extrapolates absurd ETAs. The ETA is rendered into `{wide_msg}` instead.
fn embed_progress_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.cyan} Embedding [{bar:38.cyan/blue}] {pos}/{len}  {wide_msg}",
    )
    .unwrap()
    .progress_chars("=>-")
}

const ETA_DISPLAY_CAP: Duration = Duration::from_secs(24 * 60 * 60);

// Clamped in f64 before converting to `Duration`: a pathological rate can overflow or
// produce `inf`, and must yield the `>24h` string, never a panic.
fn format_eta(remaining_tokens: u64, per_token: Option<Duration>) -> String {
    let Some(per_token) = per_token else {
        return "ETA calibrating…".to_string();
    };
    if remaining_tokens == 0 {
        return "ETA 0s".to_string();
    }

    let seconds = (per_token.as_secs_f64() * remaining_tokens as f64).clamp(0.0, f64::MAX);
    if !seconds.is_finite() || seconds >= ETA_DISPLAY_CAP.as_secs_f64() {
        return "ETA >24h".to_string();
    }
    let remaining_duration = Duration::from_secs_f64(seconds);

    let total_secs = remaining_duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("ETA {hours}h{minutes:02}m")
    } else if minutes > 0 {
        if secs > 0 {
            format!("ETA {minutes}m{secs}s")
        } else {
            format!("ETA {minutes}m")
        }
    } else {
        format!("ETA {secs}s")
    }
}

// Callers must name the denominator; a bare percentage is banned from embedding-state output.
fn pct(done: u64, total: u64) -> u64 {
    done.saturating_mul(100).checked_div(total).unwrap_or(0)
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

#[derive(Clone, Copy)]
enum StopHint {
    RequestBudget,
    EmbedderDeviceLost,
}

fn persistent_failure_hint(server_url: &str, hint: StopHint) -> String {
    match hint {
        StopHint::RequestBudget => format!(
            "If this keeps happening: the inkentry-server at {server_url} may be enforcing a \
             smaller request budget than this batch needs."
        ),
        StopHint::EmbedderDeviceLost => format!(
            "The inkentry-server at {server_url} lost its embedder's GPU device (this can happen \
             after the machine has been idle or asleep for a long time). Restart it to \
             re-establish the device: `inkentry server stop && inkentry server start`, then \
             re-run `inkentry index`."
        ),
    }
}

// Returns instead of erroring so callers report progress so far via `Ok(embedded)`.
fn report_embed_failure(
    bar: &ProgressBar,
    embedded: u64,
    total: u64,
    server_url: &str,
    err: anyhow::Error,
    hint: StopHint,
) {
    bar.abandon_with_message(format!(
        "batch failed after {embedded}/{total} embedded; re-run `inkentry index` to finish the rest",
    ));
    eprintln!("Embedding stopped after {embedded}/{total} chunks embedded and saved: {err:#}");
    eprintln!(
        "Re-run `inkentry index` to embed the remaining {} chunk(s); already-embedded chunks \
         are skipped.",
        total - embedded,
    );
    eprintln!("{}", persistent_failure_hint(server_url, hint));
}

// Items are `(chunk_id, embedding_text, token_count)`.
pub(super) async fn run_embed_phase(
    chunk_ids_and_texts: Vec<(i64, String, usize)>,
    db: &Database,
    cfg: &Config,
    tier: &Tier,
    project_root: &std::path::Path,
    batch_size: usize,
    mp: &MultiProgress,
) -> Result<u64> {
    run_embed_phase_with_backoff(
        chunk_ids_and_texts,
        db,
        cfg,
        tier,
        project_root,
        batch_size,
        mp,
        &CONNECT_FAILURE_BACKOFFS,
    )
    .await
}

// Backoffs are injected so tests can run the exhausted-retries path in milliseconds.
#[allow(clippy::too_many_arguments)]
async fn run_embed_phase_with_backoff(
    chunk_ids_and_texts: Vec<(i64, String, usize)>,
    db: &Database,
    cfg: &Config,
    tier: &Tier,
    project_root: &std::path::Path,
    batch_size: usize,
    mp: &MultiProgress,
    connect_failure_backoffs: &[Duration],
) -> Result<u64> {
    let (server_url, server_key) = match tier {
        Tier::Server { url, .. } => (url.clone(), cfg.bearer_for(url)?),
        Tier::Offline(_) => return Ok(0),
    };
    // Bails on a model mismatch; stamps a fresh DB.
    db.ensure_embedding_model(inkentry_core::embeddings::MODEL_ID)?;
    // Warn, not bail: same-model drift is not corruption, but unchanged files keep old chunk
    // boundaries and only `--force` fixes that.
    let current_chunker_config = inkentry_core::indexer::chunker_config_id();
    if let Some(recorded) = db.ensure_chunker_config(&current_chunker_config)? {
        eprintln!(
            "Warning: this index was built with chunker config '{recorded}', but the \
             running build uses '{current_chunker_config}'. Unchanged files keep their old \
             chunk boundaries until re-parsed, so the index now mixes chunk granularities. \
             Run `inkentry index --force` to re-chunk everything under the current config.\n"
        );
    }
    let server_limits = tier.server_limits();

    let server_max_batch_chunks = server_limits.and_then(|l| l.max_batch_chunks);
    let ceiling = resolve_batch_ceiling(batch_size, server_max_batch_chunks);

    let target_batch_seconds = resolve_target_batch_seconds(server_limits);
    if server_limits.is_none() {
        eprintln!(
            "Note: inkentry-server at {server_url} did not report its /index/embed request \
             budget; assuming a conservative {LEGACY_SERVER_REQUEST_BUDGET_SECS}s budget and \
             targeting smaller batches accordingly."
        );
    }

    // Loopback servers may lack `cfg.project_id`.
    let project_id_owned = cfg.resolve_project_id(project_root);
    let project_id = project_id_owned.as_str();

    // Per-request timeouts only: one fixed deadline let a slow first batch expire with nothing saved.
    let client = inkentry_core::config::apply_server_ca(
        reqwest::Client::builder(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )?
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

    bar.set_message("calibrating batch size\u{2026}");
    bar.enable_steady_tick(std::time::Duration::from_millis(120));
    bar.tick();

    let mut rate = RateEstimate::new();
    let mut embedded = 0u64;
    let mut cursor = 0usize;
    let mut batch_num = 0u64;
    let mut progress_log = super::background_log::ProgressThrottle::new();
    let mut previous_batch_size = 1usize;
    let remaining = chunk_ids_and_texts.len();
    // ETA and "of work done" run over token totals; chunk fraction is coverage, a different question.
    let total_tokens: u64 = chunk_ids_and_texts
        .iter()
        .map(|(_, _, tc)| (*tc).max(1) as u64)
        .sum();
    let mut tokens_done = 0u64;
    // Slugs contain `/`, which would split the segment and 404 in routing.
    let url = format!(
        "{}/v1/projects/{}/index/embed",
        server_url.trim_end_matches('/'),
        crate::server_client::encode_project_id(project_id),
    );

    while cursor < remaining {
        batch_num += 1;
        let left = remaining - cursor;

        let mut this_batch_size = match batch_num {
            1 => CALIBRATION_BATCH_1,
            2 => CALIBRATION_BATCH_2,
            _ => {
                let per_token = rate
                    .per_token()
                    .expect("rate is seeded after the first batch completes");
                let token_tail: Vec<usize> = chunk_ids_and_texts[cursor..]
                    .iter()
                    .map(|(_, _, tc)| *tc)
                    .collect();
                next_batch_len(
                    per_token,
                    &token_tail,
                    ceiling,
                    previous_batch_size,
                    target_batch_seconds,
                )
            }
        }
        .clamp(1, left);

        // 408/timeout, connect failure and 429 are recoverable per batch (shrink, or retry the
        // same size); anything else aborts.
        let mut escalated_calibration_once = false;
        let mut connect_failures = 0usize;
        let mut saturation_retries = 0usize;
        let bytes = 'retry: loop {
            let batch_tokens: u64 = chunk_ids_and_texts[cursor..cursor + this_batch_size]
                .iter()
                .map(|(_, _, tc)| (*tc).max(1) as u64)
                .sum();
            let request_timeout = match rate.per_token() {
                Some(per_token) => batch_timeout(per_token, batch_tokens),
                None if escalated_calibration_once => MAX_REQUEST_TIMEOUT,
                None => FIRST_REQUEST_TIMEOUT,
            };

            let eta_str = format_eta(total_tokens.saturating_sub(tokens_done), rate.per_token());
            let work_pct = pct(tokens_done, total_tokens);
            bar.set_message(format!(
                "{eta_str}  \u{00b7}  sent {this_batch_size} chunk(s) ({embedded}/{total} chunks, \
                 {work_pct}% of work done), awaiting response\u{2026}",
            ));

            let batch = &chunk_ids_and_texts[cursor..cursor + this_batch_size];

            let req_chunks: Vec<ReqChunk> = batch
                .iter()
                .map(|(id, text, _)| ReqChunk {
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
                    rate.update(started.elapsed(), batch_tokens);
                    break 'retry bytes;
                }
                Err(EmbedBatchError::BudgetExceeded(e)) if this_batch_size == 1 => {
                    if !escalated_calibration_once && rate.per_token().is_none() {
                        escalated_calibration_once = true;
                        eprintln!(
                            "First embed request timed out (server request budget \
                             may be smaller than expected); retrying with more patience\u{2026}"
                        );
                        continue 'retry;
                    }
                    // Return the count, not `Err`: an `Err` would unwind before `stats()` and discard progress.
                    report_embed_failure(
                        &bar,
                        embedded,
                        total,
                        &server_url,
                        e,
                        StopHint::RequestBudget,
                    );
                    return Ok(embedded);
                }
                Err(EmbedBatchError::BudgetExceeded(e)) => {
                    let shrunk = (this_batch_size / 2).max(1);
                    if shrunk == this_batch_size {
                        // Guards against looping forever if the size-1 arm above is bypassed.
                        report_embed_failure(
                            &bar,
                            embedded,
                            total,
                            &server_url,
                            e,
                            StopHint::RequestBudget,
                        );
                        return Ok(embedded);
                    }
                    tracing::warn!(
                        "index/embed batch of {this_batch_size} chunks exceeded the server's \
                         request budget (408) — shrinking to {shrunk} chunk(s) and retrying: {e:#}",
                    );
                    // Pessimistic sample so later `next_batch_len` calls don't re-derive this batch.
                    rate.update(request_timeout, batch_tokens);
                    this_batch_size = shrunk;
                    continue 'retry;
                }
                Err(EmbedBatchError::ConnectFailure(e)) => {
                    // No batch size fixes an unreachable server, and this attempt's elapsed time
                    // measures nothing about throughput, so it is not folded into `rate`.
                    if connect_failures >= connect_failure_backoffs.len() {
                        report_embed_failure(
                            &bar,
                            embedded,
                            total,
                            &server_url,
                            e,
                            StopHint::RequestBudget,
                        );
                        return Ok(embedded);
                    }
                    let backoff = connect_failure_backoffs[connect_failures];
                    connect_failures += 1;
                    tracing::warn!(
                        "index/embed: could not connect to {server_url} (attempt \
                         {connect_failures}/{}), retrying the same batch of \
                         {this_batch_size} chunk(s) in {backoff:?}: {e:#}",
                        connect_failure_backoffs.len(),
                    );
                    tokio::time::sleep(backoff).await;
                    continue 'retry;
                }
                Err(EmbedBatchError::Saturated(retry_after)) => {
                    // Server is up but shed the request: retry the same size after its own `Retry-After`.
                    if saturation_retries >= MAX_SATURATION_RETRIES {
                        report_embed_failure(
                            &bar,
                            embedded,
                            total,
                            &server_url,
                            anyhow::anyhow!(
                                "server embed admission queue stayed saturated after \
                                 {MAX_SATURATION_RETRIES} retries"
                            ),
                            StopHint::RequestBudget,
                        );
                        return Ok(embedded);
                    }
                    saturation_retries += 1;
                    tracing::info!(
                        "index/embed: server embedder busy (429), retrying the same batch \
                         of {this_batch_size} chunk(s) in {retry_after:?} (attempt \
                         {saturation_retries}/{MAX_SATURATION_RETRIES})",
                    );
                    tokio::time::sleep(retry_after).await;
                    continue 'retry;
                }
                Err(EmbedBatchError::EmbedderDeviceLost(e)) => {
                    report_embed_failure(
                        &bar,
                        embedded,
                        total,
                        &server_url,
                        e,
                        StopHint::EmbedderDeviceLost,
                    );
                    return Ok(embedded);
                }
                Err(EmbedBatchError::Other(e)) => {
                    report_embed_failure(
                        &bar,
                        embedded,
                        total,
                        &server_url,
                        e,
                        StopHint::RequestBudget,
                    );
                    return Ok(embedded);
                }
            }
        };

        let dim = inkentry_core::embeddings::EMBEDDING_DIM;
        let stride = dim * 4;
        let batch = &chunk_ids_and_texts[cursor..cursor + this_batch_size];

        // One transaction per batch: a kill mid-batch rolls it back and
        // `chunks_missing_embeddings` re-queues it whole.
        let embeddings: Vec<(i64, Vec<f32>)> = batch
            .iter()
            .enumerate()
            .map(|(i, (row_id, _text, _token_count))| {
                let vector =
                    inkentry_core::embeddings::blob_to_vec(&bytes[i * stride..(i + 1) * stride]);
                (*row_id, vector)
            })
            .collect();
        db.insert_embeddings(&embeddings)?;
        super::crash_test_hook::pause_at("after_embed_batch", &batch_num.to_string());

        // Repaint per chunk so the ETA counts down through a batch, not once per request.
        for (_row_id, _text, token_count) in batch.iter() {
            embedded += 1;
            tokens_done += (*token_count).max(1) as u64;
            bar.inc(1);
            let eta_str = format_eta(total_tokens.saturating_sub(tokens_done), rate.per_token());
            let work_pct = pct(tokens_done, total_tokens);
            bar.set_message(format!(
                "{eta_str}  \u{00b7}  {embedded}/{total} chunks embedded \
                 ({work_pct}% of work done)"
            ));
        }

        previous_batch_size = this_batch_size;
        cursor += this_batch_size;

        // The detached worker has no bar; log plain lines instead.
        if progress_log.due(cursor >= remaining) {
            let work_pct = pct(tokens_done, total_tokens);
            super::background_log::emit(format!(
                "embedding: {embedded}/{total} chunks ({work_pct}% of work done)"
            ));
        }
    }

    bar.finish_with_message(format!("{embedded} chunks embedded"));
    Ok(embedded)
}

enum EmbedBatchError {
    BudgetExceeded(anyhow::Error),
    ConnectFailure(anyhow::Error),
    Saturated(Duration),
    EmbedderDeviceLost(anyhow::Error),
    Other(anyhow::Error),
}

fn response_signals_device_lost(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    value
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        == Some("embedder_device_lost")
}

// The server only sends delta-seconds, never an HTTP-date.
fn parse_retry_after(resp: &reqwest::Response) -> Duration {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_SATURATION_RETRY)
}

// Returns little-endian f32 vectors, one per chunk in request order.
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
        // Before `is_timeout()`: a connect failure whose OS error is a timeout satisfies both.
        Err(e) if e.is_connect() => {
            return Err(EmbedBatchError::ConnectFailure(
                anyhow::Error::new(e)
                    .context(format!("calling {url} (could not connect to the server)")),
            ));
        }
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

    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(EmbedBatchError::Saturated(parse_retry_after(&resp)));
    }

    // A 503 with `embedder_device_lost` is an upstream inference failure, not a batch-size
    // rejection; other 503s (e.g. embedder warming up) stay generic.
    if resp.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        let body = resp.text().await.unwrap_or_default();
        if response_signals_device_lost(&body) {
            return Err(EmbedBatchError::EmbedderDeviceLost(anyhow::anyhow!(
                "the inkentry-server's embedder lost its GPU device (upstream inference \
                 failure), not a request-budget or batch-size rejection"
            )));
        }
        return Err(EmbedBatchError::Other(anyhow::anyhow!(
            "server returned 503 Service Unavailable for index/embed: {}",
            body.trim(),
        )));
    }

    // Same credential hint as the memory and sync paths. Names the origin, not this endpoint,
    // because `auth set-key` normalises to an origin.
    let hint = url
        .parse::<reqwest::Url>()
        .map(|u| credential_hint(resp.status(), &u.origin().ascii_serialization()))
        .unwrap_or_default();
    let resp = match resp.error_for_status() {
        Ok(resp) => resp,
        Err(e) => {
            return Err(EmbedBatchError::Other(anyhow::Error::new(e).context(
                format!("server returned an error for index/embed.{hint}"),
            )));
        }
    };

    let bytes = resp
        .bytes()
        .await
        .context("reading index/embed response")
        .map_err(EmbedBatchError::Other)?;

    let dim = inkentry_core::embeddings::EMBEDDING_DIM;
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
        assert_eq!(resolve_batch_ceiling(1, None), 1);
        assert_eq!(resolve_batch_ceiling(32, None), 32);
        assert_eq!(resolve_batch_ceiling(64, None), 64);
        assert_eq!(resolve_batch_ceiling(200, None), 200);
        assert_eq!(resolve_batch_ceiling(MAX_BATCH, None), MAX_BATCH);
    }

    #[test]
    fn resolve_batch_ceiling_falls_back_to_default_for_zero() {
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
        assert_eq!(resolve_batch_ceiling(0, Some(32)), 32);
        assert_eq!(resolve_batch_ceiling(200, Some(32)), 32);
        assert_eq!(resolve_batch_ceiling(16, Some(256)), 16);
    }

    #[test]
    fn absent_server_limits_leave_the_chunk_ceiling_at_this_clis_own_maximum() {
        let advertised = resolve_batch_ceiling(0, Some(16));
        let degraded = resolve_batch_ceiling(0, None);
        assert_eq!(advertised, 16);
        assert_eq!(degraded, MAX_BATCH);
        assert!(
            degraded > advertised,
            "losing a small advertised cap raises the ceiling rather than \
             lowering it, which is the opposite of a conservative fallback"
        );
    }

    #[test]
    fn resolve_target_batch_seconds_uses_default_when_server_budget_is_generous() {
        let limits = ServerLimits {
            embed_request_timeout_secs: Some(1800),
            max_batch_chunks: Some(256),
            embedder_token_cap: None,
            embed_threads: None,
        };
        assert_eq!(
            resolve_target_batch_seconds(Some(limits)),
            TARGET_BATCH_SECONDS
        );
    }

    #[test]
    fn resolve_target_batch_seconds_clamps_down_for_small_server_budget() {
        let limits = ServerLimits {
            embed_request_timeout_secs: Some(60),
            max_batch_chunks: Some(256),
            embedder_token_cap: None,
            embed_threads: None,
        };
        assert_eq!(resolve_target_batch_seconds(Some(limits)), 40); // 60 * 2/3
    }

    #[test]
    fn resolve_target_batch_seconds_assumes_legacy_budget_when_server_limits_absent() {
        assert_eq!(
            resolve_target_batch_seconds(None),
            20 // 30 * 2/3, floored
        );
    }

    fn unit_tail(n: usize) -> Vec<usize> {
        vec![1; n]
    }

    #[test]
    fn next_batch_len_shrinks_for_slow_hardware() {
        assert_eq!(
            next_batch_len(
                Duration::from_secs(60),
                &unit_tail(256),
                256,
                256,
                TARGET_BATCH_SECONDS
            ),
            4
        );
    }

    #[test]
    fn next_batch_len_grows_for_fast_hardware_but_respects_growth_cap() {
        assert_eq!(
            next_batch_len(
                Duration::from_secs(1),
                &unit_tail(256),
                256,
                4,
                TARGET_BATCH_SECONDS
            ),
            32
        );
    }

    #[test]
    fn next_batch_len_reaches_budget_once_previous_batch_is_already_large() {
        assert_eq!(
            next_batch_len(
                Duration::from_secs(1),
                &unit_tail(256),
                256,
                64,
                TARGET_BATCH_SECONDS
            ),
            240 // budget-derived value, below both the 512 growth cap and the 256 ceiling
        );
    }

    #[test]
    fn next_batch_len_clamps_to_ceiling() {
        let t = next_batch_len(
            Duration::from_millis(1),
            &unit_tail(512),
            256,
            256,
            TARGET_BATCH_SECONDS,
        );
        assert_eq!(t, 256);
        let t = next_batch_len(
            Duration::from_millis(1),
            &unit_tail(512),
            32,
            32,
            TARGET_BATCH_SECONDS,
        );
        assert_eq!(t, 32);
    }

    #[test]
    fn next_batch_len_floors_at_one_for_extremely_slow_hardware() {
        let t = next_batch_len(
            Duration::from_secs(10_000),
            &unit_tail(256),
            256,
            4,
            TARGET_BATCH_SECONDS,
        );
        assert_eq!(t, 1);
    }

    #[test]
    fn next_batch_len_handles_zero_duration_without_panicking() {
        let t = next_batch_len(
            Duration::ZERO,
            &unit_tail(256),
            256,
            4,
            TARGET_BATCH_SECONDS,
        );
        assert_eq!(t, 32); // growth_cap = 4 * GROWTH_FACTOR(8)
    }

    #[test]
    fn next_batch_len_uses_smaller_clamped_target_when_passed() {
        let t = next_batch_len(Duration::from_secs(1), &unit_tail(256), 256, 256, 20);
        assert_eq!(t, 20);
    }

    #[test]
    fn next_batch_len_fills_by_token_sum_not_chunk_count() {
        let tail = vec![100usize; 256];
        let t = next_batch_len(
            Duration::from_secs(1),
            &tail,
            256,
            256,
            TARGET_BATCH_SECONDS,
        );
        assert_eq!(t, 2);
    }

    #[test]
    fn next_batch_len_stops_at_a_size_transition_in_the_queue() {
        let mut tail = vec![1usize, 1, 1];
        tail.extend(vec![1000usize; 64]);
        let t = next_batch_len(
            Duration::from_secs(1),
            &tail,
            256,
            256,
            TARGET_BATCH_SECONDS,
        );
        assert_eq!(t, 3);
    }

    #[test]
    fn next_batch_len_zero_token_chunks_are_floored_not_free() {
        let tail = vec![0usize; 512];
        let t = next_batch_len(
            Duration::from_secs(60),
            &tail,
            256,
            256,
            TARGET_BATCH_SECONDS,
        );
        assert_eq!(t, 4); // identical to the 1-token case
    }

    #[test]
    fn batch_timeout_scales_with_expected_batch_duration() {
        let t = batch_timeout(Duration::from_secs(60), 4);
        assert_eq!(t, Duration::from_secs(960));
    }

    #[test]
    fn batch_timeout_clamps_to_floor_for_fast_hardware() {
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

    #[test]
    fn batch_timeout_tracks_batch_token_sum_not_chunk_count() {
        let per_token = Duration::from_secs(1);
        let light = batch_timeout(per_token, 100);
        let heavy = batch_timeout(per_token, 1000);
        assert_eq!(light, Duration::from_secs(400));
        assert_eq!(heavy, MAX_REQUEST_TIMEOUT); // 4000s clamped to 1800s
        assert!(heavy > light);
    }

    #[test]
    fn connect_failure_backoffs_is_the_documented_schedule() {
        assert_eq!(
            CONNECT_FAILURE_BACKOFFS,
            [
                Duration::from_secs(5),
                Duration::from_secs(15),
                Duration::from_secs(45),
                Duration::from_secs(90),
                Duration::from_secs(180),
            ]
        );
    }

    #[test]
    fn rate_estimate_seeds_from_first_observation() {
        let mut r = RateEstimate::new();
        assert!(r.per_token().is_none());
        r.update(Duration::from_secs(2), 1);
        assert_eq!(r.per_token(), Some(Duration::from_secs(2)));
    }

    #[test]
    fn rate_estimate_deweights_the_batch_1_cold_sample_on_second_observation() {
        let mut r = RateEstimate::new();
        r.update(Duration::from_secs(10), 1);
        r.update(Duration::from_secs(4), 4); // 1 s/token
        let blended = r.per_token().unwrap();
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
        let mut r = RateEstimate::new();
        r.update(Duration::from_secs(10), 1); // batch 1 (cold): 10s/token
        r.update(Duration::from_secs(4), 4); // batch 2: 1s/token -> blended 1.9s/token
        r.update(Duration::from_secs(3), 1); // batch 3: 3s/token -> 50/50 blend with 1.9
        let blended = r.per_token().unwrap();
        let expected = (1.9 + 3.0) / 2.0;
        assert!(
            (blended.as_secs_f64() - expected).abs() < 1e-9,
            "expected a plain 50/50 blend from the third sample onward: {expected}, got {blended:?}"
        );
    }

    #[test]
    fn rate_estimate_reproduces_field_failure_scenario_with_fix() {
        let mut r = RateEstimate::new();
        r.update(Duration::from_secs(25), 1); // batch 1: cold
        r.update(Duration::from_millis(4800), 4); // batch 2: 1.2s/token warm
        let per_token = r.per_token().unwrap();
        assert!(
            (per_token.as_secs_f64() - 3.58).abs() < 1e-9,
            "expected 3.58s/token, got {per_token:?}"
        );
        let batch_3_size = next_batch_len(per_token, &unit_tail(256), 256, 4, TARGET_BATCH_SECONDS);
        assert_eq!(
            batch_3_size, 32,
            "growth-capped (GROWTH_FACTOR=8 * previous batch of 4) at 32, not the raw \
             240/3.58≈67 the estimate alone would suggest, and nowhere near the field \
             failure's 200"
        );
        let expected_duration = per_token.as_secs_f64() * batch_3_size as f64;
        assert!(
            expected_duration < 150.0,
            "batch 3's expected duration ({expected_duration:.1}s) must be far below the \
             field failure's ~240s (200 chunks @ ~1.2s/token)"
        );
    }

    #[test]
    fn rate_estimate_ignores_zero_token_batches() {
        let mut r = RateEstimate::new();
        r.update(Duration::from_secs(1), 0);
        assert!(r.per_token().is_none());
    }

    #[test]
    fn rate_estimate_ignores_connect_failure_attempts() {
        let mut r = RateEstimate::new();
        r.update(Duration::from_secs(10), 1); // batch 1 (cold): 10s/token
        r.update(Duration::from_secs(4), 4); // batch 2, after the retries: 1s/token
        let blended = r.per_token().unwrap();
        assert!(
            (blended.as_secs_f64() - 1.9).abs() < 1e-9,
            "connect-failure retries must not fold a bogus sample into the rate estimate: \
             expected the plain 2-real-sample blend 1.9s/token, got {blended:?}"
        );
        assert_eq!(
            r.samples_seen, 2,
            "only the two real batches count as samples, not any connect-failure attempt"
        );
    }

    #[test]
    fn work_fraction_diverges_from_chunk_fraction_on_a_token_skewed_queue() {
        let queue: Vec<(i64, String, usize)> = vec![
            (1, "a".into(), 10),
            (2, "b".into(), 10),
            (3, "c".into(), 400),
            (4, "d".into(), 400),
        ];
        let total_tokens: u64 = queue.iter().map(|(_, _, tc)| *tc as u64).sum();
        let tokens_done: u64 = queue[..2].iter().map(|(_, _, tc)| *tc as u64).sum();

        let chunk_pct = pct(2, queue.len() as u64);
        let work_pct = pct(tokens_done, total_tokens);

        assert_eq!(chunk_pct, 50);
        assert_eq!(work_pct, 2); // 20 / 820
        assert_ne!(
            chunk_pct, work_pct,
            "chunk fraction is coverage, token fraction is progress; on a skewed \
             queue they must not coincide"
        );
    }

    #[test]
    fn pct_is_zero_over_an_empty_denominator() {
        assert_eq!(pct(0, 0), 0);
        assert_eq!(pct(5, 0), 0);
    }

    #[test]
    fn eta_is_token_weighted_not_chunk_weighted() {
        let per_token = Some(Duration::from_secs(1));
        let light = format_eta(60, per_token);
        let heavy = format_eta(6000, per_token);
        assert_eq!(light, "ETA 1m");
        assert_eq!(heavy, "ETA 1h40m");
    }

    #[test]
    fn embed_progress_style_builds_without_indicatif_eta_token() {
        let style = embed_progress_style();
        let bar = ProgressBar::hidden();
        bar.set_style(style);
        bar.enable_steady_tick(Duration::from_millis(120));
        bar.set_length(10);
        bar.tick();
        bar.set_message(format_eta(9, Some(Duration::from_secs(2))));
        bar.inc(1);
        bar.finish_and_clear();
    }

    #[test]
    fn format_eta_shows_calibrating_when_rate_unknown() {
        assert_eq!(format_eta(41, None), "ETA calibrating…");
    }

    #[test]
    fn format_eta_shows_seconds_for_sub_minute_remaining() {
        assert_eq!(format_eta(12, Some(Duration::from_secs(1))), "ETA 12s");
    }

    #[test]
    fn format_eta_shows_zero_seconds_when_nothing_remains() {
        assert_eq!(format_eta(0, Some(Duration::from_secs(5))), "ETA 0s");
    }

    #[test]
    fn format_eta_shows_minutes_and_seconds() {
        assert_eq!(format_eta(100, Some(Duration::from_secs(2))), "ETA 3m20s");
    }

    #[test]
    fn format_eta_shows_bare_minutes_when_no_remainder_seconds() {
        assert_eq!(format_eta(180, Some(Duration::from_secs(1))), "ETA 3m");
    }

    #[test]
    fn format_eta_shows_hours_and_minutes() {
        assert_eq!(format_eta(65, Some(Duration::from_secs(60))), "ETA 1h05m");
    }

    #[test]
    fn format_eta_caps_pathologically_large_duration_instead_of_showing_absurd_value() {
        let eta = format_eta(1_000_000, Some(Duration::from_secs(10_000_000)));
        assert_eq!(eta, "ETA >24h");
        assert!(
            !eta.contains('y'),
            "must never render a years-scale duration like the field-observed 153y bug: {eta}"
        );
    }

    #[test]
    fn format_eta_caps_at_boundary_just_above_24h() {
        let eta = format_eta(1, Some(Duration::from_secs(24 * 60 * 60 + 1)));
        assert_eq!(eta, "ETA >24h");
    }

    #[test]
    fn format_eta_does_not_panic_on_overflow_prone_inputs() {
        let eta = format_eta(u64::MAX, Some(Duration::MAX));
        assert_eq!(eta, "ETA >24h");
    }

    use std::sync::OnceLock;

    use crate::capability::{Capabilities, EmbedderState, ServerLimits};
    use inkentry_core::config::Config;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    struct OkEmbedResponder;
    impl wiremock::Respond for OkEmbedResponder {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            #[derive(serde::Deserialize)]
            struct ReqBody {
                chunks: Vec<serde_json::Value>,
            }
            let body: ReqBody =
                serde_json::from_slice(&request.body).unwrap_or(ReqBody { chunks: vec![] });
            let dim = inkentry_core::embeddings::EMBEDDING_DIM;
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

    #[tokio::test]
    async fn embed_one_batch_classifies_a_refused_connection_as_connect_failure() {
        // A released port refuses the connection instantly.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/v1/projects/x/index/embed");

        let result = embed_one_batch(
            &client,
            &url,
            None,
            EmbedRequest { chunks: vec![] },
            0,
            Duration::from_secs(5),
        )
        .await;

        match result {
            Err(EmbedBatchError::ConnectFailure(_)) => {}
            Err(EmbedBatchError::BudgetExceeded(e)) => panic!(
                "a refused connection must classify as ConnectFailure, not BudgetExceeded: {e:#}"
            ),
            Err(EmbedBatchError::Saturated(_)) => {
                panic!("a refused connection must classify as ConnectFailure, not Saturated")
            }
            Err(EmbedBatchError::Other(e)) => {
                panic!("a refused connection must classify as ConnectFailure, not Other: {e:#}")
            }
            Err(EmbedBatchError::EmbedderDeviceLost(e)) => panic!(
                "a refused connection must classify as ConnectFailure, not EmbedderDeviceLost: {e:#}"
            ),
            Ok(_) => panic!("connecting to a released, unlistened port must fail"),
        }
    }

    #[tokio::test]
    async fn embed_one_batch_still_classifies_a_slow_connected_server_as_budget_exceeded() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(300)))
            .mount(&mock)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/v1/projects/x/index/embed", mock.uri());

        let result = embed_one_batch(
            &client,
            &url,
            None,
            EmbedRequest { chunks: vec![] },
            0,
            Duration::from_millis(50),
        )
        .await;

        match result {
            Err(EmbedBatchError::BudgetExceeded(_)) => {}
            Err(EmbedBatchError::ConnectFailure(e)) => panic!(
                "a slow-but-connected server must classify as BudgetExceeded, not \
                 ConnectFailure: {e:#}"
            ),
            Err(EmbedBatchError::Saturated(_)) => {
                panic!("a slow-but-connected server must classify as BudgetExceeded, not Saturated")
            }
            Err(EmbedBatchError::Other(e)) => panic!(
                "a slow-but-connected server must classify as BudgetExceeded, not Other: {e:#}"
            ),
            Err(EmbedBatchError::EmbedderDeviceLost(e)) => panic!(
                "a slow-but-connected server must classify as BudgetExceeded, not \
                 EmbedderDeviceLost: {e:#}"
            ),
            Ok(_) => panic!("a response delayed past the timeout must fail"),
        }
    }

    #[tokio::test]
    async fn embed_one_batch_classifies_dual_connect_and_timeout_flags_as_connect_failure() {
        // 192.0.2.1 (TEST-NET-1) is silently dropped, so `connect_timeout` elapses first
        // and the error is both `is_connect()` and `is_timeout()`.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(300))
            .build()
            .unwrap();
        let url = "http://192.0.2.1:81/v1/projects/x/index/embed";

        let result = embed_one_batch(
            &client,
            url,
            None,
            EmbedRequest { chunks: vec![] },
            0,
            Duration::from_secs(5),
        )
        .await;

        match result {
            Err(EmbedBatchError::ConnectFailure(_)) => {}
            Err(EmbedBatchError::BudgetExceeded(e)) => panic!(
                "a connect-phase timeout must classify as ConnectFailure, not \
                 BudgetExceeded, even when the underlying error also satisfies \
                 is_timeout(): {e:#}"
            ),
            Err(EmbedBatchError::Saturated(_)) => {
                panic!("a connect-phase timeout must classify as ConnectFailure, not Saturated")
            }
            Err(EmbedBatchError::Other(e)) => {
                panic!("a connect-phase timeout must classify as ConnectFailure, not Other: {e:#}")
            }
            Err(EmbedBatchError::EmbedderDeviceLost(e)) => panic!(
                "a connect-phase timeout must classify as ConnectFailure, not \
                 EmbedderDeviceLost: {e:#}"
            ),
            Ok(_) => panic!("192.0.2.1 must never actually accept a connection"),
        }
    }

    #[tokio::test]
    async fn embed_one_batch_names_the_fix_when_the_server_rejects_the_credential() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&mock)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/v1/projects/x/index/embed", mock.uri());

        let result = embed_one_batch(
            &client,
            &url,
            Some("stale-key"),
            EmbedRequest { chunks: vec![] },
            0,
            Duration::from_secs(5),
        )
        .await;

        let e = match result {
            Err(EmbedBatchError::Other(e)) => e,
            Err(EmbedBatchError::ConnectFailure(e)) | Err(EmbedBatchError::BudgetExceeded(e)) => {
                panic!("a 401 must surface as Other: {e:#}")
            }
            Err(EmbedBatchError::Saturated(_)) => panic!("a 401 must not classify as Saturated"),
            Err(EmbedBatchError::EmbedderDeviceLost(e)) => {
                panic!("a 401 must surface as Other, not EmbedderDeviceLost: {e:#}")
            }
            Ok(_) => panic!("a 401 must not succeed"),
        };
        let rendered = format!("{e:#}");
        assert!(
            rendered.contains("auth set-key"),
            "a rejected credential must name the command that fixes it, got: {rendered}"
        );
        assert!(
            rendered.contains(&mock.uri()),
            "the hint must name the server origin, got: {rendered}"
        );
        assert!(
            !rendered.contains("index/embed`"),
            "the hint must name the origin, not this endpoint's path, got: {rendered}"
        );
        assert!(
            !rendered.contains("stale-key"),
            "no error path may echo the credential, got: {rendered}"
        );
    }

    #[tokio::test]
    async fn embed_one_batch_classifies_429_as_saturated_with_parsed_retry_after() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "7"))
            .mount(&mock)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/v1/projects/x/index/embed", mock.uri());

        let result = embed_one_batch(
            &client,
            &url,
            None,
            EmbedRequest { chunks: vec![] },
            0,
            Duration::from_secs(5),
        )
        .await;

        match result {
            Err(EmbedBatchError::Saturated(retry_after)) => {
                assert_eq!(
                    retry_after,
                    Duration::from_secs(7),
                    "must parse the server's Retry-After value verbatim"
                );
            }
            Err(EmbedBatchError::BudgetExceeded(e)) => {
                panic!("a 429 must classify as Saturated, not BudgetExceeded: {e:#}")
            }
            Err(EmbedBatchError::ConnectFailure(e)) => {
                panic!("a 429 must classify as Saturated, not ConnectFailure: {e:#}")
            }
            Err(EmbedBatchError::Other(e)) => {
                panic!("a 429 must classify as Saturated, not Other: {e:#}")
            }
            Err(EmbedBatchError::EmbedderDeviceLost(e)) => {
                panic!("a 429 must classify as Saturated, not EmbedderDeviceLost: {e:#}")
            }
            Ok(_) => panic!("a 429 response must not classify as success"),
        }
    }

    #[tokio::test]
    async fn embed_one_batch_defaults_retry_after_when_429_header_is_missing() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&mock)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/v1/projects/x/index/embed", mock.uri());

        let result = embed_one_batch(
            &client,
            &url,
            None,
            EmbedRequest { chunks: vec![] },
            0,
            Duration::from_secs(5),
        )
        .await;

        match result {
            Err(EmbedBatchError::Saturated(retry_after)) => {
                assert_eq!(retry_after, DEFAULT_SATURATION_RETRY);
            }
            Err(EmbedBatchError::BudgetExceeded(e)) => {
                panic!("a 429 must classify as Saturated, not BudgetExceeded: {e:#}")
            }
            Err(EmbedBatchError::ConnectFailure(e)) => {
                panic!("a 429 must classify as Saturated, not ConnectFailure: {e:#}")
            }
            Err(EmbedBatchError::Other(e)) => {
                panic!("a 429 must classify as Saturated, not Other: {e:#}")
            }
            Err(EmbedBatchError::EmbedderDeviceLost(e)) => {
                panic!("a 429 must classify as Saturated, not EmbedderDeviceLost: {e:#}")
            }
            Ok(_) => panic!("a 429 response must not classify as success"),
        }
    }

    #[tokio::test]
    async fn embed_one_batch_classifies_device_lost_503_distinctly_from_a_budget_rejection() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "error": {"code": "embedder_device_lost", "message": "restart to recover"}
            })))
            .mount(&mock)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/v1/projects/x/index/embed", mock.uri());
        let result = embed_one_batch(
            &client,
            &url,
            None,
            EmbedRequest { chunks: vec![] },
            0,
            Duration::from_secs(5),
        )
        .await;

        match result {
            Err(EmbedBatchError::EmbedderDeviceLost(_)) => {}
            Err(EmbedBatchError::BudgetExceeded(e)) => {
                panic!("a device loss must not read as a request-budget rejection: {e:#}")
            }
            Err(EmbedBatchError::Other(e)) => {
                panic!("a device loss must be classified distinctly, not Other: {e:#}")
            }
            Err(EmbedBatchError::ConnectFailure(e)) => {
                panic!("a device loss is not a connect failure: {e:#}")
            }
            Err(EmbedBatchError::Saturated(_)) => panic!("a device loss is not saturation"),
            Ok(_) => panic!("a 503 must not classify as success"),
        }
    }

    #[tokio::test]
    async fn embed_one_batch_treats_a_non_device_lost_503_as_a_generic_failure() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "state": "loading"
            })))
            .mount(&mock)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/v1/projects/x/index/embed", mock.uri());
        let result = embed_one_batch(
            &client,
            &url,
            None,
            EmbedRequest { chunks: vec![] },
            0,
            Duration::from_secs(5),
        )
        .await;

        match result {
            Err(EmbedBatchError::Other(_)) => {}
            Err(EmbedBatchError::EmbedderDeviceLost(e)) => {
                panic!("a warming-up 503 with no device-lost code must not read as a loss: {e:#}")
            }
            Err(EmbedBatchError::BudgetExceeded(e)) => {
                panic!("a generic 503 must classify as Other, not BudgetExceeded: {e:#}")
            }
            Err(EmbedBatchError::ConnectFailure(e)) => {
                panic!("a generic 503 must classify as Other, not ConnectFailure: {e:#}")
            }
            Err(EmbedBatchError::Saturated(_)) => {
                panic!("a generic 503 must classify as Other, not Saturated")
            }
            Ok(_) => panic!("a 503 must not classify as success"),
        }
    }

    fn seed_chunks(n: usize) -> (Database, Vec<i64>) {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open in-memory DB");
        let file_id = db
            .upsert_file("src/lib.rs", Some("rust"), "hash0", 0)
            .unwrap();
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
        let mock = MockServer::start().await;
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
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

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
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(50);
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

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
    async fn chunker_config_drift_warns_but_does_not_block_the_embed_phase() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(5);
        db.ensure_chunker_config("max_chunk_tokens=2048")
            .expect("stamp an old chunker config, as an existing index.db would carry");

        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

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
        .expect("a chunker-config mismatch must not fail the embed phase");

        assert_eq!(embedded, 5, "incremental embedding still proceeds normally");
        assert_eq!(db.stats().unwrap().embedding_count, 5);
        assert_eq!(
            db.chunker_config().unwrap().as_deref(),
            Some("max_chunk_tokens=2048")
        );
    }

    #[tokio::test]
    async fn saturated_429_retries_same_batch_after_retry_after_then_succeeds() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(10);
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

        let cfg = Config::default();
        let tier = server_tier(mock.uri());
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            4,
            &mp,
        )
        .await
        .expect("a transient 429 must not abort the run");

        assert_eq!(
            embedded, 10,
            "every chunk must still get embedded once the admission queue's 429s clear"
        );
        assert_eq!(db.stats().unwrap().embedding_count, 10);
    }

    #[tokio::test]
    async fn saturated_429_gives_up_gracefully_after_max_retries_exhausted() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(3);
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

        let cfg = Config::default();
        let tier = server_tier(mock.uri());
        let mp = MultiProgress::new();

        let embedded = run_embed_phase(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            4,
            &mp,
        )
        .await
        .expect("an always-saturated server must not return Err; it stops gracefully");

        assert_eq!(
            embedded, 0,
            "a permanently-saturated queue must give up after MAX_SATURATION_RETRIES, not hang \
             forever"
        );
    }

    #[tokio::test]
    async fn small_index_below_calibration_size_still_embeds_everything() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .mount(&mock)
            .await;

        for n in [1usize, 2, 3] {
            let (db, ids) = seed_chunks(n);
            let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
                .iter()
                .map(|id| (*id, format!("text {id}"), 3))
                .collect();

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

    #[tokio::test]
    async fn calibration_batch_1_408_is_retried_and_succeeds() {
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
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

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
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(408))
            .mount(&mock)
            .await;

        let (db, ids) = seed_chunks(3);
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

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
                let dim = inkentry_core::embeddings::EMBEDDING_DIM;
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
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

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
                let dim = inkentry_core::embeddings::EMBEDDING_DIM;
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
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

        let cfg = Config::default();
        let limits = ServerLimits {
            embed_request_timeout_secs: Some(1800),
            max_batch_chunks: Some(8),
            embedder_token_cap: None,
            embed_threads: None,
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

    // Real time with a millisecond backoff schedule: paused-time auto-advance races the OS
    // connect refusal, and the request timeout can fire first and misclassify the failure
    // as `BudgetExceeded`.
    const FAST_CONNECT_FAILURE_BACKOFFS: [Duration; 5] = [
        Duration::from_millis(100),
        Duration::from_millis(100),
        Duration::from_millis(100),
        Duration::from_millis(100),
        Duration::from_millis(100),
    ];

    #[tokio::test]
    async fn connect_failure_retries_same_batch_size_then_succeeds() {
        // 150ms falls between the first (100ms) and cumulative second (200ms) backoff, so the
        // mock starts listening after exactly two connect failures.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let listener = std::net::TcpListener::bind(addr).expect("reclaim the released address");
            let mock = MockServer::builder().listener(listener).start().await;
            Mock::given(method("POST"))
                .and(path_regex(r"^/v1/projects/.+/index/embed$"))
                .respond_with(OkEmbedResponder)
                .mount(&mock)
                .await;
            // Keeps `mock` alive.
            std::future::pending::<()>().await
        });

        let (db, ids) = seed_chunks(6);
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

        let cfg = Config::default();
        let tier = server_tier(format!("http://{addr}"));
        let mp = MultiProgress::new();

        let embedded = run_embed_phase_with_backoff(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            64,
            &mp,
            &FAST_CONNECT_FAILURE_BACKOFFS,
        )
        .await
        .expect("connect failures must be retried, not fatal, once the server starts listening");

        assert_eq!(
            embedded, 6,
            "every chunk embeds once the connect failures stop"
        );
        assert_eq!(db.stats().unwrap().embedding_count, 6);
    }

    #[tokio::test]
    async fn connect_failure_exhausts_retries_and_stops_gracefully() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let (db, ids) = seed_chunks(3);
        let chunk_ids_and_texts: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();

        let cfg = Config::default();
        let tier = server_tier(format!("http://{addr}"));
        let mp = MultiProgress::new();

        let embedded = run_embed_phase_with_backoff(
            chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            std::path::Path::new("/tmp/proj"),
            64,
            &mp,
            &FAST_CONNECT_FAILURE_BACKOFFS,
        )
        .await
        .expect("must return Ok(embedded), never Err or hang, once retries are exhausted");

        assert_eq!(
            embedded, 0,
            "nothing embeds when the server is never reachable"
        );
        assert_eq!(db.stats().unwrap().embedding_count, 0);
    }

    #[tokio::test]
    async fn resume_after_interrupted_run_reembeds_the_missing_queue_without_dupes() {
        // Relies on `chunks_missing_embeddings` never re-sending an embedded chunk_id:
        // `INSERT OR REPLACE` does not replace in the vec0 embeddings table.
        let (db, ids) = seed_chunks(6);
        let cfg = Config::default();
        let mp = MultiProgress::new();

        let mock1 = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .up_to_n_times(2)
            .mount(&mock1)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock1)
            .await;

        let queue1: Vec<(i64, String, usize)> = ids
            .iter()
            .map(|id| (*id, format!("text {id}"), 3))
            .collect();
        let embedded1 = run_embed_phase(
            queue1,
            &db,
            &cfg,
            &server_tier(mock1.uri()),
            std::path::Path::new("/tmp/proj"),
            4,
            &mp,
        )
        .await
        .expect("run 1 stops gracefully, not Err");
        assert_eq!(
            embedded1, 5,
            "the 1+4 calibration batches commit; the 500'd batch commits nothing"
        );
        assert_eq!(db.stats().unwrap().embedding_count, 5);

        let missing = db.chunks_missing_embeddings().unwrap();
        assert_eq!(
            missing.len(),
            1,
            "the interrupted batch committed nothing, so exactly the unembedded chunk remains"
        );
        let queue2: Vec<(i64, String, usize)> = missing
            .iter()
            .map(|(id, _name, _meta, _summary, content, tc)| (*id, content.clone(), *tc))
            .collect();

        let mock2 = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(OkEmbedResponder)
            .mount(&mock2)
            .await;
        let embedded2 = run_embed_phase(
            queue2,
            &db,
            &cfg,
            &server_tier(mock2.uri()),
            std::path::Path::new("/tmp/proj"),
            4,
            &mp,
        )
        .await
        .expect("run 2 backfills the remainder");
        assert_eq!(
            embedded2, 1,
            "only the one missing chunk is embedded on the re-run"
        );
        assert_eq!(
            db.stats().unwrap().embedding_count,
            6,
            "all six chunks embedded exactly once — no duplicate row, no lost chunk"
        );
    }

    #[test]
    fn device_lost_body_is_recognised_by_its_code() {
        let body = r#"{"error":{"code":"embedder_device_lost","message":"restart to recover"}}"#;
        assert!(
            response_signals_device_lost(body),
            "the stable code the server sends must be recognised"
        );
    }

    #[test]
    fn budget_generic_and_warmup_bodies_are_not_device_lost() {
        assert!(!response_signals_device_lost(
            r#"{"error":{"code":"internal_error","message":"Internal server error"}}"#
        ));
        assert!(!response_signals_device_lost(r#"{"state":"loading"}"#));
        assert!(!response_signals_device_lost(
            r#"{"error":{"code":"bad_request","message":"too big"}}"#
        ));
        assert!(!response_signals_device_lost("not json at all"));
        assert!(!response_signals_device_lost(""));
    }

    #[test]
    fn request_budget_hint_points_at_batch_size_not_a_restart() {
        let hint = persistent_failure_hint("http://127.0.0.1:4655", StopHint::RequestBudget);
        assert!(
            hint.contains("request budget"),
            "the budget hint must still name the request budget: {hint}"
        );
        assert!(
            !hint.contains("server stop"),
            "the budget hint must not advise a restart: {hint}"
        );
    }

    #[test]
    fn device_lost_hint_points_at_a_server_restart_not_batch_size() {
        let hint = persistent_failure_hint("http://127.0.0.1:4655", StopHint::EmbedderDeviceLost);
        assert!(
            hint.contains("inkentry server stop && inkentry server start"),
            "the device-loss hint must point at a server restart: {hint}"
        );
        assert!(
            !hint.contains("request budget"),
            "the device-loss hint must NOT send the user chasing batch size: {hint}"
        );
    }
}
