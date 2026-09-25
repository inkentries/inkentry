use anyhow::Result;
use ignore::WalkBuilder;
use indicatif::{MultiProgress, ProgressBar};

use super::super::ui::{is_tty, progress_style, short_path};
use super::IndexArgs;
use super::graph_pass;
#[cfg(feature = "rich-formats")]
use crate::indexer::docparser::parse_doc;
use crate::{
    indexer::parser::{
        SourceParser, detect_doc_language, detect_language, detect_text_language, is_binary_file,
    },
    search::tokens::estimate_tokens,
    storage::Database,
};

// Checked via metadata before any read, so a huge or compression-bomb file can't OOM the indexer.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

// Falls back to 0, which sorts last under the embed queue's `mtime DESC` order.
fn stat_mtime(path: &std::path::Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn is_file_too_large(path: &std::path::Path, path_str: &str) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_FILE_BYTES => {
            tracing::warn!(
                "skipping {path_str}: file too large ({} bytes > {MAX_FILE_BYTES} byte cap)",
                meta.len()
            );
            true
        }
        _ => false,
    }
}

pub(super) struct ParseResult {
    pub indexed: u64,
    pub removed: u64,
    #[allow(dead_code)]
    pub filtered: u64,
}

fn filtered_notice(filtered: u64) -> String {
    format!(
        "Filtered out {filtered} generated/vendored/data file(s) \
         (built-in index filter; override in [index] of .inkentry/config.toml)"
    )
}

struct ParseAcc {
    indexed: u64,
    skipped: u64,
    reextract_edges: bool,
}

pub(super) fn run_parse_phase(
    root: &std::path::Path,
    db: &Database,
    args: &IndexArgs,
    mp: &MultiProgress,
    cfg: &crate::config::Config,
) -> Result<ParseResult> {
    let filter = inkentry_core::indexer::filter::IndexFilter::build(
        &cfg.index.exclude,
        cfg.index.use_default_excludes,
        cfg.index.detect_generated,
    )?;
    let (files, filtered) = collect_files(root, &filter)?;

    if files.is_empty() {
        if filtered > 0 {
            println!("{}", filtered_notice(filtered));
        }
        println!("No supported source files found in {}", root.display());
        return Ok(ParseResult {
            indexed: 0,
            removed: 0,
            filtered,
        });
    }

    let parse_bar = if is_tty() && !crate::utils::is_agent_mode() {
        let bar = mp.add(ProgressBar::new(files.len() as u64));
        bar.set_style(progress_style("Parsing  "));
        bar
    } else {
        ProgressBar::hidden()
    };

    let mut acc = ParseAcc {
        indexed: 0,
        skipped: 0,
        reextract_edges: graph_pass::reextraction_owed(db)?,
    };

    for entry in &files {
        let path = entry.path();
        // Root-relative and `/`-separated so the index is portable across OSes.
        let rel = path.strip_prefix(root).unwrap_or(path);
        let path_str = inkentry_core::utils::normalize_index_path(&rel.to_string_lossy());
        parse_bar.set_message(short_path(&path_str));

        #[cfg(feature = "rich-formats")]
        if let Some(doc_lang) = detect_doc_language(path)
            && process_doc_file(path, &path_str, doc_lang, db, args, &mut acc)?
        {
            parse_bar.inc(1);
            continue;
        }

        #[cfg(feature = "rich-formats")]
        if detect_language(path) == Some("pdf")
            && process_pdf_file(path, &path_str, db, args, &mut acc)?
        {
            parse_bar.inc(1);
            continue;
        }

        process_text_file(path, &path_str, db, args, &mut acc)?;
        parse_bar.inc(1);
    }

    parse_bar.finish_with_message(format!(
        "{} files parsed ({} skipped, {} new/changed)",
        acc.indexed, acc.skipped, acc.indexed
    ));

    if filtered > 0 {
        println!("{}", filtered_notice(filtered));
    }

    let removed = cleanup_stale(&files, root, db)?;
    graph_pass::finish(db, acc.reextract_edges)?;
    let ParseAcc { indexed, .. } = acc;

    // --force re-chunked every file; refresh the stamp or the chunker drift warning never clears.
    if args.force {
        db.stamp_chunker_config(&inkentry_core::indexer::chunker_config_id())?;
    }

    Ok(ParseResult {
        indexed,
        removed,
        filtered,
    })
}

pub(super) fn missing_embedding_texts(db: &Database) -> Result<Vec<(i64, String, usize)>> {
    let mut out = Vec::new();
    for (chunk_id, name, metadata, summary, content, token_count) in
        db.chunks_missing_embeddings()?
    {
        let tokens = effective_token_count(token_count, &content);
        let text =
            reconstruct_embedding_text(name.as_deref(), metadata.as_deref(), summary, content);
        out.push((chunk_id, text, tokens));
    }
    Ok(out)
}

// Stored 0 is a pre-backfill row; floor at 1 so token-weighted math never divides by zero.
fn effective_token_count(stored: usize, content: &str) -> usize {
    let tc = if stored == 0 {
        estimate_tokens(content)
    } else {
        stored
    };
    tc.max(1)
}

// Must match `Chunk::embedding_text`; the docstring is read back from the metadata JSON.
pub(super) fn reconstruct_embedding_text(
    name: Option<&str>,
    metadata: Option<&str>,
    summary: Option<String>,
    content: String,
) -> String {
    let title = name.unwrap_or("none");
    let docstring = metadata
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .and_then(|v| {
            v.get("docstring")
                .and_then(|d| d.as_str().map(str::to_string))
        });
    let body = match docstring {
        Some(doc) => format!("{doc}\n{content}"),
        None => content,
    };
    match summary {
        Some(summary) => format!("title: {title} | summary: {summary} | text: {body}"),
        None => format!("title: {title} | text: {body}"),
    }
}

