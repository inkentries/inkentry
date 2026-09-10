//! llama.cpp-backed embedder for the F2LLM-v2-330M model — inkentry's sole
//! embedding engine.
//!
//! llama.cpp's Vulkan backend gives NVIDIA/AMD/Intel GPUs a single cross-vendor
//! binary on Windows/Linux (`llama-vulkan` feature); its Metal build serves
//! macOS (`llama-metal`); the bare feature runs on CPU everywhere else.
//!
//! Loads the canonical llama.cpp GGUF (`blk.N.*` tensor names, tokenizer and
//! last-token pooling baked into metadata), 896-dim, Q8_0-quantized, under a
//! fixed `MODEL_ID`. `inkentry-server`'s `embed_hub` resolves the file.

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::token::LlamaToken;
use tokio::sync::oneshot;

use crate::EmbedLane;
use crate::error::EmbedError;
use crate::vector::l2_normalise;

/// The one llama context size the embedder runs at on every machine. A sequence
/// longer than this is truncated to it, so it IS the token cap. Fixing it makes
/// that truncation happen at the same token boundary on every host: a per-machine
/// size embedded the same source to different vectors depending on the machine's
/// RAM, which diverges once those vectors reach a shared team/cloud store. A
/// machine that cannot allocate a context this size is refused at load rather
/// than stepped down to a smaller one (see [`ensure_embed_context_fits`]).
const EMBED_UBATCH: u32 = 8192;

/// Default bulk-lane worker count for direct callers (e.g. the `embed_bench`
/// example). The server passes its own admission capacities to
/// [`LlamaEmbedder::load_from_path`], so the pool and the admission gate never
/// drift and every admitted concurrent embed finds its own warm context.
/// Independent contexts, NOT one shared behind a mutex — a shared context would
/// serialize every embed and reintroduce the interactive-embed starvation
/// ADR-096 removes.
pub const DEFAULT_EMBED_POOL_SIZE: usize = 2;

/// A bulk-lane (and non-primary interactive) context is dropped after this long
/// with no work, so its ~5.5 GiB of reserved address space is paid only while a
/// pass is actually running, not held between passes (ADR-096 §4 memory budget).
/// During an active index, batches arrive far faster than this so the context
/// stays hot for the pass. The one persistently-hot interactive context is
/// exempt (see [`worker_idle_timeout`]): it is never idle-evicted, so the first
/// `search`/`memory add` after any lull skips the ~2 s cold-context build.
const CONTEXT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

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
/// `libggml-base` is the core library, not a runtime-loaded backend module, and
/// shares the `libggml-` prefix, so it is excluded: a directory holding only
/// core libs has no backend to load and must not be selected.
#[cfg(feature = "llama-vulkan")]
fn dir_has_ggml_modules(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.file_name().to_str().is_some_and(is_ggml_backend_module))
    })
}

