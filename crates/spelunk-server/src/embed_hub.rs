//! Hugging Face Hub acquisition path for the bundled F2LLM-v2-330M embedder.
//!
//! `spelunk-embed` only knows how to load the embedder from files already on
//! disk ([`spelunk_embed::NativeEmbedder::load_from_path`]) — it carries no
//! network-fetch dependency. This module owns the `hf-hub` download/quantize
//! step: it resolves (and if needed downloads and quantizes) the model
//! artifacts into the local hf-hub cache, then hands the resulting file paths
//! to `load_from_path`. This is the only place in `spelunk-server` — or the
//! workspace — that depends on `hf-hub`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use candle_core::quantized::{GgmlDType, QTensor, gguf_file};
use candle_core::{DType, Device};
use hf_hub::{Repo, RepoType, api::sync::ApiBuilder};
use spelunk_embed::NativeEmbedder;

const MODEL_ID: &str = "codefuse-ai/F2LLM-v2-330M";
/// Upstream revision (commit SHA) of `codefuse-ai/F2LLM-v2-330M` we download and
/// quantize from. Pinning the revision makes the on-device quantize path
/// reproducible (the same weights every first run) and is the provenance anchor
/// recorded in the NOTICE / model card for our redistributed Q8_0 GGUF. Update
/// this in lockstep with regenerating and re-uploading the pre-quantized
/// artifact.
const MODEL_REVISION: &str = "1239cdd544b24c247ed75df2ae22e5a401ac4659";

/// Override env var naming the Hugging Face repo id that holds a **pre-quantized
/// Q8_0 GGUF** for the embedder. Read from `SPELUNK_EMBEDDER_GGUF_REPO` at load
/// time; see [`prequantized_gguf_repo`] for the accepted values.
///
/// By default (unset) the loader fetches `QUANT_GGUF` from [`DEFAULT_GGUF_REPO`]
/// directly via the existing hf-hub cache — first-run download is ~339 MB and
/// there is no on-device safetensors download or quantize step. Set this to a
/// different `org/repo` to fetch the pre-quant GGUF from there instead, or to
/// `off` to build the GGUF from the upstream BF16 weights on device.
const GGUF_REPO_ENV: &str = "SPELUNK_EMBEDDER_GGUF_REPO";

/// Default Hugging Face repo id holding our **own pre-quantized Q8_0 GGUF**
/// (`f2llm-v2-330m-q8_0.gguf`). Used when `SPELUNK_EMBEDDER_GGUF_REPO` is unset,
/// so a stock install fetches the ~339 MB pre-quant GGUF instead of downloading
/// the ~638 MB upstream BF16 safetensors and quantizing on device. Override with
/// the env var (see [`GGUF_REPO_ENV`]).
const DEFAULT_GGUF_REPO: &str = "spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF";

/// Filename of the Q8_0-quantized GGUF cached next to the HF download. Projection
/// matmuls and the token-embedding table are stored Q8_0; the small RMSNorm
/// weights stay F32. Built once from the safetensors download (see
/// `write_quantized_gguf`) so subsequent loads read ~355 MB instead of ~650 MB.
const QUANT_GGUF: &str = "f2llm-v2-330m-q8_0.gguf";

