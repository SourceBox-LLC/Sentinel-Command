//! What answers a request that no handler wants.
//!
//! These cases were the proxy's job until the Python was deleted, and
//! they are the ones a differential harness can no longer check — there
//! is nothing left to diff against. So they are pinned here instead,
//! against what the Python demonstrably did:
//!
//! * an unknown path under `/api` is the router's 404, `{"detail": "Not
//!   Found"}`, and **not** the React document. Serving `index.html` with
//!   a 200 to a CameraNode asking for a route that no longer exists is
//!   the failure mode the pass-through list exists to prevent, and the
//!   one nothing would notice.
//! * a known path with an unknown method is `405` with `{"detail":
//!   "Method Not Allowed"}`, because FastAPI's `APIRoute` declares its
//!   methods and Starlette raises for the rest. `HEAD` is in that set:
//!   `APIRoute` never adds it, so `HEAD /api/health` is a 405 even
//!   though `GET /api/health` is fine. axum would have answered HEAD
//!   from the GET handler.
//! * `/mcp` is the exception to both, because Python's SPA middleware is
//!   outermost and answers every non-POST under that path with the
//!   dashboard page. `GET /mcp` is a React route; only `POST` is the
//!   protocol.
//! * `POST /mcp` passes the pre-auth gates first — `Content-Length`
//!   required, 2 MB cap — which is the pair that silently stopped
//!   applying when `/mcp/` became a real route rather than a forwarded
//!   one.
//!
//! No database is touched: the pool is lazy and every case here is
//! answered by the router before a handler runs. That is the point —
//! these are routing answers, and a routing test that needs a database
//! is a routing test that will not be run.
//!
//! The router is built **once**, in a `OnceCell`, and that is not an
//! optimisation. `Config::from_env` reads the process environment, and
//! the cases below run as threads in one process: a per-test
//! `set_var("STATIC_DIR", …)` meant one test could read another's value
//! and serve a directory whose `index.html` had not been written yet.
//! It passed on its own and failed inside a full `cargo test` run, which
//! is the worst way for a test to be wrong.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sentinel_command::app::{build_router, AppState};
use sentinel_command::config::Config;
use tower::ServiceExt;

/// The one router every case shares, with its static directory.
static APP: tokio::sync::OnceCell<(std::path::PathBuf, axum::Router)> =
    tokio::sync::OnceCell::const_new();

async fn app() -> &'static (std::path::PathBuf, axum::Router) {
    APP.get_or_init(build).await
}

/// A router over a static directory holding a recognisable index.
///
/// The document has to exist: the SPA fallback serves the file from disk,
/// so a missing build would answer 404 and every `/mcp` case below would
/// pass for the wrong reason.
async fn build() -> (std::path::PathBuf, axum::Router) {
    let dir = tempdir();
    std::fs::write(dir.join("index.html"), "<!doctype html><title>SPA</title>").unwrap();
    std::env::set_var("STATIC_DIR", &dir);
    std::env::set_var(
        "DATABASE_URL",
        "postgresql://unused:unused@127.0.0.1:1/unused",
    );
    // Local auth, so the case set does not depend on a Clerk key being
    // present in the environment.
    std::env::set_var("AUTH_PROVIDER", "local");
    std::env::set_var("APP_SECRET_KEY", "x".repeat(32));

    let config = Config::from_env();
    // Lazy: nothing here reaches a handler, and requiring a live
    // Postgres would put these cases behind an env var.
    let pool = sentinel_command::db::PoolOptions::new()
        .connect_lazy(&config.database_url)
        .unwrap();
    let http = reqwest::Client::new();
    let state = AppState {
        auth: Arc::new(sentinel_command::auth::Authenticator::from_config(
            &config,
            http.clone(),
        )),
        cors: sentinel_command::cors::CorsConfig::from_env(&config.frontend_url, ""),
        hls: Arc::new(sentinel_command::hls::HlsCache::new()),
        limiter: Arc::new(sentinel_command::ratelimit::Limiter::from_env("").await),
        http,
        config: Arc::new(config),
        pool,
        started_at: Instant::now(),
        started_at_wall: chrono::Utc::now(),
    };
    (dir, build_router(state))
}

