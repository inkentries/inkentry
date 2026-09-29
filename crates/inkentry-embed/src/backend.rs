use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;

/// Which admission lane an embed request runs on.
///
/// A person waiting on a `search` or `memory add` embed is `Interactive`; a
/// background index pass, a memory batch push, or the vectorless-repair sweep is
/// `Bulk`. The two lanes have separate admission slots and separate warm
/// contexts, so an interactive embed is never shed nor left waiting behind a
/// bulk index batch. The distinction is the caller's intent, not the request's
/// size: the repair worker's per-row fallback carries a single text yet stays
/// `Bulk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedLane {
    Interactive,
    Bulk,
}

/// Trait every embedding backend must implement.
///
/// Owned here (not in `inkentry-core`) so this crate stays storage-free: a
/// consumer wanting only the trait depends on `inkentry-embed` with
/// `default-features = false` and pulls in no `rusqlite`/`libsqlite3-sys`.
/// `inkentry-core` re-exports it at `inkentry_core::embeddings::EmbeddingBackend`.
#[async_trait::async_trait]
pub trait EmbeddingBackend: Send + Sync {
    /// Embed a batch of text strings. Returns one vector per input.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// Embed a batch, checking `cancel` cooperatively and stopping early
    /// (returning an error) once it is set. Default delegates to [`Self::embed`]
    /// and ignores `cancel` — correct for any backend whose own work already
    /// cancels on future drop (e.g. a pure-async HTTP shim). The one backend
    /// whose work does not stop on drop is [`LlamaEmbedder`](crate::LlamaEmbedder),
    /// which runs its forward passes on a pool of worker threads and overrides
    /// this method to check the flag between chunks, so an abandoned request
    /// doesn't keep computing to completion.
    async fn embed_with_cancel(
        &self,
        texts: &[&str],
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<Vec<f32>>> {
        let _ = cancel;
        self.embed(texts).await
    }

    /// Embed a batch on a declared admission [`EmbedLane`], checking `cancel`
    /// cooperatively. The default ignores the lane and delegates to
    /// [`Self::embed_with_cancel`]: correct for any backend with no lane
    /// structure of its own (an external HTTP shim, a test double). The one
    /// backend that routes by lane is [`LlamaEmbedder`](crate::LlamaEmbedder),
    /// which keeps a warm context reserved for each lane so an interactive embed
    /// runs at once rather than waiting on a bulk decode.
    async fn embed_lane(
        &self,
        texts: &[&str],
        cancel: Arc<AtomicBool>,
        lane: EmbedLane,
    ) -> Result<Vec<Vec<f32>>> {
        let _ = lane;
        self.embed_with_cancel(texts, cancel).await
    }

    /// Dimensionality of the output vectors.
    fn dimension(&self) -> usize;

    /// Per-chunk token truncation cap this backend enforces before embedding a
    /// single input, if any. `None` by default (no known or enforced cap, e.g.
    /// an external OpenAI-compatible embedding server that truncates or rejects
    /// oversized inputs on its own terms this process can't see).
    ///
    /// The one concrete backend with a real, fixed cap is
    /// [`LlamaEmbedder`](crate::LlamaEmbedder), which overrides this so a
    /// client can size a request's total token budget realistically instead
    /// of assuming every chunk is small.
    fn token_cap(&self) -> Option<usize> {
        None
    }
}
