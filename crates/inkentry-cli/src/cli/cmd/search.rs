use anyhow::Result;
use clap::Args;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct SearchArgs {
    /// Natural language search query
    pub query: String,

    /// Number of results to return (max 100)
    #[arg(short, long, default_value = "10", conflicts_with = "budget")]
    pub limit: usize,

    /// Return best results fitting within this token budget (mutually exclusive with --limit)
    #[arg(long, conflicts_with = "limit")]
    pub budget: Option<usize>,

    /// Output format: text, json, or jsonl
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Enrich results with 1-hop call-graph neighbours (callers + callees)
    #[arg(short, long)]
    pub graph: bool,

    /// Maximum number of graph-expanded results to add (when --graph is set)
    #[arg(long, default_value = "10")]
    pub graph_limit: usize,

    /// Path to the SQLite database (overrides config)
    #[arg(short, long)]
    pub db: Option<PathBuf>,

    /// Skip the lightweight staleness probe (suppress stale-index warning)
    #[arg(long)]
    pub no_stale_check: bool,

    /// Search only the primary project index, skipping all linked project DBs
    #[arg(long)]
    pub local_only: bool,

    /// Search only the code corpus (skip memory entries). The escape hatch when
    /// the interleaved memory results are unwanted.
    #[arg(long, conflicts_with = "only_memory")]
    pub only_code: bool,

    /// Search only the memory corpus (skip code chunks)
    #[arg(long)]
    pub only_memory: bool,

    /// Full-text search only: no query embedding and no inference server needed
    #[arg(long)]
    pub only_text: bool,

    /// Restrict memory results to those valid at this instant (ISO 8601, e.g. 2026-03-15).
    /// Applies to the memory corpus only; code chunks have no temporal dimension.
    #[arg(long, value_name = "DATE")]
    pub as_of: Option<String>,

    /// Expand memory results by 1 hop along relates_to edges
    #[arg(long)]
    pub expand_graph: bool,
}

use super::color::cprintln;
use super::fusion::{self, UnifiedResult};
use super::helpers::{embed_query, project_display_name, require_server_client};
use super::ui::spinner;
use crate::{
    capability,
    config::Config,
    registry::{Project, resolve_project_context},
    search::{SearchResult, rag},
    storage::{Database, MemoryStore},
};

/// The QA instruction prefix the memory corpus was embedded to match, applied to
/// the memory query embed. The code corpus uses the code-search prefix, applied
/// server-side by the `/search` endpoint (`search_query`). Embedding the one
/// query under both prefixes is what ADR-081 fuses.
const MEMORY_QA_TASK: &str = "Given a question, retrieve passages that answer the question";

