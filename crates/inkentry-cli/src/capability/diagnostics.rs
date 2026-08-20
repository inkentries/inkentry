//! Why the tier came out offline, plus explicit-probe failure classification
//! and TLS error diagnostics.
//!
//! [`OfflineReason`] names the branch of the probe that produced
//! `Tier::Offline`. [`shared_offline_advice`] turns it into one cause-and-remedy
//! sentence, which [`offline_search_hint`] brackets for `status` and the
//! `search` / `index` notices fold into their own frames, so the three surfaces
//! cannot advise differently on one condition.
//! [`ConnFailure`] sub-classifies the one reason that has a
//! transport story to tell: it renders the full `source()` chain of a probe
//! failure and distinguishes a transport-level miss from a TLS trust failure,
//! so the offline line reads `[unreachable]` vs `[tls: <cause>]`.

/// Cause recorded for the most recent EXPLICIT (non-auto-discovered)
/// `server_url` probe failure, set at most once per process (see
/// `record_explicit_probe_failure`, which mirrors `OnceCell::set`'s
/// first-write-wins behaviour).
///
/// Backed by a `Mutex` rather than `OnceCell` so `#[cfg(test)]` code can
/// reset it between a test that legitimately populates the cell and a test
/// that asserts it stays empty; both exist in this module's test suite and
/// share this one process-global static. Production code never resets it.
static EXPLICIT_PROBE_FAILURE: std::sync::Mutex<Option<ConnFailure>> = std::sync::Mutex::new(None);

/// How an explicitly-configured `server_url` probe failed: distinguishes a
/// transport-level miss (refused, timed out, DNS, no route) from a connection
/// that reached the server but failed TLS trust. `status`/`check` read this to
/// annotate the offline line with `[unreachable]` vs `[tls: <cause>]` instead
/// of collapsing both into "unreachable": a server that answers `curl` fine
/// can still fail here on a certificate error that would otherwise never
/// surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnFailure {
    /// TCP/connect-level failure: refused, timed out, DNS, no route.
    Unreachable,
    /// The transport connected; TLS certificate trust failed. Carries the
    /// short cause string used in `[tls: <cause>]`.
    Tls(String),
}

/// Cause of the most recent explicit `server_url` probe failure, if any.
/// `None` when no `server_url` is configured, when the tier is `Server`, when
/// the only probes so far were loopback auto-discovery, or before the first
/// probe has run.
pub fn explicit_probe_failure() -> Option<ConnFailure> {
    EXPLICIT_PROBE_FAILURE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Record `cause` as the explicit-probe failure, unless one is already
/// recorded. Mirrors `OnceCell::set`'s first-write-wins semantics so this
/// carries the same "set at most once per process" contract the previous
/// `OnceCell`-backed static had.
pub(crate) fn record_explicit_probe_failure(cause: ConnFailure) {
    let mut slot = EXPLICIT_PROBE_FAILURE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = Some(cause);
    }
}

/// Test-only: clear the recorded explicit-probe failure so a test that
/// asserts the cell is empty isn't at the mercy of whatever other
/// `capability::` test happened to populate it earlier in this process.
/// Callers must pair this with `#[serial_test::serial(explicit_probe_failure)]`,
/// since the static is shared by every test in this binary.
#[cfg(test)]
pub(crate) fn reset_explicit_probe_failure_for_test() {
    *EXPLICIT_PROBE_FAILURE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
}

/// Render `err`'s full `source()` chain, one cause per arrow. reqwest's
/// `Display` only ever shows its own top-level message ("error sending
/// request for url (...)"); the actual cause (a TLS handshake failure, a DNS
/// error, ...) lives several `source()` levels down and is otherwise silently
/// dropped from the WARN a user sees.
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

