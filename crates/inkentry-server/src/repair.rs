//! Background repair of memory rows that were stored without a vector.
//!
//! A memory write never fails just because no vector could be produced: the
//! entry is stored text-only and the caller is told so (`embedded: false` on
//! the batch result). Without this module that state was permanent, since
//! `note_embeddings` was only ever written at insert time. Here the server
//! reads those rows back, embeds their stored text, and fills the gap.
//!
//! The pass is driven entirely by signals, never by a timer. Two edges raise
//! one: the embedder becoming ready, and any request that stores a row without
//! a vector. A server with nothing to repair therefore does no work and
//! schedules no wakeups.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::AppState;

/// Cheap-clone wakeup handle for the repair worker.
///
/// Raises coalesce: [`Notify::notify_one`] stores at most one permit, so N
/// concurrent degraded writes wake the worker at most once more than it was
/// already going to wake. A raise while a sweep is running is not lost, it is
/// held and consumed by the next [`RepairSignal::wait`].
#[derive(Clone, Default)]
pub struct RepairSignal(Arc<Notify>);

impl RepairSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask for a repair sweep. Never blocks and never fails, so a write path
    /// can call it while holding whatever it happens to hold.
    pub fn raise(&self) {
        self.0.notify_one();
    }

    /// Wait for the next request for a sweep, consuming one pending raise.
    pub async fn wait(&self) {
        self.0.notified().await;
    }
}

/// What one sweep did. Returned rather than only logged so the pass is
/// assertable without reading log output.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RepairStats {
    /// Vectorless rows read and attempted.
    pub scanned: usize,
    /// Rows that gained a `note_embeddings` row.
    pub repaired: usize,
    /// Rows whose text the embedder refused even one at a time. Left for a
    /// later sweep; a poisonous text costs its own vector and no other.
    pub failed: usize,
    /// Whether the sweep ended before exhausting the backlog because no
    /// progress was possible: the embedder is not ready, or the request path
    /// has taken every admission permit.
    pub stopped_early: bool,
}

/// How many vectorless rows one sweep reads, embeds and writes back at a time.
pub const REPAIR_PAGE_SIZE: usize = 32;

/// Shortest interval between two sweeps. Purely a floor on signalled work: it
/// delays a sweep a signal already asked for and never schedules one, so an
/// idle server still has no wakeups. Without it a sustained embed outage would
/// turn every failing write into its own full scan of the backlog.
pub const REPAIR_DEBOUNCE: Duration = Duration::from_secs(5);

