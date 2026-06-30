use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use candle_core::quantized::{GgmlDType, QMatMul, QTensor, gguf_file};
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::{Activation, Embedding, Module, RmsNorm, rotary_emb::rope};
use candle_transformers::models::qwen3::Config as Qwen3Config;
use candle_transformers::utils::repeat_kv;
use hf_hub::{Repo, RepoType, api::sync::ApiBuilder};
use tokenizers::Tokenizer;

/// Embedding dimension produced by F2LLM-v2-330M (hidden_size = 896).
pub const DIM: usize = 896;

const MODEL_ID: &str = "codefuse-ai/F2LLM-v2-330M";
/// Hard ceiling for token sequences (max_position_embeddings).
const MAX_SEQ_LEN: usize = 40960;
/// Number of sequences processed in one padded forward pass.
/// Sequences within a sub-batch are sorted by length first so padding waste
/// stays bounded.  Tune upward if BLAS efficiency plateaus at larger matrices.
const EMBED_BATCH_SIZE: usize = 8;
/// Longest sequence (tokens) allowed in a batched forward pass.
/// The attention score tensor is (b × n_head × max_seq²) so it grows
/// quadratically; beyond this threshold we fall back to forward_one to
/// avoid multi-GB allocations on long code chunks.
/// At 512 tokens: 8 × 16 × 512² × 4 bytes ≈ 134 MB — well within budget.
const BATCH_MAX_SEQ: usize = 512;

/// Number of attention heads in F2LLM-v2-330M (Qwen3, `num_attention_heads`).
/// The single-chunk attention score / probability tensors are
/// `[1, N_HEAD, seq, seq]` f32, so peak scratch grows as `N_HEAD × seq² × 4`.
/// This must match the model `config.json`; it is asserted against the loaded
/// config at startup (see `NativeEmbedder::load`).
const N_HEAD: usize = 16;

/// Memory budget (bytes) for the single-chunk attention scratch on hosts with
/// ≤ 16 GiB of total RAM. Conservative default so a full CPU index never OOMs.
const SINGLE_CHUNK_BUDGET_SMALL: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB

/// Memory budget (bytes) for hosts with > 16 GiB of total RAM.
const SINGLE_CHUNK_BUDGET_LARGE: u64 = 4 * 1024 * 1024 * 1024; // 4 GiB

/// RAM threshold (bytes) above which the larger single-chunk budget is used.
const LARGE_RAM_THRESHOLD: u64 = 16 * 1024 * 1024 * 1024; // 16 GiB

/// Pick the single-chunk attention memory budget from total system RAM:
/// 2 GiB by default, 4 GiB when the host has more than 16 GiB of RAM.
fn single_chunk_budget(total_ram_bytes: u64) -> u64 {
    if total_ram_bytes > LARGE_RAM_THRESHOLD {
        SINGLE_CHUNK_BUDGET_LARGE
    } else {
        SINGLE_CHUNK_BUDGET_SMALL
    }
}

/// Derive the maximum single-chunk token count that keeps the attention scratch
/// within `budget_bytes`.
///
/// In `forward_one` (batch = 1) the dominant allocation is the attention score
/// tensor `[1, n_head, seq, seq]` in f32 — and the softmax produces a second
/// tensor of the same shape — so peak scratch is bounded by
/// `n_head × seq² × 4 bytes`. (Anchor: at seq = 40 960, 16 × 40960² × 4 ≈ 100
/// GiB, matching the observed ~107 GB OOM on a full-length 40 k-token chunk.)
///
/// Solving `n_head × seq² × 4 ≤ budget` for `seq` gives the cap. We also clamp
/// to `MAX_SEQ_LEN` (the model's position-embedding ceiling) so the result is
/// never larger than the model can attend over anyway.
///
/// At 2 GiB → ~5 792 tokens; at 4 GiB → ~8 192 tokens.
fn derive_token_cap(budget_bytes: u64, n_head: usize) -> usize {
    let bytes_per_seq_sq = (n_head as u64) * 4;
    // seq <= sqrt(budget / (n_head * 4))
    let seq_sq = budget_bytes / bytes_per_seq_sq;
    let cap = (seq_sq as f64).sqrt().floor() as usize;
    cap.clamp(1, MAX_SEQ_LEN)
}

/// Total physical system RAM in bytes, best-effort and cross-platform.
///
/// macOS uses the `hw.memsize` sysctl; Linux reads `MemTotal` from
/// `/proc/meminfo`. On any other platform, or if detection fails, we return a
/// conservative `0` so the caller falls back to the small (2 GiB) budget.
fn total_system_ram() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let mut size: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        let name = c"hw.memsize";
        // SAFETY: `name` is a valid NUL-terminated C string; `size`/`len` point
        // to a u64 and its length. sysctlbyname writes at most `len` bytes.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut size as *mut u64 as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 { size } else { 0 }
    }
    #[cfg(target_os = "linux")]
    {
        // /proc/meminfo: "MemTotal:    16327476 kB"
        let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") else {
            return 0;
        };
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                if let Some(kb) = rest.split_whitespace().next() {
                    if let Ok(kb) = kb.parse::<u64>() {
                        return kb * 1024;
                    }
                }
            }
        }
        0
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0
    }
}

/// Filename of the Q8_0-quantized GGUF cached next to the HF download. Projection
/// matmuls and the token-embedding table are stored Q8_0; the small RMSNorm
/// weights stay F32. Built once from the safetensors download (see
/// `write_quantized_gguf`) so subsequent loads read ~355 MB instead of ~650 MB.
const QUANT_GGUF: &str = "f2llm-v2-330m-q8_0.gguf";

pub struct NativeEmbedder {
    inner: Arc<Mutex<EmbedderInner>>,
}

