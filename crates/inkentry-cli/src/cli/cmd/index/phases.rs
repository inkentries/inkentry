//! Phase-runner entry points for `inkentry index`, plus the embedder-readiness
//! wait and the notices printed when embedding is skipped.
//!
//! `run_pre_embed_phases` (PageRank, structural summaries) runs after parse and
//! before the first embed, so the embed queue is PageRank-ordered on a cold
//! index and the first vector already carries its structural summary — both are
//! offline. `run_post_embed_phases` (tier-3 MMR refinement, convention
//! extraction) runs after the primary embed; it is shared between the inline
//! foreground path and `run_background_phases` (the `--_background-phases`
//! child). `run_embed_phases` is the entry point for the `--_embed-phases`
//! child: it rebuilds the embed queue from the DB, waits for the embedder via
//! `wait_for_embedder`, then runs the post-embed phases too.

use anyhow::Result;
use indicatif::MultiProgress;

use super::IndexArgs;
use crate::cli::cmd::embed_worker::EmbedWorkerGuard;
use crate::{capability, config::Config, registry::Registry, storage::Database};

use super::{embed_phase, parse_phase, summaries, tier3};

/// First delay of the embed worker's readiness-wait backoff.
const EMBED_WAIT_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);
/// Backoff growth is bounded at this interval; the wait itself is not
/// time-bounded while the embedder reports `loading` (a model download can
/// legitimately take many minutes, and the queue is durable).
const EMBED_WAIT_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);
/// Consecutive offline probes tolerated before the worker concludes the server
/// is gone (crashed after spawning us) rather than momentarily unreachable.
/// Counted only for offline reasons a retry could actually change; an explicit
/// opt-out returns on the first probe (see [`wait_for_embedder`]).
const EMBED_WAIT_MAX_OFFLINE_PROBES: u32 = 10;

