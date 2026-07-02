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

    /// Construct from the `SPELUNK_SERVER_KEY` environment variable.
    pub fn from_env() -> Self {
        Self::new(std::env::var("SPELUNK_SERVER_KEY").ok())
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
}