async fn send(
    router: &axum::Router,
    request: Request<Body>,
) -> (StatusCode, Vec<(String, String)>, String) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, headers, String::from_utf8_lossy(&body).to_string())
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn with(method: &str, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn an_unknown_api_path_is_a_router_404_not_the_spa() {
    let (_dir, app) = app().await;

    for path in [
        "/api/nope",
        "/api/cameras/extra/segments",
        "/ws/nope",
        "/install.bat",
        "/mcp-setup.bat",
        "/downloads/nope",
        "/.well-known/nope",
    ] {
        let (status, _, body) = send(app, get(path)).await;
        assert_eq!(status, 404, "{path}");
        assert_eq!(body, r#"{"detail":"Not Found"}"#, "{path}");
    }
}

#[tokio::test]
async fn an_unknown_front_end_path_is_the_spa_document() {
    let (_dir, app) = app().await;

    // Client-side routes, which the React router owns.
    for path in ["/", "/dashboard", "/settings", "/incidents", "/docs"] {
        let (status, headers, body) = send(app, get(path)).await;
        assert_eq!(status, 200, "{path}");
        assert!(body.contains("SPA"), "{path} served {body:?}");
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "content-type" && v == "text/html; charset=utf-8"),
            "{path} content type"
        );
    }
}

