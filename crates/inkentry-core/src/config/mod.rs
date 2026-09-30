use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod caller;
mod paths;
mod persist;
mod predicates;
mod project_id;
mod sync_mode;
mod team_target;
mod tls;

pub mod llm_key;
pub mod org_tokens;
pub mod secret_store;
pub mod server_keys;

use paths::{find_project_config, inkentry_config_dir};
use secret_store::SecretStore;

pub use caller::CallerDeclaration;
pub use paths::{
    find_project_db, find_project_dir, require_project_db, require_project_db_at, resolve_db,
};
pub use persist::{remove_auth_tokens, remove_auth_tokens_from, write_project_slug};
pub use predicates::{
    is_loopback_url, is_loopback_url_missing_port, no_server_env_set, validate_transport_url,
};
pub use project_id::derive_project_id;
pub use sync_mode::SyncMode;
pub use team_target::{TeamTarget, declared_team_targets};
pub use tls::{apply_server_ca, find_rustls_cause};

static WARNINGS_SILENCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Stops [`Config::load`] and [`Config::validate`] writing warnings to stderr
/// for the rest of the process. For callers whose contract is to print
/// nothing, such as an agent hook.
pub fn silence_warnings() {
    WARNINGS_SILENCED.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn emit_warning(message: &str) {
    if !WARNINGS_SILENCED.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("{message}");
    }
}

/// Default TCP port for `inkentry-server`.
///
/// 4655 spells `inkl` on a phone keypad; team deployments conventionally use
/// 4658 (`inkt`).
pub const DEFAULT_SERVER_PORT: u16 = 4655;

/// How long a client may spend establishing a connection to a configured
/// server before it is reported unreachable.
///
/// Short by design: a real transfer or server-side embed can legitimately run
/// for minutes once connected, but a firewall that drops rather than refuses a
/// connection would otherwise burn the whole request budget with nothing to
/// fail on. Covers the TLS handshake as well as the bare TCP connect.
/// Liveness probes derive their own timeouts from this value rather than
/// reuse it, since a probe must tell "still connecting" apart from "waiting
/// for a reply".
pub const REMOTE_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(test)]
use tempfile::TempDir;

fn default_true() -> bool {
    true
}

/// The `[index]` config table: controls the built-in index-time file filter
/// that skips generated/vendored/minified/machine-data files (see
/// [`crate::indexer::filter`]). Distinct from the unconditional sensitive-file
/// exclusion (`.env`, keys), which is not configurable here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConfig {
    /// Extra gitignore-syntax exclude lines layered on top of the built-ins.
    /// A `!pattern` line re-includes a path the defaults would drop (last
    /// match wins). Cannot re-include a sensitive file (that layer is
    /// separate).
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Whether to apply the built-in default exclude set. Default `true`.
    #[serde(default = "default_true")]
    pub use_default_excludes: bool,
    /// Whether to skip files whose head self-declares as generated
    /// (`@generated` or `// Code generated ... DO NOT EDIT.`). Default `true`.
    #[serde(default = "default_true")]
    pub detect_generated: bool,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            exclude: Vec::new(),
            use_default_excludes: true,
            detect_generated: true,
        }
    }
}

// Per-field override of `IndexConfig` from a project `.inkentry/config.toml`.
// Every field is `Option` so an absent key leaves the layered value untouched.
#[derive(Debug, Default, Deserialize)]
struct ProjectIndexConfig {
    exclude: Option<Vec<String>>,
    use_default_excludes: Option<bool>,
    detect_generated: Option<bool>,
}

/// Whether a non-empty duplicate-band candidate refuses `memory add`'s write
/// until the caller resolves it. `--reconcile` on the command selects
/// [`Self::Block`] for that one invocation regardless of what this resolves
/// to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReconcileMode {
    /// Write as today; `candidates`/`related` are additive response fields.
    #[default]
    Off,
    /// Refuse a write with an unresolved duplicate-band candidate.
    Block,
}

/// The `[memory]` config table. Project-only, like `[index]`: whether
/// `memory add` blocks is a team policy, not a personal preference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default)]
    pub reconcile: ReconcileMode,
}

// Per-field override of `MemoryConfig` from a project `.inkentry/config.toml`.
#[derive(Debug, Default, Deserialize)]
struct ProjectMemoryConfig {
    reconcile: Option<ReconcileMode>,
}

// Fields settable in `.inkentry/config.toml` (project-level, checked-in). Only
// fields safe to share with the team (no secrets).
//
// No `server_key` field: a credential in a committed file is in the repo's
// history for good. A file that still has one keeps working for its other
// fields and is named on stderr so its owner knows to rotate it (use
// `inkentry auth set-key --server <url>` instead); the personal config is read
// the same way, via `personal_config_credential_warning`.
//
// Every other key the file is not read for is likewise named on stderr rather
// than dropped in silence. The keys here are mirrored in
// `PROJECT_CONFIG_KEYS`, which the warning reads; adding a field here means
// adding it there too.
#[derive(Debug, Default, Deserialize)]
struct ProjectConfig {
    server_url: Option<String>,
    // Mutually exclusive with `server_url`.
    cloud: Option<bool>,
    project_id: Option<String>,
    // A WorkOS org id, a slug, or a local org UUID. Not a secret, so it
    // belongs in the committed file next to `project_id`.
    org: Option<String>,
    // A team pointing at one approved provider states it once here rather
    // than in every developer's own file. The credential it is presented to
    // is not a config key in either file.
    llm_url: Option<String>,
    server_ca: Option<String>,
    // Overrides a personal value; `INKENTRY_MODE` still wins over both.
    mode: Option<SyncMode>,
    index: Option<ProjectIndexConfig>,
    memory: Option<ProjectMemoryConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Path to the SQLite database file
    #[serde(default = "Config::default_db_path")]
    pub db_path: PathBuf,

    /// Chat model id, resolved by inkentry-server, for `ask` and `harvest`.
    /// When unset, commands that require a chat model are unavailable.
    /// `INKENTRY_LLM_MODEL` overrides this.
    #[serde(default)]
    pub llm_model: Option<String>,

    /// Base URL of an OpenAI-compatible chat completions endpoint (a local
    /// LM Studio / Ollama, or a self-hosted gateway), passed to the
    /// auto-spawned `inkentry-server` so it gains LLM capability.
    ///
    /// Settable in either file, with `.inkentry/config.toml` winning over the
    /// personal one, and `INKENTRY_LLM_URL` winning over both. The credential
    /// presented to this endpoint is never itself a config field (`inkentry
    /// auth set-key --llm` or `INKENTRY_LLM_KEY`).
    #[serde(default)]
    pub llm_url: Option<String>,

    /// URL of the inkentry-server instance, e.g.
    /// `https://inkentry.internal.example.com` (or
    /// `http://127.0.0.1:<DEFAULT_SERVER_PORT>` for loopback; non-loopback
    /// `http://` is rejected). When set, the CLI operates in Tier 1
    /// (server-connected) mode, enabling semantic search and embedding.
    ///
    /// Settable only via `.inkentry/config.toml` or `INKENTRY_SERVER_URL`: a
    /// value from the personal config is discarded, since a team server is a
    /// project-wide choice, not a per-developer one.
    #[serde(default)]
    pub server_url: Option<String>,

    /// When true, this project uses the hosted inkentry cloud, targeting the
    /// compile-time cloud URL ([`server_keys::DEFAULT_CLOUD_URL`]). Mutually
    /// exclusive with `server_url`, which names a self-hosted team server.
    ///
    /// Settable only in `.inkentry/config.toml`; a personal value is
    /// discarded, like `server_url`.
    #[serde(default)]
    pub cloud: bool,

    /// Project slug for the inkentry-server (e.g. `acme/my-app`).
    /// Required when `server_url` is set.
    /// Set in `.inkentry/config.toml` (project-level) or via `INKENTRY_PROJECT_ID`.
    #[serde(default)]
    pub project_id: Option<String>,

    /// Path to a PEM CA bundle trusted in addition to the built-in roots, for a
    /// team `server_url` whose certificate is signed by a self-signed or
    /// internal CA. Verification stays on — this only adds a trust anchor, it
    /// does not disable checks.
    ///
    /// `INKENTRY_SERVER_CA` overrides this; settable in either config file.
    #[serde(default)]
    pub server_ca: Option<String>,

    /// Sync mode: `offline` / `local_first` / `cloud_first`.
    ///
    /// Settable in either config file, with `.inkentry/config.toml` winning
    /// over the personal one, and `INKENTRY_MODE` winning over both. Unlike
    /// `server_url`, a personal value is not discarded: this field only
    /// chooses a behaviour toward whatever server the project config already
    /// named, so it cannot send anything anywhere the project config did not
    /// already permit.
    ///
    /// Stored as `Option` so an absent value can derive the effective mode
    /// from `server_url` instead of pinning one — see [`Config::resolve_mode`],
    /// which is the only place this field should be read.
    #[serde(default)]
    pub mode: Option<SyncMode>,

    /// URL of a server used only for inference (embeddings + LLM), never for
    /// memory storage. Populated at runtime, not from config files, when a
    /// loopback server is auto-discovered.
    ///
    /// Inference clients prefer this field and fall back to `server_url`; the
    /// memory backend selector ignores it entirely, so an auto-discovered
    /// server never diverts memory CRUD away from the project's local
    /// `memory.db`.
    #[serde(skip)]
    pub inference_url: Option<String>,

    /// Context-window size (tokens) of the LLM used for `harvest` and `ask`.
    /// inkentry uses this to split harvest batches that would overflow the model's window.
    /// Set to match the `n_ctx` / context-length of the model you have loaded.
    /// Default: 8192
    #[serde(default = "Config::default_llm_context_length")]
    pub llm_context_length: usize,

    /// When true (the default), `inkentry memory add` also appends the new
    /// entry as a line of JSON in `refs/notes/inkentry` on HEAD. Set
    /// `store_in_git_notes = false` to opt out.
    ///
    /// Failure to write the git note is non-fatal: a warning is logged and the
    /// primary SQLite write is unaffected.
    #[serde(default = "Config::default_store_in_git_notes")]
    pub store_in_git_notes: bool,

    /// Organization this repo is pinned to. Accepts the same forms `org
    /// switch` does — a WorkOS org id, a slug, or a local org UUID — and
    /// selects which cached WorkOS session cloud requests use, above the
    /// cache's `active` pointer.
    ///
    /// Not a secret, so it lives in the committed `.inkentry/config.toml`,
    /// never the secret store. `INKENTRY_ORG` overrides it at load; an
    /// explicit `--org` flag outranks even that. The WorkOS session itself
    /// lives in the org-token cache (see [`org_tokens`](crate::config::org_tokens)).
    #[serde(default)]
    pub org: Option<String>,

    /// `[index]` table: built-in index-time file filter settings. Project
    /// `.inkentry/config.toml` overrides the global value per field.
    #[serde(default)]
    pub index: IndexConfig,

    /// `[memory]` table: `reconcile = "block"` makes `memory add` block on
    /// an unresolved duplicate-band candidate, the same as passing
    /// `--reconcile`. Project `.inkentry/config.toml` only.
    #[serde(default)]
    pub memory: MemoryConfig,

    /// The caller's self-declaration
    /// (`INKENTRY_TRIGGER`/`INKENTRY_ACTOR`/`INKENTRY_SESSION_REF`/
    /// `INKENTRY_TOOL`/`INKENTRY_MODEL`), read once from the environment at
    /// [`Config::load`] — never from a config file, and never guessed from a
    /// TTY check.
    #[serde(skip)]
    pub caller: CallerDeclaration,
}

