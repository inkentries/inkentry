//! Hugging Face Hub acquisition path for the bundled F2LLM-v2-330M embedder.
//!
//! `inkentry-embed` only knows how to load the embedder from files already on
//! disk ([`inkentry_embed::NativeEmbedder::load_from_path`]) — it carries no
//! network-fetch dependency. This module owns the `hf-hub` download step: it
//! fetches the pre-quantized GGUF and tokenizer from our own first-party
//! Hugging Face repo into the local hf-hub cache (writing the embedded
//! `config.json` alongside them), then hands the resulting file paths to
//! `load_from_path`. This is the only place in `inkentry-server` — or the
//! workspace — that depends on `hf-hub`.
//!
//! [`load_from_model_dir`] is the air-gapped counterpart: it resolves the
//! same artifacts from an operator-provisioned directory instead of the Hub,
//! with no `hf_hub` involvement at all (see "Air-gapped / no-egress install"
//! in `docs/server-setup.md`).
//!
//! Everything here comes from `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF`, a repo
//! we own under the predecessor product's org name (see [`DEFAULT_GGUF_REPO`]).
//! There is no runtime dependency on the third-party upstream
//! `codefuse-ai/F2LLM-v2-330M` repo. See `docs/third-party-models.md` for the
//! Apache-2.0 attribution and the pinned upstream revision these artifacts
//! were derived from.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use hf_hub::{Repo, RepoType, api::sync::ApiBuilder};
use inkentry_embed::NativeEmbedder;
#[cfg(feature = "embed-llama")]
use inkentry_embed::{DeviceRequest, LlamaEmbedder};

/// `config.json` for F2LLM-v2-330M (Qwen3 architecture config; ~1 KB).
/// Embedded directly in the binary — it's tiny and never changes independent
/// of the pinned model revision recorded in `docs/third-party-models.md`, so
/// there's no reason to fetch it over the network. Vendored at
/// `crates/inkentry-server/assets/f2llm-v2-330m-config.json`.
const CONFIG_JSON: &str = include_str!("../assets/f2llm-v2-330m-config.json");

/// Override env var naming the Hugging Face repo id that holds a **pre-quantized
/// Q8_0 GGUF** (and, alongside it, the tokenizer) for the embedder. Read from
/// `INKENTRY_EMBEDDER_GGUF_REPO` at load time; see [`prequantized_gguf_repo`]
/// for the accepted values.
///
/// By default (unset) the loader fetches `QUANT_GGUF` and `tokenizer.json`
/// from [`DEFAULT_GGUF_REPO`] via the existing hf-hub cache — first-run
/// download is ~339 MB. Set this to a different `org/repo` to fetch both from
/// there instead (it must host both files, e.g. a mirror of our repo).
const GGUF_REPO_ENV: &str = "INKENTRY_EMBEDDER_GGUF_REPO";

/// Default Hugging Face repo id holding our **own pre-quantized Q8_0 GGUF**
/// (`f2llm-v2-330m-q8_0.gguf`) and tokenizer (`tokenizer.json`). Used when
/// `INKENTRY_EMBEDDER_GGUF_REPO` is unset, so a stock install fetches the
/// ~339 MB pre-quant GGUF plus tokenizer from here — no third-party repo
/// involved. Override with the env var (see [`GGUF_REPO_ENV`]).
///
/// The `spelunk-cloud` org is the predecessor product's name, kept
/// deliberately. Renaming it buys a tidier URL and nothing else, and it is
/// not free: the org is part of the hf-hub cache key, so existing installs
/// refetch `tokenizer.json` (~8 MB), and the air-gapped provisioning
/// procedure in `docs/server-setup.md` hard-codes the current cache directory
/// name in a copy-paste command. The ~339 MB GGUF is *not* refetched:
/// [`load_from_hub`] reads it from a flat path at the cache root and skips
/// the download when it is already there. A rename sweep leaves this alone;
/// see `docs/model-attribution.md` for the same reasoning in prose.
const DEFAULT_GGUF_REPO: &str = "spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF";

/// Filename of the Q8_0-quantized GGUF cached next to the HF download.
/// Projection matmuls and the token-embedding table are stored Q8_0; the small
/// RMSNorm weights stay F32. Produced upstream by the pre-quantize pipeline
/// that publishes `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF` (see
/// `docs/third-party-models.md`), not built on device.
const QUANT_GGUF: &str = "f2llm-v2-330m-q8_0.gguf";

