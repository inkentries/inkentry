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
pub struct ApiKeyAuth {
    /// `None` → accept all requests (no key configured; safe on a local loopback).
    key: Option<String>,
}

impl ApiKeyAuth {
    /// Construct from an explicit key value.
    pub fn new(key: Option<String>) -> Self {
        Self { key }
    }

    /// Construct from the `SPELUNK_SERVER_KEY` environment variable.
    pub fn from_env() -> Self {
        Self {
            key: std::env::var("SPELUNK_SERVER_KEY").ok(),
        }
    }
}

#[async_trait::async_trait]
impl AuthProvider for ApiKeyAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthContext, AuthError> {
        match &self.key {
            None => Ok(AuthContext {
                principal: Principal::ApiKey(String::new()),
            }),
            Some(expected) => {
                let provided = headers
                    .get("Authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "));
                match provided {
                    Some(t) if t == expected => Ok(AuthContext {
                        principal: Principal::ApiKey(t.to_owned()),
                    }),
                    _ => Err(AuthError("Unauthorized".into())),
                }
            }
        }
    }
}
