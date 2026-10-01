//! Hugging Face Hub acquisition path for the bundled F2LLM-v2-330M embedder.
//!
//! `inkentry-embed` only loads the embedder from a GGUF already on disk
//! ([`inkentry_embed::LlamaEmbedder::load_from_path`]) and carries no
//! network-fetch dependency; this module owns the `hf-hub` download step,
//! fetching the canonical llama.cpp GGUF into the local cache and handing the
//! resulting path to `load_from_path`. It is the only place in the workspace
//! that depends on `hf-hub`.
//!
//! An air-gapped counterpart reads the same GGUF from an operator-provisioned
//! directory with no `hf_hub` involvement.
//!
//! The GGUF is fetched from a repo we own; there is no runtime dependency on
//! the third-party upstream `codefuse-ai/F2LLM-v2-330M` repo.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use hf_hub::{Repo, RepoType, api::sync::ApiBuilder};
use inkentry_embed::{DeviceRequest, LlamaEmbedder};

// Override env var naming the Hugging Face repo id for the canonical GGUF.
const GGUF_REPO_ENV: &str = "INKENTRY_EMBEDDER_GGUF_REPO";

// Predecessor product's org name, kept deliberately: it's part of the hf-hub
// cache key, so renaming it would force existing installs to refetch.
const DEFAULT_GGUF_REPO: &str = "spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF";

// llama.cpp GGUF: baked tokenizer + last-token pooling config, Q8_0-quantized.
const LLAMA_GGUF: &str = "f2llm-v2-330m-llama-q8_0.gguf";

// Values are API-agnostic (`auto`/`gpu`/`cpu`, no `vulkan`/`metal`) so the
// same setting means the same thing on every platform.
const EMBED_DEVICE_ENV: &str = "INKENTRY_EMBED_DEVICE";

// hf-hub stages a download in `blobs/<etag>.part` and resumes it over an
// HTTP Range request. A partial is progress worth keeping only when
// `can_resume`; every other partial is dead weight. Cleanup never fails a
// model load — the caller logs and carries on.
fn prune_partial_downloads(blobs_dir: &Path, can_resume: bool) -> Result<usize> {
    let entries = match std::fs::read_dir(blobs_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(e).with_context(|| format!("reading model cache {}", blobs_dir.display()));
        }
    };

    let mut reclaimed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("part") {
            continue;
        }
        if can_resume && !path.with_extension("").exists() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => reclaimed += 1,
            Err(e) => tracing::warn!(
                "could not remove unusable partial download {}: {e}",
                path.display()
            ),
        }
    }
    Ok(reclaimed)
}

// Anything already in the blob directory (a partial included) means this
// machine has fetched from here before, even with the model file itself
// still absent.
fn fetch_note(blobs_dir: &Path) -> &'static str {
    let inhabited = std::fs::read_dir(blobs_dir).is_ok_and(|mut e| e.next().is_some());
    if inhabited {
        "cache present, model file missing"
    } else {
        "first run"
    }
}

// Hard-links (not copies) the download to the flat path the loader reads,
// keeping the cache at a single copy; falls back to a copy when hard-linking
// isn't possible. The pointer is resolved to its target first — on Unix it's
// a symlink relative to the snapshot directory, so linking the pointer
// itself would leave a flat path resolving against the wrong root. Two
// servers can race here on a cold cache, so the link lands under a
// temporary name and is renamed into place atomically: an in-place copy
// must never happen, because if the two names turn out to be the same file
// it truncates the model to nothing while the loader still sees a file
// present. hf-hub's own cache is left untouched — deleting its blob would
// strand the snapshot pointer with no way to recover.
fn materialise_model(downloaded: &Path, gguf_path: &Path) -> Result<()> {
    let source = std::fs::canonicalize(downloaded)
        .with_context(|| format!("resolving downloaded model at {}", downloaded.display()))?;
    if same_file::is_same_file(&source, gguf_path).unwrap_or(false) {
        return Ok(());
    }

    let staging = staging_path(gguf_path);
    let _ = std::fs::remove_file(&staging);

    if std::fs::hard_link(&source, &staging).is_err()
        && let Err(e) = std::fs::copy(&source, &staging)
    {
        let _ = std::fs::remove_file(&staging);
        return Err(e)
            .with_context(|| format!("caching {} -> {}", source.display(), gguf_path.display()));
    }

    if let Err(e) = std::fs::rename(&staging, gguf_path) {
        let _ = std::fs::remove_file(&staging);
        return Err(e)
            .with_context(|| format!("moving the model into place at {}", gguf_path.display()));
    }
    Ok(())
}

