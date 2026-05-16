//! `spelunk memory harvest --source entire`
//!
//! Mines `refs/entire/checkpoints/v1` for memory entries.
//! Checkpoints with a `Summary` struct are extracted directly without an LLM call
//! (fast path). Checkpoints without a `Summary` fall back to LLM extraction from
//! the session's `prompt.txt`.

use anyhow::{Context, Result};

use super::{MemoryHarvestArgs, backend_err};
use crate::{
    config::Config,
    embeddings::{EmbeddingBackend as _, vec_to_blob},
    indexer::secrets::contains_secret,
    storage::{NoteInput, open_memory_backend},
};

// ── Checkpoint metadata structs ───────────────────────────────────────────────

#[derive(serde::Deserialize, Debug)]
struct CommittedMetadata {
    #[serde(rename = "CheckpointID")]
    checkpoint_id: String,
    #[serde(rename = "CreatedAt")]
    created_at: String,
    #[serde(rename = "FilesTouched", default)]
    files_touched: Vec<String>,
    #[serde(rename = "Summary")]
    summary: Option<Summary>,
}

#[derive(serde::Deserialize, Debug)]
struct Summary {
    #[serde(rename = "Intent", default)]
    intent: String,
    #[serde(rename = "Outcome", default)]
    outcome: String,
    /// Flexible: may be an array of strings, an object with categorised arrays,
    /// or a plain string depending on the Entire CLI version.
    #[serde(rename = "Learnings", default)]
    learnings: serde_json::Value,
    #[serde(rename = "Friction", default)]
    friction: Vec<String>,
    #[serde(rename = "OpenItems", default)]
    open_items: Vec<String>,
}

#[derive(Clone)]
struct Checkpoint {
    id: String,
    /// Git object path prefix, e.g. "a3/b2c4d5e6f7" (no trailing slash).
    shard_path: String,
    files_touched: Vec<String>,
    summary: Option<CheckpointSummary>,
}

/// Extracted and owned copy of the relevant Summary fields.
#[derive(Clone)]
struct CheckpointSummary {
    intent: String,
    outcome: String,
    learnings: serde_json::Value,
    friction: Vec<String>,
    open_items: Vec<String>,
}

// ── Main entry point ──────────────────────────────────────────────────────────

