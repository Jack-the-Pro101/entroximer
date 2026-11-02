//! `POST /v1/uploads` and `DELETE /v1/uploads/{id}`: audio files clients upload to play, if the
//! operator enabled uploads (see [`crate::uploads`]).

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use entroximer_core::error::display_chain;
use serde::Deserialize;

use super::protocol::{ApiError, ErrorCode, Millis, Upload};
use super::{Authorized, HttpError};
use crate::node::Node;
use crate::node::tracks::to_track;
use crate::sources::upload::found_track;
use crate::uploads::{UploadError, UploadStore};

/// The longest title an upload may be given, in characters.
const MAX_TITLE: usize = 200;

#[derive(Debug, Deserialize)]
pub(super) struct UploadQuery {
    /// Its title, if not the file's own.
    title: Option<String>,
}

/// Stores the file in the body; it plays as the returned track.
pub(super) async fn create(
    authorized: Result<Authorized, HttpError>,
    State(node): State<Arc<Node>>,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    match receive(
        authorized,
        node.ctx.uploads.as_deref(),
        query,
        &headers,
        body,
    )
    .await
    {
        Ok(upload) => (StatusCode::CREATED, Json(upload)).into_response(),
        // refused before the body was read, or partway: the rest of it is not read, so the
        // connection cannot take another request
        Err(error) => ([(header::CONNECTION, "close")], error).into_response(),
    }
}

async fn receive(
    authorized: Result<Authorized, HttpError>,
    store: Option<&UploadStore>,
    query: UploadQuery,
    headers: &HeaderMap,
    body: Body,
) -> Result<Upload, HttpError> {
    authorized?;
    let store = store.ok_or_else(uploads_off)?;
    let title = query
        .title
        .map(|title| title.trim().to_owned())
        .filter(|title| !title.is_empty());
    if title
        .as_ref()
        .is_some_and(|title| title.chars().count() > MAX_TITLE)
    {
        return Err(HttpError::bad_request(format!(
            "titles are at most {MAX_TITLE} characters"
        )));
    }
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    let file = store
        .receive(body.into_data_stream(), declared, title)
        .await
        .map_err(|e| upload_error(&e))?;
    let upload = Upload {
        id: file.id.clone(),
        track: to_track(found_track(&file)),
        bytes: file.bytes,
        expires_at: Millis(Millis::now().0 + Millis::from(store.limits().keep).0),
    };
    Ok(upload)
}

/// Deletes an upload. One that is playing stops being playable at once and its file goes when
/// it ends.
pub(super) async fn delete(
    _: Authorized,
    State(node): State<Arc<Node>>,
    Path(id): Path<String>,
) -> Result<StatusCode, HttpError> {
    let store = node.ctx.uploads.as_deref().ok_or_else(uploads_off)?;
    if store.delete(&id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(HttpError::not_found(format!(
            "no upload `{id}`; it may have expired"
        )))
    }
}

fn uploads_off() -> HttpError {
    HttpError::not_found(
        "uploads are off; the engine's operator can enable them with ENTROXIMER_UPLOADS",
    )
}

fn upload_error(e: &UploadError) -> HttpError {
    let (status, code) = match e {
        UploadError::TooLarge { .. } => (StatusCode::PAYLOAD_TOO_LARGE, ErrorCode::TooLarge),
        UploadError::Full => (StatusCode::INSUFFICIENT_STORAGE, ErrorCode::StorageFull),
        UploadError::Unplayable(_) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, ErrorCode::NoAudio),
        UploadError::Empty | UploadError::Body(_) => {
            (StatusCode::BAD_REQUEST, ErrorCode::BadRequest)
        }
        UploadError::Io(_) => {
            tracing::warn!(error = %display_chain(e), "cannot store an upload");
            (StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal)
        }
    };
    HttpError(status, ApiError::new(code, display_chain(e)))
}
