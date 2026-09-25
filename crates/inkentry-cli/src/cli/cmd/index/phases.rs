use anyhow::Result;
use indicatif::MultiProgress;

use super::IndexArgs;
use crate::cli::cmd::embed_worker::EmbedWorkerGuard;
use crate::{capability, config::Config, registry::Registry, storage::Database};

use super::{embed_phase, parse_phase, summaries, tier3};

const EMBED_WAIT_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);
// The wait itself is unbounded while the embedder reports `loading`: a model
// download can take many minutes and the queue is durable.
const EMBED_WAIT_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);
// Tells a crashed server from a momentarily unreachable one. Explicit opt-outs
// never count; they return on the first probe.
const EMBED_WAIT_MAX_OFFLINE_PROBES: u32 = 10;

// `loading` is waited out, never skipped: health goes live at socket bind,
// before the model loads. Uses the `_fresh` tier so every poll re-resolves
// local-vs-remote routing instead of reusing `get_tier`'s cached first probe.
async fn wait_for_embedder(
    cfg: &Config,
    initial_backoff: std::time::Duration,
    max_backoff: std::time::Duration,
) -> capability::Tier {
    let mut backoff = initial_backoff;
    let mut offline_probes = 0u32;
    let mut announced = false;
    loop {
        let tier = capability::get_inference_tier_fresh(cfg).await;
        match &tier {
            capability::Tier::Server { .. } => {
                if matches!(tier.caps(), Some(c) if c.index_embed) {
                    return tier;
                }
                if !matches!(
                    tier.embedder_state(),
                    Some(capability::EmbedderState::Loading)
                ) {
                    return tier;
                }
                offline_probes = 0;
                if !announced {
                    eprintln!("Waiting for the embedder to finish loading\u{2026}");
                    announced = true;
                }
            }
            capability::Tier::Offline(reason) => {
                // Decided before any socket opens and fixed for the process, so
                // every later probe would return this same tier.
                if reason.is_explicit_opt_out() {
                    return tier;
                }
                offline_probes += 1;
                if offline_probes >= EMBED_WAIT_MAX_OFFLINE_PROBES {
                    return tier;
                }
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

pub(super) async fn run_embed_phases(
    args: &IndexArgs,
    cfg: &Config,
    db: &Database,
    project_root: &std::path::Path,
    root_canonical: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<()> {
    // Held through the readiness wait too, so `inkentry status` does not advise
    // a resume that would double up a worker still waiting on the embedder.
    let worker_guard = EmbedWorkerGuard::acquire(db, db_path);

    let tier = wait_for_embedder(cfg, EMBED_WAIT_INITIAL_BACKOFF, EMBED_WAIT_MAX_BACKOFF).await;
    let embed_ready = matches!(tier.caps(), Some(c) if c.index_embed);
    if tier.is_server() && embed_ready {
        let chunk_ids_and_texts = parse_phase::missing_embedding_texts(db)?;
        if !chunk_ids_and_texts.is_empty() {
            let mp = MultiProgress::new();
            embed_phase::run_embed_phase(
                chunk_ids_and_texts,
                db,
                cfg,
                &tier,
                project_root,
                args.batch_size,
                &mp,
            )
            .await?;
        }
    } else {
        eprint_embed_skipped_notice(&tier, cfg);
    }
    drop(worker_guard);

    run_post_embed_phases(args, cfg, db, project_root, root_canonical, db_path).await
}

fn embed_skipped_lines(
    tier: &capability::Tier,
    server_url: Option<&str>,
    is_windows: bool,
) -> Vec<String> {
    use capability::{EmbedderState, Tier};
    match tier {
        Tier::Server {
            embedder_state: EmbedderState::Loading,
            ..
        } => vec![
            "Note: the embedder is still warming up — chunks indexed for full-text search."
                .to_string(),
            "Re-run `inkentry index` in a moment to add embeddings (check `inkentry server status`)."
                .to_string(),
        ],
        Tier::Server {
            embedder_state: EmbedderState::Unavailable,
            ..
        } => match tier.explicit_remote_url() {
            Some(url) => vec![
                format!(
                    "Warning: the embedder failed to load on team server {url}; chunks indexed \
                     for full-text search only."
                ),
                "Check that server's own logs for the load error, then re-run `inkentry index`."
                    .to_string(),
            ],
            None => vec![
                "Warning: the embedder failed to load; chunks indexed for full-text search \
                 only."
                    .to_string(),
                "See `inkentry server logs` for the load error, then re-run `inkentry index`."
                    .to_string(),
            ],
        },
        // `disabled`, or an older server that never advertised `index.embed`.
        Tier::Server { .. } => vec![
            "Note: this server has no embedder — chunks indexed for full-text search only."
                .to_string(),
        ],
        Tier::Offline(reason) => embed_skipped_offline_lines(*reason, server_url, is_windows),
    }
}

// Keyed to the probe's recorded reason, not to whether `server_url` is set:
// under `local_first` the probe is loopback even when `server_url` points
// elsewhere, so a config-derived notice would name a server never contacted.
fn embed_skipped_offline_lines(
    reason: capability::OfflineReason,
    server_url: Option<&str>,
    is_windows: bool,
) -> Vec<String> {
    use capability::OfflineReason;
    if let Some(advice) = capability::shared_offline_advice(reason) {
        // Explicit opt-outs are not warnings.
        let prefix = match reason {
            OfflineReason::KillSwitch
            | OfflineReason::ModeOfflineEnv
            | OfflineReason::ModeOfflineConfig => "Note",
            _ => "Warning",
        };
        return vec![
            format!("{prefix}: {advice}."),
            "Chunks are indexed for full-text search; re-run `inkentry index` afterwards \
             to add embeddings."
                .to_string(),
        ];
    }
    match reason {
        OfflineReason::ExplicitServerUnavailable => {
            let target = match server_url {
                Some(url) => format!("server_url is explicitly configured to {url}, which is"),
                None => "the configured server_url is".to_string(),
            };
            let mut lines = vec![format!(
                "Warning: {target} unreachable, so the embedding phase is skipped. This \
                 overrides the auto-discovered local server, so a healthy `inkentry server \
                 start` daemon elsewhere will not be used while server_url is set."
            )];
            if is_windows {
                lines.push(
                    "On Windows, allow the loopback listener through Defender Firewall \
                     (accept the prompt on `inkentry server start`)."
                        .to_string(),
                );
            }
            lines.push(
                "Chunks are indexed for full-text search. Re-run `inkentry index` once \
                 the server is reachable to add embeddings."
                    .to_string(),
            );
            lines
        }
        // The only other reason `shared_offline_advice` declines.
        _ => vec![
            "Note: start a local server (`inkentry server start`) to enable semantic search."
                .to_string(),
        ],
    }
}

pub(super) fn eprint_embed_skipped_notice(tier: &capability::Tier, cfg: &Config) {
    for line in embed_skipped_lines(tier, cfg.server_url.as_deref(), cfg!(windows)) {
        eprintln!("{line}");
    }
}

// Must precede the first embed: PageRank orders the queue central-first on a
// cold index, and the first vector should already carry its summary.
pub(super) fn run_pre_embed_phases(args: &IndexArgs, db: &Database) -> Result<()> {
    eprintln!("Computing graph rank\u{2026}");
    let edges = db.graph_edges_all()?;
    if !edges.is_empty() {
        let pr_scores = crate::indexer::pagerank::compute_pagerank(&edges, 20, 0.85);
        let named_chunks = db.chunks_with_names()?;
        let updates: Vec<(i64, f32)> = named_chunks
            .into_iter()
            .filter_map(|(id, name)| name.and_then(|n| pr_scores.get(&n).copied().map(|s| (id, s))))
            .collect();
        if !updates.is_empty() {
            db.update_graph_ranks(&updates)?;
        }
    }

    if !args.no_summaries {
        summaries::generate_structural_summaries(db)?;
    }
    Ok(())
}

pub(super) async fn run_post_embed_phases(
    args: &IndexArgs,
    cfg: &Config,
    db: &Database,
    project_root: &std::path::Path,
    root_canonical: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<()> {
    // Needs a ready embedder; when skipped, candidates keep `summary IS NULL`
    // and are retried on the next index.
    if !args.no_summaries {
        let tier = capability::get_inference_tier(cfg).await;
        if tier.is_server() && matches!(tier.caps(), Some(c) if c.index_embed) {
            match tier3::run_tier3_selection(cfg, db).await {
                Ok(refined) if refined > 0 => {
                    let pending = parse_phase::missing_embedding_texts(db)?;
                    if !pending.is_empty() {
                        let mp = MultiProgress::new();
                        let worker_guard = EmbedWorkerGuard::acquire(db, db_path);
                        embed_phase::run_embed_phase(
                            pending,
                            db,
                            cfg,
                            &tier,
                            project_root,
                            args.batch_size,
                            &mp,
                        )
                        .await?;
                        drop(worker_guard);
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln!("Warning: tier-3 refinement failed: {e:#}"),
            }
        }
    }

    eprintln!("Extracting conventions\u{2026}");
    match crate::conventions::run_extraction(db) {
        Ok(records) => {
            if !records.is_empty() {
                eprintln!("Conventions: {} record(s) detected.", records.len());
            }
        }
        Err(e) => tracing::warn!("convention extraction failed (non-fatal): {e}"),
    }

    if let Ok(reg) = Registry::open() {
        let db_canonical = inkentry_core::utils::canonicalize(db_path);
        if let Err(e) = reg.register(root_canonical, &db_canonical) {
            tracing::warn!("registry update failed: {e}");
        }
    }
    Ok(())
}

pub(super) async fn run_background_phases(
    args: &IndexArgs,
    cfg: &Config,
    db: &Database,
    project_root: &std::path::Path,
    root_canonical: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<()> {
    run_post_embed_phases(args, cfg, db, project_root, root_canonical, db_path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_tier(
        state: capability::EmbedderState,
        auto_discovered: bool,
        url: &str,
    ) -> capability::Tier {
        capability::Tier::Server {
            url: url.to_string(),
            caps: capability::Capabilities::all(),
            auto_discovered,
            embedder_state: state,
            server_limits: None,
        }
    }

    #[test]
    fn embed_skipped_loading_advises_retry() {
        let tier = server_tier(
            capability::EmbedderState::Loading,
            true,
            "http://127.0.0.1:4655",
        );
        let lines = embed_skipped_lines(&tier, None, false);
        assert!(!lines.is_empty(), "notice must not be silent");
        let joined = lines.join("\n");
        assert!(joined.contains("warming up"));
        assert!(joined.contains("Re-run `inkentry index`"));
    }

    #[test]
    fn embed_skipped_unavailable_loopback_points_at_logs() {
        let tier = server_tier(
            capability::EmbedderState::Unavailable,
            true,
            "http://127.0.0.1:4655",
        );
        let lines = embed_skipped_lines(&tier, None, false);
        let joined = lines.join("\n");
        assert!(joined.contains("failed to load"));
        assert!(joined.contains("inkentry server logs"));
    }

    #[test]
    fn embed_skipped_unavailable_remote_names_that_server_never_local_logs() {
        let tier = server_tier(
            capability::EmbedderState::Unavailable,
            false,
            "https://team.example:4655",
        );
        let lines = embed_skipped_lines(&tier, None, false);
        let joined = lines.join("\n");
        assert!(joined.contains("failed to load"));
        assert!(
            joined.contains("https://team.example:4655"),
            "got: {joined}"
        );
        assert!(
            !joined.contains("inkentry server logs"),
            "must not point a remote failure at local logs: {joined}"
        );
    }

    #[test]
    fn embed_skipped_unreachable_server_names_configured_server_url() {
        let tier = capability::Tier::Offline(capability::OfflineReason::ExplicitServerUnavailable);
        let lines = embed_skipped_lines(&tier, Some("http://127.0.0.1:4655"), false);
        let joined = lines.join("\n");
        assert!(joined.contains("http://127.0.0.1:4655"), "got: {joined}");
        assert!(joined.contains("unreachable"), "got: {joined}");
        assert!(joined.contains("server_url"), "got: {joined}");
        assert!(
            joined.contains("configured"),
            "must say the target came from a *configured* server_url, not just name \
             `server_url` in passing (this is the specific wording the defect asked for, \
             distinguishing it from the auto-discovered daemon): got: {joined}"
        );
        assert!(
            joined.contains("overrides") || joined.contains("override"),
            "must explain that an explicit server_url overrides the auto-discovered \
             local daemon, so a healthy daemon elsewhere is not the fix: got: {joined}"
        );
    }

    #[test]
    fn embed_skipped_unreachable_server_shows_firewall_hint_only_on_windows() {
        let tier = capability::Tier::Offline(capability::OfflineReason::ExplicitServerUnavailable);
        let windows_lines = embed_skipped_lines(&tier, Some("http://127.0.0.1:4655"), true);
        assert!(
            windows_lines.join("\n").contains("Firewall"),
            "the Windows hint must still show when the host platform is Windows"
        );

        let non_windows_lines = embed_skipped_lines(&tier, Some("http://127.0.0.1:4655"), false);
        assert!(
            !non_windows_lines.join("\n").contains("Firewall"),
            "the Windows-only hint must not print on a non-Windows host: got: {:?}",
            non_windows_lines
        );
    }

    #[test]
    fn embed_skipped_no_server_suggests_starting_one() {
        let tier = capability::Tier::Offline(capability::OfflineReason::NoLocalServer);
        let lines = embed_skipped_lines(&tier, None, false);
        let joined = lines.join("\n");
        assert!(joined.contains("inkentry server start"));
    }

    const NO_LOCAL_SERVER_NOTICE: &str =
        "Note: start a local server (`inkentry server start`) to enable semantic search.";

    #[test]
    fn embed_skipped_recorded_daemon_refused_by_discovery_does_not_ask_for_a_fresh_start() {
        let tier = capability::Tier::Offline(capability::OfflineReason::RecordedServerUnreachable);
        for server_url in [None, Some("https://team.example:4655")] {
            let joined = embed_skipped_lines(&tier, server_url, false).join("\n");
            assert_ne!(joined, NO_LOCAL_SERVER_NOTICE);
            assert!(
                joined.contains("could not be identified"),
                "must name the cause the warning above named: {joined}"
            );
            assert!(
                joined.contains("inkentry server stop"),
                "must carry the same stop/start remedy: {joined}"
            );
        }
    }

    #[test]
    fn embed_skipped_local_server_unusable_names_the_dimension_mismatch() {
        let tier = capability::Tier::Offline(capability::OfflineReason::LocalServerUnusable);
        for server_url in [None, Some("https://team.example:4655")] {
            let joined = embed_skipped_lines(&tier, server_url, false).join("\n");
            assert_ne!(joined, NO_LOCAL_SERVER_NOTICE);
            assert!(joined.contains("different dimension"), "{joined}");
            assert!(joined.contains("inkentry server stop"), "{joined}");
        }
    }

    #[test]
    fn embed_skipped_genuinely_no_server_and_no_server_url_keeps_its_existing_text() {
        let tier = capability::Tier::Offline(capability::OfflineReason::NoLocalServer);
        assert_eq!(
            embed_skipped_lines(&tier, None, false),
            vec![NO_LOCAL_SERVER_NOTICE.to_string()]
        );
    }

    #[test]
    fn embed_skipped_explicit_offline_opt_out_names_the_switch_not_a_server_to_start() {
        for reason in [
            capability::OfflineReason::KillSwitch,
            capability::OfflineReason::ModeOfflineEnv,
            capability::OfflineReason::ModeOfflineConfig,
        ] {
            let tier = capability::Tier::Offline(reason);
            let joined =
                embed_skipped_lines(&tier, Some("https://team.example:4655"), false).join("\n");
            assert!(
                !joined.contains("inkentry server start"),
                "{reason:?} offers a server start that cannot take effect: {joined}"
            );
        }
    }

    #[test]
    fn a_loopback_offline_reason_never_names_the_configured_server_url() {
        for reason in [
            capability::OfflineReason::NoLocalServer,
            capability::OfflineReason::LocalServerUnusable,
            capability::OfflineReason::RecordedServerUnreachable,
        ] {
            let tier = capability::Tier::Offline(reason);
            let lines = embed_skipped_lines(&tier, Some("https://team.example:4655"), false);
            let joined = lines.join(" ");
            assert!(
                !joined.contains("https://team.example:4655"),
                "{reason:?} names a server the embed phase never contacted: {joined}"
            );
        }
    }

    #[test]
    fn status_and_the_command_notices_agree_on_every_offline_reason() {
        const URL: &str = "https://team.example:4655";
        for reason in capability::ALL_OFFLINE_REASONS {
            let tier = capability::Tier::Offline(reason);
            let status = capability::offline_search_hint(reason, None);
            let search =
                crate::cli::cmd::search::semantic_unavailable_message(&tier, Some(URL), false);
            let index = embed_skipped_lines(&tier, Some(URL), false).join("\n");

            if let Some(advice) = capability::shared_offline_advice(reason) {
                for (surface, text) in [("status", &status), ("search", &search), ("index", &index)]
                {
                    assert!(
                        text.contains(advice),
                        "{reason:?}: {surface} does not render the shared advice\n                           advice: {advice}\n  {surface}: {text}"
                    );
                }
            }

            // status renders a transport annotation here, not advice.
            if reason == capability::OfflineReason::ExplicitServerUnavailable {
                continue;
            }
            for (surface, text) in [("search", &search), ("index", &index)] {
                for remedy in ["inkentry server start", "inkentry server stop"] {
                    assert_eq!(
                        text.contains(remedy),
                        status.contains(remedy),
                        "{reason:?}: status and {surface} disagree on whether \
                         `{remedy}` is the fix\n  status: {status}\n  {surface}: {text}"
                    );
                }
            }
        }
    }

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn health_body(state: &str) -> serde_json::Value {
        let (caps, dim) = if state == "ready" {
            (
                vec!["memory", "index.embed", "search.semantic"],
                inkentry_core::embeddings::EMBEDDING_DIM,
            )
        } else {
            (vec!["memory"], 0)
        };
        serde_json::json!({
            "status": "ok",
            "version": "0.9.3",
            "capabilities": caps,
            "instance_id": "00000000-0000-0000-0000-000000000001",
            "embedding_dim": dim,
            "embedder": { "state": state, "detail": null }
        })
    }

    // cloud_first so `wait_for_embedder` probes `url`; under local_first it
    // routes to loopback and never touches `server_url`.
    fn cfg_for(url: String) -> Config {
        Config {
            server_url: Some(url),
            project_id: Some("local/test".to_string()),
            mode: Some(crate::config::SyncMode::CloudFirst),
            ..Default::default()
        }
    }

    const TEST_BACKOFF: std::time::Duration = std::time::Duration::from_millis(1);

    #[tokio::test]
    async fn wait_for_embedder_outlasts_a_loading_embedder() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("loading")))
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("ready")))
            .mount(&mock)
            .await;

        let tier = wait_for_embedder(&cfg_for(mock.uri()), TEST_BACKOFF, TEST_BACKOFF).await;
        assert!(
            matches!(tier.caps(), Some(c) if c.index_embed),
            "the wait must return only once the embedder serves; got {tier:?}"
        );
        assert_eq!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Ready)
        );
    }

    #[tokio::test]
    async fn wait_for_embedder_treats_unavailable_as_terminal() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("unavailable")))
            .expect(1)
            .mount(&mock)
            .await;

        let tier = wait_for_embedder(&cfg_for(mock.uri()), TEST_BACKOFF, TEST_BACKOFF).await;
        assert_eq!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Unavailable)
        );
        assert!(!matches!(tier.caps(), Some(c) if c.index_embed));
    }

    #[tokio::test]
    async fn wait_for_embedder_treats_disabled_as_terminal() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("disabled")))
            .expect(1)
            .mount(&mock)
            .await;

        let tier = wait_for_embedder(&cfg_for(mock.uri()), TEST_BACKOFF, TEST_BACKOFF).await;
        assert_eq!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Disabled)
        );
    }

    #[tokio::test]
    async fn wait_for_embedder_loading_then_unavailable_is_terminal() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("loading")))
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("unavailable")))
            .mount(&mock)
            .await;

        let tier = wait_for_embedder(&cfg_for(mock.uri()), TEST_BACKOFF, TEST_BACKOFF).await;
        assert_eq!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Unavailable),
            "the terminal state observed mid-wait must be returned as-is"
        );
        assert!(!matches!(tier.caps(), Some(c) if c.index_embed));
    }

    #[tokio::test]
    async fn wait_for_embedder_offline_counter_resets_on_a_reachable_probe() {
        // 7 offline + loading + 7 offline: 14 cumulative, never 10 in a row.
        // A non-2xx health response probes as Tier::Offline.
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(7)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("loading")))
            .up_to_n_times(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(7)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("ready")))
            .mount(&mock)
            .await;

        let tier = wait_for_embedder(&cfg_for(mock.uri()), TEST_BACKOFF, TEST_BACKOFF).await;
        assert!(
            matches!(tier.caps(), Some(c) if c.index_embed),
            "14 cumulative but never {EMBED_WAIT_MAX_OFFLINE_PROBES} consecutive offline \
             probes must not trip the give-up; got {tier:?}"
        );
    }

    #[test]
    fn the_memo_ttl_is_short_enough_for_this_poller_to_look_again() {
        // Polls land at cumulative 1, 3, 7, 15, 31s. Each poll inside the TTL is
        // skipped and counts against the offline tolerance, so a long TTL would
        // end the wait instead of deferring it.
        let mut skipped = 0u32;
        let mut since_record = std::time::Duration::ZERO;
        let mut backoff = EMBED_WAIT_INITIAL_BACKOFF;
        loop {
            since_record += backoff;
            if since_record >= inkentry_core::reachability::MEMO_TTL {
                break;
            }
            skipped += 1;
            backoff = (backoff * 2).min(EMBED_WAIT_MAX_BACKOFF);
        }

        assert!(
            skipped <= 2,
            "a recorded miss must cost this poller at most two consecutive skipped polls, \
             leaving most of its tolerance of {EMBED_WAIT_MAX_OFFLINE_PROBES} for real \
             misses; MEMO_TTL of {:?} would skip {skipped}",
            inkentry_core::reachability::MEMO_TTL,
        );
    }

    #[tokio::test]
    #[serial_test::serial(reachability_memo)]
    async fn wait_for_embedder_recovers_after_a_real_refusal_was_memoised() {
        // A genuine refusal, not a 500: only connect failures are memoised.
        inkentry_core::reachability::clear_for_test();

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = format!("http://127.0.0.1:{port}");
        let cfg = cfg_for(url.clone());

        let refused = capability::get_inference_tier_fresh(&cfg).await;
        assert!(
            matches!(refused, capability::Tier::Offline(_)),
            "a closed port must probe offline; got {refused:?}"
        );
        assert!(
            inkentry_core::reachability::connect_already_failed(&url),
            "a genuine refusal must be what lands in the memo, or this test proves nothing"
        );

        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let mock = MockServer::builder().listener(listener).start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("ready")))
            .mount(&mock)
            .await;
        // A literal, not derived from MEMO_TTL, which would be expired by
        // construction. Polls land at 0, 1, 3, 7s; a miss from the first must be
        // stale well before the fourth.
        inkentry_core::reachability::record_connect_failure_aged(
            &url,
            std::time::Duration::from_secs(5),
        );

        let tier = wait_for_embedder(&cfg, TEST_BACKOFF, TEST_BACKOFF).await;
        assert!(
            matches!(tier.caps(), Some(c) if c.index_embed),
            "the poller must look again once the recorded miss has expired, rather than \
             abandoning a server that came back; got {tier:?}"
        );
    }

    #[tokio::test]
    async fn wait_for_embedder_gives_up_after_bounded_offline_probes() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let dead_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        drop(listener);

        let started = std::time::Instant::now();
        let tier = wait_for_embedder(&cfg_for(dead_url), TEST_BACKOFF, TEST_BACKOFF).await;
        assert!(matches!(tier, capability::Tier::Offline(_)));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the offline give-up must be bounded"
        );
    }

    #[tokio::test]
    async fn wait_for_embedder_explicit_opt_out_costs_no_backoff() {
        // Real constants: only the production backoff can show no wait was spent.
        // The mock advertises a ready embedder, so probing instead of
        // short-circuiting would also fail on the tier.
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("ready")))
            .mount(&mock)
            .await;
        let cfg = Config {
            mode: Some(crate::config::SyncMode::Offline),
            ..cfg_for(mock.uri())
        };

        let started = std::time::Instant::now();
        let tier =
            wait_for_embedder(&cfg, EMBED_WAIT_INITIAL_BACKOFF, EMBED_WAIT_MAX_BACKOFF).await;
        let elapsed = started.elapsed();

        assert!(
            matches!(&tier, capability::Tier::Offline(r) if r.is_explicit_opt_out()),
            "an offline opt-out must resolve to one of the explicit reasons; got {tier:?}"
        );
        assert!(
            elapsed < EMBED_WAIT_INITIAL_BACKOFF,
            "the opt-out must return before even the first backoff sleep, let alone all \
             {EMBED_WAIT_MAX_OFFLINE_PROBES} probes; took {elapsed:?}"
        );
        assert_eq!(
            mock.received_requests().await.map(|r| r.len()),
            Some(0),
            "an explicit opt-out must not reach the network at all"
        );
    }

    // The tests above substitute TEST_BACKOFF for these; pin the real values so
    // an edit fails loudly instead of silently changing the worker's wait.
    #[test]
    fn embed_wait_initial_backoff_is_1s() {
        assert_eq!(EMBED_WAIT_INITIAL_BACKOFF.as_secs(), 1);
    }

    #[test]
    fn embed_wait_max_backoff_is_30s() {
        assert_eq!(EMBED_WAIT_MAX_BACKOFF.as_secs(), 30);
    }

    #[test]
    fn embed_wait_max_offline_probes_is_10() {
        assert_eq!(EMBED_WAIT_MAX_OFFLINE_PROBES, 10);
    }

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
    async fn wait_for_embedder_local_first_routes_loopback_transition_not_server_url() {
        // server_url is deliberately unroutable, so a fallback to it errors
        // instead of passing silently.
        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };

        let loopback = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("loading")))
            .up_to_n_times(2)
            .mount(&loopback)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("ready")))
            .mount(&loopback)
            .await;

        let loopback_port: u16 = loopback
            .uri()
            .rsplit(':')
            .next()
            .expect("uri has a port")
            .trim_end_matches('/')
            .parse()
            .expect("uri port is numeric");

        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let prev_state_dir = std::env::var_os("INKENTRY_STATE_DIR");
        let prev_discovery_port = std::env::var_os("INKENTRY_TEST_DISCOVERY_PORT");
        // SAFETY: serialised via #[serial(server_state_dir_env)] against
        // every other test touching these vars.
        //
        // The mock is reached through discovery's fixed-port fallback.
        unsafe {
            std::env::set_var("INKENTRY_STATE_DIR", &state_dir);
            std::env::set_var("INKENTRY_TEST_DISCOVERY_PORT", loopback_port.to_string());
        }

        let cfg = Config {
            server_url: Some("https://cloud.invalid.example:1".to_string()),
            project_id: Some("local/test".to_string()),
            mode: None,
            ..Default::default()
        };
        assert_eq!(cfg.resolve_mode(), crate::config::SyncMode::LocalFirst);

        let tier = wait_for_embedder(&cfg, TEST_BACKOFF, TEST_BACKOFF).await;

        unsafe {
            match prev_state_dir {
                Some(v) => std::env::set_var("INKENTRY_STATE_DIR", v),
                None => std::env::remove_var("INKENTRY_STATE_DIR"),
            }
            match prev_discovery_port {
                Some(v) => std::env::set_var("INKENTRY_TEST_DISCOVERY_PORT", v),
                None => std::env::remove_var("INKENTRY_TEST_DISCOVERY_PORT"),
            }
        }

        assert!(
            matches!(tier.caps(), Some(c) if c.index_embed),
            "the wait must observe the loopback's loading -> ready transition; got {tier:?}"
        );
        assert_eq!(
            tier.server_url(),
            Some(format!("http://127.0.0.1:{loopback_port}")).as_deref(),
            "local_first must route the wait loop to the loopback server, not the \
             configured (and unreachable) server_url; got {tier:?}"
        );
    }

    #[test]
    fn embed_skipped_is_never_silent() {
        let mut tiers: Vec<capability::Tier> = Vec::new();
        for state in [
            capability::EmbedderState::Loading,
            capability::EmbedderState::Unavailable,
            capability::EmbedderState::Disabled,
            capability::EmbedderState::Unknown,
        ] {
            for auto_discovered in [true, false] {
                tiers.push(server_tier(
                    state,
                    auto_discovered,
                    "https://team.example:4655",
                ));
            }
        }
        tiers.extend(
            capability::ALL_OFFLINE_REASONS
                .into_iter()
                .map(capability::Tier::Offline),
        );

        for tier in &tiers {
            for url in [Some("http://x:1"), None] {
                for is_windows in [false, true] {
                    assert!(
                        !embed_skipped_lines(tier, url, is_windows).is_empty(),
                        "tier {tier:?} url {url:?} is_windows {is_windows} produced no notice"
                    );
                }
            }
        }
    }
}
