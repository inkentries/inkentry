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
}

/// Return the cached capability tier for this process.
///
/// On the first call, probes the server (if `server_url` is set) with a 2-second
/// timeout. Subsequent calls return immediately from the cache.
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