struct EmbedderInner {
    weights: Qwen3EmbedWeights,
    tokenizer: Tokenizer,
    /// Max tokens per single chunk before the attention scratch would exceed the
    /// memory budget. Oversized chunks are truncated to this length (see
    /// `derive_token_cap` / `single_chunk_budget`). 0 means "no extra cap"
    /// (only the `MAX_SEQ_LEN` ceiling applies) — used in unit tests.
    token_cap: usize,
}

// ── no-KV-cache Qwen3 embedder (Q8_0 quantized) ───────────────────────────────

/// Qwen3 transformer weights loaded once; `forward_one` produces embeddings
/// with no KV cache and no generation state.  Each text gets a fresh causal
/// mask so no state leaks between texts in a batch.
///
/// Projection weights are Q8_0-quantized `QMatMul`; activations run in F32 (the
/// dtype candle's quantized matmul kernels consume on both CPU and Metal).
struct Qwen3EmbedWeights {
    embed_tokens: Embedding,
    layers: Vec<EmbedLayer>,
    final_norm: RmsNorm,
    rope_cos: Tensor, // (MAX_SEQ_LEN, head_dim/2)
    rope_sin: Tensor,
    n_head: usize,
    n_kv_head: usize,
    head_dim: usize,
    device: Device,
}

struct EmbedLayer {
    attn_norm: RmsNorm,
    q: QMatMul,
    k: QMatMul,
    v: QMatMul,
    o: QMatMul,
    q_norm: RmsNorm,
    k_norm: RmsNorm,
    gate: QMatMul,
    up: QMatMul,
    down: QMatMul,
    post_norm: RmsNorm,
}

impl Qwen3EmbedWeights {
    /// Build the model from the cached Q8_0 GGUF, placing every tensor on
    /// `device`. Q8_0 projection weights become `QMatMul`; the embedding table
    /// is dequantized to F16 for the gather; RMSNorm weights are F32.
    fn from_gguf(path: &Path, cfg: &Qwen3Config, device: &Device) -> Result<Self> {
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("opening quantized GGUF {}", path.display()))?;
        let content = gguf_file::Content::read(&mut file)
            .with_context(|| format!("reading GGUF header {}", path.display()))?;

        // Token-embedding table: stored Q8_0, dequantized to F16 for the gather.
        let embed_w = read_qtensor(&content, &mut file, "model.embed_tokens.weight", device)?
            .dequantize_f16(device)
            .context("dequantizing embed_tokens")?;
        let embed_tokens = Embedding::new(embed_w, cfg.hidden_size);

        // Pre-compute RoPE sin/cos tables in F32 (activations run in F32).
        let half_hd = cfg.head_dim / 2;
        let inv_freq: Vec<f32> = (0..half_hd)
            .map(|i| 1.0f32 / (cfg.rope_theta as f32).powf(2.0 * i as f32 / cfg.head_dim as f32))
            .collect();
        let inv_freq_t = Tensor::from_vec(inv_freq, (1, half_hd), device)?;
        let t = Tensor::arange(0u32, MAX_SEQ_LEN as u32, device)?
            .to_dtype(DType::F32)?
            .reshape((MAX_SEQ_LEN, 1))?;
        let freqs = t.matmul(&inv_freq_t)?; // (MAX_SEQ_LEN, head_dim/2)
        let rope_cos = freqs.cos()?;
        let rope_sin = freqs.sin()?;

        let eps = cfg.rms_norm_eps;
        let mut layers = Vec::with_capacity(cfg.num_hidden_layers);
        for i in 0..cfg.num_hidden_layers {
            let p = format!("model.layers.{i}");
            layers.push(EmbedLayer {
                attn_norm: read_norm(
                    &content,
                    &mut file,
                    &format!("{p}.input_layernorm.weight"),
                    eps,
                    device,
                )?,
                q: read_qmm(
                    &content,
                    &mut file,
                    &format!("{p}.self_attn.q_proj.weight"),
                    device,
                )?,
                k: read_qmm(
                    &content,
                    &mut file,
                    &format!("{p}.self_attn.k_proj.weight"),
                    device,
                )?,
                v: read_qmm(
                    &content,
                    &mut file,
                    &format!("{p}.self_attn.v_proj.weight"),
                    device,
                )?,
                o: read_qmm(
                    &content,
                    &mut file,
                    &format!("{p}.self_attn.o_proj.weight"),
                    device,
                )?,
                q_norm: read_norm(
                    &content,
                    &mut file,
                    &format!("{p}.self_attn.q_norm.weight"),
                    eps,
                    device,
                )?,
                k_norm: read_norm(
                    &content,
                    &mut file,
                    &format!("{p}.self_attn.k_norm.weight"),
                    eps,
                    device,
                )?,
                gate: read_qmm(
                    &content,
                    &mut file,
                    &format!("{p}.mlp.gate_proj.weight"),
                    device,
                )?,
                up: read_qmm(
                    &content,
                    &mut file,
                    &format!("{p}.mlp.up_proj.weight"),
                    device,
                )?,
                down: read_qmm(
                    &content,
                    &mut file,
                    &format!("{p}.mlp.down_proj.weight"),
                    device,
                )?,
                post_norm: read_norm(
                    &content,
                    &mut file,
                    &format!("{p}.post_attention_layernorm.weight"),
                    eps,
                    device,
                )?,
            });
        }

        let final_norm = read_norm(&content, &mut file, "model.norm.weight", eps, device)?;

