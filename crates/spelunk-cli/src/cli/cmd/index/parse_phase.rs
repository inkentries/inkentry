use super::mentions::extract_mention_tokens;

use anyhow::Result;
use ignore::WalkBuilder;
use indicatif::{MultiProgress, ProgressBar};

use super::super::ui::{is_tty, progress_style, short_path};
use super::IndexArgs;
#[cfg(feature = "rich-formats")]
use crate::indexer::docparser::parse_doc;
use crate::{
    indexer::{
        graph::EdgeExtractor,
        parser::{
            SourceParser, detect_doc_language, detect_language, detect_text_language,
            is_binary_file,
        },
    },
    search::tokens::estimate_tokens,
    storage::Database,
};

/// Upper bound on the size of any single file read into memory during
/// indexing, checked via `metadata().len()` *before* the file is opened for
/// reading. Applied uniformly to every format (text, markdown, tree-sitter
/// source, PDF, DOCX, XLSX, …) — a single gate, not one per branch — so a
/// multi-GB file (or a compression-bomb office/PDF doc) can't be read fully
/// into memory and OOM-kill the indexer. This is distinct from (and
/// complementary to) `MAX_PARSE_BYTES` in `spelunk_core::indexer::parser`,
/// which only bounds how much of an *already-read* buffer tree-sitter will
/// attempt to GLR-parse before falling back to a sliding window.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Return `true` (and log a warning) if `path` is over `MAX_FILE_BYTES`,
/// checked via a `metadata()` call — no file content is read either way.
/// Callers must skip the file without reading it when this returns `true`.
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
    /// (chunk_id, embedding_text) pairs awaiting embedding.
    pub chunk_ids_and_texts: Vec<(i64, String)>,
    pub indexed: u64,
    pub removed: u64,
}

/// Mutable accumulators shared across per-file processor functions.
/// Bundled into one struct so processor signatures stay under 7 arguments.
struct ParseAcc {
    out: Vec<(i64, String)>,
    indexed: u64,
    skipped: u64,
}

