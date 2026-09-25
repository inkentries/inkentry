use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::Serialize;
use uuid::Uuid;

use crate::cli::cmd::auth_api;
use crate::config::Config;
use inkentry_core::config::AuthTokens;
use inkentry_core::config::secret_store::SecretStore;

// Slugs contain `/` (`github.com/owner/repo`); raw in the path they split the
// segment and 404 in axum. Encoding keeps the slug in one `{project_id}` segment,
// which axum decodes back unchanged. Must include `/`.
const PROJECT_ID_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/')
    .add(b'%');

pub(crate) fn encode_project_id(project_id: &str) -> String {
    utf8_percent_encode(project_id, PROJECT_ID_SEGMENT).to_string()
}

#[derive(Serialize)]
struct LlmMsg<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct LlmCompleteReq<'a> {
    messages: Vec<LlmMsg<'a>>,
    max_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    json_schema: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct EmbedChunkIn<'a> {
    chunk_id: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct EmbedReq<'a> {
    chunks: Vec<EmbedChunkIn<'a>>,
}

pub struct LlmMessage {
    pub role: String,
    pub content: String,
}

impl LlmMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
}

// LLM and embed routing resolve independently, so a command needing both builds
// two clients.
pub struct ServerInferenceClient {
    client: reqwest::Client,
    base_url: String,
    project_id: String,
    // True when `base_url` came from an explicit team `server_url` rather than
    // loopback auto-discovery; errors must then name it, since `inkentry server
    // logs` reads only the local daemon's log.
    is_explicit_remote: bool,
    // A plain `Mutex` suffices: refresh happens at most once per request.
    auth: Mutex<BearerState>,
}

struct BearerState {
    bearer: Option<String>,
    // Present only when the bearer is a cached WorkOS session; a per-origin
    // server key cannot be refreshed.
    refresh: Option<RefreshState>,
}

struct RefreshState {
    tokens: AuthTokens,
    workos_url: String,
    client_id: String,
    // Rotated sessions go into the org's own slot, leaving the active pointer
    // and sibling orgs untouched. `Arc` so a test can share the store.
    store: Arc<dyn SecretStore>,
}

impl ServerInferenceClient {
    // An auto-discovered loopback server sets `inference_url` while leaving
    // `server_url` unset, so inference reaches it though memory stays local. The
    // bearer is per-origin: a self-hosted server never gets a cloud session token.
    pub fn from_config(cfg: &Config) -> Option<Self> {
        let store: Arc<dyn SecretStore> = Arc::from(
            inkentry_core::config::default_secret_store().expect("resolving the secret store"),
        );
        Self::from_config_with_arc_store(cfg, store)
    }

    // One store backs bearer resolution, the cloud session and the refresh write-back.
    fn from_config_with_arc_store(cfg: &Config, store: Arc<dyn SecretStore>) -> Option<Self> {
        let base_url = cfg
            .resolve_inference_url()?
            .trim_end_matches('/')
            .to_string();
        let bearer = cfg
            .bearer_for_with_store(&base_url, store.as_ref())
            .expect("resolving per-server bearer credential");
        let session = cfg
            .cloud_session_with_store(store.as_ref())
            .expect("resolving the cached cloud session");
        Some(Self::build(cfg, base_url, bearer, session, store))
    }

    // `from_config` infers "explicit remote" from the inference target being unset,
    // which fails once LLM routing points that target at `server_url`.
    pub fn from_config_explicit_remote(cfg: &Config) -> Option<Self> {
        let mut client = Self::from_config(cfg)?;
        client.is_explicit_remote = true;
        Some(client)
    }

    #[cfg(test)]
    fn from_config_with_store(cfg: &Config, store: Arc<dyn SecretStore>) -> Option<Self> {
        Self::from_config_with_arc_store(cfg, store)
    }

    fn build(
        cfg: &Config,
        base_url: String,
        bearer: Option<String>,
        session: Option<AuthTokens>,
        store: Arc<dyn SecretStore>,
    ) -> Self {
        if let Err(msg) = inkentry_core::config::validate_transport_url(&base_url) {
            // Fail immediately rather than send a bearer in the clear; no opt-out.
            eprintln!("error: {msg}");
            std::process::exit(2);
        }
        let project_id = cfg.project_id.clone().unwrap_or_default();
        // Loopback needs a connect bound too: a firewall that drops rather than
        // rejects leaves the SYN unanswered until the 300s budget, and
        // `cloud_first` commonly targets a loopback team server.
        let client = inkentry_core::config::apply_server_ca(
            reqwest::Client::builder(),
            cfg.server_ca.as_deref().map(std::path::Path::new),
        )
        .expect("applying custom CA for server inference")
        .connect_timeout(inkentry_core::config::REMOTE_CONNECT_TIMEOUT)
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .expect("building HTTP client for server inference");

        // Only a bearer from the cached cloud session is refreshable; a server key or env token is not.
        let refresh = session
            .filter(|a| Some(a.access_token.as_str()) == bearer.as_deref())
            .map(|tokens| RefreshState {
                tokens,
                workos_url: auth_api::workos_url(),
                client_id: auth_api::workos_client_id(auth_api::DEFAULT_CLOUD_URL),
                store,
            });

        Self {
            client,
            base_url,
            project_id,
            // Mirrors `resolve_inference_url`'s fallback: `base_url` came from
            // `server_url` iff `inference_url` was unset. Under `local_first` both
            // are set and `server_url` is only a sync replica.
            is_explicit_remote: cfg.inference_url.is_none() && cfg.server_url.is_some(),
            auth: Mutex::new(BearerState { bearer, refresh }),
        }
    }

