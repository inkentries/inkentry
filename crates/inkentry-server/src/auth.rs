use axum::http::HeaderMap;

/// Determines whether an incoming request is authorised and returns the
/// caller's identity. Implement this for each auth strategy.
#[async_trait::async_trait]
pub trait AuthProvider: Send + Sync + 'static {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthContext, AuthError>;
}

/// Outcome of a successful authentication check.
#[derive(Clone)]
pub struct AuthContext {
    pub principal: Principal,
}

/// Caller identity. Extensible for alternative auth strategies.
#[derive(Clone)]
pub enum Principal {
    /// Default: bearer token matched the configured key.
    ApiKey(String),
    /// Future: authenticated user identity (e.g. OAuth2).
    User { id: String },
}

/// Authentication failed. Always maps to HTTP 401.
#[derive(Debug)]
pub struct AuthError(pub String);

// ── ApiKeyAuth ─────────────────────────────────────────────────────────────────

/// OSS API-key auth provider. Checks for a `Authorization: Bearer <key>` header.
/// When no key is configured the server accepts all requests (safe on loopback).
///
/// The configured key is never held or compared as plaintext after
/// construction: it is hashed once with BLAKE3 into a fixed-length 32-byte
/// digest, and per-request comparisons hash the provided token and compare
/// the two digests in constant time, closing a timing side channel on a
/// network-exposed server.
pub struct ApiKeyAuth {
    /// `None` → accept all requests (no key configured; safe on a local loopback).
    key_hash: Option<[u8; 32]>,
}

impl ApiKeyAuth {
    /// Construct from an explicit key value.
    pub fn new(key: Option<String>) -> Self {
        Self {
            key_hash: key.map(|k| *blake3::hash(k.as_bytes()).as_bytes()),
        }
    }

    /// Construct from the `INKENTRY_SERVER_KEY` environment variable.
    pub fn from_env() -> Self {
        Self::new(std::env::var("INKENTRY_SERVER_KEY").ok())
    }
}

