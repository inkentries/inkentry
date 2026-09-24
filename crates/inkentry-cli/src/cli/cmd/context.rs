use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

use super::color::cprintln;
use super::memory::cross_project::collect_dep_cross_cutting;
use super::memory::print_note_summary;
use crate::storage::memory::Note;
use crate::{
    config::Config,
    storage::{NoteId, open_memory_backend},
};

const DEFAULT_UNKNOWN_KIND_LIMIT: usize = 20;

// Session-scoped kinds (handoff, question, intent) never cross projects.
const DEP_PASS_KINDS: &[&str] = &["decision", "requirement"];

/// Agent-facing entry-point command: pull the most relevant memory sections
/// in one shot (handoffs → questions → decisions → requirements).
/// Appends a "conventions" section from the local index when available.
#[derive(Args, Debug)]
pub struct ContextArgs {
    /// Path to the memory database (overrides auto-detect)
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Path to the inkentry index database (overrides auto-detect).
    /// Used to load the conventions section.
    #[arg(long, value_name = "INDEX_DB")]
    pub index_db: Option<PathBuf>,

    /// Storage backend: sqlite (default) or git-notes
    #[arg(long, default_value = "sqlite", value_name = "BACKEND")]
    pub backend: String,

    /// Filter to a specific kind instead of the default multi-section view
    #[arg(short, long, value_name = "KIND")]
    pub kind: Option<String>,

    /// Maximum entries per section (defaults: handoff=3, question=10, decision=10, requirement=10)
    #[arg(short, long, value_name = "N", conflicts_with = "budget")]
    pub limit: Option<usize>,

    /// Cap total output to this many tokens (safety net independent of entry
    /// count; mirrors `search --budget`). Mutually exclusive with --limit.
    #[arg(
        long,
        visible_alias = "max-tokens",
        value_name = "N",
        conflicts_with = "limit"
    )]
    pub budget: Option<usize>,

    /// Only show entries tagged with this file or directory path (substring match)
    #[arg(long, value_name = "PATH")]
    pub path: Option<String>,

    /// Only show entries carrying this exact tag, normalised the same way a
    /// write is (ADR-101 D4)
    #[arg(long, value_name = "TAG")]
    pub tag: Option<String>,

    /// Only show entries linking this exact repository-relative path
    /// (ADR-101 D4) — an exact match, unlike `--path`'s substring match
    #[arg(long, value_name = "PATH")]
    pub file: Option<String>,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Skip the conventions section (default: false)
    #[arg(long)]
    pub no_conventions: bool,

    /// Query only the local project's memory, skipping linked project stores
    #[arg(long)]
    pub local_only: bool,
}

struct Section {
    kind: &'static str,
    // Small on purpose: `context` runs every session and compaction.
    default_limit: usize,
}

const SECTIONS: &[Section] = &[
    // The roster of other live sessions is the only time-sensitive item at
    // session start, so it is displayed first.
    Section {
        kind: "intent",
        default_limit: 20,
    },
    Section {
        kind: "handoff",
        default_limit: 3,
    },
    Section {
        kind: "question",
        default_limit: 10,
    },
    Section {
        kind: "decision",
        default_limit: 10,
    },
    Section {
        kind: "requirement",
        default_limit: 10,
    },
];

fn section_limit(kind: &str, limit_override: Option<usize>) -> usize {
    let default = SECTIONS
        .iter()
        .find(|s| s.kind == kind)
        .map(|s| s.default_limit)
        .unwrap_or(DEFAULT_UNKNOWN_KIND_LIMIT);
    limit_override.unwrap_or(default)
}

fn note_tokens(n: &Note) -> usize {
    crate::search::tokens::estimate_tokens(&n.title)
        + crate::search::tokens::estimate_tokens(&n.body)
}

fn cap_sections(sections: &mut [(String, Vec<Note>)], limit_override: Option<usize>) {
    for (kind, notes) in sections.iter_mut() {
        notes.truncate(section_limit(kind, limit_override));
    }
}

