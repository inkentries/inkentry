//! llama.cpp-backed embedder for the same F2LLM-v2-330M model the candle
//! engine runs.
//!
//! Why a second engine: candle has no cross-vendor GPU path on Windows or
//! Linux, so every non-Mac user runs CPU-only. llama.cpp's Vulkan backend
//! covers NVIDIA/AMD/Intel there with a single binary (`llama-vulkan`
//! feature); its Metal build doubles as the parity/bench reference on macOS.
//! Cross-engine vector drift was measured equal to the already-shipped candle
//! CPU↔Metal drift, which is what lets both engines serve one `MODEL_ID`
//! without a re-index.
//!
//! Loads the *canonical* llama.cpp GGUF (`blk.N.*` tensor names, tokenizer
//! and last-token pooling baked into metadata) — NOT the HF-named GGUF the
//! candle loader reads. The two artifacts coexist in the same model repo and
//! cache; `inkentry-server`'s `embed_hub` resolves the right file per engine.

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::token::LlamaToken;

use crate::error::EmbedError;
use crate::vector::l2_normalise;

/// Context sizes tried at load, largest first. A sequence is truncated to the
/// resolved rung and must fit one micro-batch, so the rung IS the token cap.
/// 8192 matches the candle engine's large-RAM cap; the lower rungs keep
/// small-VRAM GPUs loadable at reduced chunk length.
const UBATCH_LADDER: [u32; 3] = [8192, 4096, 2048];

/// Where the caller wants inference to run. `Auto` and `Gpu` both offload the
/// whole model — llama.cpp itself degrades to CPU buffers when no GPU
/// backend/driver is usable — while `Cpu` forces zero offloaded layers. The
/// two GPU-ish variants exist because the *factory* treats them differently
/// (`Auto` may pick a different engine entirely); by the time a request
/// reaches this engine they act the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRequest {
    Auto,
    Gpu,
    Cpu,
}

impl std::str::FromStr for DeviceRequest {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "gpu" => Ok(Self::Gpu),
            "cpu" => Ok(Self::Cpu),
            other => Err(anyhow::anyhow!(
                "invalid embed device {other:?} (expected auto, gpu, or cpu)"
            )),
        }
    }
}

/// Process-wide llama.cpp backend handle. ggml's backend registry is global
/// state that may be initialised exactly once per process; the handle lives in
/// a static so it is never dropped out from under a second embedder instance.
fn backend() -> Result<&'static LlamaBackend> {
    static BACKEND: OnceLock<std::result::Result<LlamaBackend, String>> = OnceLock::new();
    BACKEND
        .get_or_init(|| {
            // llama.cpp logs straight to stderr by default (very chatty at
            // model load); route it into `tracing` with everything else.
            llama_cpp_2::send_logs_to_tracing(llama_cpp_2::LogOptions::default());
            #[cfg(feature = "llama-vulkan")]
            load_backend_modules();
            LlamaBackend::init().map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!("initialising llama.cpp backend: {e}"))
}

/// With `dynamic-backends` (the `llama-vulkan` build) every ggml backend —
/// Vulkan *and* the CPU-SIMD variants — is a runtime-loaded module, and
/// nothing loads them implicitly: skipping this leaves the registry empty and
/// every model load failing. `GGML_BACKEND_PATH` is the operator override;
/// otherwise the two shipped layouts below are probed, then the compile-time
/// build-tree dir that covers `cargo run`/tests. A module whose driver is
/// missing (no Vulkan) simply fails to load, which is the graceful CPU
/// degrade this build exists for.
#[cfg(feature = "llama-vulkan")]
fn load_backend_modules() {
    use llama_cpp_2::llama_backend::{load_backends, load_backends_from_path};

    if let Ok(dir) = std::env::var("GGML_BACKEND_PATH") {
        tracing::info!("loading ggml backend modules from GGML_BACKEND_PATH ({dir})");
        load_backends_from_path(std::path::Path::new(&dir));
        return;
    }
    // Two shipped layouts: archives are flat (modules beside the binary);
    // the .deb splits them (/usr/bin + /usr/lib/inkentry, matching the
    // binary's $ORIGIN/../lib/inkentry rpath for its core libs).
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| {
        p.parent().and_then(|d| {
            [d.to_path_buf(), d.join("../lib/inkentry")]
                .into_iter()
                .find(|c| dir_has_ggml_modules(c))
        })
    }) {
        tracing::info!("loading ggml backend modules from {}", exe_dir.display());
        load_backends_from_path(&exe_dir);
        return;
    }
    load_backends();
}