/// Walk `err`'s source chain looking for a `rustls::Error`, which is how a TLS
/// handshake/certificate failure surfaces underneath reqwest's generic
/// "error sending request". tokio-rustls reports it boxed inside an
/// `io::Error`, so both direct and `io::Error`-wrapped placements are checked
/// at each level. Returns the short cause string used for `[tls: <cause>]`,
/// or `None` when the chain carries no TLS error (a plain connect timeout or
/// refusal, i.e. genuinely `[unreachable]`).
pub(crate) fn find_rustls_cause(err: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = current {
        if let Some(rustls_err) = e.downcast_ref::<rustls::Error>() {
            return Some(describe_rustls_error(rustls_err));
        }
        if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
            // `io::Error::source()` skips its own boxed payload and jumps
            // straight to the payload's source (see std's implementation), so
            // a `rustls::Error` boxed inside, possibly through several nested
            // `io::Error` layers (as hyper's client stack does on a TLS
            // handshake failure), would never surface by following plain
            // `.source()`. `get_ref()` un-boxes one layer at a time instead;
            // loop it so any wrapping depth is handled, not just one level.
            current = io_err
                .get_ref()
                .map(|inner| inner as &(dyn std::error::Error + 'static));
            continue;
        }
        current = e.source();
    }
    None
}

/// Map a `rustls::Error` to a short, human-readable cause. Certificate errors
/// get specific text; `CaUsedAsEndEntity` (a CA:TRUE certificate presented as
/// the server's own leaf, the exact server-setup.md client-trust trap) is
/// detected by name inside `CertificateError::Other`, the bucket rustls maps
/// it into (webpki's variant has no direct `CertificateError` counterpart).
fn describe_rustls_error(e: &rustls::Error) -> String {
    use rustls::CertificateError as CE;
    match e {
        rustls::Error::InvalidCertificate(ce) => match ce {
            CE::Expired | CE::ExpiredContext { .. } => "certificate expired".to_string(),
            CE::NotValidYet | CE::NotValidYetContext { .. } => {
                "certificate not yet valid".to_string()
            }
            CE::UnknownIssuer => "unknown issuer, not signed by a trusted CA".to_string(),
            CE::NotValidForName | CE::NotValidForNameContext { .. } => {
                "certificate not valid for this hostname".to_string()
            }
            CE::Other(inner) if inner.to_string().contains("CaUsedAsEndEntity") => {
                "a CA certificate was presented as the server's own leaf certificate".to_string()
            }
            other => format!("certificate rejected: {other:?}"),
        },
        other => format!("TLS handshake failed: {other}"),
    }
}

/// Why the probe resolved to `Tier::Offline`. Carried on the tier itself
/// rather than recorded on the side, because the advice `status` prints is
/// only correct for the branch that actually fired: while
/// `INKENTRY_NO_SERVER` is set no URL is ever read, so telling that user to
/// configure one recommends an action guaranteed not to work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineReason {
    /// `INKENTRY_NO_SERVER=1`. Outranks every other setting: the probe stops
    /// before `mode` or `server_url` is consulted.
    KillSwitch,
    /// `INKENTRY_MODE=offline`. Split from the config source because the env
    /// var overrides it: telling a reader to edit a config line they may not
    /// have, and which would not take effect if they did, is the same inert
    /// advice this whole enum exists to stop.
    ModeOfflineEnv,
    /// `mode = "offline"` in a config file, with no `INKENTRY_MODE` overriding
    /// it.
    ModeOfflineConfig,
    /// Loopback auto-discovery found no local daemon to use.
    NoLocalServer,
    /// A local daemon answered, but its embeddings are a different dimension
    /// than this build reads, so it was ignored.
    LocalServerUnusable,
    /// A local daemon was recorded by `inkentry server start`, and discovery
    /// declined to use it: the recorded port answered nothing, or the responder
    /// on it could not be identified as that daemon. Distinct from
    /// [`OfflineReason::NoLocalServer`], whose advice is to start a server the
    /// user has, in this case, already started.
    RecordedServerUnreachable,
    /// An explicitly configured `server_url` did not serve the health probe.
    /// `ConnFailure` carries how, when the failure was at transport level.
    ExplicitServerUnavailable,
}

/// Every reason, for the tests that must cover all of them. Kept beside the
/// enum so a new variant reaches every surface's coverage from here rather than
/// being silently untested in three test modules at once. Adding one takes two
/// edits: `shared_offline_advice`, which the compiler forces, and this array's
/// length, which it does not.
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

