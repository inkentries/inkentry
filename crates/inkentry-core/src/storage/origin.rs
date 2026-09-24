//! ADR-098 D6: an entry's origin — who or what produced it.
//!
//! Optional and additive on every carrier: absent means the caller declared
//! nothing, which reads as `unknown`, never as "no human wrote this". Not
//! part of `entity_id`, so identity and dedupe are unchanged by whether two
//! otherwise-identical entries carry an origin at all (see
//! `entity_id::entity_id`).

use serde::{Deserialize, Serialize};

use crate::config::caller::{ActorKind, CallerDeclaration};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub actor_kind: ActorKind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
}

impl Origin {
    /// The origin a caller's own declaration implies, or `None` when it
    /// declared no actor at all — absence, not a fabricated `unknown` object,
    /// is how "not declared" is represented (D6).
    pub fn from_caller(decl: &CallerDeclaration) -> Option<Self> {
        if decl.actor == ActorKind::Unknown {
            return None;
        }
        Some(Self {
            actor_kind: decl.actor,
            tool: decl.tool.clone(),
            model: decl.model.clone(),
        })
    }

    /// The origin `inkentry harvest` stamps on every entry it writes,
    /// regardless of what the environment declares (D6: only harvest itself
    /// may set `actor_kind = harvest`). `model` is the model harvest used for
    /// extraction, when known.
    pub fn harvest(model: Option<String>) -> Self {
        Self {
            actor_kind: ActorKind::Harvest,
            tool: None,
            model,
        }
    }

    /// Build from the three nullable `notes.origin_*` columns (or the
    /// equivalent carrier/wire fields), as a row mapper reads them.
    /// `None` unless `actor_kind` is present and recognised: a NULL/absent
    /// `actor_kind` is what "no origin recorded" looks like on every path
    /// that stores these three columns independently, and a value this build
    /// does not recognise is treated the same way rather than surfaced as a
    /// half-formed origin.
    pub fn from_parts(
        actor_kind: Option<&str>,
        tool: Option<String>,
        model: Option<String>,
    ) -> Option<Self> {
        let actor_kind = ActorKind::parse(actor_kind?)?;
        Some(Self {
            actor_kind,
            tool,
            model,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undeclared_caller_yields_no_origin() {
        assert_eq!(Origin::from_caller(&CallerDeclaration::default()), None);
    }

    #[test]
    fn a_declared_human_actor_yields_an_origin() {
        let decl = CallerDeclaration {
            actor: ActorKind::Human,
            tool: Some("cli".to_string()),
            ..Default::default()
        };
        let origin = Origin::from_caller(&decl).expect("declared actor yields an origin");
        assert_eq!(origin.actor_kind, ActorKind::Human);
        assert_eq!(origin.tool.as_deref(), Some("cli"));
    }

    #[test]
    fn harvest_origin_is_never_derived_from_the_caller_declaration() {
        let origin = Origin::harvest(Some("gpt-oss-20b".to_string()));
        assert_eq!(origin.actor_kind, ActorKind::Harvest);
        assert_eq!(origin.model.as_deref(), Some("gpt-oss-20b"));
    }

    #[test]
    fn from_parts_is_none_without_an_actor_kind() {
        assert_eq!(Origin::from_parts(None, None, None), None);
    }

    #[test]
    fn from_parts_ignores_an_unrecognised_actor_kind() {
        assert_eq!(Origin::from_parts(Some("robot"), None, None), None);
    }

    #[test]
    fn from_parts_round_trips_a_recognised_actor_kind() {
        let origin = Origin::from_parts(Some("agent"), Some("cli".to_string()), None)
            .expect("recognised actor_kind");
        assert_eq!(origin.actor_kind, ActorKind::Agent);
        assert_eq!(origin.tool.as_deref(), Some("cli"));
        assert_eq!(origin.model, None);
    }
}
