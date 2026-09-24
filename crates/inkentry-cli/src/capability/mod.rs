mod diagnostics;
mod guard;
mod llm_message;
mod llm_route;
mod probe;
mod state;
mod tier;

#[cfg(test)]
pub(crate) use diagnostics::ALL_OFFLINE_REASONS;
pub use diagnostics::{explicit_probe_failure, offline_search_hint, shared_offline_advice};
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
// Reused by server::probe_local_relay_port so relay reuse refuses the same responders as discovery.
pub(crate) use probe::untrusted_responder;
pub use probe::{get_inference_tier, get_inference_tier_fresh, get_tier};
// `Capabilities` is only reached from outside this module by other crates'
// `#[cfg(test)]` code (`Capabilities::all()`), so a non-test build sees this
// re-export as unused.
#[allow(unused_imports)]
pub use state::Capabilities;
pub use state::{EmbedderState, ServerLimits};
pub use tier::Tier;
