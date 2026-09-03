use anyhow::{Context, Result};
use std::path::Path;

/// Apply the configured custom CA bundle to a reqwest client builder.
///
/// `ca_path` is the resolved [`Config::server_ca`] (env `INKENTRY_SERVER_CA`
/// precedence is already applied at load time). Adds every certificate in the
/// PEM bundle as a trust anchor **on top of** the built-in roots; certificate
/// verification stays on. A `None` path is a no-op, so every
/// team-server client site can route through this unconditionally.
pub fn apply_server_ca(
    builder: reqwest::ClientBuilder,
    ca_path: Option<&Path>,
) -> Result<reqwest::ClientBuilder> {
    let Some(path) = ca_path else {
        return Ok(builder);
    };
    let pem = std::fs::read(path)
        .with_context(|| format!("reading INKENTRY_SERVER_CA bundle at {}", path.display()))?;
    let certs = reqwest::Certificate::from_pem_bundle(&pem)
        .with_context(|| format!("parsing PEM CA bundle at {}", path.display()))?;
    // `from_pem_bundle` yields an empty vec for a file with no PEM blocks rather
    // than erroring — surface that as a config error, else a wrong path would
    // silently add no trust anchor and fail TLS with a confusing message.
    if certs.is_empty() {
        anyhow::bail!(
            "no PEM certificates found in CA bundle at {}",
            path.display()
        );
    }
    let mut builder = builder;
    for cert in certs {
        builder = builder.add_root_certificate(cert);
    }
    Ok(builder)
}

/// Walk `err`'s source chain looking for a `rustls::Error`, which is how a TLS
/// handshake/certificate failure surfaces underneath reqwest's generic
/// "error sending request". tokio-rustls reports it boxed inside an
/// `io::Error`, so both direct and `io::Error`-wrapped placements are checked
/// at each level. Returns the short cause string used for `[tls: <cause>]`,
/// or `None` when the chain carries no TLS error (a plain connect timeout or
/// refusal, i.e. genuinely `[unreachable]`).
pub fn find_rustls_cause(err: &(dyn std::error::Error + 'static)) -> Option<String> {
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
pub(crate) fn describe_rustls_error(e: &rustls::Error) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── Custom CA trust (INKENTRY_SERVER_CA / config `server_ca`) ─────────────

    /// A throwaway self-signed CA used only to prove the PEM is parsed and
    /// accepted as a trust anchor. Not trusted by anything real.
    const TEST_CA_PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\n\
MIIDFTCCAf2gAwIBAgIUdz5ZLoL+3T+MwWN0dJjElxlwsRwwDQYJKoZIhvcNAQEL\n\
BQAwGjEYMBYGA1UEAwwPc3BlbHVuay10ZXN0LWNhMB4XDTI2MDcxMzE3MjkyMFoX\n\
DTM2MDcxMDE3MjkyMFowGjEYMBYGA1UEAwwPc3BlbHVuay10ZXN0LWNhMIIBIjAN\n\
BgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAz/BAzTJJbgWUWnUqV0qFJHT+TIDT\n\
WQJbIRVBb9MezLblAGun2RG22U47jubOKoSa4DrenrEJIafd74IR9aLUdcRp6lyN\n\
WsuzY6P26ntZ1epHUjYeBgqpu71v3FK2pBvQ9PP//AhQN7apE6V4UocKd7OxbSk7\n\
g1bZSYSXoFQtSZzV9KCWNpuqUMNdaMIoy1EYY86t55jeDdpFRkiO3W5jZ6M37ekg\n\
mDq5wIOC1QHziDLWFkpBbuOxsN/admbwbsDH5301H3P25RBY12Guqsz4/lgsEuN9\n\
L+RJfs/Vdmen5wKhbPDkr8EYx7hLF0T2ZKOf0TrJojrqHkO5n4+7ESeaUwIDAQAB\n\
o1MwUTAdBgNVHQ4EFgQUJsLeVcwx4exuV//vdoLfqb5H3ZQwHwYDVR0jBBgwFoAU\n\
JsLeVcwx4exuV//vdoLfqb5H3ZQwDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0B\n\
AQsFAAOCAQEAT5lW043iyZlbYM0372z/Ec8Z3VYDZ3bvryKN+6kGYuZJJnCep2c/\n\
QX2iPx+HRWx0rz+QcnNrOdetr2KAac6ODxU2LVzjehac5wUVWm6uICzojjy84Ztn\n\
1t5Ori6kvPSbOxJbznQuC7FILxpZswOBh6qfOHNgKeGVK4OkG2069YiFI+kwMdkI\n\
d9qQF0w9nfELOC5M+ZxwP4vE/QkXLG57ZrOvKl2V4pthKSBv3LBAnh/C7X7/KC+f\n\
iwNpumIaYRGylEbxW2WVv9YsWDmTBFqEkgrmx1QPJr3FtA6eeWmZ+EJIr3ImOv/d\n\
CPBfHwWj/FUeFj+csF5QpOj+u/D1F1Kh5w==\n\
-----END CERTIFICATE-----\n";

    #[test]
    fn apply_server_ca_none_is_noop() {
        // No path → builder unchanged and still buildable.
        let client = apply_server_ca(reqwest::Client::builder(), None)
            .unwrap()
            .build();
        assert!(client.is_ok());
    }

    #[test]
    fn apply_server_ca_adds_valid_bundle() {
        let tmp = TempDir::new().unwrap();
        let ca = tmp.path().join("ca.pem");
        std::fs::write(&ca, TEST_CA_PEM).unwrap();
        // A valid PEM bundle must parse and be accepted as a trust anchor; the
        // client (verification still on) builds successfully.
        let client = apply_server_ca(reqwest::Client::builder(), Some(&ca))
            .expect("valid CA bundle should be accepted")
            .build();
        assert!(client.is_ok());
    }

    #[test]
    fn apply_server_ca_missing_file_errors() {
        let missing = Path::new("/nonexistent/inkentry-server-ca.pem");
        let err = apply_server_ca(reqwest::Client::builder(), Some(missing)).unwrap_err();
        assert!(err.to_string().contains("INKENTRY_SERVER_CA"), "got: {err}");
    }

    #[test]
    fn apply_server_ca_malformed_pem_errors() {
        let tmp = TempDir::new().unwrap();
        let ca = tmp.path().join("bad.pem");
        std::fs::write(&ca, b"not a certificate").unwrap();
        assert!(apply_server_ca(reqwest::Client::builder(), Some(&ca)).is_err());
    }

    // ── find_rustls_cause / describe_rustls_error ───────────────────────────

    // Minimal chained error, since `reqwest::Error`'s constructors are private.
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
}
