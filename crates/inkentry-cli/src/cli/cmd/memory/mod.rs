use super::color::cprintln;
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use inkentry_core::storage::NoteId;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct MemoryArgs {
    #[command(subcommand)]
    pub command: MemoryCommand,

    /// Path to the memory database (overrides auto-detect)
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,

    /// Storage backend: sqlite (default) or git-notes
    #[arg(long, global = true, default_value = "sqlite", value_name = "BACKEND")]
    pub backend: String,
}

#[derive(Subcommand, Debug)]
pub enum MemoryCommand {
    /// Store a memory entry (decision, requirement, note, question, handoff, intent, antipattern, etc.)
    Add(MemoryAddArgs),
    /// List memory entries (newest first)
    List(MemoryListArgs),
    /// Show the full content of a memory entry
    Show(MemoryShowArgs),
    /// Deprecated: use `inkentry harvest`. Auto-harvest memory entries from git
    /// commit messages using the LLM. Kept as a still-working alias so hooks and
    /// scripts installed before the promotion keep running.
    #[command(hide = true)]
    Harvest(MemoryHarvestArgs),
    /// Archive a memory entry (hidden from search and ask, but preserved)
    Archive(MemoryArchiveArgs),
    /// Archive an entry and mark it as superseded by a newer entry
    Supersede(MemorySupersededArgs),
    /// Two-way sync: push local entries to the server and pull remote entries to local
    Sync(MemorySyncArgs),
    /// Show how the team's understanding of a topic evolved over time
    Timeline(MemoryTimelineArgs),
    /// Show the relationship graph for a memory entry
    Graph(MemoryGraphArgs),
    /// List all stored antipatterns (shortcut for `list --kind antipattern`)
    Failures(MemoryFailuresArgs),
    /// Import unique notes from server.db into the local memory.db (recovery / migration tool)
    Reconcile(MemoryReconcileArgs),
    /// Backfill missing local embeddings so semantic search can find notes left unembedded (recovery tool)
    Reindex(MemoryReindexArgs),
    /// Collapse duplicate-entity_id groups already resident in local memory.db (recovery tool)
    Dedupe(MemoryDedupeArgs),
    /// List the tag vocabulary with how many active entries carry each (ADR-101)
    Tags(MemoryTagsArgs),
    /// Anchor memory entries to a commit (ADR-099). With no ids, claims
    /// pending entries per the D2 claim rule (same worktree, and the entry
    /// was written from an ancestor of the commit, or the commit it amends);
    /// this is what the post-commit hook calls. With ids, anchors those
    /// entries to the commit directly, skipping the claim rule. Plumbing:
    /// always exits 0 and prints nothing, so it never fails a commit.
    Anchor(MemoryAnchorArgs),
}

#[derive(Args, Debug)]
pub struct MemoryGraphArgs {
    /// Entry ID to show the relationship graph for
    pub id: NoteId,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct MemoryTimelineArgs {
    /// Topic to trace through time
    pub query: String,

    /// Number of entries to retrieve before timeline construction
    #[arg(short, long, default_value = "20")]
    pub limit: usize,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct MemoryAddArgs {
    /// Short title summarising the entry (inferred from URL if --from-url is used)
    #[arg(short, long)]
    pub title: Option<String>,

    /// Full body text (omit to open $EDITOR)
    #[arg(short, long)]
    pub body: Option<String>,

    /// Fetch content from a URL (GitHub issue, Linear ticket, or any web page)
    #[arg(long)]
    pub from_url: Option<String>,

    /// Kind: decision, context, requirement, note, question, answer, handoff, intent, antipattern.
    /// An unknown kind is rejected (it would be invisible to `memory list`,
    /// `context`, and `memory failures`).
    #[arg(
        short,
        long,
        default_value = "note",
        value_parser = inkentry_core::storage::parse_note_kind
    )]
    pub kind: String,

    /// Comma-separated tags (e.g. auth,database)
    #[arg(long)]
    pub tags: Option<String>,

    /// Comma-separated file paths this entry relates to
    #[arg(long)]
    pub files: Option<String>,

    /// When this entry became valid (ISO 8601, e.g. 2026-03-15 or 2026-03-15T10:00:00).
    /// Defaults to now (created_at) when omitted.
    #[arg(long, value_name = "DATE")]
    pub valid_at: Option<String>,

