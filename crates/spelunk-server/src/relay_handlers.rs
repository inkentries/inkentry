//! Axum handlers for the ADR-037 P2 local relay surface (`crate::relay`).
//!
//! These routes are **local-only**: the CLI on the same machine is the only
//! intended caller. They sit behind the same [`crate::auth_middleware`] as
//! every other route (item 39) — on the common auto-spawned, unauthenticated,
//! loopback-bound daemon (`spelunk server`'s doc comment: "the auto-spawned
//! daemon is unauthenticated, so it MUST only ever bind loopback"), that
//! gives these routes the exact same trust posture the existing `/memory`
//! routes already have on that same daemon: loopback-bind is the boundary,
//! not a request-level check. A server started with `--key` additionally
//! requires it here too, same as everywhere else.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::relay::{RelayPollResponse, RelayPushRequest};
use crate::{AppError, AppState};

/// `POST /local/relay/push` — see [`crate::relay::RelayRegistry::push`].
pub async fn relay_push(
    State(state): State<AppState>,
    Json(body): Json<RelayPushRequest>,
) -> Result<Response, AppError> {
    state.relay.push(body).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"accepted": true})),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
pub struct RelayPollQuery {
    pub server_url: String,
    pub project_id: String,
}

/// `GET /local/relay/poll` — see [`crate::relay::RelayRegistry::poll`].
pub async fn relay_poll(
    State(state): State<AppState>,
    Query(q): Query<RelayPollQuery>,
) -> Json<RelayPollResponse> {
    Json(state.relay.poll(&q.server_url, &q.project_id).await)
}