/// Sweep whenever asked, and never otherwise. Runs for the life of the
/// process; `main` does nothing but spawn it.
///
/// There is deliberately no interval here. Every sweep is the direct
/// consequence of an edge that produced repairable state: the embedder became
/// ready, or a write stored a row without a vector. A sweep that finds nothing
/// or fails does not ask for another, so the loop cannot sustain itself and a
/// server with nothing to repair parks on [`RepairSignal::wait`] indefinitely.
pub async fn run_repair_worker(state: AppState, page_size: usize, debounce: Duration) {
    if state.embedder.state() == crate::EmbedderState::Disabled {
        // Terminal, unlike `loading` and `unavailable`: `set_ready` is only
        // ever reached from the native load path, which does not run in a
        // build with no embedder. There is no transition to wait for, so the
        // worker is inert rather than idle.
        return;
    }

    let mut last_attempt: Option<Instant> = None;
    loop {
        state.repair_signal.wait().await;

        if let Some(prev) = last_attempt
            && let Some(remaining) = debounce.checked_sub(prev.elapsed())
        {
            tokio::time::sleep(remaining).await;
        }
        last_attempt = Some(Instant::now());

        match repair_missing_embeddings(&state, page_size).await {
            Ok(stats) if stats.repaired > 0 || stats.failed > 0 => {
                tracing::info!(
                    "memory vector repair: {} repaired, {} still unembeddable",
                    stats.repaired,
                    stats.failed,
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("memory vector repair pass ended: {e}"),
        }
    }
}

/// Embed and store a vector for every active memory row that has none.
///
/// Server-wide by design: there is one `note_embeddings` table at one
/// configured dimension behind one embedder, so a row's project has no bearing
/// on whether it can be repaired.
///
/// Every page is read, embedded and written as three separate steps, because
/// the [`crate::handlers`] storage embed helper documents two invariants that
/// the compiler cannot hold and that call sites have broken silently before:
/// the global `ServerDb` guard is never alive across an embed await, and the
/// embed runs under an admission permit that is released between pages.
pub async fn repair_missing_embeddings(
    state: &AppState,
    page_size: usize,
) -> anyhow::Result<RepairStats> {
    let mut stats = RepairStats::default();
    let page_size = page_size.max(1);
    // Advances past every row read, repaired or not. A row the embedder
    // refuses stays a candidate, so without this the next page would return it
    // again and the sweep would never end. See `notes_missing_embeddings`.
    let mut cursor = 0i64;

    loop {
        let page = {
            let db = state.db.lock().await;
            db.notes_missing_embeddings(cursor, page_size)?
        };
        if page.is_empty() {
            return Ok(stats);
        }
        cursor = page.last().expect("non-empty page").rowid;

        let vectors = match embed_page(state, &page).await {
            PageOutcome::Vectors(v) => v,
            PageOutcome::Stop => {
                stats.stopped_early = true;
                return Ok(stats);
            }
        };
        stats.scanned += page.len();

        let db = state.db.lock().await;
        for (note, vector) in page.iter().zip(vectors) {
            match vector {
                Some(v) if db.insert_embedding_if_missing(note.rowid, &v)? => stats.repaired += 1,
                Some(_) => {}
                None => stats.failed += 1,
            }
        }
        drop(db);

        // The native embedder is serialized behind one mutex, so a large
        // backlog must give interactive search traffic a turn between pages
        // rather than queuing every page back to back.
        tokio::task::yield_now().await;
    }
}

enum PageOutcome {
    /// One slot per input row: `None` where the embedder refused that row's
    /// text even on its own.
    Vectors(Vec<Option<Vec<f32>>>),
    /// No progress is possible right now. Not an error: the embedder is not
    /// ready, or the request path holds every admission permit and the sweep
    /// must not compete with it.
    Stop,
}

async fn embed_page(state: &AppState, page: &[crate::db::VectorlessNote]) -> PageOutcome {
    let texts: Vec<String> = page
        .iter()
        .map(|n| crate::handlers::storage_embedding_text(&n.title, &n.body))
        .collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();

    match crate::handlers::embed_for_storage(state, &refs).await {
        Ok(crate::handlers::StorageEmbedding::Vectors(vectors)) => {
            PageOutcome::Vectors(vectors.into_iter().map(Some).collect())
        }
        Ok(crate::handlers::StorageEmbedding::NotReady) => PageOutcome::Stop,
        Ok(crate::handlers::StorageEmbedding::Failed) => {
            // One text in this page is poison and the batched call cannot say
            // which. Retrying row by row costs the vector of the offending row
            // and no other, which is what keeps a single bad text from costing
            // a whole push worth of vectors.
            tracing::debug!(
                "batched repair embed failed for {} rows, retrying one at a time",
                page.len()
            );
            let mut out = Vec::with_capacity(refs.len());
            for text in &refs {
                match crate::handlers::embed_for_storage(state, &[text]).await {
                    Ok(crate::handlers::StorageEmbedding::Vectors(mut v)) => out.push(v.pop()),
                    Ok(crate::handlers::StorageEmbedding::Failed) => out.push(None),
                    Ok(crate::handlers::StorageEmbedding::NotReady) | Err(_) => {
                        return PageOutcome::Stop;
                    }
                }
            }
            PageOutcome::Vectors(out)
        }
        // The only error `embed_for_storage` produces is a full admission
        // queue, which means the request path is saturated. Backing off is the
        // correct response, not a failure to report.
        Err(_busy) => PageOutcome::Stop,
    }
}