/// One organization's WorkOS session.
///
/// Written by `inkentry login` / `inkentry org switch` and rotated by the
/// token refresh path, cached per organization in the secret store keyed by
/// `org_id` (see [`org_tokens`](crate::config::org_tokens)). A legacy
/// plaintext `[auth]` table is migrated into this shape on the first load
/// that finds one.
///
/// Every field is `#[serde(default)]`, so a partial session — a login without
/// an org, or a hand-trimmed table — never fails the whole load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthTokens {
    /// Short-lived WorkOS access token, sent as `Authorization: Bearer`.
    /// Empty (or absent) means not logged in: no bearer is resolved.
    #[serde(default)]
    pub access_token: String,
    /// Long-lived rotating refresh token, exchanged directly at WorkOS's
    /// refresh grant to rotate the access token or switch organisation.
    #[serde(default)]
    pub refresh_token: String,
    /// Absolute expiry of `access_token`, as a Unix timestamp (seconds).
    /// Absent ⇒ `0`, which [`AuthTokens::is_expired_at`] treats as expired.
    #[serde(default)]
    pub expires_at: i64,
    /// WorkOS organisation the tokens are scoped to. Empty means logged in
    /// without an org: no scoping is applied.
    #[serde(default)]
    pub org_id: String,
    /// Normalized origin of the cloud host these tokens were issued for. The
    /// access token is released only to this origin (see
    /// [`server_keys::bearer_for`]), so a request aimed at a different cloud
    /// URL cannot be sent a token issued for another host. Empty resolves to
    /// no bearer.
    #[serde(default)]
    pub cloud_origin: String,
}

impl AuthTokens {
    /// Whether the access token is at or past its expiry, with a small skew
    /// margin so a token that is about to expire is refreshed pre-emptively
    /// rather than failing mid-request.
    pub fn is_expired(&self) -> bool {
        self.is_expired_at(chrono::Utc::now().timestamp())
    }

    /// [`is_expired`](Self::is_expired) against an explicit `now` (Unix
    /// seconds). Treats the token as expired 30 s early.
    pub fn is_expired_at(&self, now: i64) -> bool {
        const SKEW_SECS: i64 = 30;
        now >= self.expires_at - SKEW_SECS
    }
}

impl Config {
    fn default_db_path() -> PathBuf {
        inkentry_config_dir().join("index.db")
    }
    fn default_llm_context_length() -> usize {
        8192
    }
    fn default_store_in_git_notes() -> bool {
        true
    }

    /// Whether `[memory] reconcile = "block"` is set. `--reconcile` on the
    /// command turns on the same behaviour for one invocation regardless of
    /// this; callers check both.
    pub fn reconcile_block(&self) -> bool {
        self.memory.reconcile == ReconcileMode::Block
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db_path: Self::default_db_path(),
            llm_model: None,
            llm_url: None,
            server_url: None,
            cloud: false,
            project_id: None,
            server_ca: None,
            mode: None,
            inference_url: None,
            llm_context_length: Self::default_llm_context_length(),
            store_in_git_notes: Self::default_store_in_git_notes(),
            org: None,
            index: IndexConfig::default(),
            memory: MemoryConfig::default(),
            caller: CallerDeclaration::default(),
        }
    }
}

impl Config {
    /// Cheaply checks whether the personal config sets `llm_model`, without
    /// resolving the bearer credential or touching the secret store.
    pub fn llm_model_configured(path: Option<&Path>) -> bool {
        let global_path = match path {
            Some(p) => p.to_path_buf(),
            None => inkentry_config_dir().join("config.toml"),
        };
        let Ok(raw) = std::fs::read_to_string(&global_path) else {
            return false;
        };
        toml::from_str::<Config>(&raw)
            .map(|c| c.llm_model.is_some())
            .unwrap_or(false)
    }

    /// Load config with layered overrides:
    ///   1. Defaults
    ///   2. `~/.config/inkentry/config.toml` (global personal)
    ///   3. `.inkentry/config.toml` discovered by walking up from CWD (project-level, team-wide)
    ///   4. Environment variables: `INKENTRY_SERVER_URL`, `INKENTRY_PROJECT_ID`
    ///
    /// `server_url` is the one field step 2 cannot set: a team server is a
    /// project-wide decision, so only the checked-in project config or an
    /// explicit env var may supply it. `INKENTRY_SERVER_KEY` takes no part in
    /// this load — it is a bearer, resolved at request time by
    /// [`server_keys::bearer_for`].
    ///
    /// Pass `path` to override the global config location (used by `--config` flag).
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let store = secret_store::default_store(&inkentry_config_dir())?;
        Self::load_with_store(path, store.as_ref())
    }

    /// Same as [`Config::load`] but with an injected [`SecretStore`], so a
    /// test can assert that the load reads no secret without touching the
    /// host keychain.
    pub fn load_with_store(path: Option<&Path>, store: &dyn SecretStore) -> Result<Self> {
        let project_root = std::env::current_dir().ok();
        Self::load_with_store_from(path, store, project_root.as_deref())
    }

    // Like `load_with_store`, but project discovery walks up from
    // `project_root` instead of the CWD. `None` skips discovery entirely, so a
    // hermetic test load never picks up this repo's own `.inkentry/config.toml`.
    pub(crate) fn load_with_store_from(
        path: Option<&Path>,
        store: &dyn SecretStore,
        project_root: Option<&Path>,
    ) -> Result<Self> {
        let global_path = match path {
            Some(p) => p.to_path_buf(),
            None => inkentry_config_dir().join("config.toml"),
        };
        let mut cfg: Config = if global_path.exists() {
            let raw = std::fs::read_to_string(&global_path)
                .with_context(|| format!("reading config at {}", global_path.display()))?;
            let parsed = parse_global_config(&raw, &global_path)?;
            if let Some(warning) = personal_config_credential_warning(&raw, &global_path) {
                emit_warning(&warning);
            }
            // Migrate a legacy plaintext [auth] session into the org-token
            // cache and strip it from the file. Best-effort: a store that
            // cannot be written (a locked keychain) must not brick every
            // command, so the session is left in place to retry on a later load.
            if let Some(legacy) = legacy_auth_tokens(&raw)
                && let Err(e) = migrate_legacy_auth(store, &global_path, &legacy)
            {
                emit_warning(&format!(
                    "Warning: could not migrate the stored cloud session out of {} into the \
                     secret store; it will be retried on the next run: {e:#}",
                    global_path.display()
                ));
            }
            parsed
        } else {
            Config::default()
        };
        // A personal config must never point the CLI at a team server on its
        // own: everyone on a project needs the same server_url, which only the
        // project config or an env var can guarantee. The cloud opt-in is
        // project-wide for the same reason.
        cfg.server_url = None;
        cfg.cloud = false;

        // `ProjectConfig` has no `server_key` field: a checked-in file never
        // carries a credential. A file that still has one keeps working for
        // its other fields; every unread key, credentials included, is named
        // on stderr.
        if let Some(root) = project_root
            && let Some(proj_path) = find_project_config(root)
        {
            let raw = std::fs::read_to_string(&proj_path)
                .with_context(|| format!("reading project config at {}", proj_path.display()))?;
            let proj = parse_project_config(&raw, &proj_path)?;
            for warning in project_config_key_warnings(&raw, &proj_path) {
                emit_warning(&warning);
            }

            if let Some(v) = proj.server_url {
                cfg.server_url = Some(v);
            }
            if let Some(v) = proj.cloud {
                cfg.cloud = v;
            }
            if let Some(v) = proj.project_id {
                cfg.project_id = Some(v);
            }
            if let Some(v) = proj.org {
                cfg.org = Some(v);
            }
            if let Some(v) = proj.server_ca {
                cfg.server_ca = Some(v);
            }
            if let Some(v) = proj.mode {
                cfg.mode = Some(v);
            }
            if let Some(v) = proj.llm_url {
                cfg.llm_url = Some(v);
            }
            // Overrides the global value per field: an absent key in the
            // project table leaves the global (or default) value in place.
            if let Some(pidx) = proj.index {
                if let Some(v) = pidx.exclude {
                    cfg.index.exclude = v;
                }
                if let Some(v) = pidx.use_default_excludes {
                    cfg.index.use_default_excludes = v;
                }
                if let Some(v) = pidx.detect_generated {
                    cfg.index.detect_generated = v;
                }
            }
            if let Some(pmem) = proj.memory
                && let Some(v) = pmem.reconcile
            {
                cfg.memory.reconcile = v;
            }
        }

        if let Ok(v) = std::env::var("INKENTRY_SERVER_URL") {
            cfg.server_url = Some(v);
        }
        if let Ok(v) = std::env::var("INKENTRY_PROJECT_ID") {
            cfg.project_id = Some(v);
        }
        // INKENTRY_ORG outranks the project `org` pin; an explicit --org flag
        // outranks even this, but is applied at the call site (see
        // `org_tokens::resolve_session`), not here.
        if let Ok(v) = std::env::var(org_tokens::ENV_ORG) {
            cfg.org = Some(v);
        }
        if let Ok(v) = std::env::var("INKENTRY_SERVER_CA") {
            cfg.server_ca = Some(v);
        }
        if let Ok(v) = std::env::var(llm_key::ENV_LLM_URL) {
            cfg.llm_url = Some(v);
        }
        if let Ok(v) = std::env::var(llm_key::ENV_LLM_MODEL) {
            cfg.llm_model = Some(v);
        }
        // An unrecognised value is a hard error: silently falling back to a
        // default would defeat the point of a deterministic mode switch.
        if let Ok(v) = std::env::var("INKENTRY_MODE") {
            let parsed = SyncMode::parse(&v).with_context(|| {
                format!(
                    "INKENTRY_MODE={v:?} is not a valid sync mode (expected one of: {})",
                    SyncMode::valid_values()
                )
            })?;
            cfg.mode = Some(parsed);
        }

        // `cloud = true` targets the hosted cloud; `server_url` names a
        // self-hosted team server. The two cannot both apply.
        if cfg.cloud {
            if cfg.server_url.is_some() {
                anyhow::bail!(
                    "`cloud = true` and `server_url` cannot both be set: a project uses either the hosted cloud or a self-hosted team server"
                );
            }
            // Mode is left alone: an unset mode derives `local_first` from
            // this server_url, exactly as for a team server.
            cfg.server_url = Some(server_keys::cloud_url());
        }

        // Read once here, never from a config file and never re-derived per
        // command, so every command in this process shares one declaration.
        cfg.caller = CallerDeclaration::from_env();

        Ok(cfg)
    }

    /// Resolve the effective bearer for a request to `server_url`, using the
    /// host's default secret store. See [`Config::bearer_for_with_store`] for
    /// the resolution rules.
    pub fn bearer_for(&self, server_url: &str) -> Result<Option<String>> {
        let store = secret_store::default_store(&inkentry_config_dir())?;
        self.bearer_for_with_store(server_url, store.as_ref())
    }

    /// Same as [`Config::bearer_for`] but with an injected [`SecretStore`]
    /// (tests, and callers that already resolved a store).
    ///
    /// `INKENTRY_SERVER_KEY` outranks everything and is checked before the
    /// store is touched at all, so an env-supplied key costs no keychain
    /// read. The cloud credential is the resolved org's cached session; it
    /// and the self-hosted server-key kind are then branched on
    /// `server_url`'s origin by [`server_keys::bearer_for`].
    pub fn bearer_for_with_store(
        &self,
        server_url: &str,
        store: &dyn SecretStore,
    ) -> Result<Option<String>> {
        if let Ok(v) = std::env::var(server_keys::ENV_SERVER_KEY) {
            return Ok(Some(v));
        }
        let session = org_tokens::resolve_session(store, self.org.as_deref())?;
        server_keys::bearer_for(session.as_ref(), server_url, store)
    }

    /// The cached WorkOS session this invocation resolves to — its pinned
    /// ([`Config::org`]) or the cache's active org — read from `store`. `None`
    /// when not logged in for that org. Used by the refresh and org-switch
    /// paths that need the whole session, not just the bearer.
    pub fn cloud_session_with_store(&self, store: &dyn SecretStore) -> Result<Option<AuthTokens>> {
        org_tokens::resolve_session(store, self.org.as_deref())
    }

    /// [`Config::cloud_session_with_store`] against the host's default secret
    /// store.
    pub fn cloud_session(&self) -> Result<Option<AuthTokens>> {
        let store = secret_store::default_store(&inkentry_config_dir())?;
        self.cloud_session_with_store(store.as_ref())
    }
}

