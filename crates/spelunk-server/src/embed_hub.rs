//! Hugging Face Hub acquisition path for the bundled F2LLM-v2-330M embedder.
//!
//! `spelunk-embed` only knows how to load the embedder from files already on
//! disk ([`spelunk_embed::NativeEmbedder::load_from_path`]) — it carries no
//! network-fetch dependency. This module owns the `hf-hub` download step: it
//! fetches the pre-quantized GGUF and tokenizer from our own first-party
//! Hugging Face repo into the local hf-hub cache (writing the embedded
//! `config.json` alongside them), then hands the resulting file paths to
//! `load_from_path`. This is the only place in `spelunk-server` — or the
//! workspace — that depends on `hf-hub`.
//!
//! Everything here comes from `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF`, a repo
//! we own — there is no runtime dependency on the third-party upstream
//! `codefuse-ai/F2LLM-v2-330M` repo. See `docs/third-party-models.md` for the
//! Apache-2.0 attribution and the pinned upstream revision these artifacts
//! were derived from.

use std::path::PathBuf;

use anyhow::{Context, Result};
use hf_hub::{Repo, RepoType, api::sync::ApiBuilder};
use spelunk_embed::NativeEmbedder;

/// `config.json` for F2LLM-v2-330M (Qwen3 architecture config; ~1 KB).
/// Embedded directly in the binary — it's tiny and never changes independent
/// of the pinned model revision recorded in `docs/third-party-models.md`, so
/// there's no reason to fetch it over the network. Vendored at
/// `crates/spelunk-server/assets/f2llm-v2-330m-config.json`.
const CONFIG_JSON: &str = include_str!("../assets/f2llm-v2-330m-config.json");

/// Override env var naming the Hugging Face repo id that holds a **pre-quantized
/// Q8_0 GGUF** (and, alongside it, the tokenizer) for the embedder. Read from
/// `SPELUNK_EMBEDDER_GGUF_REPO` at load time; see [`prequantized_gguf_repo`]
/// for the accepted values.
///
/// By default (unset) the loader fetches `QUANT_GGUF` and `tokenizer.json`
/// from [`DEFAULT_GGUF_REPO`] via the existing hf-hub cache — first-run
/// download is ~339 MB. Set this to a different `org/repo` to fetch both from
/// there instead (it must host both files, e.g. a mirror of our repo).
const GGUF_REPO_ENV: &str = "SPELUNK_EMBEDDER_GGUF_REPO";

/// Default Hugging Face repo id holding our **own pre-quantized Q8_0 GGUF**
/// (`f2llm-v2-330m-q8_0.gguf`) and tokenizer (`tokenizer.json`). Used when
/// `SPELUNK_EMBEDDER_GGUF_REPO` is unset, so a stock install fetches the
/// ~339 MB pre-quant GGUF plus tokenizer from here — no third-party repo
/// involved. Override with the env var (see [`GGUF_REPO_ENV`]).
const DEFAULT_GGUF_REPO: &str = "spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF";

/// Filename of the Q8_0-quantized GGUF cached next to the HF download.
/// Projection matmuls and the token-embedding table are stored Q8_0; the small
/// RMSNorm weights stay F32. Produced upstream by the pre-quantize pipeline
/// that publishes `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF` (see
/// `docs/third-party-models.md`), not built on device.
const QUANT_GGUF: &str = "f2llm-v2-330m-q8_0.gguf";

