// The pull cursor is the max synced remote_id (a UUIDv7), not a wall clock, so
// clock drift between local and remote cannot skip entries.

use anyhow::{Context, Result};

use super::MemorySyncArgs;
use crate::{
    capability,
    cli::cmd::auth_api,
    config::Config,
    storage::{CloudSyncClient, MemoryStore},
};

mod local_embed;
mod pull;
#[cfg(test)]
mod pull_embed_tests;
mod push;
mod round;
#[cfg(test)]
pub(in crate::cli::cmd::memory) mod test_support;

pub(in crate::cli::cmd) use local_embed::LocalEmbedPolicy;
pub(super) use local_embed::{local_embed_summary, pending_embedding_warning, pull_embed_summary};
pub(super) use pull::parse_iso_to_secs;
pub(in crate::cli::cmd) use pull::pull_and_apply;
pub(in crate::cli::cmd) use push::push_local_oneway;
use round::{SyncRoundOutcome, sync_round};

// Never auto-derive a slug from the folder or git remote; the server creates
// the project lazily from whatever slug is sent.
fn resolve_sync_project(cli_project: Option<&str>, cfg: &Config) -> Result<String> {
    cli_project
        .map(str::to_string)
        .or_else(|| cfg.project_id.clone())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No project specified. Re-run as `inkentry sync --project <slug>` \
                 to choose the cloud project to sync into.\n\
                 (The project is created on first sync from the slug you pass; \
                 the slug is never guessed from the folder or git remote.)"
            )
        })
}

async fn sync_target(
    feature: &str,
    cfg: &Config,
    cli_project: Option<&str>,
) -> Result<(String, String, Option<String>)> {
    let base_url = capability::require_explicit_server_url(feature, cfg)?;
    let project_id = resolve_sync_project(cli_project, cfg)?;
    let key = auth_api::ensure_fresh_server_key(cfg, &base_url).await?;
    Ok((base_url, project_id, key))
}

