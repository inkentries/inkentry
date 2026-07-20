// Chunk-size retrieval-quality eval: measures MRR@10 / Recall@10 / nDCG@10 +
// Recall@fixed-token-budget at a swept set of chunk-token caps, so the
// MAX_CHUNK_TOKENS default can be picked on quality evidence, not perf alone.
//
// Lives here (not in `spelunk-embed`, where `embed_bench` lives) because it
// needs the real chunker (`spelunk_core::indexer`), and `spelunk-core`
// depends on `spelunk-embed` – the reverse dependency isn't available, so a
// tool needing both can only live on the `spelunk-core` side of that edge.
//
// No `spelunk` CLI and no `spelunk-server`/HTTP in the path: this calls
// `SourceParser` and `NativeEmbedder` directly, the same way `embed_bench`
// calls the embedder directly.
//
// Methodology (see the task this harness was built for):
//   - Relevance is keyed on file + line-range overlap, not chunk id, because
//     the retrieval unit itself changes with the cap (a chunk that's whole at
//     2048 may be split into several pieces at 512).
//   - Two arms: "leaky" embeds chunks as shipped (the docstring named in a
//     query is present inside `embedding_text()`, so retrieval is
//     near-exact-match); "held_out" strips the docstring from every indexed
//     chunk first, a genuine "can NL find this code" test.
//   - Recall@token-budget (not just @10) because the decision this data feeds
//     is about context-window cost per unit of relevant code, which `@10`
//     alone structurally flatters large chunks on.
//
// Usage:
//   cargo run --release -p spelunk-core --example chunk_quality_eval -- \
//       --gguf <path> --tokenizer <path> --config <path> \
//       --corpus /path/to/repo \
//       [--caps 2048,1024,512,384] [--budget-tokens 8192] \
//       [--min-docstring-chars 20] [--max-queries 300] [--limit-files 0] \
//       [--out results.json]

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use spelunk_core::embeddings::EmbeddingBackend;
use spelunk_core::indexer::filter::{Decision, IndexFilter, generated_marker};
use spelunk_core::indexer::parser::detect_language;
use spelunk_core::indexer::{Chunk, SourceParser, set_chunk_token_cap};
use spelunk_embed::NativeEmbedder;
use tokenizers::Tokenizer;

/// Code-search query instruction prefix – matches the format `spelunk-server`
/// uses for real code-search queries (see CLAUDE.md "Embedding input format").
const QUERY_INSTRUCTION: &str =
    "Instruct: Given a code search query, retrieve the relevant code snippets\nQuery: ";

const DEFAULT_CAPS: &[usize] = &[2048, 1024, 512, 384];
const DEFAULT_TOP_K: usize = 10;
/// A modest, realistic context-assembly budget (tokens) for Recall@budget –
/// not tied to any particular cap's `@10`, since the whole point of this
/// metric is to compare caps on a cap-independent axis.
const DEFAULT_BUDGET_TOKENS: usize = 8192;
/// Docstrings shorter than this are usually a one-word `// TODO` or a bare
/// derive comment, not a usable natural-language query.
const DEFAULT_MIN_DOCSTRING_CHARS: usize = 20;
/// External batch size for embedding calls: bounds one call's memory and
/// gives incremental progress output on a multi-minute corpus embed.
const EMBED_PROGRESS_BATCH: usize = 32;

struct Args {
    gguf: PathBuf,
    tokenizer: PathBuf,
    config: PathBuf,
    corpus: PathBuf,
    caps: Vec<usize>,
    budget_tokens: usize,
    min_docstring_chars: usize,
    max_queries: Option<usize>,
    limit_files: Option<usize>,
    out: Option<PathBuf>,
}