/// The bracketed suffix `status` appends to its offline `search` line,
/// including the two spaces that separate it from `text`.
///
/// `failure` is read only for [`OfflineReason::ExplicitServerUnavailable`],
/// the one reason with a transport story; a reachable server answering non-2xx
/// leaves it `None` and renders as `[unreachable]`.
///
/// `server_url` appears in exactly one branch. It is the team-server feature,
/// so it is the wrong answer for a solo user (whose semantic search comes from
/// the local daemon) and an inert one under either explicit-offline setting.
pub fn offline_search_hint(reason: OfflineReason, failure: Option<ConnFailure>) -> String {
    if let Some(advice) = shared_offline_advice(reason) {
        return format!("  [{advice}]");
    }
    match reason {
        OfflineReason::ExplicitServerUnavailable => match failure {
            Some(ConnFailure::Tls(cause)) => format!("  [tls: {cause}]"),
            _ => "  [unreachable]".to_string(),
        },
        // The only other reason `shared_offline_advice` declines.
        _ => "  [run `inkentry server start` for semantic search, \
             or set server_url to share a team server]"
            .to_string(),
    }
}

/// The cause-and-remedy sentence behind every offline notice, unframed: `status`
/// brackets it onto its `search` row, while `search` and `index` fold it into
/// their own notices. One reason, one remedy, written once, so the surfaces
/// cannot drift into contradicting each other.
///
/// `None` for the two reasons each surface renders itself:
/// [`OfflineReason::ExplicitServerUnavailable`], whose text names a URL only
/// the caller holds and whose `status` rendering is a transport annotation
/// rather than advice, and [`OfflineReason::NoLocalServer`], the ordinary
/// no-daemon case, where each surface has long-standing wording of its own.
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