pub async fn search(args: SearchArgs, cfg: Config) -> Result<()> {
    // `--only-code` / `--only-memory` are mutually exclusive (clap enforces it);
    // absent both, search spans both corpora.
    let want_code = !args.only_memory;
    let want_memory = !args.only_code;

    // An uninitialised directory funnels here: `resolve_project_and_deps` errors
    // with the "run `inkentry init`" message. A code search requires the index.db
    // file; a `--only-memory` search only needs the project's memory.db (the
    // index.db path still resolves the `.inkentry/` project and keys the memory
    // cross-project registry lookup, but the file need not exist yet).
    let (db_path, dep_projects) = resolve_project_and_deps(args.db.as_ref(), &cfg, want_code)?;
    crate::storage::record_usage_at(&db_path, "search");

    let as_of = crate::utils::dates::parse_as_of(args.as_of.as_deref())?;
    let mem_path = db_path.with_file_name("memory.db");

    // Read the index's rebuild state before anything else opens it, so a run
    // that rebuilds says so here and every later run can tell an emptied index
    // from one that was never built. Guarded on the file existing because
    // `Database::open` would otherwise create the very index a `--only-memory`
    // search is entitled not to have.
    let mut rebuilt_unpopulated: Option<i32> = None;
    if db_path.exists()
        && let Ok(db) = Database::open(&db_path)
    {
        super::helpers::announce_index_rebuild(&db);
        rebuilt_unpopulated = db.unpopulated_since_rebuild().unwrap_or(None);
    }

    // Fill in server_url/project_id from the auto-discovered loopback tier so the
    // inference client can be built. `get_inference_tier` (not `get_tier`):
    // local_first prefers the local embedder even with an explicit server_url.
    let project_root = db_path.parent().unwrap_or(&db_path).to_path_buf();
    let tier = capability::get_inference_tier(&cfg).await;
    let cfg = tier.effective_config(&cfg, &project_root);

    let dep_projects = if args.local_only {
        vec![]
    } else {
        dep_projects
    };

    if !args.no_stale_check {
        maybe_warn_stale(&db_path);
    }

    // Over-fetch each corpus so cross-corpus fusion has enough candidates.
    let fetch_limit = if let Some(budget) = args.budget {
        (budget / 50).clamp(20, 100)
    } else {
        args.limit.min(100)
    };

    // ── Query embeds, second elided when a corpus filter makes it redundant ────
    // `--only-text` issues zero embeds; `--only-code` embeds only the code prefix;
    // `--only-memory` only the QA prefix; the default issues both.
    let mut code_vec: Option<Vec<f32>> = None;
    let mut qa_blob: Option<Vec<u8>> = None;
    let mut embed_degraded = false;

    if !args.only_text {
        match require_server_client(&cfg, "search").ok() {
            Some(client) => {
                let sp = spinner("Embedding query…");
                if want_code {
                    match client
                        .search_query(&args.query, "hybrid", fetch_limit)
                        .await
                    {
                        Ok(Some(v)) => code_vec = Some(v),
                        _ => embed_degraded = true,
                    }
                }
                if want_memory {
                    match embed_query(&client, MEMORY_QA_TASK, &args.query).await {
                        Ok(b) => qa_blob = Some(b),
                        Err(_) => embed_degraded = true,
                    }
                }
                sp.finish_and_clear();
            }
            None => embed_degraded = true,
        }
    }

    if embed_degraded {
        eprint_semantic_unavailable_notice(&tier, &cfg);
    }

    // ── Code-corpus coverage & freshness notices (stderr keeps stdout clean) ───
    let mut code_coverage: Option<(i64, i64)> = None;
    let mut code_refresh_pending: i64 = 0;
    if want_code && !args.only_text {
        if let Ok(db) = Database::open(&db_path) {
            if let Ok(s) = db.stats() {
                code_coverage = Some((s.embedding_count, s.chunk_count));
            }
            code_refresh_pending = db.refresh_pending_count().unwrap_or(0);
        }
        match code_coverage {
            Some((e, t)) if t > 0 && e <= 0 => eprintln!("{}", warmup_notice_zero(t)),
            Some((e, t)) if e < t => eprintln!("{}", warmup_notice_partial(e, t)),
            _ => {}
        }
        if code_refresh_pending > 0 {
            eprintln!("{}", refresh_pending_notice(code_refresh_pending));
        }
    }

    // ── Memory-corpus coverage notice (existing signal; no new subsystem) ──────
    let mut memory_missing: i64 = 0;
    if want_memory && !args.only_text {
        memory_missing = memory_missing_count(&mem_path);
        if memory_missing > 0 {
            eprintln!("{}", memory_warmup_notice(memory_missing));
        }
    }

    // ── Per-corpus retrieval → two ranked lists ───────────────────────────────
    let code_list: Vec<SearchResult> = if want_code {
        match &code_vec {
            Some(v) => {
                search_all_dbs_linearrag(&db_path, &dep_projects, &args.query, v, fetch_limit)?
            }
            None => {
                // `--only-text`, an unavailable embedder, or zero coverage: FTS
                // over the primary index covers every chunk from parse time.
                let mut list = Database::open(&db_path)
                    .and_then(|db| db.search_text(&args.query, fetch_limit))
                    .unwrap_or_default();
                annotate_specs(&mut list, &db_path);
                list
            }
        }
    } else {
        vec![]
    };

    let memory = if want_memory {
        super::memory::memory_corpus_search(
            &cfg,
            &db_path,
            &mem_path,
            &args.query,
            qa_blob.as_deref(),
            fetch_limit,
            as_of,
            args.expand_graph,
            args.local_only,
        )
        .await?
    } else {
        super::memory::MemoryCorpus {
            ranked: vec![],
            attachments: vec![],
        }
    };

    // ── Cross-corpus rank fusion (ADR-081) ────────────────────────────────────
    let fuse_cap = if args.budget.is_some() {
        fetch_limit.saturating_mul(2)
    } else {
        args.limit
    };
    let fused = fusion::fuse(code_list, memory.ranked, fuse_cap);

    // ── Unranked appendix: attachments, never fusion members ──────────────────
    // Memory attachments (relates-to neighbours, cross-project entries) join the
    // `--graph` code neighbours here rather than in `fuse`, so neither corpus
    // can put an unranked item in a ranked position (ADR-081).
    let mut appendix: Vec<UnifiedResult> = vec![];
    if args.graph
        && want_code
        && !fused.is_empty()
        && let Ok(primary_db) = Database::open(&db_path)
    {
        let seen_ids: HashSet<i64> = fused
            .iter()
            .filter_map(|u| u.code.as_ref().map(|c| c.chunk_id))
            .collect();
        let names: Vec<&str> = fused
            .iter()
            .filter_map(|u| u.code.as_ref())
            .filter_map(|c| c.name.as_deref())
            .collect();
        if !names.is_empty()
            && let Ok(neighbor_ids) = primary_db.graph_neighbor_chunks(&names)
        {
            let new_ids: Vec<i64> = neighbor_ids
                .into_iter()
                .filter(|id| !seen_ids.contains(id))
                .take(args.graph_limit)
                .collect();
            if !new_ids.is_empty()
                && let Ok(mut extra) = primary_db.chunks_by_ids(&new_ids)
            {
                for r in &mut extra {
                    r.from_graph = true;
                }
                appendix = fusion::graph_appendix(extra);
            }
        }
    }

    // Bounded like the code appendix is by --graph-limit: a store with hundreds
    // of locked cross-project entries must not swamp the ranked list it follows.
    let mut mem_attachments = memory.attachments;
    mem_attachments.truncate(args.limit);
    appendix.extend(fusion::memory_appendix(mem_attachments));

    if fused.is_empty() && appendix.is_empty() {
        return print_empty(
            &args,
            want_code,
            want_memory,
            code_coverage,
            code_refresh_pending,
            memory_missing,
            rebuilt_unpopulated,
        );
    }

    let all: Vec<UnifiedResult> = fused.into_iter().chain(appendix).collect();

    if let Some(budget) = args.budget {
        return emit_budget(&args, all, budget);
    }

    match crate::utils::effective_format(&args.format) {
        "json" => println!("{}", serde_json::to_string_pretty(&all)?),
        "jsonl" => {
            for u in &all {
                println!("{}", serde_json::to_string(u)?);
            }
        }
        _ => print_unified_text(&all),
    }

    Ok(())
}