// Sensitive files are dropped by the walk before `filter` sees them, so `[index]` config can never re-include them.
fn collect_files(
    root: &std::path::Path,
    filter: &inkentry_core::indexer::filter::IndexFilter,
) -> Result<(Vec<ignore::DirEntry>, u64)> {
    use inkentry_core::indexer::filter::Decision;

    let sensitive_patterns = [
        "!.env",
        "!.env.*",
        "!*.pem",
        "!*.key",
        "!*.p12",
        "!*.pfx",
        "!*.p8",
        "!*.cer",
        "!*.crt",
        "!*.der",
        "!id_rsa",
        "!id_ecdsa",
        "!id_ed25519",
        "!id_dsa",
        "!*.keystore",
        "!*.jks",
        "!.netrc",
        "!.npmrc",
    ];
    let mut walk = WalkBuilder::new(root);
    walk.standard_filters(true);
    walk.add_custom_ignore_filename(".inkentryignore");
    let mut ob = ignore::overrides::OverrideBuilder::new(root);
    ob.case_insensitive(true).ok();
    for pat in &sensitive_patterns {
        ob.add(pat).ok();
    }
    if let Ok(ov) = ob.build() {
        walk.overrides(ov);
    }

    // Prune excluded dirs so the walk never descends into them; files are counted individually below.
    let root_owned = root.to_path_buf();
    let dir_filter = filter.clone();
    walk.filter_entry(move |entry| {
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if !is_dir {
            return true;
        }
        match entry.path().strip_prefix(&root_owned) {
            // Never prune the root itself (empty relative path).
            Ok(rel) if !rel.as_os_str().is_empty() => !dir_filter.prune_dir(rel),
            _ => true,
        }
    });

    let mut files = Vec::new();
    let mut filtered = 0u64;
    for entry in walk.build().filter_map(|e| e.ok()) {
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let p = entry.path();
        // Count only files the indexer would otherwise ingest.
        if !(detect_language(p).is_some()
            || detect_text_language(p).is_some()
            || detect_doc_language(p).is_some())
        {
            continue;
        }
        let rel = p.strip_prefix(root).unwrap_or(p);
        match filter.decide(rel, false) {
            Decision::Exclude(mi) => {
                tracing::debug!(
                    "index filter: excluding {} (matched {:?}, {})",
                    rel.display(),
                    mi.pattern,
                    if mi.from_default { "default" } else { "user" },
                );
                filtered += 1;
                continue;
            }
            // A user `!` re-include also exempts the file from generated-marker detection.
            Decision::ForceInclude(_) => {
                files.push(entry);
                continue;
            }
            Decision::Keep => {}
        }
        if filter.detect_generated()
            && let Some(marker) = inkentry_core::indexer::filter::generated_marker(p)
        {
            tracing::debug!(
                "index filter: excluding {} (generated marker: {})",
                rel.display(),
                marker,
            );
            filtered += 1;
            continue;
        }
        files.push(entry);
    }
    Ok((files, filtered))
}

#[cfg(feature = "rich-formats")]
fn process_doc_file(
    path: &std::path::Path,
    path_str: &str,
    doc_lang: &'static str,
    db: &Database,
    args: &IndexArgs,
    acc: &mut ParseAcc,
) -> Result<bool> {
    if is_file_too_large(path, path_str) {
        acc.skipped += 1;
        return Ok(true);
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("read error for {path_str}: {e}");
            return Ok(true);
        }
    };
    let hash = format!("{}", blake3::hash(&bytes));
    if !args.force
        && let Some(existing) = db.file_hash(path_str)?
        && existing == hash
    {
        acc.skipped += 1;
        return Ok(true);
    }
    let chunks = parse_doc(&bytes, path_str, doc_lang);
    let file_id = db.upsert_file(path_str, Some(doc_lang), &hash, stat_mtime(path))?;
    db.delete_embeddings_for_file(file_id)?;
    db.delete_chunks_for_file(file_id)?;
    store_chunks(&chunks, path_str, file_id, db)?;
    acc.indexed += 1;
    Ok(true)
}

#[cfg(feature = "rich-formats")]
fn process_pdf_file(
    path: &std::path::Path,
    path_str: &str,
    db: &Database,
    args: &IndexArgs,
    acc: &mut ParseAcc,
) -> Result<bool> {
    if is_file_too_large(path, path_str) {
        acc.skipped += 1;
        return Ok(true);
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("read error for {path_str}: {e}");
            return Ok(true);
        }
    };
    let hash = format!("{}", blake3::hash(&bytes));
    if !args.force
        && let Some(existing) = db.file_hash(path_str)?
        && existing == hash
    {
        return Ok(true);
    }
    match crate::indexer::pdf::extract_pdf_text(path) {
        Ok(pages) => {
            let file_id = db.upsert_file(path_str, Some("pdf"), &hash, stat_mtime(path))?;
            db.delete_embeddings_for_file(file_id)?;
            db.delete_chunks_for_file(file_id)?;
            let chunks = pages_to_chunks(pages, path_str);
            store_chunks(&chunks, path_str, file_id, db)?;
            acc.indexed += 1;
        }
        Err(e) => {
            tracing::warn!("skipping PDF {}: {e}", path.display());
        }
    }
    Ok(true)
}

