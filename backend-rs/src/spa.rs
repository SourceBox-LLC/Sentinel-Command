//! The SPA fallback: the React document and the static files beside it.
//!
//! Ported from `main.py`'s `spa_middleware`, which is the LAST middleware
//! registered and therefore the OUTERMOST — and which returns
//! `FileResponse` objects directly, without calling down the stack. That
//! shape is the source of two behaviours worth naming before the code:
//!
//! **It stamps the security headers itself.** Because nothing below it
//! runs, `_apply_security_headers` is called explicitly on both file
//! paths. Its docstring records what happened before that was factored
//! out: "the dashboard HTML document and every /assets/* file shipped
//! with NO X-Frame-Options / nosniff / HSTS — i.e. the one response where
//! frame-ancestors actually matters was the one being skipped, leaving
//! the dashboard clickjackable."
//!
//! **It does NOT stamp `X-Request-Id`.** The same skipped stack, and this
//! half was never fixed — PYTHON_BUGS #15. Rust stamps it via a layer
//! that this fallback cannot opt out of, which is a deliberate
//! divergence recorded in `expected_divergences.md`: nothing can depend
//! on the header's absence, and matching it would mean writing code to
//! strip it.
//!
//! The pass-through list is the interesting part of the port, because
//! every entry on it is a route that would otherwise be served the React
//! index and break in a way nobody would notice from the response code.
//! `/.well-known/` and `/security.txt` are the clearest: a security
//! scanner greps for a file and gets an HTML document with a 200.
//!
//! **Since the proxy went, this file also answers what Python's router
//! used to.** A request that reaches here on a pass-through path had a
//! real route once and has none now, so it gets Starlette's own answer:
//! `{"detail": "Not Found"}`, or `{"detail": "Method Not Allowed"}` when
//! the path matched and the method did not. The distinction is the whole
//! reason there are two entry points below rather than one — a `POST` to
//! a GET-only route must not be told the route does not exist.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};

use crate::app::AppState;

/// Prefixes the SPA never answers.
///
/// Each one is a real route, and being served `index.html` instead would
/// fail quietly — a 200 with an HTML body, which most clients read as
/// success. `/docs` is deliberately absent: it IS a front-end route now
/// (the docs moved to the marketing site and the SPA redirects there),
/// and `/downloads/` is deliberately present, because it is the
/// backend's binary-redirect route rather than a page.
const PASS_THROUGH: [&str; 7] = [
    "/api",
    "/ws",
    "/install.",
    "/mcp-setup.",
    "/downloads/",
    "/.well-known/",
    "/security.txt",
];

/// Whether this path is one the SPA must not answer.
///
/// Also any dot-file path (`/.env`, `/.git/config`, `/foo/.htaccess`):
/// the app has none, and answering them `200 text/html` made every
/// automated scanner report a leaked `.env` that was really the React
/// page. `/.well-known/` is a real route and is matched first.
pub fn passes_through(path: &str) -> bool {
    PASS_THROUGH.iter().any(|prefix| path.starts_with(prefix))
        || path.split('/').any(|segment| segment.starts_with('.'))
}

/// The router's fallback: no route matched this path at all.
///
/// The pass-through list is not belt-and-braces here. These prefixes are
/// the service's own, so a path under one of them that matched nothing is
/// a 404 from the router — not the React document, which would answer
/// `200 text/html` to a CameraNode asking for an endpoint that no longer
/// exists.
pub async fn fallback(State(state): State<AppState>, request: Request) -> Response {
    let path = request.uri().path().to_string();

    if passes_through(&path) {
        return detail(404, "Not Found");
    }

    // `POST /mcp…` is the protocol, not the dashboard. `/mcp` and `/mcp/`
    // are real routes and never reach here; what does is a path *under*
    // them, which in Python entered FastMCP's mount and got its 404. The
    // pre-auth gates still run first, because the cost they bound —
    // reading a body before anyone is authenticated — is the same
    // whatever the path turns out to be.
    if path.starts_with("/mcp") && request.method() == axum::http::Method::POST {
        let peer = request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.0.ip().to_string());
        if let Some(refusal) = crate::mcp::pre_auth::gate(request.headers(), peer.as_deref()) {
            return refusal;
        }
        return detail(404, "Not Found");
    }

    document(&state, &path).await
}

/// A path Rust serves, reached with a method it does not serve.
///
/// Only the `/mcp` pair uses this. Python's SPA middleware is OUTERMOST,
/// so for a path it owns it never consults the router's method table at
/// all: `GET /mcp` is the dashboard page, and so is `PUT /mcp`. Every
/// other route answers 405 through axum's own method fallback, which is
/// left in place because it computes the `Allow` header.
pub async fn method_fallback(State(state): State<AppState>, request: Request) -> Response {
    document(&state, request.uri().path()).await
}

/// A real file under the static root, else the React document.
///
/// Both are served with the security headers stamped, which the layer
/// would also do — but Python stamps them here explicitly and the two
/// have to agree on a response that skips its own middleware stack.
async fn document(state: &AppState, path: &str) -> Response {
    let root = std::path::Path::new(&state.config.static_dir);
    if let Some(file) = safe_join(root, path) {
        if file.is_file() {
            return serve_file(&file).await;
        }
    }

    // `if not request.url.path.startswith("/api")` — already true here,
    // since `/api` passes through above. Kept as the index fallback for
    // every client-side route the SPA owns.
    let index = root.join("index.html");
    if index.is_file() {
        return serve_file(&index).await;
    }

    // No build on disk — a dev run of the binary alone, or a broken
    // image. Python registered its SPA middleware only when the static
    // directory existed, so the request fell through to the router and
    // 404ed; same answer, reached the same way.
    detail(404, "Not Found")
}

