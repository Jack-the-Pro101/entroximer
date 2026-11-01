//! The engine's HTTP surface: health, info, the client WebSocket and YouTube session admin.
//!
//! When the engine has a password, everything but `/health` requires the `Authorization` header
//! to carry it.

pub mod protocol;
mod ws;

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use entroximer_core::error::display_chain;
use serde::Serialize;
use ts_rs::TS;

use crate::node::Node;
use crate::serve::HEALTH_PATH;
use crate::youtube::{CheckOutcome, SessionStatus, check_session};
use protocol::{ApiError, ErrorCode};

pub fn router(node: Arc<Node>) -> Router {
    Router::new()
        .route(HEALTH_PATH, get(|| async { "ok" }))
        .route("/v1/info", get(info))
        .route("/v1/ws", get(ws::upgrade))
        .route(
            "/v1/youtube/session",
            get(session_status)
                .put(import_session)
                .delete(remove_session),
        )
        .with_state(node)
}

/// Proof that the request carried the engine's password, if it has one.
pub(crate) struct Authorized;

impl FromRequestParts<Arc<Node>> for Authorized {
    type Rejection = HttpError;

    async fn from_request_parts(parts: &mut Parts, node: &Arc<Node>) -> Result<Self, HttpError> {
        let Some(password) = &node.settings.password else {
            return Ok(Self);
        };
        let given = parts
            .headers
            .get(header::AUTHORIZATION)
            .map(|v| v.as_bytes())
            .unwrap_or_default();
        if constant_time_eq(given, password.as_bytes()) {
            Ok(Self)
        } else {
            Err(HttpError(
                StatusCode::UNAUTHORIZED,
                ApiError::new(
                    ErrorCode::Unauthorized,
                    "missing or wrong Authorization header",
                ),
            ))
        }
    }
}

/// An error answered over HTTP, with the same body as protocol errors.
#[derive(Debug)]
pub(crate) struct HttpError(StatusCode, ApiError);

impl HttpError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self(
            StatusCode::BAD_REQUEST,
            ApiError::new(ErrorCode::BadRequest, message),
        )
    }

    fn internal(message: impl Into<String>) -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::new(ErrorCode::Internal, message),
        )
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

async fn info(_: Authorized, State(node): State<Arc<Node>>) -> Json<protocol::NodeInfo> {
    Json(node.info().await)
}

async fn session_status(
    _: Authorized,
    State(node): State<Arc<Node>>,
) -> Result<Json<SessionStatus>, HttpError> {
    node.ctx
        .youtube
        .store
        .status()
        .map(Json)
        .map_err(|e| HttpError::internal(display_chain(&e)))
}

/// Result of `PUT /v1/youtube/session`.
#[derive(Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SessionImport {
    pub cookies: usize,
    /// Cookies of other sites, left out.
    pub dropped: usize,
    /// Whether YouTube accepted the session when checked right after importing.
    pub accepted: Option<bool>,
    /// Why the check was inconclusive, if it was.
    pub check_error: Option<String>,
}

/// Replaces the session with the cookies.txt in the body, then checks it.
async fn import_session(
    _: Authorized,
    State(node): State<Arc<Node>>,
    body: String,
) -> Result<Json<SessionImport>, HttpError> {
    let report = node
        .ctx
        .youtube
        .store
        .import(&body)
        .map_err(|e| HttpError::bad_request(display_chain(&e)))?;
    let check = check_session(&node.ctx, true)
        .await
        .map_err(|e| HttpError::internal(display_chain(&e)))?;
    let (accepted, check_error) = match check {
        CheckOutcome::Valid => (Some(true), None),
        CheckOutcome::Expired => (Some(false), None),
        CheckOutcome::NoSession => (None, Some("the session disappeared".to_owned())),
        CheckOutcome::Inconclusive(reason) => (None, Some(reason)),
    };
    Ok(Json(SessionImport {
        cookies: report.cookies,
        dropped: report.dropped,
        accepted,
        check_error,
    }))
}

async fn remove_session(
    _: Authorized,
    State(node): State<Arc<Node>>,
) -> Result<StatusCode, HttpError> {
    node.ctx
        .youtube
        .store
        .remove()
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| HttpError::internal(display_chain(&e)))
}

/// Compares secrets without revealing through timing how much of them matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_secrets() {
        assert!(constant_time_eq(b"hunter2", b"hunter2"));
        assert!(!constant_time_eq(b"hunter2", b"hunter3"));
        assert!(!constant_time_eq(b"hunter", b"hunter2"));
        assert!(!constant_time_eq(b"", b"x"));
    }
}
