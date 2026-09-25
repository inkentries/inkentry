use anyhow::{Context, Result};
use serde::Serialize;

use super::super::helpers::require_server_client;
use super::MemoryReindexArgs;
use crate::{
    capability,
    config::{Config, SyncMode},
    embeddings::vec_to_blob,
    storage::MemoryStore,
};

// `total_active == already_embedded + missing_before`; `would_embed` is set
// under `--dry-run` only.
#[derive(Debug, Serialize)]
struct ReindexSummary {
    total_active: usize,
    missing_before: usize,
    already_embedded: usize,
    embedded: usize,
    remaining: usize,
    would_embed: usize,
    include_archived: bool,
    force: bool,
}

enum Outcome {
    DryRun,
    NothingToDo,
    Done,
}

// Suppressed when run as another command's finishing pass: printing would put a
// second document on a stdout the caller already wrote JSON to.
#[derive(PartialEq, Eq)]
pub(crate) enum Summary {
    Printed,
    Suppressed,
}

pub(crate) async fn memory_reindex(
    args: MemoryReindexArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
    summary_output: Summary,
) -> Result<()> {
    if backend_override == Some("git-notes") {
        anyhow::bail!(
            "This operation requires the sqlite backend. \
             Re-run without --backend git-notes."
        );
    }

    // Mirrors `open_memory_backend`'s `route_remote` condition: `cloud_first`
    // with no `server_url` falls back to `memory.db`, so there is something
    // local to re-embed. Gating on `mode` alone would reject that case.
    if cfg.resolve_mode() == SyncMode::CloudFirst && cfg.server_url.is_some() {
        anyhow::bail!(
            "'inkentry memory reindex' is not applicable in cloud_first mode with \
             server_url set: memory.db is not the store of record there (server_url \
             owns memory), so there is nothing local to re-embed."
        );
    }

    let json = crate::utils::effective_format(&args.format) == "json";

    let store = MemoryStore::open(mem_path)
        .with_context(|| format!("opening memory.db at {}", mem_path.display()))?;

    let total_active = store.count().context("counting active notes")? as usize;
    // Independent of --force / --include-archived so the summary always partitions.
    let missing_before = store
        .notes_missing_embeddings(false)
        .context("finding notes missing embeddings")?
        .len();
    let already_embedded = total_active.saturating_sub(missing_before);

    let candidates = if args.force {
        store
            .all_active_notes_for_reembed(args.include_archived)
            .context("listing notes to re-embed")?
    } else {
        store
            .notes_missing_embeddings(args.include_archived)
            .context("finding notes missing embeddings")?
    };

    let mut summary = ReindexSummary {
        total_active,
        missing_before,
        already_embedded,
        embedded: 0,
        remaining: 0,
        would_embed: 0,
        include_archived: args.include_archived,
        force: args.force,
    };

    // Neither path touches the embedder, so neither needs a running server.
    if args.dry_run {
        summary.would_embed = candidates.len();
        emit_summary(&summary, json, Outcome::DryRun, &summary_output);
        return Ok(());
    }
    if candidates.is_empty() {
        emit_summary(&summary, json, Outcome::NothingToDo, &summary_output);
        return Ok(());
    }

    // An auto-discovered loopback server sets the tier without populating
    // `server_url`; bridge it into an effective config as `memory add` does.
    let project_root = mem_path.parent().unwrap_or(mem_path);
    // `get_inference_tier`, not `get_tier`: local_first prefers the loopback
    // embedder even with an explicit server_url.
    let tier = capability::get_inference_tier(cfg).await;
    let eff_cfg = tier.effective_config(cfg, project_root);
    // Unlike reconcile, fail before any write when no embedder is reachable:
    // embedding is the point here, so a silent success would recreate the bug.
    let client = require_server_client(&eff_cfg, "memory reindex")?;

    let total = candidates.len();
    let mut embedded = 0usize;
    for (id, title, body) in &candidates {
        // Must match add.rs's document string so a backfilled vector equals an
        // add-time one. `embed_text`, not `embed_query`, which would prepend the
        // query instruction.
        let doc = format!("title: {title} | text: {body}");
        let vec = match client.embed_text(&doc).await {
            Ok(v) => v,
            Err(e) => {
                return Err(e.context(format!(
                    "embedding note {id} ({embedded} of {total} done and durably stored; \
                     re-run 'inkentry memory reindex' to resume the rest)"
                )));
            }
        };
        let blob = vec_to_blob(&vec);
        store
            .insert_embedding(id, &blob)
            .with_context(|| format!("storing embedding for note {id}"))?;
        embedded += 1;
        eprintln!("[inkentry] embedded {embedded}/{total}…");
    }

    summary.embedded = embedded;
    summary.remaining = total - embedded;
    emit_summary(&summary, json, Outcome::Done, &summary_output);
    Ok(())
}

fn emit_summary(s: &ReindexSummary, json: bool, outcome: Outcome, output: &Summary) {
    if *output == Summary::Suppressed {
        return;
    }
    if json {
        println!("{}", serde_json::to_string(s).unwrap_or_default());
        return;
    }
    match outcome {
        Outcome::DryRun => println!(
            "Dry run: {} note(s) would be embedded ({} active total, {} already embedded). \
             Nothing written.",
            s.would_embed, s.total_active, s.already_embedded
        ),
        Outcome::NothingToDo => println!(
            "Nothing to reindex: all {} active note(s) already embedded.",
            s.total_active
        ),
        Outcome::Done => println!(
            "Reindex complete: {} embedded, {} remaining ({} missing before, {} active total).",
            s.embedded, s.remaining, s.missing_before, s.total_active
        ),
    }
}