    /// ID of an existing entry that this new entry supersedes.
    /// The old entry's invalid_at is set to now atomically in the same transaction.
    #[arg(long, value_name = "ID")]
    pub supersedes: Option<NoteId>,

    /// ID of an existing entry this entry relates to (creates a relates_to edge).
    #[arg(long, value_name = "ID")]
    pub relates_to: Option<NoteId>,

    /// Anchor this entry to a commit immediately (ADR-099 D4), instead of
    /// recording a pending anchor for the post-commit hook to claim later.
    /// The commit does not have to exist on disk under `git show` for this
    /// process's working tree only — it must resolve with `git rev-parse`.
    #[arg(long, value_name = "SHA")]
    pub commit: Option<String>,

    /// Output format: text, json, or jsonl
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct MemoryAnchorArgs {
    /// The commit (or any ref git resolves to one) to claim pending entries
    /// for, or to anchor `<id>...` to by hand.
    #[arg(long)]
    pub commit: String,

    /// Anchor these entries to `--commit` directly, skipping the D2 claim
    /// rule (ADR-099 D4).
    pub ids: Vec<NoteId>,
}

#[derive(Args, Debug)]
pub struct MemoryListArgs {
    /// Filter by kind: decision, context, requirement, note, intent
    #[arg(short, long)]
    pub kind: Option<String>,

    /// Filter by commit SHA (exact or prefix match against source_ref)
    #[arg(long)]
    pub source_ref: Option<String>,

    /// Number of entries to show
    #[arg(short, long, default_value = "20")]
    pub limit: usize,

    /// Output format: text, json, or jsonl
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Include archived entries
    #[arg(long)]
    pub archived: bool,

    /// Return only entries valid at this point in time (ISO 8601, e.g. 2026-03-15 or 2026-03-15T10:00:00)
    #[arg(long, value_name = "DATE")]
    pub as_of: Option<String>,

    /// List only local project's memory, skipping linked project stores
    #[arg(long)]
    pub local_only: bool,

    /// Only entries carrying this exact tag (normalised the same way a write
    /// is), backed by the `note_tags` index (ADR-101 D4). Requires the sqlite
    /// backend.
    #[arg(long, value_name = "TAG")]
    pub tag: Option<String>,