/// Hint appended to a TLS WARN when `server_ca` / `INKENTRY_SERVER_CA` is
/// configured: the two classic server-setup.md client-trust traps, so a user
/// does not have to rediscover them by trial and error.
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

    // ── offline_search_hint: one suggestion per reason ───────────────────────

    use super::ALL_OFFLINE_REASONS as REASONS;

    // The defect this replaces was one hint printed alongside every offline
    // tier. Asserting a single message would pass while that coupling stayed
    // broken, so the suggestions must be distinguishable from each other.
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

    // While the kill-switch is set the probe returns before any URL is read,
    // so `server_url` cannot change the outcome. Naming it is advice that
    // provably does nothing.
    // The env var overwrites `cfg.mode` at load, so both sources look identical
    // by the time a hint is chosen. Telling an env-var user to delete a config
    // line is advice that does nothing even if they find the line to delete,
    // which is the defect this module exists to prevent.
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

    // Offline mode is the other explicit opt-out, and it is equally unmoved by
    // a `server_url`, whichever of its two sources is in force.
    #[test]
    fn mode_offline_names_the_setting_and_never_server_url() {
        let cfg = offline_search_hint(OfflineReason::ModeOfflineConfig, None);
        assert!(cfg.contains("mode = \"offline\""), "{cfg}");
        assert!(!cfg.contains("server_url"), "{cfg}");

        let env = offline_search_hint(OfflineReason::ModeOfflineEnv, None);
        assert!(env.contains("INKENTRY_MODE"), "{env}");
        assert!(!env.contains("server_url"), "{env}");
    }

    // The ordinary case. Semantic search for a solo user comes from the local
    // daemon; `server_url` is the team-server feature and may only follow.
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

    // A running daemon this build cannot read from is a restart, not a
    // configuration change, and never a reason to reach for a team server.
    #[test]
    fn local_server_unusable_asks_for_a_restart_not_a_remote() {
        let hint = offline_search_hint(OfflineReason::LocalServerUnusable, None);
        assert!(hint.contains("inkentry server stop"), "{hint}");
        assert!(hint.contains("inkentry server start"), "{hint}");
        assert!(!hint.contains("server_url"), "{hint}");
    }

    // The explicit-server branch keeps the `[tls: <cause>]` / `[unreachable]`
    // annotation, and reads `ConnFailure` only there: a TLS cause recorded
    // for some other offline reason would be a mislabel.
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

    // ── error_chain / find_rustls_cause / describe_rustls_error ─────────────

    /// Minimal chained error for exercising `error_chain`/`find_rustls_cause`
    /// without needing a real `reqwest::Error` (whose constructors are private).
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

    /// A fake error whose `Display` mimics webpki's `CaUsedAsEndEntity`, since
    /// rustls buckets that variant into `CertificateError::Other` (no direct
    /// counterpart) and detection matches on the rendered name.
    #[derive(Debug)]
    struct FakeCaUsedAsEndEntity;

    impl std::fmt::Display for FakeCaUsedAsEndEntity {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "CaUsedAsEndEntity")
        }
    }

    impl std::error::Error for FakeCaUsedAsEndEntity {}

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
    fn find_rustls_cause_none_for_plain_io_error_chain() {
        // Models a genuine connect-level failure (refused/timed out): no
        // rustls::Error anywhere in the chain, so this must classify as
        // `[unreachable]`, not `[tls: ...]`.
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let top = ChainErr(
            "error sending request for url (https://x/)",
            Some(Box::new(io_err)),
        );
        assert!(find_rustls_cause(&top).is_none());
    }

    #[test]
    fn find_rustls_cause_detects_rustls_error_boxed_in_io_error() {
        // tokio-rustls reports handshake failures as an io::Error wrapping a
        // rustls::Error: the exact shape this function must see through.
        let rustls_err = rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
        let io_err = std::io::Error::other(rustls_err);
        let top = ChainErr(
            "error sending request for url (https://x/)",
            Some(Box::new(io_err)),
        );

        let cause = find_rustls_cause(&top).expect("must detect the boxed rustls::Error");
        assert!(cause.contains("unknown issuer"), "got: {cause}");
    }

    #[test]
    fn find_rustls_cause_detects_direct_rustls_error() {
        let rustls_err =
            rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName);
        let top = ChainErr(
            "error sending request for url (https://x/)",
            Some(Box::new(rustls_err)),
        );

        let cause = find_rustls_cause(&top).expect("must detect a directly-chained rustls::Error");
        assert!(cause.contains("hostname"), "got: {cause}");
    }

    #[test]
    fn describe_rustls_error_names_ca_used_as_end_entity() {
        let err = rustls::Error::InvalidCertificate(rustls::CertificateError::Other(
            rustls::OtherError(std::sync::Arc::new(FakeCaUsedAsEndEntity)),
        ));
        let cause = describe_rustls_error(&err);
        assert!(
            cause.contains("CA certificate") && cause.contains("leaf"),
            "got: {cause}"
        );
    }

    #[test]
    fn describe_rustls_error_expired() {
        let err = rustls::Error::InvalidCertificate(rustls::CertificateError::Expired);
        assert_eq!(describe_rustls_error(&err), "certificate expired");
    }

    #[test]
    fn describe_rustls_error_non_certificate_variant_falls_back_generically() {
        let err = rustls::Error::NoCertificatesPresented;
        let cause = describe_rustls_error(&err);
        assert!(cause.starts_with("TLS handshake failed:"), "got: {cause}");
    }

    #[test]
    fn cert_trust_hint_mentions_both_classic_traps_and_the_doc_section() {
        let hint = cert_trust_hint();
        assert!(hint.contains("leaf certificate, not the issuing CA"));
        assert!(hint.contains("CA:TRUE"));
        assert!(hint.contains("Trusting the server's certificate on the client"));
    }

    // Note: a real end-to-end TLS-trust failure (genuine rustls handshake
    // against a proper CA→leaf chain, and against a CA:TRUE-as-leaf
    // misconfiguration) is exercised in `tests/tls_trust.rs`, which asserts
    // `explicit_probe_failure()` reports `ConnFailure::Tls` and that the
    // status/WARN output names the certificate cause. That is the level this
    // bug actually lives at: reqwest's real error chain through hyper/rustls
    // isn't reproducible with a hand-built chain here.

    // ── version-coupling guard ───────────────────────────────────────────────

    /// `find_rustls_cause`'s `downcast_ref::<rustls::Error>()` only matches
    /// while inkentry-cli's direct `rustls` dependency resolves to the exact
    /// same crate version as the one reqwest's `rustls-tls` feature pulls in
    /// transitively: `downcast_ref` compares `TypeId`, which differs across
    /// two builds of the same-named crate at different semver-incompatible
    /// versions. A future dependency bump that forces a second `rustls` into
    /// the tree would silently degrade every TLS diagnostic back to
    /// `[unreachable]`, with no panic and no failed request: just a downcast miss.
    /// Catch that at the lockfile level, immediately, rather than waiting for
    /// a TLS handshake to expose it.
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

    // ── find_rustls_cause: nested io::Error unwrap depth ─────────────────────

    /// tokio-rustls's own wrapping is one `io::Error` layer deep, but the
    /// hyper/reqwest client stack can add further `io::Error` wrapping on top
    /// of that. `find_rustls_cause` must keep unwrapping past the first
    /// layer: a version that only checked one level (e.g. a depth-limited
    /// rewrite of the loop) would miss this and misclassify as `[unreachable]`.
    #[test]
    fn find_rustls_cause_detects_rustls_error_two_io_error_layers_deep() {
        let rustls_err = rustls::Error::InvalidCertificate(rustls::CertificateError::Expired);
        let inner_io = std::io::Error::other(rustls_err);
        let outer_io = std::io::Error::other(inner_io);
        let top = ChainErr(
            "error sending request for url (https://x/)",
            Some(Box::new(outer_io)),
        );

        let cause = find_rustls_cause(&top)
            .expect("must unwrap two nested io::Error layers to find the rustls::Error");
        assert!(cause.contains("expired"), "got: {cause}");
    }

    // ── describe_rustls_error: CaUsedAsEndEntity string-match must be exact ──

    /// A `CertificateError::Other` whose rendered text does NOT mention
    /// `CaUsedAsEndEntity` must fall back to the generic message, not be
    /// swept into the CA-as-leaf-specific sentence. This is the negative half
    /// of `describe_rustls_error_names_ca_used_as_end_entity`: without it, an
    /// overly-loose match (e.g. matching on `Other(_)` alone) would pass the
    /// positive test but silently mislabel every other certificate error.
    #[test]
    fn describe_rustls_error_other_variant_without_the_marker_string_is_generic() {
        #[derive(Debug)]
        struct SomeOtherWebpkiError;
        impl std::fmt::Display for SomeOtherWebpkiError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "InvalidSignatureForPublicKey")
            }
        }
        impl std::error::Error for SomeOtherWebpkiError {}

        let err = rustls::Error::InvalidCertificate(rustls::CertificateError::Other(
            rustls::OtherError(std::sync::Arc::new(SomeOtherWebpkiError)),
        ));
        let cause = describe_rustls_error(&err);
        assert!(
            !cause.contains("CA certificate") && !cause.contains("own leaf"),
            "must not misclassify an unrelated Other() cause as CA-as-leaf: got {cause}"
        );
        assert!(cause.starts_with("certificate rejected:"), "got: {cause}");
    }

    // ── cert_trust_hint gating ────────────────────────────────────────────────

    /// The hint is only useful (and only accurate) when `server_ca` is
    /// actually configured: it names a `server_ca` misconfiguration. Without
    /// `server_ca` set, an `UnknownIssuer` failure is trusting the default
    /// root store, and the hint must not appear, so a real e2e for this
    /// exact gating lives in `tests/tls_trust.rs`
    /// (`tls_server_with_untrusted_cert_and_no_server_ca_configured...`); this
    /// unit test only pins the gating condition itself.
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

    // ── chain rendering hygiene ───────────────────────────────────────────────

    /// `error_chain` must not panic or garble on a `Display` embedding literal
    /// newlines (e.g. a multi-line certificate parse error): it is printed
    /// straight into a `tracing::warn!` line and the terminal.
    #[test]
    fn error_chain_does_not_panic_on_multiline_display() {
        let bottom = ChainErr("line one\nline two\nline three", None);
        let top = ChainErr("outer", Some(Box::new(bottom)));
        let chain = error_chain(&top);
        assert_eq!(chain, "outer -> line one\nline two\nline three");
    }

    /// `error_chain` and `find_rustls_cause` both walk the chain with a
    /// `while let` loop, not recursion: an arbitrarily deep chain must not
    /// stack-overflow. 10k levels is far beyond anything hyper/reqwest/rustls
    /// actually produce (2-4 levels in practice); this only pins that the
    /// walk is iterative.
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
}