fn print_usage() {
    eprintln!(
        "chunk_quality_eval – chunk-size retrieval-quality eval\n\n\
         Required:\n\
         \x20 --gguf <path>       Q8_0 GGUF weights\n\
         \x20 --tokenizer <path>  tokenizer.json\n\
         \x20 --config <path>     Qwen3 config.json\n\
         \x20 --corpus <dir>      repo root to index and query against\n\
         Optional:\n\
         \x20 --caps a,b,c            chunk-token caps to sweep (default: {DEFAULT_CAPS:?})\n\
         \x20 --budget-tokens N       Recall@budget token budget (default: {DEFAULT_BUDGET_TOKENS})\n\
         \x20 --min-docstring-chars N minimum docstring length to source a query from (default: {DEFAULT_MIN_DOCSTRING_CHARS})\n\
         \x20 --max-queries N         cap the query set (deterministic stride sample)\n\
         \x20 --limit-files N         cap the corpus file list (deterministic even stride over the sorted list)\n\
         \x20 --out <path>            write results as JSON\n"
    );
}

fn parse_args() -> Result<Args> {
    let mut gguf = None;
    let mut tokenizer = None;
    let mut config = None;
    let mut corpus = None;
    let mut caps = None;
    let mut budget_tokens = DEFAULT_BUDGET_TOKENS;
    let mut min_docstring_chars = DEFAULT_MIN_DOCSTRING_CHARS;
    let mut max_queries = None;
    let mut limit_files = None;
    let mut out = None;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut next_path = |flag: &str| -> Result<PathBuf> {
            it.next()
                .map(PathBuf::from)
                .with_context(|| format!("{flag} requires a value"))
        };
        match arg.as_str() {
            "--gguf" => gguf = Some(next_path("--gguf")?),
            "--tokenizer" => tokenizer = Some(next_path("--tokenizer")?),
            "--config" => config = Some(next_path("--config")?),
            "--corpus" => corpus = Some(next_path("--corpus")?),
            "--out" => out = Some(next_path("--out")?),
            "--caps" => {
                let raw = it.next().context("--caps requires a value")?;
                caps = Some(
                    raw.split(',')
                        .map(|s| {
                            s.trim()
                                .parse::<usize>()
                                .context("--caps must be a comma-separated integer list")
                        })
                        .collect::<Result<Vec<_>>>()?,
                );
            }
            "--budget-tokens" => {
                budget_tokens = it
                    .next()
                    .context("--budget-tokens requires a value")?
                    .parse()
                    .context("--budget-tokens must be a positive integer")?;
            }
            "--min-docstring-chars" => {
                min_docstring_chars = it
                    .next()
                    .context("--min-docstring-chars requires a value")?
                    .parse()
                    .context("--min-docstring-chars must be a positive integer")?;
            }
            "--max-queries" => {
                max_queries = Some(
                    it.next()
                        .context("--max-queries requires a value")?
                        .parse()
                        .context("--max-queries must be a positive integer")?,
                );
            }
            "--limit-files" => {
                let n: usize = it
                    .next()
                    .context("--limit-files requires a value")?
                    .parse()
                    .context("--limit-files must be an integer")?;
                if n > 0 {
                    limit_files = Some(n);
                }
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => bail!("unrecognised argument: {other}"),
        }
    }

    let mut caps = caps.unwrap_or_else(|| DEFAULT_CAPS.to_vec());
    caps.sort_unstable_by(|a, b| b.cmp(a)); // descending – largest (gold) cap first
    caps.dedup();
    anyhow::ensure!(!caps.is_empty(), "--caps must name at least one cap");

    Ok(Args {
        gguf: gguf.context("--gguf is required")?,
        tokenizer: tokenizer.context("--tokenizer is required")?,
        config: config.context("--config is required")?,
        corpus: corpus.context("--corpus is required")?,
        caps,
        budget_tokens,
        min_docstring_chars,
        max_queries,
        limit_files,
        out,
    })
}

/// One corpus file: absolute path (for reading), corpus-relative path (the
/// stable identity used for both chunk `file_path` and query ground truth),
/// and detected language.
struct CorpusFile {
    abs: PathBuf,
    rel: String,
    language: &'static str,
}

