//! Axum handlers for the ADR-037 P2 local relay surface (`crate::relay`).
//!
//! These routes are **local-only**: the CLI on the same machine is the only
//! intended caller, and [`crate::relay::RelayRegistry::for_bind`] means a
//! daemon on a non-loopback address does not serve them at all.
//!
//! They sit behind the same [`crate::auth_middleware`] as every other route,
//! but that parity is not what makes them safe. On the common auto-spawned,
//! unauthenticated, loopback-bound daemon the middleware admits everyone, and
//! unlike its neighbours this surface makes the daemon open outbound
//! connections — a capability no other route grants, so "same auth as the
//! rest" settles nothing about it. What bounds it is that the destination
//! comes from local configuration (`crate::relay::RelayPolicy`), never from
//! the request body.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::relay::{RelayAckRequest, RelayPollResponse, RelayPushRequest};
use crate::{AppError, AppState};

/// `POST /local/relay/push` — see [`crate::relay::RelayRegistry::push`].
///
/// A refusal is the caller's own fault (an undeclared target, a full
/// registry), so it answers `400` with the refusal's fixed text rather than
/// `500` with an opaque one.
pub async fn relay_push(
    State(state): State<AppState>,
    Json(body): Json<RelayPushRequest>,
) -> Result<Response, AppError> {
    state
        .relay
        .push(body)
        .await
        .map_err(|refused| AppError::BadRequest(refused.to_string()))?;
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

/// `GET /local/relay/poll` — see [`crate::relay::RelayRegistry::poll`]. A
/// peek, not a drain: buffered entries stay until confirmed via
/// `POST /local/relay/ack`.
pub async fn relay_poll(
    State(state): State<AppState>,
    Query(q): Query<RelayPollQuery>,
) -> Json<RelayPollResponse> {
    Json(state.relay.poll(&q.server_url, &q.project_id).await)
}

/// `POST /local/relay/ack` — see [`crate::relay::RelayRegistry::ack`]. The
/// CLI calls this after it has durably applied a poll's results locally;
/// only the named entries are retired from the relay's buffer.
pub async fn relay_ack(
    State(state): State<AppState>,
    Json(body): Json<RelayAckRequest>,
) -> Response {
    state
        .relay
        .ack(
            &body.server_url,
            &body.project_id,
            &body.applied_push_external_ids,
            &body.applied_pull_remote_ids,
        )
        .await;
    (StatusCode::OK, Json(serde_json::json!({"acked": true}))).into_response()
}
