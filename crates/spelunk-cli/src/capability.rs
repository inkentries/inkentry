//! Capability tier detection for the spelunk CLI.
//!
//! Tier 0 (Offline): no server_url configured, or server unreachable.
//! Tier 1 (Server):  server_url set and GET /v1/health succeeds.
//!
//! The probe runs lazily on the first call that needs Tier 1 and its result
//! is cached for the process lifetime.

use serde::Serialize;
use tokio::sync::OnceCell;

use crate::config::Config;

static TIER: OnceCell<Tier> = OnceCell::const_new();

/// Feature availability for a server-connected tier.
#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub search_semantic: bool,
    pub index_embed: bool,
    pub memory_push: bool,
    pub memory_pull: bool,
    pub memory_search: bool,
    pub memory_harvest: bool,
    pub explore: bool,
    pub plan: bool,
}

impl Capabilities {
    fn from_server_caps(caps: &[&str]) -> Self {
        let has = |c: &str| caps.contains(&c);
        let memory = has("memory");
        Self {
            search_semantic: has("search.semantic"),
            index_embed: has("index.embed"),
            memory_push: memory,
            memory_pull: memory,
            memory_search: memory,
            memory_harvest: memory,
            explore: has("explore"),
            plan: has("plan"),
        }
    }

    /// Conservative set assumed when talking to a legacy server that returns
    /// plain-text health ("ok") instead of JSON.
    fn legacy_memory_only() -> Self {
        Self {
            search_semantic: false,
            index_embed: false,
            memory_push: true,
            memory_pull: true,
            memory_search: true,
            memory_harvest: false,
            explore: false,
            plan: false,
        }
    }

    /// Full set for a fully-featured server.
    #[allow(dead_code)]
    pub fn all() -> Self {
        Self {
            search_semantic: true,
            index_embed: true,
            memory_push: true,
            memory_pull: true,
            memory_search: true,
            memory_harvest: true,
            explore: true,
            plan: true,
        }
    }
}

/// CLI capability tier for this process.
#[derive(Debug, Clone)]
pub enum Tier {
    /// No server configured, or server unreachable. Offline features only.
    Offline,
    /// Server reachable. All `caps`-listed features are available.
    Server { url: String, caps: Capabilities },
}

impl Tier {
    pub fn is_server(&self) -> bool {
        matches!(self, Tier::Server { .. })
    }

    #[allow(dead_code)]
    pub fn server_url(&self) -> Option<&str> {
        match self {
            Tier::Server { url, .. } => Some(url),
            Tier::Offline => None,
        }
    }

    #[allow(dead_code)]
    pub fn caps(&self) -> Option<&Capabilities> {
        match self {
            Tier::Server { caps, .. } => Some(caps),
            Tier::Offline => None,
        }
    }
}

/// Return the cached capability tier for this process.
///
/// On the first call, probes the server (if `server_url` is set) with a 2-second
/// timeout. Subsequent calls return immediately from the cache.
///
/// **Per-process cache**: the result is stored in a `static OnceCell` and is fixed
/// for the lifetime of the process. This is correct for CLI invocations (one process
/// = one config), but unsuitable for long-running daemons that may use multiple
/// configs — they would always see the tier determined by the first call.
pub async fn get_tier(cfg: &Config) -> &'static Tier {
    let url = cfg.server_url.clone();
    let key = cfg.server_key.clone();
    TIER.get_or_init(|| async move { probe(url.as_deref(), key.as_deref()).await })
        .await
}

async fn probe(url: Option<&str>, key: Option<&str>) -> Tier {
    let Some(url) = url else {
        return Tier::Offline;
    };

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("could not build HTTP client for server probe: {e}");
            return Tier::Offline;
        }
    };

    let mut req = client.get(format!("{}/v1/health", url.trim_end_matches('/')));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }

    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let caps = parse_capabilities(resp).await;
            Tier::Server {
                url: url.to_string(),
                caps,
            }
        }
        Ok(resp) => {
            tracing::warn!(
                "spelunk-server at {url} returned {} — running in offline mode",
                resp.status()
            );
            Tier::Offline
        }
        Err(e) => {
            tracing::warn!("spelunk-server at {url} unreachable — running in offline mode: {e}");
            Tier::Offline
        }
    }
}

async fn parse_capabilities(resp: reqwest::Response) -> Capabilities {
    #[derive(serde::Deserialize)]
    struct HealthBody {
        #[serde(default)]
        capabilities: Vec<String>,
    }

    match resp.json::<HealthBody>().await {
        Ok(body) => {
            let cap_strs: Vec<&str> = body.capabilities.iter().map(String::as_str).collect();
            Capabilities::from_server_caps(&cap_strs)
        }
        Err(_) => {
            // Legacy server returns plain-text "ok" — conservative fallback.
            Capabilities::legacy_memory_only()
        }
    }
}

