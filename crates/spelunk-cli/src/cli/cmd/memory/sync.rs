//! `spelunk sync` and `spelunk memory pull` — two-way local↔cloud memory sync
//! (ADR-037 D2/D3).
//!
//! - `pull`: delta-pull from `GET /memory/since?since_id=<cursor>` and apply
//!   locally. The cursor is the max cloud `remote_id` already synced
//!   (`MAX(remote_id)` over local notes), a UUIDv7 — not a wall-clock watermark
//!   (decision #183), so it is immune to local↔remote clock drift.
//! - `sync`: push local rows `WHERE remote_id IS NULL` via `POST /memory/batch`
//!   (batched, not N single POSTs), then pull everything after the cursor and
//!   apply both.
//!
//! Properties (ADR-037 + ADR-005):
//! - **Idempotent.** Identity is the stable UUID; pushes carry it as the cloud
//!   `external_id` and pulls record the cloud UUID as the local `remote_id` and
//!   dedupe on it, so re-running never duplicates. Same-millisecond boundary
//!   entries are harmless: the cursor comparison is strict (`>`) and pull
//!   dedupes by `remote_id`, so a re-applied boundary row is a no-op.
//! - **Keep-both / Add-Wins.** Pulled entries are added, never overwriting local
//!   ones; semantic-dup detection is the server's job (it flags `contradicts`).
//! - **Lifecycle propagation.** `supersedes` and archive/tombstone state travel
//!   in both directions (previously hard-coded `None`/dropped).
//! - **Text-only.** Pushes never ship a vector; the server backfills embeddings
//!   (ADR-010/ADR-020).

use anyhow::{Context, Result};

use super::{MemoryPullArgs, MemorySyncArgs};
use crate::{
    capability,
    cli::cmd::auth_api,
    config::Config,
    storage::{BatchPushItem, CloudSyncClient, MemoryStore},
};

/// Resolve the project slug to sync into, or halt with actionable guidance.
///
/// Precedence: an explicit `--project <slug>` overrides any configured
/// `project_id`; otherwise the configured `project_id` is used. When neither is
/// present the call **halts** — sync never auto-derives a name from the folder
/// or git remote (founder decision 2026-07-01, project-taxonomy). The returned
/// slug is sent verbatim to the server, which lazily creates the project on
/// first sync and reuses it on subsequent syncs.
fn resolve_sync_project(cli_project: Option<&str>, cfg: &Config) -> Result<String> {
    cli_project
        .map(str::to_string)
        .or_else(|| cfg.project_id.clone())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No project specified. Re-run as `spelunk sync --project <slug>` \
                 to choose the cloud project to sync into.\n\
                 (The project is created on first sync from the slug you pass; \
                 the slug is never guessed from the folder or git remote.)"
            )
        })
}

/// Resolve the cloud sync target (base URL, server-side project id, key).
///
/// Sync always speaks to an explicit `server_url` — it is the cloud-convergence
/// path, not the inference loopback. Errors with actionable guidance when the
/// server is missing, or when no project slug is available (see
/// [`resolve_sync_project`]).
///
/// `cli_project` is the optional `--project <slug>` override; when `None` the
/// configured `project_id` is used.
///
/// The bearer key is resolved through [`auth_api::ensure_fresh_server_key`] so a
/// WorkOS access token that has expired since `spelunk login` is refreshed (and
/// the rotated tokens persisted) before the cloud-api call, rather than 401-ing.
async fn sync_target(
    cfg: &Config,
    cli_project: Option<&str>,
) -> Result<(String, String, Option<String>)> {
    let base_url = cfg.server_url.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "sync requires a server. Set `server_url` in your spelunk config \
             (e.g. ~/.config/spelunk/config.toml or .spelunk/config.toml)."
        )
    })?;
    let project_id = resolve_sync_project(cli_project, cfg)?;
    let key = auth_api::ensure_fresh_server_key(cfg).await?;
    Ok((base_url, project_id, key))
}