/// Walk `root` for code files, applying the same two exclude layers the real
/// indexer applies (`IndexFilter` defaults + self-declared generated-file
/// markers) so the eval corpus matches what `spelunk index` would actually
/// embed. This is a simplified mirror of `spelunk-cli`'s `collect_files` (not
/// reusable directly – it lives in a crate `spelunk-core` doesn't depend on)
/// and skips its sensitive-file `OverrideBuilder` layer, since this harness
/// only ever points at repos the operator already trusts locally.
fn collect_corpus_files(root: &Path, limit_files: Option<usize>) -> Result<Vec<CorpusFile>> {
    anyhow::ensure!(
        root.is_dir(),
        "--corpus {} is not a directory",
        root.display()
    );
    let filter = IndexFilter::build(&[], true, true).context("building index filter")?;

    let mut walk = WalkBuilder::new(root);
    walk.standard_filters(true);
    let root_owned = root.to_path_buf();
    let dir_filter = filter.clone();
    walk.filter_entry(move |entry| {
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if !is_dir {
            return true;
        }
        match entry.path().strip_prefix(&root_owned) {
            Ok(rel) if !rel.as_os_str().is_empty() => !dir_filter.prune_dir(rel),
            _ => true,
        }
    });

    let mut files = Vec::new();
    for entry in walk.build().filter_map(|e| e.ok()) {
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let abs = entry.path().to_path_buf();
        let Some(language) = detect_language(&abs) else {
            continue; // text/doc formats out of scope for a code-search eval
        };
        let rel_path = abs.strip_prefix(root).unwrap_or(&abs);
        match filter.decide(rel_path, false) {
            Decision::Exclude(_) => continue,
            Decision::ForceInclude(_) => {}
            Decision::Keep => {
                if generated_marker(&abs).is_some() {
                    continue;
                }
            }
        }
        files.push(CorpusFile {
            abs: abs.clone(),
            rel: rel_path.to_string_lossy().replace('\\', "/"),
            language,
        });
    }

    // Deterministic order (walk order isn't stable across filesystems).
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    if let Some(limit) = limit_files
        && files.len() > limit
    {
        // An even stride across the sorted list, not a prefix: a real repo's
        // first paths alphabetically are often one lightly-commented
        // directory (e.g. `migrations/` sorts before `src/`), so a prefix
        // truncate silently samples only that directory instead of the whole
        // corpus.
        let step = (files.len() as f64 / limit as f64).ceil() as usize;
        files = files.into_iter().step_by(step.max(1)).take(limit).collect();
    }
    Ok(files)
}

/// Parse every corpus file at the process-global `chunk_token_cap()` in
/// effect (caller sets it via `set_chunk_token_cap` first). Files that fail
/// to read or parse are skipped with a warning, not fatal – a benchmark
/// corpus is expected to contain the odd oddball file.
fn chunk_corpus(files: &[CorpusFile]) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    for f in files {
        let source = match std::fs::read_to_string(&f.abs) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipping {} (read error: {e})", f.rel);
                continue;
            }
        };
        match SourceParser::parse(&source, &f.rel, f.language) {
            Ok(cs) => chunks.extend(cs),
            Err(e) => eprintln!("skipping {} (parse error: {e})", f.rel),
        }
    }
    chunks
}

/// A query sourced from one gold-pass chunk's docstring, with the ground-truth
/// line range it was drawn from (see module doc: relevance at every cap is
/// judged against this range by overlap, not by chunk id).
struct Query {
    text: String,
    file_path: String,
    start_line: usize,
    end_line: usize,
}

fn build_gold_queries(
    gold_chunks: &[Chunk],
    min_docstring_chars: usize,
    max_queries: Option<usize>,
) -> Vec<Query> {
    let mut queries: Vec<Query> = gold_chunks
        .iter()
        .filter_map(|c| {
            let doc = c.docstring.as_deref()?.trim();
            if doc.chars().count() < min_docstring_chars {
                return None;
            }
            Some(Query {
                text: doc.to_string(),
                file_path: c.file_path.clone(),
                start_line: c.start_line,
                end_line: c.end_line,
            })
        })
        .collect();

    if let Some(max) = max_queries
        && queries.len() > max
    {
        // Deterministic stride sample rather than a shuffle – reproducible
        // without needing an RNG dependency, and spreads the sample evenly
        // across the corpus (queries are in file-walk order) instead of
        // biasing toward whichever files sort first.
        let step = (queries.len() as f64 / max as f64).ceil() as usize;
        queries = queries.into_iter().step_by(step.max(1)).take(max).collect();
    }
    queries
}

