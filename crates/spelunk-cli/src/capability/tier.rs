//! Capability tier: the resolved server state for this process.

use crate::config::Config;

use super::state::{Capabilities, EmbedderState, ServerLimits};

/// CLI capability tier for this process.
#[derive(Debug, Clone)]
pub enum Tier {
    /// No server configured, or server unreachable. Offline features only.
    Offline,
    /// Server reachable. All `caps`-listed features are available.
    Server {
        url: String,
        caps: Capabilities,
        /// `true` when the URL was discovered automatically (loopback probe),
        /// `false` when it was set explicitly via config / env var.
        /// Used to annotate UX output (e.g. `(local, auto)` in `spelunk status`).
        /// Consumed by `is_auto_discovered()` and sub-issue #324 UX wiring.
        auto_discovered: bool,
        /// Server-side embedder readiness, mirrored from the `/v1/health`
        /// `embedder.state` field. `Unknown` when the field is absent (server
        /// pre-dates it). Lets the CLI distinguish "server up but model still
        /// warming up / failed to load" from a ready server when semantic
        /// search is unavailable (rendered by `status`).
        embedder_state: EmbedderState,
        /// Server-enforced `/index/embed` limits, mirrored from `/v1/health`'s
        /// `limits` object. `None` when the field is absent: a server that
        /// pre-dates this fix and still enforces the old blanket 30s budget
        /// with no `/index/embed` exemption. The embed phase
        /// (`embed_phase.rs`) reads this to clamp its own calibration to what
        /// this particular server actually supports instead of assuming.
        server_limits: Option<ServerLimits>,
    },
}

impl Tier {
    pub fn is_server(&self) -> bool {
        matches!(self, Tier::Server { .. })
    }

    // Used by check.rs / status.rs via pattern matching on the enum variant;
    // also consumed by sub-issues #323/#324 UX wiring.
    #[cfg(test)]
    pub fn server_url(&self) -> Option<&str> {
        match self {
            Tier::Server { url, .. } => Some(url),
            Tier::Offline => None,
        }
    }

    pub fn caps(&self) -> Option<&Capabilities> {
        match self {
            Tier::Server { caps, .. } => Some(caps),
            Tier::Offline => None,
        }
    }

    /// Server-side embedder readiness for a `Server` tier, or `None` when
    /// offline. `EmbedderState::Unknown` is returned for a reachable server that
    /// pre-dates the `embedder.state` health field. Used by the offline notice
    /// (`search`/`index`) and by `spelunk status` to explain why semantic search
    /// is unavailable.
    pub fn embedder_state(&self) -> Option<EmbedderState> {
        match self {
            Tier::Server { embedder_state, .. } => Some(*embedder_state),
            Tier::Offline => None,
        }
    }

    /// Server-enforced `/index/embed` limits for a `Server` tier, or `None`
    /// when offline *or* when the server pre-dates the `/v1/health` `limits`
    /// field. Used by the embed phase (`embed_phase.rs`) to clamp its own
    /// calibration to what this particular server actually supports.
    pub fn server_limits(&self) -> Option<ServerLimits> {
        match self {
            Tier::Server { server_limits, .. } => *server_limits,
            Tier::Offline => None,
        }
    }

    /// Returns `true` when the server URL was discovered automatically via
    /// the loopback probe rather than set explicitly in config or environment.
    /// Used by `spelunk status` (sub-issue #324) to annotate the URL with `(local, auto)`.
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

    /// `Some(url)` when this tier reached `Server` via an **explicit**
    /// `server_url` (not loopback auto-discovery); `None` for `Offline` and
    /// for the auto-discovered loopback case.
    ///
    /// `spelunk server logs` only ever reads the local auto-daemon's log
    /// file. A command-output hint that names a server to check must use
    /// this instead of unconditionally pointing at that command: with an
    /// explicit remote `server_url`, `spelunk server logs` reads a healthy
    /// local daemon's log while the real failure lives on the named server
    /// (the pattern `embedder_status_line` in `status.rs` established).
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