/// Budget-aware packing over the fused, typed list. Memory items are estimated
/// from `title + body`; code items use their stored token count or a content
/// estimate. The envelope carries the same `token_budget`/`tokens_used`/
/// `tokens_remaining` frame as the non-fused path did.
fn emit_budget(args: &SearchArgs, all: Vec<UnifiedResult>, budget: usize) -> Result<()> {
    let mut remaining = budget;
    let mut packed: Vec<UnifiedResult> = Vec::new();
    for u in all {
        let tc = unified_token_estimate(&u);
        if tc <= remaining {
            remaining -= tc;
            packed.push(u);
        }
        if remaining < 10 {
            break;
        }
    }
    let tokens_used = budget - remaining;

    match crate::utils::effective_format(&args.format) {
        "json" => {
            #[derive(serde::Serialize)]
            struct BudgetResponse<'a> {
                token_budget: usize,
                tokens_used: usize,
                tokens_remaining: usize,
                results: &'a [UnifiedResult],
            }
            let resp = BudgetResponse {
                token_budget: budget,
                tokens_used,
                tokens_remaining: remaining,
                results: &packed,
            };
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
        "jsonl" => {
            for u in &packed {
                println!("{}", serde_json::to_string(u)?);
            }
        }
        _ => {
            print_unified_text(&packed);
            println!("tokens used: {tokens_used}/{budget}");
        }
    }
    Ok(())
}

fn unified_token_estimate(u: &UnifiedResult) -> usize {
    if let Some(c) = &u.code {
        if c.token_count > 0 {
            c.token_count
        } else {
            crate::search::tokens::estimate_tokens(&c.content)
        }
    } else if let Some(m) = &u.memory {
        crate::search::tokens::estimate_tokens(&format!("{}\n{}", m.title, m.body))
    } else {
        0
    }
}

/// Print the fused, heterogeneous list in fused order, each line labelled with
/// its corpus so the interleave is legible.
fn print_unified_text(results: &[UnifiedResult]) {
    for u in results {
        if let Some(c) = &u.code {
            let name = c.name.as_deref().unwrap_or("<anonymous>");
            let label = if c.from_graph {
                "code · graph"
            } else {
                "code"
            };
            let project_prefix = c
                .project_name
                .as_deref()
                .map(|p| format!("\x1b[36m[{p}]\x1b[0m "))
                .unwrap_or_default();
            cprintln!(
                "\x1b[2m[{label}]\x1b[0m {project_prefix}\x1b[1m{}\x1b[0m  \x1b[2m{}:{}-{}\x1b[0m  \x1b[33m[{}: {}]\x1b[0m",
                c.file_path,
                c.language,
                c.start_line,
                c.end_line,
                c.node_type,
                name,
            );
            if let Some(summary) = &c.summary
                && !summary.is_empty()
            {
                println!("    Summary: {summary}");
            }
            for line in c.content.lines().take(4) {
                println!("    {line}");
            }
        } else if let Some(m) = &u.memory {
            let source = m
                .source_project
                .as_deref()
                .map(|p| format!("  \x1b[36m[from: {p}]\x1b[0m"))
                .unwrap_or_default();
            let label = if u.fused_rank.is_none() {
                "memory · attached"
            } else {
                "memory"
            };
            cprintln!(
                "\x1b[2m[{label}]\x1b[0m \x1b[33m[{}]\x1b[0m #{}  \x1b[1m{}\x1b[0m{source}",
                m.kind,
                m.id,
                m.title,
            );
            for line in m.body.lines().take(2) {
                println!("    {line}");
            }
        }
        println!();
    }
}

/// The empty-result line. The bare `No results found.` is printed only when
/// every in-scope corpus was complete; otherwise the incomplete corpus and its
/// fraction are named so an absence is never mistaken for "not in the codebase".
/// json/jsonl stdout stays machine-clean (`[]` / nothing); the qualifying detail
/// already went to stderr as the coverage/freshness notices above.
fn print_empty(
    args: &SearchArgs,
    want_code: bool,
    want_memory: bool,
    code_coverage: Option<(i64, i64)>,
    code_refresh_pending: i64,
    memory_missing: i64,
    rebuilt_unpopulated: Option<i32>,
) -> Result<()> {
    match crate::utils::effective_format(&args.format) {
        "json" => {
            println!("[]");
            return Ok(());
        }
        "jsonl" => return Ok(()),
        _ => {}
    }
    println!(
        "{}",
        empty_message(
            want_code,
            want_memory,
            args.only_text,
            code_coverage,
            code_refresh_pending,
            memory_missing,
            rebuilt_unpopulated,
        )
    );
    Ok(())
}

