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
/// Upstream revision (commit SHA) of `codefuse-ai/F2LLM-v2-330M` we download and
/// quantize from. Pinning the revision makes the on-device quantize path
/// reproducible (the same weights every first run) and is the provenance anchor
/// recorded in the NOTICE / model card for our redistributed Q8_0 GGUF. Update
/// this in lockstep with regenerating and re-uploading the pre-quantized
/// artifact.
const MODEL_REVISION: &str = "1239cdd544b24c247ed75df2ae22e5a401ac4659";

/// Optional Hugging Face repo id holding our **own pre-quantized Q8_0 GGUF**
/// (e.g. `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF`). Read from
/// `SPELUNK_EMBEDDER_GGUF_REPO` at load time.
///
/// When set, `load()` downloads `QUANT_GGUF` directly from that repo via the
/// existing hf-hub cache — first-run download drops to ~339 MB and there is no
/// on-device safetensors download or quantize step. When unset (the default),
/// the loader keeps the download-safetensors-and-quantize-on-device path, so
/// merging this change alters nothing until the GGUF repo exists and an operator
/// sets the env var. Activation = upload the artifact, then default this on.
const GGUF_REPO_ENV: &str = "SPELUNK_EMBEDDER_GGUF_REPO";

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
    /// Two acquisition paths select the Q8_0 GGUF cached in
    /// `~/.local/share/spelunk/models/`:
    ///
    /// * **Default (dev / pre-activation):** download the ~638 MB BF16
    ///   safetensors from the pinned upstream revision of
    ///   `codefuse-ai/F2LLM-v2-330M`, quantize on device to a ~339 MB GGUF
    ///   (`f2llm-v2-330m-q8_0.gguf`), then delete the cached safetensors so
    ///   steady-state disk is ~339 MB rather than ~1.5 GB.
    /// * **Direct GGUF (activated via `SPELUNK_EMBEDDER_GGUF_REPO`):** download
    ///   our own pre-quantized GGUF straight from the named HF repo through the
    ///   same hf-hub cache (checksum/resume reused) — first-run download is
    ///   ~339 MB and there is no on-device quantize step.
    ///
    /// Either way subsequent calls read the cached GGUF directly with no network
    /// access. The tokenizer and config are always fetched from the pinned
    /// upstream revision. Uses Metal/GPU on macOS when built with the `metal`
    /// cargo feature, CPU otherwise.
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
        // Tokenizer + config always come from the pinned upstream revision.
        let repo = api.repo(Repo::with_revision(
            MODEL_ID.to_string(),
            RepoType::Model,
            MODEL_REVISION.to_string(),
        ));

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

        // Acquire the Q8_0 GGUF if it isn't already cached.
        if !gguf_path.exists() {
            match prequantized_gguf_repo() {
                // Activated: pull our own pre-quantized GGUF directly.
                Some(gguf_repo) => {
                    tracing::info!(
                        "fetching pre-quantized F2LLM-v2-330M Q8_0 GGUF from {gguf_repo} (first run)…"
                    );
                    let downloaded = api
                        .repo(Repo::new(gguf_repo.clone(), RepoType::Model))
                        .get(QUANT_GGUF)
                        .with_context(|| format!("downloading {QUANT_GGUF} from {gguf_repo}"))?;
                    // hf-hub returns a path inside its own blob/snapshot layout;
                    // copy it to the stable cache path the loader reads from.
                    if downloaded != gguf_path {
                        std::fs::copy(&downloaded, &gguf_path).with_context(|| {
                            format!(
                                "caching {} -> {}",
                                downloaded.display(),
                                gguf_path.display()
                            )
                        })?;
                    }
                    tracing::info!("fetched pre-quantized model to {}", gguf_path.display());
                }
                // Default / dev fallback: download safetensors and quantize.
                None => {
                    tracing::info!("quantizing F2LLM-v2-330M to Q8_0 GGUF (first run; one-time)…");
                    let weight_paths = download_weights(&repo)?;
                    write_quantized_gguf(&weight_paths, &gguf_path)
                        .context("writing quantized F2LLM-v2-330M GGUF")?;
                    tracing::info!("wrote quantized model to {}", gguf_path.display());
                    // Reclaim ~638 MB: the BF16 safetensors are only needed to
                    // build the GGUF; the loader reads the GGUF from here on.
                    cleanup_safetensors(&weight_paths);
                }
            }
        }

        let weights = Qwen3EmbedWeights::from_gguf(&gguf_path, &config, &device)
            .context("loading quantized F2LLM-v2-330M weights")?;

        tracing::info!(
            "F2LLM-v2-330M ready (dim={DIM}, Q8_0, device={})",
            if on_gpu { "Metal/GPU" } else { "CPU" }
        );

        Ok(Self {
            inner: Arc::new(Mutex::new(EmbedderInner { weights, tokenizer })),
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

/// HF repo id of our pre-quantized Q8_0 GGUF, from `SPELUNK_EMBEDDER_GGUF_REPO`.
///
/// Returns `None` (the default) when the variable is unset or blank, keeping the
/// download-safetensors-and-quantize path active. This is the single config gate
/// that activates direct-GGUF distribution; flipping the default to
/// `Some("spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF")` is the activation step, done
/// only once the artifact has been uploaded.
fn prequantized_gguf_repo() -> Option<String> {
    std::env::var(GGUF_REPO_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Delete the cached BF16 safetensors after the Q8_0 GGUF has been written.
///
/// The hf-hub cache stores each file as a content-addressed blob under `blobs/`
/// with a symlink from `snapshots/<rev>/<file>`. We resolve the symlink to its
/// blob and remove both, reclaiming ~638 MB; the loader only ever reads the
/// GGUF after this point. Best-effort: a failure to reclaim disk is logged, not
/// fatal (the GGUF is already written and valid).
fn cleanup_safetensors(weight_paths: &[PathBuf]) {
    let mut reclaimed: u64 = 0;
    for path in weight_paths {
        // Resolve the blob the snapshot symlink points at (if it is one).
        let blob = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        for target in [&blob, path] {
            match std::fs::metadata(target) {
                Ok(meta) if meta.is_file() => {
                    let len = meta.len();
                    match std::fs::remove_file(target) {
                        Ok(()) => reclaimed += len,
                        Err(e) => tracing::warn!(
                            "could not delete cached safetensors {}: {e}",
                            target.display()
                        ),
                    }
                }
                _ => {}
            }
            if blob == *path {
                break; // not a symlink; only one path to remove
            }
        }
    }
    if reclaimed > 0 {
        tracing::info!(
            "reclaimed {:.0} MB by deleting cached BF16 safetensors after quantization",
            reclaimed as f64 / 1_048_576.0
        );
    }
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
            let mut id_vecs: Vec<Vec<u32>> = Vec::with_capacity(owned.len());
            for text in &owned {
                let encoding = guard
                    .tokenizer
                    .encode(text.as_str(), true) // add_special_tokens=true → appends EOS
                    .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;
                let ids: Vec<u32> = encoding
                    .get_ids()
                    .iter()
                    .take(MAX_SEQ_LEN)
                    .copied()
                    .collect();
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

    /// The direct-GGUF path is gated: with `SPELUNK_EMBEDDER_GGUF_REPO` unset or
    /// blank, `prequantized_gguf_repo()` returns `None`, so `load()` keeps the
    /// default download-safetensors-and-quantize path. This is what makes the PR
    /// safe to merge before the HF GGUF repo exists — merging changes nothing
    /// until an operator sets the variable. Uses `serial` because it mutates a
    /// process-global env var.
    #[test]
    #[serial_test::serial(gguf_repo_env)]
    fn prequantized_gguf_repo_gate_defaults_off() {
        // SAFETY: guarded by #[serial] so no other test reads/writes this var
        // concurrently; we restore it before returning.
        let prev = std::env::var(GGUF_REPO_ENV).ok();

        unsafe { std::env::remove_var(GGUF_REPO_ENV) };
        assert_eq!(
            prequantized_gguf_repo(),
            None,
            "unset env var must keep the quantize-on-first-run default path"
        );

        unsafe { std::env::set_var(GGUF_REPO_ENV, "   ") };
        assert_eq!(
            prequantized_gguf_repo(),
            None,
            "blank/whitespace env var must be treated as unset"
        );

        unsafe { std::env::set_var(GGUF_REPO_ENV, "spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF") };
        assert_eq!(
            prequantized_gguf_repo().as_deref(),
            Some("spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF"),
            "set env var must activate the direct-GGUF path with the repo id"
        );

        // Trimming: surrounding whitespace is stripped from the repo id.
        unsafe { std::env::set_var(GGUF_REPO_ENV, "  org/repo  ") };
        assert_eq!(prequantized_gguf_repo().as_deref(), Some("org/repo"));

        match prev {
            Some(v) => unsafe { std::env::set_var(GGUF_REPO_ENV, v) },
            None => unsafe { std::env::remove_var(GGUF_REPO_ENV) },
        }
    }

    /// `cleanup_safetensors` removes the cached weight files (reclaiming disk)
    /// and tolerates already-absent paths without erroring — it is best-effort
    /// and must never fail the load after the GGUF is already written.
    #[test]
    fn cleanup_safetensors_removes_files_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("model-00001-of-00002.safetensors");
        let b = dir.path().join("model-00002-of-00002.safetensors");
        std::fs::write(&a, vec![0u8; 4096]).unwrap();
        std::fs::write(&b, vec![0u8; 4096]).unwrap();
        assert!(a.exists() && b.exists());

        let paths = vec![a.clone(), b.clone()];
        cleanup_safetensors(&paths);
        assert!(!a.exists(), "safetensors must be deleted to reclaim disk");
        assert!(!b.exists(), "safetensors must be deleted to reclaim disk");

        // Second call over now-missing paths must not panic or error.
        cleanup_safetensors(&paths);
    }

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
}