/// `HTTPException(status, detail)` as FastAPI serialises it.
///
/// The router's 404 and 405 use this envelope, not the `{"error": …}`
/// shape `json_error` writes — those two are different Python code paths
/// and a client branching on the body sees the difference.
pub fn detail(status: u16, message: &str) -> Response {
    (
        axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::NOT_FOUND),
        axum::Json(serde_json::json!({ "detail": message })),
    )
        .into_response()
}

/// Join a request path onto the static root without escaping it.
///
/// Python does `static_dir / request.url.path.lstrip("/")` and relies on
/// Starlette having already normalised the path. This does not rely on
/// that: a component that is `..`, absolute, or a root/prefix marker
/// rejects the whole path. A traversal here reads any file the process
/// can, which on Fly includes the volume and the environment.
fn safe_join(root: &std::path::Path, path: &str) -> Option<std::path::PathBuf> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let mut out = root.to_path_buf();
    for part in trimmed.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return None;
        }
        let component = std::path::Path::new(part);
        // A single component should be exactly that. Anything Rust
        // parses as a root, a prefix or a parent is not a filename.
        if component.components().count() != 1
            || !matches!(
                component.components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            return None;
        }
        out.push(part);
    }
    Some(out)
}

async fn serve_file(path: &std::path::Path) -> Response {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let mime = mime_for(path);
            ([(axum::http::header::CONTENT_TYPE, mime)], bytes).into_response()
        }
        // Raced a deletion between `is_file` and the read.
        Err(_) => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

/// The media types Starlette's `FileResponse` guesses for a React build.
///
/// `text/html; charset=utf-8` matters most: the document is the response
/// the read differential compares, and a bare `text/html` is a different
/// header. Starlette appends the charset for every `text/*`.
fn mime_for(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("ico") => "image/vnd.microsoft.icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("txt") => "text/plain; charset=utf-8",
        Some("map") => "application/json",
        _ => "application/octet-stream",
    }
}

/// The shape `JSONResponse({"error": ...}, status_code=...)` produces —
/// a bare `error` key, not the `ApiError` envelope. These three answers
/// predate that envelope and a client parsing them would break on it.
pub fn json_error(status: u16, message: &str) -> Response {
    let mut response = (
        axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::BAD_REQUEST),
        axum::Json(serde_json::json!({ "error": message })),
    )
        .into_response();
    if status == 429 {
        response
            .headers_mut()
            .insert(axum::http::header::RETRY_AFTER, "60".parse().unwrap());
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_files_are_not_answered_with_the_app() {
        for path in ["/.env", "/.git/config", "/assets/.DS_Store", "/a/.htaccess"] {
            assert!(passes_through(path), "{path}");
        }
        for path in [
            "/dashboard",
            "/settings",
            "/assets/index-abc.js",
            "/legal/terms.v2",
        ] {
            assert!(!passes_through(path), "{path}");
        }
    }

    /// Every pass-through prefix, and the two that are deliberately NOT
    /// on the list.
    #[test]
    fn the_pass_through_list_is_what_python_lists() {
        for path in [
            "/api/cameras",
            "/ws/node",
            "/install.sh",
            "/mcp-setup.ps1",
            "/downloads/linux/x86_64",
            "/.well-known/security.txt",
            "/security.txt",
        ] {
            assert!(passes_through(path), "{path} must not be served the SPA");
        }
        // `/docs` IS a front-end route — the docs moved to the marketing
        // site and the SPA redirects there.
        assert!(!passes_through("/docs"));
        // And `/mcp` is the dashboard on GET; only POST is the protocol,
        // which the handler branches on rather than the prefix list.
        assert!(!passes_through("/mcp"));
        assert!(!passes_through("/dashboard"));
        assert!(!passes_through("/"));
    }

    /// A traversal must not resolve. Python leans on Starlette having
    /// normalised the path; this does not lean on anything.
    #[test]
    fn a_traversal_does_not_escape_the_static_root() {
        let root = std::path::Path::new("/srv/static");
        for path in [
            "/../etc/passwd",
            "/assets/../../etc/passwd",
            "/..%2fetc/passwd/..",
            "/a/../../b",
        ] {
            let joined = safe_join(root, path);
            if let Some(joined) = joined {
                assert!(
                    joined.starts_with(root),
                    "{path} escaped to {}",
                    joined.display()
                );
            }
        }
        assert_eq!(safe_join(root, "/.."), None);
        assert_eq!(safe_join(root, "/assets/../.."), None);
        // An ordinary path still resolves, or the fallback serves
        // nothing at all.
        assert_eq!(
            safe_join(root, "/assets/app.js"),
            Some(std::path::PathBuf::from("/srv/static/assets/app.js"))
        );
        // The bare root has no file to serve; the index handles it.
        assert_eq!(safe_join(root, "/"), None);
    }

    /// Starlette appends a charset to every `text/*`, and the document's
    /// content type is compared by the read differential.
    #[test]
    fn the_document_carries_a_charset() {
        assert_eq!(
            mime_for(std::path::Path::new("/x/index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            mime_for(std::path::Path::new("/x/app.css")),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            mime_for(std::path::Path::new("/x/logo.svg")),
            "image/svg+xml"
        );
        assert_eq!(
            mime_for(std::path::Path::new("/x/blob")),
            "application/octet-stream"
        );
    }
}