/// Resolve the host's default [`SecretStore`], honouring [`secret_store::ENV_SECRET_STORE`].
/// The entry point for CLI commands that need to read or write the per-origin
/// key map directly (`inkentry auth set-key` / `list-servers` / `remove-key`),
/// the same resolution [`Config::load`] and [`Config::bearer_for`] use
/// internally.
pub fn default_secret_store() -> Result<Box<dyn SecretStore>> {
    secret_store::default_store(&inkentry_config_dir())
}

/// Cache `tokens` as the active org's WorkOS session in the host's default
/// secret store — what `inkentry login` / `inkentry org switch` persist once a
/// session is minted. `slug` records the human identifier so a repo can later
/// pin `org = "<slug>"`.
pub fn store_active_session(tokens: &AuthTokens, slug: Option<&str>) -> Result<()> {
    let store = default_secret_store()?;
    org_tokens::set_active(store.as_ref(), tokens, slug)
}

/// Write `tokens` back into their own org's cached slot without moving the
/// active pointer or touching any sibling org — the refresh rotation
/// persistence, against the host's default secret store.
pub fn update_org_session(tokens: &AuthTokens) -> Result<()> {
    let store = default_secret_store()?;
    org_tokens::update_in_place(store.as_ref(), tokens)
}

// Deserialize a legacy plaintext [auth] table out of a raw config.toml, or
// None when the file has none. Used only on the migration path.
fn legacy_auth_tokens(raw: &str) -> Option<AuthTokens> {
    let table = raw.parse::<toml::Table>().ok()?;
    let auth = table.get("auth")?.clone();
    auth.try_into::<AuthTokens>().ok()
}

// Move a legacy [auth] session into the org-token cache and strip the table
// from `config_path`. The cache write happens first so a failure to rewrite
// the file afterward leaves the session recoverable and the next load simply
// re-runs an idempotent migration.
fn migrate_legacy_auth(
    store: &dyn SecretStore,
    config_path: &Path,
    legacy: &AuthTokens,
) -> Result<()> {
    org_tokens::migrate_legacy(store, legacy)?;
    persist::remove_auth_tokens_from(config_path)
}

// Warning line for a loopback `server_url` with no port, or `None` when none
// applies. Pure so it's unit-testable without capturing stderr;
// `Config::validate_with_project` prints the result.
//
// A loopback `server_url` missing a port can never be the auto-discovered
// local daemon, which always binds a specific port (DEFAULT_SERVER_PORT): a
// near-certain leftover misconfiguration. This is a warning, not a validation
// error: unlike a non-loopback plaintext `http://` URL, it isn't a security
// problem, just a likely mistake.
fn portless_loopback_server_url_warning(url: &str) -> Option<String> {
    if !is_loopback_url_missing_port(url) {
        return None;
    }
    Some(format!(
        "Warning: server_url ({url}) is a loopback host with no port; this can never be \
         the auto-discovered local server (default port {DEFAULT_SERVER_PORT}). If this is a leftover \
         value, remove server_url from config; otherwise add the port your server \
         actually listens on."
    ))
}

// Actionable error for a config.toml that failed to parse: names the file,
// embeds the toml diagnostic pinpointing the offending key, and states the
// remedy. A bare `.context("parsing config.toml")` loses the file path and the
// toml diagnostic, since `anyhow::Error`'s `Display` shows only its top context.
fn config_parse_error(path: &Path, source: toml::de::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "could not parse the inkentry config file {path}:\n{source}\n\
         Fix the offending key shown above, or remove it to fall back to the default.",
        path = path.display(),
    )
}

// Parse the global personal config.toml, turning any failure into an
// actionable error (see `config_parse_error`). An unrecognised `mode` is
// singled out so the message names the bad value and lists the accepted set,
// rather than relying on however serde/toml renders the enum error.
fn parse_global_config(raw: &str, path: &Path) -> Result<Config> {
    toml::from_str::<Config>(raw).map_err(|source| {
        if let Some(bad) = bad_mode_value(raw) {
            return anyhow::anyhow!(
                "invalid `mode` value {bad:?} in the inkentry config file {path} \
                 (expected one of: {valid})",
                path = path.display(),
                valid = SyncMode::valid_values(),
            );
        }
        config_parse_error(path, source)
    })
}

// Parse a project-level .inkentry/config.toml, with the same `mode`
// diagnostic as `parse_global_config`.
fn parse_project_config(raw: &str, path: &Path) -> Result<ProjectConfig> {
    toml::from_str::<ProjectConfig>(raw).map_err(|source| {
        if let Some(bad) = bad_mode_value(raw) {
            return anyhow::anyhow!(
                "invalid `mode` value {bad:?} in the inkentry config file {path} \
                 (expected one of: {valid})",
                path = path.display(),
                valid = SyncMode::valid_values(),
            );
        }
        config_parse_error(path, source)
    })
}

// The keys `.inkentry/config.toml` is read for. Single source of truth for the
// merge in `load_with_store_from` and for `project_config_key_warnings`, so
// the two cannot drift.
const PROJECT_CONFIG_KEYS: &[&str] = &[
    "server_url",
    "cloud",
    "project_id",
    "org",
    "server_ca",
    "mode",
    "llm_url",
    "index",
    "memory",
];

// Keys the project config has no field for that name a credential. These get
// their own wording: the file is committed, so the value is already in the
// repository's history and no client change can take it back. Rotation is the
// only remedy.
const PROJECT_CONFIG_CREDENTIAL_KEYS: &[&str] = &["server_key"];

// Warnings for keys present in a project .inkentry/config.toml that it is not
// read for. Pure so the wording is unit-testable without capturing stderr;
// `Config::load_with_store_from` prints the result. A malformed file yields
// nothing: the typed parse alongside this already fails with a diagnostic
// pointing at the offending line.
fn project_config_key_warnings(raw: &str, path: &Path) -> Vec<String> {
    let Ok(table) = raw.parse::<toml::Table>() else {
        return Vec::new();
    };
    table
        .keys()
        .filter(|key| !PROJECT_CONFIG_KEYS.contains(&key.as_str()))
        .map(|key| {
            let path = path.display();
            if PROJECT_CONFIG_CREDENTIAL_KEYS.contains(&key.as_str()) {
                return format!(
                    "Warning: `{key}` in {path} has no effect, and a credential in a committed \
                     file is already in the repository's history: rotate it, then set the \
                     replacement with `inkentry auth set-key --server <url>` or \
                     INKENTRY_SERVER_KEY."
                );
            }
            format!(
                "Warning: `{key}` in {path} has no effect: the project config is read for \
                 {read} only. Check the spelling, or set it in \
                 ~/.config/inkentry/config.toml instead.",
                read = PROJECT_CONFIG_KEYS.join(", "),
            )
        })
        .collect()
}

// Warning for a `server_key` still present in the personal
// ~/.config/inkentry/config.toml. The field is not read, and the value it
// names has been sitting in a plaintext file, so rotation is the remedy and
// `inkentry auth set-key` is where the replacement goes.
fn personal_config_credential_warning(raw: &str, path: &Path) -> Option<String> {
    let table = raw.parse::<toml::Table>().ok()?;
    table.contains_key("server_key").then(|| {
        format!(
            "Warning: `server_key` in {path} is no longer read. Rotate the key it holds \
             (a plaintext file is not a credential store), then set the replacement with \
             `inkentry auth set-key --server <url>` or INKENTRY_SERVER_KEY.",
            path = path.display(),
        )
    })
}

// If `raw` sets `mode` to a string that is not a valid SyncMode, return that
// value. Runs only on the error path.
fn bad_mode_value(raw: &str) -> Option<String> {
    let table = raw.parse::<toml::Table>().ok()?;
    let value = table.get("mode")?.as_str()?;
    SyncMode::parse(value).is_none().then(|| value.to_string())
}

impl Config {
    /// Validate cross-field constraints. Call after `load()`.
    ///
    /// When `server_url` points to a loopback address (`127.0.0.1`,
    /// `localhost`, `::1`), `project_id` is allowed to be absent — it is
    /// derived at runtime by [`Config::resolve_project_id`].
    pub fn validate(&self) -> Result<()> {
        self.validate_with_project(self.project_id.is_some())
    }

    /// Like [`validate`](Self::validate) but lets the caller assert that a
    /// project identity is available from a source outside the config — e.g.
    /// an explicit `inkentry sync --project <slug>` flag.
    ///
    /// Pass `project_available = true` when the caller resolves its own
    /// project slug lazily, so the non-loopback `server_url` requirement is
    /// satisfied without a persisted `project_id`. The actual slug resolution
    /// is done by the caller.
    pub fn validate_with_project(&self, project_available: bool) -> Result<()> {
        if let Some(url) = &self.server_url
            && !project_available
            && !is_loopback_url(url)
        {
            anyhow::bail!(
                "server_url is set but project_id is missing.\n\
                 Add `project_id = \"my-project\"` to .inkentry/config.toml \
                 or set INKENTRY_PROJECT_ID."
            );
        }
        if let Some(url) = &self.server_url
            && let Some(warning) = portless_loopback_server_url_warning(url)
        {
            emit_warning(&warning);
        }
        Ok(())
    }

    /// The effective project id: `project_id` as set, or one derived from
    /// `project_root` via [`derive_project_id`].
    pub fn resolve_project_id(&self, project_root: &Path) -> String {
        self.project_id
            .clone()
            .unwrap_or_else(|| derive_project_id(project_root))
    }

    /// The URL to use for inference (embeddings + LLM), if any.
    ///
    /// Always prefers `inference_url` (set for an auto-discovered loopback
    /// server). Whether it also falls back to `server_url` depends on
    /// [`Config::resolve_mode`]:
    ///
    /// - `cloud_first`: falls back to `server_url` — the explicit remote owns
    ///   both inference and memory.
    /// - `local_first` / `offline`: never falls back to `server_url`. An
    ///   explicit `server_url` in these modes is a sync replica only;
    ///   inference always prefers the local loopback embedder, which
    ///   `inference_url` alone carries.
    ///
    /// Memory storage selection does not use this — see `open_memory_backend`.
    pub fn resolve_inference_url(&self) -> Option<&str> {
        if self.resolve_mode() == SyncMode::CloudFirst {
            self.inference_url.as_deref().or(self.server_url.as_deref())
        } else {
            self.inference_url.as_deref()
        }
    }