/// Build the empty-result message: bare when complete, qualified otherwise.
///
/// `rebuilt_unpopulated` is the version a rebuild discarded while the index it
/// left behind is still empty. Unlike the coverage and freshness qualifiers it
/// holds under `--only-text` too: full-text search over an emptied index finds
/// nothing for the same reason semantic search does.
fn empty_message(
    want_code: bool,
    want_memory: bool,
    only_text: bool,
    code_coverage: Option<(i64, i64)>,
    code_refresh_pending: i64,
    memory_missing: i64,
    rebuilt_unpopulated: Option<i32>,
) -> String {
    let code_partial =
        want_code && !only_text && matches!(code_coverage, Some((e, t)) if t > 0 && e < t);
    let code_stale = want_code && !only_text && code_refresh_pending > 0;
    let memory_partial = want_memory && !only_text && memory_missing > 0;
    let rebuilt = if want_code { rebuilt_unpopulated } else { None };

    if !code_partial && !code_stale && !memory_partial && rebuilt.is_none() {
        return "No results found.".to_string();
    }

    let mut parts: Vec<String> = Vec::new();
    if let Some(found) = rebuilt {
        parts.push(format!(
            "the index was rebuilt from {} and not reindexed since, so it holds nothing; \
             run `inkentry index .`",
            super::helpers::replaced_schema(found)
        ));
    }
    if let (true, Some((e, t))) = (code_partial, code_coverage) {
        parts.push(format!("code searchable {e}/{t} chunks"));
    }
    if code_stale {
        parts.push(format!(
            "{code_refresh_pending} code chunk(s) await re-embedding, so rankings may still shift"
        ));
    }
    if memory_partial {
        parts.push(format!(
            "{memory_missing} memory entr{} not embedded yet",
            if memory_missing == 1 { "y" } else { "ies" }
        ));
    }
    format!("No results found ({}).", parts.join("; "))
}

/// Count active notes with no embedding row, best-effort, for the partial-memory
/// notice. `0` when there is no local store to read (an uninitialised path or a
/// cloud-routed store) so it never fabricates incompleteness.
fn memory_missing_count(mem_path: &std::path::Path) -> i64 {
    if !mem_path.exists() {
        return 0;
    }
    MemoryStore::open(mem_path)
        .ok()
        .and_then(|s| s.notes_missing_embeddings(false).ok())
        .map(|v| v.len() as i64)
        .unwrap_or(0)
}

/// Emit a staleness warning to stderr if the index appears out of date.
/// Silently skips if the DB doesn't exist or the probe returns an error.
pub(crate) fn maybe_warn_stale(db_path: &std::path::Path) {
    if !db_path.exists() {
        return;
    }
    // In-project probe: indexed paths are relative to the project root, which is
    // the cwd for these commands.
    let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    if let Ok(db) = Database::open(db_path)
        && let Ok(report) = db.staleness_report(&root, Some(20))
        && report.stale > 0
    {
        eprintln!(
            "warning: index may be stale ({}/{} sampled files changed). \
             Run `inkentry index .` to refresh.",
            report.stale, report.sampled
        );
    }
}

/// Resolve the primary index-db path and any dep projects via the registry.
///
/// Always fails closed (ADR-067) when there is no local `.inkentry/` project.
/// When `require_index_file` is set, it additionally errors if the index.db file
/// does not exist yet (the code-search path needs it); a `--only-memory` search
/// passes `false`, since it reads only the sibling memory.db and the index-db
/// path is used solely to resolve the project and key the registry.
pub(crate) fn resolve_project_and_deps(
    explicit_db: Option<&std::path::PathBuf>,
    cfg: &Config,
    require_index_file: bool,
) -> Result<(std::path::PathBuf, Vec<Project>)> {
    // ADR-067: without an explicit --db, refuse when there is no local
    // `.inkentry/` project rather than silently searching the global store. The
    // scoped path also wins over any stray global `index.db`.
    let project_db = match explicit_db {
        Some(_) => None,
        None => Some(crate::config::require_project_db(&cfg.db_path, false)?),
    };

    let resolved = resolve_project_context(explicit_db.map(|p| p.as_path()), &cfg.db_path)?;
    let db_path = project_db.unwrap_or(resolved.db_path);

    if require_index_file && !db_path.exists() {
        if explicit_db.is_some() {
            anyhow::bail!(
                "Database not found at '{}'. Run `inkentry index <path>` first.",
                db_path.display()
            );
        }
        anyhow::bail!(
            "No index found (checked current directory and parents).\n\
             Run `inkentry index <path>` inside your project first."
        );
    }

    Ok((db_path, resolved.deps))
}

/// Annotate results with `project_name` / `project_path` for dep results.
fn annotate_dep_results(
    results: &mut [SearchResult],
    project_name: Option<String>,
    project_path: String,
) {
    for r in results.iter_mut() {
        r.project_name = project_name.clone();
        r.project_path = Some(project_path.clone());
    }
}

