//! Hugging Face Hub acquisition path for the bundled F2LLM-v2-330M embedder.
//!
//! `inkentry-embed` only knows how to load the embedder from a GGUF already on
//! disk ([`inkentry_embed::LlamaEmbedder::load_from_path`]) — it carries no
//! network-fetch dependency. This module owns the `hf-hub` download step: it
//! fetches the canonical llama.cpp GGUF from our own first-party Hugging Face
//! repo into the local hf-hub cache, then hands the resulting path to
//! `load_from_path`. This is the only place in `inkentry-server` — or the
//! workspace — that depends on `hf-hub`.
//!
//! [`load_llama_from_model_dir`] is the air-gapped counterpart: it resolves the
//! same GGUF from an operator-provisioned directory instead of the Hub, with no
//! `hf_hub` involvement at all (see "Air-gapped / no-egress install" in
//! `docs/server-setup.md`).
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
use inkentry_embed::{DeviceRequest, LlamaEmbedder};

/// Override env var naming the Hugging Face repo id that holds the canonical
/// llama.cpp GGUF for the embedder. Read from `INKENTRY_EMBEDDER_GGUF_REPO` at
/// load time; see [`prequantized_gguf_repo`] for the accepted values.
///
/// By default (unset) the loader fetches [`LLAMA_GGUF`] from
/// [`DEFAULT_GGUF_REPO`] via the existing hf-hub cache — first-run download is
/// ~345 MB. Set this to a different `org/repo` to fetch from there instead (it
/// must host the same file, e.g. a mirror of our repo).
const GGUF_REPO_ENV: &str = "INKENTRY_EMBEDDER_GGUF_REPO";

/// Default Hugging Face repo id holding our own canonical llama.cpp GGUF
/// ([`LLAMA_GGUF`]). Used when `INKENTRY_EMBEDDER_GGUF_REPO` is unset, so a
/// stock install fetches the ~345 MB GGUF from here — no third-party repo
/// involved. Override with the env var (see [`GGUF_REPO_ENV`]).
///
/// The `spelunk-cloud` org is the predecessor product's name, kept
/// deliberately. Renaming it buys a tidier URL and nothing else, and it is not
/// free: the org is part of the hf-hub cache key, so existing installs would
/// refetch, and the air-gapped provisioning procedure in `docs/server-setup.md`
/// hard-codes the current cache directory name in a copy-paste command. A
/// rename sweep leaves this alone; see `docs/model-attribution.md` for the same
/// reasoning in prose.
const DEFAULT_GGUF_REPO: &str = "spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF";

/// Filename of the canonical llama.cpp GGUF: llama.cpp tensor names
/// (`blk.N.*`), arch metadata, and the baked tokenizer + last-token pooling
/// config that `LlamaEmbedder` needs, Q8_0-quantized from the pinned upstream
/// revision (see `docs/third-party-models.md`). Hosted in [`DEFAULT_GGUF_REPO`].
const LLAMA_GGUF: &str = "f2llm-v2-330m-llama-q8_0.gguf";

/// Env var selecting where the embedder runs: `auto` (default), `gpu`, or
/// `cpu`. Deliberately API-agnostic values — no `vulkan`/`metal` — so the same
/// setting means the same thing on every platform. `cpu` forces the CPU engine
/// and skips GPU device selection.
const EMBED_DEVICE_ENV: &str = "INKENTRY_EMBED_DEVICE";

/// Reclaim `<etag>.part` staging files that nothing will ever finish.
///
/// hf-hub stages a download in `blobs/<etag>.part`, then reopens that file in
/// append mode and continues over an HTTP `Range` request. A partial belonging
/// to a file the current run is about to fetch is therefore progress worth
/// keeping, which is what `can_resume` guards. Every other partial is dead
/// weight no code path reads back: one sitting beside its own completed blob,
/// and all of them on a run that fetches nothing at all.
///
/// Returns how many were removed. Cleanup never fails a model load, so the
/// caller logs and carries on.
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

/// How to describe a model fetch in the log, given the cache it inherited.
///
/// Anything already in the repo's blob directory, a partial included, means
/// this machine has downloaded from here before. Calling that a first run
/// because the model file itself is absent sends the reader hunting for a
/// cache that does exist.
fn fetch_note(blobs_dir: &Path) -> &'static str {
    let inhabited = std::fs::read_dir(blobs_dir).is_ok_and(|mut e| e.next().is_some());
    if inhabited {
        "cache present, model file missing"
    } else {
        "first run"
    }
}

