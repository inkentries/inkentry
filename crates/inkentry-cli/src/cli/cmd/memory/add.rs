use anyhow::{Context, Result};

use super::MemoryAddArgs;
use crate::{
    capability,
    config::{Config, SyncMode},
    indexer::secrets::contains_secret,
    server_client::ServerInferenceClient,
    storage::{
        CarriedEdge, GitNotesBackend, MemoryBackend, MemoryStore, NoteId, NoteInput, NoteRecord,
        RewriteRefStatus, append_state_update, append_to_git_notes, note_entity_id, now_millis,
        now_secs, open_memory_backend, unresolvable_id_message,
    },
};

// Bounds the embedder being held by a bulk index pass, where the wait runs to
// minutes; a healthy embed takes tens of milliseconds.
pub(super) const INTERACTIVE_EMBED_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

pub(super) fn pending_embedding_warning(reason: &str) -> String {
    format!(
        "warning: entry stored, but {reason}, so `inkentry search` cannot rank it \
         semantically yet. `inkentry memory reindex` embeds it now; `inkentry sync` \
         picks it up on its own."
    )
}

pub(super) async fn memory_add(
    args: MemoryAddArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
    pre_init_notes: bool,
) -> Result<()> {
    let started = std::time::Instant::now();
    // Loopback auto-discovery sets the tier without populating `cfg.server_url`;
    // the effective config routes inference there while leaving `server_url`
    // unset so the note still lands in the local `memory.db`.
    // On the git-notes paths `mem_path` is a placeholder; the project is the git
    // repo at CWD.
    let cwd;
    let placeholder_path = pre_init_notes || backend_override == Some("git-notes");
    let project_root: &std::path::Path = if placeholder_path {
        cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        &cwd
    } else {
        mem_path.parent().unwrap_or(mem_path)
    };
    // Not `get_tier`: in `local_first` inference must prefer the local loopback
    // embedder even when `server_url` is set, and `get_tier` would probe `server_url`.
    let tier = capability::get_inference_tier(cfg).await;
    let eff_cfg = tier.effective_config(cfg, project_root);
    let cfg = &eff_cfg;
    let (title, body) = if let Some(url) = &args.from_url {
        let (fetched_title, fetched_body) = fetch_url_content(url)
            .await
            .with_context(|| format!("fetching {url}"))?;
        let title = args.title.clone().unwrap_or(fetched_title);
        let body = args.body.clone().unwrap_or(fetched_body);
        (title, body)
    } else {
        let title = args
            .title
            .clone()
            .context("--title is required when --from-url is not provided")?;
        let body = match args.body.clone() {
            Some(b) => b,
            None => {
                let t = title.clone();
                tokio::task::spawn_blocking(move || super::open_editor_for_body(&t))
                    .await
                    .context("editor task panicked")?
                    .context("opening editor for body")?
            }
        };
        (title, body)
    };

    let tags: Vec<String> = args
        .tags
        .as_deref()
        .map(|s| s.split(',').map(|t| t.trim().to_string()).collect())
        .unwrap_or_default();

    let files: Vec<String> = args
        .files
        .as_deref()
        .map(|s| s.split(',').map(|f| f.trim().to_string()).collect())
        .unwrap_or_default();

    // Resolved ahead of any write so an escaping path errors before anything is
    // stored; storage re-resolves the same paths. `files_root` is the real
    // project root, not `project_root`, which is a placeholder pre-init.
    let files_root = if placeholder_path {
        project_root.to_path_buf()
    } else {
        mem_path
            .parent()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| project_root.to_path_buf())
    };
    let mut file_link_states: Vec<(String, String)> = Vec::new();
    for raw in files.iter().filter(|f| !f.trim().is_empty()) {
        let link = crate::storage::resolve_file_link(&files_root, raw)
            .with_context(|| format!("resolving linked file '{raw}'"))?;
        if link.state == crate::storage::FileState::Missing {
            eprintln!(
                "warning: linked file '{}' does not exist yet (state: missing)",
                link.path
            );
        }
        file_link_states.push((link.path, link.state.as_str().to_string()));
    }

    // Before any persistence so no credential reaches either store; the error
    // deliberately does not echo the matched text.
    if contains_secret(&title) || contains_secret(&body) {
        anyhow::bail!(
            "memory add: refusing to store entry — title or body matches a secret pattern. \
             Remove the credential and try again. (No data was written to SQLite or git notes.)"
        );
    }

    // The id `add` surfaces is the portable one, not the per-machine row id.
    let entity_id = crate::storage::entity_id::entity_id(&args.kind, &title, &body);

    // Only a local row can have a vector attached after the fact, so only there
    // can the write go first; other backends take the vector as part of the add
    // and have no local backfill.
    let store_first = !placeholder_path
        && !(cfg.resolve_mode() == SyncMode::CloudFirst && cfg.server_url.is_some());
    // Git notes hold no vector.
    let embedding = if store_first || pre_init_notes {
        None
    } else {
        let embed_text = format!("title: {title} | text: {body}");
        try_embed_via_server(cfg, &embed_text).await
    };

    let valid_at = args
        .valid_at
        .and_then(|s| super::parse_as_of(Some(&s)).ok().flatten());

    // OLD must still be active before any write: the SQL `WHERE status = 'active'`
    // guard on the archive UPDATE silently no-ops on a stale OLD, which would
    // leave an orphaned new note plus a conflicting carrier record.
    let mut backend_for_add: Option<Box<dyn MemoryBackend + Send>> = None;
    let mut old_note_for_carrier = None;
    if let Some(old_id) = args.supersedes.clone() {
        let old = if pre_init_notes {
            GitNotesBackend::with_root(project_root.to_path_buf())
                .get(old_id.clone())
                .await?
        } else {
            let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
            let old = backend.get(old_id.clone()).await?;
            backend_for_add = Some(backend);
            old
        };
        match old {
            Some(note) if note.status == "active" => {
                old_note_for_carrier = Some(note);
            }
            _ => {
                anyhow::bail!("No active memory entry with id {old_id} (old).");
            }
        }
    }

    // Checked before the write so a bad id fails cleanly with a message naming
    // it, not a foreign-key error. The target need not be active. Skipped
    // pre-init: there is no local graph. The carrier records the edge by the
    // target's `entity_id`, the only name that survives an `init` renumbering
    // ids on another machine.
    let mut relates_to_entity_id: Option<String> = None;
    if let Some(rel_id) = args.relates_to.as_ref()
        && !pre_init_notes
    {
        let backend = match backend_for_add.take() {
            Some(backend) => backend,
            None => open_memory_backend(cfg, mem_path, backend_override).await?,
        };
        let target = backend.get(rel_id.clone()).await?;
        backend_for_add = Some(backend);
        let Some(target) = target else {
            anyhow::bail!("{}", unresolvable_id_message(rel_id));
        };
        relates_to_entity_id = Some(note_entity_id(&target));
    }

    // Pre-init there is no primary store; the carrier is the sole writer, so the
    // id is minted the way the backends do. Held past the write so the
    // `--relates-to` edge goes through the same handle.
    let mut primary_backend: Option<Box<dyn MemoryBackend + Send>> = None;
    let (id, created) = if pre_init_notes {
        (crate::storage::carrier_token(now_millis()), true)
    } else {
        let backend = match backend_for_add.take() {
            Some(backend) => backend,
            None => open_memory_backend(cfg, mem_path, backend_override).await?,
        };
        let added = backend
            .add(NoteInput {
                kind: args.kind.clone(),
                title: title.clone(),
                body: body.clone(),
                tags: tags.clone(),
                linked_files: files.clone(),
                embedding,
                source_ref: None,
                valid_at,
                supersedes: args.supersedes.clone(),
                origin: crate::storage::Origin::from_caller(&cfg.caller),
            })
            .await?;
        primary_backend = Some(backend);
        added
    };

    // The remote backend reports edge ops as a no-op, hence the kind check.
    if let Some(rel_id) = args.relates_to.as_ref()
        && let Some(backend) = primary_backend.as_ref()
        && matches!(backend.backend_kind(), "sqlite" | "git-notes")
    {
        backend
            .add_edge(&id, rel_id, "relates_to")
            .await
            .with_context(|| format!("recording relates_to edge to {rel_id}"))?;
    }

    // Suppressed when git notes is already the primary store, to avoid a double
    // write. Post-`init` it is best-effort; pre-`init` it is the sole store, so
    // a failed carry is fatal.
    let write_through =
        pre_init_notes || (cfg.store_in_git_notes && backend_override != Some("git-notes"));
    let mut notes_rewrite_note: Option<&str> = None;
    if write_through {
        let record = NoteRecord {
            schema_version: 1,
            id: id.as_str().parse().unwrap_or_else(|_| now_millis()),
            kind: args.kind.clone(),
            title: title.clone(),
            body: body.clone(),
            tags: tags.clone(),
            linked_files: files.clone(),
            created_at: now_secs(),
            status: "active".to_string(),
            source_ref: None,
            valid_at,
            invalid_at: None,
            superseded_by: None,
            remote_id: None,
            entity_id: Some(entity_id.clone()),
            superseded_by_entity_id: None,
            edges: relates_to_entity_id
                .iter()
                .map(|to| CarriedEdge::new("relates_to", to.clone()))
                .collect(),
            origin: crate::storage::Origin::from_caller(&cfg.caller),
        };
        match append_to_git_notes(Some(project_root), &record).await {
            Ok(outcome) => {
                // An unserialized write can lose a concurrent entry; `eprintln!`
                // because `tracing` is invisible without RUST_LOG.
                if let Some(degradation) = outcome.lock_degradation {
                    eprintln!("Warning: {degradation}");
                }
                match outcome.rewrite_ref {
                    // Announce only the call that set it, so a repo says this once.
                    RewriteRefStatus::Configured => {
                        notes_rewrite_note = Some(
                            "Configured git notes.rewriteRef in this repo, so memory now survives \
                             `git commit --amend` and `git rebase`.",
                        );
                    }
                    RewriteRefStatus::Failed => {
                        notes_rewrite_note = Some(
                            "Warning: could not set git notes.rewriteRef, so memory may not survive \
                             `git commit --amend` or `git rebase`. Set it with: \
                             git config --add notes.rewriteRef refs/notes/inkentry",
                        );
                    }
                    RewriteRefStatus::AlreadyCovered => {}
                }
            }
            Err(e) if pre_init_notes => {
                return Err(e.context(
                    "recording memory entry to git notes (no local project store to fall back on)",
                ));
            }
            Err(e) => {
                eprintln!(
                    "Warning: entry stored locally, but the git-notes carry failed, \
                     so it will not travel with the repo: {e:#}"
                );
            }
        }

        // The supersede edge travels as a state-update on OLD's own record
        // pointing at NEW's `entity_id`, not on NEW's record. Non-fatal: the
        // primary store already holds the archive.
        if let Some(old_note) = old_note_for_carrier {
            let old_id = old_note.id.clone();
            let invalid_at = old_note.invalid_at.or_else(|| Some(now_secs()));
            if let Err(e) = append_state_update(
                Some(project_root),
                &old_note,
                "archived",
                invalid_at,
                Some(entity_id.clone()),
            )
            .await
            {
                eprintln!(
                    "Warning: entry stored locally, but carrying #{old_id}'s \
                     supersede edge to git notes failed, so it will not travel \
                     with the repo: {e:#}"
                );
            }
        }
    }

    // Embedding only after the entry is durable, so a stalled embed costs the
    // vector, not the entry. A vectorless entry is what `memory reindex` and
    // sync's repair already look for.
    let mut pending_embedding = None;
    if store_first {
        // Closed so the attach does not contend with our own idle connection.
        drop(primary_backend);
        let doc = format!("title: {title} | text: {body}");
        pending_embedding = embed_and_attach(cfg, mem_path, &id, &doc).await;
    }

    let format = crate::utils::effective_format(&args.format);
    match format {
        // stdout is only the object; the human lead line and rewrite-ref note
        // would corrupt it.
        "json" | "jsonl" => {
            let mut obj = serde_json::json!({
                "id": &id,
                "entity_id": entity_id,
                "kind": args.kind,
                "title": title,
                "created": created,
            });
            if !file_link_states.is_empty() {
                obj["linked_files"] = file_link_states
                    .iter()
                    .map(|(path, state)| serde_json::json!({"path": path, "state": state}))
                    .collect();
            }
            if format == "jsonl" {
                println!("{}", serde_json::to_string(&obj)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&obj)?);
            }
        }
        _ => {
            let handle = crate::storage::entity_id_handle(&entity_id);
            if created {
                println!("Stored [{kind}] #{handle}: {title}", kind = args.kind);
            } else {
                println!(
                    "Already recorded as [{kind}] #{handle}: {title}",
                    kind = args.kind
                );
            }
            println!("entity_id:  {entity_id}");
            println!("id:         {id}");
            if let Some(line) = notes_rewrite_note {
                println!("{line}");
            }
        }
    }
    // stderr so stdout is unchanged; `eprintln!` because `tracing` is invisible
    // without `RUST_LOG`.
    if let Some(reason) = pending_embedding {
        eprintln!("{}", pending_embedding_warning(&reason));
    }

    // Not just `pre_init_notes`: with `--backend git-notes`, `mem_path` is still
    // a placeholder, and nudging would make `MemoryStore::open` create a phantom
    // `memory.db` for a project that opted out of one.
    if !placeholder_path {
        super::outbox::nudge_after_write(cfg, mem_path).await;
    }

    if !pre_init_notes {
        let tokens_out = crate::search::tokens::estimate_tokens(&title)
            + crate::search::tokens::estimate_tokens(&body);
        super::super::events::record(
            cfg,
            mem_path,
            backend_override,
            "memory.add",
            None,
            Some(1),
            std::slice::from_ref(&entity_id),
            Some(tokens_out as i64),
            started,
            true,
        );
    }
    Ok(())
}