/// Populate `governing_specs` on each result using the primary DB.
fn annotate_specs(all: &mut [SearchResult], primary_db_path: &std::path::Path) {
    if let Ok(primary_db) = Database::open(primary_db_path) {
        let file_paths: Vec<String> = all.iter().map(|r| r.file_path.clone()).collect();
        if let Ok(all_specs) = primary_db.specs_for_files(&file_paths)
            && !all_specs.is_empty()
        {
            for result in all.iter_mut() {
                if let Ok(per) = primary_db.specs_for_files(std::slice::from_ref(&result.file_path))
                {
                    result.governing_specs = per.into_iter().map(|(p, _)| p).collect();
                }
            }
        }
    }
}

/// LinearRAG search across a primary DB and any dep projects.
/// Runs LinearRAG on each DB independently and merges by distance, deduped.
pub(crate) fn search_all_dbs_linearrag(
    primary_db_path: &std::path::Path,
    dep_projects: &[Project],
    query: &str,
    query_vec: &[f32],
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let primary_db = Database::open(primary_db_path)?;
    let fetch = (limit * 2).max(limit + 10);
    let mut all = rag::linearrag_search(&primary_db, query_vec, query, fetch).unwrap_or_default();

    for dep in dep_projects {
        match Database::open(&dep.db_path) {
            Ok(dep_db) => match rag::linearrag_search(&dep_db, query_vec, query, fetch) {
                Ok(mut dep_results) => {
                    let name = project_display_name(&dep.root_path);
                    let root = dep.root_path.to_string_lossy().into_owned();
                    annotate_dep_results(&mut dep_results, Some(name), root);
                    all.append(&mut dep_results);
                }
                Err(e) => {
                    tracing::warn!(
                        "linearrag search failed on dep {}: {e}",
                        dep.db_path.display()
                    )
                }
            },
            Err(e) => tracing::warn!("could not open dep DB {}: {e}", dep.db_path.display()),
        }
    }

    // Sort by ascending distance (lower = better score in LinearRAG output).
    // The dedupe below keeps the first row per (path, start, end), so at equal
    // distance the tie-break picks which project's copy survives — leaving that
    // to sort order alone made a shared chunk flip owner between runs.
    all.sort_by(|a, b| {
        a.distance.total_cmp(&b.distance).then_with(|| {
            (&a.file_path, a.start_line, a.end_line, a.chunk_id).cmp(&(
                &b.file_path,
                b.start_line,
                b.end_line,
                b.chunk_id,
            ))
        })
    });
    let mut seen = std::collections::HashSet::new();
    all.retain(|r| seen.insert((r.file_path.clone(), r.start_line, r.end_line)));
    all.truncate(limit);

    annotate_specs(&mut all, primary_db_path);

    Ok(all)
}

/// One-line warmup notice for a partially-embedded code corpus: carries the
/// coverage percentage AND its shape. The queue drains in priority order
/// (`graph_rank DESC, mtime DESC` — most-referenced code first, then most
/// recently modified), so a prefix is the most important/recent code, not a
/// sample across the repo.
fn warmup_notice_partial(embedded: i64, total: i64) -> String {
    let pct = if total > 0 {
        (embedded.max(0) as u64).saturating_mul(100) / total as u64
    } else {
        0
    };
    format!(
        "[warmup: searchable {embedded}/{total} chunks ({pct}%), front-loaded by importance \
         and recency; a missing result may mean \"not embedded yet\", not \"not in the \
         codebase\" (check `inkentry status`)]"
    )
}

/// One-line freshness notice: the corpus is fully searchable (coverage), but
/// `pending` chunks have a vector whose input changed and await an in-place
/// re-embed, so rankings may still shift. "Same query, same answer" holds once
/// this reaches zero.
fn refresh_pending_notice(pending: i64) -> String {
    format!(
        "[refresh: {pending} chunk(s) awaiting re-embedding after an indexing-scheme change; \
         they still return their previous vector, so rankings may shift until the refresh \
         finishes (check `inkentry status`)]"
    )
}

/// Zero-coverage notice: the code corpus has no vectors yet, so the search runs
/// over full-text only (which covers every chunk from parse time) while
/// embeddings build in the background.
fn warmup_notice_zero(total: i64) -> String {
    format!(
        "[warmup: 0/{total} chunks embedded; using full-text search while embeddings build \
         in the background (check `inkentry status`)]"
    )
}

/// Partial-memory notice: some memory entries have no vector yet, so they are
/// reachable through memory full-text search but not the vector half.
fn memory_warmup_notice(missing: i64) -> String {
    format!(
        "[warmup: {missing} memory entr{} not yet embedded; reachable via full-text search \
         only (check `inkentry status`)]",
        if missing == 1 { "y" } else { "ies" }
    )
}