    /// Only entries linking this exact repository-relative path, backed by
    /// the `note_files` index (ADR-101 D4). Requires the sqlite backend.
    #[arg(long, value_name = "PATH")]
    pub file: Option<String>,
}

#[derive(Args, Debug)]
pub struct MemoryTagsArgs {
    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct MemoryShowArgs {
    /// Entry ID (from list or search output)
    pub id: NoteId,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct MemoryHarvestArgs {
    /// Git revision range to analyse, e.g. `HEAD~10..HEAD` or `v0.1.0..HEAD`.
    /// Mutually exclusive with --branch.
    #[arg(long, default_value = "HEAD~10..HEAD", conflicts_with = "branch")]
    pub git_range: String,

    /// Harvest the entire commit history of a branch, e.g. `main` or `master`.
    /// Mutually exclusive with --git-range.
    #[arg(long, conflicts_with = "git_range")]
    pub branch: Option<String>,

    /// Number of commits/sessions to send to the LLM in each request.
    /// Smaller values are more stable; larger values risk hitting context-window limits.
    #[arg(long, default_value_t = 3)]
    pub batch_size: usize,

    /// Source to harvest from: git (default), claude-code, or failures
    #[arg(long, default_value = "git")]
    pub source: String,

    /// Path to Claude Code history file (default: ~/.claude/history.jsonl).
    /// Only used with --source claude-code.
    #[arg(long)]
    pub history_file: Option<std::path::PathBuf>,

    /// Only harvest sessions after this date (ISO 8601, e.g. 2026-04-01).
    /// Only used with --source claude-code.
    #[arg(long)]
    pub since: Option<String>,

    /// Confirm reading the Claude Code history file (required for --source claude-code)
    #[arg(long)]
    pub confirm: bool,

    /// Detach immediately: re-exec inkentry in the background and return.
    /// Useful in git hooks so the hook does not block the git process.
    #[arg(long, default_value_t = false)]
    pub detach: bool,
}

#[derive(Args, Debug)]
pub struct MemorySyncArgs {
    /// Local memory.db to sync (default: auto-detected memory.db)
    #[arg(long)]
    pub source: Option<std::path::PathBuf>,
    /// Include archived entries in the push (propagates tombstones)
    #[arg(long)]
    pub include_archived: bool,
    /// Cloud project slug to sync into. Required when no `project_id` is
    /// configured. On first sync the server lazily creates this project from the
    /// slug; repeat syncs with the same slug reuse it. The slug is never
    /// auto-derived from the folder or git remote (project-taxonomy).
    #[arg(long)]
    pub project: Option<String>,
}

#[derive(Args, Debug)]
pub struct MemoryArchiveArgs {
    /// ID of the entry to archive (from `inkentry memory list`)
    pub id: NoteId,
}

#[derive(Args, Debug)]
pub struct MemorySupersededArgs {
    /// ID of the entry to archive (the outdated one)
    pub old_id: NoteId,
    /// ID of the entry that replaces it (the new one)
    pub new_id: NoteId,
}

#[derive(Args, Debug)]
pub struct MemoryFailuresArgs {
    /// Number of entries to show
    #[arg(short, long, default_value = "20")]
    pub limit: usize,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Return only entries valid at this point in time (ISO 8601)
    #[arg(long, value_name = "DATE")]
    pub as_of: Option<String>,
}

#[derive(Args, Debug)]
pub struct MemoryReconcileArgs {
    /// Path to the source server.db (default: ~/.local/state/inkentry/server.db).
    /// Named --source-db to avoid conflicting with the global --db (memory.db path).
    #[arg(long = "source-db")]
    pub source_db: Option<std::path::PathBuf>,

    /// Detect and report candidates without importing anything
    #[arg(long)]
    pub dry_run: bool,

    /// Reconcile every project slug found in server.db (default: active project only)
    #[arg(long)]
    pub all_projects: bool,

    /// Output format: text or json (NDJSON summary object)
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct MemoryReindexArgs {
    /// Re-embed every active note, replacing existing vectors (e.g. after a model or dimension change)
    #[arg(long)]
    pub force: bool,

    /// Also re-embed archived notes (default: active notes only)
    #[arg(long)]
    pub include_archived: bool,

    /// Report how many notes would be embedded, then exit without writing anything
    #[arg(long)]
    pub dry_run: bool,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct MemoryDedupeArgs {
    /// Detect and report duplicate entity_id groups without collapsing anything
    #[arg(long)]
    pub dry_run: bool,

    /// Output format: text or json (NDJSON summary object)
    #[arg(long, default_value = "text")]
    pub format: String,
}

use super::status::format_age;

mod add;
pub(crate) mod anchor;
mod archive;
mod corpus;
pub(crate) mod cross_project;
mod dedupe;
mod failures;
mod graph_cmd;
mod harvest;
mod harvest_claude;
mod list;
pub(crate) mod outbox;
pub(crate) mod reconcile;
pub(crate) mod reindex;
mod resolve;
mod show;
mod supersede;
pub mod sync;
mod tags;
mod timeline;

pub(crate) use corpus::{MemoryCorpus, memory_corpus_search};

pub async fn memory(args: MemoryArgs, cfg: crate::config::Config) -> Result<()> {
    cfg.validate()?;
    let be = backend_override(&args.backend);
    // Hook plumbing (ADR-099): never errors, never prints to stdout, whatever
    // is wrong with the project — a git hook's exit code gates the commit.
    if let MemoryCommand::Anchor(a) = args.command {
        if let Ok((mem_path, false)) = resolve_store_path(args.db.clone(), false, &cfg, be).await {
            let _ = anchor::memory_anchor(a, &mem_path).await;
        }
        return Ok(());
    }
    let (mem_path, pre_init_notes) = resolve_memory_store(&args, &cfg, be).await?;
    match args.command {
        MemoryCommand::Add(a) => add::memory_add(a, &mem_path, &cfg, be, pre_init_notes).await,
        MemoryCommand::List(a) => list::memory_list(a, &mem_path, &cfg, be, pre_init_notes).await,
        MemoryCommand::Show(a) => show::memory_show(a, &mem_path, &cfg, be).await,
        MemoryCommand::Harvest(a) => {
            // Only the alias warns; the top-level `inkentry harvest` shares the
            // handler silently.
            eprintln!(
                "warning: 'inkentry memory harvest' is deprecated; use 'inkentry harvest' instead."
            );
            harvest::memory_harvest(a, &mem_path, &cfg, be).await
        }
        MemoryCommand::Archive(a) => archive::memory_archive(a, &mem_path, &cfg, be).await,
        MemoryCommand::Supersede(a) => supersede::memory_supersede(a, &mem_path, &cfg, be).await,
        MemoryCommand::Sync(a) => sync::memory_sync(a, &mem_path, &cfg).await,
        MemoryCommand::Timeline(a) => timeline::memory_timeline(a, &mem_path, &cfg, be).await,
        MemoryCommand::Graph(a) => graph_cmd::memory_graph(a, &mem_path, &cfg, be).await,
        MemoryCommand::Failures(a) => failures::memory_failures(a, &mem_path, &cfg, be).await,
        MemoryCommand::Reconcile(a) => reconcile::memory_reconcile(a, &mem_path, &cfg).await,
        MemoryCommand::Reindex(a) => {
            reindex::memory_reindex(a, &mem_path, &cfg, be, reindex::Summary::Printed).await
        }
        MemoryCommand::Dedupe(a) => dedupe::memory_dedupe(a, &mem_path).await,
        MemoryCommand::Tags(a) => tags::memory_tags(a, &mem_path).await,
        // Handled and returned above, before `mem_path` was even resolved.
        MemoryCommand::Anchor(_) => Ok(()),
    }
}

fn backend_override(s: &str) -> Option<&'static str> {
    match s {
        "git-notes" => Some("git-notes"),
        _ => None,
    }
}

async fn resolve_memory_store(
    args: &MemoryArgs,
    cfg: &crate::config::Config,
    be: Option<&'static str>,
) -> Result<(PathBuf, bool)> {
    // Only `add`/`list` may ride the git-notes carrier; every other subcommand,
    // harvest included, fails closed without a local project.
    let allow_pre_init_carrier =
        matches!(args.command, MemoryCommand::Add(_) | MemoryCommand::List(_));
    resolve_store_path(args.db.clone(), allow_pre_init_carrier, cfg, be).await
}

// Shared with the top-level `inkentry harvest` so both pick the store identically.
pub(crate) async fn resolve_store_path(
    db: Option<PathBuf>,
    allow_pre_init_carrier: bool,
    cfg: &crate::config::Config,
    be: Option<&'static str>,
) -> Result<(PathBuf, bool)> {
    use crate::config::SyncMode;

    if let Some(p) = db {
        return Ok((p, false));
    }
    match crate::config::require_project_db(&cfg.db_path, false) {
        Ok(p) => return Ok((p.with_file_name("memory.db"), false)),
        Err(e) => {
            if !allow_pre_init_carrier {
                return Err(e);
            }
        }
    }
    // An explicit CloudFirst `server_url` still owns the store and wins over the
    // carrier; `open_memory_backend` routes remote from this placeholder path.
    if cfg.resolve_mode() == SyncMode::CloudFirst && cfg.server_url.is_some() {
        return Ok((cfg.db_path.with_file_name("memory.db"), false));
    }
    // Placeholder path the pre-init callers never open. Explicit `--backend
    // git-notes` already makes notes the primary store, so it is not carrier
    // mode (which would double-write).
    if git_head_reachable().await {
        return Ok((
            cfg.db_path.with_file_name("memory.db"),
            be != Some("git-notes"),
        ));
    }
    anyhow::bail!(
        "no inkentry project here, and not inside a git repo. \
         Run 'inkentry init' first, or run inside a git repository."
    )
}

pub(super) async fn run_harvest(
    args: MemoryHarvestArgs,
    db: Option<PathBuf>,
    backend: &str,
    cfg: &crate::config::Config,
) -> Result<()> {
    cfg.validate()?;
    let be = backend_override(backend);
    let (mem_path, _pre_init_notes) = resolve_store_path(db, false, cfg, be).await?;
    harvest::memory_harvest(args, &mem_path, cfg, be).await
}

// An empty repo with no commits fails this: `git rev-parse HEAD` errors.
async fn git_head_reachable() -> bool {
    use std::process::Stdio;
    tokio::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

pub(super) fn print_note_summary(n: &crate::storage::memory::Note) {
    let dist = if let Some(s) = n.score {
        format!("  score: {s:.4}")
    } else {
        n.distance
            .map(|d| format!("  dist: {d:.4}"))
            .unwrap_or_default()
    };
    let archived_badge = if n.status == "archived" {
        " \x1b[31m[archived]\x1b[0m"
    } else {
        ""
    };
    let source_badge = n
        .source_project
        .as_deref()
        .map(|p| format!("  \x1b[36m[from: {p}]\x1b[0m"))
        .unwrap_or_default();
    cprintln!(
        "\x1b[1m#{id}\x1b[0m  \x1b[33m[{kind}]\x1b[0m  {title}{archived}{dist_fmt}{source}",
        id = crate::storage::entity_id_handle(&n.entity_id),
        kind = n.kind,
        title = n.title,
        archived = archived_badge,
        dist_fmt = if dist.is_empty() {
            String::new()
        } else {
            format!("\x1b[2m{dist}\x1b[0m")
        },
        source = source_badge,
    );
    cprintln!("     \x1b[2m{}\x1b[0m", format_age(n.created_at));
    if let Some(valid_at) = n.valid_at {
        cprintln!("     \x1b[2mvalid_at: {}\x1b[0m", format_age(valid_at));
    }
    if !n.tags.is_empty() {
        println!("     tags: {}", n.tags.join(", "));
    }
    if !n.linked_files.is_empty() {
        println!("     files: {}", n.linked_files.join(", "));
    }
    if let Some(sup) = &n.superseded_by {
        cprintln!("     \x1b[2msuperseded by #{sup}\x1b[0m");
    }
    if !matches!(n.kind.as_str(), "question" | "answer") {
        let preview: Vec<&str> = n.body.lines().take(2).collect();
        for line in &preview {
            cprintln!("     \x1b[2m{line}\x1b[0m");
        }
        if n.body.lines().count() > 2 {
            cprintln!("     \x1b[2m…\x1b[0m");
        }
    } else {
        cprintln!(
            "     \x1b[2m(use `inkentry memory show {}` to read body)\x1b[0m",
            n.id
        );
    }
    println!();
}

// Separate from `open_editor_for_body` so tests can create a draft without
// spawning an editor.
fn create_draft_file(title: &str) -> Result<tempfile::NamedTempFile> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("inkentry_memory_").suffix(".md");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o600));
    }
    let mut file = builder
        .tempfile()
        .context("failed to create temporary draft file")?;