/// Filename of the *canonical llama.cpp* GGUF for the same model, hosted in
/// the same repo as [`QUANT_GGUF`]. The two are not interchangeable: this one
/// carries llama.cpp tensor names (`blk.N.*`), arch metadata, and the baked
/// tokenizer + last-token pooling config that `LlamaEmbedder` needs, while
/// [`QUANT_GGUF`] keeps HF-style names only the candle loader reads. Both are
/// Q8_0 quantizations of the same pinned upstream revision, so they share the
/// vector space and `MODEL_ID`.
#[cfg(feature = "embed-llama")]
const LLAMA_GGUF: &str = "f2llm-v2-330m-llama-q8_0.gguf";

/// Env var selecting where the embedder runs: `auto` (default), `gpu`, or
/// `cpu`. Deliberately API-agnostic values — no `vulkan`/`metal` — so the
/// same setting means the same thing on every platform. `cpu` is the escape
/// hatch that skips the llama engine entirely and keeps today's candle path.
#[cfg(feature = "embed-llama")]
const EMBED_DEVICE_ENV: &str = "INKENTRY_EMBED_DEVICE";

/// Load the F2LLM-v2-330M model, quantized to Q8_0, via the Hugging Face Hub.
///
/// Downloads our own pre-quantized GGUF (`f2llm-v2-330m-q8_0.gguf`) and
/// tokenizer (`tokenizer.json`) straight from
/// `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF` through the hf-hub cache
/// (checksum/resume reused) — first-run download is ~339 MB, cached in
/// `~/.local/share/inkentry/models/`. Set `INKENTRY_EMBEDDER_GGUF_REPO` to a
/// different `org/repo` to fetch both from there instead. `config.json` is
/// embedded in the binary (see [`CONFIG_JSON`]) and written to the same cache
/// directory so it lands next to the other artifacts as a real file.
///
/// Subsequent calls read everything from the local cache with no network
/// access. There is no runtime dependency on any third-party Hugging Face
/// repo. Once the GGUF/tokenizer/config are resolved on disk this hands off to
/// [`inkentry_embed::NativeEmbedder::load_from_path`], which does the actual
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

    let gguf_repo = prequantized_gguf_repo();
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

/// Load the F2LLM-v2-330M embedder from a directory an operator provisioned
/// out-of-band (`inkentry-server --model-dir <path>` /
/// `INKENTRY_MODEL_DIR`), with zero network access. Unlike [`load_from_hub`],
/// this function never references `hf_hub`: the offline path is a pure
/// filesystem read, so there is no code path here for a corp firewall to
/// block. See "Air-gapped / no-egress install" in `docs/server-setup.md` for
/// the fetch-and-transfer procedure that produces this directory on a
/// connected machine.
///
/// Expects `dir` to contain the two artifacts that vary per pinned model
/// revision: the Q8_0 GGUF (see [`QUANT_GGUF`]) and `tokenizer.json`, exactly
/// as fetched by [`load_from_hub`]. `config.json` never changes independent
/// of the pinned revision (see [`CONFIG_JSON`]), so it's optional here: if
/// present it's used as-is (an explicit override), otherwise the embedded
/// default is written into `dir` so a second load from the same directory is
/// fully self-contained from just those two transferred files.
pub fn load_from_model_dir(dir: &Path) -> Result<NativeEmbedder> {
    anyhow::ensure!(
        dir.is_dir(),
        "--model-dir {} is not a directory. See \"Air-gapped / no-egress install\" in \
         docs/server-setup.md for the offline provisioning procedure.",
        dir.display()
    );

    let gguf_path = dir.join(QUANT_GGUF);
    let tokenizer_path = dir.join("tokenizer.json");
    let config_path = dir.join("config.json");

    anyhow::ensure!(
        gguf_path.exists(),
        "offline model artifact missing: {} not found in --model-dir {}. See \
         \"Air-gapped / no-egress install\" in docs/server-setup.md for the fetch-and-transfer \
         procedure.",
        QUANT_GGUF,
        dir.display()
    );
    anyhow::ensure!(
        tokenizer_path.exists(),
        "offline model artifact missing: tokenizer.json not found in --model-dir {}. See \
         \"Air-gapped / no-egress install\" in docs/server-setup.md for the fetch-and-transfer \
         procedure.",
        dir.display()
    );

    if !config_path.exists() {
        std::fs::write(&config_path, CONFIG_JSON).with_context(|| {
            format!("writing embedded config.json to {}", config_path.display())
        })?;
    }

    tracing::info!(
        "loading F2LLM-v2-330M (Q8_0) from offline --model-dir {} (zero network access)",
        dir.display()
    );

    NativeEmbedder::load_from_path(&gguf_path, &tokenizer_path, &config_path)
}

/// A ready embedding backend plus the identity facts `/v1/health` surfaces
/// about it. `engine`/`device` exist so a field report can say *which* engine
/// on *which* device produced a problem without reading server logs.
pub struct LoadedEmbedder {
    pub backend: Arc<dyn inkentry_core::embeddings::EmbeddingBackend>,
    /// `"candle"` or `"llama"`.
    pub engine: &'static str,
    /// `"cpu"`, `"metal"`, `"vulkan"`, or `"gpu"` — the device the engine
    /// resolved at load.
    pub device: &'static str,
}

