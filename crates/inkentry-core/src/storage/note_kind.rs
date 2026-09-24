//! The canonical memory-entry kinds and a strict parser for a user-supplied kind.
//!
//! Retrieval selects entries by kind, so an entry stored under a kind outside
//! [`NOTE_KINDS`] (a typo such as `decisions`) is invisible to it.

/// The kinds a memory entry may have.
///
/// Retrieval selects on a subset of these, and every kind it selects on must be
/// a member.
pub const NOTE_KINDS: [&str; 9] = [
    "decision",
    "context",
    "requirement",
    "note",
    "question",
    "answer",
    "handoff",
    "intent",
    "antipattern",
];

/// Whether `kind` is one of the canonical [`NOTE_KINDS`].
pub fn is_valid_note_kind(kind: &str) -> bool {
    NOTE_KINDS.contains(&kind)
}

/// Returns `kind` unchanged when it is one of [`NOTE_KINDS`], otherwise an error
/// naming the value and listing the valid kinds.
///
/// The signature is that of a clap value parser, so an invalid `--kind` is
/// rejected before any store is opened.
pub fn parse_note_kind(kind: &str) -> Result<String, String> {
    if is_valid_note_kind(kind) {
        Ok(kind.to_string())
    } else {
        Err(format!(
            "unknown kind '{kind}' — valid kinds are: {}",
            NOTE_KINDS.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_set_is_exactly_the_nine_documented_kinds() {
        let mut got = NOTE_KINDS.to_vec();
        got.sort_unstable();
        let mut want = vec![
            "answer",
            "antipattern",
            "context",
            "decision",
            "handoff",
            "intent",
            "note",
            "question",
            "requirement",
        ];
        want.sort_unstable();
        assert_eq!(got, want);
    }

    #[test]
    fn every_canonical_kind_is_valid() {
        for kind in NOTE_KINDS {
            assert!(is_valid_note_kind(kind), "{kind} should be valid");
        }
    }

    #[test]
    fn unknown_and_typo_kinds_are_invalid() {
        for kind in ["", "bogus", "decisions", "desicion", "Decision", "notes"] {
            assert!(!is_valid_note_kind(kind), "{kind:?} should be invalid");
        }
    }

    #[test]
    fn parse_accepts_every_canonical_kind_unchanged() {
        for kind in NOTE_KINDS {
            assert_eq!(parse_note_kind(kind).as_deref(), Ok(kind));
        }
    }

    #[test]
    fn parse_rejects_unknown_naming_value_and_listing_kinds() {
        let err = parse_note_kind("decisions").expect_err("must reject");
        assert!(
            err.contains("decisions"),
            "must name the offending value: {err}"
        );
        for kind in NOTE_KINDS {
            assert!(err.contains(kind), "must list valid kind {kind}: {err}");
        }
    }
}
