//! Capability tier detection for the inkentry CLI.
//!
//! Tier 0 (Offline): no server_url configured, or server unreachable.
//! Tier 1 (Server):  server_url set and GET /v1/health succeeds.
//!
//! ## Loopback auto-discovery (spelunk-cloud/spelunk#316 / 0.8.0)
//!
//! When `cfg.server_url` is `None` **and** `INKENTRY_NO_SERVER` is not set, the probe
//! attempts to reach a locally-running inkentry-server before falling through to
//! `Tier::Offline`:
//!
//! 1. Read `~/.local/state/inkentry/server.port` (written by `inkentry server start`);
//!    use `http://127.0.0.1:<port>` if the file exists and the responder there is
//!    the daemon recorded beside it (`server.pid` still an `inkentry-server`,
//!    reported `instance_id` the recorded one). A responder failing either check
//!    is not used, and step 2 is not tried.
//! 2. When nothing was recorded, probe `http://127.0.0.1:<DEFAULT_SERVER_PORT>`
//!    with a **250 ms** timeout (distinct from the 2 s timeout used for
//!    explicitly-configured remote URLs).
//! 3. On success, treat as `Tier::Server` with `auto_discovered = true`.
//! 4. On failure, return `Tier::Offline`.
//!
//! `INKENTRY_NO_SERVER=1` short-circuits all loopback probing and forces `Tier::Offline`.
//!
//! Every path to `Tier::Offline` names its [`OfflineReason`], so a command
//! rendering the offline state can key its advice to the branch that fired
//! instead of guessing one that fits every branch and fits none.
//!
//! The probe runs lazily on the first call that needs Tier 1 and its result
//! is cached for the process lifetime.
//!
//! ## Module layout
//!
//! - [`state`]: the data types parsed from `/v1/health` (`Capabilities`,
//!   `EmbedderState`, `ServerLimits`).
//! - [`tier`]: the resolved [`Tier`] enum itself.
//! - [`probe`]: loopback auto-discovery + explicit `server_url` health probing,
//!   and the per-process `Tier` cache (`get_tier`).
//! - [`diagnostics`]: the [`OfflineReason`] classification and the advice it
//!   yields, plus probe-failure classification and TLS error rendering.
//! - [`guard`]: the `require_*` functions commands call to gate a feature on
//!   a `Tier`.

mod diagnostics;
mod guard;
mod llm_message;
mod llm_route;
mod probe;
mod state;
mod tier;

pub use diagnostics::{explicit_probe_failure, offline_search_hint};
// Both are named only inside `capability` and its tests: a caller pattern-matches
// the `Tier::Offline` payload and hands it straight back to `offline_search_hint`
// alongside `explicit_probe_failure()`, never spelling either type out.
#[allow(unused_imports)]
pub use diagnostics::{ConnFailure, OfflineReason};
pub use guard::{inference_server_required_message, require_explicit_server_url, require_tier1};
pub use llm_message::{NoLlmReason, no_llm_message};
// `LlmRoute` is named only inside `llm_route` and its tests; callers work
// through the methods on the value `resolve_llm_route` hands back.
#[allow(unused_imports)]
pub use llm_route::{LlmRoute, resolve_llm_route};
pub(crate) use probe::inkentry_state_dir;
// The loopback-discovery trust check, reused by `server::probe_local_relay_port`
// so the relay-reuse gate refuses the same responders step 3a does (ADR-091).
pub(crate) use probe::untrusted_responder;
pub use probe::{get_inference_tier, get_inference_tier_fresh, get_tier};
// `Capabilities` is only reached from outside this module by other crates'
// `#[cfg(test)]` code (`Capabilities::all()`), so a non-test build sees this
// re-export as unused.
#[allow(unused_imports)]
pub use state::Capabilities;
pub use state::{EmbedderState, ServerLimits};
pub use tier::Tier;