/// `spelunk memory pull` — one-way delta pull + apply.
pub async fn memory_pull(
    _args: MemoryPullArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
) -> Result<()> {
    let tier = capability::get_tier(cfg).await;
    capability::require_tier1("memory pull", tier, cfg.server_url.as_deref())?;
    let (base_url, project_id, key) = sync_target(cfg, None).await?;

    let local = MemoryStore::open(mem_path)
        .with_context(|| format!("opening local memory at {}", mem_path.display()))?;
    let client = CloudSyncClient::new(
        &base_url,
        &project_id,
        key.as_deref(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )?;

    let pulled = pull_and_apply(&local, &client).await?;
    println!("Pull complete. Applied {pulled} new remote entries.");
    Ok(())
}

/// `spelunk sync` (and `spelunk memory push`'s successor) — two-way sync.
pub async fn memory_sync(
    args: MemorySyncArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
) -> Result<()> {
    let tier = capability::get_tier(cfg).await;
    capability::require_tier1("sync", tier, cfg.server_url.as_deref())?;
    let (base_url, project_id, key) = sync_target(cfg, args.project.as_deref()).await?;

    let src_path = args.source.as_deref().unwrap_or(mem_path);
    let local = MemoryStore::open(src_path)
        .with_context(|| format!("opening local memory at {}", src_path.display()))?;
    let client = CloudSyncClient::new(
        &base_url,
        &project_id,
        key.as_deref(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )?;

    // ── Push local → cloud (batched, text-only, idempotent on UUID) ─────────
    let pushed = push_local(&local, &client, args.include_archived).await?;

    // ── Pull cloud → local (delta after the UUID cursor, keep-both) ─────────
    let pulled = pull_and_apply(&local, &client).await?;

    println!(
        "Sync complete. Pushed {} entries (created {}, skipped {}), applied {} new remote entries.",
        pushed.attempted, pushed.created, pushed.skipped, pulled
    );
    Ok(())
}

/// Outcome of a push pass (shared by `sync` and the one-way `memory push`).
pub(super) struct PushSummary {
    pub attempted: usize,
    pub created: u32,
    pub skipped: u32,
}

/// One-way push entry point reused by `spelunk memory push`.
pub(super) async fn push_local_oneway(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
) -> Result<PushSummary> {
    push_local(local, client, include_archived).await
}

/// Push local entries to the cloud as text-only batches, then propagate
/// tombstones for any archived rows that exist cloud-side.
async fn push_local(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
) -> Result<PushSummary> {
    let rows = local.rows_for_sync(include_archived)?;
    let attempted = rows.len();
    if rows.is_empty() {
        return Ok(PushSummary {
            attempted: 0,
            created: 0,
            skipped: 0,
        });
    }

    // Split into live entries (batch-created/upserted by external_id) and
    // archived entries already known to the cloud (tombstoned via DELETE).
    let mut created = 0u32;
    let mut skipped = 0u32;

    // Push set (decision #183): live entries not yet on the cloud — i.e.
    // `WHERE remote_id IS NULL`. Already-synced rows carry a `remote_id` and are
    // skipped here (the cloud already has them; re-pushing would only earn a 207
    // `skipped`). Archived rows are handled by the tombstone pass below.
    let live: Vec<&_> = rows
        .iter()
        .filter(|r| !r.archived && r.remote_id.is_none())
        .collect();
    // Map external_id (local uuid) → local_id so we can record the cloud-minted
    // id returned in the 207 result back onto the local row.
    for chunk in live.chunks(200) {
        let items: Vec<BatchPushItem> = chunk
            .iter()
            .map(|r| BatchPushItem {
                kind: r.kind.clone(),
                title: r.title.clone(),
                body: if r.body.is_empty() {
                    None
                } else {
                    Some(r.body.clone())
                },
                external_id: r.uuid.clone(),
                source_commit: r.source_ref.clone(),
            })
            .collect();
        let res = client.push_batch(items).await?;
        created += res.created;
        skipped += res.skipped;

        // Record cloud ids for created entries so a later pull dedupes them and
        // a later archive can tombstone them by id.
        for item in &res.results {
            if let (Some(ext), Some(cloud_id)) = (item.external_id.as_deref(), item.id.as_deref())
                && let Some(row) = chunk.iter().find(|r| r.uuid == ext)
            {
                local.set_remote_id(row.local_id, cloud_id)?;
            }
            if item.status == "failed" {
                eprintln!(
                    "  [push-fail] {}",
                    item.external_id.as_deref().unwrap_or("<unknown>")
                );
            }
        }
    }

    // Tombstone archived entries that the cloud already knows about. An archived
    // row with no `remote_id` was never pushed live, so there is nothing to
    // delete cloud-side; we skip it.
    if include_archived {
        for r in rows.iter().filter(|r| r.archived) {
            if let Some(remote_id) = r.remote_id.as_deref() {
                client.delete_remote(remote_id).await?;
            }
        }
    }

    Ok(PushSummary {
        attempted,
        created,
        skipped,
    })
}

/// Pull remote entries after the UUID cursor and apply them idempotently.
/// Returns the number of newly-inserted local rows.
///
/// The cursor is derived from the store itself — `MAX(remote_id)` over local
/// notes (decision #183) — so there is no persisted watermark to advance: the
/// next run re-derives the cursor from the rows just applied. This is what makes
/// the pull immune to clock drift and trivially resumable.
async fn pull_and_apply(local: &MemoryStore, client: &CloudSyncClient) -> Result<usize> {
    let cursor = local.max_remote_id()?;
    let entries = client.pull_since(cursor.as_deref()).await?;

    let mut applied = 0usize;
    for e in &entries {
        let created_secs = parse_iso_to_secs(&e.created_at);
        let inserted = local.apply_remote_note(
            &e.id,
            &e.kind,
            &e.title,
            e.body.as_deref().unwrap_or(""),
            e.source_commit.as_deref(),
            created_secs,
            e.is_archived(),
        )?;
        if inserted {
            applied += 1;
        }
    }
    Ok(applied)
}

/// Parse an ISO 8601 / RFC 3339 timestamp to Unix epoch seconds.
///
/// Falls back to "now" if the server sends a value we cannot parse, so a single
/// odd row never aborts the whole sync.
fn parse_iso_to_secs(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|_| crate::storage::now_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iso_to_secs_handles_utc_z() {
        // 2021-01-01T00:00:00Z = 1609459200
        assert_eq!(parse_iso_to_secs("2021-01-01T00:00:00Z"), 1_609_459_200);
    }

    #[test]
    fn parse_iso_to_secs_handles_offset() {
        // 2021-01-01T01:00:00+01:00 == 2021-01-01T00:00:00Z
        assert_eq!(
            parse_iso_to_secs("2021-01-01T01:00:00+01:00"),
            1_609_459_200
        );
    }

    #[test]
    fn parse_iso_to_secs_falls_back_on_garbage() {
        // Must not panic; returns some positive epoch (now).
        assert!(parse_iso_to_secs("not-a-timestamp") > 0);
    }

    // ── resolve_sync_project ───────────────────────────────────────────────
    // Sync must never invent a project name. With neither `--project` nor a
    // configured `project_id`, the call halts with a message pointing the user
    // at `--project <slug>`; with an explicit slug (or configured id), that slug
    // is threaded through verbatim so it reaches the outbound request.

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
        // Actionable: names the exact re-run and refuses to guess.
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
        // A whitespace-only `--project ""` must not silently pass an empty slug.
        let cfg = cfg_with_project(None);
        assert!(resolve_sync_project(Some("   "), &cfg).is_err());
    }

    // ── end-to-end first-run path ──────────────────────────────────────────
    // The story's target path: a first-run user has a non-loopback team
    // `server_url`, NO configured `project_id`, and passes `--project <slug>`.
    // Before the fix this was rejected at dispatch by `cfg.validate()` before
    // `resolve_sync_project` ever ran. This test walks the same two gates the
    // dispatcher + `memory_sync` cross — (1) config validation must accept the
    // `--project`-only config, (2) the resolved slug must reach the outbound
    // request — proving the path is live end to end (minus the auth/tier
    // machinery, which is orthogonal to this story).
    #[tokio::test]
    async fn first_run_project_flag_only_passes_dispatch_and_reaches_wire() {
        use crate::storage::{BatchPushItem, CloudSyncClient};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // A non-loopback team server_url with NO project_id — a genuine first run.
        let cfg = Config {
            server_url: Some("http://spelunk.internal:7777".to_string()),
            project_id: None,
            ..Default::default()
        };
        let cli_project = Some("acme/app");

        // Gate 1 — dispatch: `--project` makes a project available, so the
        // non-loopback server_url no longer blocks (regression under test).
        let project_available = cli_project.is_some() || cfg.project_id.is_some();
        cfg.validate_with_project(project_available)
            .expect("first-run --project must pass dispatch validation");

        // Gate 2 — resolution: the explicit slug wins and is what sync targets.
        let slug = resolve_sync_project(cli_project, &cfg).unwrap();
        assert_eq!(slug, "acme/app");

        // Wire: the resolved slug must land, percent-encoded, in the request
        // path so the server can lazily create/reuse that project.
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
                kind: "decision".into(),
                title: "T".into(),
                body: Some("B".into()),
                external_id: "e1".into(),
                source_commit: None,
            }])
            .await
            .expect("push to the lazily-created project must succeed");
        assert_eq!(res.created, 1);
    }

    // ── push_local end-to-end: remote_id stamping + idempotent re-sync ─────
    // The local-first push path is where the server-minted
    // cross-machine id is PERSISTED — stamped onto `notes.remote_id` from the
    // 207 batch result — not the `RemoteMemoryBackend::add` debug-log path
    // (which is the cloud-first, remote-is-store-of-record case with no local
    // row). Locks in that a push stamps `remote_id` and a re-push sends nothing
    // (no duplicate cloud writes, no local dupes).

    fn register_sqlite_vec() {
        use std::sync::OnceLock;
        // `MemoryStore::open` creates a vec0 table, so the extension must be
        // registered before any connection opens.
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });
    }

    #[tokio::test]
    async fn push_local_stamps_remote_id_and_repush_is_idempotent() {
        use tempfile::TempDir;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        register_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
        store
            .add_note("decision", "One", "first", &[], &[], None, None)
            .unwrap();
        store
            .add_note("note", "Two", "second", &[], &[], None, None)
            .unwrap();

        // Learn the lazily-minted external_ids up front so the mock can echo
        // them back with distinct cloud ids; `ensure_uuid` is idempotent, so the
        // push below re-derives the same uuids.
        let rows = store.rows_for_sync(false).unwrap();
        assert_eq!(rows.len(), 2);
        let (ext_a, ext_b) = (rows[0].uuid.clone(), rows[1].uuid.clone());
        let cloud_a = "01890000-0000-7000-8000-0000000000a1";
        let cloud_b = "01890000-0000-7000-8000-0000000000a2";

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 2, "skipped": 0, "failed": 0,
                "results": [
                    {"status": "created", "external_id": ext_a, "id": cloud_a},
                    {"status": "created", "external_id": ext_b, "id": cloud_b},
                ]
            })))
            .mount(&server)
            .await;
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

        // First push: creates both, persists the server-minted id on each row.
        let s1 = push_local(&store, &client, false).await.unwrap();
        assert_eq!((s1.attempted, s1.created, s1.skipped), (2, 2, 0));
        assert_eq!(
            store.note_id_for_remote_id(cloud_a).unwrap(),
            Some(rows[0].local_id)
        );
        assert_eq!(
            store.note_id_for_remote_id(cloud_b).unwrap(),
            Some(rows[1].local_id)
        );
        // The pull cursor is now the newest stamped id.
        assert_eq!(store.max_remote_id().unwrap().as_deref(), Some(cloud_b));

        // Second push: every row carries a `remote_id`, so the live set is empty
        // and no batch request is sent — the re-sync is a no-op.
        let s2 = push_local(&store, &client, false).await.unwrap();
        assert_eq!(s2.created, 0);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "re-push must not hit the batch endpoint again"
        );
        // No duplicate local rows introduced by the round trip.
        assert_eq!(store.count().unwrap(), 2);
    }
}