/// Collect source files from `root`, parse them, store chunks + graph edges,
/// then remove stale index records for files that no longer exist.
pub(super) fn run_parse_phase(
    root: &std::path::Path,
    db: &Database,
    args: &IndexArgs,
    mp: &MultiProgress,
) -> Result<ParseResult> {
    let files = collect_files(root)?;

    if files.is_empty() {
        println!("No supported source files found in {}", root.display());
        return Ok(ParseResult {
            chunk_ids_and_texts: vec![],
            indexed: 0,
            removed: 0,
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
        out: Vec::new(),
        indexed: 0,
        skipped: 0,
    };

    for entry in &files {
        let path = entry.path();
        // Store paths relative to the project root so the index is portable.
        // Normalize separators to `/` so the on-disk index is identical across
        // OSes and matches forward-slash CLI/query paths (Windows `to_string_lossy`
        // would otherwise emit `src\lib.rs`).
        let rel = path.strip_prefix(root).unwrap_or(path);
        let path_str = spelunk_core::utils::normalize_index_path(&rel.to_string_lossy());
        parse_bar.set_message(short_path(&path_str));

        // ── Binary document formats (DOCX, XLSX, PDF, …) ─────────────────────
        #[cfg(feature = "rich-formats")]
        if let Some(doc_lang) = detect_doc_language(path)
            && process_doc_file(path, &path_str, doc_lang, db, args, &mut acc)?
        {
            parse_bar.inc(1);
            continue;
        }

        // ── PDF documents (feature-gated) ─────────────────────────────────────
        #[cfg(feature = "rich-formats")]
        if detect_language(path) == Some("pdf")
            && process_pdf_file(path, &path_str, db, args, &mut acc)?
        {
            parse_bar.inc(1);
            continue;
        }

        // ── Text / code formats ───────────────────────────────────────────────
        process_text_file(path, &path_str, db, args, &mut acc)?;
        parse_bar.inc(1);
    }

    parse_bar.finish_with_message(format!(
        "{} files parsed ({} skipped, {} new/changed)",
        acc.indexed, acc.skipped, acc.indexed
    ));

    let removed = cleanup_stale(&files, root, db)?;
    let ParseAcc {
        out: mut chunk_ids_and_texts,
        indexed,
        ..
    } = acc;

    // Backfill: pick up any chunks that exist in the index but have no
    // embedding row yet (e.g. a prior `init`/`index` parsed & chunked while
    // the embedder was still loading, so the embed phase was skipped). These
    // belong to unchanged files that the hash-based skip above never re-emits,
    // so without this union a plain `spelunk index` would report "nothing to
    // do" and leave them permanently unembedded (spelunk-oss^72).
    //
    // Freshly-parsed chunks from this run also lack an embedding row, so they
    // appear here too; dedupe against the ids we already queued to avoid
    // embedding them twice.
    let already: std::collections::HashSet<i64> =
        chunk_ids_and_texts.iter().map(|(id, _)| *id).collect();
    for (chunk_id, name, metadata, summary, content) in db.chunks_missing_embeddings()? {
        if already.contains(&chunk_id) {
            continue;
        }
        let text =
            reconstruct_embedding_text(name.as_deref(), metadata.as_deref(), summary, content);
        chunk_ids_and_texts.push((chunk_id, text));
    }

    Ok(ParseResult {
        chunk_ids_and_texts,
        indexed,
        removed,
    })
}

/// Build the `(chunk_id, embedding_text)` list for every chunk in the index
/// that has no embedding row yet, reconstructing each chunk's document text
/// from its stored columns. This is the same union `run_parse_phase` applies as
/// a backfill (spelunk-oss^72); exposed separately so a detached embed-only
/// subprocess can rebuild the embed queue straight from the DB without
/// re-parsing (spelunk-oss^74).
pub(super) fn missing_embedding_texts(db: &Database) -> Result<Vec<(i64, String)>> {
    let mut out = Vec::new();
    for (chunk_id, name, metadata, summary, content) in db.chunks_missing_embeddings()? {
        let text =
            reconstruct_embedding_text(name.as_deref(), metadata.as_deref(), summary, content);
        out.push((chunk_id, text));
    }
    Ok(out)
}

/// Rebuild the exact document text that `Chunk::embedding_text()` produces,
/// from the columns stored for a chunk. The `docstring` lives inside the
/// `metadata` JSON (`{ "docstring": ..., "parent_scope": ... }`), mirroring how
/// `store_chunks` persists it. Keep this in lockstep with
/// `spelunk_core::indexer::Chunk::embedding_text`.
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

// ── File collection ───────────────────────────────────────────────────────────

fn collect_files(root: &std::path::Path) -> Result<Vec<ignore::DirEntry>> {
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
    walk.add_custom_ignore_filename(".spelunkignore");
    let mut ob = ignore::overrides::OverrideBuilder::new(root);
    ob.case_insensitive(true).ok();
    for pat in &sensitive_patterns {
        ob.add(pat).ok();
    }
    if let Ok(ov) = ob.build() {
        walk.overrides(ov);
    }

    Ok(walk
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_file()).unwrap_or(false))
        .filter(|e| {
            let p = e.path();
            detect_language(p).is_some()
                || detect_text_language(p).is_some()
                || detect_doc_language(p).is_some()
        })
        .collect())
}

// ── Per-file processors ───────────────────────────────────────────────────────

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
    let file_id = db.upsert_file(path_str, Some(doc_lang), &hash)?;
    db.delete_embeddings_for_file(file_id)?;
    db.delete_chunks_for_file(file_id)?;
    store_chunks(&chunks, path_str, file_id, db, acc)?;
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
            let file_id = db.upsert_file(path_str, Some("pdf"), &hash)?;
            db.delete_embeddings_for_file(file_id)?;
            db.delete_chunks_for_file(file_id)?;
            let chunks = pages_to_chunks(pages, path_str);
            store_chunks(&chunks, path_str, file_id, db, acc)?;
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

    // Skip binary files (e.g. compiled output with wrong extension)
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
    {
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

    let file_id = db.upsert_file(path_str, Some(language), &hash)?;
    db.delete_embeddings_for_file(file_id)?;
    db.delete_chunks_for_file(file_id)?;

    // Extract and store graph edges for this file (structural: calls/imports/extends).
    match EdgeExtractor::extract(&source, path_str, language) {
        Ok(edges) => {
            if let Err(e) = db.replace_edges(path_str, &edges) {
                tracing::warn!("graph edge storage failed for {path_str}: {e}");
            }
        }
        Err(e) => tracing::warn!("graph extraction failed for {path_str}: {e}"),
    }

    // Store mention edges (broader than calls — used by LinearRAG C matrix).
    // replace_edges already cleared the file's edges, so we just append here.
    let mention_owned: Vec<(Option<String>, String)> = chunks
        .iter()
        .filter(|c| c.name.is_some())
        .flat_map(|c| {
            let name = c.name.clone().unwrap();
            extract_mention_tokens(&c.content, language)
                .into_iter()
                .map(move |sym| (Some(name.clone()), sym))
        })
        .collect();
    let mention_refs: Vec<(Option<&str>, &str)> = mention_owned
        .iter()
        .map(|(n, s)| (n.as_deref(), s.as_str()))
        .collect();
    if !mention_refs.is_empty()
        && let Err(e) = db.append_mention_edges(path_str, &mention_refs)
    {
        tracing::warn!("mention edge storage failed for {path_str}: {e}");
    }

    store_chunks(&chunks, path_str, file_id, db, acc)?;
    acc.indexed += 1;
    Ok(())
}