    use std::io::Write;
    write!(
        file,
        "# {title}\n\n\
         # Write your memory entry below. Lines starting with # are ignored.\n\
         # Save and close the editor when done.\n\n"
    )?;
    file.flush()?;
    Ok(file)
}

pub(super) fn open_editor_for_body(title: &str) -> Result<String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());

    // Read back through the retained handle, not by re-opening the path, so a
    // symlink swapped in during the edit can't redirect the read.
    let mut tmp = create_draft_file(title)?;
    let tmp_path = tmp.path().to_path_buf();

    let status = std::process::Command::new(&editor)
        .arg(&tmp_path)
        .status()
        .with_context(|| format!("could not open editor '{editor}'"))?;

    let content = {
        use std::io::{Read, Seek, SeekFrom};
        // The editor wrote via the path, so rewind our fd before reading.
        tmp.seek(SeekFrom::Start(0))
            .context("failed to seek draft file for read-back")?;
        let mut buf = String::new();
        tmp.read_to_string(&mut buf)
            .context("failed to read draft file back through the retained handle")?;
        buf
    };

    if !status.success() {
        anyhow::bail!("Editor exited with a non-zero status; entry not saved.");
    }

    let body: String = content
        .lines()
        .filter(|l| !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    if body.is_empty() {
        anyhow::bail!("Body is empty; entry not saved.");
    }
    Ok(body)
}

