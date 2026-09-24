use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedderState {
    Loading,
    Ready,
    Unavailable,
    Disabled,
    // `other`: an unrecognised state must not fail deserialization of the whole
    // health body, which would discard `limits` and every advertised capability.
    #[default]
    #[serde(other)]
    Unknown,
}

impl EmbedderState {
    pub fn as_str(&self) -> &'static str {
        match self {
            EmbedderState::Loading => "loading",
            EmbedderState::Ready => "ready",
            EmbedderState::Unavailable => "unavailable",
            EmbedderState::Disabled => "disabled",
            EmbedderState::Unknown => "unknown",
        }
    }
}

// Members are independently optional so one unreadable field does not discard
// its siblings; each consumer applies its own legacy fallback for `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerLimits {
    pub embed_request_timeout_secs: Option<u64>,
    pub max_batch_chunks: Option<usize>,
    pub embedder_token_cap: Option<usize>,
    // `Some(1)` is the only value acted on: embedding is single-threaded and the
    // user should be pointed at `INKENTRY_EMBED_THREADS`.
    pub embed_threads: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub search_semantic: bool,
    pub index_embed: bool,
    pub memory_push: bool,
    pub memory_pull: bool,
    pub memory_search: bool,
    pub memory_harvest: bool,
    // The only reliable LLM signal: a server can advertise older capabilities
    // while serving no `/llm/complete` route. Skipped so the `status` JSON keeps
    // its shape.
    #[serde(skip_serializing)]
    pub llm_complete: bool,
    // Reserved until an `inkentry plan` command ships.
    #[serde(skip_serializing)]
    #[allow(dead_code)]
    pub plan: bool,
    // A top-level bool in `/v1/health`, not an entry in `capabilities`. When
    // false the sync push stays text-only.
    #[serde(skip_serializing)]
    pub accepts_pushed_vectors: bool,
}

impl Capabilities {
    pub(crate) fn from_server_caps(caps: &[&str]) -> Self {
        let has = |c: &str| caps.contains(&c);
        let memory = has("memory");
        Self {
            search_semantic: has("search.semantic"),
            index_embed: has("index.embed"),
            memory_push: memory,
            memory_pull: memory,
            memory_search: memory,
            memory_harvest: memory,
            llm_complete: has("llm.complete"),
            plan: has("plan"),
            accepts_pushed_vectors: false,
        }
    }

    pub(crate) fn legacy_memory_only() -> Self {
        Self {
            search_semantic: false,
            index_embed: false,
            memory_push: true,
            memory_pull: true,
            memory_search: true,
            memory_harvest: false,
            llm_complete: false,
            plan: false,
            accepts_pushed_vectors: false,
        }
    }

    #[cfg(test)]
    pub fn all() -> Self {
        Self {
            search_semantic: true,
            index_embed: true,
            memory_push: true,
            memory_pull: true,
            memory_search: true,
            memory_harvest: true,
            llm_complete: true,
            plan: true,
            accepts_pushed_vectors: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_server_caps_empty_returns_all_false() {
        let caps = Capabilities::from_server_caps(&[]);
        assert!(!caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(!caps.memory_push);
        assert!(!caps.memory_pull);
        assert!(!caps.memory_search);
        assert!(!caps.memory_harvest);
        assert!(!caps.plan);
    }

    #[test]
    fn from_server_caps_full_set() {
        let caps =
            Capabilities::from_server_caps(&["search.semantic", "index.embed", "memory", "plan"]);
        assert!(caps.search_semantic);
        assert!(caps.index_embed);
        assert!(caps.memory_push);
        assert!(caps.memory_pull);
        assert!(caps.memory_search);
        assert!(caps.memory_harvest);
        assert!(caps.plan);
    }

    #[test]
    fn from_server_caps_memory_only() {
        let caps = Capabilities::from_server_caps(&["memory"]);
        assert!(!caps.search_semantic);
        assert!(!caps.index_embed);
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
        assert!(caps.plan);
        assert!(!caps.memory_push);
        assert!(!caps.memory_pull);
        assert!(!caps.memory_search);
        assert!(!caps.memory_harvest);
    }

    #[test]
    fn from_server_caps_llm_complete_sets_the_flag() {
        let caps = Capabilities::from_server_caps(&["memory", "llm.complete"]);
        assert!(caps.llm_complete);
    }

    #[test]
    fn from_server_caps_legacy_cap_without_llm_complete_is_not_llm_capable() {
        let caps =
            Capabilities::from_server_caps(&["memory", "index.embed", "legacy.feature", "plan"]);
        assert!(!caps.llm_complete);
    }

    #[test]
    fn from_server_caps_still_lists_removed_explore_cap_is_ignored_and_not_llm_capable() {
        let caps = Capabilities::from_server_caps(&["memory", "index.embed", "explore"]);
        assert!(!caps.llm_complete);
        assert!(caps.memory_push);
    }

    #[test]
    fn from_server_caps_without_llm_complete_is_not_llm_capable() {
        let caps = Capabilities::from_server_caps(&["memory", "index.embed"]);
        assert!(!caps.llm_complete);
    }

    #[test]
    fn legacy_memory_only_is_not_llm_capable() {
        assert!(!Capabilities::legacy_memory_only().llm_complete);
    }

    #[test]
    fn all_is_llm_capable() {
        assert!(Capabilities::all().llm_complete);
    }

    #[test]
    fn llm_complete_is_not_serialized_into_status_json() {
        let value = serde_json::to_value(Capabilities::all()).expect("serialize capabilities");
        let object = value
            .as_object()
            .expect("capabilities serialize as an object");
        assert!(
            !object.contains_key("llm_complete"),
            "llm_complete must not reach `inkentry status --format json`: {object:?}"
        );
    }

    #[test]
    fn from_server_caps_unknown_capability_is_ignored() {
        let caps = Capabilities::from_server_caps(&["search.semantic", "unknown.future", "memory"]);
        assert!(caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(caps.memory_push);
    }

    #[test]
    fn legacy_memory_only_values() {
        let caps = Capabilities::legacy_memory_only();
        assert!(!caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(!caps.plan);
        assert!(caps.memory_push);
        assert!(caps.memory_pull);
        assert!(caps.memory_search);
        assert!(!caps.memory_harvest);
    }

    #[test]
    fn all_values_are_true() {
        let caps = Capabilities::all();
        assert!(caps.search_semantic);
        assert!(caps.index_embed);
        assert!(caps.memory_push);
        assert!(caps.memory_pull);
        assert!(caps.memory_search);
        assert!(caps.memory_harvest);
        assert!(caps.plan);
    }

    #[test]
    fn embedder_state_default_is_unknown() {
        assert_eq!(EmbedderState::default(), EmbedderState::Unknown);
    }

    #[test]
    fn embedder_state_deserializes_lowercase_wire_values() {
        for (wire, want) in [
            ("loading", EmbedderState::Loading),
            ("ready", EmbedderState::Ready),
            ("unavailable", EmbedderState::Unavailable),
            ("disabled", EmbedderState::Disabled),
        ] {
            let got: EmbedderState =
                serde_json::from_value(serde_json::Value::String(wire.to_string())).unwrap();
            assert_eq!(got, want, "wire {wire:?} should deserialize to {want:?}");
            assert_eq!(want.as_str(), wire, "as_str round-trips the wire value");
        }
    }
}