// Returns `Some(reason)` when the entry is left without a vector. Attaches via
// `insert_embedding`, as backfill does, so both paths are identical on disk.
async fn embed_and_attach(
    cfg: &Config,
    mem_path: &std::path::Path,
    id: &NoteId,
    doc: &str,
) -> Option<String> {
    use crate::embeddings::vec_to_blob;

    let Some(client) = ServerInferenceClient::from_config(cfg) else {
        return Some("no embedder was reachable".to_string());
    };
    let sp = super::super::ui::spinner("Embedding…");
    // Dropping the future cancels the request, adding no load to a saturated
    // embedder.
    let result = tokio::time::timeout(INTERACTIVE_EMBED_BUDGET, client.embed_text(doc)).await;
    sp.finish_and_clear();

    let vec = match result {
        Ok(Ok(vec)) => vec,
        Ok(Err(e)) => {
            tracing::warn!("embedding entry {id} failed: {e:#}");
            return Some("embedding it failed".to_string());
        }
        Err(_elapsed) => {
            return Some(format!(
                "embedding it did not finish within {}s",
                INTERACTIVE_EMBED_BUDGET.as_secs()
            ));
        }
    };

    match MemoryStore::open(mem_path)
        .and_then(|store| store.insert_embedding(id, &vec_to_blob(&vec)))
    {
        Ok(()) => None,
        Err(e) => {
            tracing::warn!("storing the embedding for entry {id} failed: {e:#}");
            Some("its vector could not be stored".to_string())
        }
    }
}