/// Load the embedding backend, choosing the engine at runtime.
///
/// With the `embed-llama` feature and `INKENTRY_EMBED_DEVICE` not `cpu`, the
/// llama.cpp engine (GPU) is tried first; any failure there — missing
/// artifact, no usable driver, out of device memory — logs a warning and
/// falls back to the candle engine, so a build carrying the llama engine can
/// never embed *less* than one without it. `cpu` (or a build without
/// `embed-llama`) is exactly today's candle path.
pub fn load_backend(model_dir: Option<&Path>, embed_threads: usize) -> Result<LoadedEmbedder> {
    #[cfg(feature = "embed-llama")]
    {
        let device = embed_device_request()?;
        if !matches!(device, DeviceRequest::Cpu) {
            let llama = match model_dir {
                Some(dir) => load_llama_from_model_dir(dir, device, embed_threads),
                None => load_llama_from_hub(device, embed_threads),
            };
            match llama {
                Ok(embedder) => {
                    return Ok(LoadedEmbedder {
                        device: embedder.device(),
                        backend: Arc::new(embedder),
                        engine: "llama",
                    });
                }
                Err(e) => tracing::warn!(
                    "llama embedding engine failed to load ({e:#}); falling back to candle"
                ),
            }
        }
    }
    #[cfg(not(feature = "embed-llama"))]
    let _ = embed_threads;

    let native = match model_dir {
        Some(dir) => load_from_model_dir(dir),
        None => load_from_hub(),
    }?;
    Ok(LoadedEmbedder {
        backend: Arc::new(native),
        engine: "candle",
        // Compile-time flavor: the candle loader's rare runtime
        // Metal-init fallback to CPU is logged but not surfaced here.
        device: if cfg!(feature = "metal") {
            "metal"
        } else {
            "cpu"
        },
    })
}

/// Load the llama.cpp engine's canonical GGUF via the Hugging Face Hub —
/// same repo, cache, and flat-copy layout as [`load_from_hub`], but a single
/// file: the canonical GGUF embeds its own tokenizer and config.
#[cfg(feature = "embed-llama")]
fn load_llama_from_hub(device: DeviceRequest, embed_threads: usize) -> Result<LlamaEmbedder> {
    let cache_dir = model_cache_dir()?;
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating model cache dir {}", cache_dir.display()))?;
    let gguf_path = cache_dir.join(LLAMA_GGUF);

    if !gguf_path.exists() {
        let gguf_repo = prequantized_gguf_repo();
        tracing::info!("fetching canonical llama.cpp GGUF from {gguf_repo} (first run)…");
        let api = ApiBuilder::new()
            .with_cache_dir(cache_dir)
            .build()
            .context("building HuggingFace Hub API client")?;
        let repo = api.repo(Repo::new(gguf_repo.clone(), RepoType::Model));
        let downloaded = repo
            .get(LLAMA_GGUF)
            .with_context(|| format!("downloading {LLAMA_GGUF} from {gguf_repo}"))?;
        if downloaded != gguf_path {
            std::fs::copy(&downloaded, &gguf_path).with_context(|| {
                format!(
                    "caching {} -> {}",
                    downloaded.display(),
                    gguf_path.display()
                )
            })?;
        }
        tracing::info!(
            "fetched canonical llama.cpp GGUF to {}",
            gguf_path.display()
        );
    }

    LlamaEmbedder::load_from_path(&gguf_path, device, Some(embed_threads))
}

/// Air-gapped counterpart of [`load_llama_from_hub`]: reads the canonical
/// llama.cpp GGUF from the operator-provisioned `--model-dir`. Zero network
/// access, no `hf_hub` involvement.
#[cfg(feature = "embed-llama")]
fn load_llama_from_model_dir(
    dir: &Path,
    device: DeviceRequest,
    embed_threads: usize,
) -> Result<LlamaEmbedder> {
    anyhow::ensure!(
        dir.is_dir(),
        "--model-dir {} is not a directory. See \"Air-gapped / no-egress install\" in \
         docs/server-setup.md for the offline provisioning procedure.",
        dir.display()
    );
    let gguf_path = dir.join(LLAMA_GGUF);
    anyhow::ensure!(
        gguf_path.exists(),
        "offline model artifact missing: {} not found in --model-dir {}. See \
         \"Air-gapped / no-egress install\" in docs/server-setup.md for the fetch-and-transfer \
         procedure.",
        LLAMA_GGUF,
        dir.display()
    );
    tracing::info!(
        "loading F2LLM-v2-330M (Q8_0) via llama.cpp from offline --model-dir {} \
         (zero network access)",
        dir.display()
    );
    LlamaEmbedder::load_from_path(&gguf_path, device, Some(embed_threads))
}

