use axum::body::Body;
use axum::http::{self, Request};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::support::{make_app, post_note};

// POST /v1/projects/{slug}/memory/search with no embedder should return 400.
#[tokio::test]
async fn search_without_embedder_returns_400() {
    let (app, _) = make_app(0.92);
    // First create the project.
    let _ = post_note(
        app.clone(),
        "search-proj",
        "seed note",
        vec![1.0, 0.0, 0.0, 0.0],
    )
    .await;

    let body = json!({"query": "test query", "limit": 5});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/projects/search-proj/memory/search")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        http::StatusCode::BAD_REQUEST,
        "search without embedder must return 400"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(
        json["error"]["code"],
        json!("bad_request"),
        "error code must be bad_request"
    );
}