/// Insert a slice of parsed chunks into the DB and record their embedding texts.
fn store_chunks(
    chunks: &[crate::indexer::Chunk],
    path_str: &str,
    file_id: i64,
    db: &Database,
    acc: &mut ParseAcc,
) -> Result<()> {
    for chunk in chunks {
        // Scan the full text that will be persisted/embedded (docstring + content;
        // `chunk.summary` is always `None` at this point, so `embedding_text()`
        // here is exactly docstring+content). Dropping the chunk here — before the
        // metadata JSON is built — ensures a secret in the docstring never lands
        // in stored metadata either. See secrets.rs module doc: this is
        // best-effort defense-in-depth, not a security boundary.
        if crate::indexer::secrets::contains_secret(&chunk.embedding_text()) {
            tracing::warn!(
                "skipping chunk '{}' in {path_str} (possible secret detected)",
                chunk.name.as_deref().unwrap_or("<anonymous>"),
            );
            continue;
        }
        let metadata =
            serde_json::json!({ "docstring": chunk.docstring, "parent_scope": chunk.parent_scope });
        let tc = estimate_tokens(&chunk.content);
        let chunk_id = db.insert_chunk(
            file_id,
            &chunk.kind.to_string(),
            chunk.name.as_deref(),
            chunk.start_line,
            chunk.end_line,
            &chunk.content,
            Some(&metadata.to_string()),
            tc,
        )?;
        acc.out.push((chunk_id, chunk.embedding_text()));
    }
    Ok(())
}

// ── Stale file cleanup ────────────────────────────────────────────────────────