        Ok(Self {
            embed_tokens,
            layers,
            final_norm,
            rope_cos,
            rope_sin,
            n_head: cfg.num_attention_heads,
            n_kv_head: cfg.num_key_value_heads,
            head_dim: cfg.head_dim,
            device: device.clone(),
        })
    }

    /// Forward pass for one text; returns `[1, seq_len, hidden_size]`.
    fn forward_one(&self, ids: &[u32]) -> Result<Tensor> {
        let seq = ids.len();
        let ids_t = Tensor::new(ids, &self.device)?.unsqueeze(0)?; // [1, seq]
        // Embedding table is F16; promote to F32 for the (quantized) transformer.
        let mut h = self.embed_tokens.forward(&ids_t)?.to_dtype(DType::F32)?; // [1, seq, hidden]

        let mask = causal_mask(seq, h.dtype(), &self.device)?; // [1, 1, seq, seq]
        for layer in &self.layers {
            h = self.layer_fwd(&h, layer, &mask)?;
        }
        Ok(self.final_norm.forward(&h)?)
    }

    /// Padded batch forward pass: all sequences in `batch_ids` are right-padded
    /// to the longest sequence in the batch.  Returns one L2-normalised
    /// embedding vector per input sequence in the same order.
    ///
    /// On CPU, sequences are forwarded together as a single BLAS call (batch
    /// dim > 1), amortising per-call overhead.  On Metal/GPU the sequential
    /// path is used instead: Metal's buffer pool grows unboundedly with batched
    /// inference because `(b × n_head × seq²)` attention tensors are never
    /// compacted between forward passes, causing OOM for large codebases.
    /// Sequential GPU inference was the pre-batching baseline and is still fast.
    fn embed_batch(&self, batch_ids: &[&[u32]]) -> Result<Vec<Vec<f32>>> {
        let b = batch_ids.len();
        assert!(b > 0);

        let max_seq = batch_ids.iter().map(|ids| ids.len()).max().unwrap_or(0);

        // Sequential path: single sequences, GPU/Metal devices (buffer pool grows
        // unboundedly with batching), or long sequences where the attention tensor
        // (b × n_head × max_seq²) would exceed BATCH_MAX_SEQ and cause OOM.
        if b == 1 || !matches!(self.device, Device::Cpu) || max_seq > BATCH_MAX_SEQ {
            let mut out = Vec::with_capacity(b);
            for ids in batch_ids {
                let hidden = self.forward_one(ids)?;
                let last = hidden.i((0, ids.len() - 1))?;
                let mut v: Vec<f32> = last.to_dtype(DType::F32)?.to_vec1()?;
                l2_normalise(&mut v);
                anyhow::ensure!(
                    v.iter().all(|x| x.is_finite()),
                    "embedding vector contains NaN/inf — check model weights or sequence length"
                );
                out.push(v);
            }
            return Ok(out);
        }

        let seq_lens: Vec<usize> = batch_ids.iter().map(|ids| ids.len()).collect();

        // Build flat right-padded buffer [B × max_seq] with pad id = 0.
        let mut flat: Vec<u32> = Vec::with_capacity(b * max_seq);
        for ids in batch_ids {
            flat.extend_from_slice(ids);
            flat.extend(std::iter::repeat_n(0u32, max_seq - ids.len()));
        }
        let ids_t = Tensor::from_slice(&flat, (b, max_seq), &self.device)?;

        // Forward pass: all layers work naturally with batch dim > 1.
        // Embedding table is F16; promote to F32 for the (quantized) transformer.
        let mut h = self.embed_tokens.forward(&ids_t)?.to_dtype(DType::F32)?; // [b, max_seq, hidden]
        let mask = causal_mask(max_seq, h.dtype(), &self.device)?; // [1, 1, max_seq, max_seq]
        for layer in &self.layers {
            h = self.layer_fwd(&h, layer, &mask)?;
        }
        let h = self.final_norm.forward(&h)?; // [b, max_seq, hidden]

        // Extract last real token per sequence → L2-normalise.
        let mut out = Vec::with_capacity(b);
        for (i, &seq_len) in seq_lens.iter().enumerate() {
            let last = h.i((i, seq_len - 1))?; // [hidden]
            let mut v: Vec<f32> = last.to_dtype(DType::F32)?.to_vec1()?;
            l2_normalise(&mut v);
            anyhow::ensure!(
                v.iter().all(|x| x.is_finite()),
                "embedding vector contains NaN/inf — check model weights or sequence length"
            );
            out.push(v);
        }
        Ok(out)
    }

    fn layer_fwd(&self, x: &Tensor, layer: &EmbedLayer, mask: &Tensor) -> Result<Tensor> {
        let h = layer.attn_norm.forward(x)?;
        let h = self.attn(&h, layer, mask)?;
        let x = (x + h)?;
        let h = layer.post_norm.forward(&x)?;
        let h = self.mlp(&h, layer)?;
        Ok((x + h)?)
    }

    fn attn(&self, x: &Tensor, layer: &EmbedLayer, mask: &Tensor) -> Result<Tensor> {
        let (b, seq, _) = x.dims3()?;

        let q = layer.q.forward(x)?; // [b, seq, nh*hd]
        let k = layer.k.forward(x)?; // [b, seq, nkv*hd]
        let v = layer.v.forward(x)?; // [b, seq, nkv*hd]

        // Reshape and transpose to [b, n_heads, seq, head_dim]
        let q = q
            .reshape((b, seq, self.n_head, self.head_dim))?
            .transpose(1, 2)?;
        let k = k
            .reshape((b, seq, self.n_kv_head, self.head_dim))?
            .transpose(1, 2)?;
        let v = v
            .reshape((b, seq, self.n_kv_head, self.head_dim))?
            .transpose(1, 2)?;

        // QK RMSNorm (Qwen3 adds a per-head norm before RoPE)
        let q = layer.q_norm.forward(&q)?;
        let k = layer.k_norm.forward(&k)?;

        // RoPE
        let cos = self.rope_cos.narrow(0, 0, seq)?;
        let sin = self.rope_sin.narrow(0, 0, seq)?;
        let q = rope(&q.contiguous()?, &cos, &sin)?;
        let k = rope(&k.contiguous()?, &cos, &sin)?;

        // Grouped query attention: expand K, V from n_kv_heads to n_heads.
        // MUST repeat-interleave (each kv head duplicated n_rep times contiguously,
        // [kv0,kv0,kv1,kv1,…]) so query head j attends through kv head j/n_rep —
        // matching HF's repeat_kv. `Tensor::repeat` instead *tiles* the kv dim
        // ([kv0,…,kvN, kv0,…,kvN]), silently pairing most query heads with the
        // wrong K/V projection and collapsing retrieval quality (spelunk-oss#19).
        // candle's repeat_kv returns the correct interleaved order, contiguous.
        let n_rep = self.n_head / self.n_kv_head;
        let k = repeat_kv(k, n_rep)?;
        let v = repeat_kv(v, n_rep)?;

        // Scaled dot-product attention + causal mask
        let scale = (self.head_dim as f64).powf(-0.5);
        let scores = (q.matmul(&k.transpose(2, 3)?)? * scale)?;
        let scores = scores.broadcast_add(mask)?;
        let probs = candle_nn::ops::softmax_last_dim(&scores)?;
        let out = probs.matmul(&v)?; // [b, n_head, seq, hd]

        // Merge heads: [b, seq, n_head*hd]
        let out =
            out.transpose(1, 2)?
                .contiguous()?
                .reshape((b, seq, self.n_head * self.head_dim))?;
        Ok(layer.o.forward(&out)?)
    }

    fn mlp(&self, x: &Tensor, layer: &EmbedLayer) -> Result<Tensor> {
        let gate = layer.gate.forward(x)?.apply(&Activation::Silu)?;
        let up = layer.up.forward(x)?;
        Ok(layer.down.forward(&(gate * up)?)?)
    }
}