    /// Resolve the effective sync mode.
    ///
    /// Precedence (highest first):
    /// 1. `INKENTRY_NO_SERVER=1` (or `true`/`yes`) → [`SyncMode::Offline`], a
    ///    hard kill switch that wins over everything else.
    /// 2. An explicit `mode` in config / `INKENTRY_MODE` (already folded into
    ///    `self.mode` by [`Config::load`]).
    /// 3. No `server_url` ⇒ [`SyncMode::Offline`]; `server_url` present ⇒
    ///    [`SyncMode::LocalFirst`].
    ///
    /// The single source of truth for the mode — backend selection and the
    /// tier probe both call it rather than reading `self.mode` directly.
    pub fn resolve_mode(&self) -> SyncMode {
        if no_server_env_set() {
            return SyncMode::Offline;
        }
        if let Some(mode) = self.mode {
            return mode;
        }
        if self.server_url.is_some() {
            SyncMode::LocalFirst
        } else {
            SyncMode::Offline
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secret_store::MemoryStore;

    // A fresh in-memory secret store, so credential tests never touch the host
    // keychain.
    fn load_hermetic(path: &Path) -> Result<Config> {
        load_hermetic_with(path, &MemoryStore::default())
    }

    // No project-config discovery, so a .inkentry/config.toml checked in to this
    // repo cannot leak into the loaded Config (that is what makes it hermetic).
    fn load_hermetic_with(path: &Path, store: &dyn SecretStore) -> Result<Config> {
        Config::load_with_store_from(Some(path), store, None)
    }

    fn clear_inkentry_env() {
        unsafe {
            std::env::remove_var("INKENTRY_SERVER_URL");
            std::env::remove_var("INKENTRY_SERVER_KEY");
            std::env::remove_var("INKENTRY_PROJECT_ID");
            std::env::remove_var("INKENTRY_MODE");
            std::env::remove_var("INKENTRY_NO_SERVER");
            std::env::remove_var("INKENTRY_LLM_URL");
            std::env::remove_var("INKENTRY_LLM_MODEL");
            std::env::remove_var("INKENTRY_CLOUD_URL");
        }
    }

    // The cloud-kind bearer, resolved the way every caller resolves it.
    fn cloud_bearer(cfg: &Config, store: &dyn SecretStore) -> Option<String> {
        cfg.bearer_for_with_store(server_keys::DEFAULT_CLOUD_URL, store)
            .unwrap()
    }

    #[test]
    #[serial_test::serial]
    fn resolve_mode_defaults_offline_without_server_url() {
        clear_inkentry_env();
        let cfg = Config::default();
        assert_eq!(cfg.resolve_mode(), SyncMode::Offline);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_mode_defaults_local_first_with_server_url() {
        clear_inkentry_env();
        let cfg = Config {
            server_url: Some("http://team.example.com:4655".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_mode(), SyncMode::LocalFirst);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_mode_explicit_mode_wins_over_default() {
        clear_inkentry_env();
        let cfg = Config {
            server_url: Some("http://team.example.com:4655".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_mode(), SyncMode::CloudFirst);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_mode_no_server_env_forces_offline() {
        clear_inkentry_env();
        let cfg = Config {
            server_url: Some("http://team.example.com:4655".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };
        unsafe { std::env::set_var("INKENTRY_NO_SERVER", "1") };
        assert_eq!(cfg.resolve_mode(), SyncMode::Offline);
        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };
    }

    #[test]
    #[serial_test::serial]
    fn env_inkentry_mode_overrides_config() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "mode = \"offline\"\n").unwrap();

        unsafe { std::env::set_var("INKENTRY_MODE", "cloud_first") };
        let cfg = load_hermetic(&config_path).unwrap();
        assert_eq!(cfg.mode, Some(SyncMode::CloudFirst));
        unsafe { std::env::remove_var("INKENTRY_MODE") };
    }

    #[test]
    #[serial_test::serial]
    fn env_inkentry_mode_invalid_is_hard_error() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        unsafe { std::env::set_var("INKENTRY_MODE", "sideways") };
        let err = load_hermetic(&config_path).unwrap_err();
        assert!(err.to_string().contains("INKENTRY_MODE"));
        unsafe { std::env::remove_var("INKENTRY_MODE") };
    }

    #[test]
    #[serial_test::serial]
    fn config_toml_mode_parses() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "mode = \"local_first\"\n").unwrap();
        let cfg = load_hermetic(&config_path).unwrap();
        assert_eq!(cfg.mode, Some(SyncMode::LocalFirst));
    }

    #[test]
    #[serial_test::serial]
    fn config_with_pruned_keys_still_parses() {
        // `Config` has no `deny_unknown_fields`: pruned dead keys are ignored,
        // not rejected, so old config files keep loading.
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
mode = "local_first"
batch_size = 32
models_dir = "/opt/models"
api_base_url = "http://inference.internal:1234"
lmstudio_base_url = "http://127.0.0.1:1234"
plans_dir = "docs/plans"
specs_dir = "docs/specs"
embedding_model = "some-other-model"
"#,
        )
        .unwrap();

        let cfg = load_hermetic(&config_path).unwrap();
        assert_eq!(cfg.mode, Some(SyncMode::LocalFirst));
    }

    #[test]
    #[serial_test::serial]
    fn deprecated_memory_server_keys_are_ignored() {
        // The old aliases no longer populate server_url/server_key and are
        // silently dropped as unknown.
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
memory_server_url = "http://old.example.com:4655"
memory_server_key = "secret-token"
project_id = "my-proj"
"#,
        )
        .unwrap();

        let store = MemoryStore::default();
        let cfg = load_hermetic_with(&config_path, &store).unwrap();
        assert_eq!(cfg.server_url, None);
        assert_eq!(cloud_bearer(&cfg, &store), None);
        assert_eq!(cfg.project_id, Some("my-proj".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn a_personal_config_server_key_is_read_for_nothing_and_left_where_it_is() {
        // The plaintext key resolves nowhere, is not lifted into the secret
        // store, and the file is not rewritten behind the user's back.
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            "server_key = \"sk-legacy\"\nllm_model = \"gpt-oss\"\n",
        )
        .unwrap();

        let store = MemoryStore::default();
        let cfg = load_hermetic_with(&config_path, &store).unwrap();
        assert_eq!(cfg.llm_model.as_deref(), Some("gpt-oss"));
        assert_eq!(cloud_bearer(&cfg, &store), None);
        assert_eq!(
            cfg.bearer_for_with_store("http://team.example:4655", &store)
                .unwrap(),
            None
        );
        assert_eq!(store.get(server_keys::KEY_SERVER_KEYS_MAP).unwrap(), None);

        let on_disk = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            on_disk.contains("server_key"),
            "the file must be left exactly as the user wrote it, got:\n{on_disk}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn loads_without_any_server_config() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        let store = MemoryStore::default();
        let cfg = load_hermetic_with(&config_path, &store).unwrap();
        assert_eq!(cfg.server_url, None);
        assert_eq!(cloud_bearer(&cfg, &store), None);
        assert_eq!(cfg.project_id, None);
    }

    #[test]
    fn validate_fails_when_server_url_set_without_project_id() {
        let cfg = Config {
            server_url: Some("http://example.com".to_string()),
            ..Default::default()
        };
        let result = cfg.validate();
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("server_url"));
        assert!(msg.contains("project_id"));
    }

    #[test]
    fn validate_passes_when_both_server_url_and_project_id_set() {
        let cfg = Config {
            server_url: Some("http://example.com".to_string()),
            project_id: Some("my-proj".to_string()),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_passes_when_neither_server_url_nor_project_id_set() {
        let cfg = Config::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_passes_when_only_project_id_set() {
        let cfg = Config {
            project_id: Some("my-proj".to_string()),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_passes_for_loopback_url_without_project_id() {
        for url in &[
            "http://127.0.0.1:4655",
            "http://localhost:4655",
            "http://127.0.0.1:7778/",
        ] {
            let cfg = Config {
                server_url: Some(url.to_string()),
                project_id: None,
                ..Default::default()
            };
            assert!(
                cfg.validate().is_ok(),
                "expected validate() to pass for loopback URL {url}"
            );
        }
    }

    #[test]
    fn validate_fails_for_non_loopback_url_without_project_id() {
        let cfg = Config {
            server_url: Some("http://inkentry.internal:4655".to_string()),
            project_id: None,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn portless_loopback_server_url_warning_fires_for_bare_localhost() {
        let warning = portless_loopback_server_url_warning("http://localhost")
            .expect("a portless loopback server_url must produce a warning");
        assert!(warning.contains("http://localhost"), "got: {warning}");
        assert!(warning.contains("no port"), "got: {warning}");
    }

    #[test]
    fn portless_loopback_server_url_warning_is_none_when_port_present() {
        assert_eq!(
            portless_loopback_server_url_warning("http://localhost:4655"),
            None
        );
        assert_eq!(
            portless_loopback_server_url_warning("http://127.0.0.1:4655"),
            None
        );
    }

    #[test]
    fn portless_loopback_server_url_warning_is_none_for_non_loopback_host() {
        // A non-loopback https:// URL with no explicit port is normal
        // (default port 443), not a misconfiguration signal.
        assert_eq!(
            portless_loopback_server_url_warning("https://team.example.com"),
            None
        );
    }

    #[test]
    fn validate_still_passes_for_a_portless_loopback_server_url() {
        // Advisory, not a hard error: a portless loopback server_url is still
        // a valid transport.
        let cfg = Config {
            server_url: Some("http://localhost".to_string()),
            project_id: None,
            ..Default::default()
        };
        assert!(
            cfg.validate().is_ok(),
            "a portless loopback server_url must warn, not fail validate()"
        );
    }

    #[test]
    fn validate_with_project_true_passes_non_loopback_without_project_id() {
        // First-run `inkentry sync --project <slug>`: no project_id is
        // persisted, but the caller asserts a slug is available from --project.
        let cfg = Config {
            server_url: Some("http://inkentry.internal:4655".to_string()),
            project_id: None,
            ..Default::default()
        };
        assert!(cfg.validate_with_project(true).is_ok());
    }

    #[test]
    fn validate_with_project_false_still_fails_non_loopback_without_project_id() {
        let cfg = Config {
            server_url: Some("http://inkentry.internal:4655".to_string()),
            project_id: None,
            ..Default::default()
        };
        assert!(cfg.validate_with_project(false).is_err());
    }

    #[test]
    fn validate_delegates_to_validate_with_project() {
        // validate() == validate_with_project(project_id.is_some()).
        let with_id = Config {
            server_url: Some("http://inkentry.internal:4655".to_string()),
            project_id: Some("p".to_string()),
            ..Default::default()
        };
        assert!(with_id.validate().is_ok());
        let without_id = Config {
            server_url: Some("http://inkentry.internal:4655".to_string()),
            project_id: None,
            ..Default::default()
        };
        assert!(without_id.validate().is_err());
    }

    #[test]
    fn resolve_project_id_returns_set_value_when_present() {
        let tmp = TempDir::new().unwrap();
        let cfg = Config {
            project_id: Some("acme/my-app".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_project_id(tmp.path()), "acme/my-app");
    }

    #[test]
    fn resolve_project_id_derives_when_unset() {
        let tmp = TempDir::new().unwrap();
        let cfg = Config::default();
        let id = cfg.resolve_project_id(tmp.path());
        // The local/ fallback, since tmp dir is not a git repo.
        assert!(id.starts_with("local/"), "got {id}");
    }

    #[test]
    fn resolve_inference_url_prefers_inference_url() {
        // Auto-discovered case: inference_url set, server_url unset.
        let cfg = Config {
            inference_url: Some("http://127.0.0.1:4655".to_string()),
            server_url: None,
            ..Default::default()
        };
        assert_eq!(cfg.resolve_inference_url(), Some("http://127.0.0.1:4655"));
    }

    #[test]
    #[serial_test::serial]
    fn resolve_inference_url_falls_back_to_server_url_in_cloud_first() {
        // Explicit team/cloud server in cloud_first mode: only server_url set;
        // it serves inference too, the only mode where that happens.
        clear_inkentry_env();
        let cfg = Config {
            inference_url: None,
            server_url: Some("http://team.example.com:4655".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_inference_url(),
            Some("http://team.example.com:4655")
        );
    }

    #[test]
    #[serial_test::serial]
    fn resolve_inference_url_local_first_never_falls_back_to_server_url() {
        // local_first (the default once server_url is set) must never fall
        // back to server_url for inference; an explicit server_url there is a
        // sync replica only.
        clear_inkentry_env();
        let cfg = Config {
            inference_url: None,
            server_url: Some("https://api.inkentry.com".to_string()),
            mode: None, // defaults to local_first because server_url is set
            ..Default::default()
        };
        assert_eq!(cfg.resolve_mode(), SyncMode::LocalFirst);
        assert_eq!(cfg.resolve_inference_url(), None);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_inference_url_offline_never_falls_back_to_server_url() {
        clear_inkentry_env();
        let cfg = Config {
            inference_url: None,
            server_url: Some("https://api.inkentry.com".to_string()),
            mode: Some(SyncMode::Offline),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_inference_url(), None);
    }

    #[test]
    fn resolve_inference_url_none_when_neither_set() {
        let cfg = Config::default();
        assert_eq!(cfg.resolve_inference_url(), None);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_inference_url_inference_url_wins_over_server_url() {
        // Defensive: if both are set, inference_url wins; exercised in
        // cloud_first, the one mode where server_url would otherwise be a
        // candidate too.
        clear_inkentry_env();
        let cfg = Config {
            inference_url: Some("http://127.0.0.1:4655".to_string()),
            server_url: Some("http://team.example.com:4655".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_inference_url(), Some("http://127.0.0.1:4655"));
    }

    #[test]
    #[serial_test::serial]
    fn global_personal_config_cannot_set_server_url() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"server_url = "http://personal.example.com:4655"
"#,
        )
        .unwrap();

        let cfg = load_hermetic(&config_path).unwrap();
        assert_eq!(cfg.server_url, None);
    }

    #[test]
    #[serial_test::serial]
    fn env_inkentry_server_url_overrides_config() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"server_url = "http://config.example.com:4655"
"#,
        )
        .unwrap();

        unsafe {
            std::env::set_var("INKENTRY_SERVER_URL", "http://env.example.com:4655");
        }
        let cfg = load_hermetic(&config_path).unwrap();
        assert_eq!(
            cfg.server_url,
            Some("http://env.example.com:4655".to_string())
        );
    }

    #[test]
    #[serial_test::serial]
    fn env_inkentry_server_key_resolves_as_the_bearer_over_an_unread_file_key() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"server_key = "config-token"
"#,
        )
        .unwrap();

        unsafe {
            std::env::set_var("INKENTRY_SERVER_KEY", "env-token");
        }
        let store = MemoryStore::default();
        let cfg = load_hermetic_with(&config_path, &store).unwrap();
        assert_eq!(cloud_bearer(&cfg, &store).as_deref(), Some("env-token"));
        unsafe {
            std::env::remove_var("INKENTRY_SERVER_KEY");
        }
    }

    #[test]
    #[serial_test::serial]
    fn env_inkentry_project_id_overrides_config() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"project_id = "config-proj"
"#,
        )
        .unwrap();

        unsafe {
            std::env::set_var("INKENTRY_PROJECT_ID", "env-proj");
        }
        let cfg = load_hermetic(&config_path).unwrap();
        assert_eq!(cfg.project_id, Some("env-proj".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn env_inkentry_memory_server_url_is_ignored() {
        // The removed INKENTRY_MEMORY_SERVER_URL alias must not populate
        // server_url. Not in clear_inkentry_env's unset list since nothing
        // reads it; cleared here instead.
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        unsafe {
            std::env::set_var("INKENTRY_MEMORY_SERVER_URL", "http://old.example.com:4655");
        }
        let cfg = load_hermetic(&config_path).unwrap();
        unsafe {
            std::env::remove_var("INKENTRY_MEMORY_SERVER_URL");
        }
        assert_eq!(cfg.server_url, None);
    }

    #[test]
    #[serial_test::serial]
    fn project_level_config_merges_server_url() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let proj_dir = tmp.path().join("project");
        let inkentry_dir = proj_dir.join(".inkentry");
        std::fs::create_dir_all(&inkentry_dir).unwrap();
        std::fs::write(
            inkentry_dir.join("config.toml"),
            r#"server_url = "http://proj.example.com:4655"
project_id = "team/proj"
"#,
        )
        .unwrap();

        let global_config = tmp.path().join("global.toml");
        std::fs::write(&global_config, "").unwrap();

        let cfg = Config::load_with_store_from(
            Some(&global_config),
            &MemoryStore::default(),
            Some(&proj_dir),
        )
        .unwrap();
        assert_eq!(
            cfg.server_url,
            Some("http://proj.example.com:4655".to_string())
        );
        assert_eq!(cfg.project_id, Some("team/proj".to_string()));
    }

    // Write a personal config and a project `.inkentry/config.toml`, then load
    // with project discovery anchored at the project root. `None` writes no
    // project file at all — not the same as an empty one.
    fn load_layered(personal: &str, project: Option<&str>) -> (TempDir, Result<Config>) {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("global.toml");
        std::fs::write(&global, personal).unwrap();

        let proj_dir = tmp.path().join("project");
        std::fs::create_dir_all(&proj_dir).unwrap();
        if let Some(body) = project {
            let inkentry_dir = proj_dir.join(".inkentry");
            std::fs::create_dir_all(&inkentry_dir).unwrap();
            std::fs::write(inkentry_dir.join("config.toml"), body).unwrap();
        }

        let cfg =
            Config::load_with_store_from(Some(&global), &MemoryStore::default(), Some(&proj_dir));
        (tmp, cfg)
    }

    #[test]
    #[serial_test::serial]
    fn project_config_mode_takes_effect() {
        clear_inkentry_env();
        let (_tmp, cfg) = load_layered(
            "",
            Some(
                r#"server_url = "http://127.0.0.1:7788"
project_id = "github.com/sass/sass"
mode = "cloud_first"
"#,
            ),
        );
        let cfg = cfg.unwrap();
        assert_eq!(cfg.mode, Some(SyncMode::CloudFirst));
        assert_eq!(cfg.resolve_mode(), SyncMode::CloudFirst);
    }

    #[test]
    #[serial_test::serial]
    fn org_pin_loads_from_project_config() {
        clear_inkentry_env();
        let (_tmp, cfg) = load_layered("", Some("project_id = \"p\"\norg = \"acme\"\n"));
        assert_eq!(cfg.unwrap().org.as_deref(), Some("acme"));
    }

    #[test]
    #[serial_test::serial]
    fn env_inkentry_org_overrides_project_org() {
        clear_inkentry_env();
        unsafe { std::env::set_var("INKENTRY_ORG", "from-env") };
        let (_tmp, cfg) = load_layered("", Some("org = \"from-project\"\n"));
        unsafe { std::env::remove_var("INKENTRY_ORG") };
        assert_eq!(cfg.unwrap().org.as_deref(), Some("from-env"));
    }

    // A pinned org resolves over the cache's `active` pointer; with no pin,
    // active is used.
    #[test]
    #[serial_test::serial]
    fn bearer_resolves_the_pinned_org_over_active() {
        clear_inkentry_env();
        let store = MemoryStore::default();
        let cloud_origin = server_keys::normalize_origin(server_keys::DEFAULT_CLOUD_URL).unwrap();
        let a = AuthTokens {
            access_token: "at-a".into(),
            refresh_token: "rt-a".into(),
            expires_at: 4_000_000_000,
            org_id: "org_a".into(),
            cloud_origin: cloud_origin.clone(),
        };
        let b = AuthTokens {
            access_token: "at-b".into(),
            org_id: "org_b".into(),
            ..a.clone()
        };
        org_tokens::set_active(&store, &a, None).unwrap();
        org_tokens::set_active(&store, &b, None).unwrap(); // active is now org_b

        // No pin → the active org's token.
        let cfg = Config::default();
        assert_eq!(cloud_bearer(&cfg, &store).as_deref(), Some("at-b"));

        // A pin selects that org over the active pointer.
        let cfg = Config {
            org: Some("org_a".into()),
            ..Default::default()
        };
        assert_eq!(cloud_bearer(&cfg, &store).as_deref(), Some("at-a"));
    }

    #[test]
    #[serial_test::serial]
    fn mode_precedence_across_personal_project_and_env() {
        // Every combination of the three sources, as one table so a change to
        // any single layer can't pass by agreeing with the others. The
        // project column is three-valued: `None` is no file, `Some("")` a
        // file that sets no `mode` — proving a project config's mere
        // presence doesn't blank a personal value.
        const OFF: Option<SyncMode> = Some(SyncMode::Offline);
        const LOCAL: Option<SyncMode> = Some(SyncMode::LocalFirst);
        const CLOUD: Option<SyncMode> = Some(SyncMode::CloudFirst);
        type Row = (
            Option<&'static str>,
            Option<&'static str>,
            Option<&'static str>,
            Option<SyncMode>,
        );

        // personal, project, INKENTRY_MODE, pinned mode
        let rows: &[Row] = &[
            (None, None, None, None),
            (None, Some(""), None, None),
            (None, Some("cloud_first"), None, CLOUD),
            (Some("offline"), None, None, OFF),
            (Some("offline"), Some(""), None, OFF),
            (Some("offline"), Some("cloud_first"), None, CLOUD),
            (None, None, Some("local_first"), LOCAL),
            (None, Some(""), Some("local_first"), LOCAL),
            (None, Some("cloud_first"), Some("local_first"), LOCAL),
            (Some("offline"), None, Some("local_first"), LOCAL),
            (Some("offline"), Some(""), Some("local_first"), LOCAL),
            (
                Some("offline"),
                Some("cloud_first"),
                Some("local_first"),
                LOCAL,
            ),
        ];

        for (personal, project, env, expected) in rows {
            clear_inkentry_env();
            if let Some(v) = env {
                unsafe { std::env::set_var("INKENTRY_MODE", v) };
            }
            let mode_line = |m: &str| {
                if m.is_empty() {
                    String::new()
                } else {
                    format!("mode = \"{m}\"\n")
                }
            };
            let personal_body = personal.map(mode_line).unwrap_or_default();
            let project_body =
                project.map(|m| format!("project_id = \"team/proj\"\n{}", mode_line(m)));
            let (_tmp, cfg) = load_layered(&personal_body, project_body.as_deref());
            assert_eq!(
                cfg.unwrap().mode,
                *expected,
                "personal={personal:?} project={project:?} env={env:?}"
            );
        }
        clear_inkentry_env();
    }

    #[test]
    #[serial_test::serial]
    fn no_server_env_forces_offline_over_a_project_config_mode() {
        clear_inkentry_env();
        let (_tmp, cfg) = load_layered(
            "",
            Some(
                "server_url = \"http://team.example:4655\"\nproject_id = \"team/proj\"\nmode = \"cloud_first\"\n",
            ),
        );
        let cfg = cfg.unwrap();
        unsafe { std::env::set_var("INKENTRY_NO_SERVER", "1") };
        let resolved = cfg.resolve_mode();
        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };
        assert_eq!(resolved, SyncMode::Offline);
    }

    fn project_warnings(raw: &str) -> Vec<String> {
        project_config_key_warnings(raw, Path::new("/repo/.inkentry/config.toml"))
    }

    #[test]
    fn a_key_the_project_config_does_not_read_is_named_in_a_warning() {
        let warnings =
            project_warnings("server_url = \"https://team.example\"\ndb_path = \"/tmp/x\"\n");
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        let warning = &warnings[0];
        assert!(warning.contains("db_path"), "must name the key: {warning}");
        assert!(
            warning.contains("/repo/.inkentry/config.toml"),
            "must name the file: {warning}"
        );
    }

    #[test]
    fn keys_the_project_config_reads_produce_no_warning() {
        assert!(
            project_warnings(
                "server_url = \"https://team.example\"\nproject_id = \"p\"\nserver_ca = \"/ca.pem\"\nmode = \"cloud_first\"\n\n[index]\nexclude = [\"x/**\"]\n"
            )
            .is_empty()
        );
    }

    #[test]
    fn a_credential_in_the_project_config_is_named_so_it_can_be_rotated() {
        // The file is committed, so the value is already in the repo's
        // history; dropping it silently would leave nobody aware there's
        // something to rotate.
        let warnings = project_warnings(
            "server_url = \"https://team.example\"\nserver_key = \"team-shared-key\"\n",
        )
        .join("\n");
        assert!(
            warnings.contains("server_key"),
            "`server_key` must be named, got: {warnings}"
        );
        assert!(
            warnings.contains("rotate"),
            "`server_key` must say to rotate it, got: {warnings}"
        );
    }

    fn personal_warning(raw: &str) -> Option<String> {
        personal_config_credential_warning(raw, Path::new("/home/dev/.config/inkentry/config.toml"))
    }

    #[test]
    fn a_server_key_in_the_personal_config_is_named_so_it_can_be_rotated() {
        let warning = personal_warning("server_key = \"sk-legacy\"\nllm_model = \"gpt-oss\"\n")
            .expect("a `server_key` line must be named");
        assert!(
            warning.contains("server_key"),
            "must name the key: {warning}"
        );
        assert!(
            warning.contains("/home/dev/.config/inkentry/config.toml"),
            "must name the file: {warning}"
        );
        assert!(
            warning.to_lowercase().contains("rotate"),
            "must say to rotate: {warning}"
        );
        assert!(
            warning.contains("inkentry auth set-key"),
            "must name the replacement command: {warning}"
        );
    }

    #[test]
    fn a_personal_config_without_a_server_key_warns_about_nothing() {
        assert!(personal_warning("llm_model = \"gpt-oss\"\n").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn an_unread_project_key_does_not_fail_the_load() {
        clear_inkentry_env();
        let (_tmp, cfg) = load_layered(
            "",
            Some(
                "server_url = \"https://team.example\"\nproject_id = \"team/proj\"\nnot_a_key = 1\n",
            ),
        );
        let cfg = cfg.expect("an unread key must warn, never fail the load");
        assert_eq!(cfg.server_url, Some("https://team.example".to_string()));
        assert_eq!(cfg.project_id, Some("team/proj".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn cloud_flag_targets_the_fixed_cloud_url_and_defaults_to_local_first() {
        clear_inkentry_env();
        let (_tmp, cfg) = load_layered("", Some("cloud = true\nproject_id = \"team/proj\"\n"));
        let cfg = cfg.expect("`cloud = true` must load");
        let target = server_keys::cloud_url();
        assert_eq!(cfg.server_url.as_deref(), Some(target.as_str()));
        assert_eq!(cfg.mode, None);
        assert_eq!(cfg.resolve_mode(), SyncMode::LocalFirst);
    }

    #[test]
    #[serial_test::serial]
    fn cloud_flag_keeps_an_explicit_cloud_first_mode() {
        clear_inkentry_env();
        unsafe { std::env::set_var("INKENTRY_MODE", "cloud_first") };
        let (_tmp, cfg) = load_layered("", Some("cloud = true\nproject_id = \"team/proj\"\n"));
        unsafe { std::env::remove_var("INKENTRY_MODE") };
        let cfg = cfg.expect("`cloud = true` must load");
        assert_eq!(cfg.resolve_mode(), SyncMode::CloudFirst);
    }

    #[test]
    #[serial_test::serial]
    fn cloud_flag_with_server_url_is_a_load_error() {
        clear_inkentry_env();
        let (_tmp, cfg) = load_layered(
            "",
            Some("cloud = true\nserver_url = \"https://team.example\"\nproject_id = \"p\"\n"),
        );
        let err = cfg.expect_err("`cloud = true` with a server_url must fail the load");
        assert!(err.to_string().contains("cloud"), "{err}");
    }

    #[test]
    #[serial_test::serial]
    fn cloud_flag_honours_the_development_cloud_url_override() {
        clear_inkentry_env();
        unsafe { std::env::set_var("INKENTRY_CLOUD_URL", "https://dev.example") };
        let (_tmp, cfg) = load_layered("", Some("cloud = true\nproject_id = \"p\"\n"));
        let target = cfg.map(|c| c.server_url);
        unsafe { std::env::remove_var("INKENTRY_CLOUD_URL") };
        assert_eq!(target.unwrap().as_deref(), Some("https://dev.example"));
    }

    #[test]
    #[serial_test::serial]
    fn a_personal_cloud_opt_in_is_discarded() {
        clear_inkentry_env();
        let (_tmp, cfg) = load_layered("cloud = true\n", None);
        let cfg = cfg.expect("a personal cloud opt-in is ignored, not an error");
        assert!(!cfg.cloud);
        assert_eq!(cfg.server_url, None);
    }

    #[test]
    #[serial_test::serial]
    fn invalid_project_config_mode_names_the_value_and_the_file() {
        clear_inkentry_env();
        let (_tmp, cfg) = load_layered("", Some("mode = \"sideways\"\n"));
        let err = cfg.unwrap_err().to_string();
        assert!(err.contains("sideways"), "got: {err}");
        assert!(err.contains(".inkentry"), "got: {err}");
        assert!(err.contains(SyncMode::valid_values()), "got: {err}");
    }

    fn sample_tokens() -> AuthTokens {
        AuthTokens {
            access_token: "at-sample".to_string(),
            refresh_token: "rt-sample".to_string(),
            expires_at: 4_000_000_000,
            org_id: "org_sample".to_string(),
            cloud_origin: server_keys::normalize_origin(server_keys::DEFAULT_CLOUD_URL).unwrap(),
        }
    }

    // Seed a legacy plaintext [auth] table — the shape the migration path
    // lifts out.
    fn write_legacy_auth(path: &Path, tokens: &AuthTokens) {
        let mut doc = if path.exists() {
            std::fs::read_to_string(path)
                .unwrap()
                .parse::<toml::Table>()
                .unwrap()
        } else {
            toml::Table::new()
        };
        doc.insert("auth".to_string(), toml::Value::try_from(tokens).unwrap());
        std::fs::write(path, toml::to_string_pretty(&doc).unwrap()).unwrap();
    }

    fn file_has_auth_table(path: &Path) -> bool {
        std::fs::read_to_string(path)
            .unwrap()
            .parse::<toml::Table>()
            .unwrap()
            .contains_key("auth")
    }

    #[test]
    #[serial_test::serial]
    fn legacy_auth_is_migrated_into_the_cache_and_stripped_from_disk() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_legacy_auth(&path, &sample_tokens());

        let store = MemoryStore::default();
        let cfg = load_hermetic_with(&path, &store).unwrap();

        assert_eq!(cloud_bearer(&cfg, &store).as_deref(), Some("at-sample"));
        let session = org_tokens::resolve_session(&store, None).unwrap().unwrap();
        assert_eq!(session.refresh_token, "rt-sample");
        assert_eq!(session.org_id, "org_sample");
        assert!(
            !file_has_auth_table(&path),
            "migration must strip the [auth] table from disk"
        );
    }

    // Hand-editing the config is documented, and a login without an org
    // leaves org_id empty, so a trimmed [auth] table must migrate rather than
    // brick the load.

    #[test]
    #[serial_test::serial]
    fn migrating_a_table_without_org_id_leaves_the_session_unscoped() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[auth]\n\
             access_token = \"at\"\n\
             refresh_token = \"rt\"\n\
             expires_at = 4000000000\n\
             cloud_origin = \"https://api.inkentry.com\"\n",
        )
        .unwrap();

        let store = MemoryStore::default();
        let cfg = load_hermetic_with(&path, &store)
            .expect("a [auth] table without org_id must still migrate");
        assert_eq!(cloud_bearer(&cfg, &store).as_deref(), Some("at"));
        let session = org_tokens::resolve_session(&store, None).unwrap().unwrap();
        assert_eq!(
            session.org_id, "",
            "missing org_id is treated as no scoping"
        );
    }

    #[test]
    #[serial_test::serial]
    fn migrating_a_table_without_expires_at_treats_the_session_as_expired() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[auth]\n\
             access_token = \"at\"\n\
             refresh_token = \"rt\"\n\
             org_id = \"org_x\"\n\
             cloud_origin = \"https://api.inkentry.com\"\n",
        )
        .unwrap();

        let store = MemoryStore::default();
        load_hermetic_with(&path, &store)
            .expect("a [auth] table without expires_at must still migrate");
        let session = org_tokens::resolve_session(&store, None).unwrap().unwrap();
        assert!(
            session.is_expired_at(1),
            "a token with no expiry must be treated as expired"
        );
    }

    #[test]
    #[serial_test::serial]
    fn migrating_a_table_without_access_token_means_not_logged_in() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[auth]\n\
             refresh_token = \"rt\"\n\
             org_id = \"org_x\"\n",
        )
        .unwrap();

        let store = MemoryStore::default();
        let cfg = load_hermetic_with(&path, &store)
            .expect("a [auth] table without access_token must migrate");
        assert_eq!(
            cloud_bearer(&cfg, &store),
            None,
            "a missing/empty access token must resolve to no bearer"
        );
    }

    // A bare [auth] header (every field trimmed away) must still load rather
    // than error, and the empty table is stripped.
    #[test]
    #[serial_test::serial]
    fn bare_auth_header_does_not_brick_load() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "llm_model = \"gpt-oss\"\n[auth]\n").unwrap();

        let store = MemoryStore::default();
        let cfg = load_hermetic_with(&path, &store)
            .expect("a bare [auth] header must not brick the load");
        assert_eq!(cfg.llm_model.as_deref(), Some("gpt-oss"));
        assert_eq!(cloud_bearer(&cfg, &store), None);
        assert!(
            !file_has_auth_table(&path),
            "the empty table is stripped too"
        );
    }

    // An unrecognised `mode` names the bad value and lists the valid modes and
    // the file, mirroring the INKENTRY_MODE env-var message.
    #[test]
    #[serial_test::serial]
    fn invalid_mode_value_error_names_value_modes_and_file() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "mode = \"bogus_mode\"\n").unwrap();

        let err = load_hermetic(&path).unwrap_err().to_string();
        assert!(err.contains("bogus_mode"), "must name the bad value: {err}");
        assert!(err.contains("offline"), "must list valid modes: {err}");
        assert!(err.contains("local_first"), "must list valid modes: {err}");
        assert!(err.contains("cloud_first"), "must list valid modes: {err}");
        assert!(
            err.contains(&path.display().to_string()),
            "must name the config file: {err}"
        );
    }

