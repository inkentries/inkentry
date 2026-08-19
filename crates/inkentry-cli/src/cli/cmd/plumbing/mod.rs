use anyhow::Result;
use clap::{Args, Subcommand};
use inkentry_core::storage::NoteId;

#[derive(Args, Debug)]
pub struct PlumbingArgs {
    #[command(subcommand)]
    pub command: PlumbingCommand,

    /// Path to the SQLite database (overrides auto-detect)
    #[arg(short, long, global = true)]
    pub db: Option<std::path::PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum PlumbingCommand {
    /// Emit indexed chunks for a file as JSONL
    CatChunks(PlumbingCatChunksArgs),
    /// List all indexed files as JSONL
    LsFiles(PlumbingLsFilesArgs),
    /// Parse a file and emit chunks as JSONL (without storing)
    ParseFile(PlumbingParseFileArgs),
    /// Compute blake3 hash of a file and check index currency
    HashFile(PlumbingHashFileArgs),
    /// KNN vector search returning JSONL results
    Knn(PlumbingKnnArgs),
    /// Read lines from stdin and emit embedding vectors as JSONL
    Embed(PlumbingEmbedArgs),
    /// Emit code graph edges as JSONL
    GraphEdges(PlumbingGraphEdgesArgs),
    /// Emit memory entries as JSONL
    ReadMemory(PlumbingReadMemoryArgs),
    /// Publish memory notes (refs/notes/inkentry) to a remote
    PublishNotes(PlumbingPublishNotesArgs),
    /// Push local memory to the configured team server (one-way; emits a JSONL report)
    Push(PlumbingPushArgs),
    /// Pull new memory from the configured team server (one-way; emits a JSONL report)
    Pull(PlumbingPullArgs),
}

#[derive(Args, Debug)]
pub struct PlumbingCatChunksArgs {
    /// Path of the file whose chunks to emit (relative to project root)
    pub file: String,
}

#[derive(Args, Debug)]
pub struct PlumbingLsFilesArgs {
    /// Only list files whose path starts with this prefix
    #[arg(long)]
    pub prefix: Option<String>,

    /// Only emit files where on-disk hash differs from stored hash
    #[arg(long)]
    pub stale: bool,

    /// Project root for resolving relative paths stored in the index (defaults to CWD)
    #[arg(long)]
    pub root: Option<std::path::PathBuf>,
}

#[derive(Args, Debug)]
pub struct PlumbingParseFileArgs {
    /// Path to the file to parse
    pub file: std::path::PathBuf,
}

#[derive(Args, Debug)]
pub struct PlumbingHashFileArgs {
    /// Path to the file to hash
    pub file: std::path::PathBuf,
}

#[derive(Args, Debug)]
pub struct PlumbingKnnArgs {
    /// Maximum number of results (default: 10)
    #[arg(long, default_value = "10")]
    pub limit: usize,

    /// Drop results below this cosine similarity score
    #[arg(long, default_value = "0.0")]
    pub min_score: f32,

    /// Restrict results to chunks from files of this language
    #[arg(long)]
    pub lang: Option<String>,
}

#[derive(Args, Debug)]
pub struct PlumbingEmbedArgs {
    /// Prepend query retrieval prefix instead of document prefix
    #[arg(long)]
    pub query: bool,
}

#[derive(Args, Debug)]
pub struct PlumbingGraphEdgesArgs {
    /// Filter edges to those involving this file (path relative to project root)
    #[arg(long)]
    pub file: Option<String>,

    /// Filter edges to those involving this symbol name
    #[arg(long)]
    pub symbol: Option<String>,
}

#[derive(Args, Debug)]
pub struct PlumbingReadMemoryArgs {
    /// Filter by memory kind (decision, question, note, etc.)
    #[arg(long)]
    pub kind: Option<String>,

    /// Fetch a single entry by id
    #[arg(long)]
    pub id: Option<NoteId>,

    /// Maximum number of entries (default: 50)
    #[arg(long, default_value = "50")]
    pub limit: usize,
}

#[derive(Args, Debug)]
pub struct PlumbingPushArgs {
    /// Local memory.db to push from (default: the project's memory store)
    #[arg(long)]
    pub source: Option<std::path::PathBuf>,

    /// Push archived entries too (propagates tombstones)
    #[arg(long)]
    pub include_archived: bool,
}

#[derive(Args, Debug)]
pub struct PlumbingPullArgs {}

#[derive(Args, Debug)]
pub struct PlumbingPublishNotesArgs {
    /// Remote to publish to (default: origin)
    pub remote: Option<String>,

    /// Remote URL. Accepted and ignored: git passes it to a pre-push hook as $2.
    #[arg(hide = true)]
    pub url: Option<String>,