pub(super) async fn harvest_entire(
    args: MemoryHarvestArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    use crate::llm::LlmBackend;

    // 1. Require explicit confirmation.
    if !args.confirm {
        println!("This will read git objects from refs/entire/checkpoints/v1 in the repository.");
        println!("Re-run with --confirm to proceed.");
        return Ok(());
    }

    let repo_dir = args.entire_repo.as_deref();
    let branch_ref = "refs/entire/checkpoints/v1";

    // 2. Verify the branch exists.
    let check = git_cmd(repo_dir, &["rev-parse", "--verify", branch_ref])?;
    if !check.status.success() {
        println!("No refs/entire/checkpoints/v1 branch found in this repository.");
        println!("Ensure Entire.io has been used in this project.");
        return Ok(());
    }

    // 3. List all files on the branch.
    let ls = git_cmd(repo_dir, &["ls-tree", "-r", "--name-only", branch_ref])?;
    if !ls.status.success() {
        anyhow::bail!(
            "git ls-tree failed: {}",
            String::from_utf8_lossy(&ls.stderr)
        );
    }
    let ls_output = String::from_utf8(ls.stdout).context("git ls-tree output not UTF-8")?;

    // Top-level CommittedMetadata files have exactly 3 path components:
    // <XX>/<YYYYYYYYYY>/metadata.json
    // Per-session metadata has 4: <XX>/<YYYYYYYYYY>/0/metadata.json — skip those.
    let meta_paths: Vec<&str> = ls_output
        .lines()
        .filter(|p| {
            let parts: Vec<&str> = p.split('/').collect();
            parts.len() == 3 && parts[2] == "metadata.json"
        })
        .collect();

    if meta_paths.is_empty() {
        println!("No checkpoints found on refs/entire/checkpoints/v1.");
        return Ok(());
    }

    // 4. Parse --since threshold.
    let since_secs: i64 = if let Some(ref s) = args.since {
        crate::utils::dates::parse_as_of(Some(s.as_str()))
            .with_context(|| format!("parsing --since '{s}'"))?
            .unwrap_or(0)
    } else {
        0
    };

    // 5. Open backend for dedup checks.
    let backend = open_memory_backend(cfg, mem_path, backend_override)?;

    // 6. Parse checkpoint metadata, applying the --since filter.
    let mut checkpoints: Vec<Checkpoint> = Vec::new();

    for path in &meta_paths {
        let obj_ref = format!("{branch_ref}:{path}");
        let cat = git_cmd(repo_dir, &["cat-file", "blob", &obj_ref])?;
        if !cat.status.success() {
            eprintln!(
                "warning: could not read {path}: {}",
                String::from_utf8_lossy(&cat.stderr)
            );
            continue;
        }
        let json = match String::from_utf8(cat.stdout) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("warning: {path} is not valid UTF-8, skipping");
                continue;
            }
        };
        let meta: CommittedMetadata = match serde_json::from_str(&json) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("warning: could not parse {path}: {e}");
                continue;
            }
        };

        // Parse RFC 3339 CreatedAt into epoch seconds.
        // Strip fractional seconds before the '.' and trailing 'Z' so the
        // existing dates utility can handle it (e.g. "2026-04-15T10:30:00.123Z").
        let ts_trimmed = meta
            .created_at
            .split('.')
            .next()
            .unwrap_or(&meta.created_at)
            .trim_end_matches('Z');
        let created_at_secs = crate::utils::dates::parse_as_of(Some(ts_trimmed))
            .unwrap_or(None)
            .unwrap_or(0);

        if created_at_secs < since_secs {
            continue;
        }

        // "XX/YYYYYYYYYY/metadata.json" → "XX/YYYYYYYYYY"
        let shard_path = path
            .rsplit_once('/')
            .map(|(dir, _)| dir.to_string())
            .unwrap_or_default();

        let summary = meta.summary.map(|s| CheckpointSummary {
            intent: s.intent,
            outcome: s.outcome,
            learnings: s.learnings,
            friction: s.friction,
            open_items: s.open_items,
        });

        checkpoints.push(Checkpoint {
            id: meta.checkpoint_id,
            shard_path,
            files_touched: meta.files_touched,
            summary,
        });
    }

    if checkpoints.is_empty() {
        println!("No checkpoints found after the --since filter.");
        return Ok(());
    }

    // 7. Filter already-harvested checkpoints.
    let mut new_checkpoints: Vec<Checkpoint> = Vec::new();
    for cp in checkpoints {
        let source_ref = format!("entire:{}", cp.id);
        if backend
            .has_source_ref(&source_ref)
            .await
            .map_err(backend_err)?
        {
            continue;
        }
        new_checkpoints.push(cp);
    }

    if new_checkpoints.is_empty() {
        println!("All checkpoints already harvested.");
        return Ok(());
    }

    let total = new_checkpoints.len();

    // 8. --dry-run: list without writing.
    if args.dry_run {
        println!("Would harvest {total} checkpoint(s):");
        for cp in &new_checkpoints {
            let mode = if cp.summary.is_some() {
                "fast-path"
            } else {
                "LLM"
            };
            println!("  {} [{}]", cp.id, mode);
        }
        return Ok(());
    }

    // 9. Load embedder (required for both fast-path and LLM fallback).
    let embedder = crate::backends::ActiveEmbedder::load(cfg)
        .await
        .context("loading embedding model")?;

    let mut stored = 0usize;
    let mut dedup_skipped = 0usize;
    const DEDUP_THRESHOLD: f64 = 0.15;

    let (with_summary, without_summary): (Vec<_>, Vec<_>) = new_checkpoints
        .into_iter()
        .partition(|cp| cp.summary.is_some());

    println!(
        "Harvesting {} new checkpoint(s) ({} fast-path, {} LLM fallback)…",
        total,
        with_summary.len(),
        without_summary.len()
    );

    // ── Fast path: extract directly from Summary ──────────────────────────────

    for cp in with_summary {
        let summary = cp.summary.unwrap();
        let source_ref = format!("entire:{}", cp.id);
        let short_id = &cp.id[..cp.id.len().min(8)];

        let mut entries: Vec<(String, String, String, Vec<String>)> = Vec::new();

        // Intent + Outcome → handoff
        if !summary.intent.is_empty() || !summary.outcome.is_empty() {
            let title = if !summary.intent.is_empty() {
                format!("Session: {}", truncate_str(&summary.intent, 72))
            } else {
                format!("Session outcome: {}", truncate_str(&summary.outcome, 72))
            };
            let body = format!(
                "**Intent:** {}\n\n**Outcome:** {}",
                summary.intent, summary.outcome
            );
            entries.push((
                "handoff".to_string(),
                title,
                body,
                vec!["entire".to_string(), "session".to_string()],
            ));
        }

        // Learnings → decision entries
        for learning in extract_strings(&summary.learnings) {
            if learning.trim().is_empty() {
                continue;
            }
            let title = truncate_str(&learning, 80).to_string();
            entries.push((
                "decision".to_string(),
                title,
                learning,
                vec!["entire".to_string(), "learning".to_string()],
            ));
        }

        // OpenItems → question entries
        for item in summary.open_items {
            if item.trim().is_empty() {
                continue;
            }
            let title = truncate_str(&item, 80).to_string();
            entries.push((
                "question".to_string(),
                title,
                item.clone(),
                vec!["entire".to_string(), "open-item".to_string()],
            ));
        }

        // Friction → note entries
        for friction in summary.friction {
            if friction.trim().is_empty() {
                continue;
            }
            let title = truncate_str(&friction, 80).to_string();
            entries.push((
                "note".to_string(),
                title,
                friction.clone(),
                vec!["entire".to_string(), "friction".to_string()],
            ));
        }

        for (kind, title, body, tags) in entries {
            let embed_text = format!("title: {title} | text: {body}");
            let vecs = embedder.embed(&[&embed_text]).await?;
            let Some(vec) = vecs.into_iter().next() else {
                continue;
            };
            let blob = vec_to_blob(&vec);

            let neighbors = backend.search(&blob, 1, None).await?;
            if let Some(top) = neighbors.first()
                && top.distance.unwrap_or(1.0) < DEDUP_THRESHOLD
            {
                println!(
                    "  [dedup] '{}' too similar to #{} '{}' (dist={:.3})",
                    title,
                    top.id,
                    top.title,
                    top.distance.unwrap_or(0.0)
                );
                dedup_skipped += 1;
                continue;
            }

            let note_id = backend
                .add(NoteInput {
                    kind: kind.clone(),
                    title: title.clone(),
                    body,
                    tags,
                    linked_files: cp.files_touched.clone(),
                    embedding: Some(blob),
                    source_ref: Some(source_ref.clone()),
                    valid_at: None,
                    supersedes: None,
                })
                .await?;

            println!("  + [{kind}] #{note_id}: {title}  \x1b[2m({short_id})\x1b[0m");
            stored += 1;
        }
    }

    // ── LLM fallback for checkpoints without Summary ──────────────────────────

    if !without_summary.is_empty() {
        let sp = super::super::ui::spinner("Loading LLM for fallback harvest…");
        let llm = crate::backends::ActiveLlm::load(cfg)
            .await
            .context("loading LLM")?;
        sp.finish_and_clear();

        let system = "You help build a project memory store from Entire.io agent session \
            transcripts. Respond ONLY with valid JSON matching the provided schema. No other text.";

        let schema = serde_json::json!({
            "name": "harvest_result",
            "strict": true,
            "schema": {
                "type": "object",
                "properties": {
                    "entries": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "checkpoint_id": {"type": "string"},
                                "kind": {
                                    "type": "string",
                                    "enum": ["decision","handoff","requirement","note","question"]
                                },
                                "title": {"type": "string"},
                                "body": {"type": "string"},
                                "tags": {"type": "array", "items": {"type": "string"}}
                            },
                            "required": ["checkpoint_id","kind","title","body","tags"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["entries"],
                "additionalProperties": false
            }
        });

        let batch_size = args.batch_size.max(1);
        let context_length = cfg.llm_context_length;
        let estimate_tokens = |s: &str| s.len() / 3;
        let output_budget = |n: usize| (n * 400).clamp(256, context_length / 2);

        // Work queue using indices into `without_summary` to avoid Clone-heavy splitting.
        let mut work: std::collections::VecDeque<Vec<usize>> = without_summary
            .iter()
            .enumerate()
            .map(|(i, _)| i)
            .collect::<Vec<_>>()
            .chunks(batch_size)
            .map(<[usize]>::to_vec)
            .collect();

        let mut batch_num = 0usize;
        let num_batches = work.len();

        while let Some(batch_indices) = work.pop_front() {
            batch_num += 1;

            // Read prompt.txt for each checkpoint in this batch.
            // (id, prompt_text, files_touched)
            let mut checkpoint_texts: Vec<(String, String, Vec<String>)> = Vec::new();
            for &i in &batch_indices {
                let cp = &without_summary[i];
                let prompt_path = format!("{}/0/prompt.txt", cp.shard_path);
                let obj_ref = format!("{branch_ref}:{prompt_path}");
                let cat = git_cmd(repo_dir, &["cat-file", "blob", &obj_ref]);
                let text = match cat {
                    Ok(out) if out.status.success() => match String::from_utf8(out.stdout) {
                        Ok(s) => {
                            let s = s.trim().to_string();
                            const CAP: usize = 8_000;
                            if s.len() > CAP {
                                let b = s.floor_char_boundary(CAP);
                                format!("{}\n\n[...truncated]", &s[..b])
                            } else {
                                s
                            }
                        }
                        Err(_) => {
                            eprintln!(
                                "warning: prompt.txt for {} is not valid UTF-8, skipping",
                                cp.id
                            );
                            continue;
                        }
                    },
                    _ => {
                        eprintln!("warning: no prompt.txt for checkpoint {}", cp.id);
                        continue;
                    }
                };
                if !text.is_empty() {
                    if contains_secret(&text) {
                        eprintln!(
                            "warning: skipping checkpoint {} (secret detected in prompt.txt)",
                            cp.id
                        );
                        continue;
                    }
                    checkpoint_texts.push((cp.id.clone(), text, cp.files_touched.clone()));
                }
            }

            if checkpoint_texts.is_empty() {
                println!("  Batch {batch_num}: no readable prompt.txt found, skipping.");
                continue;
            }

            let session_list = checkpoint_texts
                .iter()
                .map(|(id, text, _)| format!("CHECKPOINT {id}\n{text}"))
                .collect::<Vec<_>>()
                .join("\n\n---\n\n");

            let user = format!(
                "Review these Entire.io agent session transcripts (user prompts). \
                 Identify content that represents:\n\
                 - \"decision\": A significant architectural or design choice and reasoning\n\
                 - \"handoff\": What was accomplished and what remains (intent + outcome)\n\
                 - \"requirement\": A hard constraint the codebase must satisfy\n\
                 - \"note\": A surprising or non-obvious discovery\n\
                 - \"question\": An unresolved question or open item\n\n\
                 SKIP — return NO entry for:\n\
                 - Routine coding questions with no design significance\n\
                 - Trivial edits, typos, comment wording\n\
                 - Questions about syntax or standard library usage\n\n\
                 For each significant session write: checkpoint_id (exact as given), kind, \
                 title (one sentence), body (include why, what alternatives were considered), \
                 tags (2-4 keywords).\n\n\
                 Sessions:\n{session_list}"
            );

            let input_tokens = estimate_tokens(system) + estimate_tokens(&user);
            let out_budget = output_budget(checkpoint_texts.len());

            if input_tokens + out_budget > context_length && batch_indices.len() > 1 {
                batch_num -= 1;
                let mid = batch_indices.len() / 2;
                work.push_front(batch_indices[mid..].to_vec());
                work.push_front(batch_indices[..mid].to_vec());
                continue;
            }

            let max_tokens = context_length
                .saturating_sub(input_tokens)
                .min(out_budget)
                .max(128);

            if num_batches > 1 || work.front().is_some() {
                println!(
                    "\nBatch {batch_num} ({} checkpoint(s))…",
                    checkpoint_texts.len()
                );
            }

            let messages = vec![
                crate::llm::Message::system(system),
                crate::llm::Message::user(user),
            ];

            let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::llm::Token>(256);
            let generate = llm.generate(&messages, max_tokens, tx, Some(schema.clone()));
            let collect = async move {
                let mut buf = String::new();
                while let Some(t) = rx.recv().await {
                    buf.push_str(&t);
                }
                buf
            };
            let (_, raw_json) =
                tokio::try_join!(generate, async { Ok::<_, anyhow::Error>(collect.await) })?;
            let raw_json = crate::utils::strip_ansi(&raw_json);

            let parsed: serde_json::Value = serde_json::from_str(&raw_json).with_context(|| {
                format!("parsing LLM harvest response (batch {batch_num}):\n{raw_json}")
            })?;

            let entries = parsed["entries"].as_array().cloned().unwrap_or_default();
            if entries.is_empty() {
                println!("  No significant sessions in this batch.");
                continue;
            }

            println!("Embedding {} entries…", entries.len());

            for entry in &entries {
                let cp_id = entry["checkpoint_id"].as_str().unwrap_or("").to_string();
                let kind = entry["kind"].as_str().unwrap_or("note");
                let title = entry["title"].as_str().unwrap_or("").to_string();
                let body = entry["body"].as_str().unwrap_or("").to_string();
                let tags: Vec<String> = entry["tags"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();

                let files = checkpoint_texts
                    .iter()
                    .find(|(id, _, _)| *id == cp_id || id.starts_with(&cp_id))
                    .map(|(_, _, f)| f.clone())
                    .unwrap_or_default();

                if contains_secret(&body) {
                    eprintln!("warning: skipping entry '{title}' (secret detected in LLM body)");
                    continue;
                }

                let source_ref = format!("entire:{cp_id}");
                if backend
                    .has_source_ref(&source_ref)
                    .await
                    .map_err(backend_err)?
                {
                    println!("  [skip] already harvested {cp_id}");
                    continue;
                }

                let embed_text = format!("title: {title} | text: {body}");
                let vecs = embedder.embed(&[&embed_text]).await?;
                let Some(vec) = vecs.into_iter().next() else {
                    continue;
                };
                let blob = vec_to_blob(&vec);

                let neighbors = backend.search(&blob, 1, None).await?;
                if let Some(top) = neighbors.first()
                    && top.distance.unwrap_or(1.0) < DEDUP_THRESHOLD
                {
                    println!(
                        "  [dedup] '{}' too similar to #{} '{}' (dist={:.3})",
                        title,
                        top.id,
                        top.title,
                        top.distance.unwrap_or(0.0)
                    );
                    dedup_skipped += 1;
                    continue;
                }

                let note_id = backend
                    .add(NoteInput {
                        kind: kind.to_string(),
                        title: title.clone(),
                        body,
                        tags,
                        linked_files: files,
                        embedding: Some(blob),
                        source_ref: Some(source_ref),
                        valid_at: None,
                        supersedes: None,
                    })
                    .await?;

                let short_id = &cp_id[..cp_id.len().min(8)];
                println!("  + [{kind}] #{note_id}: {title}  \x1b[2m({short_id})\x1b[0m");
                stored += 1;
            }
        }
    }

    println!(
        "\nStored {stored} memory entries from {total} new checkpoint(s). \
         Skipped {dedup_skipped} near-duplicate."
    );
    Ok(())
}

// ── Git helper ────────────────────────────────────────────────────────────────

fn git_cmd(repo_dir: Option<&std::path::Path>, args: &[&str]) -> Result<std::process::Output> {
    let mut cmd = std::process::Command::new("git");
    if let Some(dir) = repo_dir {
        cmd.args([std::ffi::OsStr::new("-C"), dir.as_os_str()]);
    }
    cmd.args(args)
        .output()
        .context("running git (is git installed?)")
}

// ── String helpers ────────────────────────────────────────────────────────────

fn truncate_str(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        s
    } else {
        let boundary = s.floor_char_boundary(max_chars);
        &s[..boundary]
    }
}