// Distinct per process so two servers materialising at once cannot collide.
fn staging_path(gguf_path: &Path) -> PathBuf {
    let mut name = gguf_path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    gguf_path.with_file_name(name)
}

/// A ready embedding backend plus the identity facts `/v1/health` surfaces
/// about it.
pub struct LoadedEmbedder {
    pub backend: Arc<dyn inkentry_core::embeddings::EmbeddingBackend>,
    /// Always `"llama"` — the sole embedding engine.
    pub engine: &'static str,
    /// `"cpu"`, `"metal"`, `"vulkan"`, or `"gpu"` — the device the engine
    /// resolved at load.
    pub device: &'static str,
    /// A non-fatal, actionable note about how the device resolved. Set when a
    /// GPU was wanted but embedding fell back to CPU for a fixable reason;
    /// `None` when the device resolved as expected.
    pub note: Option<String>,
}

/// Load the llama.cpp embedding backend.
///
/// The device is resolved from `INKENTRY_EMBED_DEVICE`: `auto`/`gpu` try a GPU
/// and fall back to CPU when none is usable; `cpu` forces the CPU engine. A
/// malformed value is a hard error, surfaced through `/v1/health` as
/// `unavailable`.
pub fn load_backend(
    model_dir: Option<&Path>,
    embed_threads: usize,
    interactive_capacity: usize,
) -> Result<LoadedEmbedder> {
    let requested = embed_device_request()?;
    let embedder = match model_dir {
        Some(dir) => load_llama_from_model_dir(dir, requested, embed_threads, interactive_capacity),
        None => load_llama_from_hub(requested, embed_threads, interactive_capacity),
    }?;
    let device = embedder.device();
    // A GPU was requested (not `cpu`) but resolved to CPU: worth a status
    // note. A `cpu` request running on CPU is expected, so no note there.
    let note = if device == "cpu" && !matches!(requested, DeviceRequest::Cpu) {
        gpu_fallback_note()
    } else {
        None
    };
    Ok(LoadedEmbedder {
        device,
        backend: Arc::new(embedder),
        engine: "llama",
        note,
    })
}

// Distinguishes a fixable permission problem (join the `render` group) from
// everything else (no GPU, or a driver rejecting the device) that the
// render-group advice can't help.
#[cfg(feature = "embed-llama")]
#[derive(Debug, PartialEq, Eq)]
enum RenderNodeAccess {
    // No `renderD*` node: no GPU render device present.
    NoNode,
    // A render node exists but this process can't open it (`EACCES`).
    PermissionDenied(String),
    // A render node exists and opens: not a permission problem.
    Reachable(String),
    // A render node exists but open failed for some other reason.
    Unknown,
}

// `dri_dir` is injected (rather than hard-wired to `/dev/dri`) so this is
// unit-testable without a real GPU. Opens read+write and drops the fd
// immediately — what a Vulkan driver needs, with no side effect on the
// device.
#[cfg(feature = "embed-llama")]
fn classify_render_nodes(dri_dir: &Path) -> RenderNodeAccess {
    let node = std::fs::read_dir(dri_dir).ok().and_then(|entries| {
        entries.flatten().map(|e| e.path()).find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("renderD"))
        })
    });
    let Some(node) = node else {
        return RenderNodeAccess::NoNode;
    };
    let path = node.display().to_string();
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&node)
    {
        Ok(_) => RenderNodeAccess::Reachable(path),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            RenderNodeAccess::PermissionDenied(path)
        }
        Err(_) => RenderNodeAccess::Unknown,
    }
}