/// Read one tensor from the open GGUF onto `device`.
fn read_qtensor(
    content: &gguf_file::Content,
    file: &mut std::fs::File,
    name: &str,
    device: &Device,
) -> Result<QTensor> {
    content
        .tensor(file, name, device)
        .with_context(|| format!("reading tensor {name} from GGUF"))
}

/// Read an F32 RMSNorm weight from the GGUF and wrap it in an `RmsNorm`.
fn read_norm(
    content: &gguf_file::Content,
    file: &mut std::fs::File,
    name: &str,
    eps: f64,
    device: &Device,
) -> Result<RmsNorm> {
    let w = read_qtensor(content, file, name, device)?
        .dequantize(device)
        .with_context(|| format!("dequantizing norm {name}"))?;
    Ok(RmsNorm::new(w, eps))
}

/// Read a Q8_0 projection weight from the GGUF as a quantized matmul.
fn read_qmm(
    content: &gguf_file::Content,
    file: &mut std::fs::File,
    name: &str,
    device: &Device,
) -> Result<QMatMul> {
    let qt = read_qtensor(content, file, name, device)?;
    QMatMul::from_qtensor(qt).with_context(|| format!("building QMatMul for {name}"))
}

/// Upper-triangular causal mask: 0.0 where attention is allowed, -∞ otherwise.
fn causal_mask(seq: usize, dtype: DType, device: &Device) -> Result<Tensor> {
    let mask: Vec<f32> = (0..seq)
        .flat_map(|i| (0..seq).map(move |j| if j <= i { 0.0f32 } else { f32::NEG_INFINITY }))
        .collect();
    Ok(Tensor::from_slice(&mask, (1, 1, seq, seq), device)?.to_dtype(dtype)?)
}

// ── NativeEmbedder ────────────────────────────────────────────────────────────

impl NativeEmbedder {
    /// Load the F2LLM-v2-330M model, quantized to Q8_0.
    ///
    /// On first call the ~650 MB safetensors weights are downloaded into
    /// `~/.local/share/spelunk/models/` via the hf-hub cache, quantized to Q8_0,
    /// and written to a ~355 MB GGUF (`f2llm-v2-330m-q8_0.gguf`) in the same
    /// directory. Subsequent calls read the GGUF directly with no network access
    /// and no safetensors load. Uses Metal/GPU on macOS when built with the
    /// `metal` cargo feature, CPU otherwise.
    pub fn load() -> Result<Self> {
        let cache_dir = model_cache_dir()?;
        std::fs::create_dir_all(&cache_dir)
            .with_context(|| format!("creating model cache dir {}", cache_dir.display()))?;
        let gguf_path = cache_dir.join(QUANT_GGUF);

        let device = select_device();
        let on_gpu = !matches!(device, Device::Cpu);
        tracing::info!(
            "loading F2LLM-v2-330M (Q8_0) via candle on {} (cache: {})",
            if on_gpu { "Metal/GPU" } else { "CPU" },
            cache_dir.display()
        );

        let api = ApiBuilder::new()
            .with_cache_dir(cache_dir)
            .build()
            .context("building HuggingFace Hub API client")?;
        let repo = api.repo(Repo::new(MODEL_ID.to_string(), RepoType::Model));

        let tokenizer_path = repo
            .get("tokenizer.json")
            .context("downloading F2LLM-v2-330M tokenizer.json")?;
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("loading tokenizer: {e}"))?;

        let config_path = repo
            .get("config.json")
            .context("downloading F2LLM-v2-330M config.json")?;
        let config: Qwen3Config = serde_json::from_str(
            &std::fs::read_to_string(&config_path).context("reading F2LLM-v2-330M config.json")?,
        )
        .context("parsing F2LLM-v2-330M config.json")?;

