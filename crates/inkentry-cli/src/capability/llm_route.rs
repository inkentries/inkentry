use std::path::Path;

use crate::config::Config;
use crate::server_client::ServerInferenceClient;

use super::llm_message::NoLlmReason;
use super::probe::{get_inference_tier, get_tier};
use super::tier::Tier;

#[derive(Debug, Clone)]
pub enum LlmRoute {
    Local(Config),
    Remote(Config),
    Unavailable(NoLlmReason),
}

impl LlmRoute {
    pub fn reason(&self) -> Option<NoLlmReason> {
        match self {
            LlmRoute::Unavailable(reason) => Some(*reason),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn target_url(&self) -> Option<&str> {
        match self {
            LlmRoute::Local(cfg) | LlmRoute::Remote(cfg) => cfg.resolve_inference_url(),
            LlmRoute::Unavailable(_) => None,
        }
    }

    // from_config infers explicit-remote from inference_url being unset, which the Remote route
    // sets; using it would point remote failures at the local daemon's log.
    pub fn client(&self) -> Option<ServerInferenceClient> {
        match self {
            LlmRoute::Local(cfg) => ServerInferenceClient::from_config(cfg),
            LlmRoute::Remote(cfg) => ServerInferenceClient::from_config_explicit_remote(cfg),
            LlmRoute::Unavailable(_) => None,
        }
    }
}

pub async fn resolve_llm_route(cfg: &Config, project_root: &Path) -> LlmRoute {
    let explicit_offline = inkentry_core::config::no_server_env_set()
        || cfg.mode == Some(inkentry_core::config::SyncMode::Offline);
    if explicit_offline {
        return LlmRoute::Unavailable(NoLlmReason::Offline);
    }

    let inference_tier = get_inference_tier(cfg).await;
    if let Some(route) = route_without_probing_the_remote(cfg, project_root, &inference_tier) {
        return route;
    }
    remote_route(cfg, project_root, get_tier(cfg).await)
}

// Split from resolve_llm_route so tests can reach the no-server_url terminal without a loopback probe.
fn route_without_probing_the_remote(
    cfg: &Config,
    project_root: &Path,
    inference_tier: &Tier,
) -> Option<LlmRoute> {
    if let Some(route) = local_route(cfg, project_root, inference_tier) {
        return Some(route);
    }
    cfg.server_url
        .is_none()
        .then_some(LlmRoute::Unavailable(NoLlmReason::NoLlmAnywhere))
}

fn local_route(cfg: &Config, project_root: &Path, inference_tier: &Tier) -> Option<LlmRoute> {
    // Key on the advertised capability: config cannot say whether the running daemon picked it up.
    if inference_tier.caps().is_some_and(|c| c.llm_complete) {
        return Some(LlmRoute::Local(
            inference_tier.effective_config(cfg, project_root),
        ));
    }
    // Privacy guard: with llm_url set a remote is no substitute; the daemon needs a restart.
    if cfg.llm_url.is_some() {
        return Some(LlmRoute::Unavailable(
            NoLlmReason::LocalConfiguredButNotServed,
        ));
    }
    None
}

fn remote_route(cfg: &Config, project_root: &Path, remote_tier: &Tier) -> LlmRoute {
    if remote_tier.caps().is_some_and(|c| c.llm_complete)
        && let Some(url) = remote_tier.server_url()
    {
        let mut out = cfg.clone();
        out.inference_url = Some(url.to_string());
        if out.project_id.is_none() {
            out.project_id = Some(cfg.resolve_project_id(project_root));
        }
        return LlmRoute::Remote(out);
    }
    LlmRoute::Unavailable(NoLlmReason::NoLlmAnywhere)
}

#[cfg(test)]
mod tests {
    use super::super::diagnostics::OfflineReason;
    use super::super::state::{Capabilities, EmbedderState};
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ROOT: &str = "/tmp/inkentry-llm-route-fixture";

    fn root() -> &'static Path {
        Path::new(ROOT)
    }

    fn caps_with_llm(llm_complete: bool) -> Capabilities {
        let mut caps = Capabilities::all();
        caps.llm_complete = llm_complete;
        caps
    }

    fn server_tier(url: &str, llm_complete: bool, auto_discovered: bool) -> Tier {
        Tier::Server {
            url: url.to_string(),
            caps: caps_with_llm(llm_complete),
            auto_discovered,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        }
    }

    fn health_with_llm() -> serde_json::Value {
        serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory", "index.embed", "search.semantic", "llm.complete"],
            "embedding_dim": inkentry_core::embeddings::EMBEDDING_DIM,
        })
    }

    // Lists a legacy capability but not llm.complete: keying on anything else misfires under version skew.
    fn health_without_llm() -> serde_json::Value {
        serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory", "index.embed", "search.semantic", "legacy.feature"],
            "embedding_dim": inkentry_core::embeddings::EMBEDDING_DIM,
        })
    }

    async fn mock_server(body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    fn port_of(uri: &str) -> u16 {
        uri.rsplit(':')
            .next()
            .expect("uri has a port")
            .trim_end_matches('/')
            .parse()
            .expect("uri port is numeric")
    }

    // Uses the fixed-port discovery fallback, not the server.port file: that path trusts only a
    // live inkentry-server pid, which wiremock is not.
    struct StateDirGuard {
        _tmp: tempfile::TempDir,
        previous_state_dir: Option<std::ffi::OsString>,
        previous_discovery_port: Option<std::ffi::OsString>,
    }

    impl StateDirGuard {
        fn pointing_at(uri: &str) -> Self {
            let tmp = tempfile::TempDir::new().expect("temp state dir");
            let state_dir = tmp.path().join("state");
            std::fs::create_dir_all(&state_dir).expect("create state dir");
            let previous_state_dir = std::env::var_os("INKENTRY_STATE_DIR");
            let previous_discovery_port = std::env::var_os("INKENTRY_TEST_DISCOVERY_PORT");
            unsafe {
                std::env::set_var("INKENTRY_STATE_DIR", &state_dir);
                std::env::set_var("INKENTRY_TEST_DISCOVERY_PORT", port_of(uri).to_string());
            }
            Self {
                _tmp: tmp,
                previous_state_dir,
                previous_discovery_port,
            }
        }
    }

    impl Drop for StateDirGuard {
        fn drop(&mut self) {
            unsafe {
                match self.previous_state_dir.take() {
                    Some(v) => std::env::set_var("INKENTRY_STATE_DIR", v),
                    None => std::env::remove_var("INKENTRY_STATE_DIR"),
                }
                match self.previous_discovery_port.take() {
                    Some(v) => std::env::set_var("INKENTRY_TEST_DISCOVERY_PORT", v),
                    None => std::env::remove_var("INKENTRY_TEST_DISCOVERY_PORT"),
                }
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env)]
    async fn offline_mode_routes_nowhere_and_probes_nothing() {
        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };

        let loopback = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_with_llm()))
            .expect(0)
            .mount(&loopback)
            .await;
        let _state = StateDirGuard::pointing_at(&loopback.uri());

        let cfg = Config {
            server_url: Some("https://cloud.invalid.example:1".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(inkentry_core::config::SyncMode::Offline),
            ..Default::default()
        };

        let route = resolve_llm_route(&cfg, root()).await;
        assert_eq!(route.reason(), Some(NoLlmReason::Offline), "got {route:?}");
        assert_eq!(
            loopback.received_requests().await.expect("recorded").len(),
            0,
            "explicit offline must not probe anything"
        );
    }

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env)]
    async fn no_server_env_kill_switch_routes_nowhere_and_probes_nothing() {
        let loopback = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_with_llm()))
            .mount(&loopback)
            .await;
        let _state = StateDirGuard::pointing_at(&loopback.uri());

        unsafe { std::env::set_var("INKENTRY_NO_SERVER", "1") };
        let route = resolve_llm_route(&Config::default(), root()).await;
        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };

        assert_eq!(route.reason(), Some(NoLlmReason::Offline), "got {route:?}");
        assert_eq!(
            loopback.received_requests().await.expect("recorded").len(),
            0,
            "the kill switch must not probe anything"
        );
    }

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env)]
    async fn loopback_with_an_llm_and_no_server_url_routes_local() {
        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };
        let loopback = mock_server(health_with_llm()).await;
        let _state = StateDirGuard::pointing_at(&loopback.uri());

        let route = resolve_llm_route(&Config::default(), root()).await;
        assert!(matches!(route, LlmRoute::Local(_)), "got {route:?}");
        assert_eq!(route.target_url(), Some(loopback.uri().as_str()));
    }

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env)]
    async fn loopback_with_an_llm_wins_over_an_llm_capable_server_url() {
        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };
        let loopback = mock_server(health_with_llm()).await;
        let remote = mock_server(health_with_llm()).await;
        let _state = StateDirGuard::pointing_at(&loopback.uri());

        let cfg = Config {
            server_url: Some(remote.uri()),
            project_id: Some("team/proj".to_string()),
            mode: None,
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_mode(),
            inkentry_core::config::SyncMode::LocalFirst
        );

        let route = resolve_llm_route(&cfg, root()).await;
        assert!(matches!(route, LlmRoute::Local(_)), "got {route:?}");
        assert_eq!(route.target_url(), Some(loopback.uri().as_str()));
        assert_eq!(
            remote.received_requests().await.expect("recorded").len(),
            0,
            "a usable local LLM must not cause the remote to be contacted at all"
        );
    }

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env)]
    async fn configured_local_llm_not_served_stops_and_never_reaches_the_remote() {
        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };
        let loopback = mock_server(health_without_llm()).await;
        let remote = mock_server(health_with_llm()).await;
        let _state = StateDirGuard::pointing_at(&loopback.uri());

        let cfg = Config {
            server_url: Some(remote.uri()),
            project_id: Some("team/proj".to_string()),
            llm_url: Some("http://127.0.0.1:1234".to_string()),
            ..Default::default()
        };

        let route = resolve_llm_route(&cfg, root()).await;
        assert_eq!(
            route.reason(),
            Some(NoLlmReason::LocalConfiguredButNotServed),
            "got {route:?}"
        );
        assert_eq!(
            remote.received_requests().await.expect("recorded").len(),
            0,
            "the remote must not even be probed once a local LLM was configured"
        );
    }

    // get_tier caches in a process-wide cell with no reset hook, so the decision functions are tested directly.
    #[test]
    fn nothing_configured_anywhere_reports_no_llm_not_offline() {
        let route = route_without_probing_the_remote(
            &Config::default(),
            root(),
            &Tier::Offline(OfflineReason::NoLocalServer),
        )
        .expect("with no server_url there is nothing left to probe");
        assert_eq!(
            route.reason(),
            Some(NoLlmReason::NoLlmAnywhere),
            "got {route:?}"
        );
    }

    #[test]
    fn a_configured_server_url_still_reaches_the_remote_step() {
        let cfg = Config {
            server_url: Some("https://team.example:4655".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        assert!(
            route_without_probing_the_remote(
                &cfg,
                root(),
                &Tier::Offline(OfflineReason::NoLocalServer)
            )
            .is_none(),
            "the remote arm must still be given its chance to probe"
        );
    }

    #[test]
    fn local_route_takes_the_local_llm_when_the_tier_advertises_one() {
        let tier = server_tier("http://127.0.0.1:4655", true, true);
        let route = local_route(&Config::default(), root(), &tier).expect("local arm applies");
        assert!(matches!(route, LlmRoute::Local(_)), "got {route:?}");
        assert_eq!(route.target_url(), Some("http://127.0.0.1:4655"));
    }

    #[test]
    fn local_route_declines_a_tier_without_llm_complete() {
        let mut caps = Capabilities::all();
        caps.llm_complete = false;
        let tier = Tier::Server {
            url: "http://127.0.0.1:4655".to_string(),
            caps,
            auto_discovered: true,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert!(
            local_route(&Config::default(), root(), &tier).is_none(),
            "a tier without llm.complete must never yield a local LLM route"
        );
    }

    #[test]
    fn local_route_stops_when_llm_url_is_set_and_the_tier_serves_no_llm() {
        let cfg = Config {
            llm_url: Some("http://127.0.0.1:1234".to_string()),
            server_url: Some("https://team.example:4655".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        let tier = server_tier("http://127.0.0.1:4655", false, true);
        let route = local_route(&cfg, root(), &tier).expect("the guard must apply");
        assert_eq!(
            route.reason(),
            Some(NoLlmReason::LocalConfiguredButNotServed)
        );
    }

    #[test]
    fn local_route_stops_when_llm_url_is_set_and_no_local_server_is_up() {
        let cfg = Config {
            llm_url: Some("http://127.0.0.1:1234".to_string()),
            ..Default::default()
        };
        let route = local_route(&cfg, root(), &Tier::Offline(OfflineReason::NoLocalServer))
            .expect("the guard must apply");
        assert_eq!(
            route.reason(),
            Some(NoLlmReason::LocalConfiguredButNotServed)
        );
    }

    // In cloud_first the inference tier is server_url itself, so the privacy guard never applies;
    // embedding already sends chunk text to that remote.
    #[test]
    fn cloud_first_routes_to_the_remote_even_with_llm_url_set() {
        let cfg = Config {
            server_url: Some("https://team.example:4655".to_string()),
            project_id: Some("team/proj".to_string()),
            llm_url: Some("http://127.0.0.1:1234".to_string()),
            mode: Some(inkentry_core::config::SyncMode::CloudFirst),
            ..Default::default()
        };
        let tier = server_tier("https://team.example:4655", true, false);
        let route = local_route(&cfg, root(), &tier).expect("step 2 matches on the remote");
        assert!(matches!(route, LlmRoute::Local(_)), "got {route:?}");
        assert_eq!(route.target_url(), Some("https://team.example:4655"));
    }

    #[test]
    fn local_first_with_the_same_config_stops_instead_of_using_the_remote() {
        let cfg = Config {
            server_url: Some("https://team.example:4655".to_string()),
            project_id: Some("team/proj".to_string()),
            llm_url: Some("http://127.0.0.1:1234".to_string()),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_mode(),
            inkentry_core::config::SyncMode::LocalFirst
        );
        let tier = server_tier("http://127.0.0.1:4655", false, true);
        let route = local_route(&cfg, root(), &tier).expect("the guard must apply");
        assert_eq!(
            route.reason(),
            Some(NoLlmReason::LocalConfiguredButNotServed)
        );
    }

    #[test]
    fn local_route_defers_to_the_remote_when_no_local_llm_was_asked_for() {
        let cfg = Config {
            server_url: Some("https://team.example:4655".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        assert!(local_route(&cfg, root(), &Tier::Offline(OfflineReason::NoLocalServer)).is_none());
        let no_llm = server_tier("http://127.0.0.1:4655", false, true);
        assert!(local_route(&cfg, root(), &no_llm).is_none());
    }

    #[test]
    fn remote_route_targets_the_server_url_when_it_advertises_an_llm() {
        let cfg = Config {
            server_url: Some("https://team.example:4655".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        let tier = server_tier("https://team.example:4655", true, false);
        let route = remote_route(&cfg, root(), &tier);
        assert!(matches!(route, LlmRoute::Remote(_)), "got {route:?}");
        assert_eq!(route.target_url(), Some("https://team.example:4655"));
    }

    #[test]
    fn remote_route_config_names_the_remote_origin_for_credential_resolution() {
        let cfg = Config {
            server_url: Some("https://team.example:4655".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        let tier = server_tier("https://team.example:4655", true, false);
        match remote_route(&cfg, root(), &tier) {
            LlmRoute::Remote(eff) => assert_eq!(
                eff.resolve_inference_url(),
                Some("https://team.example:4655"),
                "the remote's own origin is what the bearer must be resolved for"
            ),
            other => panic!("expected the remote arm, got {other:?}"),
        }
    }

    #[test]
    fn remote_route_derives_a_project_id_when_the_config_has_none() {
        let cfg = Config {
            server_url: Some("http://127.0.0.1:4655".to_string()),
            ..Default::default()
        };
        let tier = server_tier("http://127.0.0.1:4655", true, false);
        match remote_route(&cfg, root(), &tier) {
            LlmRoute::Remote(eff) => assert!(
                eff.project_id.is_some(),
                "the client cannot address a project without one"
            ),
            other => panic!("expected the remote arm, got {other:?}"),
        }
    }

    #[test]
    fn remote_route_reports_no_llm_when_the_server_url_has_none() {
        let cfg = Config {
            server_url: Some("https://team.example:4655".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        let tier = server_tier("https://team.example:4655", false, false);
        assert_eq!(
            remote_route(&cfg, root(), &tier).reason(),
            Some(NoLlmReason::NoLlmAnywhere)
        );
    }

    #[test]
    fn remote_route_reports_no_llm_when_the_server_url_is_unreachable() {
        let cfg = Config {
            server_url: Some("https://team.example:4655".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        assert_eq!(
            remote_route(&cfg, root(), &Tier::Offline(OfflineReason::NoLocalServer)).reason(),
            Some(NoLlmReason::NoLlmAnywhere)
        );
    }

    #[test]
    fn unavailable_route_builds_no_client() {
        assert!(
            LlmRoute::Unavailable(NoLlmReason::NoLlmAnywhere)
                .client()
                .is_none()
        );
    }
}