pub async fn memory_sync(
    args: MemorySyncArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
) -> Result<()> {
    let started = std::time::Instant::now();
    let tier = capability::get_tier(cfg).await;
    capability::require_tier1("sync", tier, cfg.server_url.as_deref())?;
    let (base_url, project_id, key) = sync_target("sync", cfg, args.project.as_deref()).await?;

    let src_path = args.source.as_deref().unwrap_or(mem_path);
    let local = MemoryStore::open(src_path)
        .with_context(|| format!("opening local memory at {}", src_path.display()))?;
    let client = CloudSyncClient::new(
        &base_url,
        &project_id,
        key.as_deref(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )?;

    let accepts_pushed_vectors = tier.caps().is_some_and(|c| c.accepts_pushed_vectors);
    let local_embed = LocalEmbedPolicy::resolve(cfg, src_path);
    let SyncRoundOutcome { pushed, pulled } = sync_round(
        &local,
        &client,
        args.include_archived,
        accepts_pushed_vectors,
        &local_embed,
    )
    .await?;
    // The push stamps `remote_id`, moving its rows into the second pull's
    // repair scope, so both halves count the same vectorless row. Take the pull
    // count (measured last, includes those rows); fall back to the push count
    // when the push could not stamp.
    let pending = if pulled.without_local_vector > 0 {
        pulled.without_local_vector
    } else {
        pushed.without_local_vector
    };
    if pending > 0 {
        eprintln!("{}", pending_embedding_warning(pending));
    }
    let mut edges_note = if pushed.edges_pushed > 0 {
        format!(" Linked {} relationship edge(s).", pushed.edges_pushed)
    } else {
        String::new()
    };
    // ADR-099 D5: entries claimed locally after they had already synced.
    if pushed.anchors_pushed > 0 {
        edges_note.push_str(&format!(
            " Sent {} anchor update(s).",
            pushed.anchors_pushed
        ));
    }

    if pushed.attempted == 0 {
        println!(
            "Nothing to push: {} entries already synced. Applied {} new remote entries.{}",
            pushed.already_synced,
            pulled.applied,
            pull_embed_summary(&pulled)
        );
    } else if let Some(reason) = pushed.interrupted.as_deref() {
        // The pull already ran; still fail loud. Landed chunks are stamped, so a
        // re-run pushes only the remainder.
        anyhow::bail!(
            "Pushed {} of {} entries, then stopped: {reason}. \
             Re-run to resume (already-pushed entries are skipped). \
             Pull applied {} new remote entries.",
            pushed.created + pushed.skipped,
            pushed.attempted,
            pulled.applied
        );
    } else if pushed.created == 0 && pushed.skipped == 0 {
        // The pull already ran; still fail loud so a caller checking only the
        // exit code or skimming for "Sync complete" cannot read this as success.
        anyhow::bail!(
            "Sync failed: 0 of {} push entries reached the server ({} failed); \
             pull still applied {} new remote entries.",
            pushed.attempted,
            pushed.failed,
            pulled.applied
        );
    } else if pushed.failed > 0 {
        println!(
            "Sync complete. Pushed {} entries (created {}, skipped {}, {} failed), applied {} new remote entries.{}{}{}",
            pushed.attempted,
            pushed.created,
            pushed.skipped,
            pushed.failed,
            pulled.applied,
            local_embed_summary(&pushed),
            pull_embed_summary(&pulled),
            edges_note
        );
    } else {
        println!(
            "Sync complete. Pushed {} entries (created {}, skipped {}), applied {} new remote entries.{}{}{}",
            pushed.attempted,
            pushed.created,
            pushed.skipped,
            pulled.applied,
            local_embed_summary(&pushed),
            pull_embed_summary(&pulled),
            edges_note
        );
    }
    super::super::events::record(
        cfg,
        mem_path,
        None,
        "sync",
        None,
        Some(pulled.applied as i64 + pushed.created as i64),
        &[],
        None,
        started,
        true,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_project(id: Option<&str>) -> Config {
        Config {
            project_id: id.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_sync_project_halts_when_nothing_configured_or_passed() {
        let cfg = cfg_with_project(None);
        let err = resolve_sync_project(None, &cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--project <slug>"), "msg: {msg}");
        assert!(
            msg.contains("never guessed") || msg.contains("git remote"),
            "must state it won't auto-derive: {msg}"
        );
    }

    #[test]
    fn resolve_sync_project_uses_cli_flag_when_passed() {
        let cfg = cfg_with_project(None);
        let slug = resolve_sync_project(Some("acme/app"), &cfg).unwrap();
        assert_eq!(slug, "acme/app");
    }

    #[test]
    fn resolve_sync_project_falls_back_to_configured_id() {
        let cfg = cfg_with_project(Some("team/proj"));
        let slug = resolve_sync_project(None, &cfg).unwrap();
        assert_eq!(slug, "team/proj");
    }

    #[test]
    fn resolve_sync_project_cli_flag_overrides_configured_id() {
        let cfg = cfg_with_project(Some("team/proj"));
        let slug = resolve_sync_project(Some("other/slug"), &cfg).unwrap();
        assert_eq!(slug, "other/slug");
    }

    #[test]
    fn resolve_sync_project_treats_blank_slug_as_absent() {
        let cfg = cfg_with_project(None);
        assert!(resolve_sync_project(Some("   "), &cfg).is_err());
    }

    #[tokio::test]
    async fn first_run_project_flag_only_passes_dispatch_and_reaches_wire() {
        use crate::storage::{BatchPushItem, CloudSyncClient};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let cfg = Config {
            server_url: Some("http://inkentry.internal:4655".to_string()),
            project_id: None,
            ..Default::default()
        };
        let cli_project = Some("acme/app");

        // `--project` makes a project available, so a non-loopback server_url
        // must not block dispatch validation.
        let project_available = cli_project.is_some() || cfg.project_id.is_some();
        cfg.validate_with_project(project_available)
            .expect("first-run --project must pass dispatch validation");

        let slug = resolve_sync_project(cli_project, &cfg).unwrap();
        assert_eq!(slug, "acme/app");

        // The slug must land percent-encoded in the request path.
        Mock::given(method("POST"))
            .and(path("/v1/projects/acme%2Fapp/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": "e1", "id": "cloud-1"}]
            })))
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), &slug, None, None).unwrap();
        let res = client
            .push_batch(vec![BatchPushItem {
                id: None,
                kind: "decision".into(),
                title: "T".into(),
                body: Some("B".into()),
                external_id: "e1".into(),
                source_commit: None,
                vector: None,
                vector_model: None,
                vector_precision: None,
            }])
            .await
            .expect("push to the lazily-created project must succeed");
        assert_eq!(res.created, 1);
    }
}