/// A ggml backend-module filename (not a core lib). See [`dir_has_ggml_modules`].
#[cfg(feature = "llama-vulkan")]
fn is_ggml_backend_module(name: &str) -> bool {
    (name.starts_with("libggml-") || name.starts_with("ggml-"))
        && !name.starts_with("libggml-base")
        && !name.starts_with("ggml-base")
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
    // pooled token is the entire vector-space contract. `n_seq_max(1)`: one
    // chunk is decoded per forward pass, the KV cache cleared between chunks.
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

/// The outcome of one embed request, sent back over the job's reply channel.
type EmbedResult = std::result::Result<Vec<Vec<f32>>, anyhow::Error>;

/// One embed request handed to a pool worker. Owns its inputs so it can cross
/// the thread boundary; the reply travels back over a `oneshot` the awaiting
/// [`LlamaEmbedder::embed_with_cancel`] holds.
struct Job {
    texts: Vec<String>,
    cancel: Arc<AtomicBool>,
    reply: oneshot::Sender<EmbedResult>,
}

/// One lane's worker threads: senders to reach them, per-worker "busy" flags,
/// and the rotation counter used once every worker is busy. A `LlamaContext`
/// borrows its `&LlamaModel`, so it cannot be stored beside the
/// `Arc<LlamaModel>` in a struct (self-referential) — but a worker thread can
/// hold both on its own stack for its whole life and decode job after job
/// against the same warm context.
///
/// [`claim_worker`] prefers the first idle worker within the lane, so serial
/// work in a lane keeps reusing its first worker's warm context while the rest
/// never build one; the moment a second request in the same lane overlaps, it
/// lands on the next worker's context with no wait and no shared lock.
struct LaneWorkers {
    senders: Vec<mpsc::Sender<Job>>,
    busy: Vec<Arc<AtomicBool>>,
    round_robin: AtomicUsize,
}

/// A bounded set of worker threads split into two lanes, each worker owning one
/// persistent `LlamaContext`. Bulk requests reach the [`bulk`](Self::bulk)
/// lane's workers and interactive requests the [`interactive`](Self::interactive)
/// lane's, so a bulk index batch never occupies a context an interactive embed
/// needs (ADR-096): with the pool sized to the total admission capacity, at
/// least `interactive.len()` contexts stay reachable for interactive work no
/// matter how many the bulk lane holds.
struct WorkerPool {
    interactive: LaneWorkers,
    bulk: LaneWorkers,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl WorkerPool {
    fn new(
        model: Arc<LlamaModel>,
        token_cap: usize,
        n_threads: i32,
        bulk_size: usize,
        interactive_size: usize,
    ) -> Self {
        // A pool with no workers at all can embed nothing, and would leave both
        // lanes empty (see `effective_lane`). Guarantee at least one worker so at
        // most one lane is ever empty; clamp in release, not just a debug_assert.
        let bulk_size = if bulk_size + interactive_size == 0 {
            1
        } else {
            bulk_size
        };
        let mut handles = Vec::with_capacity(bulk_size + interactive_size);
        let interactive = Self::spawn_lane(
            &model,
            token_cap,
            n_threads,
            interactive_size,
            EmbedLane::Interactive,
            &mut handles,
        );
        let bulk = Self::spawn_lane(
            &model,
            token_cap,
            n_threads,
            bulk_size,
            EmbedLane::Bulk,
            &mut handles,
        );
        Self {
            interactive,
            bulk,
            handles,
        }
    }

    fn spawn_lane(
        model: &Arc<LlamaModel>,
        token_cap: usize,
        n_threads: i32,
        size: usize,
        lane: EmbedLane,
        handles: &mut Vec<std::thread::JoinHandle<()>>,
    ) -> LaneWorkers {
        let mut senders = Vec::with_capacity(size);
        let mut busy = Vec::with_capacity(size);
        let tag = match lane {
            EmbedLane::Interactive => "interactive",
            EmbedLane::Bulk => "bulk",
        };
        for i in 0..size {
            let (tx, rx) = mpsc::channel::<Job>();
            let flag = Arc::new(AtomicBool::new(false));
            let idle_timeout = worker_idle_timeout(lane, i);
            let model = Arc::clone(model);
            let worker_flag = Arc::clone(&flag);
            let handle = std::thread::Builder::new()
                .name(format!("llama-embed-{tag}-{i}"))
                .spawn(move || {
                    worker_loop(model, token_cap, n_threads, &rx, &worker_flag, idle_timeout)
                })
                .expect("spawning a llama embed worker thread");
            senders.push(tx);
            busy.push(flag);
            handles.push(handle);
        }
        LaneWorkers {
            senders,
            busy,
            round_robin: AtomicUsize::new(0),
        }
    }

    fn dispatch(&self, job: Job, lane: EmbedLane) {
        let lane = effective_lane(
            lane,
            self.interactive.senders.len(),
            self.bulk.senders.len(),
        );
        let workers = match lane {
            EmbedLane::Interactive => &self.interactive,
            EmbedLane::Bulk => &self.bulk,
        };
        let worker = claim_worker(&workers.busy, &workers.round_robin);
        // A closed channel means the worker exited (pool shutting down); the
        // dropped `job` drops its reply, so the awaiting caller sees a cancelled
        // oneshot and returns an error rather than hanging.
        let _ = workers.senders[worker].send(job);
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        // Close every channel so each worker's `recv` returns `Disconnected`
        // and the loop exits, then join: a worker's context borrows the model
        // Arc it holds, so it must be torn down before this returns.
        self.interactive.senders.clear();
        self.bulk.senders.clear();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

/// How long a worker at `index_in_lane` in `lane` keeps a built context through
/// idle before dropping it. `None` means never for idle — exactly one
/// interactive context (the first worker in the interactive lane) stays
/// permanently hot so serial interactive use never pays a cold-context build.
/// Every other worker — bulk, and any additional interactive worker built only
/// under concurrent interactive load — idle-drops on [`CONTEXT_IDLE_TIMEOUT`]
/// so its reserved address space is not held between passes (ADR-096 §3/§4).
fn worker_idle_timeout(lane: EmbedLane, index_in_lane: usize) -> Option<Duration> {
    match lane {
        EmbedLane::Interactive if index_in_lane == 0 => None,
        _ => Some(CONTEXT_IDLE_TIMEOUT),
    }
}

/// The lane to actually dispatch on. Normally the requested one, but a lane can
/// have no workers when a direct caller sizes it to zero — a single-context
/// embedder built with `interactive_capacity = 0`. Falling back to the other
/// lane keeps `claim_worker`'s `% busy.len()` from dividing by zero. A pool
/// always has at least one worker overall (guaranteed at construction), so at
/// most one lane is ever empty.
fn effective_lane(
    requested: EmbedLane,
    interactive_workers: usize,
    bulk_workers: usize,
) -> EmbedLane {
    match requested {
        EmbedLane::Interactive if interactive_workers == 0 => EmbedLane::Bulk,
        EmbedLane::Bulk if bulk_workers == 0 => EmbedLane::Interactive,
        other => other,
    }
}

/// Pick the worker to run the next job within a lane: the first idle one
/// (claiming it), else — every worker in the lane being busy means the lane's
/// admission capacity is saturated — the next by rotation, whose queue it joins.
/// Pulled out of [`WorkerPool::dispatch`] so the preference is unit-testable
/// without a model.
fn claim_worker(busy: &[Arc<AtomicBool>], round_robin: &AtomicUsize) -> usize {
    for (i, flag) in busy.iter().enumerate() {
        if flag
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return i;
        }
    }
    round_robin.fetch_add(1, Ordering::Relaxed) % busy.len()
}

/// Owns one persistent context for its whole life and decodes jobs against it.
/// The context is built lazily on the first job (so an unused worker never
/// allocates one). It is dropped on a decode failure (which may have wedged it),
/// and — when `idle_timeout` is `Some` — after that long with no job; a `None`
/// timeout keeps the context resident through any idle spell (the persistently
/// hot interactive context, ADR-096 §3). Either drop path rebuilds on the next
/// job.
fn worker_loop(
    model: Arc<LlamaModel>,
    token_cap: usize,
    n_threads: i32,
    rx: &mpsc::Receiver<Job>,
    busy: &AtomicBool,
    idle_timeout: Option<Duration>,
) {
    let backend = match backend() {
        Ok(b) => b,
        // Without a backend nothing can be embedded; let jobs' replies drop so
        // callers get an error, and exit the worker.
        Err(e) => {
            tracing::error!("llama embed worker cannot start: {e:#}");
            return;
        }
    };
    let ubatch = match u32::try_from(token_cap) {
        Ok(u) => u,
        Err(_) => {
            tracing::error!("llama embed worker: token cap {token_cap} exceeds u32");
            return;
        }
    };
    // `ctx` borrows `*model` for the rest of this function; declared after
    // `model` so it drops first. This in-one-stack-frame self-reference is
    // exactly why a context cannot live in a plain pool next to the Arc.
    let model_ref: &LlamaModel = &model;
    let mut ctx: Option<LlamaContext<'_>> = None;
    let mut batch = LlamaBatch::new(token_cap, 1);

    loop {
        busy.store(false, Ordering::Release);
        // Only a worker holding a context on an idle timeout waits with a
        // deadline; a worker with no context yet, or one exempt from idle
        // eviction, blocks until the next job (or channel close).
        let job = match (ctx.is_some(), idle_timeout) {
            (true, Some(timeout)) => match rx.recv_timeout(timeout) {
                Ok(job) => job,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    ctx = None;
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            _ => match rx.recv() {
                Ok(job) => job,
                Err(_) => break,
            },
        };
        busy.store(true, Ordering::Release);

        if ctx.is_none() {
            match model_ref.new_context(backend, context_params(ubatch, n_threads)) {
                Ok(c) => ctx = Some(c),
                Err(e) => {
                    let _ = job.reply.send(Err(EmbedError::Inference(format!(
                        "creating llama context: {e}"
                    ))
                    .into()));
                    continue;
                }
            }
        }

        let result = run_job(
            ctx.as_mut().expect("context just ensured"),
            &mut batch,
            model_ref,
            token_cap,
            &job.texts,
            &job.cancel,
        );
        // A decode/read failure may have left the context wedged (e.g. a lost
        // Metal device); drop it so the next job rebuilds. A cancellation left
        // it clean (KV cleared), so that context is kept warm.
        if result.is_err() && !is_cancelled(&result) {
            ctx = None;
        }
        let _ = job.reply.send(result);
    }
}

/// True when the error is a cooperative cancellation rather than a real
/// failure — the one error that does not mean the reused context is suspect.
fn is_cancelled(result: &EmbedResult) -> bool {
    matches!(
        result,
        Err(e) if matches!(e.downcast_ref::<EmbedError>(), Some(EmbedError::Cancelled { .. }))
    )
}

/// Embed every chunk through one reused context, one chunk per `llama_decode`
/// with the KV cache cleared between chunks. `cancel` is checked before each
/// chunk; on cancel the KV cache is cleared so the context is clean for reuse.
fn run_job(
    ctx: &mut LlamaContext,
    batch: &mut LlamaBatch,
    model: &LlamaModel,
    token_cap: usize,
    texts: &[String],
    cancel: &AtomicBool,
) -> anyhow::Result<Vec<Vec<f32>>> {
    let total = texts.len();
    if cancel.load(Ordering::Relaxed) {
        tracing::info!("embed batch abandoned before starting (0/{total} chunks completed)");
        return Err(EmbedError::Cancelled {
            completed: 0,
            total,
        }
        .into());
    }

    // Tokenize everything upfront so malformed input fails before any decode.
    let eos = model.token_eos();
    let mut token_lists: Vec<Vec<LlamaToken>> = Vec::with_capacity(total);
    for text in texts {
        // llama.cpp tokenizes through a C string, which cannot carry interior
        // NUL bytes; such input surfaces as a Tokenization error rather than
        // silently embedding different text.
        let mut toks = model
            .str_to_token(text, AddBos::Never)
            .map_err(|e| EmbedError::Tokenization(e.to_string()))?;
        // Mirror the HF post-processor exactly: EOS appended first, then the
        // cap applied — a truncated chunk loses its EOS on both engines alike.
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

    let mut out = Vec::with_capacity(total);
    for (i, toks) in token_lists.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("embed batch cancelled after {i}/{total} chunks");
            // Leave the reused context clean for the next job.
            ctx.clear_kv_cache();
            return Err(EmbedError::Cancelled {
                completed: i,
                total,
            }
            .into());
        }
        batch.clear();
        // `true` marks the tokens as output-bearing. A pooling embedder needs
        // logits at the pooled positions, so with `false` llama.cpp overrides
        // the flag on every decode and logs a WARN per forward pass ("some input
        // tokens were not marked as outputs") — ~one line per chunk, thousands
        // per index. Marking them up front is what it does anyway; the pooled
        // read (`embeddings_seq_ith`) and the vectors are unchanged.
        batch
            .add_sequence(toks, 0, true)
            .map_err(|e| EmbedError::Inference(format!("batching chunk {i}: {e}")))?;
        ctx.clear_kv_cache();
        ctx.decode(batch)
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
}

pub struct LlamaEmbedder {
    pool: WorkerPool,
    dim: usize,
    token_cap: usize,
    device: &'static str,
}

impl LlamaEmbedder {
    /// Load the F2LLM embedder from a canonical llama.cpp GGUF already on
    /// disk, with zero network access — the tokenizer and model config travel
    /// inside the GGUF, so this takes just the one file.
    ///
    /// `threads` caps llama.cpp's per-context CPU threadpool; `None` uses all
    /// available parallelism.
    ///
    /// `bulk_capacity` and `interactive_capacity` are how many persistent
    /// contexts (worker threads) back each admission lane. The server passes its
    /// two embed-admission capacities so every admitted concurrent embed has its
    /// own context on its own lane (ADR-096); direct callers can pass
    /// [`DEFAULT_EMBED_POOL_SIZE`] for bulk and `1` for interactive.
    pub fn load_from_path(
        gguf_path: &Path,
        device: DeviceRequest,
        threads: Option<usize>,
        bulk_capacity: usize,
        interactive_capacity: usize,
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
            // Auto lands here on every deliberately CPU-only build (bare
            // `llama` / arm64 release), so it is routine, not a warning;
            // an explicit gpu request that can't be honored is.
            (DeviceRequest::Auto, None) => {
                tracing::info!("no GPU backend available; llama engine on CPU");
                (0, "cpu")
            }
            (DeviceRequest::Gpu, None) => {
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
        // The no-re-index guarantee rests on every shipped GGUF producing
        // 896-dim vectors in one vector space under a fixed MODEL_ID. A GGUF with
        // a different hidden size would otherwise load `ready` and emit
        // wrong-width vectors; refuse it up front rather than serve them.
        anyhow::ensure!(
            dim == crate::DIM,
            "GGUF reports embedding dim {dim}, but this build ships {} ({}); refusing to \
             load a different-width model under an unchanged MODEL_ID",
            crate::DIM,
            crate::MODEL_ID,
        );

        let n_threads = i32::try_from(threads.unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
        }))
        .unwrap_or(i32::MAX);

        let token_cap = ensure_embed_context_fits(&model, backend, n_threads)?;

        tracing::info!(
            "F2LLM-v2-330M ready (dim={dim}, Q8_0, engine=llama, device={device_name}); \
             token cap {token_cap}, {bulk_capacity} bulk + {interactive_capacity} interactive \
             warm context(s)"
        );

        let pool = WorkerPool::new(
            Arc::new(model),
            token_cap as usize,
            n_threads,
            bulk_capacity,
            interactive_capacity,
        );

        Ok(Self {
            pool,
            dim,
            token_cap: token_cap as usize,
            device: device_name,
        })
    }

    /// The resolved inference device, for logs and `/v1/health` (`"cpu"`,
    /// `"metal"`, `"vulkan"`, or `"gpu"` for any other GPU-class backend).
    pub fn device(&self) -> &'static str {
        self.device
    }
}

/// Verify a llama context at the fixed [`EMBED_UBATCH`] allocates on this
/// device, and return that size as the token cap. Context creation is where
/// llama.cpp reserves the KV-cache and compute buffers, so a failure means this
/// hardware cannot hold a context that size — refused, not stepped down: a
/// smaller context would truncate long inputs at a different token boundary than
/// other hosts and so embed the same source to different vectors.
fn ensure_embed_context_fits(
    model: &LlamaModel,
    backend: &LlamaBackend,
    n_threads: i32,
) -> Result<u32> {
    match model.new_context(backend, context_params(EMBED_UBATCH, n_threads)) {
        Ok(_ctx) => Ok(EMBED_UBATCH),
        Err(e) => Err(anyhow::anyhow!(
            "this hardware cannot run the inkentry embedder: a llama context at ubatch \
             {EMBED_UBATCH} would not allocate ({e}). The embedder runs one fixed context size \
             on every machine so embeddings are identical across hosts, and does not fall back \
             to a smaller one."
        )),
    }
}

#[async_trait::async_trait]
impl crate::EmbeddingBackend for LlamaEmbedder {
    /// Embed a batch of strings with no way to cancel early. Delegates to
    /// [`Self::embed_with_cancel`] with a flag that's never set, so there is
    /// exactly one path to the worker pool.
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.embed_with_cancel(texts, Arc::new(AtomicBool::new(false)))
            .await
    }

    /// Delegates to [`Self::embed_lane`] on the bulk lane: a caller with no lane
    /// of its own (the bench, a direct library consumer) is background work by
    /// default, never a person waiting.
    async fn embed_with_cancel(
        &self,
        texts: &[&str],
        cancel: Arc<AtomicBool>,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        self.embed_lane(texts, cancel, EmbedLane::Bulk).await
    }

    /// Embed a batch of strings via llama.cpp on the declared `lane`, stopping
    /// early if `cancel` is observed set.
    ///
    /// The request is handed to a [`WorkerPool`] worker on `lane` that owns a
    /// persistent context: each chunk is tokenized with llama.cpp's own
    /// tokenizer (byte-identical to the HF tokenizer for this model, verified
    /// including the appended EOS), truncated to the token cap, then decoded one
    /// chunk per forward pass with last-token pooling and the KV cache cleared
    /// between chunks; the pooled vector is L2-normalised. The context is
    /// *reused* across calls, so a serial index does not rebuild (and rewarm) a
    /// Metal context per request, which would starve GPU utilisation.
    ///
    /// `cancel` is checked before starting and between chunks, bounding waste to
    /// one chunk's forward pass, and `completed`/`total` count chunks. There is
    /// no interior mutex: workers are independent, and the interactive lane's
    /// contexts are separate from the bulk lane's, so a bulk index batch never
    /// blocks a concurrent interactive embed (ADR-096).
    async fn embed_lane(
        &self,
        texts: &[&str],
        cancel: Arc<AtomicBool>,
        lane: EmbedLane,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let (reply, reply_rx) = oneshot::channel();
        let job = Job {
            texts: texts.iter().map(|s| s.to_string()).collect(),
            cancel,
            reply,
        };
        self.pool.dispatch(job, lane);
        reply_rx.await.unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "llama embed worker dropped the reply channel"
            ))
        })
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

    fn flags(states: &[bool]) -> Vec<Arc<AtomicBool>> {
        states
            .iter()
            .map(|&b| Arc::new(AtomicBool::new(b)))
            .collect()
    }

    #[test]
    fn claim_worker_prefers_the_first_idle_worker_and_marks_it_busy() {
        // Serial dispatch keeps landing on worker 0 (kept warm) while the rest
        // stay idle and never build a context.
        let busy = flags(&[false, false]);
        let rr = AtomicUsize::new(0);
        assert_eq!(claim_worker(&busy, &rr), 0);
        assert!(
            busy[0].load(Ordering::Acquire),
            "claimed worker is marked busy"
        );
        assert!(!busy[1].load(Ordering::Acquire), "the spare stays idle");
    }

    #[test]
    fn claim_worker_takes_the_next_idle_worker_when_the_first_is_busy() {
        // A concurrent embed arriving while worker 0 is mid-batch lands on the
        // idle worker 1 rather than queuing behind the bulk work.
        let busy = flags(&[true, false]);
        let rr = AtomicUsize::new(0);
        assert_eq!(claim_worker(&busy, &rr), 1);
        assert!(busy[1].load(Ordering::Acquire));
    }

    #[test]
    fn exactly_one_interactive_context_is_persistently_hot() {
        // The first interactive worker never idle-drops its context, so serial
        // interactive use always lands on a warm one; every other worker —
        // additional interactive workers and all bulk workers — idle-drops so
        // its reserved address space is not held between passes.
        assert_eq!(worker_idle_timeout(EmbedLane::Interactive, 0), None);
        assert_eq!(
            worker_idle_timeout(EmbedLane::Interactive, 1),
            Some(CONTEXT_IDLE_TIMEOUT)
        );
        assert_eq!(
            worker_idle_timeout(EmbedLane::Interactive, 2),
            Some(CONTEXT_IDLE_TIMEOUT)
        );
        assert_eq!(
            worker_idle_timeout(EmbedLane::Bulk, 0),
            Some(CONTEXT_IDLE_TIMEOUT)
        );
        assert_eq!(
            worker_idle_timeout(EmbedLane::Bulk, 1),
            Some(CONTEXT_IDLE_TIMEOUT)
        );
    }

    #[test]
    fn claim_worker_rotates_when_every_worker_is_busy() {
        // Past the pool size (bounded by the server's embed admission), work is
        // handed out by rotation; no worker's busy flag is disturbed.
        let busy = flags(&[true, true]);
        let rr = AtomicUsize::new(0);
        assert_eq!(claim_worker(&busy, &rr), 0);
        assert_eq!(claim_worker(&busy, &rr), 1);
        assert_eq!(claim_worker(&busy, &rr), 0);
    }

    #[test]
    fn dispatch_falls_back_to_a_populated_lane_when_the_requested_one_is_empty() {
        assert_eq!(
            effective_lane(EmbedLane::Interactive, 0, 1),
            EmbedLane::Bulk
        ); // single-context embedder: interactive lane sized to zero
        assert_eq!(
            effective_lane(EmbedLane::Bulk, 1, 0),
            EmbedLane::Interactive
        );
        assert_eq!(
            effective_lane(EmbedLane::Interactive, 3, 4),
            EmbedLane::Interactive
        );
        assert_eq!(effective_lane(EmbedLane::Bulk, 3, 4), EmbedLane::Bulk);
    }

    #[test]
    fn is_cancelled_distinguishes_cancellation_from_real_failure() {
        let cancelled: EmbedResult = Err(EmbedError::Cancelled {
            completed: 1,
            total: 3,
        }
        .into());
        let failed: EmbedResult =
            Err(EmbedError::Inference("llama decode: device lost".into()).into());
        let ok: EmbedResult = Ok(vec![vec![0.0; 4]]);
        assert!(is_cancelled(&cancelled));
        assert!(
            !is_cancelled(&failed),
            "a real failure must drop the context"
        );
        assert!(!is_cancelled(&ok));
    }

    #[test]
    fn load_from_missing_gguf_errors_without_network() {
        let err = match LlamaEmbedder::load_from_path(
            Path::new("/nonexistent/model.gguf"),
            DeviceRequest::Cpu,
            None,
            DEFAULT_EMBED_POOL_SIZE,
            1,
        ) {
            Ok(_) => panic!("load of a nonexistent GGUF must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("GGUF file not found"));
    }

    #[cfg(feature = "llama-vulkan")]
    #[test]
    fn ggml_backend_module_detection_excludes_the_core_base_lib() {
        // Real runtime-loaded backend modules qualify.
        assert!(is_ggml_backend_module("libggml-vulkan.so"));
        assert!(is_ggml_backend_module("libggml-cpu-haswell.so.0"));
        assert!(is_ggml_backend_module("ggml-vulkan.dll"));
        // The core base library shares the `libggml-` prefix but is not a
        // backend module, so a directory holding only it must not be selected.
        assert!(!is_ggml_backend_module("libggml-base.so.0"));
        assert!(!is_ggml_backend_module("ggml-base.dll"));
        // Other core libs and unrelated files don't match the prefix at all.
        assert!(!is_ggml_backend_module("libggml.so"));
        assert!(!is_ggml_backend_module("libllama.so"));
        assert!(!is_ggml_backend_module("tokenizer.json"));
    }
}