#[cfg(feature = "rich-formats")]
fn pages_to_chunks(pages: Vec<(u32, String)>, path_str: &str) -> Vec<crate::indexer::Chunk> {
    pages
        .into_iter()
        .map(|(page_num, text)| crate::indexer::Chunk {
            file_path: path_str.to_string(),
            language: "pdf".to_string(),
            kind: crate::indexer::ChunkKind::Section,
            name: Some(format!("page {page_num}")),
            start_line: page_num as usize,
            end_line: page_num as usize,
            content: text,
            docstring: None,
            parent_scope: None,
            summary: None,
        })
        .collect()
}

fn process_text_file(
    path: &std::path::Path,
    path_str: &str,
    db: &Database,
    args: &IndexArgs,
    acc: &mut ParseAcc,
) -> Result<()> {
    let language = detect_language(path)
        .or_else(|| detect_text_language(path))
        .unwrap(); // safe: files were filtered to only include detectable files

    if matches!(language, "text" | "markdown") && is_binary_file(path) {
        return Ok(());
    }
    if is_file_too_large(path, path_str) {
        acc.skipped += 1;
        return Ok(());
    }
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("skipping {path_str}: {e}");
            return Ok(());
        }
    };
    let hash = format!("{}", blake3::hash(source.as_bytes()));

    if !args.force
        && let Some(existing) = db.file_hash(path_str)?
        && existing == hash
        && db.file_has_chunks(path_str)?
    {
        if acc.reextract_edges {
            let chunked_by_tree = db.file_chunked_by_tree(path_str)?;
            graph_pass::store_edges(db, &source, path_str, language, chunked_by_tree);
        }
        acc.skipped += 1;
        return Ok(());
    }

    let chunks = match SourceParser::parse(&source, path_str, language) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("parse error for {path_str}: {e}");
            return Ok(());
        }
    };

    let file_id = db.upsert_file(path_str, Some(language), &hash, stat_mtime(path))?;
    // No transaction spans this hash write and the chunk writes below; a crash leaves a current hash
    // with zero chunks, which the `file_has_chunks` check above repairs on the next run.
    super::crash_test_hook::pause_at("after_index_hash_write", path_str);
    db.delete_embeddings_for_file(file_id)?;
    db.delete_chunks_for_file(file_id)?;

    graph_pass::store_edges(
        db,
        &source,
        path_str,
        language,
        graph_pass::chunked_by_tree_as_stored(&chunks),
    );

    store_chunks(&chunks, path_str, file_id, db)?;
    acc.indexed += 1;
    Ok(())
}

pub(super) fn holds_secret(chunk: &crate::indexer::Chunk) -> bool {
    crate::indexer::secrets::contains_secret(&chunk.embedding_text())
}

fn store_chunks(
    chunks: &[crate::indexer::Chunk],
    path_str: &str,
    file_id: i64,
    db: &Database,
) -> Result<()> {
    for chunk in chunks {
        // Dropped before the metadata JSON is built so a secret in the docstring never lands in stored metadata.
        if holds_secret(chunk) {
            tracing::warn!(
                "skipping chunk '{}' in {path_str} (possible secret detected)",
                chunk.name.as_deref().unwrap_or("<anonymous>"),
            );
            continue;
        }
        let metadata =
            serde_json::json!({ "docstring": chunk.docstring, "parent_scope": chunk.parent_scope });
        let tc = estimate_tokens(&chunk.content);
        db.insert_chunk(
            file_id,
            &chunk.kind.to_string(),
            chunk.name.as_deref(),
            chunk.start_line,
            chunk.end_line,
            &chunk.content,
            Some(&metadata.to_string()),
            tc,
        )?;
    }
    Ok(())
}