pub(super) use crate::utils::dates::parse_as_of;

pub(super) fn backend_err(e: anyhow::Error) -> anyhow::Error {
    if e.downcast_ref::<crate::error::InkentryError>()
        .is_some_and(|s| matches!(s, crate::error::InkentryError::BackendUnsupported(_)))
    {
        anyhow::anyhow!(
            "This operation requires the sqlite backend. \
             Re-run without --backend git-notes."
        )
    } else {
        e
    }
}

#[cfg(test)]
mod draft_file_tests {
    use super::create_draft_file;

    #[test]
    fn round_trip_content_is_readable() {
        let file = create_draft_file("My Title").expect("draft file should be created");
        let content = std::fs::read_to_string(file.path()).expect("draft file should be readable");
        assert!(content.contains("# My Title"));
        assert!(content.contains("Write your memory entry below"));
    }

    #[test]
    fn draft_path_has_md_suffix() {
        let file = create_draft_file("t").expect("draft file should be created");
        assert_eq!(file.path().extension().and_then(|e| e.to_str()), Some("md"));
    }

    #[cfg(unix)]
    #[test]
    fn draft_file_mode_is_0600() {
        use std::os::unix::fs::PermissionsExt;

        let file = create_draft_file("t").expect("draft file should be created");
        let mode = std::fs::metadata(file.path())
            .expect("draft file should have metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "draft file must be owner-only");
    }

    #[cfg(unix)]
    #[test]
    fn preexisting_symlink_at_guessed_path_is_not_clobbered() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir should be created");
        let victim = dir.path().join("victim.md");
        std::fs::write(&victim, "victim contents\n").expect("victim file should be writable");

        let guessed = dir
            .path()
            .join(format!("ca_memory_{}.md", std::process::id()));
        symlink(&victim, &guessed).expect("symlink should be created");

        let file = create_draft_file("t").expect("draft file should be created");
        assert_ne!(
            file.path(),
            guessed.as_path(),
            "draft must not reuse the old predictable path"
        );

        let victim_contents =
            std::fs::read_to_string(&victim).expect("victim file should still be readable");
        assert_eq!(
            victim_contents, "victim contents\n",
            "pre-existing symlink target must not be clobbered"
        );
        assert!(
            std::fs::symlink_metadata(&guessed)
                .expect("guessed path should still be a symlink")
                .file_type()
                .is_symlink(),
            "pre-existing symlink itself must be left alone"
        );
    }

