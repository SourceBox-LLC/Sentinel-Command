//! CORS for routes Rust serves.
//!
//! Python wraps every response in Starlette's `CORSMiddleware`. A ported
//! route leaves that wrapper behind, so its responses came back with no
//! CORS headers at all — the preflight still succeeded, because `OPTIONS`
//! falls through to the proxy, and then the browser blocked the actual
//! response. Every cross-origin caller would have broken: the Vite dev
//! server on :5173, any separately-hosted frontend, the documented
//! `CORS_ALLOWED_ORIGINS` deployments.
//!
//! Only *simple* responses are handled here. Preflight stays with
//! Python: `OPTIONS` is not a method any ported route claims, so it
//! reaches the proxy and Starlette answers it — and a second
//! implementation of preflight is a second thing to keep in sync.

use axum::http::{header, HeaderValue, Request, Response};
use std::sync::Arc;

/// Mirrors `expose_headers=["X-Request-Id"]` in `main.py`.
const EXPOSE_HEADERS: &str = "X-Request-Id";

#[derive(Clone, Debug)]
pub struct CorsConfig {
    origins: Arc<Vec<String>>,
}

impl CorsConfig {
    /// Build the allow-list the way `main.py` does: two localhost origins
    /// as a developer-convenience baseline, then `CORS_ALLOWED_ORIGINS`,
    /// then `FRONTEND_URL` — each validated, duplicates dropped, order
    /// preserved.
    pub fn from_env(frontend_url: &str, extra_raw: &str) -> Self {
        let mut origins = vec![
            "http://localhost:5173".to_string(),
            "http://localhost:8000".to_string(),
        ];
        for raw in extra_raw.split(',') {
            if let Some(o) = validate_origin(raw) {
                if !origins.contains(&o) {
                    origins.push(o);
                }
            }
        }
        if let Some(o) = validate_origin(frontend_url) {
            if !origins.contains(&o) {
                origins.push(o);
            }
        }
        Self {
            origins: Arc::new(origins),
        }
    }

    fn allows(&self, origin: &str) -> bool {
        self.origins.iter().any(|o| o == origin)
    }
}

/// `_validate_frontend_url` from `main.py`.
///
/// A trailing slash is trimmed rather than rejected, despite the comment
/// above it saying "Reject trailing slashes" — the code rstrips and
/// carries on. Reproducing the code, not the comment.
fn validate_origin(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let url = url.trim_end_matches('/');
    if url.chars().any(char::is_whitespace) || url.contains(',') {
        return None;
    }
    Some(url.to_string())
}

/// Add the simple-request CORS headers Starlette would have added.
///
/// Skipped when the response already carries them: a proxied response
/// has been through Python's middleware, and emitting a second
/// `Access-Control-Allow-Origin` makes the browser reject it outright.
pub async fn layer<B>(
    axum::extract::State(config): axum::extract::State<crate::app::AppState>,
    request: Request<B>,
    next: axum::middleware::Next,
) -> Response<axum::body::Body>
where
    B: Into<axum::body::Body>,
{
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let (parts, body) = request.into_parts();
    let mut response = next.run(Request::from_parts(parts, body.into())).await;

    // No Origin header: Starlette's middleware short-circuits before it
    // adds anything at all.
    let Some(origin) = origin else {
        return response;
    };
    // Already been through Python's middleware — this is a proxied
    // response. A second Access-Control-Allow-Origin makes the browser
    // reject it outright.
    if response
        .headers()
        .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
    {
        return response;
    }

    // Starlette's "simple headers" go on every response that carried an
    // Origin, allowed or not. Measured, because reasoning gave the wrong
    // answer twice: a disallowed origin still gets allow-credentials and
    // expose-headers, and does *not* get `Vary: Origin` even though the
    // response genuinely varies by it.
    let allowed = config.cors.allows(&origin);
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static(EXPOSE_HEADERS),
    );
    if !allowed {
        return response;
    }

    // allow_credentials=True in main.py, which is why the origin is
    // echoed back rather than answered with `*` — the two are mutually
    // exclusive to browsers.
    if let Ok(value) = HeaderValue::from_str(&origin) {
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
    append_vary_origin(&mut response);
    response
}

fn append_vary_origin(response: &mut Response<axum::body::Body>) {
    let headers = response.headers_mut();
    let existing = headers
        .get(header::VARY)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let value = match existing {
        Some(v)
            if v.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("origin")) =>
        {
            return
        }
        Some(v) => format!("{v}, Origin"),
        None => "Origin".to_string(),
    };
    if let Ok(value) = HeaderValue::from_str(&value) {
        headers.insert(header::VARY, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_localhost_baseline_is_always_allowed() {
        // `npm run dev` against a deployed backend works with no env
        // configuration at all, which is why these are unconditional.
        let c = CorsConfig::from_env("", "");
        assert!(c.allows("http://localhost:5173"));
        assert!(c.allows("http://localhost:8000"));
        assert!(!c.allows("http://localhost:9999"));
    }

    #[test]
    fn extra_origins_and_the_frontend_url_are_added() {
        let c = CorsConfig::from_env(
            "https://app.example.com",
            "https://a.example.com,https://b.example.com",
        );
        assert!(c.allows("https://a.example.com"));
        assert!(c.allows("https://b.example.com"));
        assert!(c.allows("https://app.example.com"));
    }

    #[test]
    fn a_malformed_origin_is_dropped_rather_than_widening_cors() {
        let c = CorsConfig::from_env("", "not-a-url,ftp://x,  ,https://ok.example.com");
        assert!(c.allows("https://ok.example.com"));
        for bad in ["not-a-url", "ftp://x", ""] {
            assert!(!c.allows(bad), "{bad}");
        }
    }

    #[test]
    fn a_trailing_slash_is_trimmed_not_rejected() {
        // The comment in main.py says "Reject trailing slashes"; the code
        // rstrips and carries on. The code is what runs.
        let c = CorsConfig::from_env("https://app.example.com/", "");
        assert!(c.allows("https://app.example.com"));
    }

    #[test]
    fn origins_are_matched_exactly() {
        let c = CorsConfig::from_env("https://app.example.com", "");
        for near in [
            "https://app.example.com.evil.test",
            "https://evil.test/https://app.example.com",
            "http://app.example.com",
            "https://APP.example.com",
        ] {
            assert!(!c.allows(near), "{near} must not be treated as allowed");
        }
    }

    #[test]
    fn duplicates_collapse() {
        let c = CorsConfig::from_env("http://localhost:5173", "http://localhost:5173");
        assert_eq!(
            c.origins
                .iter()
                .filter(|o| *o == "http://localhost:5173")
                .count(),
            1
        );
    }
}