/// Extract string items from a JSON value.
/// Handles: array of strings, object whose values are strings or arrays of strings,
/// and plain string.
fn extract_strings(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        serde_json::Value::Object(map) => map
            .values()
            .flat_map(|v| match v {
                serde_json::Value::String(s) => vec![s.clone()],
                serde_json::Value::Array(arr) => arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect(),
                _ => vec![],
            })
            .collect(),
        serde_json::Value::String(s) => vec![s.clone()],
        _ => vec![],
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_committed_metadata_with_summary() {
        let json = r#"{
            "CheckpointID": "a3b2c4d5e6f7",
            "SessionID": "sess-001",
            "CreatedAt": "2026-04-15T10:30:00Z",
            "Branch": "main",
            "FilesTouched": ["src/lib.rs", "src/main.rs"],
            "Agent": "Claude Code",
            "Model": "claude-sonnet-4-6",
            "Summary": {
                "Intent": "Implement the entire harvest command",
                "Outcome": "harvest_entire.rs created and wired up",
                "Learnings": ["Used git cat-file for object reads", "No Entire CLI needed"],
                "Friction": ["RFC3339 parsing needed custom handling"],
                "OpenItems": ["Add integration test"]
            }
        }"#;
        let meta: CommittedMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.checkpoint_id, "a3b2c4d5e6f7");
        assert_eq!(meta.files_touched, vec!["src/lib.rs", "src/main.rs"]);
        let summary = meta.summary.unwrap();
        assert_eq!(summary.intent, "Implement the entire harvest command");
        assert_eq!(summary.friction.len(), 1);
        assert_eq!(summary.open_items.len(), 1);
    }

    #[test]
    fn parses_committed_metadata_without_summary() {
        let json = r#"{
            "CheckpointID": "000000000001",
            "SessionID": "sess-old",
            "CreatedAt": "2026-01-01T00:00:00Z",
            "FilesTouched": [],
            "Agent": "Claude Code",
            "Model": "claude-sonnet-4-5"
        }"#;
        let meta: CommittedMetadata = serde_json::from_str(json).unwrap();
        assert!(meta.summary.is_none());
    }

    #[test]
    fn extract_strings_from_array() {
        let v = serde_json::json!(["alpha", "beta", "gamma"]);
        assert_eq!(extract_strings(&v), vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn extract_strings_from_object() {
        let v = serde_json::json!({"cat1": ["a", "b"], "cat2": "c"});
        let mut got = extract_strings(&v);
        got.sort();
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    #[test]
    fn extract_strings_from_plain_string() {
        let v = serde_json::json!("just a string");
        assert_eq!(extract_strings(&v), vec!["just a string"]);
    }

    #[test]
    fn extract_strings_from_null() {
        let v = serde_json::Value::Null;
        assert!(extract_strings(&v).is_empty());
    }

    #[test]
    fn truncate_str_short() {
        assert_eq!(truncate_str("hello", 80), "hello");
    }

    #[test]
    fn truncate_str_long() {
        let s = "a".repeat(200);
        let t = truncate_str(&s, 80);
        assert!(t.len() <= 80);
    }

    #[test]
    fn rfc3339_date_parsed_by_dates_util() {
        // "2026-04-15T10:30:00Z" — strip Z, parse as ISO 8601.
        let ts = "2026-04-15T10:30:00Z";
        let trimmed = ts.split('.').next().unwrap().trim_end_matches('Z');
        let secs = crate::utils::dates::parse_as_of(Some(trimmed))
            .unwrap()
            .unwrap();
        assert!(secs > 0);
        // 2026-04-15 10:30:00 UTC = 1776249000
        assert_eq!(secs, 1776249000);
    }
}