    // A genuinely malformed config (a type error here) produces a message that
    // names the file and points at a remedy, not a bare "parsing config.toml".
    #[test]
    #[serial_test::serial]
    fn malformed_config_error_names_file_and_remedy() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        // `llm_context_length` is a usize; a string is a hard type error.
        std::fs::write(&path, "llm_context_length = \"lots\"\n").unwrap();

        let err = load_hermetic(&path).unwrap_err().to_string();
        assert!(
            err.contains(&path.display().to_string()),
            "must name the config file: {err}"
        );
        assert!(
            err.contains("remove") || err.contains("Fix"),
            "must point at a remedy: {err}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn env_server_key_wins_over_cloud_session() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_legacy_auth(&path, &sample_tokens());

        unsafe { std::env::set_var("INKENTRY_SERVER_KEY", "ci-token") };
        let store = MemoryStore::default();
        let cfg = load_hermetic_with(&path, &store).unwrap();
        assert_eq!(cloud_bearer(&cfg, &store).as_deref(), Some("ci-token"));
        unsafe { std::env::remove_var("INKENTRY_SERVER_KEY") };
        // The migrated refresh token is still available for the refresh path.
        assert_eq!(
            org_tokens::resolve_session(&store, None)
                .unwrap()
                .unwrap()
                .refresh_token,
            "rt-sample"
        );
    }

