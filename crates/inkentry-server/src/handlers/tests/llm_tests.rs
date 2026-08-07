use axum::body::Body;
use axum::http::{self, Request};
use serde_json::json;
use tower::ServiceExt;

use super::support::{make_app, make_app_with_llm_and_limit, post_llm_complete};

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

// Two different client IPs (via `X-Forwarded-For`) must not share one
// rate-limit bucket: each gets its own budget, so a shared key can't collapse
// every caller onto one global bucket.
#[tokio::test]
async fn llm_complete_rate_limit_keyed_per_client_ip() {
    let app = make_app_with_llm_and_limit(1);

    let body = json!({"messages": [{"role": "user", "content": "q"}], "max_tokens": 16});
    let req_from = |ip: &str| {
        Request::builder()
            .method("POST")
            .uri("/v1/projects/llm-test/llm/complete")
            .header("content-type", "application/json")
            .header("x-forwarded-for", ip)
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    };

    // Client A's first call succeeds and exhausts its (budget=1) bucket.
    let resp_a1 = app.clone().oneshot(req_from("10.0.0.1")).await.unwrap();
    assert_eq!(resp_a1.status(), http::StatusCode::OK);

    // Client A's second call is rate-limited.
    let resp_a2 = app.clone().oneshot(req_from("10.0.0.1")).await.unwrap();
    assert_eq!(resp_a2.status(), http::StatusCode::TOO_MANY_REQUESTS);

    // Client B (different IP) still has its own budget.
    let resp_b1 = app.clone().oneshot(req_from("10.0.0.2")).await.unwrap();
    assert_eq!(
        resp_b1.status(),
        http::StatusCode::OK,
        "a different client IP must not share client A's exhausted bucket"
    );
}