/// Build the one-line notice explaining why semantic ranking is unavailable and
/// the search fell back to full-text. Pure so it can be unit-tested without
/// capturing stderr.
///
/// The whole tier is the input rather than fields derived from it: an offline
/// tier carries the reason the probe recorded, and only that reason can say
/// which server was contacted and what would change the outcome. `server_url`
/// is `cfg.server_url`, read only where the notice names it. `is_windows` is
/// injected so the platform-gated hint stays unit-testable on any host.
///
/// Visible to `index::phases` for the cross-surface agreement test, which pins
/// this notice and the index one to the same remedy per reason.
pub(in crate::cli::cmd) fn semantic_unavailable_message(
    tier: &capability::Tier,
    server_url: Option<&str>,
    is_windows: bool,
) -> String {
    use capability::{EmbedderState, Tier};
    match tier {
        Tier::Server {
            embedder_state: EmbedderState::Loading,
            ..
        } => "[semantic ranking unavailable: model still warming up — \
             retry shortly (`inkentry server status`); using full-text search]"
            .to_string(),
        Tier::Server {
            embedder_state: EmbedderState::Unavailable,
            ..
        } => match tier.explicit_remote_url() {
            Some(url) => format!(
                "[semantic ranking unavailable: embedder failed to load on team server {url}; \
                 check that server's own logs; using full-text search]"
            ),
            None => "[semantic ranking unavailable: embedder failed to load; \
                 see `inkentry server logs`; using full-text search]"
                .to_string(),
        },
        Tier::Server { .. } => {
            "[semantic ranking unavailable on this server; using full-text search]".to_string()
        }
        Tier::Offline(reason) => offline_semantic_notice(*reason, server_url, is_windows),
    }
}

/// The offline half of [`semantic_unavailable_message`], keyed to the reason the
/// probe recorded rather than to whether a `server_url` happens to be set.
///
/// `search` runs on the inference tier, which under `local_first` is a loopback
/// probe even when `server_url` points elsewhere. Derived from the config, this
/// notice named a server the run never contacted, and reported a daemon that
/// discovery had just refused out loud as no server at all.
fn offline_semantic_notice(
    reason: capability::OfflineReason,
    server_url: Option<&str>,
    is_windows: bool,
) -> String {
    use capability::OfflineReason;
    if let Some(advice) = capability::shared_offline_advice(reason) {
        return format!("[{advice}; using full-text search]");
    }
    match reason {
        OfflineReason::ExplicitServerUnavailable => {
            let windows_hint = if is_windows {
                " On Windows, allow the loopback listener through Defender Firewall."
            } else {
                ""
            };
            let target = match server_url {
                Some(url) => format!(
                    "at {url} (the configured server_url, overriding the auto-discovered \
                     local daemon)"
                ),
                None => "at the configured server_url".to_string(),
            };
            format!("[no server reachable {target};{windows_hint} using full-text search]")
        }
        // The only other reason `shared_offline_advice` declines.
        _ => "[no server running — start one with `inkentry server start` to enable \
             semantic ranking; using full-text search]"
            .to_string(),
    }
}