    // The cloud session and a stored per-origin key resolve independently by
    // target origin: they don't compete in one flat precedence chain.
    #[test]
    #[serial_test::serial]
    fn cloud_session_and_per_origin_key_resolve_by_kind_not_precedence() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_legacy_auth(&path, &sample_tokens());

        let store = MemoryStore::default();
        server_keys::set_key_for_origin("https://team.example:4655", "sk-team", &store).unwrap();
        let cfg = Config::load_with_store_from(Some(&path), &store, None).unwrap();

        assert_eq!(cloud_bearer(&cfg, &store).as_deref(), Some("at-sample"));
        assert_eq!(
            cfg.bearer_for_with_store("https://team.example:4655", &store)
                .unwrap()
                .as_deref(),
            Some("sk-team")
        );
    }

    // Migration preserves other top-level keys (e.g. `llm_model`) while lifting
    // the session out and stripping the table.
    #[test]
    #[serial_test::serial]
    fn migration_preserves_other_keys() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "llm_model = \"gpt-oss\"\n").unwrap();
        write_legacy_auth(&path, &sample_tokens());

        let store = MemoryStore::default();
        let cfg = load_hermetic_with(&path, &store).unwrap();
        assert_eq!(cfg.llm_model.as_deref(), Some("gpt-oss"));
        assert_eq!(
            org_tokens::resolve_session(&store, None)
                .unwrap()
                .unwrap()
                .access_token,
            "at-sample"
        );
        assert!(!file_has_auth_table(&path));
    }

    // `remove_auth_tokens_from` clears the `[auth]` table and nothing else: the
    // per-origin server keys are a separate store with its own command.
    #[test]
    #[serial_test::serial]
    fn remove_auth_tokens_clears_only_auth_table() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "llm_model = \"gpt-oss\"\n").unwrap();
        write_legacy_auth(&path, &sample_tokens());

        let store = MemoryStore::default();
        server_keys::set_key_for_origin("https://team.example:4655", "sk-team", &store).unwrap();

        remove_auth_tokens_from(&path).unwrap();
        // With the table already stripped there is nothing to migrate, so the
        // cloud kind resolves to nothing while the server key is untouched.
        let cfg = Config::load_with_store_from(Some(&path), &store, None).unwrap();
        assert!(!file_has_auth_table(&path));
        assert_eq!(cfg.llm_model.as_deref(), Some("gpt-oss"));
        assert_eq!(cloud_bearer(&cfg, &store), None);
        assert_eq!(
            cfg.bearer_for_with_store("https://team.example:4655", &store)
                .unwrap()
                .as_deref(),
            Some("sk-team")
        );
    }

    #[test]
    fn auth_tokens_expiry_with_skew() {
        let t = sample_tokens(); // expires_at = 4_000_000_000
        assert!(!t.is_expired_at(4_000_000_000 - 31));
        assert!(t.is_expired_at(4_000_000_000 - 30));
        assert!(t.is_expired_at(4_000_000_000 + 100));
    }

    #[test]
    #[serial_test::serial]
    fn project_level_config_ignores_deprecated_memory_server_url() {
        // The removed alias no longer resolves in project config.
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let proj_dir = tmp.path().join("project");
        let inkentry_dir = proj_dir.join(".inkentry");
        std::fs::create_dir_all(&inkentry_dir).unwrap();
        std::fs::write(
            inkentry_dir.join("config.toml"),
            r#"memory_server_url = "http://old.example.com:4655"
project_id = "team/old"
"#,
        )
        .unwrap();

        let global_config = tmp.path().join("global.toml");
        std::fs::write(&global_config, "").unwrap();

        let cfg = Config::load_with_store_from(
            Some(&global_config),
            &MemoryStore::default(),
            Some(&proj_dir),
        )
        .unwrap();
        assert_eq!(cfg.server_url, None);
        assert_eq!(cfg.project_id, Some("team/old".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn project_level_config_live_key_wins_over_deprecated() {
        // Mixed project config: the live server_url resolves and the removed
        // alias is dropped (no error, no override).
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let proj_dir = tmp.path().join("project");
        let inkentry_dir = proj_dir.join(".inkentry");
        std::fs::create_dir_all(&inkentry_dir).unwrap();
        std::fs::write(
            inkentry_dir.join("config.toml"),
            r#"server_url = "http://new.example.com:4655"
memory_server_url = "http://old.example.com:4655"
project_id = "team/new"
"#,
        )
        .unwrap();

        let global_config = tmp.path().join("global.toml");
        std::fs::write(&global_config, "").unwrap();

        let cfg = Config::load_with_store_from(
            Some(&global_config),
            &MemoryStore::default(),
            Some(&proj_dir),
        )
        .unwrap();
        assert_eq!(
            cfg.server_url,
            Some("http://new.example.com:4655".to_string())
        );
        assert_eq!(cfg.project_id, Some("team/new".to_string()));
    }

    // Exercised through an injected MemoryStore, so no real keychain or Secret
    // Service daemon is required.

    // A credential stored via `auth set-key` lands only in the secret store,
    // never in config.toml.
    #[test]
    #[serial_test::serial]
    fn stored_credential_is_in_store_not_in_config_file() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "server_url = \"http://team:4655\"\nproject_id = \"p\"\n",
        )
        .unwrap();

        let store = MemoryStore::default();
        server_keys::set_key_for_origin("http://team:4655", "sk-ink-new", &store).unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("sk-ink-new"));
        assert!(!on_disk.contains("server_key"));

        let cfg = Config::load_with_store(Some(&path), &store).unwrap();
        assert_eq!(
            cfg.bearer_for_with_store("http://team:4655", &store)
                .unwrap()
                .as_deref(),
            Some("sk-ink-new")
        );
    }

    #[test]
    #[serial_test::serial]
    fn env_server_key_wins_over_store() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "").unwrap();

        let store = MemoryStore::default();
        server_keys::set_key_for_origin("https://team.example:4655", "sk-in-store", &store)
            .unwrap();

        unsafe { std::env::set_var("INKENTRY_SERVER_KEY", "sk-from-env") };
        let cfg = Config::load_with_store(Some(&path), &store).unwrap();
        assert_eq!(
            cfg.bearer_for_with_store("https://team.example:4655", &store)
                .unwrap()
                .as_deref(),
            Some("sk-from-env")
        );
        unsafe { std::env::remove_var("INKENTRY_SERVER_KEY") };
    }

    // A `server_key` line in the checked-in project config is read for
    // nothing at any tier, and the file is left exactly as it is.
    #[test]
    #[serial_test::serial]
    fn project_config_server_key_field_is_read_for_nothing() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let proj_dir = tmp.path().join("project");
        let inkentry_dir = proj_dir.join(".inkentry");
        std::fs::create_dir_all(&inkentry_dir).unwrap();
        let proj_cfg = inkentry_dir.join("config.toml");
        std::fs::write(
            &proj_cfg,
            "server_url = \"https://team.example:4655\"\nserver_key = \"team-shared-key\"\nproject_id = \"team/proj\"\n",
        )
        .unwrap();

        let global_config = tmp.path().join("global.toml");
        std::fs::write(&global_config, "").unwrap();

        let store = MemoryStore::default();
        let original_cwd = std::env::current_dir().ok();
        std::env::set_current_dir(&proj_dir).unwrap();
        let cfg = Config::load_with_store(Some(&global_config), &store).unwrap();
        if let Some(d) = original_cwd {
            std::env::set_current_dir(d).unwrap();
        }

        // The file's other fields still load fine (no parse error from the
        // now-unrecognized `server_key` key).
        assert_eq!(
            cfg.server_url,
            Some("https://team.example:4655".to_string())
        );
        assert_eq!(cfg.project_id, Some("team/proj".to_string()));
        // No credential resolves anywhere: not for the cloud, not per-origin.
        assert_eq!(cloud_bearer(&cfg, &store), None);
        assert_eq!(
            cfg.bearer_for_with_store("https://team.example:4655", &store)
                .unwrap(),
            None
        );
        // Never touches the personal secret store.
        assert_eq!(store.get(server_keys::KEY_SERVER_KEYS_MAP).unwrap(), None);
        // The checked-in file itself is left untouched.
        assert!(
            std::fs::read_to_string(&proj_cfg)
                .unwrap()
                .contains("server_key")
        );
    }

    // The file-backed store stands in for a keychain on a headless host;
    // `default_store` falls back to it the same way.
    #[test]
    #[serial_test::serial]
    fn file_store_fallback_resolves_credential_like_keychain() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "").unwrap();

        let file_store = secret_store::FileStore::new(tmp.path().join("secrets.toml"));
        server_keys::set_key_for_origin("https://team.example:4655", "sk-headless", &file_store)
            .unwrap();

        let cfg = Config::load_with_store(Some(&path), &file_store).unwrap();
        assert_eq!(
            cfg.bearer_for_with_store("https://team.example:4655", &file_store)
                .unwrap()
                .as_deref(),
            Some("sk-headless")
        );
    }

    #[test]
    #[serial_test::serial]
    fn no_credential_anywhere_yields_none_without_error() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        let store = MemoryStore::default();
        let cfg = Config::load_with_store(Some(&path), &store).unwrap();
        assert_eq!(cloud_bearer(&cfg, &store), None);
        assert_eq!(
            cfg.bearer_for_with_store("https://team.example:4655", &store)
                .unwrap(),
            None
        );
    }

    // Counts `get` calls per key, wrapping a MemoryStore. Used to assert that
    // a higher-precedence credential never costs a keychain round-trip.
    #[derive(Default)]
    struct CountingStore {
        inner: MemoryStore,
        get_calls: std::sync::atomic::AtomicUsize,
    }

    impl SecretStore for CountingStore {
        fn get(&self, key: &str) -> Result<Option<String>> {
            self.get_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.get(key)
        }

        fn set(&self, key: &str, value: &str) -> Result<()> {
            self.inner.set(key, value)
        }

        fn delete(&self, key: &str) -> Result<()> {
            self.inner.delete(key)
        }

        fn kind(&self) -> &'static str {
            "counting-test-double"
        }
    }

    // The store must never be asked for server_key at all, not just
    // overridden after the fact.
    #[test]
    #[serial_test::serial]
    fn env_server_key_skips_store_read_entirely() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "").unwrap();

        let store = CountingStore::default();
        unsafe { std::env::set_var("INKENTRY_SERVER_KEY", "sk-from-env") };
        let cfg = Config::load_with_store(Some(&path), &store).unwrap();
        let bearer = cfg
            .bearer_for_with_store(server_keys::DEFAULT_CLOUD_URL, &store)
            .unwrap();
        unsafe { std::env::remove_var("INKENTRY_SERVER_KEY") };

        assert_eq!(bearer.as_deref(), Some("sk-from-env"));
        assert_eq!(
            store.get_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the personal secret store must not be read when INKENTRY_SERVER_KEY \
             already resolves the bearer"
        );
    }

    // The cloud session lives in the secret store, so resolving the cloud
    // bearer necessarily reads it. The env-var escape hatch still skips the
    // store entirely (see env_server_key_skips_store_read_entirely).
    #[test]
    #[serial_test::serial]
    fn cloud_bearer_resolves_from_the_store_after_migration() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_legacy_auth(&path, &sample_tokens());

        let store = CountingStore::default();
        let cfg = Config::load_with_store_from(Some(&path), &store, None).unwrap();

        assert_eq!(
            cfg.bearer_for_with_store(server_keys::DEFAULT_CLOUD_URL, &store)
                .unwrap()
                .as_deref(),
            Some("at-sample")
        );
        assert!(
            store.get_calls.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "the cloud session lives in the store now, so resolving it reads the store"
        );
    }

    // `llm_model_configured` takes no SecretStore — its signature is
    // `fn(path: Option<&Path>) -> bool` — so it is structurally incapable of
    // touching the secret store. These tests cover its file-parsing contract.

    #[test]
    fn llm_model_configured_reads_only_the_config_file() {
        let tmp = TempDir::new().unwrap();

        let with_model = tmp.path().join("with_model.toml");
        std::fs::write(&with_model, "llm_model = \"gpt-x\"\n").unwrap();
        assert!(Config::llm_model_configured(Some(&with_model)));

        let without_model = tmp.path().join("without_model.toml");
        std::fs::write(&without_model, "server_url = \"http://x\"\n").unwrap();
        assert!(!Config::llm_model_configured(Some(&without_model)));

        let missing = tmp.path().join("does_not_exist.toml");
        assert!(!Config::llm_model_configured(Some(&missing)));
    }

    #[test]
    #[serial_test::serial]
    fn server_ca_env_overrides_config() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "server_ca = \"/from/config.pem\"\n").unwrap();

        unsafe { std::env::set_var("INKENTRY_SERVER_CA", "/from/env.pem") };
        let cfg = load_hermetic(&global).unwrap();
        unsafe { std::env::remove_var("INKENTRY_SERVER_CA") };

        assert_eq!(cfg.server_ca.as_deref(), Some("/from/env.pem"));
    }

    #[test]
    #[serial_test::serial]
    fn server_ca_from_config_when_env_unset() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "server_ca = \"/from/config.pem\"\n").unwrap();

        unsafe { std::env::remove_var("INKENTRY_SERVER_CA") };
        let cfg = load_hermetic(&global).unwrap();

        assert_eq!(cfg.server_ca.as_deref(), Some("/from/config.pem"));
    }

    #[test]
    #[serial_test::serial]
    fn llm_url_loads_from_personal_config() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_url = \"http://127.0.0.1:1234\"\n").unwrap();

        let cfg = load_hermetic(&global).unwrap();

        assert_eq!(cfg.llm_url.as_deref(), Some("http://127.0.0.1:1234"));
    }

    #[test]
    #[serial_test::serial]
    fn llm_url_absent_from_personal_config_stays_none() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_model = \"gpt-oss\"\n").unwrap();

        let cfg = load_hermetic(&global).unwrap();

        assert_eq!(cfg.llm_url, None);
    }

    #[test]
    #[serial_test::serial]
    fn llm_url_in_project_config_takes_effect() {
        clear_inkentry_env();
        let (_tmp, cfg) = load_layered("", Some("llm_url = \"http://gateway.example:1234\"\n"));
        assert_eq!(
            cfg.unwrap().llm_url.as_deref(),
            Some("http://gateway.example:1234"),
            "a team endpoint stated once in the project config must configure the CLI"
        );
    }

    #[test]
    #[serial_test::serial]
    fn llm_url_project_config_wins_over_personal_and_env_wins_over_both() {
        clear_inkentry_env();
        let (_tmp, cfg) = load_layered(
            "llm_url = \"http://personal.example:1\"\n",
            Some("llm_url = \"http://project.example:2\"\n"),
        );
        assert_eq!(
            cfg.unwrap().llm_url.as_deref(),
            Some("http://project.example:2"),
            "the project file must win over the personal one"
        );

        unsafe { std::env::set_var("INKENTRY_LLM_URL", "http://env.example:3") };
        let (_tmp2, cfg2) = load_layered(
            "llm_url = \"http://personal.example:1\"\n",
            Some("llm_url = \"http://project.example:2\"\n"),
        );
        unsafe { std::env::remove_var("INKENTRY_LLM_URL") };
        assert_eq!(
            cfg2.unwrap().llm_url.as_deref(),
            Some("http://env.example:3"),
            "the environment must win over both files"
        );
    }

    #[test]
    fn the_llm_credential_does_not_follow_its_url_into_the_project_config() {
        // The endpoint is a project-wide fact; the key presented to it is not.
        let warnings =
            project_warnings("llm_url = \"http://gateway.example:1234\"\nllm_key = \"sk-live\"\n")
                .join("\n");
        assert!(
            warnings.contains("llm_key"),
            "`llm_key` must still be an unread key: {warnings}"
        );
        assert!(
            !warnings.contains("`llm_url`"),
            "`llm_url` is read here now and must not warn: {warnings}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn llm_url_env_overrides_personal_config() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_url = \"http://127.0.0.1:1234\"\n").unwrap();

        unsafe { std::env::set_var("INKENTRY_LLM_URL", "https://gateway.example") };
        let cfg = load_hermetic(&global);
        unsafe { std::env::remove_var("INKENTRY_LLM_URL") };

        assert_eq!(
            cfg.unwrap().llm_url.as_deref(),
            Some("https://gateway.example")
        );
    }

    #[test]
    #[serial_test::serial]
    fn llm_url_env_applies_when_personal_config_sets_nothing() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "").unwrap();

        unsafe { std::env::set_var("INKENTRY_LLM_URL", "https://gateway.example") };
        let cfg = load_hermetic(&global);
        unsafe { std::env::remove_var("INKENTRY_LLM_URL") };

        assert_eq!(
            cfg.unwrap().llm_url.as_deref(),
            Some("https://gateway.example")
        );
    }

    #[test]
    #[serial_test::serial]
    fn llm_model_env_overrides_personal_config() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_model = \"from-config\"\n").unwrap();

        unsafe { std::env::set_var("INKENTRY_LLM_MODEL", "from-env") };
        let cfg = load_hermetic(&global);
        unsafe { std::env::remove_var("INKENTRY_LLM_MODEL") };

        assert_eq!(cfg.unwrap().llm_model.as_deref(), Some("from-env"));
    }

    #[test]
    #[serial_test::serial]
    fn llm_model_env_applies_when_personal_config_sets_nothing() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "").unwrap();

        unsafe { std::env::set_var("INKENTRY_LLM_MODEL", "from-env") };
        let cfg = load_hermetic(&global);
        unsafe { std::env::remove_var("INKENTRY_LLM_MODEL") };

        assert_eq!(cfg.unwrap().llm_model.as_deref(), Some("from-env"));
    }

    #[test]
    #[serial_test::serial]
    fn migration_preserves_llm_url_and_llm_model() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "llm_model = \"gpt-oss\"\nllm_url = \"http://127.0.0.1:1234\"\n",
        )
        .unwrap();
        write_legacy_auth(&path, &sample_tokens());

        let store = MemoryStore::default();
        let cfg = load_hermetic_with(&path, &store).unwrap();
        assert_eq!(cfg.llm_url.as_deref(), Some("http://127.0.0.1:1234"));
        assert_eq!(cfg.llm_model.as_deref(), Some("gpt-oss"));
        assert_eq!(
            org_tokens::resolve_session(&store, None)
                .unwrap()
                .unwrap()
                .access_token,
            "at-sample"
        );
    }

    // The daemon-spawn path is the only reader of the LLM credential, so a
    // plain `Config::load` must not pay for a secret-store read of it.
    #[test]
    #[serial_test::serial]
    fn config_load_never_reads_the_llm_key_from_the_store() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_url = \"http://127.0.0.1:1234\"\n").unwrap();

        let store = RecordingStore::default();
        store
            .set(secret_store::KEY_LLM_KEY, "sk-llm-secret")
            .unwrap();
        store.reads.lock().unwrap().clear();

        let cfg = Config::load_with_store_from(Some(&global), &store, None).unwrap();

        assert_eq!(cfg.llm_url.as_deref(), Some("http://127.0.0.1:1234"));
        assert!(
            !store
                .reads
                .lock()
                .unwrap()
                .iter()
                .any(|k| k == secret_store::KEY_LLM_KEY),
            "Config::load must not read the LLM key: reads were {:?}",
            store.reads.lock().unwrap()
        );
    }

    // Broader than the guard above: the ordinary load path reads no secret at
    // all, so any read added later goes red here too.
    #[test]
    #[serial_test::serial]
    fn config_load_reads_nothing_at_all_from_an_injected_store() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(
            &global,
            "llm_url = \"http://127.0.0.1:1234\"\nllm_model = \"gpt-oss\"\n",
        )
        .unwrap();

        let store = RecordingStore::default();
        store
            .set(secret_store::KEY_LLM_KEY, "sk-llm-secret")
            .unwrap();
        store
            .set(
                server_keys::KEY_SERVER_KEYS_MAP,
                "{\"https://a.example\":\"sk-a\"}",
            )
            .unwrap();
        store.reads.lock().unwrap().clear();

        Config::load_with_store_from(Some(&global), &store, None).unwrap();

        let reads = store.reads.lock().unwrap().clone();
        assert!(
            reads.is_empty(),
            "a config load must read no secret at all: {reads:?}"
        );
    }

    // `Config::load` resolves its own store, so the RecordingStore guards
    // above can't observe that path. An unparseable secrets.toml makes any
    // read fail regardless of backend, so loading successfully is the proof.
    #[test]
    #[serial_test::serial]
    fn the_public_load_entry_point_reads_no_secret_either() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_url = \"http://127.0.0.1:1234\"\n").unwrap();
        std::fs::write(tmp.path().join("secrets.toml"), "not = valid = toml\n").unwrap();

        let prev_store = std::env::var(secret_store::ENV_SECRET_STORE).ok();
        unsafe {
            std::env::set_var("INKENTRY_CONFIG_DIR", tmp.path());
            std::env::set_var(secret_store::ENV_SECRET_STORE, "file");
        }
        let loaded = Config::load(Some(&global));
        unsafe {
            std::env::remove_var("INKENTRY_CONFIG_DIR");
            match &prev_store {
                Some(v) => std::env::set_var(secret_store::ENV_SECRET_STORE, v),
                None => std::env::remove_var(secret_store::ENV_SECRET_STORE),
            }
        }

        let cfg = loaded.expect("Config::load must not read the secret store");
        assert_eq!(cfg.llm_url.as_deref(), Some("http://127.0.0.1:1234"));
    }

    // An explicitly empty INKENTRY_LLM_URL is an override like any other value,
    // so it blanks the personal config rather than falling through to it. The
    // spawn path then normalizes the blank away and configures no endpoint.
    #[test]
    #[serial_test::serial]
    fn llm_url_env_set_to_empty_still_overrides_the_personal_config() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_url = \"http://127.0.0.1:1234\"\n").unwrap();

        unsafe { std::env::set_var("INKENTRY_LLM_URL", "") };
        let cfg = load_hermetic(&global);
        unsafe { std::env::remove_var("INKENTRY_LLM_URL") };

        assert_eq!(cfg.unwrap().llm_url.as_deref(), Some(""));
    }

    #[test]
    #[serial_test::serial]
    fn llm_model_env_set_to_empty_still_overrides_the_personal_config() {
        clear_inkentry_env();
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("config.toml");
        std::fs::write(&global, "llm_model = \"gpt-oss\"\n").unwrap();

        unsafe { std::env::set_var("INKENTRY_LLM_MODEL", "") };
        let cfg = load_hermetic(&global);
        unsafe { std::env::remove_var("INKENTRY_LLM_MODEL") };

        assert_eq!(cfg.unwrap().llm_model.as_deref(), Some(""));
    }

    // A MemoryStore that records every key passed to `get`.
    #[derive(Default)]
    struct RecordingStore {
        inner: MemoryStore,
        reads: std::sync::Mutex<Vec<String>>,
    }

    impl SecretStore for RecordingStore {
        fn get(&self, key: &str) -> Result<Option<String>> {
            self.reads.lock().unwrap().push(key.to_string());
            self.inner.get(key)
        }
        fn set(&self, key: &str, value: &str) -> Result<()> {
            self.inner.set(key, value)
        }
        fn delete(&self, key: &str) -> Result<()> {
            self.inner.delete(key)
        }
        fn kind(&self) -> &'static str {
            "recording"
        }
    }
}