async fn fetch_url_content(url: &str) -> Result<(String, String)> {
    let gh_issue_re =
        regex::Regex::new(r"https?://github\.com/([^/]+)/([^/]+)/(?:issues|pull)/(\d+)").unwrap();

    if let Some(caps) = gh_issue_re.captures(url) {
        let owner = &caps[1];
        let repo = &caps[2];
        let num = &caps[3];
        let api_path = format!("repos/{owner}/{repo}/issues/{num}");
        let out = tokio::process::Command::new("gh")
            .args(["api", &api_path])
            .output()
            .await;
        if let Ok(out) = out
            && out.status.success()
        {
            let json: serde_json::Value =
                serde_json::from_slice(&out.stdout).context("parsing gh api response")?;
            let title = json["title"].as_str().unwrap_or("GitHub Issue").to_string();
            let body = json["body"].as_str().unwrap_or("").to_string();
            return Ok((title, body));
        }
    }

    // The script runs only from an inkentry-owned path, never a general home-dir
    // location, so an attacker-writable script elsewhere is not a code-execution
    // path.
    let script = web_to_md_script_path().filter(|p| p.exists());

    if let Some(script_path) = script {
        let out = tokio::process::Command::new("bun")
            .arg(&script_path)
            .arg(url)
            .output()
            .await;
        if let Ok(out) = out
            && out.status.success()
        {
            let md = String::from_utf8_lossy(&out.stdout);
            return parse_web_to_md_output(&md, url);
        }
    }

    let http = reqwest::Client::builder()
        .user_agent(concat!("inkentry/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let html = http.get(url).send().await?.text().await?;

    let title_re = regex::Regex::new(r"(?i)<title[^>]*>([\s\S]*?)</title>").unwrap();
    let title = title_re
        .captures(&html)
        .and_then(|c| c.get(1))
        .map(|m| html_unescape(m.as_str().trim()))
        .unwrap_or_else(|| url.to_string());

    let no_script =
        regex::Regex::new(r"(?is)<(?:script|style)[^>]*>[\s\S]*?</(?:script|style)>").unwrap();
    let no_tags = regex::Regex::new(r"<[^>]+>").unwrap();
    let ws = regex::Regex::new(r"\s{3,}").unwrap();
    let stripped = no_script.replace_all(&html, " ");
    let stripped = no_tags.replace_all(&stripped, " ");
    let body = ws.replace_all(stripped.trim(), "\n\n").to_string();
    let body = if body.len() > 8192 {
        body[..8192].to_string()
    } else {
        body
    };

    Ok((title, body))
}

// `INKENTRY_SCRIPTS_DIR` exists because `dirs::home_dir()` ignores `HOME` on
// Windows, so tests cannot redirect the home directory.
fn web_to_md_script_path() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("INKENTRY_SCRIPTS_DIR") {
        return Some(std::path::PathBuf::from(dir).join("web-to-md.ts"));
    }
    dirs::home_dir().map(|h| {
        h.join(".config")
            .join("inkentry")
            .join("scripts")
            .join("web-to-md.ts")
    })
}

fn parse_web_to_md_output(md: &str, url: &str) -> Result<(String, String)> {
    let md = md.trim();
    if let Some(rest) = md.strip_prefix("# ") {
        let (title_line, body) = rest.split_once('\n').unwrap_or((rest, ""));
        Ok((title_line.trim().to_string(), body.trim_start().to_string()))
    } else {
        Ok((url.to_string(), md.to_string()))
    }
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

// `None` lets callers store the entry without a vector rather than fail.
async fn try_embed_via_server(cfg: &Config, text: &str) -> Option<Vec<u8>> {
    use crate::embeddings::vec_to_blob;
    let Some(client) = ServerInferenceClient::from_config(cfg) else {
        tracing::warn!(
            "No server_url configured — memory entry stored without embedding vector; \
             semantic search will not surface it."
        );
        return None;
    };
    let sp = super::super::ui::spinner("Embedding…");
    let result: anyhow::Result<Vec<u8>> = async {
        let vec = client
            .embed_text(text)
            .await
            .context("embedding memory entry")?;
        Ok(vec_to_blob(&vec))
    }
    .await;
    sp.finish_and_clear();
    match result {
        Ok(blob) => Some(blob),
        Err(e) => {
            tracing::warn!(
                "Server embedding failed — entry stored without vector; \
                 semantic search will not surface it. ({e})"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    fn with_scripts_dir<F: FnOnce()>(dir: &std::path::Path, f: F) {
        let prev = std::env::var_os("INKENTRY_SCRIPTS_DIR");
        // SAFETY: guarded by #[serial] — no other thread in this test binary
        // reads/writes INKENTRY_SCRIPTS_DIR concurrently.
        unsafe { std::env::set_var("INKENTRY_SCRIPTS_DIR", dir) };
        f();
        unsafe {
            match &prev {
                Some(v) => std::env::set_var("INKENTRY_SCRIPTS_DIR", v),
                None => std::env::remove_var("INKENTRY_SCRIPTS_DIR"),
            }
        }
    }

    #[test]
    #[serial]
    fn web_to_md_script_path_is_config_inkentry_scripts() {
        let tmp = TempDir::new().unwrap();
        with_scripts_dir(tmp.path(), || {
            let path = web_to_md_script_path().expect("INKENTRY_SCRIPTS_DIR is set");
            assert_eq!(path, tmp.path().join("web-to-md.ts"));
        });
    }

    #[test]
    #[serial]
    fn old_home_scripts_path_is_not_used() {
        let tmp = TempDir::new().unwrap();
        let old_dir = tmp.path().join("scripts");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("web-to-md.ts"), b"// legacy script").unwrap();

        let new_dir = tmp.path().join("new-scripts");
        with_scripts_dir(&new_dir, || {
            let path = web_to_md_script_path().expect("INKENTRY_SCRIPTS_DIR is set");
            assert_ne!(
                path,
                old_dir.join("web-to-md.ts"),
                "resolved script path must not be the old ~/scripts/web-to-md.ts location"
            );
            assert!(
                !path.exists(),
                "a script only present at the old location must not be found at the \
                 resolved (new) path — the old path must be silently ignored, not \
                 still honoured"
            );
        });
    }

    #[test]
    #[serial]
    fn new_config_inkentry_scripts_path_is_used_when_present() {
        let tmp = TempDir::new().unwrap();
        let new_dir = tmp.path().join(".config").join("inkentry").join("scripts");
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(new_dir.join("web-to-md.ts"), b"// new script").unwrap();

        with_scripts_dir(&new_dir, || {
            let path = web_to_md_script_path().expect("INKENTRY_SCRIPTS_DIR is set");
            assert!(
                path.exists(),
                "script placed at the new opt-in path should be found"
            );
        });
    }

    #[test]
    fn parse_web_to_md_output_extracts_title_from_heading() {
        let md = "# My Title\n\nSome body text.";
        let (title, body) = parse_web_to_md_output(md, "https://example.com").unwrap();
        assert_eq!(title, "My Title");
        assert_eq!(body, "Some body text.");
    }

    #[test]
    fn parse_web_to_md_output_falls_back_to_url_without_heading() {
        let md = "No heading here, just body text.";
        let (title, body) = parse_web_to_md_output(md, "https://example.com/page").unwrap();
        assert_eq!(title, "https://example.com/page");
        assert_eq!(body, "No heading here, just body text.");
    }
}
