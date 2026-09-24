use crate::config::{Config, SyncMode};

use super::diagnostics::OfflineReason;
use super::state::{Capabilities, EmbedderState, ServerLimits};

#[derive(Debug, Clone)]
pub enum Tier {
    Offline(OfflineReason),
    Server {
        url: String,
        caps: Capabilities,
        auto_discovered: bool,
        embedder_state: EmbedderState,
        server_limits: Option<ServerLimits>,
    },
}

impl Tier {
    pub fn is_server(&self) -> bool {
        matches!(self, Tier::Server { .. })
    }

    pub fn server_url(&self) -> Option<&str> {
        match self {
            Tier::Server { url, .. } => Some(url),
            Tier::Offline(_) => None,
        }
    }

    pub fn caps(&self) -> Option<&Capabilities> {
        match self {
            Tier::Server { caps, .. } => Some(caps),
            Tier::Offline(_) => None,
        }
    }

    pub fn embedder_state(&self) -> Option<EmbedderState> {
        match self {
            Tier::Server { embedder_state, .. } => Some(*embedder_state),
            Tier::Offline(_) => None,
        }
    }

    pub fn server_limits(&self) -> Option<ServerLimits> {
        match self {
            Tier::Server { server_limits, .. } => *server_limits,
            Tier::Offline(_) => None,
        }
    }

    #[cfg(test)]
    pub fn is_auto_discovered(&self) -> bool {
        matches!(
            self,
            Tier::Server {
                auto_discovered: true,
                ..
            }
        )
    }

    // `inkentry server logs` only reads the local daemon's log, so a hint about
    // a failing server must name an explicit URL rather than point at that command.
    pub fn explicit_remote_url(&self) -> Option<&str> {
        match self {
            Tier::Server {
                url,
                auto_discovered: false,
                ..
            } => Some(url),
            _ => None,
        }
    }

