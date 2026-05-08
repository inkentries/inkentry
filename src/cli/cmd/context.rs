use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

use super::memory::print_note_summary;
use crate::storage::memory::Note;
use crate::{config::Config, storage::open_memory_backend};

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

    /// Maximum entries per section (defaults: handoff=3, question=all, decision=10, requirement=all)
    #[arg(short, long, value_name = "N")]
    pub limit: Option<usize>,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

struct Section {
    kind: &'static str,
    default_limit: usize,
}

const SECTIONS: &[Section] = &[
    Section {
        kind: "handoff",
        default_limit: 3,
    },
    Section {
        kind: "question",
        default_limit: 1000,
    },
    Section {
        kind: "decision",
        default_limit: 10,
    },
    Section {
        kind: "requirement",
        default_limit: 1000,
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

    match crate::utils::effective_format(&args.format) {
        "json" => {
            let sections = collect_sections(&*backend, args.kind.as_deref(), args.limit).await?;
            println!("{}", serde_json::to_string_pretty(&sections)?);
        }
        _ => {
            let sections = collect_sections(&*backend, args.kind.as_deref(), args.limit).await?;
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
) -> Result<Vec<(String, Vec<Note>)>> {
    let mut result = Vec::new();

    let sections: Vec<(&str, usize)> = if let Some(k) = kind_filter {
        let default_limit = SECTIONS
            .iter()
            .find(|s| s.kind == k)
            .map(|s| s.default_limit)
            .unwrap_or(20);
        vec![(k, limit_override.unwrap_or(default_limit))]
    } else {
        SECTIONS
            .iter()
            .map(|s| (s.kind, limit_override.unwrap_or(s.default_limit)))
            .collect()
    };

    for (kind, limit) in sections {
        let notes = backend.list(Some(kind), limit, false, None).await?;
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