    #[test]
    fn draft_file_is_removed_on_drop_after_simulated_success_path() {
        let file = create_draft_file("t").expect("draft file should be created");
        let path = file.path().to_path_buf();
        assert!(
            path.exists(),
            "draft should exist immediately after creation"
        );

        let _content = std::fs::read_to_string(&path).expect("draft should be readable");
        drop(file);

        assert!(
            !path.exists(),
            "draft file must be deleted once the NamedTempFile guard drops (success path)"
        );
    }

    #[test]
    fn draft_file_is_removed_on_drop_after_simulated_editor_failure_path() {
        let file = create_draft_file("t").expect("draft file should be created");
        let path = file.path().to_path_buf();
        assert!(
            path.exists(),
            "draft should exist immediately after creation"
        );

        let result: anyhow::Result<()> = (|| {
            anyhow::bail!("Editor exited with a non-zero status; entry not saved.");
        })();
        assert!(result.is_err());
        drop(file);

        assert!(
            !path.exists(),
            "draft file must be deleted even when the editor-failure path is taken"
        );
    }

    #[cfg(unix)]
    #[test]
    fn handle_based_read_back_ignores_a_post_creation_symlink_swap() {
        use std::io::{Read, Seek, SeekFrom};
        use std::os::unix::fs::symlink;

        let mut file = create_draft_file("t").expect("draft file should be created");
        let tmp_path = file.path().to_path_buf();

        let dir = tmp_path.parent().unwrap();
        let victim = dir.join("attacker_victim.md");
        std::fs::write(&victim, "ATTACKER-CONTROLLED CONTENT\n")
            .expect("victim file should be writable");

        std::fs::remove_file(&tmp_path).expect("should be able to remove the draft for the PoC");
        symlink(&victim, &tmp_path).expect("symlink should be created at the draft's old path");

        // Control: a path-based read follows the symlink.
        let path_based_content = std::fs::read_to_string(&tmp_path)
            .expect("path-based read-back follows the symlink (control demonstrates the gap)");
        assert_eq!(
            path_based_content, "ATTACKER-CONTROLLED CONTENT\n",
            "control: path-based read-back should still be shown to follow the swapped symlink"
        );

        file.seek(SeekFrom::Start(0))
            .expect("seek on retained handle should succeed");
        let mut handle_based_content = String::new();
        file.read_to_string(&mut handle_based_content).expect(
            "handle-based read-back should succeed even though the path now points elsewhere",
        );

        assert_ne!(
            handle_based_content, "ATTACKER-CONTROLLED CONTENT\n",
            "handle-based read-back must NOT observe the attacker's swapped-in content"
        );
        assert!(
            handle_based_content.contains("# t"),
            "handle-based read-back should still see the original draft content \
             (the title header written at creation), proving it reads through \
             the original fd rather than the swapped path: got {handle_based_content:?}"
        );

        let _ = std::fs::remove_file(&tmp_path);
    }
}

#[cfg(test)]
mod add_embed_tests;
