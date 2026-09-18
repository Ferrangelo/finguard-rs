//! HTTP-facing error wrapper for Axum handlers.
//!
//! Wraps [`crate::Error`] so it can be returned directly from handlers and
//! turned into a proper HTTP response (status code + JSON body) instead of
//! Axum's default `200 OK` / `text/plain` handling of `String` errors.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::Error;

/// Error type returned by HTTP handlers.
///
/// Converts a crate-wide [`Error`] into an HTTP response with an appropriate
/// status code and a `{ "error": "<message>" }` JSON body.
pub struct AppError(pub Error);

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            Error::InvalidArgument(_) => StatusCode::BAD_REQUEST,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::AlreadyExists(_) => StatusCode::CONFLICT,
            Error::Io(_)
            | Error::Json(_)
            | Error::Polars(_)
            | Error::NoHomeDir
            | Error::RowIdsMissing(_)
            | Error::RowIdMigration { .. }
            | Error::SyncResetBackup { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            Error::SyncProtocol(_) => StatusCode::BAD_REQUEST,
            Error::SyncRefused(_) => StatusCode::CONFLICT,
            Error::Network(_) => StatusCode::SERVICE_UNAVAILABLE,
            Error::MergeRejected(_) => StatusCode::UNPROCESSABLE_ENTITY,
            // Another writer holds the change log. The request can succeed
            // once that writer exits, so this is the one retryable failure
            // here, not a permanent one.
            Error::SyncLogLocked { .. } => StatusCode::SERVICE_UNAVAILABLE,
        };

        let body = ErrorBody {
            error: self.0.to_string(),
        };

        (status, Json(body)).into_response()
    }
}

impl From<Error> for AppError {
    fn from(err: Error) -> Self {
        AppError(err)
    }
}
