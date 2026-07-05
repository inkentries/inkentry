use anyhow::Result;

/// Trait every embedding backend must implement.
///
/// Owned here (not in `spelunk-core`) so this crate stays storage-free: a
/// consumer wanting only the trait depends on `spelunk-embed` with
/// `default-features = false` and pulls in no `rusqlite`/`libsqlite3-sys`.
/// `spelunk-core` re-exports it at `spelunk_core::embeddings::EmbeddingBackend`.
#[async_trait::async_trait]
pub trait EmbeddingBackend: Send + Sync {
    /// Embed a batch of text strings. Returns one vector per input.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// Dimensionality of the output vectors.
    #[allow(dead_code)]
    fn dimension(&self) -> usize;

    /// Per-chunk token truncation cap this backend enforces before embedding a
    /// single input, if any. `None` by default (no known/enforced cap, e.g.
    /// an external OpenAI-compatible embedding server, which truncates or
    /// rejects oversized inputs on its own terms that this process can't see).
    ///
    /// The one concrete backend with a real, host-derived cap is
    /// [`NativeEmbedder`](crate::NativeEmbedder) (see its
    /// `derive_token_cap`/`single_chunk_budget`), which overrides this. It is
    /// surfaced so a client can size a request's *total* token budget
    /// realistically instead of assuming every chunk is small (see
    /// `HealthResponse.limits.embedder_token_cap` in spelunk-server).
    fn token_cap(&self) -> Option<usize> {
        None
    }
}
