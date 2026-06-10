use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use ort::execution_providers::ExecutionProviderDispatch;

/// Embedding dimension produced by [`DEFAULT_MODEL`].
pub const DIM: usize = 768;

/// The bundled model: Nomic Embed Text v1.5 (768-dim, Apache-2.0).
///
/// 768 dimensions matches the server default (`--embedding-dim 768`).
/// EmbeddingGemma 300M uses the same dimension and query prefix convention
/// (`task: code retrieval | query: …`); this model is a drop-in replacement
/// until a native EmbeddingGemma ONNX is available in fastembed's model list.
const DEFAULT_MODEL: EmbeddingModel = EmbeddingModel::NomicEmbedTextV15;

/// ONNX inference batch size passed to fastembed.
///
/// Nomic Embed Text v1.5 (12 layers, 12 heads, seq_len=512) allocates
/// roughly batch × heads × seq² × 4 bytes of attention matrices per layer.
/// At fastembed's default of 256, a full MAX_BATCH request produces ~3 GB of
/// attention tensors per layer.
///
/// Hardware EPs (CoreML, XNNPACK, DirectML) pre-allocate activation buffers
/// sized to the first inference's batch dimension and hold them for the session
/// lifetime — ORT's `disable_memory_pattern` does not reach these EP-private
/// buffers.  A smaller batch keeps the persistent EP footprint manageable;
/// throughput impact is negligible because hardware EPs are much faster per
/// call than the CPU path.
///
/// CPU path: batch=32 (~500 MB peak, good CPU throughput)
/// Hardware EP path: batch=8 (~125 MB peak activation footprint per call)
const ONNX_BATCH_SIZE: usize = if cfg!(any(
    all(target_vendor = "apple", feature = "embed-coreml"),
    all(target_os = "linux", feature = "embed-xnnpack"),
    all(windows, feature = "embed-directml"),
)) {
    8
} else {
    32
};

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
    ///
    /// `threads` caps ONNX intra-op parallelism (default 4 via `--embed-threads`).
    pub fn load(threads: usize) -> Result<Self> {
        let cache_dir = model_cache_dir()?;
        std::fs::create_dir_all(&cache_dir)
            .with_context(|| format!("creating model cache dir {}", cache_dir.display()))?;

        let ep_name = active_ep_name();
        tracing::info!(
            "loading native embedding model (cache: {}, threads: {threads}, ep: {ep_name}, \
             onnx_batch: {ONNX_BATCH_SIZE})",
            cache_dir.display()
        );

        let model = TextEmbedding::try_new(
            InitOptions::new(DEFAULT_MODEL)
                .with_cache_dir(cache_dir.clone())
                .with_show_download_progress(true)
                .with_intra_threads(threads)
                .with_disable_memory_pattern(true)
                .with_execution_providers(platform_execution_providers(&cache_dir)),
        )
        .context("initialising native embedding model")?;

        Ok(Self {
            model: Arc::new(Mutex::new(model)),
        })
    }
}

/// Returns the hardware-accelerated EP for this platform, if the matching
/// feature flag was enabled at compile time. Falls back to the ORT CPU EP
/// (the ort default) when no feature is active.
///
/// `cache_dir` is used as the CoreML compiled-model cache so that the model is
/// not recompiled (holding both ONNX and CoreML representations in RAM
/// simultaneously) on every server restart.
fn platform_execution_providers(cache_dir: &std::path::Path) -> Vec<ExecutionProviderDispatch> {
    if cfg!(all(target_vendor = "apple", feature = "embed-coreml")) {
        // Cache the compiled CoreML model alongside the ONNX weights.
        // Without this CoreML recompiles the model in-memory on every server
        // start, temporarily holding the ONNX graph AND the compiled form.
        let coreml_cache = cache_dir.join("coreml");
        vec![
            ort::ep::CoreML::default()
                .with_model_cache_dir(coreml_cache.to_string_lossy())
                .build(),
        ]
    } else if cfg!(all(target_os = "linux", feature = "embed-xnnpack")) {
        vec![ort::ep::XNNPACK::default().build()]
    } else if cfg!(all(windows, feature = "embed-directml")) {
        vec![ort::ep::DirectML::default().build()]
    } else {
        vec![]
    }
}

const fn active_ep_name() -> &'static str {
    if cfg!(all(target_vendor = "apple", feature = "embed-coreml")) {
        "CoreML"
    } else if cfg!(all(target_os = "linux", feature = "embed-xnnpack")) {
        "XNNPACK"
    } else if cfg!(all(windows, feature = "embed-directml")) {
        "DirectML"
    } else {
        "CPU"
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
            m.embed(refs, Some(ONNX_BATCH_SIZE))
                .context("native embedding model inference failed")
        })
        .await
        .context("spawn_blocking panicked in native embedder")?
    }

    fn dimension(&self) -> usize {
        DIM
    }
}