    /// Warn on stderr and exit 0 when publishing fails, rather than reporting it
    #[arg(long)]
    pub best_effort: bool,
}

use crate::config::Config;

mod cat_chunks;
mod embed_cmd;
mod graph_edges;
mod hash_file;
mod knn;
mod ls_files;
mod parse_file;
mod publish_notes;
mod pull;
mod push;
mod read_memory;

/// Resolve the memory store a memory-targeting plumbing subcommand acts on.
///
/// Keys on the `.inkentry/` **directory** ([`find_project_dir`]), never on a
/// present `index.db`: a project can be configured and never indexed, and the
/// index walk would then step over it to the machine-global store while
/// `memory add`, `memory list` and `sync` in that same directory stay on the
/// project store. Memory needs no index, so the index is not what says a
/// project is here.
///
/// Outside any project the global store is still the honest answer, but a
/// silent one is indistinguishable from an empty project store, so it is named
/// on stderr. stdout stays the JSONL report alone.
///
/// [`find_project_dir`]: inkentry_core::config::find_project_dir
fn resolve_memory_path(explicit_db: Option<&std::path::Path>, cfg: &Config) -> std::path::PathBuf {
    if let Some(p) = explicit_db {
        return p.with_file_name("memory.db");
    }
    if let Ok(cwd) = std::env::current_dir()
        && let Some(dir) = crate::config::find_project_dir(&cwd)
    {
        return dir.join("memory.db");
    }
    let global = cfg.db_path.with_file_name("memory.db");
    eprintln!(
        "note: no inkentry project here (no .inkentry/ directory found); \
         acting on the global memory store at {}",
        global.display()
    );
    global
}

/// Refuse a memory store that is not there.
///
/// Applied to the commands that only read from the store or only send from it
/// (`read-memory`, `push`), never to the ones that receive: exit 1 means "no
/// entries" or "empty delta", and an absent store is neither. Skipped when
/// `cloud_first` routes memory to a team server, which owns the store and
/// leaves the local path a placeholder nothing opens.
fn require_memory_store(mem_path: &std::path::Path, cfg: &Config) -> Result<()> {
    let routes_remote =
        cfg.resolve_mode() == crate::config::SyncMode::CloudFirst && cfg.server_url.is_some();
    if routes_remote || mem_path.exists() {
        return Ok(());
    }
    anyhow::bail!(
        "No memory store found at {}.\n\
         Run `inkentry init` in your project first, or pass an explicit --db.",
        mem_path.display()
    )
}

pub async fn plumbing(args: PlumbingArgs, cfg: Config) -> Result<()> {
    // Most plumbing commands need the project DB; open it once here.
    // `embed` and `parse-file` do not need it but it's cheap to open.
    let db_path = args
        .db
        .as_deref()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| crate::config::resolve_db(None, &cfg.db_path));

    match args.command {
        PlumbingCommand::ParseFile(a) => return parse_file::parse_file(a),
        PlumbingCommand::Embed(a) => return embed_cmd::embed_cmd(&cfg, &db_path, a.query).await,
        // Notes are the pre-`init` store (ADR-068), so publishing must not
        // require an index.
        PlumbingCommand::PublishNotes(a) => return publish_notes::publish_notes(a).await,
        // The memory commands target memory.db and never read a chunk, so they
        // neither require an index nor take the project's identity from one.
        // Checked here rather than inside `push` so the refusal cannot reach
        // `sync`, which shares push's core but bootstraps a fresh checkout.
        PlumbingCommand::Push(a) => {
            let mem_path = a
                .source
                .clone()
                .unwrap_or_else(|| resolve_memory_path(args.db.as_deref(), &cfg));
            require_memory_store(&mem_path, &cfg)?;
            return push::push(a, &mem_path, &cfg).await;
        }
        // Pull receives, so it may create: that is how a fresh checkout first
        // gets team memory.
        PlumbingCommand::Pull(_) => {
            let mem_path = resolve_memory_path(args.db.as_deref(), &cfg);
            return pull::pull(&mem_path, &cfg).await;
        }
        PlumbingCommand::ReadMemory(a) => {
            let mem_path = resolve_memory_path(args.db.as_deref(), &cfg);
            require_memory_store(&mem_path, &cfg)?;
            return read_memory::read_memory(a, &mem_path, &cfg).await;
        }
        _ => {}
    }

    // Commands below require the index DB.
    if !db_path.exists() {
        anyhow::bail!(
            "No index found (checked current directory and parents).\n\
             Run `inkentry index <path>` inside your project first."
        );
    }
    let db = crate::storage::Database::open(&db_path)?;

    match args.command {
        PlumbingCommand::CatChunks(a) => cat_chunks::cat_chunks(a, &db, &cfg),
        PlumbingCommand::LsFiles(a) => ls_files::ls_files(a, &db),
        PlumbingCommand::HashFile(a) => hash_file::hash_file(a, &db),
        PlumbingCommand::Knn(a) => knn::knn(a, &db).await,
        PlumbingCommand::GraphEdges(a) => graph_edges::graph_edges(a, &db),
        // Already handled above but Rust requires exhaustive match.
        PlumbingCommand::ParseFile(_)
        | PlumbingCommand::Embed(_)
        | PlumbingCommand::PublishNotes(_)
        | PlumbingCommand::Push(_)
        | PlumbingCommand::Pull(_)
        | PlumbingCommand::ReadMemory(_) => unreachable!(),
    }
}