// Budget packing order, independent of display order: the ephemeral intent
// roster of other sessions drops first so it never crowds out durable memory.
const PACK_PRIORITY: &[&str] = &["decision", "requirement", "handoff", "question", "intent"];

fn apply_budget(
    sections: &mut [(String, Vec<Note>)],
    conventions: &mut Vec<crate::conventions::ConventionRecord>,
    budget: usize,
) -> usize {
    let mut remaining = budget;
    let mut fits = |tc: usize| {
        if tc <= remaining {
            remaining -= tc;
            true
        } else {
            false
        }
    };
    for kind in PACK_PRIORITY {
        if let Some((_, notes)) = sections.iter_mut().find(|(k, _)| k == kind) {
            notes.retain(|n| fits(note_tokens(n)));
        }
    }
    // Kinds outside PACK_PRIORITY pack afterwards, in existing order.
    for (kind, notes) in sections.iter_mut() {
        if PACK_PRIORITY.contains(&kind.as_str()) {
            continue;
        }
        notes.retain(|n| fits(note_tokens(n)));
    }
    conventions.retain(|c| {
        fits(
            crate::search::tokens::estimate_tokens(&c.category)
                + crate::search::tokens::estimate_tokens(&c.description),
        )
    });
    budget - remaining
}

pub async fn context(args: ContextArgs, cfg: Config) -> Result<()> {
    // Recorded once here; every exit point below records against this
    // instant (ADR-098 D5).
    let started = std::time::Instant::now();
    cfg.validate()?;
    // Fail closed without a local `.inkentry/` project rather than use the global
    // store; `--db` is exempt.
    let mem_path = match args.db.clone() {
        Some(p) => p,
        None => crate::config::require_project_db(&cfg.db_path, false)?.with_file_name("memory.db"),
    };

    let be = match args.backend.as_str() {
        "git-notes" => Some("git-notes"),
        _ => None,
    };

    // A teammate's fetched notes sit on a tracking ref that nothing else merges
    // into `memory.db`.
    crate::cli::cmd::memory::reconcile::refresh_read_path_from_git_notes(&cfg, &mem_path, be).await;

    crate::cli::cmd::memory::reconcile::maybe_emit_nudge(&mem_path, &cfg);
    crate::cli::cmd::memory::outbox::poll_and_apply(&cfg, &mem_path).await;
    let backend = open_memory_backend(&cfg, &mem_path, be).await?;

    let mut sections = collect_sections(
        &*backend,
        args.kind.as_deref(),
        args.limit,
        args.path.as_deref(),
        args.tag.as_deref(),
        args.file.as_deref(),
    )
    .await?;

    if !args.local_only {
        let index_db_path = args
            .index_db
            .clone()
            .unwrap_or_else(|| crate::config::resolve_db(None, &cfg.db_path));
        let mut seen: std::collections::HashSet<(String, NoteId)> = Default::default();
        // Seeded with local notes so a dep note sharing a local id is skipped.
        for (_, notes) in &sections {
            for n in notes {
                seen.insert((String::new(), n.id.clone()));
            }
        }
        let dep_notes = collect_dep_cross_cutting(&index_db_path, &mut seen).await;

        for dep_note in dep_notes {
            let kind = dep_note.kind.clone();
            if !DEP_PASS_KINDS.contains(&kind.as_str()) {
                continue;
            }
            if let Some(ref kf) = args.kind
                && &kind != kf
            {
                continue;
            }
            if let Some((_, notes)) = sections.iter_mut().find(|(k, _)| k == &kind) {
                notes.push(dep_note);
            }
        }
    }

    // Cross-project appends must not exceed the per-section limit.
    cap_sections(&mut sections, args.limit);

    // Computed before `--budget` packing so dropping the roster can never drop a
    // warning.
    let overlaps = compute_overlaps(&sections);

    let mut conventions: Vec<crate::conventions::ConventionRecord> =
        if !args.no_conventions && args.kind.is_none() {
            load_conventions(args.index_db.as_deref(), &cfg)
        } else {
            vec![]
        };

    let budget_used = args
        .budget
        .map(|b| apply_budget(&mut sections, &mut conventions, b));

    let returned_ids: Vec<String> = sections
        .iter()
        .flat_map(|(_, notes)| notes.iter().map(|n| n.entity_id.clone()))
        .collect();
    let memory_results = returned_ids.len() as i64;
    let tokens_out = budget_used.unwrap_or_else(|| {
        sections
            .iter()
            .flat_map(|(_, notes)| notes.iter().map(note_tokens))
            .sum::<usize>()
            + conventions
                .iter()
                .map(|c| {
                    crate::search::tokens::estimate_tokens(&c.category)
                        + crate::search::tokens::estimate_tokens(&c.description)
                })
                .sum::<usize>()
    }) as i64;

    match crate::utils::effective_format(&args.format) {
        "json" => {
            let mut output = serde_json::json!({
                "sections": sections,
                "conventions": conventions,
                "overlaps": overlaps,
            });
            if let (Some(budget), Some(used)) = (args.budget, budget_used) {
                output["token_budget"] = budget.into();
                output["tokens_used"] = used.into();
                output["tokens_remaining"] = (budget - used).into();
            }
            println!("{output}");
        }
        _ => {
            for (kind, notes) in &sections {
                if kind == "intent" {
                    if overlaps.is_empty() && notes.is_empty() {
                        continue;
                    }
                    print_section_header(kind);
                    for file in &overlaps {
                        print_overlap_warning(file);
                    }
                    for n in notes {
                        print_note_summary(n);
                    }
                    continue;
                }
                if notes.is_empty() {
                    continue;
                }
                print_section_header(kind);
                for n in notes {
                    print_note_summary(n);
                }
            }
            if !conventions.is_empty() {
                print_conventions_section(&conventions);
            }
            if let (Some(budget), Some(used)) = (args.budget, budget_used) {
                println!("tokens used: {used}/{budget}");
            }
        }
    }
    super::events::record(
        &cfg,
        &mem_path,
        None,
        "context",
        None,
        Some(memory_results),
        &returned_ids,
        Some(tokens_out),
        started,
        true,
    );
    Ok(())
}

