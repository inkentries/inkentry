// Token exchanges are WorkOS public-client calls (`client_id` only, no secret),
// made directly. cloud-api is used only for `GET /v1/me`.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use inkentry_core::config::{AuthTokens, Config};

pub use inkentry_core::config::server_keys::DEFAULT_CLOUD_URL;

pub const DEFAULT_WORKOS_URL: &str = "https://api.workos.com";

// Must match cloud-api's `workos_client_id` (terraform/prod.tfvars); it validates
// every access token's issuer against this client.
pub const WORKOS_CLIENT_ID_PROD: &str = "client_01M0K17J4JW69SMCQCYGZS566G";

// Must match cloud-api's `workos_client_id` (terraform/dev.tfvars).
pub const WORKOS_CLIENT_ID_DEV: &str = "client_01M0K17HRZTY5F13BC0N5ZEJVE";

const GRANT_DEVICE_CODE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const GRANT_REFRESH_TOKEN: &str = "refresh_token";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("building HTTP client")
}

pub fn workos_url() -> String {
    std::env::var("INKENTRY_WORKOS_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_WORKOS_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

pub fn workos_client_id(cloud_url: &str) -> String {
    if let Ok(v) = std::env::var("INKENTRY_WORKOS_CLIENT_ID")
        && !v.trim().is_empty()
    {
        return v;
    }
    if is_prod_cloud_url(cloud_url) {
        WORKOS_CLIENT_ID_PROD.to_string()
    } else {
        WORKOS_CLIENT_ID_DEV.to_string()
    }
}

fn is_prod_cloud_url(cloud_url: &str) -> bool {
    let host = cloud_url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = host.split(['/', ':']).next().unwrap_or(host);
    host.eq_ignore_ascii_case("api.inkentry.com")
}

#[derive(Debug, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    pub interval: u64,
}

// `id` is the local org UUID; `workos_org_id` is the provider's id, as carried in
// the access token's `org_id` claim.
#[derive(Debug, Clone, Deserialize)]
pub struct MeOrg {
    pub id: String,
    pub name: String,
    pub slug: String,
    #[serde(default)]
    pub workos_org_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MeResponse {
    #[serde(default)]
    pub orgs: Vec<MeOrg>,
}

#[derive(Debug, Clone, Deserialize)]
struct WorkosAuthResponse {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    organization_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TokenSuccess {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub org_id: String,
}

impl TokenSuccess {
    // `cloud_origin` limits where the access token is later released.
    pub fn into_auth_tokens(self, cloud_origin: String) -> AuthTokens {
        AuthTokens {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: self.expires_at,
            org_id: self.org_id,
            cloud_origin,
        }
    }
}

impl WorkosAuthResponse {
    fn into_success(self) -> TokenSuccess {
        let claims = decode_jwt_claims(&self.access_token).unwrap_or_default();
        let expires_at = claims.exp.unwrap_or(0);
        let org_id = self.organization_id.or(claims.org_id).unwrap_or_default();
        TokenSuccess {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at,
            org_id,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct JwtClaims {
    exp: Option<i64>,
    org_id: Option<String>,
}

// Unverified: WorkOS issued the token and the server re-validates it on every request.
fn decode_jwt_claims(token: &str) -> Option<JwtClaims> {
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload_b64)?;
    serde_json::from_slice(&bytes).ok()
}

// Hand-rolled to avoid a base64 dependency.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }

    let input = input.trim_end_matches('=');
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in input.as_bytes() {
        let v = val(c)?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

pub async fn initiate_device(
    client: &reqwest::Client,
    workos_url: &str,
    client_id: &str,
) -> Result<DeviceCodeResponse> {
    let resp = client
        .post(format!("{workos_url}/user_management/authorize/device"))
        .form(&[("client_id", client_id)])
        .send()
        .await
        .context("POST /user_management/authorize/device failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Device authorization request failed ({status}): {body}");
    }

    resp.json()
        .await
        .context("parsing device authorization response")
}

pub enum PollOutcome {
    Success(TokenSuccess),
    Pending,
    SlowDown,
    RateLimit,
    Expired,
    Denied,
    InvalidGrant(String),
    Challenge(Option<String>),
    Error(anyhow::Error),
}

pub async fn poll_token(
    client: &reqwest::Client,
    workos_url: &str,
    client_id: &str,
    device_code: &str,
) -> PollOutcome {
    let resp = match client
        .post(format!("{workos_url}/user_management/authenticate"))
        .form(&[
            ("client_id", client_id),
            ("grant_type", GRANT_DEVICE_CODE),
            ("device_code", device_code),
        ])
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return PollOutcome::Error(anyhow::anyhow!(e).context("network error")),
    };

    let status = resp.status();

    if status.is_success() {
        return match resp.json::<WorkosAuthResponse>().await {
            Ok(t) => PollOutcome::Success(t.into_success()),
            Err(e) => PollOutcome::Error(anyhow::anyhow!(e).context("parsing token response")),
        };
    }

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return PollOutcome::RateLimit;
    }

    let err: ErrorResponse = match resp.json().await {
        Ok(e) => e,
        Err(e) => return PollOutcome::Error(anyhow::anyhow!(e).context("parsing error response")),
    };

    match err.error.as_str() {
        "authorization_pending" => PollOutcome::Pending,
        "slow_down" => PollOutcome::SlowDown,
        "expired_token" => PollOutcome::Expired,
        "access_denied" => PollOutcome::Denied,
        "mfa_required" | "mfa_challenge" | "mfa_enrollment" | "challenge_required" => {
            // WorkOS step-up is completed browser-side; surface it non-fatally.
            PollOutcome::Challenge(None)
        }
        "invalid_grant" => {
            let msg = err
                .error_description
                .unwrap_or_else(|| "invalid_grant".to_string());
            PollOutcome::InvalidGrant(msg)
        }
        other => PollOutcome::Error(anyhow::anyhow!(
            "unexpected error from authenticate endpoint: {other}"
        )),
    }
}

// Without `organization_id` the grant reverts to the account's default org.
pub async fn refresh_token(
    client: &reqwest::Client,
    workos_url: &str,
    client_id: &str,
    refresh_token: &str,
    organization_id: Option<&str>,
) -> Result<TokenSuccess> {
    let mut form: Vec<(&str, &str)> = vec![
        ("client_id", client_id),
        ("grant_type", GRANT_REFRESH_TOKEN),
        ("refresh_token", refresh_token),
    ];
    if let Some(org) = organization_id {
        form.push(("organization_id", org));
    }

    let resp = client
        .post(format!("{workos_url}/user_management/authenticate"))
        .form(&form)
        .send()
        .await
        .context("POST /user_management/authenticate (refresh) failed")?;

    token_or_error(resp, "refreshing token").await
}

// Re-sends `auth.org_id` so an org switch survives rotation; the refresh grant
// otherwise reverts to the default org.
pub async fn ensure_fresh_token(
    client: &reqwest::Client,
    workos_url: &str,
    client_id: &str,
    auth: &AuthTokens,
    persist: impl FnOnce(&AuthTokens) -> Result<()>,
) -> Result<AuthTokens> {
    if !auth.is_expired() {
        return Ok(auth.clone());
    }

    let rotated = refresh_token(
        client,
        workos_url,
        client_id,
        &auth.refresh_token,
        org_id_for_refresh(&auth.org_id),
    )
    .await
    .map_err(|e| e.context("session expired and token refresh failed — run `inkentry login`"))?
    .into_auth_tokens(auth.cloud_origin.clone());
    persist(&rotated)?;
    Ok(rotated)
}

// An empty id (orgless account) must not be sent as an empty `organization_id` field.
pub(crate) fn org_id_for_refresh(org_id: &str) -> Option<&str> {
    (!org_id.is_empty()).then_some(org_id)
}

pub async fn ensure_fresh_server_key(cfg: &Config, server_url: &str) -> Result<Option<String>> {
    let resolved = cfg.bearer_for(server_url)?;

    // Only the WorkOS-login bearer is refreshable; a self-hosted server-key is returned as-is.
    let Some(auth) = cfg
        .cloud_session()?
        .filter(|a| Some(a.access_token.as_str()) == resolved.as_deref())
    else {
        return Ok(resolved);
    };

    if !auth.is_expired() {
        return Ok(resolved);
    }

    let client = build_client()?;
    let client_id = workos_client_id(DEFAULT_CLOUD_URL);
    let fresh = ensure_fresh_token(
        &client,
        &workos_url(),
        &client_id,
        &auth,
        inkentry_core::config::update_org_session,
    )
    .await?;
    Ok(Some(fresh.access_token))
}

pub async fn fetch_me(
    client: &reqwest::Client,
    cloud_url: &str,
    access_token: &str,
) -> Result<MeResponse> {
    let resp = client
        .get(format!("{cloud_url}/v1/me"))
        .bearer_auth(access_token)
        .send()
        .await
        .context("GET /v1/me failed")?;

    let status = resp.status();
    if status.is_success() {
        return resp
            .json::<MeResponse>()
            .await
            .context("parsing /v1/me response");
    }

    let body = resp.text().await.unwrap_or_default();
    anyhow::bail!("GET /v1/me failed ({status}): {body}");
}

// Best-effort with a short timeout: a slow or failing `/v1/me` must never delay or fail login.
pub async fn lookup_org_display_name(
    cloud_url: &str,
    access_token: &str,
    workos_org_id: &str,
) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let me = fetch_me(&client, cloud_url, access_token).await.ok()?;
    me.orgs
        .into_iter()
        .find(|o| o.workos_org_id.as_deref() == Some(workos_org_id))
        .map(|o| format!("{} ({})", o.name, o.slug))
}

async fn token_or_error(resp: reqwest::Response, ctx: &str) -> Result<TokenSuccess> {
    let status = resp.status();
    if status.is_success() {
        return resp
            .json::<WorkosAuthResponse>()
            .await
            .map(WorkosAuthResponse::into_success)
            .with_context(|| format!("parsing token response while {ctx}"));
    }

    let body = resp.text().await.unwrap_or_default();
    if let Ok(err) = serde_json::from_str::<ErrorResponse>(&body) {
        if err.error == "organization_not_found" || err.error == "org_not_member" {
            anyhow::bail!("You are not a member of the requested organization.");
        }
        anyhow::bail!("{ctx} failed ({status}): {}", err.error);
    }
    anyhow::bail!("{ctx} failed ({status}): {body}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_jwt(claims: &serde_json::Value) -> String {
        fn b64url(bytes: &[u8]) -> String {
            const ALPHABET: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
                out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
                out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
                if chunk.len() > 1 {
                    out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
                }
                if chunk.len() > 2 {
                    out.push(ALPHABET[(n & 0x3f) as usize] as char);
                }
            }
            out
        }
        let header = b64url(br#"{"alg":"none"}"#);
        let payload = b64url(serde_json::to_string(claims).unwrap().as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn base64url_decode_round_trips_jwt_payload() {
        let jwt = fake_jwt(&serde_json::json!({ "exp": 123, "org_id": "org_abc" }));
        let claims = decode_jwt_claims(&jwt).expect("claims decode");
        assert_eq!(claims.exp, Some(123));
        assert_eq!(claims.org_id.as_deref(), Some("org_abc"));
    }

    #[test]
    fn decode_jwt_claims_malformed_returns_none() {
        assert!(decode_jwt_claims("not-a-jwt").is_none());
        assert!(decode_jwt_claims("a.!!!.c").is_none());
    }

    #[test]
    fn into_success_prefers_top_level_org_then_falls_back_to_claim() {
        let resp = WorkosAuthResponse {
            access_token: fake_jwt(&serde_json::json!({ "exp": 999, "org_id": "org_claim" })),
            refresh_token: "rt".into(),
            organization_id: Some("org_top".into()),
        };
        let s = resp.into_success();
        assert_eq!(s.org_id, "org_top");
        assert_eq!(s.expires_at, 999);

        let resp = WorkosAuthResponse {
            access_token: fake_jwt(&serde_json::json!({ "exp": 1000, "org_id": "org_claim" })),
            refresh_token: "rt".into(),
            organization_id: None,
        };
        let s = resp.into_success();
        assert_eq!(s.org_id, "org_claim");
        assert_eq!(s.expires_at, 1000);
    }

    #[test]
    fn into_auth_tokens_records_the_supplied_origin() {
        let success = TokenSuccess {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 5_000_000_000,
            org_id: "org_1".into(),
        };
        let tokens = success.into_auth_tokens("https://dev.example".to_string());
        assert_eq!(tokens.cloud_origin, "https://dev.example");
    }

    #[test]
    fn workos_client_id_prod_for_canonical_host() {
        let prev = std::env::var("INKENTRY_WORKOS_CLIENT_ID").ok();
        unsafe { std::env::remove_var("INKENTRY_WORKOS_CLIENT_ID") };
        assert_eq!(
            workos_client_id("https://api.inkentry.com"),
            WORKOS_CLIENT_ID_PROD
        );
        assert_eq!(
            workos_client_id("https://dev.inkentry.com"),
            WORKOS_CLIENT_ID_DEV
        );
        assert_eq!(
            workos_client_id("http://localhost:8080"),
            WORKOS_CLIENT_ID_DEV
        );
        if let Some(v) = prev {
            unsafe { std::env::set_var("INKENTRY_WORKOS_CLIENT_ID", v) };
        }
    }

    #[test]
    fn prod_client_id_is_current_inkentry_prod_value() {
        assert_eq!(
            WORKOS_CLIENT_ID_PROD, "client_01M0K17J4JW69SMCQCYGZS566G",
            "prod client_id must track the inkentry WorkOS prod environment"
        );
    }

    #[test]
    fn is_prod_cloud_url_only_matches_canonical_host() {
        assert!(is_prod_cloud_url("https://api.inkentry.com"));
        assert!(is_prod_cloud_url("https://api.inkentry.com/"));
        assert!(is_prod_cloud_url("https://API.INKENTRY.COM"));
        assert!(!is_prod_cloud_url("https://staging.inkentry.com"));
        assert!(!is_prod_cloud_url("http://127.0.0.1:8080"));
    }

    #[test]
    fn default_cloud_url_is_recognised_as_prod() {
        assert!(
            is_prod_cloud_url(DEFAULT_CLOUD_URL),
            "DEFAULT_CLOUD_URL ({DEFAULT_CLOUD_URL}) must satisfy is_prod_cloud_url, \
             or production logins select the dev client id"
        );
    }

    #[tokio::test]
    async fn ensure_fresh_token_noop_when_valid() {
        let auth = AuthTokens {
            access_token: "at-valid".into(),
            refresh_token: "rt".into(),
            expires_at: 5_000_000_000,
            org_id: "org_1".into(),
            cloud_origin: DEFAULT_CLOUD_URL.to_string(),
        };
        let client = build_client().unwrap();
        let out = ensure_fresh_token(&client, "http://127.0.0.1:1", "client_test", &auth, |_| {
            panic!("persist must not be called when the token is still valid");
        })
        .await
        .expect("a valid token needs no refresh");
        assert_eq!(out, auth);
    }

    #[tokio::test]
    async fn ensure_fresh_token_refreshes_and_persists_when_expired() {
        use std::sync::{Arc, Mutex};
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let fresh_jwt =
            fake_jwt(&serde_json::json!({ "exp": 5_000_000_000_i64, "org_id": "org_1" }));
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .and(body_string_contains("organization_id=org_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": fresh_jwt,
                "refresh_token": "rt-rotated",
                "organization_id": "org_1",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let expired = AuthTokens {
            access_token: "at-expired".into(),
            refresh_token: "rt-old".into(),
            expires_at: 0,
            org_id: "org_1".into(),
            cloud_origin: DEFAULT_CLOUD_URL.to_string(),
        };

        let persisted: Arc<Mutex<Option<AuthTokens>>> = Arc::new(Mutex::new(None));
        let sink = persisted.clone();
        let client = build_client().unwrap();
        let out = ensure_fresh_token(&client, &server.uri(), "client_test", &expired, |t| {
            *sink.lock().unwrap() = Some(t.clone());
            Ok(())
        })
        .await
        .expect("an expired token should refresh");

        assert_eq!(out.refresh_token, "rt-rotated");
        assert_eq!(out.access_token, fresh_jwt);
        assert_eq!(
            persisted.lock().unwrap().as_ref().unwrap().refresh_token,
            "rt-rotated",
            "rotated tokens must be persisted"
        );
    }

    #[tokio::test]
    async fn ensure_fresh_token_preserves_switched_org_across_refresh() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let fresh_jwt =
            fake_jwt(&serde_json::json!({ "exp": 5_000_000_000_i64, "org_id": "org_switched" }));
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .and(body_string_contains("organization_id=org_switched"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": fresh_jwt,
                "refresh_token": "rt-rotated",
                "organization_id": "org_switched",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let expired_after_switch = AuthTokens {
            access_token: "at-expired".into(),
            refresh_token: "rt-old".into(),
            expires_at: 0,
            org_id: "org_switched".into(),
            cloud_origin: DEFAULT_CLOUD_URL.to_string(),
        };

        let client = build_client().unwrap();
        let out = ensure_fresh_token(
            &client,
            &server.uri(),
            "client_test",
            &expired_after_switch,
            |_| Ok(()),
        )
        .await
        .expect("refresh scoped to the switched org should succeed");

        assert_eq!(
            out.org_id, "org_switched",
            "the rotated token must stay scoped to the switched org, not revert to the default"
        );
    }

    #[tokio::test]
    async fn ensure_fresh_token_preserves_cloud_origin_across_refresh() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let fresh_jwt =
            fake_jwt(&serde_json::json!({ "exp": 5_000_000_000_i64, "org_id": "org_1" }));
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": fresh_jwt,
                "refresh_token": "rt-rotated",
                "organization_id": "org_1",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let expired = AuthTokens {
            access_token: "at-expired".into(),
            refresh_token: "rt-old".into(),
            expires_at: 0,
            org_id: "org_1".into(),
            cloud_origin: DEFAULT_CLOUD_URL.to_string(),
        };

        let client = build_client().unwrap();
        let out = ensure_fresh_token(&client, &server.uri(), "client_test", &expired, |_| Ok(()))
            .await
            .expect("an expired token should refresh");

        assert_eq!(
            out.cloud_origin, DEFAULT_CLOUD_URL,
            "the rotated token must keep the origin it was issued for"
        );
    }

    #[tokio::test]
    async fn ensure_fresh_token_empty_org_id_sends_plain_refresh() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        struct AssertNoOrgId;
        impl Respond for AssertNoOrgId {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                let body = String::from_utf8_lossy(&request.body);
                assert!(
                    !body.contains("organization_id"),
                    "an empty org_id must not send organization_id at all, got body: {body}"
                );
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": fake_jwt(&serde_json::json!({ "exp": 5_000_000_000_i64 })),
                    "refresh_token": "rt-rotated",
                }))
            }
        }

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(AssertNoOrgId)
            .expect(1)
            .mount(&server)
            .await;

        let expired_no_org = AuthTokens {
            access_token: "at-expired".into(),
            refresh_token: "rt-old".into(),
            expires_at: 0,
            org_id: String::new(),
            cloud_origin: DEFAULT_CLOUD_URL.to_string(),
        };

        let client = build_client().unwrap();
        ensure_fresh_token(
            &client,
            &server.uri(),
            "client_test",
            &expired_no_org,
            |_| Ok(()),
        )
        .await
        .expect("plain refresh with no org should still succeed");
    }

    #[tokio::test]
    async fn ensure_fresh_token_refresh_failure_says_run_login() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let expired = AuthTokens {
            access_token: "at-expired".into(),
            refresh_token: "rt-revoked".into(),
            expires_at: 0,
            org_id: "org_1".into(),
            cloud_origin: DEFAULT_CLOUD_URL.to_string(),
        };
        let client = build_client().unwrap();
        let err = ensure_fresh_token(&client, &server.uri(), "client_test", &expired, |_| Ok(()))
            .await
            .expect_err("a revoked refresh token must error");
        assert!(
            err.to_string().contains("inkentry login"),
            "error should tell the user to re-login, got: {err}"
        );
    }

    #[tokio::test]
    async fn initiate_device_sends_client_id_and_parses_response() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        struct AssertClientId;
        impl Respond for AssertClientId {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                let body = String::from_utf8_lossy(&request.body);
                assert!(
                    body.contains("client_id=client_test"),
                    "initiate_device must send client_id in form body, got: {body}"
                );
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "device_code": "dc-abc",
                    "user_code": "USER-CODE",
                    "verification_uri": "https://workos.example.com/activate",
                    "verification_uri_complete": "https://workos.example.com/activate?code=USER-CODE",
                    "expires_in": 900,
                    "interval": 5
                }))
            }
        }

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user_management/authorize/device"))
            .respond_with(AssertClientId)
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let resp = initiate_device(&client, &server.uri(), "client_test")
            .await
            .expect("initiate_device should succeed");

        assert_eq!(resp.device_code, "dc-abc");
        assert_eq!(resp.user_code, "USER-CODE");
        assert_eq!(resp.expires_in, 900);
        assert_eq!(resp.interval, 5);
        assert_eq!(
            resp.verification_uri_complete.as_deref(),
            Some("https://workos.example.com/activate?code=USER-CODE")
        );
    }

    #[tokio::test]
    async fn initiate_device_error_response_surfaces_status() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user_management/authorize/device"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad client"))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let err = initiate_device(&client, &server.uri(), "bad_client_id")
            .await
            .expect_err("a 400 from WorkOS must surface as an error");
        assert!(
            err.to_string().contains("400"),
            "error should mention the HTTP status, got: {err}"
        );
    }

    #[tokio::test]
    async fn poll_token_mfa_required_is_non_fatal_challenge() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "mfa_required"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let outcome = poll_token(&client, &server.uri(), "client_test", "dc-xyz").await;
        assert!(
            matches!(outcome, PollOutcome::Challenge(_)),
            "mfa_required must yield PollOutcome::Challenge (non-fatal)"
        );
    }

    #[tokio::test]
    async fn poll_token_all_mfa_codes_are_non_fatal() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for code in &["mfa_challenge", "mfa_enrollment", "challenge_required"] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/user_management/authenticate"))
                .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": code
                })))
                .expect(1)
                .mount(&server)
                .await;

            let client = build_client().unwrap();
            let outcome = poll_token(&client, &server.uri(), "client_test", "dc-xyz").await;
            assert!(
                matches!(outcome, PollOutcome::Challenge(_)),
                "error code '{code}' must yield PollOutcome::Challenge (non-fatal)"
            );
        }
    }

    #[tokio::test]
    async fn poll_token_authorization_pending_is_pending() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "authorization_pending"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let outcome = poll_token(&client, &server.uri(), "client_test", "dc-xyz").await;
        assert!(
            matches!(outcome, PollOutcome::Pending),
            "authorization_pending must yield PollOutcome::Pending"
        );
    }

    #[tokio::test]
    async fn poll_token_success_decodes_jwt_claims() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let jwt = fake_jwt(&serde_json::json!({ "exp": 4_000_000_000_i64, "org_id": "org_abc" }));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": jwt,
                "refresh_token": "rt-device",
                "organization_id": "org_abc",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let outcome = poll_token(&client, &server.uri(), "client_test", "dc-xyz").await;
        let PollOutcome::Success(tok) = outcome else {
            panic!("expected Success, got non-Success outcome");
        };
        assert_eq!(tok.refresh_token, "rt-device");
        assert_eq!(tok.org_id, "org_abc");
        assert_eq!(tok.expires_at, 4_000_000_000_i64);
    }

    #[tokio::test]
    async fn login_then_switch_org_resolves_slug_and_refreshes() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use crate::cli::cmd::org::switch_org;

        const BETA_UUID: &str = "22222222-2222-2222-2222-222222222222";
        const BETA_WORKOS: &str = "org_beta01";

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orgs": [
                    { "id": "11111111-1111-1111-1111-111111111111",
                      "name": "Acme", "slug": "acme",
                      "workos_org_id": "org_acme01", "role": "admin" },
                    { "id": BETA_UUID, "name": "Beta Corp", "slug": "beta",
                      "workos_org_id": BETA_WORKOS, "role": "member" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let beta_jwt = fake_jwt(&serde_json::json!({
            "exp": 5_000_000_000_i64,
            "org_id": BETA_WORKOS
        }));
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": beta_jwt,
                "refresh_token": "rt-beta",
                "organization_id": BETA_WORKOS,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let initial_tokens = inkentry_core::config::AuthTokens {
            access_token: "at-initial-acme".into(),
            refresh_token: "rt-initial-acme".into(),
            expires_at: 5_000_000_000,
            org_id: "11111111-1111-1111-1111-111111111111".into(),
            cloud_origin: DEFAULT_CLOUD_URL.to_string(),
        };

        let client = build_client().unwrap();
        let switched = switch_org(
            &client,
            &server.uri(), // workos_url
            &server.uri(), // cloud_url (for /v1/me)
            "client_test",
            &initial_tokens,
            "beta",
        )
        .await
        .expect("login-then-switch should succeed");

        assert_eq!(switched.org_id, BETA_WORKOS);
        assert_eq!(switched.refresh_token, "rt-beta");
        assert_eq!(switched.expires_at, 5_000_000_000_i64);
    }

    #[tokio::test]
    async fn login_then_switch_org_not_member_surfaces_clear_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use crate::cli::cmd::org::switch_org;

        const BETA_UUID: &str = "22222222-2222-2222-2222-222222222222";

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orgs": [
                    { "id": BETA_UUID, "name": "Beta Corp", "slug": "beta",
                      "workos_org_id": "org_beta01", "role": "member" }
                ]
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "organization_not_found"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let initial_tokens = inkentry_core::config::AuthTokens {
            access_token: "at-initial".into(),
            refresh_token: "rt-initial".into(),
            expires_at: 5_000_000_000,
            org_id: "11111111-1111-1111-1111-111111111111".into(),
            cloud_origin: DEFAULT_CLOUD_URL.to_string(),
        };

        let client = build_client().unwrap();
        let err = switch_org(
            &client,
            &server.uri(),
            &server.uri(),
            "client_test",
            &initial_tokens,
            "beta",
        )
        .await
        .expect_err("non-member org should error");

        assert!(
            err.to_string().contains("not a member"),
            "expected a clear membership error, got: {err}"
        );
    }

    #[tokio::test]
    async fn lookup_org_display_name_resolves_name_and_slug() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orgs": [
                    {
                        "id": "11111111-1111-1111-1111-111111111111",
                        "name": "Acme Corp",
                        "slug": "acme",
                        "workos_org_id": "org_01KVGBR276C2PN9MCH6WZ5HJ1Y",
                        "role": "admin"
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let display =
            lookup_org_display_name(&server.uri(), "at-test", "org_01KVGBR276C2PN9MCH6WZ5HJ1Y")
                .await;
        assert_eq!(
            display.as_deref(),
            Some("Acme Corp (acme)"),
            "expected resolved name+slug, got: {display:?}"
        );
    }

    #[tokio::test]
    async fn lookup_org_display_name_returns_none_when_workos_org_id_missing() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orgs": [
                    { "id": "11111111-1111-1111-1111-111111111111",
                      "name": "Acme Corp", "slug": "acme", "role": "admin" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let display =
            lookup_org_display_name(&server.uri(), "at-test", "org_01KVGBR276C2PN9MCH6WZ5HJ1Y")
                .await;
        assert!(
            display.is_none(),
            "expected None when workos_org_id not found, got: {display:?}"
        );
    }

    #[tokio::test]
    async fn lookup_org_display_name_returns_none_on_server_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .expect(1)
            .mount(&server)
            .await;

        let display =
            lookup_org_display_name(&server.uri(), "at-test", "org_01KVGBR276C2PN9MCH6WZ5HJ1Y")
                .await;
        assert!(
            display.is_none(),
            "a /v1/me error must not propagate — expected None, got: {display:?}"
        );
    }

    #[tokio::test]
    async fn lookup_org_display_name_matches_correct_org() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orgs": [
                    {
                        "id": "11111111-1111-1111-1111-111111111111",
                        "name": "Acme Corp",
                        "slug": "acme",
                        "workos_org_id": "org_AAAA",
                        "role": "admin"
                    },
                    {
                        "id": "22222222-2222-2222-2222-222222222222",
                        "name": "Beta Inc",
                        "slug": "beta",
                        "workos_org_id": "org_BBBB",
                        "role": "member"
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let display = lookup_org_display_name(&server.uri(), "at-test", "org_BBBB").await;
        assert_eq!(
            display.as_deref(),
            Some("Beta Inc (beta)"),
            "should resolve the org matching the workos_org_id, got: {display:?}"
        );
    }
}
