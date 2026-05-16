use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

use super::memory::print_note_summary;
use crate::storage::memory::Note;
use crate::{config::Config, storage::open_memory_backend};

/// Fallback per-section limit when `--kind` names a kind not in SECTIONS.
const DEFAULT_UNKNOWN_KIND_LIMIT: usize = 20;

/// Agent-facing entry-point command: pull the most relevant memory sections
/// in one shot (handoffs → questions → decisions → requirements).
#[derive(Args, Debug)]
pub struct ContextArgs {
    /// Path to the memory database (overrides auto-detect)
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Storage backend: sqlite (default), git-meta, or git-notes
    #[arg(long, default_value = "sqlite", value_name = "BACKEND")]
    pub backend: String,

    /// Filter to a specific kind instead of the default multi-section view
    #[arg(short, long, value_name = "KIND")]
    pub kind: Option<String>,

    /// Maximum entries per section (defaults: handoff=3, question=500, decision=10, requirement=500)
    #[arg(short, long, value_name = "N")]
    pub limit: Option<usize>,

    /// Only show entries tagged with this file or directory path
    #[arg(long, value_name = "PATH")]
    pub path: Option<String>,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

struct Section {
    kind: &'static str,
    /// Fetch this many entries before optional path post-filter; 500 is the NoteStore hard-cap.
    default_limit: usize,
}

const SECTIONS: &[Section] = &[
    Section {
        kind: "handoff",
        default_limit: 3,
    },
    Section {
        kind: "question",
        default_limit: 500,
    },
    Section {
        kind: "decision",
        default_limit: 10,
    },
    Section {
        kind: "requirement",
        default_limit: 500,
    },
];

pub async fn context(args: ContextArgs, cfg: Config) -> Result<()> {
    cfg.validate()?;
    let mem_path = args.db.clone().unwrap_or_else(|| {
        crate::config::resolve_db(None, &cfg.db_path).with_file_name("memory.db")
    });
    let be = match args.backend.as_str() {
        "git-meta" => Some("git-meta"),
        "git-notes" => Some("git-notes"),
        _ => None,
    };
    let backend = open_memory_backend(&cfg, &mem_path, be)?;

    let sections = collect_sections(
        &*backend,
        args.kind.as_deref(),
        args.limit,
        args.path.as_deref(),
    )
    .await?;

    match crate::utils::effective_format(&args.format) {
        "json" => println!("{}", serde_json::to_string(&sections)?),
        _ => {
            for (kind, notes) in &sections {
                if notes.is_empty() {
                    continue;
                }
                print_section_header(kind);
                for n in notes {
                    print_note_summary(n);
                }
            }
        }
    }
    Ok(())
}

async fn collect_sections(
    backend: &dyn crate::storage::MemoryBackend,
    kind_filter: Option<&str>,
    limit_override: Option<usize>,
    path_filter: Option<&str>,
) -> Result<Vec<(String, Vec<Note>)>> {
    let mut result = Vec::new();

    let sections: Vec<(&str, usize)> = if let Some(k) = kind_filter {
        let default_limit = SECTIONS
            .iter()
            .find(|s| s.kind == k)
            .map(|s| s.default_limit)
            .unwrap_or(DEFAULT_UNKNOWN_KIND_LIMIT);
        vec![(k, limit_override.unwrap_or(default_limit))]
    } else {
        SECTIONS
            .iter()
            .map(|s| (s.kind, limit_override.unwrap_or(s.default_limit)))
            .collect()
    };

    for (kind, limit) in sections {
        let mut notes = backend.list(Some(kind), limit, false, None).await?;
        if let Some(p) = path_filter {
            notes.retain(|n| n.linked_files.iter().any(|f| f.contains(p)));
        }
        result.push((kind.to_string(), notes));
    }
    Ok(result)
}

fn print_section_header(kind: &str) {
    let label = match kind {
        "handoff" => "Handoffs",
        "question" => "Open questions",
        "decision" => "Decisions",
        "requirement" => "Requirements",
        other => other,
    };
    println!("\x1b[1;34m── {label} \x1b[0m");
    println!();
}