fn ranges_overlap(a_start: usize, a_end: usize, b_start: usize, b_end: usize) -> bool {
    a_start <= b_end && b_start <= a_end
}

/// Indices into `chunks` whose (file, line-range) overlaps `q`'s gold range –
/// the cap-dependent relevant set for this query (see module doc: this is
/// what makes Recall/nDCG comparable across caps despite the retrieval unit
/// itself changing size).
fn relevant_indices(chunks: &[Chunk], q: &Query) -> HashSet<usize> {
    chunks
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.file_path == q.file_path
                && ranges_overlap(c.start_line, c.end_line, q.start_line, q.end_line)
        })
        .map(|(i, _)| i)
        .collect()
}

fn mrr_at_k(ranked: &[usize], relevant: &HashSet<usize>, k: usize) -> f64 {
    for (i, idx) in ranked.iter().take(k).enumerate() {
        if relevant.contains(idx) {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

fn recall_at_k(
    ranked: &[usize],
    relevant: &HashSet<usize>,
    k: usize,
    total_relevant: usize,
) -> f64 {
    let hits = ranked
        .iter()
        .take(k)
        .filter(|idx| relevant.contains(idx))
        .count();
    hits as f64 / total_relevant as f64
}

/// Binary-relevance nDCG@k: `1/log2(rank+1)` per hit, normalised by the ideal
/// ordering (all `min(total_relevant, k)` relevant items ranked first).
fn ndcg_at_k(ranked: &[usize], relevant: &HashSet<usize>, k: usize, total_relevant: usize) -> f64 {
    let dcg: f64 = ranked
        .iter()
        .take(k)
        .enumerate()
        .filter(|(_, idx)| relevant.contains(idx))
        .map(|(i, _)| 1.0 / ((i as f64) + 2.0).log2()) // rank i is 0-based -> position i+1 -> log2(position+1)
        .sum();
    let ideal_hits = total_relevant.min(k);
    let idcg: f64 = (0..ideal_hits)
        .map(|i| 1.0 / ((i as f64) + 2.0).log2())
        .sum();
    if idcg == 0.0 { 0.0 } else { dcg / idcg }
}

/// Recall among chunks retrievable within `budget` tokens: walk `ranked` in
/// order, greedily accumulating each chunk's real embedding-text token cost,
/// and stop at the first chunk that would overflow the budget (a rank-order
/// context assembly, not a knapsack pack – swapping in a smaller lower-ranked
/// chunk to keep filling would break rank fidelity).
fn recall_at_budget(
    ranked: &[usize],
    relevant: &HashSet<usize>,
    token_counts: &[usize],
    budget: usize,
    total_relevant: usize,
) -> f64 {
    let mut used = 0usize;
    let mut hits = 0usize;
    for &idx in ranked {
        let cost = token_counts[idx];
        if used + cost > budget {
            break;
        }
        used += cost;
        if relevant.contains(&idx) {
            hits += 1;
        }
    }
    hits as f64 / total_relevant as f64
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Full ranking of every corpus chunk by descending cosine similarity to
/// `query_vec`. Vectors are L2-normalised (`NativeEmbedder`'s contract), so a
/// plain dot product is cosine similarity – brute force is fine at the
/// corpus sizes this harness targets (thousands of chunks, not millions).
fn rank_by_similarity(query_vec: &[f32], chunk_vecs: &[Vec<f32>]) -> Vec<usize> {
    let mut scored: Vec<(usize, f32)> = chunk_vecs
        .iter()
        .enumerate()
        .map(|(i, v)| (i, dot(query_vec, v)))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).expect("no NaNs in embedding output"));
    scored.into_iter().map(|(i, _)| i).collect()
}

/// Embed `texts` in `EMBED_PROGRESS_BATCH`-sized external batches (each one
/// call to the backend, which does its own internal sub-batching) so a
/// multi-minute corpus embed prints incremental progress instead of going
/// silent, and a crash mid-run loses at most one external batch of work.
async fn embed_all(
    embedder: &NativeEmbedder,
    texts: &[String],
    label: &str,
) -> Result<Vec<Vec<f32>>> {
    let mut out = Vec::with_capacity(texts.len());
    for batch in texts.chunks(EMBED_PROGRESS_BATCH) {
        let refs: Vec<&str> = batch.iter().map(String::as_str).collect();
        let vecs = embedder
            .embed(&refs)
            .await
            .context("embed_all: embedder.embed failed")?;
        out.extend(vecs);
        eprintln!("  {label}: embedded {}/{}", out.len(), texts.len());
    }
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Arm {
    Leaky,
    HeldOut,
}

impl Arm {
    fn label(self) -> &'static str {
        match self {
            Arm::Leaky => "leaky",
            Arm::HeldOut => "held_out",
        }
    }

    /// The text actually indexed for one chunk under this arm. `Leaky` is
    /// `embedding_text()` as-shipped (docstring included, so a query drawn
    /// from that same docstring is near-exact-match). `HeldOut` strips the
    /// docstring first – the genuine "can NL find this code" test.
    fn embedding_text(self, c: &Chunk) -> String {
        match self {
            Arm::Leaky => c.embedding_text(),
            Arm::HeldOut => {
                let mut stripped = c.clone();
                stripped.docstring = None;
                stripped.embedding_text()
            }
        }
    }
}

struct ResultRow {
    cap: usize,
    arm: &'static str,
    n_chunks: usize,
    n_queries: usize,
    mrr_at_10: f64,
    recall_at_10: f64,
    ndcg_at_10: f64,
    recall_at_budget: f64,
}

fn main() -> Result<()> {
    let args = parse_args()?;

    let embedder = NativeEmbedder::load_from_path(&args.gguf, &args.tokenizer, &args.config)
        .context("loading NativeEmbedder")?;
    let tokenizer = Tokenizer::from_file(&args.tokenizer)
        .map_err(|e| anyhow::anyhow!("loading measurement tokenizer copy: {e}"))?;
    let rt = tokio::runtime::Runtime::new().context("building tokio runtime")?;

    let files = collect_corpus_files(&args.corpus, args.limit_files)?;
    anyhow::ensure!(
        !files.is_empty(),
        "no code files found under {}",
        args.corpus.display()
    );
    println!(
        "corpus: {} files under {}",
        files.len(),
        args.corpus.display()
    );

    // Gold pass: ground-truth query set drawn from the largest (least
    // fragmented) cap in the sweep – the current shipped default when 2048 is
    // included, or the closest stand-in otherwise.
    let gold_cap = args.caps[0]; // sorted descending in parse_args
    set_chunk_token_cap(gold_cap);
    let gold_chunks = chunk_corpus(&files);
    let queries = build_gold_queries(&gold_chunks, args.min_docstring_chars, args.max_queries);
    anyhow::ensure!(
        !queries.is_empty(),
        "no eligible queries (docstring >= {} chars) at cap={gold_cap} – lower --min-docstring-chars or point at a larger corpus",
        args.min_docstring_chars
    );
    println!(
        "gold queries: {} (docstring >= {} chars, sourced at cap={gold_cap})",
        queries.len(),
        args.min_docstring_chars
    );

    // Query embeddings don't depend on cap or arm (same input text, same
    // model), so compute them once up front.
    let query_texts: Vec<String> = queries
        .iter()
        .map(|q| format!("{QUERY_INSTRUCTION}{}", q.text))
        .collect();
    let query_vecs = rt.block_on(embed_all(&embedder, &query_texts, "queries"))?;

    let mut results = Vec::new();
    for &cap in &args.caps {
        set_chunk_token_cap(cap);
        let chunks = chunk_corpus(&files);
        println!("\ncap={cap}: {} chunks", chunks.len());

        // Relevance is structural (file + line overlap), not embedding-based,
        // so it's computed once per cap and reused across both arms.
        let relevant_sets: Vec<HashSet<usize>> = queries
            .iter()
            .map(|q| relevant_indices(&chunks, q))
            .collect();
        let zero_relevant = relevant_sets.iter().filter(|s| s.is_empty()).count();
        if zero_relevant > 0 {
            eprintln!(
                "  warning: {zero_relevant}/{} queries have no overlapping chunk at cap={cap} (excluded from this cap's aggregate)",
                queries.len()
            );
        }

        for arm in [Arm::Leaky, Arm::HeldOut] {
            let texts: Vec<String> = chunks.iter().map(|c| arm.embedding_text(c)).collect();
            let chunk_vecs = rt.block_on(embed_all(
                &embedder,
                &texts,
                &format!("cap={cap} arm={}", arm.label()),
            ))?;
            let token_counts: Vec<usize> = texts
                .iter()
                .map(|t| {
                    tokenizer
                        .encode(t.as_str(), true)
                        .map(|e| e.get_ids().len())
                        .unwrap_or(0)
                })
                .collect();

            let (mut mrr_sum, mut recall_sum, mut ndcg_sum, mut budget_sum, mut n) =
                (0.0, 0.0, 0.0, 0.0, 0usize);
            for (qi, _q) in queries.iter().enumerate() {
                let relevant = &relevant_sets[qi];
                if relevant.is_empty() {
                    continue;
                }
                let ranked = rank_by_similarity(&query_vecs[qi], &chunk_vecs);
                let total_relevant = relevant.len();
                mrr_sum += mrr_at_k(&ranked, relevant, DEFAULT_TOP_K);
                recall_sum += recall_at_k(&ranked, relevant, DEFAULT_TOP_K, total_relevant);
                ndcg_sum += ndcg_at_k(&ranked, relevant, DEFAULT_TOP_K, total_relevant);
                budget_sum += recall_at_budget(
                    &ranked,
                    relevant,
                    &token_counts,
                    args.budget_tokens,
                    total_relevant,
                );
                n += 1;
            }

            results.push(ResultRow {
                cap,
                arm: arm.label(),
                n_chunks: chunks.len(),
                n_queries: n,
                mrr_at_10: mrr_sum / n as f64,
                recall_at_10: recall_sum / n as f64,
                ndcg_at_10: ndcg_sum / n as f64,
                recall_at_budget: budget_sum / n as f64,
            });
        }
    }

    print_table(&results);
    println!(
        "\ncorpus-size caveat: top-{DEFAULT_TOP_K} is less discriminative on a small corpus – this run had \
         {} gold queries against corpora ranging {}-{} chunks across caps.",
        queries.len(),
        results.iter().map(|r| r.n_chunks).min().unwrap_or(0),
        results.iter().map(|r| r.n_chunks).max().unwrap_or(0),
    );

    if let Some(path) = &args.out {
        write_json(path, &args, &results)?;
        println!("wrote {}", path.display());
    }

    Ok(())
}

fn print_table(rows: &[ResultRow]) {
    println!(
        "\n{:>6} {:>9} {:>9} {:>9} {:>10} {:>10} {:>14}",
        "cap", "arm", "n_chunks", "n_queries", "mrr@10", "recall@10", "ndcg@10"
    );
    for r in rows {
        println!(
            "{:>6} {:>9} {:>9} {:>9} {:>10.4} {:>10.4} {:>14.4}  recall@budget={:.4}",
            r.cap,
            r.arm,
            r.n_chunks,
            r.n_queries,
            r.mrr_at_10,
            r.recall_at_10,
            r.ndcg_at_10,
            r.recall_at_budget
        );
    }
}

fn write_json(path: &Path, args: &Args, rows: &[ResultRow]) -> Result<()> {
    let mut out = String::from("{\n");
    out.push_str(&format!(
        "  \"corpus\": {:?},\n",
        args.corpus.display().to_string()
    ));
    out.push_str(&format!("  \"budget_tokens\": {},\n", args.budget_tokens));
    out.push_str("  \"results\": [\n");
    for (i, r) in rows.iter().enumerate() {
        out.push_str(&format!(
            "    {{\"cap\": {}, \"arm\": {:?}, \"n_chunks\": {}, \"n_queries\": {}, \"mrr_at_10\": {:.6}, \"recall_at_10\": {:.6}, \"ndcg_at_10\": {:.6}, \"recall_at_budget\": {:.6}}}{}\n",
            r.cap,
            r.arm,
            r.n_chunks,
            r.n_queries,
            r.mrr_at_10,
            r.recall_at_10,
            r.ndcg_at_10,
            r.recall_at_budget,
            if i + 1 < rows.len() { "," } else { "" }
        ));
    }
    out.push_str("  ]\n}\n");
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}