// Diagnoses why a requested GPU resolved to CPU, on Linux only, and returns
// an actionable note when the cause is fixable. Returns `None` silently on a
// genuinely GPU-less host so it never nags a machine that has no GPU.
#[cfg(feature = "embed-llama")]
fn gpu_fallback_note() -> Option<String> {
    // DRM render nodes and the `render` group are a Linux-only concept.
    if !cfg!(target_os = "linux") {
        return None;
    }
    match classify_render_nodes(Path::new("/dev/dri")) {
        RenderNodeAccess::PermissionDenied(node) => {
            tracing::warn!(
                "A Vulkan-capable GPU appears present but could not be opened \
                 (permission denied on {node}). Add your user to the 'render' group — \
                 `sudo usermod -aG render $USER` — and re-login to enable GPU \
                 acceleration; embeddings are running on CPU until then."
            );
            Some(format!(
                "GPU blocked: no access to {node}. Add your user to the 'render' group \
                 (sudo usermod -aG render $USER) and re-login to enable GPU acceleration; \
                 running on CPU."
            ))
        }
        RenderNodeAccess::Reachable(node) => {
            // Reachable but not selected: a driver/hardware limit, not
            // fixable by the render group, so no actionable status note.
            tracing::info!(
                "A GPU render node ({node}) is accessible but no usable Vulkan GPU backend \
                 was selected — the device likely lacks a required Vulkan feature (e.g. \
                 16-bit storage) or has no Vulkan driver. Embeddings are running on CPU."
            );
            None
        }
        RenderNodeAccess::NoNode | RenderNodeAccess::Unknown => None,
    }
}

// Reclaims the previous candle engine's cached artifacts (flat GGUF +
// config.json at the cache root; GGUF + tokenizer.json in the hf-hub repo
// cache), keyed on the old filenames so it can never touch the llama GGUF.
// Idempotent and best-effort: a leftover file is wasted disk, not a fault.
#[cfg(feature = "embed-llama")]
fn reclaim_candle_artifacts(cache_dir: &Path, repo: &Repo) {
    const STALE_FLAT: [&str; 2] = ["f2llm-v2-330m-q8_0.gguf", "config.json"];
    const STALE_HUB: [&str; 2] = ["f2llm-v2-330m-q8_0.gguf", "tokenizer.json"];

    let mut removed = 0usize;
    for name in STALE_FLAT {
        if std::fs::remove_file(cache_dir.join(name)).is_ok() {
            removed += 1;
        }
    }

    let snapshots = cache_dir.join(repo.folder_name()).join("snapshots");
    if let Ok(revs) = std::fs::read_dir(&snapshots) {
        for rev in revs.flatten() {
            for name in STALE_HUB {
                let pointer = rev.path().join(name);
                // Resolve to the blob before removing the pointer, so the
                // actual bytes are reclaimed too.
                if let Ok(blob) = std::fs::canonicalize(&pointer)
                    && std::fs::remove_file(&blob).is_ok()
                {
                    removed += 1;
                }
                if std::fs::remove_file(&pointer).is_ok() {
                    removed += 1;
                }
            }
        }
    }

    if removed > 0 {
        tracing::info!(
            "reclaimed {removed} superseded candle model artifact(s) from the cache \
             (the previous embedding engine's files, no longer used)"
        );
    }
}