    #[cfg(test)]
    fn for_test(
        base_url: &str,
        project_id: &str,
        bearer: Option<String>,
        refresh: Option<(AuthTokens, String, Arc<dyn SecretStore>)>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            project_id: project_id.to_string(),
            is_explicit_remote: false,
            auth: Mutex::new(BearerState {
                bearer,
                refresh: refresh.map(|(tokens, workos_url, store)| RefreshState {
                    tokens,
                    workos_url,
                    client_id: "client_test".to_string(),
                    store,
                }),
            }),
        }
    }

    #[cfg(test)]
    fn from_config_explicit_remote_with_store(
        cfg: &Config,
        store: Arc<dyn SecretStore>,
    ) -> Option<Self> {
        let mut client = Self::from_config_with_store(cfg, store)?;
        client.is_explicit_remote = true;
        Some(client)
    }

    #[cfg(test)]
    fn with_explicit_remote(mut self) -> Self {
        self.is_explicit_remote = true;
        self
    }

    fn current_bearer(&self) -> Option<String> {
        self.auth
            .lock()
            .expect("auth mutex poisoned")
            .bearer
            .clone()
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(key) = self.current_bearer() {
            req.header("Authorization", format!("Bearer {key}"))
        } else {
            req
        }
    }

    fn access_token_expired(&self) -> bool {
        let guard = self.auth.lock().expect("auth mutex poisoned");
        guard
            .refresh
            .as_ref()
            .is_some_and(|r| r.tokens.is_expired())
    }

    async fn refresh_access_token(&self) -> Result<bool> {
        let (refresh_token, org_id, cloud_origin, workos_url, client_id, store) = {
            let guard = self.auth.lock().expect("auth mutex poisoned");
            match &guard.refresh {
                Some(r) => (
                    r.tokens.refresh_token.clone(),
                    r.tokens.org_id.clone(),
                    r.tokens.cloud_origin.clone(),
                    r.workos_url.clone(),
                    r.client_id.clone(),
                    r.store.clone(),
                ),
                None => return Ok(false),
            }
        };

        // Re-send the active org so a prior `org switch` survives rotation.
        let rotated = auth_api::refresh_token(
            &self.client,
            &workos_url,
            &client_id,
            &refresh_token,
            auth_api::org_id_for_refresh(&org_id),
        )
        .await
        .map_err(|e| {
            e.context("session expired and token refresh failed — re-run `inkentry login`")
        })?;
        let new_tokens = rotated.into_auth_tokens(cloud_origin);

        inkentry_core::config::org_tokens::update_in_place(store.as_ref(), &new_tokens)
            .context("persisting refreshed auth tokens")?;

        let mut guard = self.auth.lock().expect("auth mutex poisoned");
        guard.bearer = Some(new_tokens.access_token.clone());
        guard.refresh = Some(RefreshState {
            tokens: new_tokens,
            workos_url,
            client_id,
            store,
        });
        Ok(true)
    }

    async fn send_authed(
        &self,
        make_req: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        if self.access_token_expired() {
            self.refresh_access_token().await?;
        }

        // Latency shortcut: another connect timeout would reach the same conclusion.
        if inkentry_core::reachability::connect_already_failed(&self.base_url) {
            anyhow::bail!(
                "{} is unreachable (a connection attempt earlier in this command already failed)",
                self.base_url
            );
        }
        let resp = self.authed(make_req()).send().await.inspect_err(|err| {
            // TLS failures are excluded: that server answered, and recording it
            // as absent would make a later request skip its own handshake and
            // report an outage instead of the certificate cause.
            if err.is_connect() && inkentry_core::config::find_rustls_cause(err).is_none() {
                inkentry_core::reachability::record_connect_failure(&self.base_url);
            }
        })?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }

        if self.refresh_access_token().await? {
            return Ok(self.authed(make_req()).send().await?);
        }
        // No refresh state: the bearer is a per-origin key or absent, so this error is where the user learns `auth set-key`.
        anyhow::bail!(
            "{base} rejected the credential ({status}). Store a key for this server with \
             `inkentry auth set-key --server {base}`, or run `inkentry login` if it is \
             inkentry cloud.",
            base = self.base_url,
            status = resp.status(),
        );
    }

    fn llm_url(&self) -> String {
        format!(
            "{}/v1/projects/{}/llm/complete",
            self.base_url,
            encode_project_id(&self.project_id)
        )
    }

    fn embed_url(&self) -> String {
        format!(
            "{}/v1/projects/{}/index/embed",
            self.base_url,
            encode_project_id(&self.project_id)
        )
    }

    fn search_url(&self) -> String {
        format!(
            "{}/v1/projects/{}/search",
            self.base_url,
            encode_project_id(&self.project_id)
        )
    }

    pub async fn llm_complete(
        &self,
        messages: &[LlmMessage],
        max_tokens: usize,
        json_schema: Option<serde_json::Value>,
    ) -> Result<String> {
        use futures_util::StreamExt;

        let body = LlmCompleteReq {
            messages: messages
                .iter()
                .map(|m| LlmMsg {
                    role: &m.role,
                    content: &m.content,
                })
                .collect(),
            max_tokens,
            json_schema,
        };

        let url = self.llm_url();
        let resp = self
            .send_authed(|| self.client.post(&url).json(&body))
            .await
            .context("POST /llm/complete")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let remote_url = self.is_explicit_remote.then_some(self.base_url.as_str());
            anyhow::bail!(
                "{}",
                server_inference_error("/llm/complete", status, &text, remote_url)
            );
        }

        let mut stream = resp.bytes_stream();
        let mut sse_buf = String::new();
        let mut output = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("reading /llm/complete SSE stream")?;
            sse_buf.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(pos) = sse_buf.find("\n\n") {
                let event = sse_buf[..pos].to_string();
                sse_buf.drain(..pos + 2);

                for line in event.lines() {
                    let data = match line.strip_prefix("data: ") {
                        Some(d) => d,
                        None => continue,
                    };
                    if data.is_empty() {
                        continue;
                    }
                    let Ok(val) = serde_json::from_str::<serde_json::Value>(data) else {
                        continue;
                    };
                    match val.get("kind").and_then(|k| k.as_str()) {
                        Some("token") => {
                            if let Some(content) = val.get("content").and_then(|c| c.as_str()) {
                                output.push_str(content);
                            }
                        }
                        Some("done") => return Ok(output),
                        Some("error") => {
                            let msg = val
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("unknown error");
                            anyhow::bail!("llm/complete stream error: {msg}");
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(output)
    }

    pub async fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        // The `query:` prefix keeps it distinguishable from real chunk ids in server logs.
        let chunk_id = format!("query:{}", Uuid::now_v7());
        let body = EmbedReq {
            chunks: vec![EmbedChunkIn {
                chunk_id: &chunk_id,
                content: text,
            }],
        };

        let url = self.embed_url();
        let resp = self
            .send_authed(|| self.client.post(&url).json(&body))
            .await
            .context("POST /index/embed (query vector)")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let remote_url = self.is_explicit_remote.then_some(self.base_url.as_str());
            anyhow::bail!(
                "{}",
                server_inference_error("/index/embed", status, &text, remote_url)
            );
        }
        let bytes = resp
            .bytes()
            .await
            .context("reading /index/embed response")?;

        let expected = inkentry_core::embeddings::EMBEDDING_DIM * 4;
        anyhow::ensure!(
            bytes.len() == expected,
            "embed response is {} bytes, expected {expected} (one {}-dim f32 vector)",
            bytes.len(),
            inkentry_core::embeddings::EMBEDDING_DIM,
        );
        Ok(inkentry_core::embeddings::blob_to_vec(&bytes))
    }

    // `None` means the server answered `mode: "text"`: no vector, so the caller falls back to FTS.
    pub async fn search_query(
        &self,
        query: &str,
        mode: &str,
        limit: usize,
    ) -> Result<Option<Vec<f32>>> {
        #[derive(serde::Serialize)]
        struct Req<'a> {
            query: &'a str,
            limit: usize,
            mode: &'a str,
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            query_vector: Option<Vec<f32>>,
            #[allow(dead_code)]
            mode: String,
        }

        let url = self.search_url();
        let req_body = Req { query, limit, mode };
        let resp: Resp = self
            .send_authed(|| self.client.post(&url).json(&req_body))
            .await
            .context("POST /search (query vector)")?
            .error_for_status()
            .context("inkentry-server returned an error for /search")?
            .json()
            .await
            .context("parsing /search response")?;

        Ok(resp.query_vector)
    }
}

