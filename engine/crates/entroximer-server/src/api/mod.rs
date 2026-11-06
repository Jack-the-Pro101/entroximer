//! The engine's HTTP surface: health, info, the client WebSocket, artwork the engine serves
//! itself, uploads, and YouTube session admin.
//!
//! When the engine has a password, everything but `/health` requires the `Authorization` header
//! to carry it.

pub mod protocol;
mod uploads;
mod ws;

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use entroximer_core::error::display_chain;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use ts_rs::TS;

use crate::node::Node;
use crate::node::tracks::load_error;
use crate::serve::HEALTH_PATH;
use crate::sources::artwork::{COVER_PATH, CoverError};
use crate::youtube::{
    CheckOutcome, RotationOutcome, SessionStatus, browser_config, check_session, rotate_session,
};
use protocol::{ApiError, ErrorCode};

pub fn router(node: Arc<Node>) -> Router {
    // all `/health` needs to know is whether the engine is shutting down
    let closing = node.closing_signal();
    Router::new()
        .route(HEALTH_PATH, get(move || health(closing.clone())))
        .route("/v1/info", get(info))
        .route("/v1/ws", get(ws::upgrade))
        .route(&format!("{COVER_PATH}{{key}}"), get(artwork))
        .route("/v1/uploads", post(uploads::create))
        .route("/v1/uploads/{id}", delete(uploads::delete))
        .route(
            "/v1/youtube/session",
            get(session_status)
                .put(import_session)
                .delete(remove_session),
        )
        .route("/v1/youtube/session/rotate", post(rotate))
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
            .map(axum::http::HeaderValue::as_bytes)
            .unwrap_or_default();
        if constant_time_eq(given, password.expose().as_bytes()) {
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

    fn not_found(message: impl Into<String>) -> Self {
        Self(
            StatusCode::NOT_FOUND,
            ApiError::new(ErrorCode::NotFound, message),
        )
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self(
            StatusCode::SERVICE_UNAVAILABLE,
            ApiError::new(ErrorCode::Internal, message),
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

/// `ok` while the engine takes clients; 503 once it is shutting down, so that orchestrators stop
/// sending it traffic.
async fn health(closing: CancellationToken) -> (StatusCode, &'static str) {
    if closing.is_cancelled() {
        (StatusCode::SERVICE_UNAVAILABLE, "shutting down")
    } else {
        (StatusCode::OK, "ok")
    }
}

async fn info(_: Authorized, State(node): State<Arc<Node>>) -> Json<protocol::NodeInfo> {
    Json(node.info().await)
}

/// The cover embedded in a media file, for the `artworkUrl`s that point here.
async fn artwork(
    _: Authorized,
    State(node): State<Arc<Node>>,
    Path(key): Path<String>,
) -> Result<Response, HttpError> {
    match node.ctx.artwork.cover(&key, &node.ctx).await {
        Ok(cover) => Ok((
            [
                (header::CONTENT_TYPE, cover.content_type),
                // the key names the file, so its cover changes rarely
                (header::CACHE_CONTROL, "private, max-age=86400"),
            ],
            cover.data,
        )
            .into_response()),
        Err(e @ CoverError::BadKey) => Err(HttpError::bad_request(e.to_string())),
        Err(e @ CoverError::NotFound) => Err(HttpError::not_found(e.to_string())),
        Err(CoverError::Source(e)) => Err(HttpError(StatusCode::BAD_GATEWAY, load_error(&e))),
    }
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

/// Result of `POST /v1/youtube/session/rotate`.
#[derive(Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SessionRotation {
    pub outcome: RotationResult,
    /// Why YouTube refused, or why nothing could be told.
    pub reason: Option<String>,
    /// Whether Google issued new tokens, when rotated.
    pub tokens_changed: Option<bool>,
    /// YouTube's refusals in a row.
    pub refusals: u32,
    /// Rotation gave up after this refusal: the session has expired.
    pub gave_up: bool,
}

#[derive(Clone, Copy, Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum RotationResult {
    NoSession,
    Rotated,
    /// YouTube refused the session.
    Refused,
    /// The session is unchanged, e.g. because the browser failed.
    Inconclusive,
}

/// Rotates the session's cookies now with headless Chromium.
async fn rotate(
    _: Authorized,
    State(node): State<Arc<Node>>,
) -> Result<Json<SessionRotation>, HttpError> {
    let browser = browser_config(&node.ctx)
        .ok_or_else(|| HttpError::unavailable("the engine has no Chromium to rotate with"))?;
    let report = rotate_session(&node.ctx, &browser)
        .await
        .map_err(|e| HttpError::internal(display_chain(&e)))?;
    let (outcome, reason, tokens_changed) = match report.outcome {
        RotationOutcome::NoSession => (RotationResult::NoSession, None, None),
        RotationOutcome::Rotated { tokens_changed } => {
            (RotationResult::Rotated, None, Some(tokens_changed))
        }
        RotationOutcome::SignedOut(reason) => (RotationResult::Refused, Some(reason), None),
        RotationOutcome::Inconclusive(reason) => (RotationResult::Inconclusive, Some(reason), None),
    };
    Ok(Json(SessionRotation {
        outcome,
        reason,
        tokens_changed,
        refusals: report.refusals,
        gave_up: report.gave_up,
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