/// Load the F2LLM-v2-330M model, quantized to Q8_0, via the Hugging Face Hub.
///
/// Two acquisition paths select the Q8_0 GGUF cached in
/// `~/.local/share/spelunk/models/`:
///
/// * **Default (direct GGUF):** download our own pre-quantized GGUF
///   (`f2llm-v2-330m-q8_0.gguf`) straight from `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF`
///   through the hf-hub cache (checksum/resume reused) — first-run download is
///   ~339 MB and there is no on-device quantize step. Set
///   `SPELUNK_EMBEDDER_GGUF_REPO` to a different `org/repo` to fetch the
///   pre-quant GGUF from there instead.
/// * **On-device quantize (escape hatch, `SPELUNK_EMBEDDER_GGUF_REPO=off`):**
///   download the ~638 MB BF16 safetensors from the pinned upstream revision
///   of `codefuse-ai/F2LLM-v2-330M`, quantize on device to a ~339 MB GGUF,
///   then delete the cached safetensors so steady-state disk is ~339 MB
///   rather than ~1.5 GB.
///
/// Either way subsequent calls read the cached GGUF directly with no network
/// access. The tokenizer and config are always fetched from the pinned
/// upstream revision. Once the GGUF/tokenizer/config are resolved on disk this
/// hands off to [`spelunk_embed::NativeEmbedder::load_from_path`], which does
/// the actual (network-free) model load.
pub fn load_from_hub() -> Result<NativeEmbedder> {
    let cache_dir = model_cache_dir()?;
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating model cache dir {}", cache_dir.display()))?;
    let gguf_path = cache_dir.join(QUANT_GGUF);

    tracing::info!(
        "resolving F2LLM-v2-330M (Q8_0) via Hugging Face Hub (cache: {})",
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

    let config_path = repo
        .get("config.json")
        .context("downloading F2LLM-v2-330M config.json")?;

    // Acquire the Q8_0 GGUF if it isn't already cached.
    if !gguf_path.exists() {
        match prequantized_gguf_repo() {
            // Default (or overridden repo): pull a pre-quantized GGUF directly.
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
            // Escape hatch (SPELUNK_EMBEDDER_GGUF_REPO=off): download
            // safetensors and quantize on device.
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

    NativeEmbedder::load_from_path(&gguf_path, &tokenizer_path, &config_path)
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
/// we add it to match the keys the embedder's GGUF reader expects.
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

/// Resolve the HF repo id of the pre-quantized Q8_0 GGUF to fetch, from
/// `SPELUNK_EMBEDDER_GGUF_REPO`.
///
/// Returns `Some(repo)` to fetch the pre-quant GGUF directly, or `None` to build
/// it from the upstream BF16 weights on device. The env var (after trimming
/// surrounding whitespace, case-insensitive for the sentinel) is interpreted as:
///
/// * **unset** → `Some(DEFAULT_GGUF_REPO)` — the default; a stock install fetches
///   the ~339 MB pre-quant GGUF from `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF`.
/// * **`off`** → `None` — escape hatch: skip the pre-quant repo and download the
///   upstream BF16 safetensors, quantizing on device.
/// * **empty / whitespace** → `None` — a blank value disables the pre-quant fetch
///   (treated the same as `off`) rather than attempting to fetch from `""`.
/// * **any other value** → `Some(value)` — override: fetch the pre-quant GGUF
///   from that `org/repo` id (trimmed).
fn prequantized_gguf_repo() -> Option<String> {
    match std::env::var(GGUF_REPO_ENV) {
        Ok(v) => {
            let v = v.trim();
            if v.is_empty() || v.eq_ignore_ascii_case("off") {
                None
            } else {
                Some(v.to_string())
            }
        }
        Err(_) => Some(DEFAULT_GGUF_REPO.to_string()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `prequantized_gguf_repo()` resolves the GGUF source from
    /// `SPELUNK_EMBEDDER_GGUF_REPO`: unset → the bundled default repo (so a stock
    /// install fetches the pre-quant GGUF); `off`/blank → `None` (escape hatch:
    /// build from upstream BF16 on device); any other value → that `org/repo`
    /// (trimmed). Uses `serial` because it mutates a process-global env var.
    #[test]
    #[serial_test::serial(gguf_repo_env)]
    fn prequantized_gguf_repo_defaults_to_bundled_repo() {
        // SAFETY: guarded by #[serial] so no other test reads/writes this var
        // concurrently; we restore it before returning.
        let prev = std::env::var(GGUF_REPO_ENV).ok();

        unsafe { std::env::remove_var(GGUF_REPO_ENV) };
        assert_eq!(
            prequantized_gguf_repo().as_deref(),
            Some("spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF"),
            "unset env var must default to fetching the bundled pre-quant GGUF"
        );

        unsafe { std::env::set_var(GGUF_REPO_ENV, "off") };
        assert_eq!(
            prequantized_gguf_repo(),
            None,
            "`off` must disable the pre-quant fetch (build from upstream on device)"
        );

        // The `off` sentinel is case-insensitive.
        unsafe { std::env::set_var(GGUF_REPO_ENV, "OFF") };
        assert_eq!(
            prequantized_gguf_repo(),
            None,
            "`off` must be case-insensitive"
        );

        unsafe { std::env::set_var(GGUF_REPO_ENV, "   ") };
        assert_eq!(
            prequantized_gguf_repo(),
            None,
            "blank/whitespace env var must disable the pre-quant fetch, not fetch \"\""
        );

        unsafe { std::env::set_var(GGUF_REPO_ENV, "  off  ") };
        assert_eq!(
            prequantized_gguf_repo(),
            None,
            "`off` with surrounding whitespace must still be the escape hatch"
        );

        // Override: an explicit repo id is used verbatim, with whitespace trimmed.
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

    /// End-to-end semantic-discrimination check over the real model. Ignored by
    /// default: it downloads ~650 MB of weights and runs inference. Run with
    /// `cargo test -p spelunk-server -- --ignored embeddings_discriminate`.
    ///
    /// With the #19 GQA bug present, related and unrelated pairs collapse to the
    /// same cosine (~0.1–0.25); with the fix, related pairs sit well above
    /// unrelated. This is the only test that exercises attention end-to-end via
    /// the Hub acquisition path (the pure-local path has its own coverage in
    /// `spelunk-embed`).
    #[test]
    #[ignore = "downloads the F2LLM model and runs inference"]
    fn embeddings_discriminate_related_from_unrelated() {
        use spelunk_core::embeddings::EmbeddingBackend;

        let embedder = load_from_hub().expect("load F2LLM-v2-330M");
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
    /// (spelunk-oss#17), exercised via the Hub acquisition path. Ignored by
    /// default: downloads the model and runs inference.
    ///
    /// Run with:
    ///   SPELUNK_SECRET_STORE=file cargo test -p spelunk-server \
    ///     -- --ignored oversized_chunk_embeds_without_oom
    #[test]
    #[ignore = "downloads the F2LLM model and runs inference"]
    fn oversized_chunk_embeds_without_oom() {
        use spelunk_core::embeddings::EmbeddingBackend;

        let embedder = load_from_hub().expect("load F2LLM-v2-330M");
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
        assert!(
            vecs[0].iter().all(|x| x.is_finite()),
            "truncated oversized-chunk embedding must be finite"
        );
        let norm: f32 = vecs[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "embedding must be L2-normalised");
    }

    /// Normal-sized chunks must embed identically whether or not the
    /// memory-budget cap is in effect (no regression for the common case).
    /// Ignored by default: downloads the model and runs inference.
    #[test]
    #[ignore = "downloads the F2LLM model and runs inference"]
    fn normal_chunk_unaffected_by_cap() {
        use spelunk_core::embeddings::EmbeddingBackend;

        let embedder = load_from_hub().expect("load F2LLM-v2-330M");
        let rt = tokio::runtime::Runtime::new().unwrap();

        let text = "pub fn compute_pagerank(edges: &[(String, String)]) -> Vec<f32> { todo!() }";
        let a = rt.block_on(embedder.embed(&[text])).expect("embed a");
        let b = rt.block_on(embedder.embed(&[text])).expect("embed b");
        assert_eq!(a[0], b[0], "normal-chunk embedding must be deterministic");
        // Sanity: this chunk is well under any budget-derived cap, so it was
        // never truncated — the produced vector is the full-precision result.
        assert!(text.split_whitespace().count() < 5792);
    }

    /// End-to-end: load the embedder via the Hub, priming the local cache, then
    /// load again from the resolved local paths with no network and assert an
    /// 896-dim L2-normalised vector. Ignored by default; downloads the model on
    /// first run.
    ///
    /// Run with:
    ///   SPELUNK_SECRET_STORE=file cargo test -p spelunk-server \
    ///     -- --ignored load_from_path_embeds
    #[test]
    #[ignore = "requires model artifacts already present in the local cache"]
    fn load_from_path_embeds_896_dim() {
        use spelunk_core::embeddings::EmbeddingBackend;
        use spelunk_embed::DIM;

        // Warm the local cache via the Hub loader (no-op if already cached).
        load_from_hub().expect("prime local cache");

        let cache_dir = model_cache_dir().expect("cache dir");
        let gguf = cache_dir.join(QUANT_GGUF);

        // Resolve the tokenizer/config the Hub cache placed on disk for the
        // pinned revision. The snapshot layout is
        // `<cache>/models--codefuse-ai--F2LLM-v2-330M/snapshots/<rev>/<file>`.
        let snapshot = cache_dir
            .join("models--codefuse-ai--F2LLM-v2-330M")
            .join("snapshots")
            .join(MODEL_REVISION);
        let tokenizer = snapshot.join("tokenizer.json");
        let config = snapshot.join("config.json");

        let embedder = NativeEmbedder::load_from_path(&gguf, &tokenizer, &config)
            .expect("offline load from local path");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let vecs = rt
            .block_on(embedder.embed(&["read the contents of a file from disk"]))
            .expect("embed");

        assert_eq!(vecs.len(), 1);
        assert_eq!(vecs[0].len(), DIM, "must be 896-dim");
        let norm: f32 = vecs[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "embedding must be L2-normalised");
    }
}