fn cleanup_stale(files: &[ignore::DirEntry], root: &std::path::Path, db: &Database) -> Result<u64> {
    // Paths in the DB are root-relative, so visited uses the same relative form.
    let visited: std::collections::HashSet<String> = files
        .iter()
        .map(|e| {
            let p = e.path();
            // Match the normalized form stored during indexing (forward slashes).
            spelunk_core::utils::normalize_index_path(
                &p.strip_prefix(root).unwrap_or(p).to_string_lossy(),
            )
        })
        .collect();
    // Pass "" so file_paths_under returns all files in this DB (paths are relative).
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

    /// Register the sqlite-vec extension exactly once per test process.
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
            summary_batch_size: 10,
            background_phases: false,
            embed_phases: false,
            detach: false,
            detach_embed: false,
        }
    }

    /// A sparse file whose reported length is over `MAX_FILE_BYTES`, created
    /// via `set_len` so no actual bytes are written/allocated on disk — the
    /// test itself must not read megabytes of data to prove the cap works.
    fn make_oversized_sparse_file() -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("create temp file");
        file.as_file()
            .set_len(MAX_FILE_BYTES + 1)
            .expect("set_len on temp file");
        file
    }

    // ── reconstruct_embedding_text mirrors Chunk::embedding_text ─────────────

    /// The DB-side reconstruction used to backfill unembedded chunks must
    /// produce byte-for-byte the same document text as `Chunk::embedding_text()`
    /// did at store time, so a backfilled embedding is identical to one written
    /// during a normal parse (spelunk-oss^72). Covers: name present/absent and
    /// docstring present/absent (summary is always None at store time).
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
            // Metadata JSON exactly as store_chunks persists it.
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

    /// The summary branch of `reconstruct_embedding_text` must also match
    /// `Chunk::embedding_text()`. Phase-4 LLM summaries can be written to a
    /// chunk (`chunks.summary`) before a later re-index backfills its embedding,
    /// so the backfill path reconstructs with a non-null `summary` and must
    /// produce the exact `title: {name} | summary: {summary} | text: {body}`
    /// document (spelunk-oss^72). Covers summary × docstring present/absent.
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
            // Metadata JSON exactly as store_chunks persists it (docstring lives
            // in metadata; the summary is a separate stored column).
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

    // ── End-to-end backfill: parse-only run leaves chunks unembedded, a
    //    second parse run backfills them without reparsing (spelunk-oss^72) ────

    /// `run_parse_phase` stores chunks but never writes embeddings — that is the
    /// embed phase's job. So a single parse run models the real bug: an
    /// `init`/`index` that chunked while the embedder was still loading, leaving
    /// the `embeddings` table empty. This test drives the full parse path over a
    /// real fixture repo twice (no `--force`) and asserts:
    ///   (a) after run 1, every stored chunk is unembedded (embeddings empty);
    ///   (b) run 2 reparses nothing (`indexed == 0`, all files hash-skipped);
    ///   (c) yet run 2 still returns a NON-EMPTY `chunk_ids_and_texts` — the
    ///       missing-embedding chunks are unioned in for the embed phase;
    ///   (d) the backfilled ids are exactly the chunk ids stored in run 1
    ///       (same ids ⇒ no delete+reinsert ⇒ no unchanged file was reparsed).
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

        // ── Run 1: parse + store chunks. No embeddings are ever written here. ──
        let first = run_parse_phase(dir.path(), &db, &args, &mp).expect("first parse phase");
        assert!(
            first.indexed >= 2,
            "both fixture files must be indexed on the first run"
        );
        assert!(
            !first.chunk_ids_and_texts.is_empty(),
            "the first run must queue freshly-parsed chunks for embedding"
        );

        // Every chunk stored in run 1 is currently unembedded (embeddings empty):
        // the set of missing-embedding chunk ids must equal the run-1 queued ids.
        let mut queued_run1: Vec<i64> = first
            .chunk_ids_and_texts
            .iter()
            .map(|(id, _)| *id)
            .collect();
        queued_run1.sort();
        let mut missing_after_run1: Vec<i64> = db
            .chunks_missing_embeddings()
            .expect("missing after run 1")
            .into_iter()
            .map(|(id, ..)| id)
            .collect();
        missing_after_run1.sort();
        assert_eq!(
            missing_after_run1, queued_run1,
            "after a parse-only run the embeddings table is empty — every stored chunk is missing its embedding"
        );

        // ── Run 2: no file changed, so nothing is reparsed. The backfill union
        //    must still surface the unembedded chunks for the embed phase. ──────
        let second = run_parse_phase(dir.path(), &db, &args, &mp).expect("second parse phase");
        assert_eq!(
            second.indexed, 0,
            "no file changed — the hash-based skip must reparse nothing on the second run"
        );
        assert!(
            !second.chunk_ids_and_texts.is_empty(),
            "the fix must union the missing-embedding chunks into the embed batch even though indexed == 0"
        );

        // (d) The backfilled ids are exactly the run-1 chunk ids: identical ids
        // prove the chunks were NOT deleted and reinserted (a reparse would mint
        // fresh rowids), i.e. no unchanged file was reparsed — only its missing
        // embeddings were queued.
        let mut backfilled: Vec<i64> = second
            .chunk_ids_and_texts
            .iter()
            .map(|(id, _)| *id)
            .collect();
        backfilled.sort();
        assert_eq!(
            backfilled, queued_run1,
            "backfill must queue the same chunk ids stored in run 1 (no reparse / re-chunk)"
        );

        // The reconstructed embedding texts must also be byte-identical to what
        // the first (parse-time) run produced for those same chunks.
        let mut texts_run1: Vec<(i64, String)> = first.chunk_ids_and_texts.clone();
        texts_run1.sort_by_key(|(id, _)| *id);
        let mut texts_run2: Vec<(i64, String)> = second.chunk_ids_and_texts.clone();
        texts_run2.sort_by_key(|(id, _)| *id);
        assert_eq!(
            texts_run2, texts_run1,
            "backfilled embedding text must match the parse-time embedding text byte-for-byte"
        );
    }

    // ── missing_embedding_texts: detached embed-only queue reconstruction ──────
    // (spelunk-oss^74)

    /// The detached `--_embed-phases` subprocess rebuilds its embed queue purely
    /// from the DB via `missing_embedding_texts()` — it never re-parses. This test
    /// seeds chunks, embeds a subset directly, and proves the function returns
    /// exactly the un-embedded chunks (skipping embedded ones), in id order, with
    /// each text reconstructed byte-for-byte to `Chunk::embedding_text()`. If this
    /// diverged, the detached run would either re-embed already-done chunks or
    /// embed the wrong text.
    #[test]
    fn missing_embedding_texts_returns_only_unembedded_chunks_from_db() {
        use crate::indexer::{Chunk, ChunkKind};

        let db = open_db();
        let file_id = db.upsert_file("src/lib.rs", Some("rust"), "hash0").unwrap();

        // Store three chunks the way `store_chunks` does (docstring lives in the
        // metadata JSON), so the reconstructed text is comparable to the
        // parse-time `embedding_text()`.
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

        // Embed only the middle chunk (`beta`), leaving `alpha` and `gamma`
        // missing their embedding rows.
        let (beta_id, _) = &ids[1];
        db.insert_embedding(
            *beta_id,
            &vec![0.1f32; spelunk_core::embeddings::EMBEDDING_DIM],
        )
        .unwrap();

        let missing = missing_embedding_texts(&db).expect("missing_embedding_texts");

        // Exactly the two un-embedded chunks, in ascending id order, and NOT the
        // embedded one.
        let got_ids: Vec<i64> = missing.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            got_ids,
            vec![ids[0].0, ids[2].0],
            "only the un-embedded chunks (alpha, gamma) must be queued, in id order"
        );
        assert!(
            !got_ids.contains(beta_id),
            "the already-embedded chunk must not be re-queued"
        );

        // Each queued text is reconstructed byte-for-byte to the parse-time
        // `embedding_text()` for that chunk.
        for (queued_id, queued_text) in &missing {
            let (_, chunk) = ids.iter().find(|(id, _)| id == queued_id).unwrap();
            assert_eq!(
                queued_text,
                &chunk.embedding_text(),
                "queued text must match Chunk::embedding_text for chunk {queued_id}"
            );
        }
    }

    /// When every chunk already has an embedding, the detached embed queue must
    /// be empty — the subprocess then does no embed work (guards against the
    /// missing-embedding query over-matching).
    #[test]
    fn missing_embedding_texts_is_empty_when_all_embedded() {
        let db = open_db();
        let file_id = db.upsert_file("src/lib.rs", Some("rust"), "hash0").unwrap();
        let id = db
            .insert_chunk(file_id, "function", Some("f"), 1, 2, "fn f() {}", None, 1)
            .unwrap();
        db.insert_embedding(id, &vec![0.1f32; spelunk_core::embeddings::EMBEDDING_DIM])
            .unwrap();

        assert!(
            missing_embedding_texts(&db).unwrap().is_empty(),
            "a fully-embedded index yields an empty detached embed queue"
        );
    }

    // ── is_file_too_large ────────────────────────────────────────────────────

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

    // ── process_text_file: oversized files are skipped before any read ──────

    /// An oversized text file must be skipped without ever being read into
    /// memory. We assert this indirectly but strongly: `process_text_file`
    /// only calls `db.upsert_file` (recording a content hash) *after*
    /// `std::fs::read_to_string` succeeds. If the size gate didn't
    /// short-circuit before the read, the file would still get indexed (a
    /// sparse file reads back as all-zero bytes, which is valid UTF-8) and
    /// `db.file_hash` would return `Some(..)`. Asserting it stays `None`
    /// proves the read (and everything downstream of it) never happened —
    /// not just that some later step errored out.
    #[test]
    fn process_text_file_oversized_is_skipped_before_read() {
        let db = open_db();
        let dir = tempfile::tempdir().unwrap();
        // `.rs` (tree-sitter language) rather than `.txt`, so the unrelated
        // is_binary_file() sniff (which only applies to "text"/"markdown"
        // languages) doesn't short-circuit before we reach the size gate —
        // a sparse file reads back as all-zero bytes, which is_binary_file
        // would otherwise flag as binary regardless of the size cap.
        let path = dir.path().join("huge.rs");
        {
            let f = std::fs::File::create(&path).unwrap();
            f.set_len(MAX_FILE_BYTES + 1).unwrap();
        }
        let args = default_args(dir.path().to_path_buf());
        let mut acc = ParseAcc {
            out: Vec::new(),
            indexed: 0,
            skipped: 0,
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

    /// A file just at the cap boundary is allowed through to the normal read
    /// path (sanity check that the gate uses `>`, not `>=`, matching the doc
    /// comment "over the size cap").
    #[test]
    fn process_text_file_at_cap_boundary_is_not_skipped_by_size_gate() {
        let file = tempfile::NamedTempFile::new().unwrap();
        // Exactly at the cap: must NOT be flagged as too large.
        file.as_file().set_len(MAX_FILE_BYTES).unwrap();
        assert!(!is_file_too_large(file.path(), "boundary.bin"));
    }
}