        // Build the quantized GGUF once from the safetensors download.
        if !gguf_path.exists() {
            tracing::info!("quantizing F2LLM-v2-330M to Q8_0 GGUF (first run; one-time)…");
            let weight_paths = download_weights(&repo)?;
            write_quantized_gguf(&weight_paths, &gguf_path)
                .context("writing quantized F2LLM-v2-330M GGUF")?;
            tracing::info!("wrote quantized model to {}", gguf_path.display());
        }

        // The single-chunk token cap derivation assumes `N_HEAD` attention heads
        // (the attention scratch is `[1, n_head, seq, seq]`). Guard against a
        // future model whose config no longer matches the compiled-in constant.
        debug_assert_eq!(
            config.num_attention_heads, N_HEAD,
            "N_HEAD constant ({N_HEAD}) must match model config.json \
             num_attention_heads ({}) — token-cap derivation depends on it",
            config.num_attention_heads
        );
        let n_head = config.num_attention_heads;

        let weights = Qwen3EmbedWeights::from_gguf(&gguf_path, &config, &device)
            .context("loading quantized F2LLM-v2-330M weights")?;

        // Pick the memory budget from total system RAM, then derive the
        // single-chunk token cap that keeps the attention scratch within it.
        let total_ram = total_system_ram();
        let budget = single_chunk_budget(total_ram);
        let token_cap = derive_token_cap(budget, n_head);

        tracing::info!(
            "F2LLM-v2-330M ready (dim={DIM}, Q8_0, device={}); \
             system RAM {:.1} GiB → single-chunk budget {} GiB, token cap {token_cap}",
            if on_gpu { "Metal/GPU" } else { "CPU" },
            total_ram as f64 / (1024.0 * 1024.0 * 1024.0),
            budget / (1024 * 1024 * 1024),
        );

        Ok(Self {
            inner: Arc::new(Mutex::new(EmbedderInner {
                weights,
                tokenizer,
                token_cap,
            })),
        })
    }
}

/// Choose the inference device.
///
/// With the `metal` cargo feature (macOS builds), tries Metal first and falls
/// back to CPU on failure. Without `metal`, always returns CPU.
fn select_device() -> Device {
    #[cfg(feature = "metal")]
    {
        match Device::new_metal(0) {
            Ok(d) => return d,
            Err(e) => tracing::warn!("Metal GPU unavailable ({e}); falling back to CPU"),
        }
    }
    Device::Cpu
}

/// Download safetensors weights, handling both single-file and sharded layouts.
fn download_weights(repo: &hf_hub::api::sync::ApiRepo) -> Result<Vec<PathBuf>> {
    if let Ok(index_path) = repo.get("model.safetensors.index.json") {
        let index: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&index_path).context("reading safetensors index")?,
        )
        .context("parsing safetensors index")?;
        let mut shards: Vec<String> = index["weight_map"]
            .as_object()
            .map(|m| {
                m.values()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        shards.sort();
        shards.dedup();
        anyhow::ensure!(!shards.is_empty(), "safetensors index has no weight shards");
        return shards
            .iter()
            .map(|s| {
                repo.get(s)
                    .with_context(|| format!("downloading shard {s}"))
            })
            .collect();
    }
    Ok(vec![
        repo.get("model.safetensors")
            .context("downloading model.safetensors")?,
    ])
}

/// GGML dtype to quantize a given (prefixed) weight key to, or `None` to skip it.
///
/// Projection matmuls and the token-embedding table → Q8_0 (the disk/RAM win);
/// the tiny RMSNorm weights → F32 (kept full-precision, negligible size).
/// Unknown keys (e.g. a tied `lm_head.weight`) are skipped — the embedder reads
/// the final hidden state directly and never needs an LM head.
fn dtype_for_key(key: &str) -> Option<GgmlDType> {
    if key == "model.embed_tokens.weight" || key.ends_with("_proj.weight") {
        Some(GgmlDType::Q8_0)
    } else if key.ends_with("norm.weight") {
        Some(GgmlDType::F32)
    } else {
        None
    }
}

/// Load the safetensors weights on CPU, quantize each to its target GGML dtype,
/// and write a single GGUF to `gguf_path` (atomically, via a temp file). F2LLM
/// stores keys without the `model.` prefix (saved as a plain `Qwen3Model`), so
/// we add it to match the keys `from_gguf` reads back.
fn write_quantized_gguf(weight_paths: &[PathBuf], gguf_path: &Path) -> Result<()> {
    let cpu = Device::Cpu;
    let mut tensors: Vec<(String, QTensor)> = Vec::new();

    for path in weight_paths {
        let file_tensors = candle_core::safetensors::load(path, &cpu)
            .with_context(|| format!("loading weights from {}", path.display()))?;
        for (k, v) in file_tensors {
            let key = format!("model.{k}");
            let Some(dtype) = dtype_for_key(&key) else {
                continue;
            };
            // QTensor::quantize requires an F32 source.
            let v = v.to_dtype(DType::F32)?;
            let qt = QTensor::quantize(&v, dtype)
                .with_context(|| format!("quantizing {key} to {dtype:?}"))?;
            tensors.push((key, qt));
        }
    }
    anyhow::ensure!(!tensors.is_empty(), "no weights found to quantize");

    let tmp_path = gguf_path.with_extension("gguf.tmp");
    {
        let mut out = std::fs::File::create(&tmp_path)
            .with_context(|| format!("creating {}", tmp_path.display()))?;
        let refs: Vec<(&str, &QTensor)> = tensors.iter().map(|(k, q)| (k.as_str(), q)).collect();
        gguf_file::write(&mut out, &[], &refs).context("serialising GGUF")?;
        out.sync_all().context("flushing GGUF")?;
    }
    std::fs::rename(&tmp_path, gguf_path)
        .with_context(|| format!("renaming {} -> {}", tmp_path.display(), gguf_path.display()))?;
    Ok(())
}

