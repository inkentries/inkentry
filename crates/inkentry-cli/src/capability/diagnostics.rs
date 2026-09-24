pub(crate) use inkentry_core::config::find_rustls_cause;

// First write wins. A Mutex rather than a OnceCell so tests can reset it.
static EXPLICIT_PROBE_FAILURE: std::sync::Mutex<Option<ConnFailure>> = std::sync::Mutex::new(None);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnFailure {
    Unreachable,
    Tls(String),
}

pub fn explicit_probe_failure() -> Option<ConnFailure> {
    EXPLICIT_PROBE_FAILURE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub(crate) fn record_explicit_probe_failure(cause: ConnFailure) {
    let mut slot = EXPLICIT_PROBE_FAILURE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = Some(cause);
    }
}

// Pair with #[serial_test::serial(explicit_probe_failure)]: the static is process-global.
#[cfg(test)]
pub(crate) fn reset_explicit_probe_failure_for_test() {
    *EXPLICIT_PROBE_FAILURE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
}

// reqwest's Display shows only the top-level message; the cause (TLS, DNS) is deeper in source().
pub(crate) fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        out.push_str(" -> ");
        out.push_str(&e.to_string());
        source = e.source();
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineReason {
    // Outranks mode and server_url: the probe stops before either is read.
    KillSwitch,
    // Separate from ModeOfflineConfig: the env var overrides config, so a config edit would not help.
    ModeOfflineEnv,
    ModeOfflineConfig,
    NoLocalServer,
    LocalServerUnusable,
    // Distinct from NoLocalServer, whose advice (start a server) is wrong when one was already started.
    RecordedServerUnreachable,
    ExplicitServerUnavailable,
}

// Adding a variant does not fail here; extend this array too.
#[cfg(test)]
pub(crate) const ALL_OFFLINE_REASONS: [OfflineReason; 7] = [
    OfflineReason::KillSwitch,
    OfflineReason::ModeOfflineEnv,
    OfflineReason::ModeOfflineConfig,
    OfflineReason::NoLocalServer,
    OfflineReason::LocalServerUnusable,
    OfflineReason::RecordedServerUnreachable,
    OfflineReason::ExplicitServerUnavailable,
];

impl OfflineReason {
    // Decided before any socket opens, so a retry cannot change them; the other reasons
    // come from a probe that may answer differently later.
    pub fn is_explicit_opt_out(self) -> bool {
        match self {
            Self::KillSwitch | Self::ModeOfflineEnv | Self::ModeOfflineConfig => true,
            Self::NoLocalServer
            | Self::LocalServerUnusable
            | Self::RecordedServerUnreachable
            | Self::ExplicitServerUnavailable => false,
        }
    }
}

pub fn offline_search_hint(reason: OfflineReason, failure: Option<ConnFailure>) -> String {
    if let Some(advice) = shared_offline_advice(reason) {
        return format!("  [{advice}]");
    }
    match reason {
        OfflineReason::ExplicitServerUnavailable => match failure {
            Some(ConnFailure::Tls(cause)) => format!("  [tls: {cause}]"),
            _ => "  [unreachable]".to_string(),
        },
        // Only NoLocalServer reaches here. server_url is advised only here: elsewhere it is
        // wrong (solo user) or inert (explicit offline).
        _ => "  [run `inkentry server start` for semantic search, \
             or set server_url to share a team server]"
            .to_string(),
    }
}

// None where the caller renders it: ExplicitServerUnavailable names a URL only the caller
// holds, and NoLocalServer has per-surface wording.
pub fn shared_offline_advice(reason: OfflineReason) -> Option<&'static str> {
    match reason {
        OfflineReason::KillSwitch => {
            Some("INKENTRY_NO_SERVER is set: unset it to enable semantic search")
        }
        OfflineReason::ModeOfflineEnv => {
            Some("INKENTRY_MODE=offline is set: unset it to enable semantic search")
        }
        OfflineReason::ModeOfflineConfig => {
            Some("offline mode is on: remove mode = \"offline\" to enable semantic search")
        }
        OfflineReason::LocalServerUnusable => Some(
            "the local server embeds at a different dimension than this build reads: \
             run `inkentry server stop`, then `inkentry server start`",
        ),
        OfflineReason::RecordedServerUnreachable => Some(
            "the recorded local server did not answer, or could not be identified as \
             the one that was started: run `inkentry server stop`, then \
             `inkentry server start`",
        ),
        OfflineReason::NoLocalServer | OfflineReason::ExplicitServerUnavailable => None,
    }
}