#[async_trait::async_trait]
impl AuthProvider for ApiKeyAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthContext, AuthError> {
        match &self.key_hash {
            None => Ok(AuthContext {
                principal: Principal::ApiKey(String::new()),
            }),
            Some(expected_hash) => {
                let provided = headers
                    .get("Authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "));
                match provided {
                    Some(t) => {
                        let provided_hash = *blake3::hash(t.as_bytes()).as_bytes();
                        if constant_time_eq::constant_time_eq_32(expected_hash, &provided_hash) {
                            Ok(AuthContext {
                                principal: Principal::ApiKey(t.to_owned()),
                            })
                        } else {
                            Err(AuthError("Unauthorized".into()))
                        }
                    }
                    None => Err(AuthError("Unauthorized".into())),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with_bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers
    }

    #[tokio::test]
    async fn accepts_correct_key() {
        let auth = ApiKeyAuth::new(Some("correct-horse-battery-staple".into()));
        let headers = headers_with_bearer("correct-horse-battery-staple");
        let result = auth.authenticate(&headers).await;
        assert!(result.is_ok());
        match result.unwrap().principal {
            Principal::ApiKey(k) => assert_eq!(k, "correct-horse-battery-staple"),
            _ => panic!("expected ApiKey principal"),
        }
    }

    #[tokio::test]
    async fn rejects_wrong_key_different_length_and_first_byte() {
        // Configured key starts with 'c' and is 29 chars; the wrong key
        // below starts with 'z' (differs at byte 0) and is a different
        // length, so neither a length short-circuit nor a first-byte
        // short-circuit would let this slip through undetected.
        let auth = ApiKeyAuth::new(Some("correct-horse-battery-staple".into()));
        let headers = headers_with_bearer("zzz-totally-wrong-key");
        let result = auth.authenticate(&headers).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn no_key_configured_accepts_all() {
        let auth = ApiKeyAuth::new(None);
        let headers = HeaderMap::new();
        let result = auth.authenticate(&headers).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn rejects_missing_authorization_header() {
        // A key is configured but the request carries no header at all —
        // distinct code path from a present-but-wrong Bearer token.
        let auth = ApiKeyAuth::new(Some("correct-horse-battery-staple".into()));
        let headers = HeaderMap::new();
        let result = auth.authenticate(&headers).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_empty_provided_token() {
        let auth = ApiKeyAuth::new(Some("correct-horse-battery-staple".into()));
        let headers = headers_with_bearer("");
        let result = auth.authenticate(&headers).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_prefix_of_real_key() {
        // A token that is an exact prefix of the configured key is the
        // canonical case a naive byte-by-byte compare would leak timing
        // information on (more matching bytes before the first mismatch).
        // Hashing first means this has no timing advantage over any other
        // wrong key.
        let auth = ApiKeyAuth::new(Some("correct-horse-battery-staple".into()));
        let headers = headers_with_bearer("correct-horse");
        let result = auth.authenticate(&headers).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_key_plus_extra_suffix() {
        // The inverse of the prefix case: provided token is the real key
        // with trailing bytes appended.
        let auth = ApiKeyAuth::new(Some("correct-horse-battery-staple".into()));
        let headers = headers_with_bearer("correct-horse-battery-staple-extra");
        let result = auth.authenticate(&headers).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_same_length_single_byte_difference() {
        // Same length as the real key, differs only in the last byte —
        // complements the existing "differs in length and first byte" test
        // by ruling out any residual length-based or leading-byte shortcut.
        let auth = ApiKeyAuth::new(Some("correct-horse-battery-staple".into()));
        let headers = headers_with_bearer("correct-horse-battery-staplE");
        let result = auth.authenticate(&headers).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_wrong_key_when_configured_key_is_empty() {
        // Degenerate config: an empty string was configured as the key.
        // This must still require an exact (empty) match, not silently
        // behave like "no key configured" (which is represented by `None`,
        // not `Some("")`).
        let auth = ApiKeyAuth::new(Some(String::new()));
        let headers = headers_with_bearer("anything");
        let result = auth.authenticate(&headers).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn accepts_empty_key_with_empty_token() {
        let auth = ApiKeyAuth::new(Some(String::new()));
        let headers = headers_with_bearer("");
        let result = auth.authenticate(&headers).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn rejects_very_long_wrong_token() {
        // Guards against any accidental length-dependent behavior
        // (e.g. panics, allocation issues) reappearing on large input,
        // since hashing must handle arbitrary-length tokens uniformly.
        let auth = ApiKeyAuth::new(Some("correct-horse-battery-staple".into()));
        let long_wrong = "x".repeat(10_000);
        let headers = headers_with_bearer(&long_wrong);
        let result = auth.authenticate(&headers).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_unicode_token_against_ascii_key() {
        let auth = ApiKeyAuth::new(Some("correct-horse-battery-staple".into()));
        let headers = headers_with_bearer("correct-hörse-bättery-staplé");
        let result = auth.authenticate(&headers).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_unicode_key_even_with_matching_unicode_token() {
        // `HeaderValue::to_str()` (called in `authenticate`) rejects any
        // non-visible-ASCII bytes per RFC 7230, so a raw UTF-8 bearer token
        // is always unauthenticated — even a byte-for-byte match against a
        // (pathological) unicode configured key never reaches the digest
        // compare. Uses `HeaderValue::from_bytes` directly since
        // `HeaderValue::from_str`/`format!` would panic first on non-ASCII
        // input, which would only test the test helper, not `authenticate`.
        let auth = ApiKeyAuth::new(Some("clé-secrète-🔑".into()));
        let mut headers = HeaderMap::new();
        headers.insert(
            "Authorization",
            HeaderValue::from_bytes("Bearer clé-secrète-🔑".as_bytes()).unwrap(),
        );
        let result = auth.authenticate(&headers).await;
        assert!(result.is_err());
    }
}
