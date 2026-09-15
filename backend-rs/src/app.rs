//! Application state and the route table.
//!
//! The route table is the migration's progress bar. Anything registered
//! here is served by Rust; anything not registered falls through to
//! `proxy::forward` and is still Python. Slices move routes up out of the
//! fallback one group at a time.

use std::sync::Arc;
use std::time::Instant;

use axum::{routing::get, Json, Router};
use serde_json::json;
use tower_http::services::{ServeDir, ServeFile};

use crate::config::Config;
use crate::{api, proxy};

#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::PgPool,
    pub config: Arc<Config>,
    /// Shared client for the strangler proxy and outbound API calls.
    /// Reused rather than built per request so connections to the Python
    /// upstream stay pooled.
    pub http: reqwest::Client,
    /// Resolved once at startup: which credential scheme this deployment
    /// runs, and the JWKS cache behind it.
    pub auth: Arc<crate::auth::Authenticator>,
    /// Upstream client for the strangler proxy. Separate from `http`
    /// because it must not normalise request paths — see `proxy.rs`.
    pub proxy: proxy::ProxyClient,
    pub started_at: Instant,
}

/// Reported by `/api/health`. Tracks the Python service's version so a
/// client cannot tell which stack answered — during the migration both
/// are the same application.
pub const VERSION: &str = "2.1.2";

pub fn build_router(state: AppState) -> Router {
    let static_dir = state.config.static_dir.clone();
    let index = format!("{static_dir}/index.html");

    Router::new()
        // ---- served by Rust --------------------------------------------
        .route("/api/health", get(health))
        // Read-only camera routes (slice 2). Writes on these same paths
        // are slice 4 and must still reach Python — hence `ported`
        // rather than a bare `get`.
        .route("/api/cameras", ported(api::cameras::list_cameras))
        .route("/api/cameras/{camera_id}", ported(api::cameras::get_camera))
        .route("/api/camera-groups", ported(api::cameras::list_camera_groups))
        // ---- SPA --------------------------------------------------------
        // Static assets are files on disk; serving them through the Python
        // proxy would double the cost of every page load for no reason.
        .nest_service("/assets", ServeDir::new(format!("{static_dir}/assets")))
        .route_service("/favicon.svg", ServeFile::new(format!("{static_dir}/favicon.svg")))
        // ---- everything else is still Python ---------------------------
        // Deliberately last. Every slice that lands removes routes from
        // this fallback; when it forwards nothing, the Python process and
        // proxy.rs are deleted together.
        .fallback(proxy::forward)
        .with_state(state)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        // `index` is captured for the SPA fallback once client-side routes
        // are served from Rust; until then unknown paths are Python's to
        // answer, because it still owns the catch-all that returns
        // index.html.
        .layer(axum::Extension(IndexPath(index)))
}

/// A GET that Rust has ported, on a path whose other methods Python
/// still owns.
///
/// Registering a bare `get(handler)` would make axum answer every other
/// method on that path with 405 instead of forwarding it — so the moment
/// `GET /api/cameras` moved over, `POST /api/cameras` would stop working.
/// The method-level fallback keeps the unported verbs flowing to Python.
///
/// Drop the `.fallback` only once every method on the path is ported.
fn ported<H, T>(handler: H) -> axum::routing::MethodRouter<AppState>
where
    H: axum::handler::Handler<T, AppState>,
    T: 'static,
{
    get(handler).fallback(proxy::forward)
}

/// Path to the SPA entrypoint, carried so the eventual client-side-route
/// fallback has it without re-reading config.
#[derive(Clone)]
pub struct IndexPath(pub String);

/// Pure liveness — must never be slow. This is what Fly's health check
/// polls; a slow dependency must not pull the only machine out of
/// rotation. Matches the Python body exactly.
async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "healthy", "version": VERSION }))
}