fn load_conventions(
    index_db_override: Option<&std::path::Path>,
    cfg: &Config,
) -> Vec<crate::conventions::ConventionRecord> {
    let db_path = if let Some(p) = index_db_override {
        p.to_path_buf()
    } else {
        crate::config::resolve_db(None, &cfg.db_path)
    };
    if !db_path.exists() {
        return vec![];
    }
    match crate::storage::Database::open(&db_path) {
        Ok(db) => crate::conventions::list_conventions(&db, None).unwrap_or_default(),
        Err(e) => {
            tracing::debug!("conventions: could not open index db: {e}");
            vec![]
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn collect_sections(
    backend: &dyn crate::storage::MemoryBackend,
    kind_filter: Option<&str>,
    limit_override: Option<usize>,
    path_filter: Option<&str>,
    tag_filter: Option<&str>,
    file_filter: Option<&str>,
) -> Result<Vec<(String, Vec<Note>)>> {
    let mut result = Vec::new();

    let sections: Vec<(&str, usize)> = if let Some(k) = kind_filter {
        vec![(k, section_limit(k, limit_override))]
    } else {
        SECTIONS
            .iter()
            .map(|s| (s.kind, section_limit(s.kind, limit_override)))
            .collect()
    };

    // Filters run over the fetched notes, which is cheap at these small limits.
    let normalised_tag = tag_filter.map(crate::storage::normalize_tag);
    for (kind, limit) in sections {
        let mut notes = backend.list(Some(kind), limit, false, None).await?;
        if let Some(p) = path_filter {
            notes.retain(|n| n.linked_files.iter().any(|f| f.contains(p)));
        }
        if let Some(tag) = &normalised_tag {
            notes.retain(|n| {
                tag.as_deref()
                    .is_some_and(|t| n.tags.iter().any(|nt| nt == t))
            });
        }
        if let Some(f) = file_filter {
            notes.retain(|n| n.linked_files.iter().any(|nf| nf == f));
        }
        result.push((kind.to_string(), notes));
    }
    Ok(result)
}

fn print_section_header(kind: &str) {
    let label = match kind {
        "intent" => "Active agent sessions",
        "handoff" => "Handoffs",
        "question" => "Open questions",
        "decision" => "Decisions",
        "requirement" => "Requirements",
        other => other,
    };
    cprintln!("\x1b[1;34m── {label} \x1b[0m");
    println!();
}

fn compute_overlaps(sections: &[(String, Vec<Note>)]) -> Vec<String> {
    let intent_files: std::collections::HashSet<&str> = sections
        .iter()
        .find(|(k, _)| k == "intent")
        .map(|(_, notes)| {
            notes
                .iter()
                .flat_map(|n| n.linked_files.iter().map(String::as_str))
                .collect()
        })
        .unwrap_or_default();
    // Skips git discovery when no intent lists a file, so a solo dev pays nothing.
    if intent_files.is_empty() {
        return Vec::new();
    }
    let mut overlaps: Vec<String> = crate::utils::worktree_modified_files()
        .into_iter()
        .filter(|f| intent_files.contains(f.as_str()))
        .collect();
    overlaps.sort();
    overlaps.dedup();
    overlaps
}

// Docs and agents match on this wording; printed without ANSI so it holds under
// any color policy.
fn print_overlap_warning(file: &str) {
    println!("⚠  Overlap: {file} is listed in an active intent");
}

fn print_conventions_section(records: &[crate::conventions::ConventionRecord]) {
    cprintln!("\x1b[1;34m── Conventions \x1b[0m");
    println!();

    let mut by_lang: std::collections::BTreeMap<&str, Vec<&crate::conventions::ConventionRecord>> =
        std::collections::BTreeMap::new();
    for r in records {
        by_lang.entry(r.language.as_str()).or_default().push(r);
    }
    for (lang, recs) in &by_lang {
        cprintln!("\x1b[1m{lang}\x1b[0m");
        for r in recs {
            println!(
                "  [{:.0}%] {} — {}",
                r.confidence * 100.0,
                r.category,
                r.description
            );
        }
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conventions::ConventionRecord;
    use clap::Parser;

    fn test_id(n: i64) -> NoteId {
        format!("test-entry-{n:04}")
            .parse()
            .expect("a non-empty id")
    }

    fn note(id: i64, kind: &str, title: &str, body: &str) -> Note {
        Note {
            id: test_id(id),
            entity_id: crate::storage::entity_id(kind, title, body),
            kind: kind.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            tags: vec![],
            linked_files: vec![],
            created_at: 0,
            status: "active".to_string(),
            superseded_by: None,
            source_ref: None,
            valid_at: None,
            invalid_at: None,
            distance: None,
            score: None,
            source_project: None,
            source_project_path: None,
            remote_id: None,
            origin: None,
        }
    }

    // Membership, not equality: the default view selects on a subset of the
    // canonical kinds.
    #[test]
    fn every_retrieval_kind_is_a_canonical_kind() {
        use inkentry_core::storage::is_valid_note_kind;
        for section in SECTIONS {
            assert!(
                is_valid_note_kind(section.kind),
                "context section kind {:?} is not a canonical memory kind",
                section.kind
            );
        }
        for kind in PACK_PRIORITY {
            assert!(
                is_valid_note_kind(kind),
                "pack-priority kind {kind:?} is not a canonical memory kind"
            );
        }
        // `memory failures` selects on this kind; it must be canonical too.
        assert!(is_valid_note_kind("antipattern"));
    }

    #[test]
    fn default_limits_are_small_for_every_section() {
        assert_eq!(section_limit("handoff", None), 3);
        assert_eq!(section_limit("question", None), 10);
        assert_eq!(section_limit("decision", None), 10);
        assert_eq!(section_limit("requirement", None), 10);
        assert_eq!(section_limit("intent", None), 20);
        assert_eq!(section_limit("mystery", None), DEFAULT_UNKNOWN_KIND_LIMIT);
    }

    #[test]
    fn explicit_limit_override_wins() {
        assert_eq!(section_limit("question", Some(42)), 42);
        assert_eq!(section_limit("mystery", Some(1)), 1);
    }

    #[test]
    fn cap_sections_bounds_cross_project_appends() {
        let mut sections = vec![(
            "decision".to_string(),
            (0..25).map(|i| note(i, "decision", "t", "b")).collect(),
        )];
        cap_sections(&mut sections, None);
        assert_eq!(sections[0].1.len(), 10);
        assert_eq!(sections[0].1.first().unwrap().id, test_id(0));
    }

    #[test]
    fn budget_truncates_output_to_fit() {
        let body = "x".repeat(400); // ~100 tokens
        let mut sections = vec![(
            "decision".to_string(),
            (0..5).map(|i| note(i, "decision", "ti", &body)).collect(),
        )];
        let mut conv: Vec<ConventionRecord> = vec![];
        let used = apply_budget(&mut sections, &mut conv, 250);
        // 250 tokens fits two notes of ~101 each, not a third.
        assert_eq!(sections[0].1.len(), 2);
        assert!(used <= 250);
        assert!(used > 0);
    }

    #[test]
    fn budget_zero_drops_everything() {
        let mut sections = vec![(
            "decision".to_string(),
            vec![note(0, "decision", "t", "body")],
        )];
        let mut conv: Vec<ConventionRecord> = vec![];
        let used = apply_budget(&mut sections, &mut conv, 0);
        assert!(sections[0].1.is_empty());
        assert_eq!(used, 0);
    }

    #[test]
    fn budget_also_bounds_conventions() {
        let mut sections: Vec<(String, Vec<Note>)> = vec![];
        let mut conv: Vec<ConventionRecord> = (0..5)
            .map(|_| ConventionRecord {
                language: "rust".to_string(),
                category: "naming".to_string(),
                description: "x".repeat(400),
                confidence: 0.9,
                evidence_count: 5,
                extracted_at: 0,
            })
            .collect();
        apply_budget(&mut sections, &mut conv, 250);
        assert!(conv.len() < 5);
    }

    #[derive(clap::Parser, Debug)]
    struct TestCli {
        #[command(flatten)]
        ctx: ContextArgs,
    }

    #[test]
    fn limit_and_budget_conflict_is_enforced_at_arg_level() {
        assert!(
            TestCli::try_parse_from(["inkentry", "--limit", "5", "--budget", "100"]).is_err(),
            "--limit + --budget must be rejected"
        );
        let l = TestCli::try_parse_from(["inkentry", "--limit", "5"]).expect("limit alone parses");
        assert_eq!(l.ctx.limit, Some(5));
        assert_eq!(l.ctx.budget, None);
        let b =
            TestCli::try_parse_from(["inkentry", "--budget", "100"]).expect("budget alone parses");
        assert_eq!(b.ctx.budget, Some(100));
        assert_eq!(b.ctx.limit, None);
    }

    #[test]
    fn max_tokens_alias_maps_to_budget_and_still_conflicts_with_limit() {
        let a = TestCli::try_parse_from(["inkentry", "--max-tokens", "100"]).expect("alias parses");
        assert_eq!(a.ctx.budget, Some(100));
        assert!(
            TestCli::try_parse_from(["inkentry", "--limit", "5", "--max-tokens", "100"]).is_err(),
            "alias must inherit the conflict with --limit"
        );
    }

    #[test]
    fn budget_exact_fit_keeps_all_and_uses_full_budget() {
        let body = "x".repeat(400); // 100 tokens; title "ti" -> 1 => 101 each
        let mut sections = vec![(
            "decision".to_string(),
            (0..3).map(|i| note(i, "decision", "ti", &body)).collect(),
        )];
        let mut conv: Vec<ConventionRecord> = vec![];
        let used = apply_budget(&mut sections, &mut conv, 303);
        assert_eq!(sections[0].1.len(), 3, "exact-fit budget keeps every entry");
        assert_eq!(used, 303, "tokens_used equals the full budget at exact fit");
    }

    #[test]
    fn budget_larger_than_content_keeps_everything_with_correct_used() {
        let body = "x".repeat(400); // 101 tokens each
        let mut sections = vec![(
            "decision".to_string(),
            (0..3).map(|i| note(i, "decision", "ti", &body)).collect(),
        )];
        let mut conv: Vec<ConventionRecord> = vec![];
        let used = apply_budget(&mut sections, &mut conv, 100_000);
        assert_eq!(sections[0].1.len(), 3, "generous budget emits everything");
        assert_eq!(used, 303, "used is the true sum, not the budget");
        assert!(used <= 100_000, "used never exceeds the budget");
        assert_eq!(100_000 - used, 99_697);
    }

    #[test]
    fn budget_first_and_only_entry_exceeding_emits_nothing() {
        let body = "x".repeat(800); // 200 tokens
        let mut sections = vec![(
            "decision".to_string(),
            vec![note(0, "decision", "ti", &body)],
        )];
        let mut conv: Vec<ConventionRecord> = vec![];
        let used = apply_budget(&mut sections, &mut conv, 150);
        assert!(sections[0].1.is_empty(), "an entry over budget is dropped");
        assert_eq!(used, 0, "nothing packed => zero used, never underflows");
    }

    #[test]
    fn budget_greedy_skips_oversized_then_packs_later_smaller() {
        let big = "x".repeat(800); // 200 tokens, id 0
        let small = "x".repeat(400); // 101 tokens, id 1
        let mut sections = vec![(
            "decision".to_string(),
            vec![
                note(0, "decision", "ti", &big),
                note(1, "decision", "ti", &small),
            ],
        )];
        let mut conv: Vec<ConventionRecord> = vec![];
        let used = apply_budget(&mut sections, &mut conv, 150);
        assert_eq!(sections[0].1.len(), 1);
        assert_eq!(
            sections[0].1[0].id,
            test_id(1),
            "the smaller later entry survives"
        );
        assert_eq!(used, 101);
    }

    #[test]
    fn budget_packs_by_priority_not_display_order() {
        let big = "x".repeat(800); // title(1)+body(200) = 201 tokens
        let small = "x".repeat(400); // title(1)+body(100) = 101 tokens
        let mut sections = vec![
            ("handoff".to_string(), vec![note(0, "handoff", "ti", &big)]),
            (
                "decision".to_string(),
                vec![note(1, "decision", "ti", &small)],
            ),
        ];
        let mut conv: Vec<ConventionRecord> = vec![];
        // decision (101) packs first; handoff (201) no longer fits the 100 left.
        let used = apply_budget(&mut sections, &mut conv, 201);
        assert_eq!(sections.len(), 2, "section slots and order are preserved");
        assert_eq!(sections[0].0, "handoff");
        assert!(
            sections[0].1.is_empty(),
            "handoff starved despite being displayed first"
        );
        assert_eq!(sections[1].0, "decision");
        assert_eq!(sections[1].1.len(), 1, "higher-priority decision survives");
        assert_eq!(used, 101);
    }

    #[test]
    fn budget_prioritizes_durable_over_questions() {
        let body = "x".repeat(400); // title(1)+body(100) = 101 tokens each
        let mut sections = vec![
            ("handoff".to_string(), vec![note(0, "handoff", "ti", &body)]),
            (
                "question".to_string(),
                vec![
                    note(1, "question", "ti", &body),
                    note(2, "question", "ti", &body),
                    note(3, "question", "ti", &body),
                ],
            ),
            (
                "decision".to_string(),
                vec![
                    note(4, "decision", "ti", &body),
                    note(5, "decision", "ti", &body),
                ],
            ),
            (
                "requirement".to_string(),
                vec![
                    note(6, "requirement", "ti", &body),
                    note(7, "requirement", "ti", &body),
                ],
            ),
        ];
        let mut conv: Vec<ConventionRecord> = vec![];
        // 505 = 5 * 101: everything except the three questions fits.
        let used = apply_budget(&mut sections, &mut conv, 505);
        let kinds: Vec<&str> = sections.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, ["handoff", "question", "decision", "requirement"]);
        assert_eq!(sections[0].1.len(), 1, "handoff kept (outranks question)");
        assert!(sections[1].1.is_empty(), "questions dropped first");
        assert_eq!(sections[2].1.len(), 2, "every decision survives");
        assert_eq!(sections[3].1.len(), 2, "every requirement survives");
        assert_eq!(used, 505);
    }

    #[test]
    fn budget_drops_intent_roster_before_any_other_kind() {
        let body = "x".repeat(400); // title(1)+body(100) = 101 tokens each
        let mut sections = vec![
            ("intent".to_string(), vec![note(0, "intent", "ti", &body)]),
            ("handoff".to_string(), vec![note(1, "handoff", "ti", &body)]),
            (
                "question".to_string(),
                vec![note(2, "question", "ti", &body)],
            ),
            (
                "decision".to_string(),
                vec![note(3, "decision", "ti", &body)],
            ),
            (
                "requirement".to_string(),
                vec![note(4, "requirement", "ti", &body)],
            ),
        ];
        let mut conv: Vec<ConventionRecord> = vec![];
        // 404 = 4 * 101: fits every kind but intent.
        let used = apply_budget(&mut sections, &mut conv, 404);
        assert!(sections[0].1.is_empty(), "intent roster drops first");
        assert_eq!(sections[1].1.len(), 1, "handoff survives");
        assert_eq!(sections[2].1.len(), 1, "question survives over intent");
        assert_eq!(sections[3].1.len(), 1, "decision survives");
        assert_eq!(sections[4].1.len(), 1, "requirement survives");
        assert_eq!(used, 404, "roster tokens are not charged when dropped");
    }

    #[test]
    fn compute_overlaps_returns_empty_without_any_intent_files() {
        assert!(compute_overlaps(&[]).is_empty());
        let sections = vec![("intent".to_string(), vec![note(0, "intent", "ti", "body")])];
        assert!(
            compute_overlaps(&sections).is_empty(),
            "an intent with no linked_files yields no overlaps"
        );
    }

    #[test]
    fn budget_preserves_display_order_of_sections() {
        let body = "x".repeat(400);
        let mut sections = vec![
            ("handoff".to_string(), vec![note(0, "handoff", "ti", &body)]),
            (
                "question".to_string(),
                vec![note(1, "question", "ti", &body)],
            ),
            (
                "decision".to_string(),
                vec![note(2, "decision", "ti", &body)],
            ),
            (
                "requirement".to_string(),
                vec![note(3, "requirement", "ti", &body)],
            ),
        ];
        let mut conv: Vec<ConventionRecord> = vec![];
        apply_budget(&mut sections, &mut conv, 150);
        let kinds: Vec<&str> = sections.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, ["handoff", "question", "decision", "requirement"]);
    }

    #[test]
    fn cap_keeps_locals_and_trims_dep_overflow() {
        let mut notes: Vec<Note> = (0..8).map(|i| note(i, "decision", "local", "b")).collect();
        notes.extend((100..108).map(|i| note(i, "decision", "dep", "b")));
        let mut sections = vec![("decision".to_string(), notes)];
        cap_sections(&mut sections, None); // decision default = 10
        let kept = &sections[0].1;
        assert_eq!(kept.len(), 10);
        for (idx, expected) in (0..8).enumerate() {
            assert_eq!(
                kept[idx].id,
                test_id(expected),
                "local {expected} must survive"
            );
        }
        assert_eq!(kept[8].id, test_id(100));
        assert_eq!(kept[9].id, test_id(101));
    }

    #[test]
    fn note_tokens_handles_empty_and_huge_bodies() {
        // estimate_tokens floors at 1, so an empty note weighs 2.
        assert_eq!(note_tokens(&note(0, "decision", "", "")), 2);
        // Huge body: 4000 chars -> 1000 tokens, plus 1 for the 1-char title.
        let huge = note(1, "decision", "t", &"x".repeat(4000));
        assert_eq!(note_tokens(&huge), 1001);
    }
}
