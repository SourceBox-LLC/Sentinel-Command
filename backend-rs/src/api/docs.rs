//! The API documentation surface: `/api-docs`, `/api-redoc` and
//! `/api/openapi.json`.
//!
//! These were the last routes Python answered, and they are the one
//! place in this port where "reproduce the Python" is not achievable and
//! not the goal. FastAPI generates the schema by introspecting its own
//! route table and Pydantic models; there is no equivalent to port, and
//! a document written by hand in Rust would be a different, worse
//! document that happened to sit at the same URL.
//!
//! So the schema is **harvested, not rewritten**. The committed
//! `assets/openapi.json` is the document FastAPI generated for this
//! exact commit — 87 paths, 23 component schemas — captured from the
//! running Python before it was deleted and compiled into the binary.
//! The two HTML shells are likewise FastAPI's own, which is why they
//! still load Swagger UI and ReDoc from jsdelivr and point at
//! `/api/openapi.json`.
//!
//! It is the UNION OF BOTH AUTH MODES, which took a second pass to get
//! right. Python mounts the webhooks router only under Clerk and the
//! local-auth router only under local, so each mode's document is
//! missing the other's routes — 85 paths either way, and not the same
//! 85. The first harvest was from the local-auth tier and the drift
//! checker immediately named the two webhook paths it had no way to
//! know about. A reader wants every route this build can serve, so the
//! document is both.
//!
//! **The obvious objection is drift**, and it is the right one: a
//! snapshot cannot notice a route being added. So
//! `tests/differential/openapi_drift.py` compares the document's paths
//! against `app.rs`'s route table and fails on either side having
//! something the other does not. That is the same shape as
//! `route_capture.py` and it is what makes a snapshot honest rather than
//! a stale artefact nobody is accountable for.
//!
//! What this surface deliberately does NOT try to be: a generated
//! document. Doing that properly means annotating every handler and
//! every response type, which is a project of its own and buys a
//! prettier `/api-docs` rather than a working one.

use axum::response::{IntoResponse, Response};

/// FastAPI's generated document, compiled in.
const OPENAPI_JSON: &str = include_str!("../../assets/openapi.json");
/// FastAPI's own Swagger UI shell.
const SWAGGER_HTML: &str = include_str!("../../assets/api-docs.html");
/// FastAPI's own ReDoc shell.
const REDOC_HTML: &str = include_str!("../../assets/api-redoc.html");

/// `GET /api/openapi.json`.
///
/// `application/json`, with no charset — which is what Starlette's
/// `JSONResponse` sends, and the read differential compares the header.
pub async fn openapi_json() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        OPENAPI_JSON,
    )
        .into_response()
}

/// `GET /api-docs` — Swagger UI.
pub async fn swagger_ui() -> Response {
    html(SWAGGER_HTML)
}

/// `GET /api-redoc` — ReDoc.
pub async fn redoc() -> Response {
    html(REDOC_HTML)
}

// There is deliberately NO `/docs/oauth2-redirect` handler.
//
// FastAPI declares that route and `blockers.py` lists it, which is what
// sent me looking for it — but it is UNREACHABLE in Python. The SPA
// middleware runs outermost and passes through only `/api`, `/ws`,
// `/install.`, `/mcp-setup.`, `/downloads/`, `/.well-known/` and
// `/security.txt`; `/docs` is not on that list, so every `/docs/*`
// request gets the React document. Verified against the running Python:
// `/docs/oauth2-redirect` returns `<!DOCTYPE html><html lang="en">`,
// the SPA, not Swagger's callback page.
//
// So the correct port is to serve the SPA there, which `spa::fallback`
// already does by having no route in the way. Adding a handler would
// have made Rust diverge from Python on a path a Swagger "authorize"
// click actually visits — and I nearly did, on the strength of a
// captured asset that turned out to be the SPA index.

/// Swagger UI and ReDoc come from jsdelivr and are started by an inline
/// script, so these two pages carry their own, looser policy in place of
/// the dashboard's (`headers::stamp` leaves a handler's policy alone).
const DOCS_CSP: &str = "default-src 'self'; \
     script-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; \
     style-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net https://fonts.googleapis.com; \
     font-src 'self' data: https://fonts.gstatic.com; \
     img-src 'self' data: https:; \
     worker-src 'self' blob:; \
     connect-src 'self'; \
     object-src 'none'; base-uri 'self'; frame-ancestors 'none'";

fn html(body: &'static str) -> Response {
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (axum::http::header::CONTENT_SECURITY_POLICY, DOCS_CSP),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The document is real JSON, is the version this build reports, and
    /// carries the paths a client would look for. A truncated or
    /// placeholder asset would still compile and still serve 200.
    #[test]
    fn the_document_is_the_harvested_schema() {
        let parsed: serde_json::Value =
            serde_json::from_str(OPENAPI_JSON).expect("assets/openapi.json must be valid JSON");
        assert_eq!(parsed["openapi"], "3.1.0");
        assert_eq!(parsed["info"]["version"], crate::app::VERSION);
        let paths = parsed["paths"].as_object().expect("paths");
        // 87 at harvest — the union of both auth modes. Asserted as a
        // floor rather than an equality so
        // adding a route does not fail here — `openapi_drift.py` is what
        // holds the two in step, and it can say WHICH path is missing.
        assert!(
            paths.len() >= 80,
            "only {} paths in the document",
            paths.len()
        );
        for path in [
            "/api/cameras",
            "/api/nodes/heartbeat",
            "/api/incidents",
            "/api/health/detailed",
            // One from each auth mode, because the document is the union
            // and a single-mode harvest would be missing one of these.
            "/api/webhooks/clerk",
            "/api/auth/local/login",
        ] {
            assert!(paths.contains_key(path), "{path} missing from the document");
        }
        assert!(
            parsed["components"]["schemas"]
                .as_object()
                .is_some_and(|s| s.len() >= 20),
            "the component schemas did not survive the harvest"
        );
    }

    /// Each shell has to point at the document, or the page loads and
    /// renders nothing — which looks like a working docs route.
    #[test]
    fn each_shell_points_at_the_document() {
        assert!(SWAGGER_HTML.contains("/api/openapi.json"));
        assert!(REDOC_HTML.contains("/api/openapi.json"));
        // And Swagger's redirect URL has to match the route that serves
        // the callback page, or authorising dead-ends.
        // The shell still NAMES the redirect path, and that is fine:
        // Python's shell names it too and Python serves the SPA there.
        // Matching means not intercepting it.
        assert!(SWAGGER_HTML.contains("/docs/oauth2-redirect"));
    }
}
