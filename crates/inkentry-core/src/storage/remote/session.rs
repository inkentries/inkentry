//! The bearer a remote memory backend sends, and its renewal.
//!
//! A WorkOS access token lives about five minutes, so a cloud session stored at
//! login is stale for most commands that use it. Rotating it takes the WorkOS
//! client, which lives in inkentry-cli; inkentry-core cannot depend on that, so
//! the CLI installs a [`SessionRefresher`] once at startup and every remote
//! backend opened afterwards consults it.

use std::future::Future;
use std::sync::{Arc, OnceLock};

use anyhow::Result;
use async_trait::async_trait;

use crate::config::Config;

/// Supplies and renews the bearer for a server origin.
///
/// Implementations own origin scoping (ADR-071 D2, ADR-095): only a cloud
/// session issued for `server_url`'s origin is ever rotated; a self-hosted
/// server key or an `INKENTRY_SERVER_KEY` override passes through unchanged.
#[async_trait]
pub trait SessionRefresher: Send + Sync {
    /// The bearer for `server_url`, rotated first when it is an expired cloud
    /// session.
    async fn current(&self, cfg: &Config, server_url: &str) -> Result<Option<String>>;

    /// A replacement for `rejected`, which `server_url` just answered `401`
    /// to. `None` when `rejected` is not a cloud session this can rotate.
    async fn replace_rejected(
        &self,
        cfg: &Config,
        server_url: &str,
        rejected: &str,
    ) -> Result<Option<String>>;
}

static REFRESHER: OnceLock<Arc<dyn SessionRefresher>> = OnceLock::new();

/// Install the process-wide refresher every remote memory backend opened after
/// this call uses. Only the first installation takes effect.
pub fn install_session_refresher(refresher: Arc<dyn SessionRefresher>) {
    let _ = REFRESHER.set(refresher);
}

pub(in crate::storage) fn installed_refresher() -> Option<Arc<dyn SessionRefresher>> {
    REFRESHER.get().cloned()
}

struct Renewal {
    refresher: Arc<dyn SessionRefresher>,
    cfg: Config,
    base_url: String,
}

struct State {
    token: Option<String>,
    // Whether `token` has been through `SessionRefresher::current` yet. Done
    // lazily, on the first request, so opening a backend that never sends one
    // costs no WorkOS round trip.
    primed: bool,
}

/// The `Authorization: Bearer` a remote backend attaches, renewable when a
/// [`SessionRefresher`] is installed.
pub struct Bearer {
    // Held across the refresh itself: WorkOS rotates the refresh token on every
    // use, so two concurrent refreshes would spend it twice and the loser's
    // grant would be rejected.
    state: tokio::sync::Mutex<State>,
    renewal: Option<Renewal>,
}

impl Bearer {
    /// A bearer that is sent as-is and never renewed.
    pub fn fixed(token: Option<String>) -> Self {
        Self {
            state: tokio::sync::Mutex::new(State {
                token,
                primed: true,
            }),
            renewal: None,
        }
    }

    pub(in crate::storage) fn renewable(
        token: Option<String>,
        refresher: Arc<dyn SessionRefresher>,
        cfg: &Config,
        base_url: &str,
    ) -> Self {
        Self {
            state: tokio::sync::Mutex::new(State {
                token,
                primed: false,
            }),
            renewal: Some(Renewal {
                refresher,
                cfg: cfg.clone(),
                base_url: base_url.to_string(),
            }),
        }
    }

    async fn token(&self) -> Result<Option<String>> {
        let mut state = self.state.lock().await;
        if !state.primed {
            if let Some(r) = &self.renewal {
                state.token = r.refresher.current(&r.cfg, &r.base_url).await?;
            }
            state.primed = true;
        }
        Ok(state.token.clone())
    }

