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

    let reclaimed = prune_partial_downloads(&blobs, false).expect("prune with nothing to fetch");
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

    let llama =
        load_llama_from_hub(DeviceRequest::Auto, 4).expect("load llama engine (canonical GGUF)");

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
