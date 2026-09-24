//! ADR-098 D5/D6: what the caller declares about itself — how the invocation
//! was triggered and by what kind of actor — read once from the environment
//! at [`super::Config::load`] and shared by every command that records an
//! event or an entry's origin.
//!
//! Never inferred from a TTY or any other environment probe: a wrong guess
//! would silently corrupt the automation metrics this declaration feeds
//! (ADR-098's rationale table).

use sha2::{Digest, Sha256};

/// How many hex characters of the session ref's SHA-256 are kept. Long enough
/// to group a session's events without carrying the raw token, which may
/// itself be sensitive, into storage.
const SESSION_REF_HASH_LEN: usize = 16;

pub const ENV_TRIGGER: &str = "INKENTRY_TRIGGER";
pub const ENV_ACTOR: &str = "INKENTRY_ACTOR";
pub const ENV_SESSION_REF: &str = "INKENTRY_SESSION_REF";
pub const ENV_TOOL: &str = "INKENTRY_TOOL";
pub const ENV_MODEL: &str = "INKENTRY_MODEL";

/// How the current invocation was triggered: `INKENTRY_TRIGGER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// A person typed the command, or an agent ran it deliberately.
    Explicit,
    /// A hook or automation ran the command without a person choosing to.
    Hook,
    /// Not declared.
    #[default]
    Unknown,
}

impl Trigger {
    /// Parse `INKENTRY_TRIGGER`'s value. `None` for anything but `explicit`/
    /// `hook`, so an unrecognised value reads the same as undeclared rather
    /// than silently becoming one of the two known trigger kinds.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "explicit" => Some(Self::Explicit),
            "hook" => Some(Self::Hook),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Hook => "hook",
            Self::Unknown => "unknown",
        }
    }
}

/// Who or what is acting. `Human`/`Agent` are the two values a caller may
/// declare via `INKENTRY_ACTOR`; `Harvest` is never declared — only
/// `inkentry harvest` itself sets it, on an entry's origin (D6); `Unknown`
/// covers an undeclared caller and is the events table's own third value (D5).
/// An entry's `origin` uses only the first three: an undeclared origin is
/// represented by the whole `origin` object being absent, never by this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Human,
    Agent,
    Harvest,
    #[default]
    Unknown,
}

impl ActorKind {
    /// Parse any of the four string forms, as stored in `events.actor_kind` or
    /// `notes.origin_actor_kind`. Wider than [`Self::parse_declared`], which
    /// is what a caller's own `INKENTRY_ACTOR` may set.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "human" => Some(Self::Human),
            "agent" => Some(Self::Agent),
            "harvest" => Some(Self::Harvest),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    /// Parse `INKENTRY_ACTOR`'s value: `human` or `agent` only. `harvest` is
    /// never a caller declaration — `inkentry harvest` sets it itself,
    /// regardless of what the environment declares (D6) — so it is refused
    /// here the same as any other unrecognised value, and the caller reads as
    /// undeclared rather than silently becoming a harvest.
    pub fn parse_declared(s: &str) -> Option<Self> {
        match s.trim() {
            "human" => Some(Self::Human),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::Harvest => "harvest",
            Self::Unknown => "unknown",
        }
    }
}

/// The caller's self-declaration, read once at [`super::Config::load`] and
/// carried on [`super::Config::caller`]. Every field defaults to "not
/// declared" so a caller that sets none of these five variables is
/// indistinguishable from one running against a build that predates them.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CallerDeclaration {
    pub trigger: Trigger,
    pub actor: ActorKind,
    /// SHA-256 hex of `INKENTRY_SESSION_REF`, truncated to
    /// [`SESSION_REF_HASH_LEN`] characters — never the raw value, which may
    /// itself carry identifying information the event log has no business
    /// storing (D5: "session_ref is stored hashed").
    pub session_ref: Option<String>,
    /// `INKENTRY_TOOL`, free text: the agent tool that made the call (e.g.
    /// `claude-code`), for an entry's `origin.tool`.
    pub tool: Option<String>,
    /// `INKENTRY_MODEL`, free text: the model the caller is running under,
    /// for an entry's `origin.model`.
    pub model: Option<String>,
}

impl CallerDeclaration {
    /// Read the five declaration variables from the real process environment.
    pub fn from_env() -> Self {
        Self::from_getter(|k| std::env::var(k).ok())
    }

    /// [`Self::from_env`] against an injected variable source, so a test can
    /// assert the parsing rules without mutating the real process
    /// environment.
    fn from_getter(get: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            trigger: get(ENV_TRIGGER)
                .and_then(|v| Trigger::parse(&v))
                .unwrap_or_default(),
            actor: get(ENV_ACTOR)
                .and_then(|v| ActorKind::parse_declared(&v))
                .unwrap_or_default(),
            session_ref: get(ENV_SESSION_REF).map(|v| hash_session_ref(&v)),
            tool: get(ENV_TOOL),
            model: get(ENV_MODEL),
        }
    }
}

fn hash_session_ref(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    let hex = digest.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    hex.chars().take(SESSION_REF_HASH_LEN).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn getter(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn undeclared_reads_as_unknown_on_every_field() {
        let d = CallerDeclaration::from_getter(getter(&[]));
        assert_eq!(d.trigger, Trigger::Unknown);
        assert_eq!(d.actor, ActorKind::Unknown);
        assert_eq!(d.session_ref, None);
        assert_eq!(d.tool, None);
        assert_eq!(d.model, None);
    }

    #[test]
    fn declared_trigger_and_actor_parse() {
        let d =
            CallerDeclaration::from_getter(getter(&[(ENV_TRIGGER, "hook"), (ENV_ACTOR, "agent")]));
        assert_eq!(d.trigger, Trigger::Hook);
        assert_eq!(d.actor, ActorKind::Agent);
    }

    #[test]
    fn an_unrecognised_value_reads_as_unknown_not_a_guess() {
        let d = CallerDeclaration::from_getter(getter(&[
            (ENV_TRIGGER, "sometimes"),
            (ENV_ACTOR, "robot"),
        ]));
        assert_eq!(d.trigger, Trigger::Unknown);
        assert_eq!(d.actor, ActorKind::Unknown);
    }

    #[test]
    fn actor_declaration_never_accepts_harvest() {
        // Only `inkentry harvest` itself may set an origin's actor_kind to
        // harvest (D6); a caller cannot claim it via the environment.
        let d = CallerDeclaration::from_getter(getter(&[(ENV_ACTOR, "harvest")]));
        assert_eq!(d.actor, ActorKind::Unknown);
    }

    #[test]
    fn session_ref_is_hashed_never_stored_raw() {
        let d =
            CallerDeclaration::from_getter(getter(&[(ENV_SESSION_REF, "super-secret-session")]));
        let hashed = d.session_ref.expect("session ref declared");
        assert_eq!(hashed.len(), SESSION_REF_HASH_LEN);
        assert_ne!(hashed, "super-secret-session");
        assert!(hashed.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn session_ref_hash_is_stable_for_the_same_input() {
        let a = hash_session_ref("same-session");
        let b = hash_session_ref("same-session");
        assert_eq!(a, b);
    }

    #[test]
    fn tool_and_model_are_carried_verbatim() {
        let d = CallerDeclaration::from_getter(getter(&[
            (ENV_TOOL, "claude-code"),
            (ENV_MODEL, "claude-sonnet-5"),
        ]));
        assert_eq!(d.tool.as_deref(), Some("claude-code"));
        assert_eq!(d.model.as_deref(), Some("claude-sonnet-5"));
    }
}