/// Put the downloaded GGUF at the stable flat path the loader reads from,
/// storing the ~345 MB of bytes once.
///
/// hf-hub materialises a download as `blobs/<etag>` plus a pointer under
/// `snapshots/<rev>/`, and hands back the pointer. A hard link from that one
/// file to the flat path is what keeps the cache at a single copy: NTFS grants
/// hard links without the elevation or Developer Mode a symlink needs, and
/// macOS and Linux link freely within a filesystem.
///
/// The pointer is resolved to its target first. On Unix it is a symlink whose
/// target is written relative to the snapshot directory, so linking the link
/// itself would leave a flat path resolving against the cache root, where that
/// target does not exist.
///
/// Two servers reach this concurrently on a cold cache: the flat path is
/// absent for the whole of the first download, and hf-hub answers the second
/// one from its cached pointer without taking a lock, handing back the very
/// file the first one just linked. Identity is therefore decided by inode
/// rather than by path, and the model is linked under a temporary name and
/// renamed into place. A second start finds the file already linked and does
/// nothing, or else replaces it atomically. Copying onto the destination in
/// place is what must never happen: when the two names turn out to be one
/// file, that truncates the model to nothing and the loader, seeing a file
/// present, never fetches it again.
///
/// hf-hub's own cache is left exactly as it found it. When a hard link cannot
/// be made the model is copied and the hub keeps its copy, so that case costs
/// a second copy on disk. Deleting the blob to reclaim it is not an option:
/// that leaves the snapshot pointer dangling, and hf-hub cannot recover from
/// it, since a later fetch re-downloads the model and then fails to recreate
/// a pointer that already exists.
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

/// A staging name beside the target, distinct per process so two servers
/// materialising at once cannot collide, and so the real path only ever sees a
/// rename.
fn staging_path(gguf_path: &Path) -> PathBuf {
    let mut name = gguf_path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    gguf_path.with_file_name(name)
}

/// A ready embedding backend plus the identity facts `/v1/health` surfaces
/// about it. `engine`/`device` exist so a field report can say *which* engine
/// on *which* device produced a problem without reading server logs.
pub struct LoadedEmbedder {
    pub backend: Arc<dyn inkentry_core::embeddings::EmbeddingBackend>,
    /// Always `"llama"` — the sole embedding engine.
    pub engine: &'static str,
    /// `"cpu"`, `"metal"`, `"vulkan"`, or `"gpu"` — the device the engine
    /// resolved at load.
    pub device: &'static str,
    /// A non-fatal, actionable note about how the device resolved, surfaced in
    /// the health body and `inkentry server status`. Set when a GPU was wanted
    /// but embedding fell back to CPU for a fixable reason (currently: a Linux
    /// DRM render node present but unopenable for lack of `render`-group
    /// membership). `None` when the device resolved as expected.
    pub note: Option<String>,
}