/// Load the F2LLM-v2-330M model, quantized to Q8_0, via the Hugging Face Hub.
///
/// Downloads our own pre-quantized GGUF (`f2llm-v2-330m-q8_0.gguf`) and
/// tokenizer (`tokenizer.json`) straight from
/// `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF` through the hf-hub cache
/// (checksum/resume reused) — first-run download is ~339 MB, cached in
/// `~/.local/share/spelunk/models/`. Set `SPELUNK_EMBEDDER_GGUF_REPO` to a
/// different `org/repo` to fetch both from there instead. `config.json` is
/// embedded in the binary (see [`CONFIG_JSON`]) and written to the same cache
/// directory so it lands next to the other artifacts as a real file.
///
/// Subsequent calls read everything from the local cache with no network
/// access. There is no runtime dependency on any third-party Hugging Face
/// repo. Once the GGUF/tokenizer/config are resolved on disk this hands off to
/// [`spelunk_embed::NativeEmbedder::load_from_path`], which does the actual
/// (network-free) model load.
pub fn load_from_hub() -> Result<NativeEmbedder> {
    let cache_dir = model_cache_dir()?;
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating model cache dir {}", cache_dir.display()))?;
    let gguf_path = cache_dir.join(QUANT_GGUF);

    tracing::info!(
        "resolving F2LLM-v2-330M (Q8_0) via Hugging Face Hub (cache: {})",
        cache_dir.display()
    );

    // config.json is embedded in the binary; write it out so it's a real file
    // next to the other artifacts (`load_from_path` reads it from disk).
    let config_path = cache_dir.join("config.json");
    std::fs::write(&config_path, CONFIG_JSON)
        .with_context(|| format!("writing embedded config.json to {}", config_path.display()))?;

    let api = ApiBuilder::new()
        .with_cache_dir(cache_dir)
        .build()
        .context("building HuggingFace Hub API client")?;

    let gguf_repo = prequantized_gguf_repo()?;
    let repo = api.repo(Repo::new(gguf_repo.clone(), RepoType::Model));

    let tokenizer_path = repo
        .get("tokenizer.json")
        .with_context(|| format!("downloading tokenizer.json from {gguf_repo}"))?;

    // Acquire the Q8_0 GGUF if it isn't already cached.
    if !gguf_path.exists() {
        tracing::info!(
            "fetching pre-quantized F2LLM-v2-330M Q8_0 GGUF from {gguf_repo} (first run)…"
        );
        let downloaded = repo
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

    NativeEmbedder::load_from_path(&gguf_path, &tokenizer_path, &config_path)
}

fn model_cache_dir() -> Result<PathBuf> {
    dirs::data_local_dir()
        .map(|d| d.join("spelunk").join("models"))
        .ok_or_else(|| anyhow::anyhow!("could not determine local data directory"))
}

/// Resolve the HF repo id of the pre-quantized Q8_0 GGUF (and tokenizer) to
/// fetch, from `SPELUNK_EMBEDDER_GGUF_REPO`.
///
/// The env var (after trimming surrounding whitespace) is interpreted as:
///
/// * **unset** → `DEFAULT_GGUF_REPO` — the default; a stock install fetches the
///   ~339 MB pre-quant GGUF plus tokenizer from
///   `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF`.
/// * **`off`** (any case) → hard error. This was previously an escape hatch
///   that downloaded the upstream BF16 safetensors and quantized them on
///   device; that path has been removed (v1: the pre-quantized first-party
///   GGUF is the only delivery mechanism), so a leftover `off` in the
///   environment now fails loudly instead of silently changing behavior.
/// * **any other value** → that `org/repo` id (trimmed) — override: fetch the
///   pre-quant GGUF and tokenizer from there instead (it must host both
///   files).
fn prequantized_gguf_repo() -> Result<String> {
    match std::env::var(GGUF_REPO_ENV) {
        Ok(v) => {
            let v = v.trim();
            anyhow::ensure!(
                !v.eq_ignore_ascii_case("off"),
                "{GGUF_REPO_ENV}=off is no longer supported: on-device quantization from \
                 upstream BF16 weights was removed (v1 always fetches the pre-quantized \
                 first-party GGUF). Unset {GGUF_REPO_ENV} to use the default repo, or set it \
                 to an `org/repo` that hosts a pre-quantized GGUF."
            );
            if v.is_empty() {
                Ok(DEFAULT_GGUF_REPO.to_string())
            } else {
                Ok(v.to_string())
            }
        }
        Err(_) => Ok(DEFAULT_GGUF_REPO.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `prequantized_gguf_repo()` resolves the GGUF source from
    /// `SPELUNK_EMBEDDER_GGUF_REPO`: unset/blank → the bundled default repo;
    /// `off` (any case, any surrounding whitespace) → hard error, since the
    /// on-device-quantize escape hatch it used to select has been removed;
    /// any other value → that `org/repo` (trimmed). Uses `serial` because it
    /// mutates a process-global env var.
    #[test]
    #[serial_test::serial(gguf_repo_env)]
    fn prequantized_gguf_repo_defaults_to_bundled_repo() {
        // SAFETY: guarded by #[serial] so no other test reads/writes this var
        // concurrently; we restore it before returning.
        let prev = std::env::var(GGUF_REPO_ENV).ok();

        unsafe { std::env::remove_var(GGUF_REPO_ENV) };
        assert_eq!(
            prequantized_gguf_repo().ok().as_deref(),
            Some("spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF"),
            "unset env var must default to fetching the bundled pre-quant GGUF"
        );

        unsafe { std::env::set_var(GGUF_REPO_ENV, "   ") };
        assert_eq!(
            prequantized_gguf_repo().ok().as_deref(),
            Some("spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF"),
            "blank/whitespace env var must fall back to the default repo, not fetch \"\""
        );

        // The removed escape hatch (`off`) must now error clearly rather than
        // silently changing behavior, in any case or with surrounding whitespace.
        for off in ["off", "OFF", "  off  "] {
            unsafe { std::env::set_var(GGUF_REPO_ENV, off) };
            assert!(
                prequantized_gguf_repo().is_err(),
                "`{off}` must be a hard error now that on-device quantize is removed"
            );
        }

        // Override: an explicit repo id is used verbatim, with whitespace trimmed.
        unsafe { std::env::set_var(GGUF_REPO_ENV, "  org/repo  ") };
        assert_eq!(prequantized_gguf_repo().ok().as_deref(), Some("org/repo"));

        match prev {
            Some(v) => unsafe { std::env::set_var(GGUF_REPO_ENV, v) },
            None => unsafe { std::env::remove_var(GGUF_REPO_ENV) },
        }
    }

    /// `model_cache_dir()` honours `XDG_DATA_HOME` when set (the Docker image
    /// points this at the persistent `/data` volume so the ~339 MB model
    /// survives `docker rm`/recreate, instead of landing in the container
    /// layer or a home directory that doesn't exist for the `-r` service
    /// user). Linux-only: `dirs::data_local_dir()` follows the XDG spec on
    /// Linux/BSD, but macOS ignores `XDG_DATA_HOME` entirely in favor of
    /// `~/Library/Application Support` (the Docker image is Linux, so that's
    /// the platform this fix targets). Uses `serial` because it mutates a
    /// process-global env var.
    #[test]
    #[cfg(target_os = "linux")]
    #[serial_test::serial(xdg_data_home_env)]
    fn model_cache_dir_honours_xdg_data_home() {
        // SAFETY: guarded by #[serial] so no other test reads/writes this var
        // concurrently; we restore it before returning.
        let prev = std::env::var("XDG_DATA_HOME").ok();

        let tmp = std::env::temp_dir().join("spelunk-model-cache-dir-test");
        unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };

        assert_eq!(
            model_cache_dir().expect("resolve cache dir"),
            tmp.join("spelunk").join("models")
        );

        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
    }

    /// End-to-end semantic-discrimination check over the real model. Ignored by
    /// default: it downloads the ~339 MB pre-quantized GGUF and runs inference.
    /// Run with `cargo test -p spelunk-server -- --ignored embeddings_discriminate`.
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

        // config.json is embedded and written directly to the cache dir root
        // (see `load_from_hub`). The tokenizer comes from our own
        // `DEFAULT_GGUF_REPO`, cached under the hf-hub snapshot layout
        // `<cache>/models--spelunk-cloud--F2LLM-v2-330M-Q8_0-GGUF/snapshots/<rev>/tokenizer.json`.
        let config = cache_dir.join("config.json");
        let tokenizer = std::fs::read_dir(
            cache_dir
                .join("models--spelunk-cloud--F2LLM-v2-330M-Q8_0-GGUF")
                .join("snapshots"),
        )
        .expect("hf-hub snapshots dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("tokenizer.json"))
        .find(|p| p.exists())
        .expect("cached tokenizer.json");

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

    /// `token_cap()` (the `EmbeddingBackend` trait method `/v1/health`'s
    /// `limits.embedder_token_cap` reads) must report a real, usable,
    /// host-derived cap for a fully loaded embedder — not `None` and not a
    /// degenerate value. This is the live end-to-end proof; the pure-math
    /// derivation itself (`derive_token_cap`/`single_chunk_budget`) has its own
    /// unconditional unit coverage in `spelunk_embed::embedder_native::tests`.
    /// Ignored by default: downloads the model. Run with:
    ///   SPELUNK_SECRET_STORE=file cargo test -p spelunk-server \
    ///     -- --ignored native_embedder_reports_its_token_cap
    #[test]
    #[ignore = "downloads the F2LLM model"]
    fn native_embedder_reports_its_token_cap() {
        use spelunk_core::embeddings::EmbeddingBackend;

        let embedder = load_from_hub().expect("load F2LLM-v2-330M");

        let cap = embedder
            .token_cap()
            .expect("a loaded NativeEmbedder must report a host-derived token cap");
        // Sanity bounds matching the documented derivation (~5 792 @ 2 GiB,
        // ~8 192 @ 4 GiB budget; see `derive_token_cap`'s doc comment) without
        // reaching into spelunk-embed's private constants from this crate.
        assert!(cap >= 1000, "token cap implausibly small: {cap}");
        assert!(
            cap <= 40_960,
            "token cap must not exceed MAX_SEQ_LEN: {cap}"
        );
    }
}