/// Parse [`EMBED_DEVICE_ENV`]; unset or blank means `auto`. An unparseable
/// value is a load error (surfaced through `/v1/health` as `unavailable`)
/// rather than a silent default: a typo'd `INKENTRY_EMBED_DEVICE=vulkan`
/// quietly running on some other device would be worse than failing loudly.
#[cfg(feature = "embed-llama")]
fn embed_device_request() -> Result<DeviceRequest> {
    match std::env::var(EMBED_DEVICE_ENV) {
        Ok(v) if !v.trim().is_empty() => v
            .parse()
            .with_context(|| format!("parsing {EMBED_DEVICE_ENV}")),
        _ => Ok(DeviceRequest::Auto),
    }
}

fn model_cache_dir() -> Result<PathBuf> {
    dirs::data_local_dir()
        .map(|d| d.join("inkentry").join("models"))
        .ok_or_else(|| anyhow::anyhow!("could not determine local data directory"))
}

/// Resolve the HF repo id of the pre-quantized Q8_0 GGUF (and tokenizer) to
/// fetch, from `INKENTRY_EMBEDDER_GGUF_REPO`.
///
/// The env var (after trimming surrounding whitespace) is interpreted as:
///
/// * **unset** → `DEFAULT_GGUF_REPO` — the default; a stock install fetches the
///   ~339 MB pre-quant GGUF plus tokenizer from
///   `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF`.
/// * **any other value** → that `org/repo` id (trimmed) — override: fetch the
///   pre-quant GGUF and tokenizer from there instead (it must host both
///   files).
fn prequantized_gguf_repo() -> String {
    match std::env::var(GGUF_REPO_ENV) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => DEFAULT_GGUF_REPO.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `prequantized_gguf_repo()` resolves the GGUF source from
    /// `INKENTRY_EMBEDDER_GGUF_REPO`: unset/blank → the bundled default repo;
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
            prequantized_gguf_repo(),
            "spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF",
            "unset env var must default to fetching the bundled pre-quant GGUF"
        );

        unsafe { std::env::set_var(GGUF_REPO_ENV, "   ") };
        assert_eq!(
            prequantized_gguf_repo(),
            "spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF",
            "blank/whitespace env var must fall back to the default repo, not fetch \"\""
        );

        // Override: an explicit repo id is used verbatim, with whitespace trimmed.
        unsafe { std::env::set_var(GGUF_REPO_ENV, "  org/repo  ") };
        assert_eq!(prequantized_gguf_repo(), "org/repo");

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

        let tmp = std::env::temp_dir().join("inkentry-model-cache-dir-test");
        unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };

        assert_eq!(
            model_cache_dir().expect("resolve cache dir"),
            tmp.join("inkentry").join("models")
        );

        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
    }

    /// End-to-end semantic-discrimination check over the real model. Ignored by
    /// default: it downloads the ~339 MB pre-quantized GGUF and runs inference.
    /// Run with `cargo test -p inkentry-server -- --ignored embeddings_discriminate`.
    ///
    /// With the #19 GQA bug present, related and unrelated pairs collapse to the
    /// same cosine (~0.1–0.25); with the fix, related pairs sit well above
    /// unrelated. This is the only test that exercises attention end-to-end via
    /// the Hub acquisition path (the pure-local path has its own coverage in
    /// `inkentry-embed`).
    #[test]
    #[ignore = "downloads the F2LLM model and runs inference"]
    fn embeddings_discriminate_related_from_unrelated() {
        use inkentry_core::embeddings::EmbeddingBackend;

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
             related={related:.3} vs unrelated={unrelated:.3} (inkentry-oss#19)"
        );
    }

    /// Cross-engine vector-space parity gate: the llama.cpp engine must land in
    /// the same vector space as the candle engine, within the drift the product
    /// already ships between candle-CPU and candle-Metal (median cosine ≥
    /// 0.999, worst chunk ≥ 0.99, measured over 300+ real repo chunks in the
    /// Phase-0 study). This is what lets both engines serve one `MODEL_ID`
    /// with no re-index. Ignored by default: needs both GGUFs on disk — the
    /// HF-named one via [`load_from_hub`] and the canonical llama.cpp one in
    /// the model cache (published alongside it, or placed manually before the
    /// repo carries it).
    #[cfg(feature = "embed-llama")]
    #[test]
    #[ignore = "requires both F2LLM GGUFs and runs inference on both engines"]
    fn llama_engine_matches_candle_vector_space() {
        use inkentry_core::embeddings::EmbeddingBackend;

        let candle = load_from_hub().expect("load candle engine");
        let llama = load_llama_from_hub(DeviceRequest::Auto, 4)
            .expect("load llama engine (canonical GGUF)");
        assert_eq!(
            candle.dimension(),
            llama.dimension(),
            "engines must agree on dim"
        );

        let texts: [&str; 8] = [
            "title: load_from_hub | text: pub fn load_from_hub() -> Result<NativeEmbedder> { \
             let cache_dir = model_cache_dir()?; }",
            "title: none | text: The server binds its listener before the model warms up so \
             health stays live during the download.",
            "title: l2_normalise | text: fn l2_normalise(v: &mut [f32]) { let norm = \
             v.iter().map(|x| x * x).sum::<f32>().sqrt(); }",
            "title: none | text: 埋め込みモデルは起動時にバックグラウンドで読み込まれます。",
            "title: none | text: SELECT id, title FROM notes WHERE archived = 0 ORDER BY \
             created_at DESC LIMIT 20;",
            "title: none | text: the fall of the roman empire and the rise of byzantium",
            "title: token_cap | text: the memory-budget-derived bound that keeps the \
             single-chunk attention scratch within RAM",
            "title: none | text: cosine similarity between L2-normalised vectors is their \
             dot product",
        ];
        let query = "Instruct: Given a code search query, retrieve the relevant code \
                     snippets\nQuery: normalise an embedding vector in place";

        let rt = tokio::runtime::Runtime::new().unwrap();
        let candle_vecs = rt.block_on(candle.embed(&texts)).expect("candle embed");
        let llama_vecs = rt.block_on(llama.embed(&texts)).expect("llama embed");

        // Embeddings are L2-normalised, so dot product == cosine similarity.
        let cos = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();

        let mut cosines: Vec<f32> = candle_vecs
            .iter()
            .zip(&llama_vecs)
            .map(|(c, l)| cos(c, l))
            .collect();
        cosines.sort_by(|a, b| a.partial_cmp(b).expect("finite cosines"));
        let min = cosines[0];
        let median = cosines[cosines.len() / 2];
        assert!(
            min >= 0.99 && median >= 0.999,
            "cross-engine drift exceeds the shipped candle CPU↔Metal envelope: \
             min={min:.5} (≥0.99 required), median={median:.5} (≥0.999 required) — \
             if this regressed after a llama-cpp-2 upgrade, the vector space moved \
             and MODEL_ID must change (forcing a re-index)"
        );

        // Retrieval agreement: both engines must rank the same best chunk for
        // a real query (the contract that actually matters to search).
        let candle_q = &rt.block_on(candle.embed(&[query])).expect("candle query")[0];
        let llama_q = &rt.block_on(llama.embed(&[query])).expect("llama query")[0];
        let argmax = |q: &[f32], vecs: &[Vec<f32>]| {
            (0..vecs.len())
                .max_by(|&a, &b| {
                    cos(q, &vecs[a])
                        .partial_cmp(&cos(q, &vecs[b]))
                        .expect("finite cosines")
                })
                .expect("non-empty corpus")
        };
        assert_eq!(
            argmax(candle_q, &candle_vecs),
            argmax(llama_q, &llama_vecs),
            "engines disagree on the top-1 chunk for the same query"
        );
    }

    /// End-to-end proof that an oversized single chunk no longer OOMs/aborts
    /// (inkentry-oss#17), exercised via the Hub acquisition path. Ignored by
    /// default: downloads the model and runs inference.
    ///
    /// Run with:
    ///   INKENTRY_SECRET_STORE=file cargo test -p inkentry-server \
    ///     -- --ignored oversized_chunk_embeds_without_oom
    #[test]
    #[ignore = "downloads the F2LLM model and runs inference"]
    fn oversized_chunk_embeds_without_oom() {
        use inkentry_core::embeddings::EmbeddingBackend;

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
        use inkentry_core::embeddings::EmbeddingBackend;

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
    ///   INKENTRY_SECRET_STORE=file cargo test -p inkentry-server \
    ///     -- --ignored load_from_path_embeds
    #[test]
    #[ignore = "requires model artifacts already present in the local cache"]
    fn load_from_path_embeds_896_dim() {
        use inkentry_core::embeddings::EmbeddingBackend;
        use inkentry_embed::DIM;

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
    /// unconditional unit coverage in `inkentry_embed::embedder_native::tests`.
    /// Ignored by default: downloads the model. Run with:
    ///   INKENTRY_SECRET_STORE=file cargo test -p inkentry-server \
    ///     -- --ignored native_embedder_reports_its_token_cap
    #[test]
    #[ignore = "downloads the F2LLM model"]
    fn native_embedder_reports_its_token_cap() {
        use inkentry_core::embeddings::EmbeddingBackend;

        let embedder = load_from_hub().expect("load F2LLM-v2-330M");

        let cap = embedder
            .token_cap()
            .expect("a loaded NativeEmbedder must report a host-derived token cap");
        // Sanity bounds matching the documented derivation (~5 792 @ 2 GiB,
        // ~8 192 @ 4 GiB budget; see `derive_token_cap`'s doc comment) without
        // reaching into inkentry-embed's private constants from this crate.
        assert!(cap >= 1000, "token cap implausibly small: {cap}");
        assert!(
            cap <= 40_960,
            "token cap must not exceed MAX_SEQ_LEN: {cap}"
        );
    }

    // ── Offline / air-gapped model-dir load ───────────────────────────────────

    /// A `--model-dir` pointing at a plain file (not a directory) is a clear
    /// misconfiguration error, not a panic or a silent Hub fallback.
    #[test]
    fn load_from_model_dir_rejects_non_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"").unwrap();

        let msg = match load_from_model_dir(&file) {
            Ok(_) => panic!("a file path must not be accepted as --model-dir"),
            Err(e) => format!("{e:#}"),
        };
        assert!(msg.contains(&file.display().to_string()));
        assert!(
            msg.contains("server-setup.md"),
            "error must point at the offline provisioning docs, got: {msg}"
        );
    }

    /// An empty `--model-dir` (no artifacts provisioned yet) must fail with a
    /// clear error naming the missing GGUF and pointing at the offline docs
    /// section, never a bare Hugging Face Hub connection error, since this
    /// path never touches `hf_hub` at all.
    #[test]
    fn load_from_model_dir_missing_gguf_names_file_and_docs() {
        let dir = tempfile::tempdir().unwrap();

        let msg = match load_from_model_dir(dir.path()) {
            Ok(_) => panic!("an empty --model-dir must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            msg.contains(QUANT_GGUF),
            "error must name the missing file: {msg}"
        );
        assert!(
            msg.contains("server-setup.md"),
            "error must point at the offline docs: {msg}"
        );
        assert!(
            !msg.contains("http") && !msg.contains("huggingface") && !msg.contains("downloading"),
            "must not reference any network fetch, got: {msg}"
        );
    }

    /// With the GGUF present but the tokenizer absent, the error names the
    /// tokenizer specifically, not a generic "artifacts missing".
    #[test]
    fn load_from_model_dir_missing_tokenizer_names_file_and_docs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(QUANT_GGUF), b"not a real gguf").unwrap();

        let msg = match load_from_model_dir(dir.path()) {
            Ok(_) => panic!("a missing tokenizer must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            msg.contains("tokenizer.json"),
            "error must name the missing file: {msg}"
        );
        assert!(
            msg.contains("server-setup.md"),
            "error must point at the offline docs: {msg}"
        );
    }

    /// Both artifacts present but corrupt: the error must come from the local
    /// parse (naming the specific bad file), matching `load_from_path`'s
    /// existing per-file error behaviour: never a network error, never a
    /// panic (proving "no crash loop" starts from a `Result`, not a `unwrap`).
    #[test]
    fn load_from_model_dir_corrupt_tokenizer_errors_locally() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(QUANT_GGUF), b"not a real gguf").unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), b"not valid json").unwrap();

        let msg = match load_from_model_dir(dir.path()) {
            Ok(_) => panic!("a corrupt tokenizer must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            msg.contains("tokenizer"),
            "error must name the tokenizer as the failing artifact, got: {msg}"
        );
        assert!(
            !msg.contains("http") && !msg.contains("huggingface") && !msg.contains("downloading"),
            "corrupt-artifact error must not reference any network fetch, got: {msg}"
        );
    }

    /// A minimal-but-valid `tokenizer.json`, built through the `tokenizers`
    /// crate's own serializer rather than hand-typed JSON, so a corrupt-GGUF
    /// test can get past tokenizer parsing and reach the GGUF parse itself
    /// (`Qwen3EmbedWeights::from_gguf`), a different failure mode with a
    /// different error path than the corrupt-tokenizer case above.
    fn write_valid_tokenizer(path: &std::path::Path) {
        let vocab: std::collections::HashMap<String, u32> =
            [("<unk>".to_string(), 0u32)].into_iter().collect();
        let model = tokenizers::models::wordlevel::WordLevel::builder()
            .vocab(vocab.into_iter().collect())
            .unk_token("<unk>".to_string())
            .build()
            .expect("valid WordLevel fixture model");
        tokenizers::Tokenizer::new(model)
            .save(path, false)
            .expect("saving fixture tokenizer.json");
    }

    /// Corrupt GGUF with a *valid* tokenizer must fail inside GGUF parsing
    /// (`Qwen3EmbedWeights::from_gguf`), not tokenizer parsing - proving the
    /// two artifact-corruption cases take genuinely distinct error paths
    /// rather than both happening to fail on whichever the code checks first.
    #[test]
    fn load_from_model_dir_corrupt_gguf_errors_locally() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(QUANT_GGUF), b"not a real gguf").unwrap();
        write_valid_tokenizer(&dir.path().join("tokenizer.json"));
        // No config.json: the real embedded config is auto-written, so the
        // failure is attributable to the GGUF alone.

        let msg = match load_from_model_dir(dir.path()) {
            Ok(_) => panic!("a corrupt GGUF must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            !msg.contains("tokenizer") && !msg.contains("config.json"),
            "error must not misattribute a GGUF failure to the tokenizer or config, got: {msg}"
        );
        assert!(
            !msg.contains("http") && !msg.contains("huggingface") && !msg.contains("downloading"),
            "corrupt-GGUF error must not reference any network fetch, got: {msg}"
        );
    }

    /// A `--model-dir` containing only `tokenizer.json` (no GGUF at all) must
    /// still name the GGUF as missing, the same as a fully empty directory -
    /// proving the existence check order doesn't let a present tokenizer mask
    /// the missing GGUF with a different (e.g. tokenizer-shaped) error.
    #[test]
    fn load_from_model_dir_tokenizer_only_still_names_missing_gguf() {
        let dir = tempfile::tempdir().unwrap();
        write_valid_tokenizer(&dir.path().join("tokenizer.json"));

        let msg = match load_from_model_dir(dir.path()) {
            Ok(_) => panic!("a tokenizer-only --model-dir must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            msg.contains(QUANT_GGUF),
            "error must name the missing GGUF even with tokenizer.json present: {msg}"
        );
        assert!(
            msg.contains("server-setup.md"),
            "error must point at the offline docs: {msg}"
        );
    }

    /// A `--model-dir` pointing at a path that doesn't exist at all (as
    /// opposed to an existing non-directory file) must fail with the same
    /// clear "not a directory" error naming the path, not a confusing
    /// downstream OS error from inside file-open calls.
    #[test]
    fn load_from_model_dir_rejects_nonexistent_path() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("does-not-exist");

        let msg = match load_from_model_dir(&missing) {
            Ok(_) => panic!("a nonexistent path must not be accepted as --model-dir"),
            Err(e) => format!("{e:#}"),
        };
        assert!(msg.contains(&missing.display().to_string()));
        assert!(
            msg.contains("is not a directory"),
            "error must clearly say the directory itself is missing, got: {msg}"
        );
        assert!(
            msg.contains("server-setup.md"),
            "error must point at the offline provisioning docs, got: {msg}"
        );
    }

    /// `load_from_model_dir` writes the embedded `config.json` into the
    /// directory when missing, mirroring `load_from_hub`'s cache layout, so
    /// an operator only ever needs to transfer the two revision-specific
    /// files (GGUF + tokenizer) and a second load from the same directory is
    /// fully self-contained.
    #[test]
    fn load_from_model_dir_writes_embedded_config_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(QUANT_GGUF), b"not a real gguf").unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), b"not valid json").unwrap();

        // The load itself still fails (corrupt fixtures), but config.json must
        // have been written before the failing tokenizer parse.
        let _ = load_from_model_dir(dir.path());
        let config_path = dir.path().join("config.json");
        assert!(
            config_path.exists(),
            "embedded config.json must be written to --model-dir"
        );
        assert_eq!(std::fs::read_to_string(config_path).unwrap(), CONFIG_JSON);
    }

    /// A second server start against the same `--model-dir` (config.json now
    /// present from the first run's auto-write) must behave identically to
    /// the first: the existing file is used as-is, not re-written or treated
    /// as a conflict, so the resulting error (from the still-corrupt GGUF /
    /// tokenizer fixtures) is unchanged between runs.
    #[test]
    fn load_from_model_dir_second_start_reuses_written_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(QUANT_GGUF), b"not a real gguf").unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), b"not valid json").unwrap();

        let first_msg = match load_from_model_dir(dir.path()) {
            Ok(_) => panic!("corrupt fixtures must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        let config_path = dir.path().join("config.json");
        assert!(config_path.exists(), "first run must write config.json");

        // Simulate an operator restart: model-dir now has all three paths
        // present, exactly like a second `inkentry-server --model-dir` start.
        let second_msg = match load_from_model_dir(dir.path()) {
            Ok(_) => panic!("corrupt fixtures must still be a load error on a second start"),
            Err(e) => format!("{e:#}"),
        };

        assert_eq!(
            first_msg, second_msg,
            "a pre-existing config.json must not change the load outcome"
        );
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            CONFIG_JSON,
            "the pre-existing config.json must be left as the same embedded default, not corrupted by a second write"
        );
    }

    /// Zero-egress guarantee under a hostile network: even with every standard
    /// proxy env var pointed at an address nothing listens on,
    /// `load_from_model_dir` must behave identically to a clean environment:
    /// same error, and fast (no hang waiting on a dead proxy). The only way
    /// that holds is if the code path never attempts a network request at
    /// all. Guards against a future edit reintroducing an `hf_hub`/`reqwest`
    /// call into this function.
    #[test]
    #[serial_test::serial(network_proxy_env)]
    fn load_from_model_dir_ignores_hostile_network_env() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(QUANT_GGUF), b"not a real gguf").unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), b"not valid json").unwrap();

        let err_msg = |dir: &std::path::Path| match load_from_model_dir(dir) {
            Ok(_) => panic!("corrupt fixtures must be a load error"),
            Err(e) => format!("{e:#}"),
        };
        let clean_msg = err_msg(dir.path());

        // Point every standard proxy env var at a closed local port: any
        // accidental network call in this path would fail differently (or
        // hang) via the proxy, changing the message or the timing.
        let proxy_vars = [
            "http_proxy",
            "https_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
        ];
        // SAFETY: guarded by #[serial] so no other test reads/writes these
        // vars concurrently; restored before returning.
        let prev: Vec<Option<String>> = proxy_vars.iter().map(|v| std::env::var(v).ok()).collect();
        for v in proxy_vars {
            unsafe { std::env::set_var(v, "http://127.0.0.1:1") };
        }

        let started = std::time::Instant::now();
        let hostile_msg = err_msg(dir.path());
        let elapsed = started.elapsed();

        for (v, val) in proxy_vars.iter().zip(prev) {
            match val {
                Some(v2) => unsafe { std::env::set_var(v, v2) },
                None => unsafe { std::env::remove_var(v) },
            }
        }

        assert_eq!(
            clean_msg, hostile_msg,
            "load_from_model_dir must behave identically regardless of network reachability"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "must fail on the local parse alone, never wait on a network call: {elapsed:?}"
        );
    }

    /// End-to-end round-trip: the artifacts `load_from_hub` fetches onto a
    /// connected machine must be exactly what `load_from_model_dir` accepts
    /// once copied into a flat directory, and both load paths must produce
    /// agreeing embeddings for the same input. This is the proof that the
    /// documented fetch-and-transfer procedure (AC5) produces a directory this
    /// offline path actually loads. Ignored by default: downloads the model.
    ///
    /// Run with:
    ///   INKENTRY_SECRET_STORE=file cargo test -p inkentry-server \
    ///     -- --ignored offline_model_dir_round_trips_with_hub_artifacts
    #[test]
    #[ignore = "downloads the F2LLM model"]
    fn offline_model_dir_round_trips_with_hub_artifacts() {
        use inkentry_core::embeddings::EmbeddingBackend;

        // Prime the Hub cache, then locate the resolved files exactly as
        // `load_from_path_embeds_896_dim` does above.
        load_from_hub().expect("prime local cache via Hub");
        let cache_dir = model_cache_dir().expect("cache dir");
        let hub_gguf = cache_dir.join(QUANT_GGUF);
        let hub_config = cache_dir.join("config.json");
        let hub_tokenizer = std::fs::read_dir(
            cache_dir
                .join("models--spelunk-cloud--F2LLM-v2-330M-Q8_0-GGUF")
                .join("snapshots"),
        )
        .expect("hf-hub snapshots dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("tokenizer.json"))
        .find(|p| p.exists())
        .expect("cached tokenizer.json");

        // Simulate the operator's transfer: copy just the two
        // revision-specific files into a fresh flat directory.
        let offline_dir = tempfile::tempdir().unwrap();
        std::fs::copy(&hub_gguf, offline_dir.path().join(QUANT_GGUF)).unwrap();
        std::fs::copy(&hub_tokenizer, offline_dir.path().join("tokenizer.json")).unwrap();
        let _ = &hub_config; // config.json is embedded; the offline loader writes its own copy.

        let hub_embedder = NativeEmbedder::load_from_path(&hub_gguf, &hub_tokenizer, &hub_config)
            .expect("load via the Hub-resolved paths");
        let offline_embedder =
            load_from_model_dir(offline_dir.path()).expect("load via the offline model-dir path");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let text = "read the contents of a file from disk";
        let hub_vec = rt.block_on(hub_embedder.embed(&[text])).expect("hub embed");
        let offline_vec = rt
            .block_on(offline_embedder.embed(&[text]))
            .expect("offline embed");

        assert_eq!(
            hub_vec, offline_vec,
            "the same artifacts loaded via either path must produce identical embeddings"
        );
    }
}