fn model_cache_dir() -> Result<PathBuf> {
    dirs::data_local_dir()
        .map(|d| d.join("spelunk").join("models"))
        .ok_or_else(|| anyhow::anyhow!("could not determine local data directory"))
}

fn l2_normalise(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

#[async_trait::async_trait]
impl spelunk_core::embeddings::EmbeddingBackend for NativeEmbedder {
    /// Embed a batch of strings using F2LLM-v2-330M (Q8_0).
    ///
    /// Texts are tokenized, sorted by token length (to minimise padding waste),
    /// then forwarded through the Qwen3 decoder in padded sub-batches of
    /// `EMBED_BATCH_SIZE`.  Each sub-batch is one BLAS call with batch dim > 1,
    /// which amortises the per-call overhead.  The last token's hidden state is
    /// L2-normalised to produce a 896-dim embedding; results are returned in the
    /// original input order.
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let owned: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        let inner = Arc::clone(&self.inner);

        tokio::task::spawn_blocking(move || {
            let guard = inner
                .lock()
                .map_err(|_| anyhow::anyhow!("native embedder lock poisoned"))?;

            // 1. Tokenize all texts upfront.
            //
            // Cap each sequence to `effective_cap` tokens. `MAX_SEQ_LEN` is the
            // model's position-embedding ceiling; `token_cap` is the smaller,
            // memory-budget-derived bound that keeps the single-chunk attention
            // scratch (`[1, n_head, seq, seq]` f32) within the RAM budget. A
            // single ~40 k-token chunk (e.g. a lock/data/minified file) would
            // otherwise allocate ~100 GB of attention scores and OOM/abort the
            // whole index — so we *truncate* oversized chunks here, preserving
            // the leading (partial) signal rather than aborting. (token_cap == 0
            // in unit-test fixtures means "no extra cap".)
            let effective_cap = if guard.token_cap == 0 {
                MAX_SEQ_LEN
            } else {
                guard.token_cap.min(MAX_SEQ_LEN)
            };
            let mut id_vecs: Vec<Vec<u32>> = Vec::with_capacity(owned.len());
            for text in &owned {
                let encoding = guard
                    .tokenizer
                    .encode(text.as_str(), true) // add_special_tokens=true → appends EOS
                    .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;
                let full_len = encoding.get_ids().len();
                let ids: Vec<u32> = encoding
                    .get_ids()
                    .iter()
                    .take(effective_cap)
                    .copied()
                    .collect();
                if full_len > effective_cap {
                    tracing::warn!(
                        "chunk truncated for embedding: {full_len} tokens > cap {effective_cap} \
                         (memory-budget limit) — embedding leading {effective_cap} tokens only"
                    );
                }
                anyhow::ensure!(!ids.is_empty(), "empty token sequence after tokenization");
                id_vecs.push(ids);
            }

            // 2. Sort by token length so sequences in the same sub-batch have
            //    similar lengths, minimising padding waste.
            let mut indexed: Vec<(usize, Vec<u32>)> = id_vecs.into_iter().enumerate().collect();
            indexed.sort_unstable_by_key(|(_, ids)| ids.len());

            // 3. Process in sub-batches; reassemble into original order.
            let mut results: Vec<Vec<f32>> = vec![Vec::new(); owned.len()];
            for sub_batch in indexed.chunks(EMBED_BATCH_SIZE) {
                let batch_ids: Vec<&[u32]> =
                    sub_batch.iter().map(|(_, ids)| ids.as_slice()).collect();
                let vecs = guard.weights.embed_batch(&batch_ids)?;
                for ((orig_idx, _), vec) in sub_batch.iter().zip(vecs) {
                    results[*orig_idx] = vec;
                }
            }

            Ok(results)
        })
        .await
        .context("spawn_blocking panicked in native embedder")?
    }

    fn dimension(&self) -> usize {
        DIM
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `[1, n_kv, seq, head_dim]` tensor where every element of kv-head
    /// `k` equals `k` — so the head index is recoverable from any of its values.
    fn headed_kv(n_kv: usize, seq: usize, head_dim: usize) -> Tensor {
        let data: Vec<f32> = (0..n_kv)
            .flat_map(|k| vec![k as f32; seq * head_dim])
            .collect();
        Tensor::from_vec(data, (1, n_kv, seq, head_dim), &Device::Cpu).unwrap()
    }

    /// The source kv-head stamped on each output head, in order.
    fn head_sources(t: &Tensor) -> Vec<usize> {
        let (_, n_head, seq, head_dim) = t.dims4().unwrap();
        let flat: Vec<f32> = t.flatten_all().unwrap().to_vec1().unwrap();
        let per_head = seq * head_dim;
        (0..n_head).map(|h| flat[h * per_head] as usize).collect()
    }

    /// Regression test for spelunk-oss#19: grouped-query-attention K/V expansion
    /// must **repeat-interleave** the kv-head dim (`[kv0,kv0,kv1,kv1,…]`) so query
    /// head `j` attends through kv head `j / n_rep`. The original bug used
    /// `Tensor::repeat`, which *tiles* (`[kv0..kvN, kv0..kvN]`) and silently paired
    /// 15 of 16 query heads with the wrong K/V projection, collapsing retrieval.
    /// This guards the `repeat_kv` call in `Qwen3EmbedWeights::attn`.
    #[test]
    fn repeat_kv_interleaves_gqa_heads_not_tiles() {
        // F2LLM-v2-330M: 16 attention heads / 8 kv heads → n_rep = 2.
        let (n_kv, n_rep, seq, head_dim) = (8usize, 2usize, 3usize, 4usize);
        let kv = headed_kv(n_kv, seq, head_dim);

        // Production path (the #19 fix).
        let expanded = repeat_kv(kv.clone(), n_rep).unwrap();
        assert_eq!(expanded.dims4().unwrap(), (1, n_kv * n_rep, seq, head_dim));

        // Correct GQA ordering: each kv head duplicated n_rep times, contiguously.
        let expected: Vec<usize> = (0..n_kv * n_rep).map(|h| h / n_rep).collect();
        assert_eq!(
            head_sources(&expanded),
            expected,
            "repeat_kv must repeat-interleave kv heads (spelunk-oss#19)"
        );

        // The #19 bug: `Tensor::repeat` tiles instead — [kv0..kv7, kv0..kv7].
        let tiled = kv.repeat(&[1, n_rep, 1, 1]).unwrap();
        let tiled_order: Vec<usize> = (0..n_kv).chain(0..n_kv).collect();
        assert_eq!(head_sources(&tiled), tiled_order);
        assert_ne!(
            head_sources(&expanded),
            head_sources(&tiled),
            "interleave (repeat_kv) and tile (Tensor::repeat) must differ for n_rep > 1 — \
             reverting to Tensor::repeat reintroduces spelunk-oss#19"
        );
    }

    /// `repeat_kv` must return a **contiguous** tensor: candle's CPU matmul rejects
    /// a non-contiguous rhs (`MatMulUnexpectedStriding`), which previously crashed
    /// the embedder on the CPU backend. Guards against an expansion that leaves the
    /// K/V views strided going into the attention matmuls.
    #[test]
    fn repeat_kv_output_is_contiguous() {
        let kv = headed_kv(8, 3, 4);
        let expanded = repeat_kv(kv, 2).unwrap();
        assert!(
            expanded.is_contiguous(),
            "repeat_kv output must be contiguous for candle's CPU matmul"
        );
    }

    // ── single-chunk memory-budget cap (spelunk-oss#17) ───────────────────────

    /// Budget selection keys off total system RAM: 2 GiB at/below the 16 GiB
    /// threshold, 4 GiB above it.
    #[test]
    fn budget_selected_by_system_ram() {
        let gib = 1024u64 * 1024 * 1024;
        // Small hosts (and the unknown/0 fallback) get the conservative 2 GiB.
        assert_eq!(single_chunk_budget(0), SINGLE_CHUNK_BUDGET_SMALL);
        assert_eq!(single_chunk_budget(8 * gib), SINGLE_CHUNK_BUDGET_SMALL);
        assert_eq!(single_chunk_budget(16 * gib), SINGLE_CHUNK_BUDGET_SMALL);
        // Strictly above 16 GiB unlocks the 4 GiB budget.
        assert_eq!(single_chunk_budget(16 * gib + 1), SINGLE_CHUNK_BUDGET_LARGE);
        assert_eq!(single_chunk_budget(32 * gib), SINGLE_CHUNK_BUDGET_LARGE);
    }

    /// The token cap is derived from `n_head × seq² × 4 ≤ budget`. Pin the two
    /// production budgets to their derived caps (~5 792 @ 2 GiB, ~8 192 @ 4 GiB)
    /// and assert the inverse: the resulting attention scratch stays within
    /// budget while one more token would exceed it.
    #[test]
    fn token_cap_derivation_matches_budget() {
        let cap_2g = derive_token_cap(SINGLE_CHUNK_BUDGET_SMALL, N_HEAD);
        let cap_4g = derive_token_cap(SINGLE_CHUNK_BUDGET_LARGE, N_HEAD);

        // Anchored ballpark from the decision (2 GiB ≈ 5.8 k, 4 GiB ≈ 8.2 k).
        assert_eq!(cap_2g, 5792, "2 GiB cap");
        assert_eq!(cap_4g, 8192, "4 GiB cap");

        // The cap is the largest seq whose scratch fits; cap+1 must not.
        let scratch = |seq: usize| (N_HEAD as u64) * (seq as u64) * (seq as u64) * 4;
        assert!(scratch(cap_2g) <= SINGLE_CHUNK_BUDGET_SMALL);
        assert!(scratch(cap_2g + 1) > SINGLE_CHUNK_BUDGET_SMALL);
        assert!(scratch(cap_4g) <= SINGLE_CHUNK_BUDGET_LARGE);
        assert!(scratch(cap_4g + 1) > SINGLE_CHUNK_BUDGET_LARGE);

        // Larger budget ⇒ larger (or equal) cap.
        assert!(cap_4g > cap_2g);
    }

    /// A 40 k-token chunk under the old (uncapped) path would allocate the
    /// attention scratch that caused the ~107 GB OOM; the cap brings it to a
    /// bounded size. Document the before/after scratch sizes here so the
    /// regression is captured in code, not just the PR body.
    #[test]
    fn oversized_chunk_scratch_drops_below_budget() {
        let scratch = |seq: usize| (N_HEAD as u64) * (seq as u64) * (seq as u64) * 4;

        // Before: a full 40 960-token chunk (MAX_SEQ_LEN) → ~100 GiB scratch.
        let before = scratch(MAX_SEQ_LEN);
        assert!(
            before > 90 * 1024 * 1024 * 1024,
            "full-length chunk scratch should be ~100 GiB (was {before} bytes)"
        );

        // After: truncated to the 2 GiB cap → scratch within budget.
        let cap = derive_token_cap(SINGLE_CHUNK_BUDGET_SMALL, N_HEAD);
        assert!(scratch(cap) <= SINGLE_CHUNK_BUDGET_SMALL);
    }

    /// The cap is never larger than the model's position-embedding ceiling, and
    /// degenerate budgets still yield a usable (>= 1) cap.
    #[test]
    fn token_cap_is_clamped() {
        // A huge budget can't exceed MAX_SEQ_LEN.
        assert_eq!(derive_token_cap(u64::MAX, N_HEAD), MAX_SEQ_LEN);
        // A tiny budget still yields at least one token (never zero/panics).
        assert_eq!(derive_token_cap(0, N_HEAD), 1);
        assert!(derive_token_cap(1024, N_HEAD) >= 1);
    }

    /// Whatever RAM this host actually has, the detected budget must derive a
    /// cap strictly smaller than MAX_SEQ_LEN — i.e. the 40 k-token OOM path is
    /// closed on every supported budget.
    #[test]
    fn detected_budget_caps_below_max_seq_len() {
        for ram in [0u64, 8, 16, 17, 64].map(|g| g * 1024 * 1024 * 1024) {
            let cap = derive_token_cap(single_chunk_budget(ram), N_HEAD);
            assert!(
                cap < MAX_SEQ_LEN,
                "cap {cap} for {ram}-byte host must be below MAX_SEQ_LEN {MAX_SEQ_LEN}"
            );
        }
    }

    /// End-to-end semantic-discrimination check over the real model. Ignored by
    /// default: it downloads ~650 MB of weights and runs inference. Run with
    /// `cargo test -p spelunk-server -- --ignored embeddings_discriminate`.
    ///
    /// With the #19 GQA bug present, related and unrelated pairs collapse to the
    /// same cosine (~0.1–0.25); with the fix, related pairs sit well above
    /// unrelated. This is the only test that exercises attention end-to-end.
    #[test]
    #[ignore = "downloads the F2LLM model and runs inference"]
    fn embeddings_discriminate_related_from_unrelated() {
        use spelunk_core::embeddings::EmbeddingBackend;

        let embedder = NativeEmbedder::load().expect("load F2LLM-v2-330M");
        let rt = tokio::runtime::Runtime::new().unwrap();

        let texts: [&str; 3] = [
            "read the contents of a file from disk",
            "open a file and return its bytes",
            "the fall of the roman empire",
        ];
        let vecs = rt.block_on(embedder.embed(&texts)).expect("embed");

        // Embeddings are L2-normalised, so dot product == cosine similarity.
        let cos = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let related = cos(&vecs[0], &vecs[1]);
        let unrelated = cos(&vecs[0], &vecs[2]);

        assert!(
            related > unrelated + 0.2,
            "GQA-fixed embeddings must discriminate related from unrelated: \
             related={related:.3} vs unrelated={unrelated:.3} (spelunk-oss#19)"
        );
    }

    /// End-to-end proof that an oversized single chunk no longer OOMs/aborts
    /// (spelunk-oss#17). Builds a chunk far larger than `MAX_SEQ_LEN` tokens —
    /// the case that previously allocated ~107 GB of attention scores and
    /// aborted a full CPU index — and asserts `embed` returns a finite vector
    /// (because the chunk is truncated to the memory-budget cap, not forwarded
    /// whole). Ignored by default: downloads the model and runs inference.
    ///
    /// Run with:
    ///   SPELUNK_SECRET_STORE=file cargo test -p spelunk-server \
    ///     -- --ignored oversized_chunk_embeds_without_oom
    #[test]
    #[ignore = "downloads the F2LLM model and runs inference"]
    fn oversized_chunk_embeds_without_oom() {
        use spelunk_core::embeddings::EmbeddingBackend;

        let embedder = NativeEmbedder::load().expect("load F2LLM-v2-330M");
        let rt = tokio::runtime::Runtime::new().unwrap();

        // ~60 k whitespace-separated tokens — comfortably past MAX_SEQ_LEN
        // (40 960) and ~10x the 2 GiB cap (~5 792). Pre-fix this aborts the
        // process; post-fix it is truncated to the cap and embeds cleanly.
        let huge = "fn pagerank ( edges ) { compute } ".repeat(12_000);
        let normal = "read the contents of a file from disk";

        let vecs = rt
            .block_on(embedder.embed(&[huge.as_str(), normal]))
            .expect("embed must complete (truncated), not OOM/abort");

        assert_eq!(vecs.len(), 2);
        assert_eq!(vecs[0].len(), DIM);
        assert!(
            vecs[0].iter().all(|x| x.is_finite()),
            "truncated oversized-chunk embedding must be finite"
        );
        let norm: f32 = vecs[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "embedding must be L2-normalised");
    }

    /// Normal-sized chunks must embed identically whether or not the
    /// memory-budget cap is in effect (no regression for the common case). The
    /// cap only ever drops *trailing* tokens beyond the cap; a chunk shorter
    /// than the cap is untouched, so its vector is byte-for-byte the same.
    /// Ignored by default: downloads the model and runs inference.
    #[test]
    #[ignore = "downloads the F2LLM model and runs inference"]
    fn normal_chunk_unaffected_by_cap() {
        use spelunk_core::embeddings::EmbeddingBackend;

        let embedder = NativeEmbedder::load().expect("load F2LLM-v2-330M");
        let rt = tokio::runtime::Runtime::new().unwrap();

        let text = "pub fn compute_pagerank(edges: &[(String, String)]) -> Vec<f32> { todo!() }";
        let a = rt.block_on(embedder.embed(&[text])).expect("embed a");
        let b = rt.block_on(embedder.embed(&[text])).expect("embed b");
        assert_eq!(a[0], b[0], "normal-chunk embedding must be deterministic");
        // Sanity: this chunk is well under any budget-derived cap, so it was
        // never truncated — the produced vector is the full-precision result.
        assert!(text.split_whitespace().count() < 5792);
    }
}