/// Wait until the server's embedder can serve, polling `/v1/health` with a
/// bounded backoff. Returns the final observed tier; the caller re-derives
/// `index_embed` from it.
///
/// A not-ready embedder is a transient condition to wait on, not a terminal
/// condition to skip: `ensure_server_running` waits for liveness only (health
/// goes live at socket bind, before the model loads), so a fresh machine
/// reaches the worker with the embedder still `loading`. Only `unavailable`
/// and `disabled` (or a server with no embedder at all) are terminal; each
/// keeps its distinct notice via `eprint_embed_skipped_notice`. `loading` is
/// never a reason to abandon durable queued work. An explicit offline opt-out
/// is terminal on the first probe: it is settled before any socket is opened,
/// so the offline-probe tolerance below cannot apply to it.
///
/// `get_inference_tier_fresh` (not `probe_tier_fresh`): local_first always
/// prefers the local loopback embedder, even with an explicit server_url set
/// (2026-07-23 founder decision), and this poller must keep re-observing
/// that same local-vs-remote routing decision on every iteration rather than
/// freezing on `get_tier`'s cached first probe of an unrelated server_url.
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
                    // unavailable / disabled / no embedder: terminal here.
                    return tier;
                }
                offline_probes = 0;
                if !announced {
                    eprintln!("Waiting for the embedder to finish loading\u{2026}");
                    announced = true;
                }
            }
            capability::Tier::Offline(reason) => {
                // An explicit opt-out is decided before any socket is opened
                // and holds for the life of the process, so every remaining
                // probe would return this same tier. Sleeping the backoff out
                // buys nothing and costs the user two and a half minutes of a
                // run that has already been told not to reach a server.
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

/// Embed-only entry point for the detached `--_embed-phases` subprocess: rebuild
/// the embed queue from the chunks already in the DB (no re-parse), wait for
/// the embedder to become ready, run the embed phase, then phases 3–5.
pub(super) async fn run_embed_phases(
    args: &IndexArgs,
    cfg: &Config,
    db: &Database,
    project_root: &std::path::Path,
    root_canonical: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<()> {
    // Liveness marker for `inkentry status` (dropped on exit; a killed worker
    // leaves it behind for status to classify as a dead pid). Held through the
    // readiness wait too: a worker waiting on a loading embedder is running,
    // and status must not advise a resume that would double it up.
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

/// Build the differentiated notice lines shown when the embedding phase is
/// skipped, so an unembedded index is never a silent surprise. Pure so it can
/// be unit-tested; the cases mirror the server's readiness contract.
///
/// The whole tier is the input rather than fields derived from it: an offline
/// tier carries the reason the probe recorded, and only that reason can say
/// which server was contacted and what would change the outcome.
/// `server_url` is `cfg.server_url`, read only where the notice names it.
///
/// `is_windows` gates the Windows Defender Firewall hint in the offline
/// case: that hint is a real cause only on Windows, and printing it on every
/// platform actively misdirects a macOS/Linux user away from the real
/// problem (an unreachable configured `server_url`). Callers pass
/// `cfg!(windows)`; injected here so the platform-specific behaviour is
/// testable without `#[cfg(windows)]` test gating.
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
        // Reachable server without a ready embedder for any other reason
        // (`disabled`, or an older server that never advertised `index.embed`).
        Tier::Server { .. } => vec![
            "Note: this server has no embedder — chunks indexed for full-text search only."
                .to_string(),
        ],
        Tier::Offline(reason) => embed_skipped_offline_lines(*reason, server_url, is_windows),
    }
}

/// The offline half of [`embed_skipped_lines`], keyed to the reason the probe
/// recorded rather than to whether a `server_url` happens to be set.
///
/// The embed phase waits on the inference tier, which under `local_first` is a
/// loopback probe even when `server_url` points elsewhere. Derived from the
/// config, this notice named a server the run never contacted, and asked a user
/// whose daemon discovery had just refused out loud to start the daemon they had
/// already started.
fn embed_skipped_offline_lines(
    reason: capability::OfflineReason,
    server_url: Option<&str>,
    is_windows: bool,
) -> Vec<String> {
    use capability::OfflineReason;
    if let Some(advice) = capability::shared_offline_advice(reason) {
        // An explicit opt-out is what the user asked for; nothing is wrong with
        // it, so it does not get a warning's prefix.
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

/// Print the embed-skipped notice to stderr.
pub(super) fn eprint_embed_skipped_notice(tier: &capability::Tier, cfg: &Config) {
    for line in embed_skipped_lines(tier, cfg.server_url.as_deref(), cfg!(windows)) {
        eprintln!("{line}");
    }
}

// ── Pre-embed phases (PageRank + structural summaries) ───────────────────────

/// Offline work that must precede the first embed: PageRank (so the embed queue
/// is central-first on a cold index) and structural summaries (so the first
/// vector carries its summary). Both need only stored chunks and graph edges,
/// present once parse completes; neither touches the embedder or the network.
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

// ── Post-embed phases (tier-3 MMR + conventions) ─────────────────────────────

/// Runs after the primary embed. Refines title-less chunks with tier-3 MMR
/// selection (when the embedder is available), drains the re-embeds that
/// produces, then extracts conventions. Shared between the inline foreground
/// path and the `--_background-phases` / `--_embed-phases` children.
pub(super) async fn run_post_embed_phases(
    args: &IndexArgs,
    cfg: &Config,
    db: &Database,
    project_root: &std::path::Path,
    root_canonical: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<()> {
    // Tier 3: MMR selection + in-place re-embed for title-less chunks. Needs a
    // ready embedder (it embeds short units and reuses the chunk's stored
    // primary vector as the centroid). When unavailable it is skipped and the
    // candidates — still `summary IS NULL` — are retried on the next index.
    if !args.no_summaries {
        let tier = capability::get_inference_tier(cfg).await;
        if tier.is_server() && matches!(tier.caps(), Some(c) if c.index_embed) {
            match tier3::run_tier3_selection(cfg, db).await {
                Ok(refined) if refined > 0 => {
                    // Drain the re-embeds tier-3 just queued (only the refined
                    // title-less subset is pending now).
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

    // Convention extraction (heuristic, no LLM).
    eprintln!("Extracting conventions\u{2026}");
    match crate::conventions::run_extraction(db) {
        Ok(records) => {
            if !records.is_empty() {
                eprintln!("Conventions: {} record(s) detected.", records.len());
            }
        }
        Err(e) => tracing::warn!("convention extraction failed (non-fatal): {e}"),
    }

    // Register / update this project in the global registry.
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

    // ── embed_skipped_lines: 0-chunks / offline notice (#5) ─────────────────────

    // A reachable server whose embedder is in `state`. `auto_discovered`
    // decides whether the notice may point at `inkentry server logs`, which
    // only ever reads the local daemon's log.
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
        // Loopback auto-discovery: the failing embedder IS the local daemon,
        // so `inkentry server logs` is the right place to look.
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
        // Explicit server_url: `inkentry server logs` reads the LOCAL daemon's
        // log, which is clean when the failure lives on the team server. The
        // notice must name the probed server instead.
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
        // Offline (no reachable server) with a configured server_url: the notice
        // must name the actual URL attempted AND say explicitly that it came
        // from a configured `server_url` (not the auto-discovered loopback
        // daemon). Without this, a user with a healthy loopback daemon running
        // has no path from the message to the real cause: the daemon was
        // never being used because server_url overrides it.
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
        // The Windows Defender Firewall hint is a real cause ONLY on Windows;
        // printing it unconditionally (the field bug, hit on macOS) actively
        // misdirects a user on any other platform.
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

    // `index` carries the same blind notice `search` did: a daemon refused by
    // discovery draws a warning naming the cause and the stop/start remedy, and
    // then this told the reader to start the server they had already started.
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

    // Half the reported defect was `status` and the command notices disagreeing
    // about one condition. The five config-independent reasons are rendered from
    // `capability::shared_offline_advice`, so agreement is by construction; this
    // pins that, and pins that no surface invents a remedy the others withhold.
    //
    // The search-side counterpart of this lives in `search.rs`. It is repeated
    // here rather than left to the agreement test because that test compares
    // advice and remedies, and an appended URL changes neither: index could name
    // a server the run never contacted and still agree with the other two.
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

    // `ExplicitServerUnavailable` is excluded from the remedy comparison: its
    // `status` rendering is a transport annotation (`[unreachable]` /
    // `[tls: ...]`), not advice, so there is nothing there to agree with.
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

    // ── wait_for_embedder: the worker owns the readiness wait (ADR-070 D2) ────

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// `/v1/health` body for an embedder in `state`. `index.embed` is
    /// advertised only when ready, mirroring the real server's contract.
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

    // `mode = "cloud_first"`: every test below drives the wait loop's polling
    // logic (loading/ready/unavailable/disabled transitions, the offline
    // give-up bound) by mocking `/v1/health` directly at `url` and expecting
    // `wait_for_embedder` to probe exactly that URL. Under the default
    // `local_first` mode, `get_inference_tier_fresh` routes inference to the
    // local loopback embedder instead and never touches `server_url` at all
    // (see `wait_for_embedder_local_first_routes_loopback_transition_not_server_url`
    // below for that path); `cloud_first` is the mode where an explicit
    // `server_url` legitimately serves inference, which is what every test
    // here needs to still be exercising the polling logic against `url`.
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
        // The readiness gate the cold-start bug lives behind: health reports
        // `loading` (twice here) before flipping to `ready`. The wait must
        // keep polling through `loading` and come back with `index.embed`.
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
        // A failed model load is terminal for this server process: return at
        // the first probe (no retries burned) and preserve the state so the
        // caller prints the distinct `unavailable` notice.
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
        // The embedder can flip loading -> unavailable mid-wait (model load
        // fails after the worker started polling). The wait must exit at the
        // transition with the terminal state preserved, so the caller prints
        // the distinct `unavailable` notice; it must not keep polling.
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
        // The give-up counter is CONSECUTIVE offline probes, not cumulative: a
        // server that flaps (down, briefly back while loading, down again)
        // must not have its earlier misses counted against the later ones.
        // 7 offline + 1 loading + 7 offline = 14 cumulative misses, but never
        // 10 in a row, so the wait must survive to the final `ready`.
        // (A non-2xx health response probes as Tier::Offline.)
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

    #[tokio::test]
    #[serial_test::serial(reachability_memo)]
    async fn wait_for_embedder_recovers_after_a_real_refusal_was_memoised() {
        // The readiness poll exists to watch a server that is not up yet come
        // up, and it can run for as long as a model download takes. Skipping a
        // redundant connect attempt must never turn into refusing to look
        // again: one refused poll while the server was still binding its port
        // would otherwise stand in for every later one, the consecutive-offline
        // counter would run out, and durable queued embed work would be
        // abandoned for a server that came back seconds later.
        //
        // Driven with a genuine connection refusal rather than a non-2xx
        // response, because only a refusal is a connect failure and only a
        // connect failure is memoised. A test that flapped with a 500 would
        // pass without ever touching the memo.
        inkentry_core::reachability::clear_for_test();

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = format!("http://127.0.0.1:{port}");
        let cfg = cfg_for(url.clone());

        // A real probe against the closed port: refused, and recorded.
        let refused = capability::get_inference_tier_fresh(&cfg).await;
        assert!(
            matches!(refused, capability::Tier::Offline(_)),
            "a closed port must probe offline; got {refused:?}"
        );
        assert!(
            inkentry_core::reachability::connect_already_failed(&url),
            "a genuine refusal must be what lands in the memo, or this test proves nothing"
        );

        // The server comes up on the same port, and enough time passes for the
        // recorded miss to stop being worth acting on. Ageing the entry stands
        // in for that wait so the test does not spend it.
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let mock = MockServer::builder().listener(listener).start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("ready")))
            .mount(&mock)
            .await;
        inkentry_core::reachability::record_connect_failure_aged(
            &url,
            inkentry_core::reachability::MEMO_TTL + std::time::Duration::from_secs(1),
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
        // A vanished server (crashed after spawning the worker) must not hang
        // the worker forever: bounded consecutive offline probes, then return
        // Offline so the skip notice prints and the durable queue stays put.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let dead_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        drop(listener); // port is real but nothing serves it

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
        // Driven with the REAL constants, not TEST_BACKOFF: the claim is that
        // an explicit opt-out costs no wait at all, and only the production
        // backoff can demonstrate it. One consumed sleep is already 1s and the
        // full ten-probe give-up is 151s, so a regression here fails on the
        // clock rather than merely being slow in the field.
        //
        // `mode = offline` decides the tier before any socket is opened, so
        // the reason is an explicit opt-out whatever the ambient environment
        // is (`INKENTRY_NO_SERVER` would only swap which of the three fires).
        // `server_url` points at a mock advertising a READY embedder, so a
        // loop that probed instead of short-circuiting would come back
        // Server/ready and fail on the tier too, not just on elapsed time.
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

    // ── wait_for_embedder backoff/give-up constants ──────────────────────────
    //
    // Every wait_for_embedder test above drives the function with
    // `TEST_BACKOFF` (1ms) so the suite doesn't take the ~150s the real
    // constants would need to reach the give-up bound. That substitution is
    // only faithful to production if the constants it stands in for keep
    // their documented values; pin them here so a silent edit (e.g. raising
    // `EMBED_WAIT_MAX_OFFLINE_PROBES` past what the give-up test's runtime
    // budget assumes) fails loudly instead of just changing real-world
    // worker wait time unnoticed. Mirrors the `loopback_probe_timeout_is_250ms`
    // -style constant pins in `capability/probe.rs`.
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

    // ── wait_for_embedder: local_first routes to loopback, not server_url ────
    //
    // The routing-bug regression this story fixes: before, the wait loop
    // probed `cfg.server_url` directly (`probe_tier_fresh`) regardless of
    // mode, so a `local_first` project with an explicit `server_url` never
    // reached its local embedder from the detached worker either.

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
    async fn wait_for_embedder_local_first_routes_loopback_transition_not_server_url() {
        // Under `local_first` (the default once `server_url` is set, with no
        // explicit `mode`), the wait loop must poll the LOCAL loopback
        // embedder, never the configured `server_url` — even while observing
        // a loading -> ready transition across several polls. `server_url` is
        // deliberately unroutable, so an accidental fallback to it surfaces
        // as a connection/DNS error, not a silent wrong-but-passing result.
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
        // The mock is reached through discovery's fixed-port fallback: its
        // `server.port` step now uses a responder only when the pid recorded
        // beside the port is a live `inkentry-server` process reporting the
        // recorded instance id, and a wiremock stand-in is neither.
        unsafe {
            std::env::set_var("INKENTRY_STATE_DIR", &state_dir);
            std::env::set_var("INKENTRY_TEST_DISCOVERY_PORT", loopback_port.to_string());
        }

        let cfg = Config {
            server_url: Some("https://cloud.invalid.example:1".to_string()),
            project_id: Some("local/test".to_string()),
            mode: None, // defaults to local_first because server_url is set
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