    /// Whether a bearer other than `sent` is now available to resend with.
    async fn renew_after_rejection(&self, sent: Option<&str>) -> Result<bool> {
        let (Some(r), Some(sent)) = (&self.renewal, sent) else {
            return Ok(false);
        };
        let mut state = self.state.lock().await;
        if state.token.as_deref() != Some(sent) {
            // A concurrent request already rotated it.
            return Ok(state.token.is_some());
        }
        match r
            .refresher
            .replace_rejected(&r.cfg, &r.base_url, sent)
            .await?
        {
            Some(fresh) if fresh != sent => {
                state.token = Some(fresh);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Run `attempt` with the current bearer; on a `401`, renew it once and run
    /// `attempt` again. A second `401` is returned to the caller as-is, so the
    /// credential hint it produces is only ever reached once renewal has been
    /// tried or cannot apply.
    pub(super) async fn send<F, Fut>(&self, attempt: F) -> Result<reqwest::Response>
    where
        F: Fn(Option<String>) -> Fut,
        Fut: Future<Output = Result<reqwest::Response>>,
    {
        let sent = self.token().await?;
        let resp = attempt(sent.clone()).await?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED
            || !self.renew_after_rejection(sent.as_deref()).await?
        {
            return Ok(resp);
        }
        attempt(self.token().await?).await
    }
}

pub(super) fn authorize(
    req: reqwest::RequestBuilder,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    match token {
        Some(key) => req.header("Authorization", format!("Bearer {key}")),
        None => req,
    }
}

/// Send `req` through `bearer`, classifying a transport failure the way every
/// remote backend does.
pub(super) async fn send_request(
    bearer: &Bearer,
    base_url: &str,
    req: reqwest::RequestBuilder,
    op: &str,
) -> Result<reqwest::Response> {
    if crate::reachability::connect_already_failed(base_url) {
        return Err(super::already_unreachable(base_url, op));
    }
    bearer
        .send(|token| {
            // Only a streaming body cannot be cloned, and no memory route sends one.
            let req = req.try_clone();
            async move {
                let req = req.ok_or_else(|| anyhow::anyhow!("{op}: request cannot be resent"))?;
                authorize(req, token.as_deref())
                    .send()
                    .await
                    .map_err(|err| super::transport_error(err, base_url, op))
            }
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct Rotating {
        current: &'static str,
        replacement: Option<&'static str>,
        replaced: AtomicUsize,
    }

    #[async_trait]
    impl SessionRefresher for Rotating {
        async fn current(&self, _: &Config, _: &str) -> Result<Option<String>> {
            Ok(Some(self.current.to_string()))
        }
        async fn replace_rejected(&self, _: &Config, _: &str, _: &str) -> Result<Option<String>> {
            self.replaced.fetch_add(1, Ordering::SeqCst);
            Ok(self.replacement.map(str::to_string))
        }
    }

    fn rotating(current: &'static str, replacement: Option<&'static str>) -> Arc<Rotating> {
        Arc::new(Rotating {
            current,
            replacement,
            replaced: AtomicUsize::new(0),
        })
    }

    async fn server_accepting(bearer: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header("authorization", format!("Bearer {bearer}")))
            .respond_with(ResponseTemplate::new(200))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .with_priority(2)
            .mount(&server)
            .await;
        server
    }

    async fn get(bearer: &Bearer, server: &MockServer) -> reqwest::StatusCode {
        let client = reqwest::Client::new();
        send_request(bearer, &server.uri(), client.get(server.uri()), "GET /")
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn the_first_request_carries_the_refreshers_current_bearer_not_the_opened_one() {
        let server = server_accepting("at-current").await;
        let refresher = rotating("at-current", None);
        let bearer = Bearer::renewable(
            Some("at-opened".into()),
            refresher.clone(),
            &Config::default(),
            &server.uri(),
        );

        assert_eq!(get(&bearer, &server).await, reqwest::StatusCode::OK);
        assert_eq!(refresher.replaced.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_rejected_bearer_is_replaced_once_and_the_request_resent() {
        let server = server_accepting("at-rotated").await;
        let refresher = rotating("at-stale", Some("at-rotated"));
        let bearer = Bearer::renewable(None, refresher.clone(), &Config::default(), &server.uri());

        assert_eq!(get(&bearer, &server).await, reqwest::StatusCode::OK);
        assert_eq!(get(&bearer, &server).await, reqwest::StatusCode::OK);
        assert_eq!(
            refresher.replaced.load(Ordering::SeqCst),
            1,
            "the rotated bearer must be kept for later requests"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_bearer_the_refresher_cannot_replace_returns_the_401_without_resending() {
        let server = server_accepting("never").await;
        let refresher = rotating("sk-self-hosted", None);
        let bearer = Bearer::renewable(None, refresher.clone(), &Config::default(), &server.uri());

        assert_eq!(
            get(&bearer, &server).await,
            reqwest::StatusCode::UNAUTHORIZED
        );
        assert_eq!(refresher.replaced.load(Ordering::SeqCst), 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_fixed_bearer_is_never_renewed() {
        let server = server_accepting("never").await;
        let bearer = Bearer::fixed(Some("sk".into()));

        assert_eq!(
            get(&bearer, &server).await,
            reqwest::StatusCode::UNAUTHORIZED
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
