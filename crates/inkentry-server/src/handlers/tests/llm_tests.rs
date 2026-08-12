use axum::body::Body;
use axum::http::{self, Request};
use serde_json::json;
use tower::ServiceExt;

use crate::client_ip::TrustedProxies;

use super::support::{
    make_app, make_app_with_llm_and_limit, make_app_with_llm_limit_and_proxies, post_llm_complete,
    post_llm_complete_from,
};

// POST /v1/projects/{slug}/llm/complete with no LLM configured should return 503.
#[tokio::test]
async fn llm_complete_without_llm_returns_503() {
    let (app, _) = make_app(0.92);
    let body = json!({"messages": [{"role": "user", "content": "hi"}], "max_tokens": 16});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/projects/proj/llm/complete")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        http::StatusCode::SERVICE_UNAVAILABLE,
        "llm/complete without an LLM backend must return 503"
    );
}

// The explore route was removed (ADR-079). A POST to the old path must fall
// through to the router's 404, not answer.
#[tokio::test]
async fn explore_route_removed_returns_404() {
    let app = make_app_with_llm_and_limit(1000);
    let body = json!({"question": "what does foo do?", "context_chunks": []});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/projects/proj/explore")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        http::StatusCode::NOT_FOUND,
        "the removed explore route must return 404"
    );
}

// ── /llm/complete rate limiting ─────────────────────────────────────────
//
// The generic inference primitive shares one rate-limit seam
// (`rate_limit_key` + `state.rate_limiter`) with every other inference route.
// These pin that shared behaviour now that it is the sole SSE generation
// endpoint.

// Once the per-bucket budget is exhausted, further calls get 429, not a normal
// (SSE 200) response.
#[tokio::test]
async fn llm_complete_returns_429_past_rate_limit() {
    let app = make_app_with_llm_and_limit(2);

    let status1 = post_llm_complete(&app, "q1").await;
    let status2 = post_llm_complete(&app, "q2").await;
    let status3 = post_llm_complete(&app, "q3").await;

    assert_eq!(status1, http::StatusCode::OK, "1st call within budget");
    assert_eq!(status2, http::StatusCode::OK, "2nd call within budget");
    assert_eq!(
        status3,
        http::StatusCode::TOO_MANY_REQUESTS,
        "3rd call must exceed the 2-request budget and return 429"
    );
}

// Two different clients must not share one rate-limit bucket: each gets its own
// budget, so a shared key can't collapse every caller onto one global bucket.
// "Different client" means a different TCP peer — the one part of a request the
// caller cannot choose.
#[tokio::test]
async fn llm_complete_rate_limit_keyed_per_tcp_peer() {
    let app = make_app_with_llm_and_limit(1);

    assert_eq!(
        post_llm_complete_from(&app, "10.0.0.1:40001", None).await,
        http::StatusCode::OK,
        "client A's first call is within its budget"
    );
    assert_eq!(
        post_llm_complete_from(&app, "10.0.0.1:40002", None).await,
        http::StatusCode::TOO_MANY_REQUESTS,
        "client A's budget is exhausted, and a new source port is the same client"
    );
    assert_eq!(
        post_llm_complete_from(&app, "10.0.0.2:40001", None).await,
        http::StatusCode::OK,
        "a different peer must not share client A's exhausted bucket"
    );
}

// `X-Forwarded-For` is a request header, so a caller can set it to anything.
// With no trusted proxy configured it must not reach the bucket key at all:
// otherwise varying it per request mints an unlimited budget and the ADR-002
// rate limit — the control that bounds spend on the operator's LLM — stops
// existing.
#[tokio::test]
async fn llm_complete_forged_forwarded_for_earns_no_fresh_budget() {
    let app = make_app_with_llm_and_limit(2);

    assert_eq!(
        post_llm_complete_from(&app, "10.0.0.1:40001", Some("203.0.113.1")).await,
        http::StatusCode::OK
    );
    assert_eq!(
        post_llm_complete_from(&app, "10.0.0.1:40002", Some("203.0.113.2")).await,
        http::StatusCode::OK
    );
    for i in 0..20 {
        assert_eq!(
            post_llm_complete_from(&app, "10.0.0.1:40003", Some(&format!("203.0.113.{i}"))).await,
            http::StatusCode::TOO_MANY_REQUESTS,
            "a forged forwarded-for value must not open a new bucket"
        );
    }
}

// An operator who really does run a proxy can opt in, and then only that peer's
// forwarded header is believed.
#[tokio::test]
async fn llm_complete_honours_forwarded_for_from_a_configured_proxy_only() {
    let proxy = "10.9.9.9".parse().expect("proxy address");
    let app = make_app_with_llm_limit_and_proxies(1, TrustedProxies::new([proxy]));

    assert_eq!(
        post_llm_complete_from(&app, "10.9.9.9:40001", Some("203.0.113.1")).await,
        http::StatusCode::OK
    );
    assert_eq!(
        post_llm_complete_from(&app, "10.9.9.9:40002", Some("203.0.113.1")).await,
        http::StatusCode::TOO_MANY_REQUESTS,
        "the same forwarded client keeps one bucket across connections"
    );
    assert_eq!(
        post_llm_complete_from(&app, "10.9.9.9:40003", Some("203.0.113.2")).await,
        http::StatusCode::OK,
        "a different forwarded client gets its own budget behind a trusted proxy"
    );

    // Same header, but arriving directly rather than through the proxy: the
    // trust is on the peer, not on the header's presence.
    assert_eq!(
        post_llm_complete_from(&app, "10.0.0.1:40001", Some("203.0.113.3")).await,
        http::StatusCode::OK
    );
    assert_eq!(
        post_llm_complete_from(&app, "10.0.0.1:40002", Some("203.0.113.4")).await,
        http::StatusCode::TOO_MANY_REQUESTS,
        "an untrusted peer's forwarded-for is ignored, so both calls are one client"
    );
}

// The forwarded value becomes a rate-limiter map key, so it is accepted only as
// an IP address. Junk falls back to the peer instead of allocating a bucket
// under an attacker-chosen string of attacker-chosen length.
#[tokio::test]
async fn llm_complete_rejects_non_ip_forwarded_for_from_a_trusted_proxy() {
    let proxy = "10.9.9.9".parse().expect("proxy address");
    let app = make_app_with_llm_limit_and_proxies(1, TrustedProxies::new([proxy]));

    let junk = "A".repeat(200);
    assert_eq!(
        post_llm_complete_from(&app, "10.9.9.9:40001", Some(&junk)).await,
        http::StatusCode::OK
    );
    assert_eq!(
        post_llm_complete_from(&app, "10.9.9.9:40002", Some(&"B".repeat(200))).await,
        http::StatusCode::TOO_MANY_REQUESTS,
        "unparseable forwarded values must all collapse onto the proxy's own bucket"
    );
}
