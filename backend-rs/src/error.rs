//! HTTP error shape.
//!
//! FastAPI's `HTTPException` renders as `{"detail": ...}`, and the SPA and
//! CameraNode both read that key. `detail` is deliberately a
//! `serde_json::Value` rather than a string: several Command Center
//! endpoints return a structured detail (the plan-limit 402 carries
//! `{"error": ..., "plan": ...}`), and flattening those to text would
//! change the contract for the frontend that branches on them.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub detail: Value,
}

impl ApiError {
    pub fn new(status: StatusCode, detail: impl Into<Value>) -> Self {
        Self {
            status,
            detail: detail.into(),
        }
    }

    pub fn unauthorized(detail: impl Into<Value>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, detail)
    }

    pub fn forbidden(detail: impl Into<Value>) -> Self {
        Self::new(StatusCode::FORBIDDEN, detail)
    }

    pub fn not_found(detail: impl Into<Value>) -> Self {
        Self::new(StatusCode::NOT_FOUND, detail)
    }

    pub fn bad_request(detail: impl Into<Value>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, detail)
    }

    pub fn internal(detail: impl Into<Value>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, detail)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "detail": self.detail }))).into_response()
    }
}

/// A database failure is a 500 with a generic body. The sqlx error names
/// columns and constraints, which is not something to hand to a caller;
/// it goes to the log instead.
impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        tracing::error!(error = %err, "database error");
        ApiError::internal("database error")
    }
}