// `error_for_status` discards the `{ error, state, detail }` body an unready
// server returns, so parse it here for a next-step hint. `remote_url` is set for
// an explicit `server_url`, where the hint must name that server instead of
// `inkentry server logs` (which reads only the local daemon's log).
fn server_inference_error(
    endpoint: &str,
    status: reqwest::StatusCode,
    body: &str,
    remote_url: Option<&str>,
) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let field = |k: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    };
    let reason = field("detail").or_else(|| field("error"));
    // On an explicit remote every failure must name its server, or the reader assumes their local daemon.
    let server = match remote_url {
        Some(url) => format!("inkentry-server at {url}"),
        None => "inkentry-server".to_string(),
    };
    let hint = match field("state") {
        Some("loading") => " Retry shortly (`inkentry server status`).",
        Some("unavailable") => match remote_url {
            Some(_) => " Check that server's logs.",
            None => " See `inkentry server logs`.",
        },
        _ => "",
    };
    match reason {
        Some(reason) => format!("{server} {endpoint} returned {status}: {reason}.{hint}"),
        None => format!("{server} {endpoint} returned {status}.{hint}"),
    }
}

pub fn harvest_requires_server() -> anyhow::Error {
    anyhow::anyhow!(crate::capability::inference_server_required_message(
        "harvest"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use inkentry_core::config::AuthTokens;
    use inkentry_core::config::org_tokens;
    use inkentry_core::config::secret_store::MemoryStore;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn arc_store() -> Arc<dyn SecretStore> {
        Arc::new(MemoryStore::default())
    }

    fn expiring_tokens(expires_at: i64) -> AuthTokens {
        AuthTokens {
            access_token: "at-old".to_string(),
            refresh_token: "rt-old".to_string(),
            expires_at,
            org_id: "org_1".to_string(),
            cloud_origin: auth_api::DEFAULT_CLOUD_URL.to_string(),
        }
    }

    // Unsigned JWT carrying `exp` and `org_id` for the refresh path's claim decode; the token doubles as the bearer the retry must send.
    fn jwt(label: &str, org_id: &str, exp: i64) -> String {
        fn b64url(bytes: &[u8]) -> String {
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
                out.push(A[((n >> 18) & 0x3f) as usize] as char);
                out.push(A[((n >> 12) & 0x3f) as usize] as char);
                if chunk.len() > 1 {
                    out.push(A[((n >> 6) & 0x3f) as usize] as char);
                }
                if chunk.len() > 2 {
                    out.push(A[(n & 0x3f) as usize] as char);
                }
            }
            out
        }
        let payload = serde_json::json!({ "exp": exp, "org_id": org_id, "lbl": label }).to_string();
        format!("{}.{}.sig", b64url(b"{}"), b64url(payload.as_bytes()))
    }

    #[tokio::test]
    async fn refresh_on_401_retries_once_and_persists() {
        let inference = MockServer::start().await;
        let cloud = MockServer::start().await;

        let at_new = jwt("new", "org_1", 5_000_000_000);

        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/search"))
            .and(header("authorization", "Bearer at-old"))
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(1)
            .mount(&inference)
            .await;

        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": at_new,
                "refresh_token": "rt-new",
                "organization_id": "org_1",
            })))
            .expect(1)
            .mount(&cloud)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/search"))
            .and(header("authorization", format!("Bearer {at_new}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_vector": [0.1_f32, 0.2, 0.3],
                "mode": "semantic",
            })))
            .expect(1)
            .mount(&inference)
            .await;

        let token = expiring_tokens(5_000_000_000); // not locally expired
        let store = arc_store();
        org_tokens::set_active(store.as_ref(), &token, None).unwrap();
        let client = ServerInferenceClient::for_test(
            &inference.uri(),
            "proj",
            Some("at-old".to_string()),
            Some((token, cloud.uri(), store.clone())),
        );

        let vec = client
            .search_query("hello", "semantic", 5)
            .await
            .expect("search should succeed after one refresh+retry");
        assert_eq!(vec, Some(vec![0.1_f32, 0.2, 0.3]));

        let session = org_tokens::resolve_session(store.as_ref(), None)
            .unwrap()
            .expect("rotated session cached");
        assert_eq!(session.access_token, at_new);
        assert_eq!(session.refresh_token, "rt-new");
    }

    #[tokio::test]
    async fn proactive_refresh_when_locally_expired() {
        let inference = MockServer::start().await;
        let cloud = MockServer::start().await;

        let at_fresh = jwt("fresh", "org_1", 5_000_000_000);

        // Must carry the stored org scope so rotation never reverts to the default org.
        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .and(body_string_contains("organization_id=org_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": at_fresh,
                "refresh_token": "rt-fresh",
                "organization_id": "org_1",
            })))
            .expect(1)
            .mount(&cloud)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/search"))
            .and(header(
                "authorization",
                format!("Bearer {at_fresh}").as_str(),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_vector": [1.0_f32],
                "mode": "semantic",
            })))
            .expect(1)
            .mount(&inference)
            .await;

        // expires_at = 0 ⇒ definitely past expiry.
        let token = expiring_tokens(0);
        let client = ServerInferenceClient::for_test(
            &inference.uri(),
            "proj",
            Some("at-old".to_string()),
            Some((token, cloud.uri(), arc_store())),
        );

        let vec = client.search_query("q", "semantic", 1).await.unwrap();
        assert_eq!(vec, Some(vec![1.0_f32]));
    }

    // `{:#}` is how the CLI renders an error, so it is what the user reads.
    #[tokio::test]
    async fn no_refresh_state_surfaces_401_naming_the_set_key_command() {
        let inference = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/search"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&inference)
            .await;

        let client = ServerInferenceClient::for_test(
            &inference.uri(),
            "proj",
            Some("sk-team".to_string()),
            None,
        );
        let err = format!(
            "{:#}",
            client.search_query("q", "semantic", 1).await.unwrap_err()
        );
        assert!(
            err.contains(&format!(
                "inkentry auth set-key --server {}",
                inference.uri()
            )),
            "must name the fix, got: {err}"
        );
    }

    #[tokio::test]
    async fn refresh_retry_caps_at_one_and_does_not_loop() {
        let inference = MockServer::start().await;
        let cloud = MockServer::start().await;

        // `.expect(2)` is the loop guard: a second retry would fail the test on drop.
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/search"))
            .respond_with(ResponseTemplate::new(401))
            .expect(2)
            .mount(&inference)
            .await;

        Mock::given(method("POST"))
            .and(path("/user_management/authenticate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": jwt("new", "org_1", 5_000_000_000),
                "refresh_token": "rt-new",
                "organization_id": "org_1",
            })))
            .expect(1)
            .mount(&cloud)
            .await;

        let token = expiring_tokens(5_000_000_000); // not locally expired
        let client = ServerInferenceClient::for_test(
            &inference.uri(),
            "proj",
            Some("at-old".to_string()),
            Some((token, cloud.uri(), arc_store())),
        );

        let err = client
            .search_query("q", "semantic", 1)
            .await
            .expect_err("a persistent 401 after one refresh must surface an error");
        assert!(
            err.to_string().contains("/search") || err.to_string().contains("401"),
            "error should reflect the failed /search, got: {err}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn from_config_attaches_refresh_state_for_cloud_session_bearer() {
        unsafe {
            std::env::remove_var("INKENTRY_SERVER_KEY");
        }
        let tokens = AuthTokens {
            access_token: "at-login".into(),
            refresh_token: "rt-login".into(),
            expires_at: 4_000_000_000,
            org_id: "org_1".into(),
            cloud_origin: auth_api::DEFAULT_CLOUD_URL.to_string(),
        };
        let store = arc_store();
        org_tokens::set_active(store.as_ref(), &tokens, None).unwrap();

        // A self-hosted origin never resolves to the cloud token, whatever the cache holds.
        let cfg = crate::config::Config {
            inference_url: Some(auth_api::DEFAULT_CLOUD_URL.to_string()),
            ..Default::default()
        };
        let client =
            ServerInferenceClient::from_config_with_store(&cfg, store).expect("client builds");

        let guard = client.auth.lock().unwrap();
        assert!(
            guard.refresh.is_some(),
            "a cloud-session bearer must carry refresh state so it can rotate"
        );
        assert_eq!(guard.bearer.as_deref(), Some("at-login"));
    }

    #[test]
    #[serial_test::serial]
    fn from_config_no_refresh_state_for_a_per_origin_server_key() {
        unsafe {
            std::env::remove_var("INKENTRY_SERVER_KEY");
        }
        let store = arc_store();
        inkentry_core::config::server_keys::set_key_for_origin(
            "http://127.0.0.1:4655",
            "sk-team",
            store.as_ref(),
        )
        .unwrap();
        let cfg = crate::config::Config {
            inference_url: Some("http://127.0.0.1:4655".into()),
            ..Default::default()
        };
        let client =
            ServerInferenceClient::from_config_with_store(&cfg, store).expect("client builds");

        let guard = client.auth.lock().unwrap();
        assert!(
            guard.refresh.is_none(),
            "a per-origin server key must not be treated as refreshable"
        );
        assert_eq!(guard.bearer.as_deref(), Some("sk-team"));
    }

    // Keys off whether `base_url` came from `server_url`, not the host: a
    // hand-configured loopback URL is still explicit. `cloud_first` is needed for
    // a bare `server_url` to resolve an inference target.
    #[test]
    #[serial_test::serial]
    fn from_config_is_explicit_remote_true_for_explicitly_configured_loopback_url() {
        unsafe {
            std::env::remove_var("INKENTRY_SERVER_KEY");
        }
        let cfg = crate::config::Config {
            server_url: Some("http://127.0.0.1:9797".to_string()),
            project_id: Some("proj".to_string()),
            mode: Some(inkentry_core::config::SyncMode::CloudFirst),
            ..Default::default()
        };
        // In-memory store: a real OS keychain lookup blocks indefinitely in a headless macOS session.
        let store = arc_store();
        let client =
            ServerInferenceClient::from_config_with_store(&cfg, store).expect("client builds");
        assert!(
            client.is_explicit_remote,
            "an explicitly configured server_url must count as explicit even when it is loopback"
        );
    }

    #[test]
    #[serial_test::serial]
    fn from_config_local_first_with_only_server_url_set_has_no_inference_target() {
        unsafe {
            std::env::remove_var("INKENTRY_SERVER_KEY");
        }
        let cfg = crate::config::Config {
            server_url: Some("https://api.inkentry.com".to_string()),
            project_id: Some("proj".to_string()),
            mode: None,
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_mode(),
            inkentry_core::config::SyncMode::LocalFirst
        );
        let store = arc_store();
        assert!(
            ServerInferenceClient::from_config_with_store(&cfg, store).is_none(),
            "local_first must not build an inference client aimed at a bare server_url"
        );
    }

    // Accepts TCP and then stays silent, standing in for a dropped SYN. Pinned at
    // this level because end to end the capability probe's own bound fires first
    // and masks a missing connect bound here.
    fn spawn_stalling_loopback_listener() -> u16 {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind stall listener");
        let port = listener.local_addr().expect("local_addr").port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => held.push(stream),
                    Err(_) => break,
                }
            }
        });
        port
    }

    #[tokio::test]
    #[serial_test::serial(reachability_memo)]
    async fn the_inference_client_bounds_connecting_to_a_stalled_loopback_server() {
        inkentry_core::reachability::clear_for_test();
        let port = spawn_stalling_loopback_listener();
        let cfg = crate::config::Config {
            server_url: Some(format!("https://127.0.0.1:{port}")),
            project_id: Some("proj".to_string()),
            mode: Some(inkentry_core::config::SyncMode::CloudFirst),
            ..Default::default()
        };
        let store = arc_store();
        let client = ServerInferenceClient::from_config_with_store(&cfg, store)
            .expect("cloud_first builds an inference client aimed at server_url");

        let started = std::time::Instant::now();
        let err = client
            .embed_text("anything")
            .await
            .expect_err("a stalled server must fail the embed");
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "connecting must be bounded on loopback, took {elapsed:?}: {err:#}"
        );
    }

    // `server_url` points at an unroutable host so an accidental fallback is a hard connection error.
    #[tokio::test]
    #[serial_test::serial]
    async fn embed_text_local_first_uses_loopback_not_configured_server_url() {
        unsafe {
            std::env::remove_var("INKENTRY_SERVER_KEY");
        }
        let loopback = MockServer::start().await;
        let dim = inkentry_core::embeddings::EMBEDDING_DIM;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/index/embed"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(inkentry_core::embeddings::vec_to_blob(&vec![0.25_f32; dim])),
            )
            .mount(&loopback)
            .await;

        let cfg = crate::config::Config {
            inference_url: Some(loopback.uri()),
            server_url: Some("https://cloud.invalid.example:1".to_string()),
            project_id: Some("proj".to_string()),
            mode: None, // defaults to local_first because server_url is set
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_mode(),
            inkentry_core::config::SyncMode::LocalFirst
        );

        let store = arc_store();
        let client = ServerInferenceClient::from_config_with_store(&cfg, store)
            .expect("client must build from inference_url (the loopback server)");
        assert!(
            !client.is_explicit_remote,
            "base_url resolved from inference_url, not server_url"
        );

        let vec = client.embed_text("hello").await.expect(
            "embedding must reach the local loopback server, not the unroutable \
             cloud server_url",
        );
        assert_eq!(vec.len(), dim);
    }

    #[test]
    fn encode_project_id_escapes_local_fallback_slug() {
        let slug = "local/9f2a8b3c4d5e6f70";
        let encoded = encode_project_id(slug);
        assert_eq!(encoded, "local%2F9f2a8b3c4d5e6f70");
    }

    #[test]
    fn encode_project_id_escapes_github_remote_slug() {
        let slug = "github.com/BurntSushi/jiff";
        let encoded = encode_project_id(slug);
        assert_eq!(encoded, "github.com%2FBurntSushi%2Fjiff");
    }

    #[test]
    fn encode_project_id_round_trips_through_percent_decode() {
        for slug in ["local/9f2a8b3c4d5e6f70", "github.com/BurntSushi/jiff"] {
            let encoded = encode_project_id(slug);
            let decoded = percent_encoding::percent_decode_str(&encoded)
                .decode_utf8()
                .expect("valid UTF-8 after percent-decoding");
            assert_eq!(decoded, slug, "round-trip mismatch for slug {slug:?}");
        }
    }

    #[test]
    fn encode_project_id_leaves_simple_slug_unchanged() {
        assert_eq!(encode_project_id("my-project"), "my-project");
    }

    // The cloud API declares the project id a uuid and this repo's server an
    // int64. The CLI survives that only by holding it as an opaque string; this
    // is the tripwire against tidying it into a typed id. See docs/version-skew.md.
    #[test]
    fn project_id_stays_opaque_across_both_peers_id_types() {
        let cloud_api_shaped = "550e8400-e29b-41d4-a716-446655440000";
        let oss_server_shaped = "4815162342";

        for id in [cloud_api_shaped, oss_server_shaped] {
            let encoded = encode_project_id(id);
            assert_eq!(
                encoded, id,
                "neither peer's id shape may be mangled on the way into the path"
            );
            let decoded = percent_encoding::percent_decode_str(&encoded)
                .decode_utf8()
                .expect("valid UTF-8 after percent-decoding");
            assert_eq!(decoded, id, "round-trip mismatch for project id {id:?}");
        }

        // Stops compiling if `project_id` gains a stricter type; that needs a conversation with both peers, not a cast.
        let ids: Vec<String> = [cloud_api_shaped, oss_server_shaped]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(ids.len(), 2, "both peer id shapes must be representable");
    }

    #[test]
    fn query_chunk_id_is_unique_uuid_v7() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        assert_ne!(a, b, "two query nonces must not collide");
        assert_eq!(a.get_version(), Some(uuid::Version::SortRand));
        let chunk_id = format!("query:{a}");
        assert!(chunk_id.starts_with("query:"));
    }

    // `from_config` hard-exits on an invalid URL, which would kill the test binary,
    // so the pure validator and the accepted shapes are tested instead.

    #[test]
    fn transport_validator_rejects_non_loopback_http() {
        let err = inkentry_core::config::validate_transport_url("http://team-server:4655")
            .expect_err("non-loopback http:// must be rejected");
        assert!(err.contains("loopback"));
        assert!(err.contains("https"));
    }

    #[test]
    fn transport_validator_rejects_spoofed_loopback_authorities() {
        for url in [
            "http://127.0.0.1.evil.example",
            "http://127.0.0.1@evil.example",
            "http://127.0.0.1:1234@evil.example",
        ] {
            let err = inkentry_core::config::validate_transport_url(url)
                .expect_err("a host that only looks like loopback must be rejected");
            assert!(err.contains("loopback"), "{url}: {err}");
        }
    }

    #[test]
    #[serial_test::serial]
    fn from_config_accepts_loopback_http_inference_url() {
        unsafe {
            std::env::remove_var("INKENTRY_SERVER_KEY");
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let store = arc_store();
        let mut cfg = crate::config::Config::load_with_store(Some(&path), store.as_ref()).unwrap();
        cfg.inference_url = Some("http://127.0.0.1:4655".into());
        assert!(
            ServerInferenceClient::from_config_with_store(&cfg, store).is_some(),
            "loopback http:// inference URL must be accepted"
        );
    }

    #[test]
    #[serial_test::serial]
    fn from_config_accepts_https_inference_url() {
        unsafe {
            std::env::remove_var("INKENTRY_SERVER_KEY");
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let store = arc_store();
        let mut cfg = crate::config::Config::load_with_store(Some(&path), store.as_ref()).unwrap();
        cfg.inference_url = Some("https://team-server:4655".into());
        assert!(
            ServerInferenceClient::from_config_with_store(&cfg, store).is_some(),
            "https:// inference URL (any host) must be accepted"
        );
    }

    // LLM routing sets the inference target to `server_url`, which `from_config`
    // reads as not explicit; the flag must be carried, not re-derived.
    #[test]
    #[serial_test::serial]
    fn explicit_remote_constructor_keeps_the_flag_when_the_inference_target_is_set() {
        unsafe {
            std::env::remove_var("INKENTRY_SERVER_KEY");
        }
        let cfg = crate::config::Config {
            inference_url: Some("https://team.example:4655".to_string()),
            server_url: Some("https://team.example:4655".to_string()),
            project_id: Some("proj".to_string()),
            ..Default::default()
        };
        let store = arc_store();
        assert!(
            !ServerInferenceClient::from_config_with_store(&cfg, store.clone())
                .expect("client builds")
                .is_explicit_remote,
            "the plain constructor derives the flag and cannot see through this shape"
        );
        assert!(
            ServerInferenceClient::from_config_explicit_remote_with_store(&cfg, store)
                .expect("client builds")
                .is_explicit_remote,
            "the remote LLM branch must carry the flag rather than re-derive it"
        );
    }

    #[tokio::test]
    async fn llm_complete_error_on_an_explicit_remote_names_that_server_not_local_logs() {
        let remote = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/llm/complete"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "error": "llm unavailable",
                "state": "unavailable",
                "detail": "upstream endpoint refused the connection",
            })))
            .mount(&remote)
            .await;

        let client = ServerInferenceClient::for_test(&remote.uri(), "proj", None, None)
            .with_explicit_remote();
        let err = client
            .llm_complete(&[LlmMessage::user("hi")], 16, None)
            .await
            .expect_err("a 503 must be an error");
        let msg = format!("{err:#}");

        assert!(
            msg.contains(&remote.uri()),
            "the failing server must be named: {msg}"
        );
        assert!(
            msg.contains("upstream endpoint refused the connection"),
            "the server's own reason must survive: {msg}"
        );
        assert!(
            !msg.contains("inkentry server logs"),
            "a remote failure must not point at the local daemon's log: {msg}"
        );
    }

    #[tokio::test]
    async fn llm_complete_error_on_the_loopback_still_points_at_the_local_log() {
        let loopback = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/llm/complete"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "error": "llm unavailable",
                "state": "unavailable",
                "detail": "no LLM configured",
            })))
            .mount(&loopback)
            .await;

        let client = ServerInferenceClient::for_test(&loopback.uri(), "proj", None, None);
        let err = client
            .llm_complete(&[LlmMessage::user("hi")], 16, None)
            .await
            .expect_err("a 503 must be an error");
        let msg = format!("{err:#}");
        assert!(msg.contains("inkentry server logs"), "got: {msg}");
    }

    #[test]
    fn inference_error_surfaces_loading_detail_and_retry_hint() {
        let body = serde_json::json!({
            "error": "embedder warming up, retry shortly",
            "state": "loading",
            "detail": "downloading model (42%)",
        })
        .to_string();
        let msg = server_inference_error(
            "/index/embed",
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            &body,
            None,
        );
        assert!(msg.contains("downloading model (42%)"), "got: {msg}");
        assert!(msg.contains("inkentry server status"), "got: {msg}");
        assert!(msg.contains("503"), "got: {msg}");
    }

    #[test]
    fn inference_error_surfaces_unavailable_loopback_points_at_logs() {
        let body = serde_json::json!({
            "error": "embedder unavailable",
            "state": "unavailable",
            "detail": "OOM loading GGUF",
        })
        .to_string();
        let msg = server_inference_error(
            "/index/embed",
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            &body,
            None,
        );
        assert!(msg.contains("OOM loading GGUF"), "got: {msg}");
        assert!(msg.contains("inkentry server logs"), "got: {msg}");
    }

    #[test]
    fn inference_error_surfaces_unavailable_remote_names_that_server_never_local_logs() {
        let body = serde_json::json!({
            "error": "embedder unavailable",
            "state": "unavailable",
            "detail": "OOM loading GGUF",
        })
        .to_string();
        let msg = server_inference_error(
            "/index/embed",
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            &body,
            Some("https://team.example:4655"),
        );
        assert!(msg.contains("OOM loading GGUF"), "got: {msg}");
        assert!(msg.contains("https://team.example:4655"), "got: {msg}");
        assert!(
            !msg.contains("inkentry server logs"),
            "must not point a remote failure at local logs: {msg}"
        );
    }

    #[test]
    fn inference_error_falls_back_when_body_not_json() {
        let msg = server_inference_error(
            "/index/embed",
            reqwest::StatusCode::BAD_GATEWAY,
            "<html>502</html>",
            None,
        );
        assert!(msg.contains("/index/embed"), "got: {msg}");
        assert!(msg.contains("502"), "got: {msg}");
    }

    #[tokio::test]
    async fn embed_text_surfaces_server_detail_on_503() {
        let inference = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/index/embed"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "error": "embedder warming up, retry shortly",
                "state": "loading",
                "detail": "loading F2LLM weights",
            })))
            .mount(&inference)
            .await;

        let client =
            ServerInferenceClient::for_test(&inference.uri(), "proj", Some("sk".into()), None);
        let err = client
            .embed_text("hello")
            .await
            .expect_err("503 must surface as an error");
        let msg = err.to_string();
        assert!(msg.contains("loading F2LLM weights"), "got: {msg}");
        assert!(msg.contains("inkentry server status"), "got: {msg}");
    }

    #[tokio::test]
    async fn embed_text_remote_names_that_server_never_local_logs() {
        let inference = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/index/embed"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "error": "embedder unavailable",
                "state": "unavailable",
                "detail": "OOM loading GGUF",
            })))
            .mount(&inference)
            .await;

        let client =
            ServerInferenceClient::for_test(&inference.uri(), "proj", Some("sk".into()), None)
                .with_explicit_remote();
        let err = client
            .embed_text("hello")
            .await
            .expect_err("503 must surface as an error");
        let msg = err.to_string();
        assert!(msg.contains("OOM loading GGUF"), "got: {msg}");
        assert!(msg.contains(&inference.uri()), "got: {msg}");
        assert!(
            !msg.contains("inkentry server logs"),
            "must not point a remote failure at local logs: {msg}"
        );
    }

    #[tokio::test]
    async fn inference_requests_still_carry_bearer_when_present() {
        let inference = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/search"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query_vector": [0.1_f32],
                "mode": "semantic",
            })))
            .expect(1)
            .mount(&inference)
            .await;

        let client = ServerInferenceClient::for_test(
            &inference.uri(),
            "proj",
            Some("sk-test".to_string()),
            None,
        );
        let vec = client.search_query("q", "semantic", 1).await.unwrap();
        assert_eq!(vec, Some(vec![0.1_f32]));
    }
}