/// Module filenames are `libggml-<backend>.so` on unix (macOS included) and
/// `ggml-<backend>.dll` on Windows, with `<backend>` varying by build
/// (vulkan, cpu-haswell, cpu-apple_m1, …) — so probe by prefix, not name.
#[cfg(feature = "llama-vulkan")]
fn dir_has_ggml_modules(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("libggml-") || n.starts_with("ggml-"))
        })
    })
}

/// Name of the first registered GPU-class backend, if any. Resolved from the
/// live ggml registry rather than compile-time features: with runtime-loaded
/// modules the Vulkan module can be absent or driverless, and `/v1/health`
/// must not claim a device that isn't actually serving.
fn first_gpu_backend() -> Option<&'static str> {
    use llama_cpp_2::LlamaBackendDeviceType;
    llama_cpp_2::list_llama_ggml_backend_devices()
        .into_iter()
        .find(|d| {
            matches!(
                d.device_type,
                LlamaBackendDeviceType::Gpu | LlamaBackendDeviceType::IntegratedGpu
            )
        })
        .map(|d| match d.backend.as_str() {
            "Vulkan" => "vulkan",
            // ggml's Metal backend registers under "MTL".
            "MTL" | "Metal" => "metal",
            _ => "gpu",
        })
}

fn context_params(ubatch: u32, n_threads: i32) -> LlamaContextParams {
    // Pooling is set explicitly rather than trusting the GGUF's baked
    // `pooling_type` metadata: with last-token pooling the identity of the
    // pooled token is the entire vector-space contract.
    LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(ubatch))
        .with_n_batch(ubatch)
        .with_n_ubatch(ubatch)
        .with_n_seq_max(1)
        .with_embeddings(true)
        .with_pooling_type(LlamaPoolingType::Last)
        .with_n_threads(n_threads)
        .with_n_threads_batch(n_threads)
}

pub struct LlamaEmbedder {
    model: Arc<LlamaModel>,
    dim: usize,
    token_cap: usize,
    n_threads: i32,
    device: &'static str,
}

impl LlamaEmbedder {
    /// Load the F2LLM embedder from a canonical llama.cpp GGUF already on
    /// disk, with zero network access — the tokenizer and model config travel
    /// inside the GGUF, so unlike the candle loader this takes one file.
    ///
    /// `threads` caps llama.cpp's per-context CPU threadpool; `None` uses all
    /// available parallelism.
    pub fn load_from_path(
        gguf_path: &Path,
        device: DeviceRequest,
        threads: Option<usize>,
    ) -> Result<Self> {
        anyhow::ensure!(
            gguf_path.exists(),
            "GGUF file not found: {}",
            gguf_path.display()
        );

        let backend = backend()?;

        let (n_gpu_layers, device_name) = match (device, first_gpu_backend()) {
            (DeviceRequest::Cpu, _) => (0, "cpu"),
            (_, Some(flavor)) => (1000, flavor),
            (_, None) => {
                tracing::warn!(
                    "GPU embedding requested but no GPU backend is available (module \
                     missing, no usable driver, or built without one); running on CPU"
                );
                (0, "cpu")
            }
        };

        tracing::info!(
            "loading F2LLM-v2-330M (Q8_0) via llama.cpp on {device_name} ({})",
            gguf_path.display()
        );

        let params = LlamaModelParams::default().with_n_gpu_layers(n_gpu_layers);
        let model = LlamaModel::load_from_file(backend, gguf_path, &params)
            .with_context(|| format!("loading llama.cpp GGUF {}", gguf_path.display()))?;

        let dim = usize::try_from(model.n_embd()).context("model reports negative n_embd")?;

        let n_threads = i32::try_from(threads.unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
        }))
        .unwrap_or(i32::MAX);

        let token_cap = probe_ubatch(&model, backend, n_threads)?;

        tracing::info!(
            "F2LLM-v2-330M ready (dim={dim}, Q8_0, engine=llama, device={device_name}); \
             token cap {token_cap}"
        );

        Ok(Self {
            model: Arc::new(model),
            dim,
            token_cap: token_cap as usize,
            n_threads,
            device: device_name,
        })
    }

    /// The resolved inference device, for logs and `/v1/health` (`"cpu"`,
    /// `"metal"`, `"vulkan"`, or `"gpu"` for any other GPU-class backend).
    pub fn device(&self) -> &'static str {
        self.device
    }
}