// Fetches the canonical GGUF (single file — it embeds its own tokenizer and
// config) via the Hub, hard-linking it to the flat path the loader reads
// from and reclaiming any unresumable partials. Subsequent calls read from
// the local cache with no network access.
#[cfg(feature = "embed-llama")]
fn load_llama_from_hub(
    device: DeviceRequest,
    embed_threads: usize,
    interactive_capacity: usize,
) -> Result<LlamaEmbedder> {
    let cache_dir = model_cache_dir()?;
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating model cache dir {}", cache_dir.display()))?;
    let gguf_path = cache_dir.join(LLAMA_GGUF);

    let gguf_repo = prequantized_gguf_repo();
    let repo_id = Repo::new(gguf_repo.clone(), RepoType::Model);

    reclaim_candle_artifacts(&cache_dir, &repo_id);

    let blobs_dir = cache_dir.join(repo_id.folder_name()).join("blobs");

    // Must run before any fetch, so it reflects the inherited cache, not
    // what this run populates.
    let note = fetch_note(&blobs_dir);

    // Nothing can resume once the GGUF is already on disk, since nothing
    // downloads in that case.
    let will_fetch = !gguf_path.exists();
    match prune_partial_downloads(&blobs_dir, will_fetch) {
        Ok(0) => {}
        Ok(n) => tracing::info!("reclaimed {n} unusable partial download(s) from the model cache"),
        Err(e) => tracing::warn!("could not sweep the model cache for partial downloads: {e:#}"),
    }

    if !gguf_path.exists() {
        tracing::info!("fetching canonical llama.cpp GGUF from {gguf_repo} ({note})…");
        let api = ApiBuilder::new()
            .with_cache_dir(cache_dir)
            .build()
            .context("building HuggingFace Hub API client")?;
        let repo = api.repo(repo_id);
        let downloaded = repo
            .get(LLAMA_GGUF)
            .with_context(|| format!("downloading {LLAMA_GGUF} from {gguf_repo}"))?;
        materialise_model(&downloaded, &gguf_path)?;
        tracing::info!(
            "fetched canonical llama.cpp GGUF to {}",
            gguf_path.display()
        );
    }

    // Sized to the server's two admission-lane capacities so every embed
    // gets its own warm context on its own lane, single-sourced here so it
    // can't drift from a separate constant in inkentry-embed.
    LlamaEmbedder::load_from_path(
        &gguf_path,
        device,
        Some(embed_threads),
        crate::EMBED_QUEUE_CAPACITY,
        interactive_capacity,
    )
}

// Air-gapped counterpart of load_llama_from_hub: reads the GGUF from the
// operator-provisioned --model-dir. Zero network access, no hf_hub
// involvement.
#[cfg(feature = "embed-llama")]
fn load_llama_from_model_dir(
    dir: &Path,
    device: DeviceRequest,
    embed_threads: usize,
    interactive_capacity: usize,
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
    // Sized to the server's two admission-lane capacities so every embed
    // gets its own warm context on its own lane, single-sourced here so it
    // can't drift from a separate constant in inkentry-embed.
    LlamaEmbedder::load_from_path(
        &gguf_path,
        device,
        Some(embed_threads),
        crate::EMBED_QUEUE_CAPACITY,
        interactive_capacity,
    )
}

fn env_trimmed(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

// An unparseable value is a hard error rather than a silent default: a typo
// running on some other device would be worse than failing loudly.
#[cfg(feature = "embed-llama")]
fn embed_device_request() -> Result<DeviceRequest> {
    match env_trimmed(EMBED_DEVICE_ENV) {
        Some(v) => v
            .parse()
            .with_context(|| format!("parsing {EMBED_DEVICE_ENV}")),
        None => Ok(DeviceRequest::Auto),
    }
}

fn model_cache_dir() -> Result<PathBuf> {
    dirs::data_local_dir()
        .map(|d| d.join("inkentry").join("models"))
        .ok_or_else(|| anyhow::anyhow!("could not determine local data directory"))
}

fn prequantized_gguf_repo() -> String {
    env_trimmed(GGUF_REPO_ENV).unwrap_or_else(|| DEFAULT_GGUF_REPO.to_string())
}

#[cfg(test)]
mod tests;