/// Load the llama.cpp embedding backend.
///
/// The device is resolved from `INKENTRY_EMBED_DEVICE`: `auto`/`gpu` try a GPU
/// and fall back to CPU when none is usable; `cpu` forces the CPU engine. A
/// malformed value is a deliberate hard error (see [`embed_device_request`]),
/// surfaced through `/v1/health` as `unavailable`.
pub fn load_backend(model_dir: Option<&Path>, embed_threads: usize) -> Result<LoadedEmbedder> {
    let requested = embed_device_request()?;
    let embedder = match model_dir {
        Some(dir) => load_llama_from_model_dir(dir, requested, embed_threads),
        None => load_llama_from_hub(requested, embed_threads),
    }?;
    let device = embedder.device();
    // A GPU was requested (not `cpu`) yet the engine resolved to CPU: no usable
    // GPU backend was selected. On Linux this is often a fixable permission
    // problem (missing `render`-group membership), worth an actionable log and a
    // health/status note. A `cpu` request running on CPU is expected — no note.
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

/// A DRM render node's openability, as it bears on a GPU-to-CPU embed fallback.
///
/// The distinction the caller acts on is permission (fixable by joining the
/// `render` group) versus everything else (a genuinely GPU-less host, or a GPU
/// the driver rejects for missing features — neither of which the render-group
/// advice would help).
#[cfg(feature = "embed-llama")]
#[derive(Debug, PartialEq, Eq)]
enum RenderNodeAccess {
    /// No `renderD*` node in the directory: no GPU render device present.
    NoNode,
    /// A render node exists but this process cannot open it (`EACCES`) — the
    /// `render`-group case. Carries the node path.
    PermissionDenied(String),
    /// A render node exists and opens: the GPU is present and reachable, so a
    /// CPU fallback is not a permission problem (the driver rejected it, or no
    /// Vulkan module/loader is present). Carries the node path.
    Reachable(String),
    /// A render node exists but the open failed for some non-permission reason
    /// (e.g. the device is busy or vanished mid-probe): nothing actionable.
    Unknown,
}

/// Classify the first `renderD*` node under `dri_dir` by whether this process
/// can open it read+write — what a Vulkan driver needs to use the GPU.
///
/// Directory-injected rather than hard-wired to `/dev/dri` so the classification
/// is unit-testable without a real GPU. The probe is one `open(O_RDWR)` and the
/// fd is dropped immediately; opening a render node is exactly what a GPU client
/// does and has no side effect on the device.
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

/// A GPU was requested but the llama engine resolved to CPU. Diagnose why on
/// Linux and, when it is actionable, log it and return a note for the health
/// body / `inkentry server status`. Returns `None` (and logs nothing) on a
/// genuinely GPU-less host, so it never nags a machine that simply has no GPU.
///
/// Cheap by construction: one directory read plus a single `open()` probe, no
/// subprocess and no Vulkan enumeration (the engine already computed the
/// device; this only explains a CPU outcome).
#[cfg(feature = "embed-llama")]
fn gpu_fallback_note() -> Option<String> {
    // DRM render nodes and the `render` group are a Linux concept; there is
    // nothing to advise on macOS/Windows. `/dev/dri` is absent there anyway,
    // but guard explicitly so the intent is clear.
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
            // The GPU is reachable but was not selected: a hardware/driver
            // limit (e.g. ggml rejecting a device that lacks 16-bit storage),
            // or no Vulkan module/loader. Not fixable by the render group, so
            // it earns an explanatory log but no actionable status note.
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

/// Reclaim the previous candle engine's cached artifacts, which the llama
/// engine never reads: the flat candle GGUF and `config.json` at the cache
/// root, and the candle GGUF + `tokenizer.json` entries in the hf-hub repo
/// cache (each snapshot pointer and the blob it resolves to). Keyed on the
/// exact old filenames, so it can never touch the llama GGUF ([`LLAMA_GGUF`]),
/// whose name differs and whose blob sits under its own pointer. Idempotent (a
/// second run finds nothing) and best-effort: a leftover file is wasted disk,
/// not a fault, so failures are ignored rather than failing the load.
#[cfg(feature = "embed-llama")]
fn reclaim_candle_artifacts(cache_dir: &Path, repo: &Repo) {
    // Candle wrote these two flat at the cache root; the llama engine writes
    // neither (its GGUF is `LLAMA_GGUF`, and it needs no separate config).
    const STALE_FLAT: [&str; 2] = ["f2llm-v2-330m-q8_0.gguf", "config.json"];
    // Candle fetched these into the shared hf-hub repo cache. `tokenizer.json`
    // is candle-only (the llama GGUF embeds its tokenizer), and the old GGUF
    // name differs from `LLAMA_GGUF`, so neither can name the llama artifact.
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
                // Resolve the pointer to its blob before removing it, so the
                // blob (the actual bytes) is reclaimed too. hf-hub materialises
                // a snapshot entry as a symlink into `blobs/`.
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

/// Load the llama.cpp engine's canonical GGUF via the Hugging Face Hub.
///
/// Single file — the canonical GGUF embeds its own tokenizer and config — but
/// otherwise the same cache handling as any large-model fetch: the download is
/// hard-linked (not copied) to the flat path the loader reads from (see
/// [`materialise_model`]), and staging files nothing can resume are reclaimed
/// (see [`prune_partial_downloads`]). Subsequent calls read from the local
/// cache with no network access.
#[cfg(feature = "embed-llama")]
fn load_llama_from_hub(device: DeviceRequest, embed_threads: usize) -> Result<LlamaEmbedder> {
    let cache_dir = model_cache_dir()?;
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating model cache dir {}", cache_dir.display()))?;
    let gguf_path = cache_dir.join(LLAMA_GGUF);

    let gguf_repo = prequantized_gguf_repo();
    let repo_id = Repo::new(gguf_repo.clone(), RepoType::Model);

    // A machine upgrading from a candle build carries that engine's cached
    // files, which this engine never reads. Reclaim them once, here on the load
    // path (idempotent, best-effort).
    reclaim_candle_artifacts(&cache_dir, &repo_id);

    let blobs_dir = cache_dir.join(repo_id.folder_name()).join("blobs");

    // Read before anything is fetched, so it describes the cache this run
    // inherited rather than the one it just populated.
    let note = fetch_note(&blobs_dir);

    // A partial is only worth keeping while this run may still fetch the file it
    // belongs to; with the GGUF already on disk nothing downloads, so nothing
    // can resume.
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

    // Size the context pool to the server's embed-admission capacity so every
    // admitted concurrent embed gets its own warm context — an interactive
    // embed never queues behind a bulk index batch. Single-sourced here rather
    // than a separate constant in inkentry-embed that could drift.
    LlamaEmbedder::load_from_path(
        &gguf_path,
        device,
        Some(embed_threads),
        crate::EMBED_QUEUE_CAPACITY,
    )
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
    // Size the context pool to the server's embed-admission capacity so every
    // admitted concurrent embed gets its own warm context — an interactive
    // embed never queues behind a bulk index batch. Single-sourced here rather
    // than a separate constant in inkentry-embed that could drift.
    LlamaEmbedder::load_from_path(
        &gguf_path,
        device,
        Some(embed_threads),
        crate::EMBED_QUEUE_CAPACITY,
    )
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

    // ── GPU-fallback render-node diagnostic ────────────────────────────────────

    #[cfg(feature = "embed-llama")]
    #[test]
    fn classify_render_nodes_no_node_for_empty_or_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(classify_render_nodes(dir.path()), RenderNodeAccess::NoNode);
        assert_eq!(
            classify_render_nodes(&dir.path().join("does-not-exist")),
            RenderNodeAccess::NoNode,
            "a missing /dev/dri (no DRM at all) is NoNode, not an error"
        );
    }

    #[cfg(feature = "embed-llama")]
    #[test]
    fn classify_render_nodes_ignores_non_render_nodes() {
        let dir = tempfile::tempdir().unwrap();
        // A GPU-less host can still have a `card0`/`by-path` under /dev/dri;
        // only `renderD*` is the compute render node this check is about.
        std::fs::write(dir.path().join("card0"), b"").unwrap();
        std::fs::create_dir(dir.path().join("by-path")).unwrap();
        assert_eq!(classify_render_nodes(dir.path()), RenderNodeAccess::NoNode);
    }

    #[cfg(feature = "embed-llama")]
    #[test]
    fn classify_render_nodes_reachable_for_openable_node() {
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("renderD128");
        std::fs::write(&node, b"").unwrap();
        assert_eq!(
            classify_render_nodes(dir.path()),
            RenderNodeAccess::Reachable(node.display().to_string()),
            "a render node this process can open read+write is reachable, not a permission case"
        );
    }

    #[cfg(all(feature = "embed-llama", unix))]
    #[test]
    fn classify_render_nodes_permission_denied_for_inaccessible_node() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("renderD128");
        std::fs::write(&node, b"").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Root bypasses DAC permission bits (CI containers often run as root),
        // so the open would succeed and there is nothing to assert.
        if std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
            .is_ok()
        {
            return;
        }
        assert_eq!(
            classify_render_nodes(dir.path()),
            RenderNodeAccess::PermissionDenied(node.display().to_string())
        );
    }

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

    // A typo'd device must fail loudly (surfaced through /v1/health as
    // `unavailable`) rather than silently running somewhere unintended. Uses
    // `serial` because it mutates a process-global env var.
    #[test]
    #[serial_test::serial(embed_device_env)]
    fn embed_device_request_rejects_an_unparseable_value() {
        let prev = std::env::var(EMBED_DEVICE_ENV).ok();
        unsafe { std::env::set_var(EMBED_DEVICE_ENV, "vulkan") };
        let result = embed_device_request();
        match prev {
            Some(v) => unsafe { std::env::set_var(EMBED_DEVICE_ENV, v) },
            None => unsafe { std::env::remove_var(EMBED_DEVICE_ENV) },
        }
        let err = result.expect_err("an unparseable device must be a hard error");
        assert!(
            err.to_string().contains(EMBED_DEVICE_ENV),
            "the error must name the offending env var: {err:#}"
        );
    }

    // The documented accepted values parse; unset/blank means `auto`.
    #[test]
    #[serial_test::serial(embed_device_env)]
    fn embed_device_request_accepts_documented_values() {
        let prev = std::env::var(EMBED_DEVICE_ENV).ok();

        let parse = |val: &str| {
            if val.is_empty() {
                unsafe { std::env::remove_var(EMBED_DEVICE_ENV) };
            } else {
                unsafe { std::env::set_var(EMBED_DEVICE_ENV, val) };
            }
            embed_device_request().expect("documented value must parse")
        };
        assert!(
            matches!(parse(""), DeviceRequest::Auto),
            "unset/blank ⇒ auto"
        );
        assert!(matches!(parse("auto"), DeviceRequest::Auto));
        assert!(matches!(parse("gpu"), DeviceRequest::Gpu));
        assert!(matches!(parse("cpu"), DeviceRequest::Cpu));

        match prev {
            Some(v) => unsafe { std::env::set_var(EMBED_DEVICE_ENV, v) },
            None => unsafe { std::env::remove_var(EMBED_DEVICE_ENV) },
        }
    }

    // The air-gapped loader rejects a non-directory --model-dir before any load,
    // naming the offline provisioning docs. Offline: errors before the GGUF.
    #[test]
    fn load_llama_from_model_dir_rejects_non_directory() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let msg = match load_llama_from_model_dir(file.path(), DeviceRequest::Cpu, 1) {
            Ok(_) => panic!("a non-directory --model-dir must be rejected"),
            Err(e) => format!("{e:#}"),
        };
        assert!(msg.contains("is not a directory"), "{msg}");
        assert!(msg.contains("docs/server-setup.md"), "{msg}");
    }

    // A model dir missing the canonical GGUF errors before any load, naming both
    // the missing file and the offline provisioning docs. Offline.
    #[test]
    fn load_llama_from_model_dir_missing_gguf_names_the_file_and_docs() {
        let dir = tempfile::tempdir().unwrap();
        let msg = match load_llama_from_model_dir(dir.path(), DeviceRequest::Cpu, 1) {
            Ok(_) => panic!("a model dir without the GGUF must be rejected"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            msg.contains(LLAMA_GGUF),
            "error must name the missing file: {msg}"
        );
        assert!(msg.contains("docs/server-setup.md"), "{msg}");
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

    // Fixture bytes standing in for the GGUF. The materialisation policy is
    // about directory entries and link counts, so the content is irrelevant
    // beyond being identifiable.
    const GGUF_BYTES: &[u8] = b"GGUF fixture bytes, not a real model";

    fn blobs_dir_with(cache: &std::path::Path, etag: &str, bytes: &[u8]) -> PathBuf {
        let blobs = cache
            .join("models--spelunk-cloud--F2LLM-v2-330M-Q8_0-GGUF")
            .join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join(etag), bytes).unwrap();
        blobs
    }

    #[test]
    fn materialise_model_links_rather_than_copying() {
        let cache = tempfile::tempdir().unwrap();
        let blobs = blobs_dir_with(cache.path(), "deadbeef", GGUF_BYTES);
        let blob = blobs.join("deadbeef");
        let flat = cache.path().join(LLAMA_GGUF);

        materialise_model(&blob, &flat).expect("materialise the downloaded model");

        assert_eq!(std::fs::read(&flat).unwrap(), GGUF_BYTES);
        assert!(
            blob.exists(),
            "a successful link must leave the hub blob in place"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let flat_meta = std::fs::metadata(&flat).unwrap();
            let blob_meta = std::fs::metadata(&blob).unwrap();
            assert_eq!(
                flat_meta.ino(),
                blob_meta.ino(),
                "the flat path must name the same file as the blob, not a second copy of it"
            );
            assert_eq!(
                flat_meta.nlink(),
                2,
                "one copy of the bytes must carry exactly two directory entries"
            );
        }
    }

    // hf-hub hands back the snapshots/<rev>/<file> pointer, a symlink whose
    // target is relative to the snapshot directory. Linking the symlink itself
    // would leave a flat path resolving against the cache root, where that
    // relative target does not exist.
    #[test]
    #[cfg(unix)]
    fn materialise_model_resolves_the_snapshot_pointer_to_its_blob() {
        let cache = tempfile::tempdir().unwrap();
        let blobs = blobs_dir_with(cache.path(), "deadbeef", GGUF_BYTES);
        let blob = blobs.join("deadbeef");

        let snapshot = cache
            .path()
            .join("models--spelunk-cloud--F2LLM-v2-330M-Q8_0-GGUF")
            .join("snapshots")
            .join("abc123");
        std::fs::create_dir_all(&snapshot).unwrap();
        let pointer = snapshot.join(LLAMA_GGUF);
        std::os::unix::fs::symlink("../../blobs/deadbeef", &pointer).unwrap();

        let flat = cache.path().join(LLAMA_GGUF);
        materialise_model(&pointer, &flat).expect("materialise via the snapshot pointer");

        assert!(
            !std::fs::symlink_metadata(&flat)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the flat path must be a real directory entry, not a copied symlink"
        );
        assert_eq!(std::fs::read(&flat).unwrap(), GGUF_BYTES);

        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(&flat).unwrap().ino(),
            std::fs::metadata(&blob).unwrap().ino()
        );
    }

    // A file already sitting at the flat path is replaced by the link. Nothing
    // is copied onto it in place, so there is no way to truncate it.
    #[test]
    fn materialise_model_replaces_an_unrelated_file_at_the_flat_path() {
        let cache = tempfile::tempdir().unwrap();
        let blobs = blobs_dir_with(cache.path(), "deadbeef", GGUF_BYTES);
        let blob = blobs.join("deadbeef");
        let flat = cache.path().join(LLAMA_GGUF);
        std::fs::write(&flat, b"a partially written earlier attempt").unwrap();

        materialise_model(&blob, &flat).expect("materialise over an existing file");

        assert_eq!(std::fs::read(&flat).unwrap(), GGUF_BYTES);
        assert!(
            blob.exists(),
            "hf-hub's own cache must never be modified by this loader"
        );
    }

    // Two servers starting on a cold cache: the first links the model into
    // place, and the second is handed hf-hub's cached pointer, which resolves
    // to the very inode the first one linked. Copying a file onto itself
    // truncates it to nothing, so the second start must leave it alone.
    #[test]
    #[cfg(unix)]
    fn materialise_model_leaves_a_model_already_linked_into_place_intact() {
        let cache = tempfile::tempdir().unwrap();
        let blobs = blobs_dir_with(cache.path(), "deadbeef", GGUF_BYTES);
        let blob = blobs.join("deadbeef");
        let flat = cache.path().join(LLAMA_GGUF);

        let snapshot = cache
            .path()
            .join("models--spelunk-cloud--F2LLM-v2-330M-Q8_0-GGUF")
            .join("snapshots")
            .join("abc123");
        std::fs::create_dir_all(&snapshot).unwrap();
        let pointer = snapshot.join(LLAMA_GGUF);
        std::os::unix::fs::symlink("../../blobs/deadbeef", &pointer).unwrap();

        // The first server has already linked the model into place.
        std::fs::hard_link(&blob, &flat).unwrap();

        materialise_model(&pointer, &flat).expect("a second start must not fail");

        assert_eq!(
            std::fs::metadata(&flat).unwrap().len() as usize,
            GGUF_BYTES.len(),
            "the model must keep its size, not be truncated to nothing"
        );
        assert_eq!(std::fs::read(&flat).unwrap(), GGUF_BYTES);
        assert!(blob.exists(), "hf-hub's blob must survive a second start");

        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(&flat).unwrap().ino(),
            std::fs::metadata(&blob).unwrap().ino(),
            "the flat path must still be the same file as the blob"
        );
    }

    #[test]
    fn materialise_model_is_idempotent_across_repeated_calls() {
        let cache = tempfile::tempdir().unwrap();
        let blobs = blobs_dir_with(cache.path(), "deadbeef", GGUF_BYTES);
        let blob = blobs.join("deadbeef");
        let flat = cache.path().join(LLAMA_GGUF);

        materialise_model(&blob, &flat).expect("first materialise");
        materialise_model(&blob, &flat).expect("second materialise");
        materialise_model(&blob, &flat).expect("third materialise");

        assert_eq!(std::fs::read(&flat).unwrap(), GGUF_BYTES);
        assert!(blob.exists(), "repeated starts must not consume the blob");

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let flat_meta = std::fs::metadata(&flat).unwrap();
            assert_eq!(flat_meta.ino(), std::fs::metadata(&blob).unwrap().ino());
            assert_eq!(
                flat_meta.nlink(),
                2,
                "repeating must not accumulate extra links"
            );
        }
    }

    // No staging file may survive a materialise, or the cache grows a stray
    // copy of the model on every start.
    #[test]
    fn materialise_model_leaves_no_staging_file_behind() {
        let cache = tempfile::tempdir().unwrap();
        let blobs = blobs_dir_with(cache.path(), "deadbeef", GGUF_BYTES);
        let flat = cache.path().join(LLAMA_GGUF);

        materialise_model(&blobs.join("deadbeef"), &flat).expect("materialise");

        let strays: Vec<_> = std::fs::read_dir(cache.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != LLAMA_GGUF && !n.starts_with("models--"))
            .collect();
        assert!(
            strays.is_empty(),
            "unexpected files in the cache: {strays:?}"
        );
    }

    // A partial next to its own completed blob belongs to a download that
    // already finished. Nothing will ever resume it, so it is pure waste.
    #[test]
    fn prune_removes_a_partial_whose_blob_already_completed() {
        let cache = tempfile::tempdir().unwrap();
        let blobs = blobs_dir_with(cache.path(), "deadbeef", GGUF_BYTES);
        let orphan = blobs.join("deadbeef.part");
        std::fs::write(&orphan, b"leftover from an interrupted run").unwrap();

        let reclaimed = prune_partial_downloads(&blobs, true).expect("prune");

        assert_eq!(
            reclaimed, 1,
            "the orphaned partial must be counted as reclaimed"
        );
        assert!(
            !orphan.exists(),
            "a partial next to its completed blob must be reclaimed"
        );
        assert!(
            blobs.join("deadbeef").exists(),
            "the completed blob itself must be untouched"
        );
    }

    // hf-hub reopens a partial in append mode and continues over an HTTP Range
    // request, so a partial for a file the run is about to fetch is worth
    // keeping. Once nothing will be fetched, no code path can pick it up again.
    #[test]
    fn prune_keeps_a_resumable_partial_only_while_a_fetch_can_resume_it() {
        let cache = tempfile::tempdir().unwrap();
        let blobs = blobs_dir_with(cache.path(), "deadbeef", GGUF_BYTES);
        let partial = blobs.join("cafebabe.part");
        std::fs::write(&partial, b"5 MB of a 339 MB download").unwrap();

        let kept = prune_partial_downloads(&blobs, true).expect("prune with a fetch pending");
        assert_eq!(
            kept, 0,
            "nothing may be reclaimed while the fetch can resume it"
        );
        assert!(
            partial.exists(),
            "a partial with no completed blob must survive for hf-hub to resume"
        );

        let reclaimed =
            prune_partial_downloads(&blobs, false).expect("prune with nothing to fetch");
        assert_eq!(reclaimed, 1);
        assert!(
            !partial.exists(),
            "a partial no fetch can resume must not be left orphaned"
        );
    }

    #[test]
    fn prune_leaves_completed_blobs_and_lock_files_alone() {
        let cache = tempfile::tempdir().unwrap();
        let blobs = blobs_dir_with(cache.path(), "deadbeef", GGUF_BYTES);
        let lock = blobs.join("deadbeef.lock");
        std::fs::write(&lock, b"").unwrap();

        let reclaimed = prune_partial_downloads(&blobs, false).expect("prune");

        assert_eq!(reclaimed, 0, "only .part files are ever removed");
        assert!(blobs.join("deadbeef").exists(), "blob must survive");
        assert!(lock.exists(), "hf-hub's own lock file must survive");
    }

    // The partial sweep and the air-gapped copy-paste procedure both address
    // hf-hub's cache by name, so the name is part of the contract.
    #[test]
    fn repo_cache_directory_matches_the_documented_hub_layout() {
        let cache = tempfile::tempdir().unwrap();
        let repo_id = Repo::new(DEFAULT_GGUF_REPO.to_string(), RepoType::Model);

        assert_eq!(
            cache.path().join(repo_id.folder_name()),
            cache
                .path()
                .join("models--spelunk-cloud--F2LLM-v2-330M-Q8_0-GGUF")
        );
    }

    // The reported defect: an interrupted download left a partial behind and
    // the next start still announced a first run while fetching the whole
    // model again. The wording has to follow the cache, not the model file.
    #[test]
    fn fetch_note_follows_cache_state_not_the_model_file() {
        let cache = tempfile::tempdir().unwrap();
        let blobs = cache.path().join("models--org--repo").join("blobs");

        assert_eq!(fetch_note(&blobs), "first run");

        std::fs::create_dir_all(&blobs).unwrap();
        assert_eq!(
            fetch_note(&blobs),
            "first run",
            "an empty cache is still a first run"
        );

        std::fs::write(blobs.join("cafebabe.part"), b"5 MB of a 339 MB download").unwrap();
        assert_eq!(
            fetch_note(&blobs),
            "cache present, model file missing",
            "an interrupted download must not be announced as a first run"
        );
    }

    // A first run has no repo directory at all, which is not an error.
    #[test]
    fn prune_tolerates_a_cache_that_does_not_exist_yet() {
        let cache = tempfile::tempdir().unwrap();
        let reclaimed = prune_partial_downloads(&cache.path().join("never-created"), true)
            .expect("an absent cache directory is a first run, not a failure");
        assert_eq!(reclaimed, 0);
    }

    #[test]
    fn reclaim_removes_candle_artifacts_but_keeps_the_llama_gguf() {
        let cache = tempfile::tempdir().unwrap();
        let root = cache.path();

        // Flat files: candle wrote the first two; the third is the llama GGUF.
        std::fs::write(root.join("f2llm-v2-330m-q8_0.gguf"), b"old candle gguf").unwrap();
        std::fs::write(root.join("config.json"), b"{}").unwrap();
        std::fs::write(root.join(LLAMA_GGUF), b"llama gguf").unwrap();

        // hf-hub snapshot dir carrying the candle GGUF, the candle tokenizer,
        // and the llama GGUF side by side.
        let repo = Repo::new(DEFAULT_GGUF_REPO.to_string(), RepoType::Model);
        let snap = root.join(repo.folder_name()).join("snapshots").join("rev0");
        std::fs::create_dir_all(&snap).unwrap();
        for f in ["f2llm-v2-330m-q8_0.gguf", "tokenizer.json", LLAMA_GGUF] {
            std::fs::write(snap.join(f), b"x").unwrap();
        }

        reclaim_candle_artifacts(root, &repo);

        // Every candle artifact is gone, flat and in the snapshot.
        assert!(!root.join("f2llm-v2-330m-q8_0.gguf").exists());
        assert!(!root.join("config.json").exists());
        assert!(!snap.join("f2llm-v2-330m-q8_0.gguf").exists());
        assert!(!snap.join("tokenizer.json").exists());
        // The llama GGUF is untouched — the whole point of keying on exact names.
        assert!(root.join(LLAMA_GGUF).exists());
        assert!(snap.join(LLAMA_GGUF).exists());

        // Idempotent: a second run over the cleaned cache is a no-op that panics
        // on nothing.
        reclaim_candle_artifacts(root, &repo);
        assert!(root.join(LLAMA_GGUF).exists());
    }

    // The pooled worker reuses one warm context across calls, clearing the KV
    // cache between chunks. That reuse must not change the vectors: many chunks
    // run through one reused context (a multi-chunk call, then repeated calls on
    // the now-warm context) must match each chunk decoded on its own. A leaked
    // KV state between chunks — the failure mode context reuse could introduce —
    // would surface here as drift on the later chunks. Ignored by default: needs
    // the canonical GGUF on disk and runs inference.
    #[cfg(feature = "embed-llama")]
    #[test]
    #[ignore = "requires the canonical F2LLM GGUF and runs inference"]
    fn llama_reused_context_matches_isolated_chunks() {
        use inkentry_core::embeddings::EmbeddingBackend;

        let llama = load_llama_from_hub(DeviceRequest::Auto, 4)
            .expect("load llama engine (canonical GGUF)");

        // Mixed lengths and scripts so a KV leak between neighbours of differing
        // size would show up.
        let texts: [&str; 6] = [
            "title: none | text: a",
            "title: l2_normalise | text: fn l2_normalise(v: &mut [f32]) { let norm = \
             v.iter().map(|x| x * x).sum::<f32>().sqrt(); for x in v { *x /= norm; } }",
            "title: none | text: SELECT id, title FROM notes WHERE archived = 0;",
            "title: none | text: 埋め込みは起動時にバックグラウンドで読み込まれます。",
            "title: none | text: the fall of the roman empire and the rise of byzantium",
            "title: none | text: cosine similarity between L2-normalised vectors is their dot product",
        ];

        let rt = tokio::runtime::Runtime::new().unwrap();
        // One multi-chunk call: all six run through one reused context in order.
        let batched = rt.block_on(llama.embed(&texts)).expect("multi-chunk embed");
        // Then each on its own, reusing the same now-warm pooled context.
        let singles: Vec<Vec<f32>> = texts
            .iter()
            .map(|t| {
                rt.block_on(llama.embed(&[*t]))
                    .expect("single embed")
                    .remove(0)
            })
            .collect();

        let cos = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        for (i, (b, s)) in batched.iter().zip(&singles).enumerate() {
            let c = cos(b, s);
            assert!(
                c >= 0.9999,
                "chunk {i}: vector from the reused context drifts from the isolated decode \
                 (cos={c:.6}); the KV cache is not being cleared cleanly between chunks"
            );
        }
    }
}