/// Print the semantic-unavailable notice to stderr so structured
/// (`--format json`/`jsonl`) output on stdout stays clean.
fn eprint_semantic_unavailable_notice(tier: &capability::Tier, cfg: &Config) {
    eprintln!(
        "{}",
        semantic_unavailable_message(tier, cfg.server_url.as_deref(), cfg!(windows))
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::EmbedderState;

    // ── empty_message: the two-corpus No-results invariant ─────────────────────

    #[test]
    fn empty_message_is_bare_when_every_in_scope_corpus_is_complete() {
        // Full code coverage, fresh, no missing memory: bare line.
        let m = empty_message(true, true, false, Some((100, 100)), 0, 0, None);
        assert_eq!(m, "No results found.");
    }

    #[test]
    fn empty_message_only_text_is_always_bare() {
        // FTS covers every chunk/note, so a text-only search's absence is
        // unqualified regardless of embedding coverage.
        let m = empty_message(true, true, true, Some((0, 100)), 5, 9, None);
        assert_eq!(m, "No results found.");
    }

    #[test]
    fn empty_message_names_partial_code_coverage_and_fraction() {
        let m = empty_message(true, true, false, Some((40, 100)), 0, 0, None);
        assert!(m.starts_with("No results found ("), "qualified: {m}");
        assert!(m.contains("40/100"), "names the fraction: {m}");
        assert_ne!(m, "No results found.");
    }

    #[test]
    fn empty_message_names_pending_freshness_so_it_is_not_unqualified() {
        // Coverage 100% but a refresh is draining: never a bare absence.
        let m = empty_message(true, true, false, Some((100, 100)), 3, 0, None);
        assert_ne!(m, "No results found.");
        assert!(m.contains("re-embedding"), "names refinement-pending: {m}");
        assert!(m.contains("rankings may still shift"), "{m}");
    }

    #[test]
    fn empty_message_names_partial_memory() {
        let m = empty_message(true, true, false, Some((100, 100)), 0, 2, None);
        assert_ne!(m, "No results found.");
        assert!(m.contains("2 memory entries not embedded yet"), "{m}");
    }

    #[test]
    fn empty_message_ignores_out_of_scope_corpus_incompleteness() {
        // --only-code: memory incompleteness is out of scope, so a full code
        // corpus still yields the bare line.
        let m = empty_message(true, false, false, Some((100, 100)), 0, 7, None);
        assert_eq!(m, "No results found.");
        // --only-memory: partial code coverage is out of scope.
        let m = empty_message(false, true, false, Some((0, 100)), 4, 0, None);
        assert_eq!(m, "No results found.");
    }

    // ── empty_message: rebuilt-and-empty vs never-indexed ──────────────────────

    #[test]
    fn empty_message_names_a_rebuilt_index_that_was_never_repopulated() {
        // A rebuilt index reads as complete on every other signal: zero chunks
        // means zero missing embeddings and nothing pending, so the coverage
        // qualifiers stay silent and the line was bare.
        let m = empty_message(true, true, false, Some((0, 0)), 0, 0, Some(15));
        assert_ne!(m, "No results found.");
        assert!(m.contains("rebuilt from schema version 15"), "{m}");
        assert!(m.contains("inkentry index ."), "names the fix: {m}");
    }

    #[test]
    fn empty_message_names_a_rebuilt_index_under_only_text_too() {
        // Full-text search over an emptied index finds nothing for the same
        // reason semantic search does, so this qualifier is not embedding-shaped.
        let m = empty_message(true, true, true, Some((0, 0)), 0, 0, Some(15));
        assert!(m.contains("rebuilt from"), "{m}");
    }

    #[test]
    fn empty_message_names_an_unstamped_rebuild_without_inventing_a_version() {
        let m = empty_message(true, true, false, Some((0, 0)), 0, 0, Some(0));
        assert!(m.contains("an older, unstamped schema"), "{m}");
        assert!(!m.contains("version 0"), "no such schema version: {m}");
    }

    #[test]
    fn empty_message_ignores_a_rebuilt_code_index_under_only_memory() {
        // --only-memory never reads the code index, so its state cannot explain
        // the absence.
        let m = empty_message(false, true, false, None, 0, 0, Some(15));
        assert_eq!(m, "No results found.");
    }

    // ── warmup / freshness notices ─────────────────────────────────────────────

    #[test]
    fn partial_notice_names_coverage_and_its_front_loaded_shape() {
        let n = warmup_notice_partial(11_813, 27_734);
        assert!(n.contains("11813/27734"), "labelled coverage: {n}");
        assert!(n.contains("42%"), "carries the percentage: {n}");
        assert!(
            n.contains("front-loaded by importance and recency"),
            "names the shape: {n}"
        );
        assert!(n.contains("inkentry status"), "actionable: {n}");
    }

    #[test]
    fn zero_notice_uses_fts_not_ast_grep() {
        let n = warmup_notice_zero(27_734);
        assert!(n.contains("0/27734"));
        assert!(n.contains("full-text search"));
        assert!(!n.contains("ast-grep"), "the ast-grep degrade is gone: {n}");
    }

    #[test]
    fn refresh_notice_names_pending_and_shifting_rankings() {
        let n = refresh_pending_notice(12);
        assert!(n.contains("12 chunk(s)"));
        assert!(n.contains("rankings may shift"));
    }

    #[test]
    fn memory_warmup_notice_singular_and_plural() {
        assert!(memory_warmup_notice(1).contains("1 memory entry not yet embedded"));
        assert!(memory_warmup_notice(3).contains("3 memory entries not yet embedded"));
        assert!(memory_warmup_notice(3).contains("full-text search"));
    }

    // ── semantic_unavailable_message: full-text degrade, never ast-grep ────────

    // A reachable server whose embedder is in `state`. `auto_discovered`
    // decides whether the notice may point at `inkentry server logs`, which
    // only ever reads the local daemon's log.
    fn server_tier(state: EmbedderState, auto_discovered: bool, url: &str) -> capability::Tier {
        capability::Tier::Server {
            url: url.to_string(),
            caps: capability::Capabilities::all(),
            auto_discovered,
            embedder_state: state,
            server_limits: None,
        }
    }

    #[test]
    fn unavailable_notice_degrades_to_full_text_never_ast_grep() {
        let mut tiers: Vec<capability::Tier> = [
            EmbedderState::Loading,
            EmbedderState::Unavailable,
            EmbedderState::Ready,
        ]
        .into_iter()
        .map(|s| server_tier(s, true, "http://127.0.0.1:4655"))
        .collect();
        tiers.extend(
            capability::ALL_OFFLINE_REASONS
                .into_iter()
                .map(capability::Tier::Offline),
        );

        for tier in &tiers {
            let msg = semantic_unavailable_message(tier, Some("http://x:1"), false);
            assert!(!msg.is_empty());
            assert!(
                !msg.contains("ast-grep"),
                "no ast-grep degrade remains: {msg}"
            );
        }
    }

    #[test]
    fn unavailable_loopback_points_at_local_logs() {
        let tier = server_tier(EmbedderState::Unavailable, true, "http://127.0.0.1:4655");
        let msg = semantic_unavailable_message(&tier, Some("http://x:1"), false);
        assert!(msg.contains("failed to load"));
        assert!(msg.contains("inkentry server logs"));
    }

    #[test]
    fn unavailable_remote_names_that_server_never_local_logs() {
        let tier = server_tier(
            EmbedderState::Unavailable,
            false,
            "https://team.example:4655",
        );
        let msg = semantic_unavailable_message(&tier, Some("http://x:1"), false);
        assert!(msg.contains("https://team.example:4655"), "got: {msg}");
        assert!(
            !msg.contains("inkentry server logs"),
            "must not point a remote failure at local logs: {msg}"
        );
    }

    #[test]
    fn no_server_with_configured_url_names_it_and_windows_hint_only_on_windows() {
        let tier = capability::Tier::Offline(capability::OfflineReason::ExplicitServerUnavailable);
        let win = semantic_unavailable_message(&tier, Some("https://team.example:4655"), true);
        assert!(win.contains("https://team.example:4655"));
        assert!(win.contains("no server reachable"));
        assert!(win.contains("Firewall"));
        assert!(win.contains("overriding"));
        let unix = semantic_unavailable_message(&tier, Some("https://team.example:4655"), false);
        assert!(!unix.contains("Firewall"));
    }

    #[test]
    fn no_server_no_url_suggests_starting_one() {
        let tier = capability::Tier::Offline(capability::OfflineReason::NoLocalServer);
        let msg = semantic_unavailable_message(&tier, None, false);
        assert!(msg.contains("inkentry server start"));
    }

    // Spelled as an escape so this file contributes no literal em-dash; the
    // byte asserted is the one the notice has always carried.
    const NO_LOCAL_SERVER_NOTICE: &str = "[no server running \u{2014} start one with \
         `inkentry server start` to enable semantic ranking; using full-text search]";

    // A daemon started by an earlier build records no instance_id, so discovery
    // refuses it and prints a warning naming that cause and the stop/start
    // remedy. This notice prints directly underneath. A server IS running; it
    // was simply not used, and saying otherwise contradicts the line above.
    #[test]
    fn recorded_daemon_refused_by_discovery_is_not_reported_as_no_server_running() {
        let tier = capability::Tier::Offline(capability::OfflineReason::RecordedServerUnreachable);
        for server_url in [None, Some("https://team.example:4655")] {
            let msg = semantic_unavailable_message(&tier, server_url, false);
            assert!(
                !msg.contains("no server running"),
                "contradicts the discovery warning printed above it: {msg}"
            );
            assert!(
                msg.contains("could not be identified"),
                "must name the cause the warning above named: {msg}"
            );
            assert!(
                msg.contains("inkentry server stop") && msg.contains("inkentry server start"),
                "must carry the same stop/start remedy: {msg}"
            );
        }
    }

    // A local daemon whose embeddings this build cannot read answered the
    // probe. It is running, and starting another one changes nothing.
    #[test]
    fn local_server_unusable_notice_names_the_dimension_mismatch() {
        let tier = capability::Tier::Offline(capability::OfflineReason::LocalServerUnusable);
        for server_url in [None, Some("https://team.example:4655")] {
            let msg = semantic_unavailable_message(&tier, server_url, false);
            assert!(!msg.contains("no server running"), "{msg}");
            assert!(msg.contains("different dimension"), "{msg}");
            assert!(
                msg.contains("inkentry server stop") && msg.contains("inkentry server start"),
                "{msg}"
            );
        }
    }

    // The one offline case where "no server running" is the truth. Its text is
    // the pre-existing one and stays byte-identical.
    #[test]
    fn genuinely_no_server_and_no_server_url_keeps_its_existing_text() {
        let tier = capability::Tier::Offline(capability::OfflineReason::NoLocalServer);
        assert_eq!(
            semantic_unavailable_message(&tier, None, false),
            NO_LOCAL_SERVER_NOTICE
        );
    }

    // An explicit `server_url` is a memory replica in local_first, and `search`
    // runs on the inference tier, which probed loopback. Naming the configured
    // URL there describes a server this run never contacted.
    #[test]
    fn a_loopback_offline_reason_never_names_the_configured_server_url() {
        for reason in [
            capability::OfflineReason::NoLocalServer,
            capability::OfflineReason::LocalServerUnusable,
            capability::OfflineReason::RecordedServerUnreachable,
        ] {
            let tier = capability::Tier::Offline(reason);
            let msg = semantic_unavailable_message(&tier, Some("https://team.example:4655"), false);
            assert!(
                !msg.contains("https://team.example:4655"),
                "{reason:?} names a server the inference probe never contacted: {msg}"
            );
        }
    }

    // Under an explicit opt-out no URL is read and no probe is made, so
    // offering to start a server is advice that provably changes nothing.
    #[test]
    fn an_explicit_offline_opt_out_names_the_switch_and_never_a_server_to_start() {
        for reason in [
            capability::OfflineReason::KillSwitch,
            capability::OfflineReason::ModeOfflineEnv,
            capability::OfflineReason::ModeOfflineConfig,
        ] {
            let tier = capability::Tier::Offline(reason);
            let msg = semantic_unavailable_message(&tier, Some("https://team.example:4655"), false);
            assert!(!msg.contains("no server running"), "{reason:?}: {msg}");
            assert!(
                !msg.contains("inkentry server start"),
                "{reason:?} offers a server start that cannot take effect: {msg}"
            );
        }
    }
}