    // `self` is the inference tier, not necessarily the tier probed from
    // `cfg.server_url`. Only cloud_first lets an explicit `server_url` own
    // inference; otherwise the tier's URL goes in `inference_url` so memory,
    // which reads only `server_url`, stays on the local store.
    pub fn effective_config(&self, cfg: &Config, project_root: &std::path::Path) -> Config {
        let mut out = cfg.clone();
        if let Tier::Server { url, .. } = self {
            let server_url_owns_inference =
                out.server_url.is_some() && cfg.resolve_mode() == SyncMode::CloudFirst;
            if !server_url_owns_inference {
                out.inference_url = Some(url.clone());
                if out.project_id.is_none() {
                    out.project_id = Some(cfg.resolve_project_id(project_root));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_server_is_server_true() {
        let tier = Tier::Server {
            url: "http://example.com".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert!(tier.is_server());
    }

    #[test]
    fn tier_offline_is_server_false() {
        let tier = Tier::Offline(OfflineReason::NoLocalServer);
        assert!(!tier.is_server());
    }

    #[test]
    fn tier_server_returns_url() {
        let tier = Tier::Server {
            url: "http://inkentry.internal:4655".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert_eq!(tier.server_url(), Some("http://inkentry.internal:4655"));
    }

    #[test]
    fn tier_offline_returns_none_url() {
        let tier = Tier::Offline(OfflineReason::NoLocalServer);
        assert_eq!(tier.server_url(), None);
    }

    #[test]
    fn tier_server_returns_caps() {
        let caps = Capabilities::all();
        let tier = Tier::Server {
            url: "http://example.com".to_string(),
            caps: caps.clone(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert!(tier.caps().is_some());
    }

    #[test]
    fn tier_offline_returns_none_caps() {
        let tier = Tier::Offline(OfflineReason::NoLocalServer);
        assert!(tier.caps().is_none());
    }

    #[test]
    fn tier_auto_discovered_flag() {
        let auto = Tier::Server {
            url: "http://127.0.0.1:4655".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        let explicit = Tier::Server {
            url: "http://server.example.com:4655".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert!(auto.is_auto_discovered());
        assert!(!explicit.is_auto_discovered());
        assert!(!Tier::Offline(OfflineReason::NoLocalServer).is_auto_discovered());
    }

    #[test]
    fn tier_explicit_remote_url_only_for_explicit_server() {
        let auto = Tier::Server {
            url: "http://127.0.0.1:4655".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        let explicit = Tier::Server {
            url: "http://server.example.com:4655".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert_eq!(auto.explicit_remote_url(), None);
        assert_eq!(
            explicit.explicit_remote_url(),
            Some("http://server.example.com:4655")
        );
        assert_eq!(
            Tier::Offline(OfflineReason::NoLocalServer).explicit_remote_url(),
            None
        );
    }

    #[test]
    fn tier_explicit_remote_url_is_explicit_even_when_host_is_loopback() {
        let explicit_loopback = Tier::Server {
            url: "http://127.0.0.1:9797".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert_eq!(
            explicit_loopback.explicit_remote_url(),
            Some("http://127.0.0.1:9797"),
            "an explicitly configured server_url must count as explicit even when its host is loopback"
        );
    }

    #[test]
    fn effective_config_auto_discovered_sets_inference_url_not_server_url() {
        let tier = Tier::Server {
            url: "http://127.0.0.1:4655".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        let cfg = Config::default();
        let eff = tier.effective_config(&cfg, std::path::Path::new("/tmp/proj"));

        assert_eq!(
            eff.server_url, None,
            "auto-discovered server must NOT populate server_url (memory stays local)"
        );
        assert_eq!(
            eff.inference_url.as_deref(),
            Some("http://127.0.0.1:4655"),
            "auto-discovered server URL must route inference via inference_url"
        );
        assert!(
            eff.project_id.is_some(),
            "project_id should be derived so the inference client can address the project"
        );
        assert_eq!(eff.resolve_inference_url(), Some("http://127.0.0.1:4655"));
    }

    #[test]
    fn effective_config_explicit_server_url_cloud_first_left_unchanged() {
        let tier = Tier::Server {
            url: "http://team.example.com:4655".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        let cfg = Config {
            server_url: Some("http://team.example.com:4655".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };
        let eff = tier.effective_config(&cfg, std::path::Path::new("/tmp/proj"));

        assert_eq!(
            eff.server_url.as_deref(),
            Some("http://team.example.com:4655"),
            "explicit team server_url must be preserved (memory stays remote)"
        );
        assert_eq!(
            eff.inference_url, None,
            "cloud_first should not synthesise a separate inference_url: \
             resolve_inference_url falls back to server_url for this mode"
        );
        assert_eq!(
            eff.resolve_inference_url(),
            Some("http://team.example.com:4655")
        );
    }

    #[test]
    fn effective_config_explicit_server_url_local_first_prefers_tier_url_for_inference() {
        let tier = Tier::Server {
            url: "http://127.0.0.1:4655".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        let cfg = Config {
            server_url: Some("https://api.inkentry.com".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: None,
            ..Default::default()
        };
        assert_eq!(cfg.resolve_mode(), SyncMode::LocalFirst);
        let eff = tier.effective_config(&cfg, std::path::Path::new("/tmp/proj"));

        assert_eq!(
            eff.server_url.as_deref(),
            Some("https://api.inkentry.com"),
            "server_url must be preserved unchanged (memory selection still reads it)"
        );
        assert_eq!(
            eff.inference_url.as_deref(),
            Some("http://127.0.0.1:4655"),
            "local_first must route inference to the tier's (loopback) URL, \
             even though an explicit server_url is also set"
        );
        assert_eq!(
            eff.resolve_inference_url(),
            Some("http://127.0.0.1:4655"),
            "resolve_inference_url must return the local loopback URL, never \
             the cloud server_url, in local_first"
        );
    }

    #[test]
    fn effective_config_offline_tier_is_noop() {
        let cfg = Config::default();
        let eff = Tier::Offline(OfflineReason::NoLocalServer)
            .effective_config(&cfg, std::path::Path::new("/tmp/proj"));
        assert_eq!(eff.server_url, None);
        assert_eq!(eff.inference_url, None);
    }

    #[test]
    fn tier_embedder_state_accessor() {
        let tier = Tier::Server {
            url: "http://127.0.0.1:4655".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
            embedder_state: EmbedderState::Loading,
            server_limits: None,
        };
        assert_eq!(tier.embedder_state(), Some(EmbedderState::Loading));
        assert_eq!(
            Tier::Offline(OfflineReason::NoLocalServer).embedder_state(),
            None
        );
    }
}
