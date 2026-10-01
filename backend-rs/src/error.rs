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

/// Wrapped in a struct rather than boxed directly as a `Vec` so the
/// pointer is the only thing `ApiError` carries: every handler in the
/// crate returns this type by value, and almost none of them set a
/// header.
#[derive(Debug, Default)]
struct ExtraHeaders(Vec<(&'static str, String)>);

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub detail: Value,
    /// Render as Starlette's bare `Internal Server Error` rather than the
    /// `{"detail": ...}` envelope.
    ///
    /// Every 500 in the Python service is an *unhandled* exception —
    /// there is not one deliberate `HTTPException(status_code=500)` in
    /// the codebase — and Starlette renders those as
    /// `text/plain; charset=utf-8` with the body `Internal Server Error`.
    /// A JSON envelope here would be a shape no Python 500 ever has, and
    /// would leak a hint about what broke besides.
    opaque: bool,
    /// A slowapi 429, which has its own body shape and `Retry-After`
    /// rather than the `{"detail": ...}` envelope.
    rate_limit: Option<(u32, u64)>,
    /// Headers an `HTTPException(..., headers=...)` carries. Distinct
    /// from the slowapi 429 above, which builds its own. Boxed because
    /// almost no error has any, and every handler in the crate returns
    /// this type by value.
    headers: Option<Box<ExtraHeaders>>,
}

impl ApiError {
    pub fn new(status: StatusCode, detail: impl Into<Value>) -> Self {
        Self {
            status,
            detail: detail.into(),
            opaque: false,
            rate_limit: None,
            headers: None,
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

    /// An internal failure the caller gets no detail about.
    ///
    /// `detail` is kept for the log and never reaches the response.
    pub fn internal(detail: impl Into<Value>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            detail: detail.into(),
            opaque: true,
            rate_limit: None,
            headers: None,
        }
    }

    /// Add a header the Python passes to `HTTPException(headers=...)`.
    ///
    /// Starlette puts those on the error response itself, so the caller
    /// sees them alongside the `{"detail": ...}` body — which is how a
    /// viewer-cap 429 carries its `Retry-After`.
    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers
            .get_or_insert_with(Box::default)
            .0
            .push((name, value.into()));
        self
    }

    pub fn rate_limited(limit: u32, window_secs: u64) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            detail: Value::Null,
            opaque: false,
            rate_limit: Some((limit, window_secs)),
            headers: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let Some((limit, window)) = self.rate_limit {
            return crate::ratelimit::too_many_requests(limit, window);
        }
        if self.opaque {
            // Byte-for-byte what Starlette emits for an unhandled
            // exception, down to the charset.
            return (
                self.status,
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/plain; charset=utf-8",
                )],
                "Internal Server Error",
            )
                .into_response();
        }
        let mut response = (self.status, Json(json!({ "detail": self.detail }))).into_response();
        for (name, value) in self.headers.iter().flat_map(|h| h.0.iter()) {
            if let (Ok(name), Ok(value)) = (
                axum::http::HeaderName::from_bytes(name.as_bytes()),
                axum::http::HeaderValue::from_str(value),
            ) {
                response.headers_mut().insert(name, value);
            }
        }
        response
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_of(err: ApiError) -> (StatusCode, String, String) {
        let response = err.into_response();
        let status = response.status();
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (
            status,
            content_type,
            String::from_utf8_lossy(&bytes).to_string(),
        )
    }

    #[tokio::test]
    async fn an_ordinary_error_uses_the_detail_envelope() {
        let (status, ct, body) = body_of(ApiError::not_found("Camera not found")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(ct.starts_with("application/json"));
        assert_eq!(body, r#"{"detail":"Camera not found"}"#);
    }

    #[tokio::test]
    async fn an_internal_error_looks_like_starlettes() {
        // Not {"detail": "database error"}: no Python 500 has that
        // shape, because every 500 over there is unhandled.
        let (status, ct, body) = body_of(ApiError::internal("database error")).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(ct, "text/plain; charset=utf-8");
        assert_eq!(body, "Internal Server Error");
    }

    #[tokio::test]
    async fn a_database_error_is_opaque_to_the_caller() {
        // The sqlx message names columns and constraints; it belongs in
        // the log, not in a response.
        let err: ApiError = sqlx::Error::RowNotFound.into();
        let (_, _, body) = body_of(err).await;
        assert_eq!(body, "Internal Server Error");
        assert!(!body.contains("RowNotFound"));
    }
}