fn cleanup_stale(files: &[ignore::DirEntry], root: &std::path::Path, db: &Database) -> Result<u64> {
    let visited: std::collections::HashSet<String> = files
        .iter()
        .map(|e| {
            let p = e.path();
            inkentry_core::utils::normalize_index_path(
                &p.strip_prefix(root).unwrap_or(p).to_string_lossy(),
            )
        })
        .collect();
    // "" matches every stored path.
    let all_indexed = db.file_paths_under("")?;
    let mut removed = 0u64;
    for (id, path) in all_indexed {
        if !visited.contains(&path) {
            db.delete_file(id, &path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::OnceLock;

    fn register_sqlite_vec() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });
    }

    fn open_db() -> Database {
        register_sqlite_vec();
        Database::open(std::path::Path::new(":memory:")).expect("open in-memory Database")
    }

    fn default_args(path: std::path::PathBuf) -> IndexArgs {
        IndexArgs {
            path,
            db: None,
            batch_size: 64,
            force: false,
            recount: false,
            no_summaries: false,
            background_phases: false,
            embed_phases: false,
            detach: false,
            detach_embed: false,
            config_path: None,
        }
    }

    // Sparse via set_len so the test never allocates the bytes.
    fn make_oversized_sparse_file() -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("create temp file");
        file.as_file()
            .set_len(MAX_FILE_BYTES + 1)
            .expect("set_len on temp file");
        file
    }

    #[test]
    fn reconstruct_embedding_text_matches_chunk_embedding_text() {
        use crate::indexer::{Chunk, ChunkKind};

        let cases = [
            (Some("do_thing"), Some("Does the thing.")),
            (Some("do_thing"), None),
            (None, Some("Anonymous doc.")),
            (None, None),
        ];
        for (name, docstring) in cases {
            let chunk = Chunk {
                file_path: "src/lib.rs".to_string(),
                language: "rust".to_string(),
                kind: ChunkKind::Function,
                name: name.map(str::to_string),
                start_line: 1,
                end_line: 3,
                content: "fn do_thing() {}".to_string(),
                docstring: docstring.map(str::to_string),
                parent_scope: None,
                summary: None,
            };
            let metadata = serde_json::json!({
                "docstring": chunk.docstring,
                "parent_scope": chunk.parent_scope,
            })
            .to_string();

            let reconstructed =
                reconstruct_embedding_text(name, Some(&metadata), None, chunk.content.clone());
            assert_eq!(
                reconstructed,
                chunk.embedding_text(),
                "reconstruction diverged for name={name:?} docstring={docstring:?}"
            );
        }
    }

    #[test]
    fn reconstruct_embedding_text_matches_chunk_embedding_text_with_summary() {
        use crate::indexer::{Chunk, ChunkKind};

        let cases = [
            (
                Some("do_thing"),
                Some("Does the thing."),
                "Summarised: does the thing.",
            ),
            (Some("do_thing"), None, "Summarised: no docstring."),
            (None, Some("Anonymous doc."), "Summarised: anonymous."),
            (None, None, "Summarised: bare."),
        ];
        for (name, docstring, summary) in cases {
            let chunk = Chunk {
                file_path: "src/lib.rs".to_string(),
                language: "rust".to_string(),
                kind: ChunkKind::Function,
                name: name.map(str::to_string),
                start_line: 1,
                end_line: 3,
                content: "fn do_thing() {}".to_string(),
                docstring: docstring.map(str::to_string),
                parent_scope: None,
                summary: Some(summary.to_string()),
            };
            let metadata = serde_json::json!({
                "docstring": chunk.docstring,
                "parent_scope": chunk.parent_scope,
            })
            .to_string();

            let reconstructed = reconstruct_embedding_text(
                name,
                Some(&metadata),
                Some(summary.to_string()),
                chunk.content.clone(),
            );
            assert_eq!(
                reconstructed,
                chunk.embedding_text(),
                "reconstruction diverged for name={name:?} docstring={docstring:?} summary={summary:?}"
            );
        }
    }

    #[test]
    fn reindex_backfills_unembedded_chunks_without_reparsing() {
        use indicatif::MultiProgress;

        let db = open_db();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "/// Doc for foo.\npub fn foo() -> i32 { 1 }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.rs"),
            "pub struct Bar { x: i32 }\npub fn bar() {}\n",
        )
        .unwrap();

        let args = default_args(dir.path().to_path_buf());
        let mp = MultiProgress::new();

        let cfg = crate::config::Config::default();
        let first = run_parse_phase(dir.path(), &db, &args, &mp, &cfg).expect("first parse phase");
        assert!(
            first.indexed >= 2,
            "both fixture files must be indexed on the first run"
        );

        let queue_run1 = missing_embedding_texts(&db).expect("queue after run 1");
        assert!(
            !queue_run1.is_empty(),
            "a parse-only run must leave chunks for the embed phase to pick up"
        );
        let mut queued_run1: Vec<i64> = queue_run1.iter().map(|(id, ..)| *id).collect();
        queued_run1.sort();

        let second =
            run_parse_phase(dir.path(), &db, &args, &mp, &cfg).expect("second parse phase");
        assert_eq!(
            second.indexed, 0,
            "no file changed — the hash-based skip must reparse nothing on the second run"
        );
        let queue_run2 = missing_embedding_texts(&db).expect("queue after run 2");
        assert!(
            !queue_run2.is_empty(),
            "the DB-driven queue must still surface the missing-embedding chunks even though indexed == 0"
        );

        // Identical ids prove no delete+reinsert, i.e. no reparse.
        let mut backfilled: Vec<i64> = queue_run2.iter().map(|(id, ..)| *id).collect();
        backfilled.sort();
        assert_eq!(
            backfilled, queued_run1,
            "the DB-driven queue must surface the same chunk ids across runs (no reparse / re-chunk)"
        );

        let mut texts_run1 = queue_run1.clone();
        texts_run1.sort_by_key(|(id, ..)| *id);
        let mut texts_run2 = queue_run2.clone();
        texts_run2.sort_by_key(|(id, ..)| *id);
        assert_eq!(
            texts_run2, texts_run1,
            "reconstructed embedding text must be byte-identical across runs"
        );
    }

    #[test]
    fn force_reindex_refreshes_the_chunker_config_stamp() {
        let db = open_db();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "pub fn foo() -> i32 { 1 }\n").unwrap();

        let stale = "max_chunk_tokens=999999";
        db.ensure_chunker_config(stale).expect("stamp old config");

        let mp = MultiProgress::new();
        let cfg = crate::config::Config::default();
        let current = inkentry_core::indexer::chunker_config_id();

        let args = default_args(dir.path().to_path_buf());
        run_parse_phase(dir.path(), &db, &args, &mp, &cfg).expect("normal parse phase");
        assert_eq!(
            db.chunker_config().unwrap().as_deref(),
            Some(stale),
            "a normal run must not silently clear the drift"
        );
        assert_eq!(
            db.ensure_chunker_config(&current).expect("drift check"),
            Some(stale.to_string()),
            "the stale stamp must still be reported as drift before --force"
        );

        let mut force_args = default_args(dir.path().to_path_buf());
        force_args.force = true;
        run_parse_phase(dir.path(), &db, &force_args, &mp, &cfg).expect("force parse phase");
        assert_eq!(
            db.chunker_config().unwrap().as_deref(),
            Some(current.as_str()),
            "a --force re-index must refresh the stamp to the current config"
        );

        assert_eq!(
            db.ensure_chunker_config(&current)
                .expect("post-force drift check"),
            None,
            "after --force, the same config must no longer be reported as drift"
        );
    }

    #[test]
    fn missing_embedding_texts_returns_only_unembedded_chunks_from_db() {
        use crate::indexer::{Chunk, ChunkKind};

        let db = open_db();
        let file_id = db
            .upsert_file("src/lib.rs", Some("rust"), "hash0", 0)
            .unwrap();

        let mut ids = Vec::new();
        let chunks = [
            ("alpha", Some("Doc for alpha."), "fn alpha() {}"),
            ("beta", None, "fn beta() {}"),
            ("gamma", Some("Doc for gamma."), "fn gamma() {}"),
        ];
        for (name, docstring, content) in chunks {
            let chunk = Chunk {
                file_path: "src/lib.rs".to_string(),
                language: "rust".to_string(),
                kind: ChunkKind::Function,
                name: Some(name.to_string()),
                start_line: 1,
                end_line: 2,
                content: content.to_string(),
                docstring: docstring.map(str::to_string),
                parent_scope: None,
                summary: None,
            };
            let metadata = serde_json::json!({
                "docstring": chunk.docstring,
                "parent_scope": chunk.parent_scope,
            })
            .to_string();
            let id = db
                .insert_chunk(
                    file_id,
                    "function",
                    Some(name),
                    1,
                    2,
                    content,
                    Some(&metadata),
                    1,
                )
                .unwrap();
            ids.push((id, chunk));
        }

        let (beta_id, _) = &ids[1];
        db.insert_embedding(
            *beta_id,
            &vec![0.1f32; inkentry_core::embeddings::EMBEDDING_DIM],
        )
        .unwrap();

        let missing = missing_embedding_texts(&db).expect("missing_embedding_texts");

        let got_ids: Vec<i64> = missing.iter().map(|(id, ..)| *id).collect();
        assert_eq!(
            got_ids,
            vec![ids[0].0, ids[2].0],
            "only the un-embedded chunks (alpha, gamma) must be queued, in id order"
        );
        assert!(
            !got_ids.contains(beta_id),
            "the already-embedded chunk must not be re-queued"
        );

        for (queued_id, queued_text, _) in &missing {
            let (_, chunk) = ids.iter().find(|(id, _)| id == queued_id).unwrap();
            assert_eq!(
                queued_text,
                &chunk.embedding_text(),
                "queued text must match Chunk::embedding_text for chunk {queued_id}"
            );
        }
    }

    #[test]
    fn missing_embedding_texts_is_empty_when_all_embedded() {
        let db = open_db();
        let file_id = db
            .upsert_file("src/lib.rs", Some("rust"), "hash0", 0)
            .unwrap();
        let id = db
            .insert_chunk(file_id, "function", Some("f"), 1, 2, "fn f() {}", None, 1)
            .unwrap();
        db.insert_embedding(id, &vec![0.1f32; inkentry_core::embeddings::EMBEDDING_DIM])
            .unwrap();

        assert!(
            missing_embedding_texts(&db).unwrap().is_empty(),
            "a fully-embedded index yields an empty detached embed queue"
        );
    }

    fn set_file_mtime(path: &std::path::Path, unix_secs: u64) {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix_secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[test]
    fn stat_mtime_nonexistent_path_falls_back_to_zero() {
        let missing = std::path::Path::new("/nonexistent/definitely-not-a-real-path.rs");
        assert_eq!(
            stat_mtime(missing),
            0,
            "an unstattable path must fall back to 0, not panic"
        );
    }

    #[test]
    fn stat_mtime_pre_epoch_time_falls_back_to_zero_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ancient.rs");
        std::fs::write(&f, "pub fn ancient() {}\n").unwrap();
        let pre_epoch = std::time::UNIX_EPOCH - std::time::Duration::from_secs(100);
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(pre_epoch)
            .unwrap();

        assert_eq!(
            stat_mtime(&f),
            0,
            "a pre-epoch mtime must fall back to 0, not panic on the i64 cast"
        );
    }

    #[test]
    fn stat_mtime_far_future_time_returns_positive_no_panic() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("future.rs");
        std::fs::write(&f, "pub fn future() {}\n").unwrap();
        set_file_mtime(&f, 4_300_000_000);

        assert_eq!(
            stat_mtime(&f),
            4_300_000_000,
            "a far-future mtime must round-trip verbatim, not overflow or panic"
        );
    }

    #[test]
    fn parse_captures_mtime_and_queue_orders_by_recency() {
        use indicatif::MultiProgress;

        let db = open_db();
        let dir = tempfile::tempdir().unwrap();
        let older = dir.path().join("older.rs");
        let newer = dir.path().join("newer.rs");
        std::fs::write(&older, "pub fn older_fn() {}\n").unwrap();
        std::fs::write(&newer, "pub fn newer_fn() {}\n").unwrap();
        set_file_mtime(&older, 1_000);
        set_file_mtime(&newer, 2_000);

        let args = default_args(dir.path().to_path_buf());
        let mp = MultiProgress::new();
        let cfg = crate::config::Config::default();
        run_parse_phase(dir.path(), &db, &args, &mp, &cfg).expect("parse phase");

        assert_eq!(db.file_mtime("older.rs").unwrap(), Some(1_000));
        assert_eq!(db.file_mtime("newer.rs").unwrap(), Some(2_000));

        let queue_ids: Vec<i64> = missing_embedding_texts(&db)
            .expect("queue")
            .iter()
            .map(|(id, ..)| *id)
            .collect();
        let newer_ids: Vec<i64> = db
            .chunks_for_file("newer.rs")
            .unwrap()
            .iter()
            .map(|c| c.chunk_id)
            .collect();
        let older_ids: Vec<i64> = db
            .chunks_for_file("older.rs")
            .unwrap()
            .iter()
            .map(|c| c.chunk_id)
            .collect();
        assert!(
            !newer_ids.is_empty() && !older_ids.is_empty(),
            "both files chunked"
        );
        let last_newer = queue_ids
            .iter()
            .rposition(|id| newer_ids.contains(id))
            .expect("newer chunks queued");
        let first_older = queue_ids
            .iter()
            .position(|id| older_ids.contains(id))
            .expect("older chunks queued");
        assert!(
            last_newer < first_older,
            "all newer-file chunks must precede older-file chunks: {queue_ids:?}"
        );
    }

    #[test]
    fn unchanged_file_retains_stored_mtime_on_reindex() {
        use indicatif::MultiProgress;

        let db = open_db();
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("keep.rs");
        std::fs::write(&f, "pub fn keep() {}\n").unwrap();
        set_file_mtime(&f, 1_000);

        let args = default_args(dir.path().to_path_buf());
        let mp = MultiProgress::new();
        let cfg = crate::config::Config::default();

        let r1 = run_parse_phase(dir.path(), &db, &args, &mp, &cfg).expect("run 1");
        assert!(r1.indexed >= 1);
        assert_eq!(
            db.file_mtime("keep.rs").unwrap(),
            Some(1_000),
            "run 1 stores the file's filesystem mtime"
        );

        set_file_mtime(&f, 5_000);
        let r2 = run_parse_phase(dir.path(), &db, &args, &mp, &cfg).expect("run 2");
        assert_eq!(
            r2.indexed, 0,
            "unchanged content → the file is hash-skipped, not reparsed"
        );
        assert_eq!(
            db.file_mtime("keep.rs").unwrap(),
            Some(1_000),
            "a skipped file's stored mtime is retained, not overwritten with the new FS mtime"
        );
    }

    #[test]
    fn is_file_too_large_true_over_cap() {
        let file = make_oversized_sparse_file();
        assert!(is_file_too_large(file.path(), "oversized.txt"));
    }

    #[test]
    fn is_file_too_large_false_under_cap() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"small file content").unwrap();
        assert!(!is_file_too_large(file.path(), "small.txt"));
    }

    // A sparse file reads back as valid UTF-8 zeros, so it would be indexed if the size gate didn't
    // short-circuit before the read.
    #[test]
    fn process_text_file_oversized_is_skipped_before_read() {
        let db = open_db();
        let dir = tempfile::tempdir().unwrap();
        // `.rs`, not `.txt`: is_binary_file sniffs text/markdown only and would flag the zeros first.
        let path = dir.path().join("huge.rs");
        {
            let f = std::fs::File::create(&path).unwrap();
            f.set_len(MAX_FILE_BYTES + 1).unwrap();
        }
        let args = default_args(dir.path().to_path_buf());
        let mut acc = ParseAcc {
            indexed: 0,
            skipped: 0,
            reextract_edges: false,
        };

        let path_str = "huge.rs";
        let result = process_text_file(&path, path_str, &db, &args, &mut acc);

        assert!(result.is_ok(), "oversized file must be skipped, not error");
        assert_eq!(acc.indexed, 0, "oversized file must not be indexed");
        assert_eq!(acc.skipped, 1, "oversized file must be counted as skipped");
        assert!(
            db.file_hash(path_str).unwrap().is_none(),
            "oversized file must never reach upsert_file — proves the read never happened"
        );
    }

    #[test]
    fn process_text_file_at_cap_boundary_is_not_skipped_by_size_gate() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(MAX_FILE_BYTES).unwrap();
        assert!(!is_file_too_large(file.path(), "boundary.bin"));
    }

    use inkentry_core::indexer::filter::IndexFilter;

    fn collected_names(files: &[ignore::DirEntry]) -> Vec<String> {
        files
            .iter()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn collect_files_excludes_junk_with_correct_count() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(dir.path().join("package-lock.json"), "{}\n").unwrap();
        std::fs::write(dir.path().join("app.min.js"), "var x=1;\n").unwrap();
        std::fs::write(dir.path().join("user.pb.go"), "package x\n").unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules/index.js"), "var y=2;\n").unwrap();

        let filter = IndexFilter::build(&[], true, true).unwrap();
        let (files, filtered) = collect_files(dir.path(), &filter).unwrap();
        let names = collected_names(&files);

        assert!(names.contains(&"lib.rs".to_string()));
        assert!(names.contains(&"package.json".to_string()));
        assert!(!names.contains(&"package-lock.json".to_string()));
        assert!(!names.contains(&"app.min.js".to_string()));
        assert!(!names.contains(&"user.pb.go".to_string()));
        assert!(!names.contains(&"index.js".to_string()));
        assert_eq!(filtered, 3);
    }

    #[test]
    fn collect_files_keeps_spec_survivors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/i18n")).unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(dir.path().join("tsconfig.json"), "{}\n").unwrap();
        std::fs::write(dir.path().join("README.md"), "# hi\n").unwrap();
        std::fs::write(dir.path().join("tests/foo_test.rs"), "fn t() {}\n").unwrap();
        std::fs::write(dir.path().join("src/i18n/index.ts"), "export const x=1;\n").unwrap();

        let filter = IndexFilter::build(&[], true, true).unwrap();
        let (files, filtered) = collect_files(dir.path(), &filter).unwrap();
        let names = collected_names(&files);

        for expected in [
            "lib.rs",
            "package.json",
            "tsconfig.json",
            "README.md",
            "foo_test.rs",
            "index.ts",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "{expected} must survive"
            );
        }
        assert_eq!(filtered, 0);
    }

    #[test]
    fn collect_files_generated_marker_toggle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("normal.rs"), "fn a() {}\n").unwrap();
        std::fs::write(
            dir.path().join("gen.rs"),
            "// Code generated by tool. DO NOT EDIT.\nfn a() {}\n",
        )
        .unwrap();

        let on = IndexFilter::build(&[], true, true).unwrap();
        let (files, filtered) = collect_files(dir.path(), &on).unwrap();
        let names = collected_names(&files);
        assert!(names.contains(&"normal.rs".to_string()));
        assert!(!names.contains(&"gen.rs".to_string()));
        assert_eq!(filtered, 1);

        let off = IndexFilter::build(&[], true, false).unwrap();
        let (files_off, filtered_off) = collect_files(dir.path(), &off).unwrap();
        assert!(collected_names(&files_off).contains(&"gen.rs".to_string()));
        assert_eq!(filtered_off, 0);
    }

    #[test]
    fn sensitive_env_not_reincludable_via_index_exclude() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=1\n").unwrap();

        let filter = IndexFilter::build(&["!.env".to_string()], true, true).unwrap();
        let (files, _filtered) = collect_files(dir.path(), &filter).unwrap();
        let names = collected_names(&files);

        assert!(
            names.contains(&"keep.rs".to_string()),
            "normal file collected"
        );
        assert!(
            !names.contains(&".env".to_string()),
            "[index].exclude=[\"!.env\"] must NOT re-include the sensitive file"
        );
    }

    #[test]
    fn reindex_with_filter_on_cleans_up_previously_indexed_junk() {
        use indicatif::MultiProgress;

        let db = open_db();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn a() {}\n").unwrap();
        std::fs::write(
            dir.path().join("app.min.js"),
            "var x=1;\nfunction f(){return x;}\n",
        )
        .unwrap();
        let args = default_args(dir.path().to_path_buf());
        let mp = MultiProgress::new();

        let mut cfg_off = crate::config::Config::default();
        cfg_off.index.use_default_excludes = false;
        cfg_off.index.detect_generated = false;
        let r1 = run_parse_phase(dir.path(), &db, &args, &mp, &cfg_off).unwrap();
        assert_eq!(r1.filtered, 0);
        let indexed_off: Vec<String> = db
            .file_paths_under("")
            .unwrap()
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert!(
            indexed_off.iter().any(|p| p == "app.min.js"),
            "junk is indexed while the filter is off"
        );

        let cfg_on = crate::config::Config::default();
        let r2 = run_parse_phase(dir.path(), &db, &args, &mp, &cfg_on).unwrap();
        assert!(r2.filtered >= 1, "app.min.js filtered on re-index");
        let indexed_on: Vec<String> = db
            .file_paths_under("")
            .unwrap()
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert!(
            !indexed_on.iter().any(|p| p == "app.min.js"),
            "excluded junk must be removed from the index on re-index"
        );
        assert!(
            indexed_on.iter().any(|p| p == "lib.rs"),
            "the real source file remains indexed"
        );
    }

    #[test]
    fn collect_files_reinclude_respects_pruned_parent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn a() {}\n").unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules/keep.js"), "var x=1;\n").unwrap();
        std::fs::create_dir(dir.path().join("vendor")).unwrap();
        std::fs::write(dir.path().join("vendor/util.rs"), "fn v() {}\n").unwrap();

        let file_reinclude =
            IndexFilter::build(&["!node_modules/keep.js".to_string()], true, true).unwrap();
        let (files, _) = collect_files(dir.path(), &file_reinclude).unwrap();
        let names = collected_names(&files);
        assert!(
            !names.contains(&"keep.js".to_string()),
            "a !file line must not re-include a file under a pruned directory"
        );
        assert!(names.contains(&"lib.rs".to_string()));

        let dir_reinclude = IndexFilter::build(&["!vendor/".to_string()], true, true).unwrap();
        let (files2, _) = collect_files(dir.path(), &dir_reinclude).unwrap();
        let names2 = collected_names(&files2);
        assert!(
            names2.contains(&"util.rs".to_string()),
            "a !dir/ line must re-include the directory's contents"
        );
    }

    #[test]
    fn collect_files_pruned_dir_contents_never_counted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("package-lock.json"), "{}\n").unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/react/lib")).unwrap();
        std::fs::write(dir.path().join("node_modules/a.js"), "1\n").unwrap();
        std::fs::write(dir.path().join("node_modules/b.js"), "2\n").unwrap();
        std::fs::write(dir.path().join("node_modules/react/index.js"), "3\n").unwrap();
        std::fs::write(dir.path().join("node_modules/react/lib/c.js"), "4\n").unwrap();

        let filter = IndexFilter::build(&[], true, true).unwrap();
        let (files, filtered) = collect_files(dir.path(), &filter).unwrap();

        assert!(collected_names(&files).contains(&"lib.rs".to_string()));
        assert_eq!(
            filtered, 1,
            "only the reachable file-level exclude is counted; the 4 files under \
             the pruned node_modules/ are never descended into, so never counted"
        );
    }

    #[test]
    fn collect_files_reincluded_file_exempt_from_marker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gen.js"),
            "// @generated\nfunction f(){return 1;}\n",
        )
        .unwrap();

        let plain = IndexFilter::build(&[], true, true).unwrap();
        let (files, filtered) = collect_files(dir.path(), &plain).unwrap();
        assert!(!collected_names(&files).contains(&"gen.js".to_string()));
        assert_eq!(filtered, 1);

        let reincluded = IndexFilter::build(&["!gen.js".to_string()], true, true).unwrap();
        let (files2, filtered2) = collect_files(dir.path(), &reincluded).unwrap();
        assert!(
            collected_names(&files2).contains(&"gen.js".to_string()),
            "a !re-included file must be exempt from generated-marker detection"
        );
        assert_eq!(filtered2, 0);
    }

    #[test]
    fn sensitive_key_material_never_reincludable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("server.pem"), "-----BEGIN-----\n").unwrap();
        std::fs::write(dir.path().join("id_rsa"), "-----BEGIN-----\n").unwrap();
        std::fs::write(dir.path().join("tls.key"), "-----BEGIN-----\n").unwrap();

        let filter = IndexFilter::build(
            &[
                "!server.pem".to_string(),
                "!id_rsa".to_string(),
                "!tls.key".to_string(),
            ],
            false,
            false,
        )
        .unwrap();
        let (files, _) = collect_files(dir.path(), &filter).unwrap();
        let names = collected_names(&files);

        assert!(names.contains(&"keep.rs".to_string()));
        for secret in ["server.pem", "id_rsa", "tls.key"] {
            assert!(
                !names.contains(&secret.to_string()),
                "sensitive file {secret} must stay excluded regardless of [index] config"
            );
        }
    }

    #[test]
    fn reindex_with_filter_on_cleans_up_all_tables() {
        use indicatif::MultiProgress;

        let db = open_db();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn real() {}\n").unwrap();
        // Named functions give it graph edges; the default `*.min.js` glob drops it.
        std::fs::write(
            dir.path().join("app.min.js"),
            "function junk(){return 1;}\nfunction more(){return junk();}\n",
        )
        .unwrap();
        let args = default_args(dir.path().to_path_buf());
        let mp = MultiProgress::new();

        let mut cfg_off = crate::config::Config::default();
        cfg_off.index.use_default_excludes = false;
        cfg_off.index.detect_generated = false;
        run_parse_phase(dir.path(), &db, &args, &mp, &cfg_off).unwrap();

        let junk_chunks = db.chunks_for_file("app.min.js").unwrap();
        assert!(
            !junk_chunks.is_empty(),
            "junk must have chunk rows while the filter is off"
        );
        assert!(
            !db.edges_for_file("app.min.js").unwrap().is_empty(),
            "junk must have graph/mention edge rows while the filter is off"
        );

        for c in db.chunks_for_file("app.min.js").unwrap() {
            db.insert_embedding(
                c.chunk_id,
                &vec![0.1f32; inkentry_core::embeddings::EMBEDDING_DIM],
            )
            .unwrap();
        }
        for c in db.chunks_for_file("lib.rs").unwrap() {
            db.insert_embedding(
                c.chunk_id,
                &vec![0.2f32; inkentry_core::embeddings::EMBEDDING_DIM],
            )
            .unwrap();
        }
        let embeddings_before = db.stats().unwrap().embedding_count;
        assert_eq!(
            embeddings_before as usize,
            db.chunks_for_file("app.min.js").unwrap().len()
                + db.chunks_for_file("lib.rs").unwrap().len(),
            "both files' chunks are embedded before cleanup"
        );

        let cfg_on = crate::config::Config::default();
        let r2 = run_parse_phase(dir.path(), &db, &args, &mp, &cfg_on).unwrap();
        assert!(r2.filtered >= 1, "app.min.js is filtered on re-index");

        let files_now: Vec<String> = db
            .file_paths_under("")
            .unwrap()
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert!(!files_now.iter().any(|p| p == "app.min.js"));
        assert!(files_now.iter().any(|p| p == "lib.rs"));

        assert!(
            db.chunks_for_file("app.min.js").unwrap().is_empty(),
            "junk chunk rows must be deleted on cleanup"
        );
        let real_chunks = db.chunks_for_file("lib.rs").unwrap();
        assert!(!real_chunks.is_empty(), "real file's chunks survive");

        assert!(
            db.edges_for_file("app.min.js").unwrap().is_empty(),
            "junk graph/mention edge rows must be deleted on cleanup"
        );

        let embeddings_after = db.stats().unwrap().embedding_count;
        assert_eq!(
            embeddings_after as usize,
            real_chunks.len(),
            "the junk's embedding rows must be gone; only the real file's remain"
        );
        assert!(
            embeddings_after < embeddings_before,
            "cleanup must reduce the embedding count"
        );
    }

    #[test]
    fn filtered_notice_names_count_and_override_location() {
        let s = filtered_notice(7);
        assert!(s.contains("Filtered out 7"));
        assert!(s.contains("[index]"));
        assert!(s.contains(".inkentry/config.toml"));
    }
}