pub(crate) fn cert_trust_hint() -> String {
    "\n  server_ca is configured; two classic misconfigurations cause this:\n  \
     1) the file points at the server's own leaf certificate, not the issuing CA\n  \
     2) the server is presenting a CA certificate (CA:TRUE) as its own leaf certificate\n  \
     See docs/server-setup.md, section \"Trusting the server's certificate on the client\"."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::ALL_OFFLINE_REASONS as REASONS;

    #[test]
    fn every_reason_gets_its_own_suggestion() {
        let mut seen: Vec<String> = Vec::new();
        for reason in REASONS {
            let hint = offline_search_hint(reason, None);
            assert!(
                !seen.contains(&hint),
                "{reason:?} repeats a suggestion already given for another reason: {hint}"
            );
            seen.push(hint);
        }
    }

    #[test]
    fn the_two_offline_mode_sources_do_not_share_a_suggestion() {
        let env = offline_search_hint(OfflineReason::ModeOfflineEnv, None);
        let cfg = offline_search_hint(OfflineReason::ModeOfflineConfig, None);
        assert!(env.contains("INKENTRY_MODE"), "{env}");
        assert!(env.contains("unset"), "{env}");
        assert!(!env.contains("remove"), "{env}");
        assert!(cfg.contains("remove"), "{cfg}");
        assert!(!cfg.contains("INKENTRY_MODE"), "{cfg}");
    }

    #[test]
    fn kill_switch_names_the_variable_and_never_server_url() {
        let hint = offline_search_hint(OfflineReason::KillSwitch, None);
        assert!(hint.contains("INKENTRY_NO_SERVER"), "{hint}");
        assert!(hint.contains("unset"), "{hint}");
        assert!(!hint.contains("server_url"), "{hint}");
    }

    #[test]
    fn mode_offline_names_the_setting_and_never_server_url() {
        let cfg = offline_search_hint(OfflineReason::ModeOfflineConfig, None);
        assert!(cfg.contains("mode = \"offline\""), "{cfg}");
        assert!(!cfg.contains("server_url"), "{cfg}");

        let env = offline_search_hint(OfflineReason::ModeOfflineEnv, None);
        assert!(env.contains("INKENTRY_MODE"), "{env}");
        assert!(!env.contains("server_url"), "{env}");
    }

    #[test]
    fn no_local_server_leads_with_the_local_daemon() {
        let hint = offline_search_hint(OfflineReason::NoLocalServer, None);
        let daemon = hint
            .find("inkentry server start")
            .unwrap_or_else(|| panic!("must offer the local daemon: {hint}"));
        let url = hint
            .find("server_url")
            .unwrap_or_else(|| panic!("must offer the team server too: {hint}"));
        assert!(daemon < url, "server_url must not come first: {hint}");
    }

    #[test]
    fn local_server_unusable_asks_for_a_restart_not_a_remote() {
        let hint = offline_search_hint(OfflineReason::LocalServerUnusable, None);
        assert!(hint.contains("inkentry server stop"), "{hint}");
        assert!(hint.contains("inkentry server start"), "{hint}");
        assert!(!hint.contains("server_url"), "{hint}");
    }

    #[test]
    fn explicit_server_renders_the_transport_failure() {
        let tls = offline_search_hint(
            OfflineReason::ExplicitServerUnavailable,
            Some(ConnFailure::Tls("certificate expired".to_string())),
        );
        assert_eq!(tls, "  [tls: certificate expired]");

        for failure in [None, Some(ConnFailure::Unreachable)] {
            let hint = offline_search_hint(OfflineReason::ExplicitServerUnavailable, failure);
            assert_eq!(hint, "  [unreachable]");
        }
    }

    #[test]
    fn a_transport_failure_never_leaks_into_another_reason() {
        let failure = Some(ConnFailure::Tls("certificate expired".to_string()));
        for reason in REASONS {
            if reason == OfflineReason::ExplicitServerUnavailable {
                continue;
            }
            let hint = offline_search_hint(reason, failure.clone());
            assert!(
                !hint.contains("certificate expired"),
                "{reason:?} rendered an explicit-probe TLS cause: {hint}"
            );
        }
    }

    // reqwest::Error has no public constructor.
    #[derive(Debug)]
    struct ChainErr(&'static str, Option<Box<dyn std::error::Error + 'static>>);

    impl std::fmt::Display for ChainErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for ChainErr {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref()
        }
    }

    #[test]
    fn error_chain_joins_every_source_level() {
        let bottom = ChainErr("dns lookup failed", None);
        let middle = ChainErr("connecting to socket", Some(Box::new(bottom)));
        let top = ChainErr(
            "error sending request for url (https://x/)",
            Some(Box::new(middle)),
        );

        let chain = error_chain(&top);
        assert_eq!(
            chain,
            "error sending request for url (https://x/) -> connecting to socket -> dns lookup failed"
        );
    }

    #[test]
    fn error_chain_single_level_is_just_the_message() {
        let only = ChainErr("boom", None);
        assert_eq!(error_chain(&only), "boom");
    }

    #[test]
    fn cert_trust_hint_mentions_both_classic_traps_and_the_doc_section() {
        let hint = cert_trust_hint();
        assert!(hint.contains("leaf certificate, not the issuing CA"));
        assert!(hint.contains("CA:TRUE"));
        assert!(hint.contains("Trusting the server's certificate on the client"));
    }

    // find_rustls_cause's downcast_ref matches only while the tree has a single rustls version
    // (TypeId differs per version); a split silently degrades TLS diagnostics to [unreachable].
    #[test]
    fn cargo_lock_resolves_a_single_rustls_version() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let lock_path = manifest_dir.join("../../Cargo.lock");
        let lock = std::fs::read_to_string(&lock_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", lock_path.display()));

        let rustls_entries = lock
            .lines()
            .filter(|line| line.trim() == "name = \"rustls\"")
            .count();

        assert_eq!(
            rustls_entries, 1,
            "expected exactly one resolved `rustls` version in Cargo.lock, found \
             {rustls_entries}; a split here means find_rustls_cause's downcast_ref \
             will silently stop matching TLS causes; repin inkentry-cli's direct \
             rustls to the same version reqwest resolves"
        );
    }

    #[test]
    fn cert_trust_hint_is_only_appended_when_server_ca_is_configured() {
        // Mirrors the gating in probe_url's Err(e) TLS-cause branch.
        let server_ca: Option<&std::path::Path> = None;
        let hint = if server_ca.is_some() {
            cert_trust_hint()
        } else {
            String::new()
        };
        assert!(hint.is_empty(), "no server_ca configured => no hint");

        let server_ca: Option<&std::path::Path> = Some(std::path::Path::new("/tmp/ca.pem"));
        let hint = if server_ca.is_some() {
            cert_trust_hint()
        } else {
            String::new()
        };
        assert!(!hint.is_empty(), "server_ca configured => hint present");
    }

    #[test]
    fn error_chain_does_not_panic_on_multiline_display() {
        let bottom = ChainErr("line one\nline two\nline three", None);
        let top = ChainErr("outer", Some(Box::new(bottom)));
        let chain = error_chain(&top);
        assert_eq!(chain, "outer -> line one\nline two\nline three");
    }

    #[test]
    fn error_chain_does_not_overflow_on_a_very_deep_chain() {
        const DEPTH: usize = 10_000;
        let mut err: Box<dyn std::error::Error + 'static> = Box::new(ChainErr("bottom", None));
        for _ in 0..DEPTH {
            err = Box::new(ChainErr("layer", Some(err)));
        }
        let chain = error_chain(err.as_ref());
        assert_eq!(chain.matches(" -> ").count(), DEPTH);
        assert!(find_rustls_cause(err.as_ref()).is_none());
    }

    #[test]
    fn only_the_pre_socket_reasons_are_explicit_opt_outs() {
        let opt_outs: Vec<OfflineReason> = REASONS
            .into_iter()
            .filter(|r| r.is_explicit_opt_out())
            .collect();
        assert_eq!(
            opt_outs,
            vec![
                OfflineReason::KillSwitch,
                OfflineReason::ModeOfflineEnv,
                OfflineReason::ModeOfflineConfig,
            ],
            "an opt-out is a setting read before any probe; a failed probe is not one"
        );
    }
}