/// Return `Ok(())` if the tier is `Server`, otherwise return an `anyhow::Error`
/// with the standard locked-feature message format.
///
/// Callers append `?` to propagate the error:
/// ```ignore
/// require_tier1("explore", tier, cfg.server_url.as_deref())?;
/// ```
pub fn require_tier1(feature: &str, tier: &Tier, server_url: Option<&str>) -> anyhow::Result<()> {
    if tier.is_server() {
        return Ok(());
    }
    let tried = server_url
        .map(|u| format!("\n       (Tried: {u} — connection refused)"))
        .unwrap_or_default();
    anyhow::bail!(
        "'spelunk {feature}' requires spelunk-server.\n\
         Set server_url in ~/.config/spelunk/config.toml to enable this feature.{tried}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Capabilities::from_server_caps ──────────────────────────────────────

    #[test]
    fn from_server_caps_empty_returns_all_false() {
        let caps = Capabilities::from_server_caps(&[]);
        assert!(!caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(!caps.memory_push);
        assert!(!caps.memory_pull);
        assert!(!caps.memory_search);
        assert!(!caps.memory_harvest);
        assert!(!caps.explore);
        assert!(!caps.plan);
    }

    #[test]
    fn from_server_caps_full_set() {
        let caps = Capabilities::from_server_caps(&[
            "search.semantic",
            "index.embed",
            "memory",
            "explore",
            "plan",
        ]);
        assert!(caps.search_semantic);
        assert!(caps.index_embed);
        assert!(caps.memory_push);
        assert!(caps.memory_pull);
        assert!(caps.memory_search);
        assert!(caps.memory_harvest);
        assert!(caps.explore);
        assert!(caps.plan);
    }

    #[test]
    fn from_server_caps_memory_only() {
        let caps = Capabilities::from_server_caps(&["memory"]);
        assert!(!caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(!caps.explore);
        assert!(!caps.plan);
        assert!(caps.memory_push);
        assert!(caps.memory_pull);
        assert!(caps.memory_search);
        assert!(caps.memory_harvest);
    }

    #[test]
    fn from_server_caps_partial_set() {
        let caps = Capabilities::from_server_caps(&["search.semantic", "plan"]);
        assert!(caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(!caps.explore);
        assert!(caps.plan);
        assert!(!caps.memory_push);
        assert!(!caps.memory_pull);
        assert!(!caps.memory_search);
        assert!(!caps.memory_harvest);
    }

    #[test]
    fn from_server_caps_unknown_capability_is_ignored() {
        let caps = Capabilities::from_server_caps(&["search.semantic", "unknown.future", "memory"]);
        assert!(caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(caps.memory_push);
        // Unknown capability should not affect any flag.
    }

    // ── Capabilities::legacy_memory_only ─────────────────────────────────────

    #[test]
    fn legacy_memory_only_values() {
        let caps = Capabilities::legacy_memory_only();
        assert!(!caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(!caps.explore);
        assert!(!caps.plan);
        assert!(caps.memory_push);
        assert!(caps.memory_pull);
        assert!(caps.memory_search);
        assert!(!caps.memory_harvest);
    }

    // ── Capabilities::all ────────────────────────────────────────────────────

    #[test]
    fn all_values_are_true() {
        let caps = Capabilities::all();
        assert!(caps.search_semantic);
        assert!(caps.index_embed);
        assert!(caps.memory_push);
        assert!(caps.memory_pull);
        assert!(caps.memory_search);
        assert!(caps.memory_harvest);
        assert!(caps.explore);
        assert!(caps.plan);
    }

    // ── Tier ─────────────────────────────────────────────────────────────────

    #[test]
    fn tier_server_is_server_true() {
        let tier = Tier::Server {
            url: "http://example.com".to_string(),
            caps: Capabilities::all(),
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
        };
        assert!(tier.caps().is_some());
    }

    #[test]
    fn tier_offline_returns_none_caps() {
        let tier = Tier::Offline;
        assert!(tier.caps().is_none());
    }

    // ── require_tier1 ────────────────────────────────────────────────────────

    #[test]
    fn require_tier1_ok_for_server() {
        let tier = Tier::Server {
            url: "http://example.com".to_string(),
            caps: Capabilities::all(),
        };
        assert!(require_tier1("explore", &tier, Some("http://example.com")).is_ok());
    }

    #[test]
    fn require_tier1_err_for_offline_no_url() {
        let tier = Tier::Offline;
        let err = require_tier1("explore", &tier, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'spelunk explore'"));
        assert!(msg.contains("requires spelunk-server"));
        assert!(msg.contains("server_url"));
    }

    #[test]
    fn require_tier1_err_for_offline_with_url_includes_tried() {
        let tier = Tier::Offline;
        let err = require_tier1("plan", &tier, Some("http://bad:7777")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'spelunk plan'"));
        assert!(msg.contains("requires spelunk-server"));
        assert!(msg.contains("http://bad:7777"));
        assert!(msg.contains("connection refused"));
    }

    #[test]
    fn require_tier1_uses_feature_name_in_message() {
        let tier = Tier::Offline;
        let err = require_tier1("memory push", &tier, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'spelunk memory push'"));
    }

    #[test]
    fn require_tier1_no_tried_line_when_url_not_set() {
        let tier = Tier::Offline;
        let err = require_tier1("explore", &tier, None).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("Tried:"));
    }
}