    /// Return a `Config` whose server fields reflect this tier, so that
    /// server-backed helpers (`ServerInferenceClient::from_config`,
    /// `open_memory_backend`) work the same whether the server was configured
    /// explicitly or discovered via the loopback probe.
    ///
    /// Loopback auto-discovery sets the capability `Tier` WITHOUT populating
    /// `cfg.server_url`. Commands that route inference through `from_config`
    /// gate on a server URL, so without this bridge they wrongly report
    /// "requires spelunk-server" even though `spelunk status` shows `Server`.
    ///
    /// ## ADR-004: inference vs memory storage are routed separately
    ///
    /// An auto-discovered loopback server is an **inference** backend only; it
    /// is never a memory store. So when the tier is `Server` and `server_url`
    /// is unset (the auto-discovered case), the discovered URL is written to
    /// `inference_url`: NOT `server_url`. `ServerInferenceClient::from_config`
    /// reads `inference_url` (falling back to `server_url`), so inference still
    /// reaches the loopback server; `open_memory_backend` reads only
    /// `server_url`, so memory stays on the project's local `memory.db`. This
    /// is what removes the split-brain where `memory add` wrote `memory.db`
    /// while `memory search` read the server's `server.db`.
    ///
    /// `project_id` is derived (mirroring `embed_phase`, see spelunk#307) so the
    /// inference client can address the project on the server. When an explicit
    /// `cfg.server_url` is already set (a team/remote server), it owns both
    /// inference and memory and the config is returned unchanged.
    pub fn effective_config(&self, cfg: &Config, project_root: &std::path::Path) -> Config {
        let mut out = cfg.clone();
        if let Tier::Server { url, .. } = self
            && out.server_url.is_none()
        {
            // Auto-discovered loopback server: route inference here, but leave
            // `server_url` unset so memory selection stays local (ADR-004).
            out.inference_url = Some(url.clone());
            if out.project_id.is_none() {
                out.project_id = Some(cfg.resolve_project_id(project_root));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Tier ─────────────────────────────────────────────────────────────────

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
        let tier = Tier::Offline;
        assert!(!tier.is_server());
    }

    #[test]
    fn tier_server_returns_url() {
        let tier = Tier::Server {
            url: "http://spelunk.internal:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert_eq!(tier.server_url(), Some("http://spelunk.internal:7777"));
    }

    #[test]
    fn tier_offline_returns_none_url() {
        let tier = Tier::Offline;
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
        let tier = Tier::Offline;
        assert!(tier.caps().is_none());
    }

    #[test]
    fn tier_auto_discovered_flag() {
        let auto = Tier::Server {
            url: "http://127.0.0.1:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        let explicit = Tier::Server {
            url: "http://server.example.com:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert!(auto.is_auto_discovered());
        assert!(!explicit.is_auto_discovered());
        assert!(!Tier::Offline.is_auto_discovered());
    }

    #[test]
    fn tier_explicit_remote_url_only_for_explicit_server() {
        let auto = Tier::Server {
            url: "http://127.0.0.1:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        let explicit = Tier::Server {
            url: "http://server.example.com:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert_eq!(auto.explicit_remote_url(), None);
        assert_eq!(
            explicit.explicit_remote_url(),
            Some("http://server.example.com:7777")
        );
        assert_eq!(Tier::Offline.explicit_remote_url(), None);
    }

    #[test]
    fn tier_explicit_remote_url_is_explicit_even_when_host_is_loopback() {
        // `explicit_remote_url` keys off *how the URL was reached*
        // (`auto_discovered`), never off the host it resolves to. An operator
        // can hand-configure `server_url = http://127.0.0.1:PORT`; it is
        // still `auto_discovered: false` because it went through the
        // `Some(url)` probe branch, not loopback auto-discovery (see
        // `probe()`). `spelunk server logs` only ever reads the fixed
        // auto-daemon log path and has no idea this loopback address was
        // hand-configured, so the hint must still name it rather than assume
        // "loopback implies safe to point at the local log".
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

    // ── effective_config (ADR-004 inference-vs-memory routing) ───────────────

    #[test]
    fn effective_config_auto_discovered_sets_inference_url_not_server_url() {
        // An auto-discovered loopback server is inference-only: its URL must
        // land in `inference_url` so memory selection (`open_memory_backend`,
        // which reads only `server_url`) stays local. This is the core of the
        // ADR-004 split-brain fix.
        let tier = Tier::Server {
            url: "http://127.0.0.1:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        let cfg = Config::default(); // server_url = None
        let eff = tier.effective_config(&cfg, std::path::Path::new("/tmp/proj"));

        assert_eq!(
            eff.server_url, None,
            "auto-discovered server must NOT populate server_url (memory stays local)"
        );
        assert_eq!(
            eff.inference_url.as_deref(),
            Some("http://127.0.0.1:7777"),
            "auto-discovered server URL must route inference via inference_url"
        );
        assert!(
            eff.project_id.is_some(),
            "project_id should be derived so the inference client can address the project"
        );
        // Inference resolves to the loopback server; memory selection does not.
        assert_eq!(eff.resolve_inference_url(), Some("http://127.0.0.1:7777"));
    }

    #[test]
    fn effective_config_explicit_server_url_left_unchanged() {
        // An explicitly-configured team server owns BOTH inference and memory
        // (team-memory tier). `effective_config` must not touch it.
        let tier = Tier::Server {
            url: "http://team.example.com:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        let cfg = Config {
            server_url: Some("http://team.example.com:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        let eff = tier.effective_config(&cfg, std::path::Path::new("/tmp/proj"));

        assert_eq!(
            eff.server_url.as_deref(),
            Some("http://team.example.com:7777"),
            "explicit team server_url must be preserved (memory stays remote)"
        );
        assert_eq!(
            eff.inference_url, None,
            "explicit server_url path should not synthesise a separate inference_url"
        );
    }

    #[test]
    fn effective_config_offline_tier_is_noop() {
        let cfg = Config::default();
        let eff = Tier::Offline.effective_config(&cfg, std::path::Path::new("/tmp/proj"));
        assert_eq!(eff.server_url, None);
        assert_eq!(eff.inference_url, None);
    }

    #[test]
    fn tier_embedder_state_accessor() {
        let tier = Tier::Server {
            url: "http://127.0.0.1:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
            embedder_state: EmbedderState::Loading,
            server_limits: None,
        };
        assert_eq!(tier.embedder_state(), Some(EmbedderState::Loading));
        assert_eq!(Tier::Offline.embedder_state(), None);
    }
}