#[tokio::test]
async fn a_known_path_with_an_unknown_method_is_405_with_the_fastapi_body() {
    let (_dir, app) = app().await;

    let (status, headers, body) = send(app, with("POST", "/api/health", &[])).await;
    assert_eq!(status, 405);
    assert_eq!(body, r#"{"detail":"Method Not Allowed"}"#);
    // axum's own method fallback computes this, which is why it is left
    // in place rather than replaced by a handler.
    assert!(
        headers.iter().any(|(k, _)| k == "allow"),
        "the 405 should still carry Allow: {headers:?}"
    );
}

/// FastAPI's `APIRoute` does not add HEAD to a GET route, so Starlette
/// raises 405 — where axum would have answered from the GET handler and
/// quietly accepted a method the service never accepted.
#[tokio::test]
async fn head_is_405_on_a_get_route() {
    let (_dir, app) = app().await;

    let (status, _, _) = send(app, with("HEAD", "/api/health", &[])).await;
    assert_eq!(status, 405);
    // And the GET it shadows still works, so this is about the method
    // and not about the route being broken.
    let (status, _, body) = send(app, get("/api/health")).await;
    assert_eq!(status, 200);
    assert!(body.contains("healthy"), "{body}");
}

/// `/mcp` is a POST-only route under a path the SPA owns. Python's
/// middleware never consulted a method table for it.
#[tokio::test]
async fn every_non_post_on_mcp_is_the_dashboard_page() {
    let (_dir, app) = app().await;

    for (method, path) in [
        ("GET", "/mcp"),
        ("GET", "/mcp/"),
        ("HEAD", "/mcp"),
        ("PUT", "/mcp"),
        ("DELETE", "/mcp/"),
    ] {
        let (status, _, body) = send(app, with(method, path, &[])).await;
        assert_eq!(status, 200, "{method} {path}");
        // HEAD has no body by definition; the status is the assertion.
        if method != "HEAD" {
            assert!(body.contains("SPA"), "{method} {path} served {body:?}");
        }
    }
}

/// The gates that stopped applying when `/mcp/` became a route.
#[tokio::test]
async fn post_mcp_is_gated_before_anything_reads_the_body() {
    let (_dir, app) = app().await;

    // No Content-Length — the check a `Transfer-Encoding: chunked`
    // request would otherwise walk straight past.
    let (status, _, body) = send(app, with("POST", "/mcp/", &[])).await;
    assert_eq!(status, 411);
    assert_eq!(body, r#"{"error":"Content-Length required."}"#);

    let (status, _, body) =
        send(app, with("POST", "/mcp/", &[("content-length", "2097153")])).await;
    assert_eq!(status, 413);
    assert_eq!(body, r#"{"error":"Request body too large (max 2 MB)."}"#);

    let (status, _, body) = send(
        app,
        with("POST", "/mcp/", &[("content-length", "not-a-number")]),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(body, r#"{"error":"Invalid Content-Length."}"#);

    // Exactly at the cap is allowed through — and reaches the transport,
    // which rejects it for its own reasons rather than the gate's.
    let (status, _, _) = send(
        app,
        with(
            "POST",
            "/mcp/",
            &[
                ("content-length", "2097152"),
                ("content-type", "application/json"),
            ],
        ),
    )
    .await;
    assert_ne!(status, 411);
    assert_ne!(status, 413);

    // The un-slashed path is Starlette's mount redirect, and it is
    // gated too.
    let (status, _, _) = send(app, with("POST", "/mcp", &[])).await;
    assert_eq!(status, 411);
    let (status, headers, _) = send(
        app,
        with("POST", "/mcp", &[("content-length", "0"), ("host", "h")]),
    )
    .await;
    assert_eq!(status, 307);
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "location" && v.ends_with("/mcp/")),
        "{headers:?}"
    );
}

/// A path *under* the MCP mount. Python routed it into FastMCP, which
/// had no such route; the gates still ran on the way in.
#[tokio::test]
async fn a_path_under_mcp_is_gated_and_then_404() {
    let (_dir, app) = app().await;

    let (status, _, body) = send(app, with("POST", "/mcp/messages", &[])).await;
    assert_eq!(
        status, 411,
        "gated before the path is even considered: {body}"
    );

    let (status, _, body) = send(
        app,
        with("POST", "/mcp/messages", &[("content-length", "0")]),
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(body, r#"{"detail":"Not Found"}"#);
}

/// A GET under `/mcp` that looks like a file must not escape the static
/// root. The fallback walks the path itself, so this is its rule and not
/// Starlette's.
#[tokio::test]
async fn the_static_walk_cannot_escape_its_root() {
    let (dir, app) = app().await;
    std::fs::write(dir.join("real.txt"), "inside").unwrap();

    let (status, _, body) = send(app, get("/real.txt")).await;
    assert_eq!(status, 200);
    assert_eq!(body, "inside");

    // Traversal lands on the index rather than on a file above the root.
    for path in [
        "/../Cargo.toml",
        "/a/../../Cargo.toml",
        "/%2e%2e/Cargo.toml",
    ] {
        let (status, _, body) = send(app, get(path)).await;
        assert_eq!(status, 200, "{path}");
        assert!(body.contains("SPA"), "{path} served {body:?}");
    }
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sentinel-routing-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The MCP server answers under whatever name it is reached by.
///
/// rmcp allows only `localhost` / `127.0.0.1` / `::1` in `Host` unless
/// told otherwise, and every harness in this repository addressed
/// 127.0.0.1 — so the Rust tier would have answered 403 to every MCP
/// client in production, with 150/150 differential cases green. Whatever
/// this request ends in (no key is presented, and there is no database),
/// it must not be that.
#[tokio::test]
async fn mcp_is_not_restricted_to_a_localhost_host_header() {
    let (_dir, app) = app().await;
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

    for host in [
        "sentinel-command.com",
        "sentinel-command.flycast:8080",
        "app:8000",
    ] {
        let request = Request::builder()
            .method("POST")
            .uri("/mcp/")
            .header("host", host)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("content-length", body.len().to_string())
            .body(Body::from(body))
            .unwrap();
        let (status, _, text) = send(app, request).await;
        assert!(
            !text.contains("Host header is not allowed"),
            "Host: {host} was refused by the DNS-rebinding guard ({status}): {text}"
        );
        assert_ne!(status, 403, "Host: {host} → {text}");
    }
}

/// A NUL byte in the URL is the caller's error on every route, not a
/// 500 from PostgreSQL refusing to bind it.
#[tokio::test]
async fn a_nul_byte_in_the_url_is_a_400_everywhere() {
    let (_dir, app) = app().await;
    for path in [
        "/api/cameras/cam%00live",
        "/api/audit-logs?username=%00",
        "/api/health?x=a%00b",
    ] {
        let (status, _, body) = send(app, with("GET", path, &[])).await;
        assert_eq!(status, 400, "{path}: {body}");
        assert!(body.contains("NUL"), "{path}: {body}");
    }
    // `%000` is a NUL followed by a 0; `%2500` is a literal "%00", which
    // must NOT be refused.
    let (status, _, _) = send(app, with("GET", "/api/health?x=%2500", &[])).await;
    assert_ne!(status, 400);
}
