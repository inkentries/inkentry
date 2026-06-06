use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

/// Embedding dimension produced by [`DEFAULT_MODEL`].
pub const DIM: usize = 768;

/// The bundled model: Nomic Embed Text v1.5 (768-dim, Apache-2.0).
///
/// 768 dimensions matches the server default (`--embedding-dim 768`).
/// EmbeddingGemma 300M uses the same dimension and query prefix convention
/// (`task: code retrieval | query: …`); this model is a drop-in replacement
/// until a native EmbeddingGemma ONNX is available in fastembed's model list.
const DEFAULT_MODEL: EmbeddingModel = EmbeddingModel::NomicEmbedTextV15;

pub struct NativeEmbedder {
    // Mutex because embed() takes &mut self.
    model: Arc<Mutex<TextEmbedding>>,
}

impl NativeEmbedder {
    /// Load (or download) the native embedding model.
    ///
    /// On first call, fastembed downloads the ONNX weights (~150 MB) into
    /// `~/.local/share/spelunk/models/` and caches them for subsequent runs.
    /// A progress bar is printed to stderr during the download.
    pub fn load() -> Result<Self> {
        let cache_dir = model_cache_dir()?;
        std::fs::create_dir_all(&cache_dir)
            .with_context(|| format!("creating model cache dir {}", cache_dir.display()))?;

        tracing::info!(
            "loading native embedding model (cache: {})",
            cache_dir.display()
        );

        let model = TextEmbedding::try_new(
            InitOptions::new(DEFAULT_MODEL)
                .with_cache_dir(cache_dir)
                .with_show_download_progress(true),
        )
        .context("initialising native embedding model")?;

        Ok(Self {
            model: Arc::new(Mutex::new(model)),
        })
    }
}

fn model_cache_dir() -> Result<PathBuf> {
    dirs::data_local_dir()
        .map(|d| d.join("spelunk").join("models"))
        .ok_or_else(|| anyhow::anyhow!("could not determine local data directory"))
}

#[async_trait::async_trait]
impl spelunk_core::embeddings::EmbeddingBackend for NativeEmbedder {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let model = Arc::clone(&self.model);
        let owned: Vec<String> = texts.iter().map(|s| s.to_string()).collect();

        tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            let mut m = model
                .lock()
                .map_err(|_| anyhow::anyhow!("native embedder lock poisoned"))?;
            m.embed(refs, None)
                .context("native embedding model inference failed")
        })
        .await
        .context("spawn_blocking panicked in native embedder")?
    }

    fn dimension(&self) -> usize {
        DIM
    }
}
