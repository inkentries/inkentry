//! Tag normalisation: the one transform every write path applies to a raw tag
//! before it is stored. The original spelling is not kept.

use unicode_normalization::UnicodeNormalization;

/// Unicode NFC, lowercased, trimmed, with runs of whitespace or `_` collapsed
/// to a single `-`. Returns `None` when the result is empty, so a caller can
/// drop the tag rather than store nothing.
pub fn normalize_tag(raw: &str) -> Option<String> {
    let nfc: String = raw.nfc().collect();
    let mut out = String::with_capacity(nfc.len());
    let mut pending_sep = false;
    for c in nfc.trim().chars() {
        if c.is_whitespace() || c == '_' {
            pending_sep = true;
            continue;
        }
        if pending_sep && !out.is_empty() {
            out.push('-');
        }
        pending_sep = false;
        out.extend(c.to_lowercase());
    }
    if out.is_empty() { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::normalize_tag;

    #[test]
    fn mixed_case_collapses_to_one_spelling() {
        assert_eq!(normalize_tag("Auth"), normalize_tag("auth"));
        assert_eq!(normalize_tag("AUTH"), Some("auth".to_string()));
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(normalize_tag("  auth  "), Some("auth".to_string()));
    }

    #[test]
    fn a_run_of_whitespace_becomes_one_dash() {
        assert_eq!(
            normalize_tag("auth   service"),
            Some("auth-service".to_string())
        );
    }

    #[test]
    fn underscores_become_dashes() {
        assert_eq!(
            normalize_tag("auth_service"),
            Some("auth-service".to_string())
        );
    }

    #[test]
    fn a_mixed_run_of_whitespace_and_underscore_collapses_to_one_dash() {
        assert_eq!(
            normalize_tag("auth _  service"),
            Some("auth-service".to_string())
        );
    }

    #[test]
    fn leading_and_trailing_separators_leave_no_dash() {
        assert_eq!(normalize_tag("__auth__"), Some("auth".to_string()));
        assert_eq!(normalize_tag("  _auth_  "), Some("auth".to_string()));
    }

    #[test]
    fn empty_or_whitespace_only_is_dropped() {
        assert_eq!(normalize_tag(""), None);
        assert_eq!(normalize_tag("   "), None);
        assert_eq!(normalize_tag("___"), None);
    }

    #[test]
    fn unicode_is_normalised_to_nfc_before_comparison() {
        // "é" as NFD (e + combining acute) vs NFC (single codepoint) must
        // normalise to the same tag.
        let nfd = "e\u{0301}cole";
        let nfc = "\u{00e9}cole";
        assert_eq!(normalize_tag(nfd), normalize_tag(nfc));
    }

    #[test]
    fn ordinary_tags_pass_through_unchanged() {
        assert_eq!(normalize_tag("database"), Some("database".to_string()));
        assert_eq!(
            normalize_tag("git:abcdef0"),
            Some("git:abcdef0".to_string())
        );
    }
}