/// Find the largest ladder rung whose context allocates on this device.
/// Context creation is where llama.cpp reserves KV-cache and compute buffers,
/// so a failed rung means "this ubatch does not fit" and the next is tried.
fn probe_ubatch(model: &LlamaModel, backend: &LlamaBackend, n_threads: i32) -> Result<u32> {
    let mut last_err = None;
    for ubatch in UBATCH_LADDER {
        match model.new_context(backend, context_params(ubatch, n_threads)) {
            Ok(_ctx) => {
                if last_err.is_some() {
                    tracing::warn!(
                        "llama context stepped down to ubatch {ubatch} — chunks longer than \
                         {ubatch} tokens will be truncated for embedding on this device"
                    );
                }
                return Ok(ubatch);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(anyhow::anyhow!(
        "no llama context size in {UBATCH_LADDER:?} fits this device: {}",
        // The loop body ran at least once, so the ladder being non-empty
        // guarantees an error is recorded here.
        last_err.map_or_else(String::new, |e| e.to_string())
    ))
}

#[async_trait::async_trait]
impl crate::EmbeddingBackend for LlamaEmbedder {
    /// Embed a batch of strings with no way to cancel early. Delegates to
    /// [`Self::embed_with_cancel`] with a flag that's never set, so there is
    /// exactly one implementation of the tokenize/decode loop.
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.embed_with_cancel(texts, Arc::new(AtomicBool::new(false)))
            .await
    }

    /// Embed a batch of strings via llama.cpp, stopping early if `cancel` is
    /// observed set.
    ///
    /// Each text is tokenized with llama.cpp's own tokenizer (byte-identical
    /// to the HF tokenizer for this model, verified including the appended
    /// EOS), truncated to the token cap, then decoded one sequence at a time
    /// with last-token pooling; the pooled vector is L2-normalised. `cancel`
    /// is checked before starting and between sequences, bounding waste to
    /// one chunk's forward pass — the same granularity as the candle engine's
    /// sequential path.
    ///
    /// Unlike the candle engine there is no interior mutex: the model is
    /// immutable and each call decodes in its own context, so concurrent
    /// calls serialise only on compute capacity.
    async fn embed_with_cancel(
        &self,
        texts: &[&str],
        cancel: Arc<AtomicBool>,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        let total = texts.len();
        if total == 0 {
            return Ok(Vec::new());
        }
        let owned: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        let model = Arc::clone(&self.model);
        let token_cap = self.token_cap;
        let n_threads = self.n_threads;

        tokio::task::spawn_blocking(move || {
            if cancel.load(Ordering::Relaxed) {
                tracing::info!(
                    "embed batch abandoned before starting (0/{total} chunks completed)"
                );
                return Err(EmbedError::Cancelled {
                    completed: 0,
                    total,
                }
                .into());
            }

            // Tokenize everything upfront so bad input fails before any
            // compute buffer is allocated.
            let eos = model.token_eos();
            let mut token_lists: Vec<Vec<LlamaToken>> = Vec::with_capacity(total);
            for text in &owned {
                // llama.cpp tokenizes through a C string, which cannot carry
                // interior NUL bytes (the HF tokenizer on the candle path
                // can); such input surfaces as a Tokenization error rather
                // than silently embedding different text.
                let mut toks = model
                    .str_to_token(text, AddBos::Never)
                    .map_err(|e| EmbedError::Tokenization(e.to_string()))?;
                // Mirror the HF post-processor exactly: EOS appended first,
                // then the cap applied — a truncated chunk loses its EOS on
                // both engines alike.
                toks.push(eos);
                if toks.len() > token_cap {
                    tracing::warn!(
                        "chunk truncated for embedding: {} tokens > cap {token_cap} \
                         (ubatch limit) — embedding leading {token_cap} tokens only",
                        toks.len()
                    );
                    toks.truncate(token_cap);
                }
                token_lists.push(toks);
            }

            let backend = backend()?;
            let ubatch = u32::try_from(token_cap).context("token cap exceeds u32")?;
            let mut ctx = model
                .new_context(backend, context_params(ubatch, n_threads))
                .map_err(|e| EmbedError::Inference(format!("creating llama context: {e}")))?;
            let mut batch = LlamaBatch::new(token_cap, 1);

            let mut out = Vec::with_capacity(total);
            for (i, toks) in token_lists.iter().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    tracing::info!("embed batch cancelled after {i}/{total} chunks");
                    return Err(EmbedError::Cancelled {
                        completed: i,
                        total,
                    }
                    .into());
                }
                batch.clear();
                batch
                    .add_sequence(toks, 0, false)
                    .map_err(|e| EmbedError::Inference(format!("batching chunk {i}: {e}")))?;
                ctx.clear_kv_cache();
                ctx.decode(&mut batch)
                    .map_err(|e| EmbedError::Inference(format!("llama decode: {e}")))?;
                let mut v = ctx
                    .embeddings_seq_ith(0)
                    .map_err(|e| EmbedError::Inference(format!("reading pooled embedding: {e}")))?
                    .to_vec();
                l2_normalise(&mut v);
                anyhow::ensure!(
                    v.iter().all(|x| x.is_finite()),
                    "non-finite embedding value from llama engine"
                );
                out.push(v);
            }
            Ok(out)
        })
        .await
        .context("spawn_blocking panicked in llama embedder")?
    }

    fn dimension(&self) -> usize {
        self.dim
    }

    /// Always `Some`: the cap is the resolved ubatch size, fixed at load.
    fn token_cap(&self) -> Option<usize> {
        Some(self.token_cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_request_parses_the_three_documented_values() {
        assert_eq!(
            "auto".parse::<DeviceRequest>().unwrap(),
            DeviceRequest::Auto
        );
        assert_eq!(
            " GPU ".parse::<DeviceRequest>().unwrap(),
            DeviceRequest::Gpu
        );
        assert_eq!("cpu".parse::<DeviceRequest>().unwrap(), DeviceRequest::Cpu);
        assert!("metal".parse::<DeviceRequest>().is_err());
    }

    #[test]
    fn ubatch_ladder_is_strictly_descending() {
        assert!(UBATCH_LADDER.windows(2).all(|w| w[0] > w[1]));
    }

    #[test]
    fn load_from_missing_gguf_errors_without_network() {
        let err = match LlamaEmbedder::load_from_path(
            Path::new("/nonexistent/model.gguf"),
            DeviceRequest::Cpu,
            None,
        ) {
            Ok(_) => panic!("load of a nonexistent GGUF must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("GGUF file not found"));
    }
}
