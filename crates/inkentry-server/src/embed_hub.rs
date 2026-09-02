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

use anyhow::{Context, Result};
use hf_hub::{Cache, Repo, RepoType, api::sync::ApiBuilder};
use inkentry_embed::NativeEmbedder;

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

/// Load the F2LLM-v2-330M model, quantized to Q8_0, via the Hugging Face Hub.
///
/// Downloads our own pre-quantized GGUF (`f2llm-v2-330m-q8_0.gguf`) and
/// tokenizer (`tokenizer.json`) straight from
/// `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF` through the hf-hub cache
/// (checksum/resume reused). The first-run download is ~339 MB, cached under
/// [`model_cache_dir`], which is the platform's own local-data location and so
/// differs per OS (see "Where the model is cached" in
/// `docs/getting-started.md`). Set `INKENTRY_EMBEDDER_GGUF_REPO` to a
/// different `org/repo` to fetch both from there instead. `config.json` is
/// embedded in the binary (see [`CONFIG_JSON`]) and written to the same cache
/// directory so it lands next to the other artifacts as a real file.
///
/// The model is stored once: the download is linked, not copied, to the flat
/// path the loader reads from (see [`materialise_model`]), and staging files
/// nothing can resume are reclaimed (see [`prune_partial_downloads`]).
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

    let gguf_repo = prequantized_gguf_repo();
    let repo_id = Repo::new(gguf_repo.clone(), RepoType::Model);
    let blobs_dir = cache_dir.join(repo_id.folder_name()).join("blobs");

    // Read before anything is fetched, so it describes the cache this run
    // inherited rather than the one it just populated.
    let note = fetch_note(&blobs_dir);

    // A partial is only worth keeping while this run may still fetch the file
    // it belongs to; with both artifacts already on disk nothing downloads,
    // so nothing can resume.
    let will_fetch = !gguf_path.exists()
        || Cache::new(cache_dir.clone())
            .repo(repo_id.clone())
            .get("tokenizer.json")
            .is_none();
    match prune_partial_downloads(&blobs_dir, will_fetch) {
        Ok(0) => {}
        Ok(n) => tracing::info!("reclaimed {n} unusable partial download(s) from the model cache"),
        Err(e) => tracing::warn!("could not sweep the model cache for partial downloads: {e:#}"),
    }

    let api = ApiBuilder::new()
        .with_cache_dir(cache_dir)
        .build()
        .context("building HuggingFace Hub API client")?;
    let repo = api.repo(repo_id);

    let tokenizer_path = repo
        .get("tokenizer.json")
        .with_context(|| format!("downloading tokenizer.json from {gguf_repo}"))?;

    if !gguf_path.exists() {
        tracing::info!("fetching pre-quantized F2LLM-v2-330M Q8_0 GGUF from {gguf_repo} ({note})…");
        let downloaded = repo
            .get(QUANT_GGUF)
            .with_context(|| format!("downloading {QUANT_GGUF} from {gguf_repo}"))?;
        materialise_model(&downloaded, &gguf_path)?;
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
/// storing the ~339 MB of bytes once.
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
        let flat = cache.path().join(QUANT_GGUF);

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
        let pointer = snapshot.join(QUANT_GGUF);
        std::os::unix::fs::symlink("../../blobs/deadbeef", &pointer).unwrap();

        let flat = cache.path().join(QUANT_GGUF);
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
        let flat = cache.path().join(QUANT_GGUF);
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
        let flat = cache.path().join(QUANT_GGUF);

        let snapshot = cache
            .path()
            .join("models--spelunk-cloud--F2LLM-v2-330M-Q8_0-GGUF")
            .join("snapshots")
            .join("abc123");
        std::fs::create_dir_all(&snapshot).unwrap();
        let pointer = snapshot.join(QUANT_GGUF);
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
        let flat = cache.path().join(QUANT_GGUF);

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
        let flat = cache.path().join(QUANT_GGUF);

        materialise_model(&blobs.join("deadbeef"), &flat).expect("materialise");

        let strays: Vec<_> = std::fs::read_dir(cache.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != QUANT_GGUF && !n.starts_with("models--"))
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
