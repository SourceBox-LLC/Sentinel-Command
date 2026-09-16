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
    /// Per-tenant rate limiting for ported routes.
    pub limiter: Arc<crate::ratelimit::Limiter>,
    /// Allowed origins for routes Rust serves; see `cors.rs`.
    pub cors: crate::cors::CorsConfig,
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
        // `ported`, not a bare `get`: axum answers HEAD from a GET
        // handler and FastAPI returns 405, so even the health check
        // diverged on HEAD.
        .route("/api/health", ported(health))
        // Read-only camera routes (slice 2). Writes on these same paths
        // are slice 4 and must still reach Python — hence `ported`
        // rather than a bare `get`.
        .route("/api/cameras", ported(api::cameras::list_cameras))
        .route("/api/cameras/{camera_id}", ported(api::cameras::get_camera))
        .route(
            "/api/camera-groups",
            served(
                get(api::cameras::list_camera_groups).post(api::groups::create_camera_group),
            ),
        )
        .route(
            "/api/camera-groups/{group_id}",
            served(axum::routing::delete(api::groups::delete_camera_group)),
        )
        .route(
            "/api/cameras/{camera_id}/group",
            served(axum::routing::put(api::groups::assign_camera_group)),
        )
        // Siblings of /{camera_id}/group. The route-capture guard caught
        // these the moment that route landed — including push-segment,
        // the hot video ingest path, which a 404 here would have taken
        // down. All still belong to Python.
        .route("/api/cameras/{camera_id}/codec", still_python())
        .route("/api/cameras/{camera_id}/motion", still_python())
        .route("/api/cameras/{camera_id}/playlist", still_python())
        .route("/api/cameras/{camera_id}/push-segment", still_python())
        .route(
            "/api/cameras/{camera_id}/recording",
            served(axum::routing::post(api::recording::toggle_recording)),
        )
        .route(
            "/api/cameras/{camera_id}/recording-settings",
            served(axum::routing::patch(api::recording::update_recording_policy)),
        )
        .route("/api/cameras/{camera_id}/snapshot", still_python())
        .route("/api/cameras/{camera_id}/stream.m3u8", still_python())
        .route("/api/settings", ported(api::settings::get_all_settings))
        .route(
            "/api/settings/notifications",
            served(
                get(api::settings::get_notification_settings)
                    .post(api::groups::update_notification_settings),
            ),
        )
        .route(
            "/api/settings/motion-ingestion",
            served(
                get(api::settings::get_motion_ingestion)
                    .post(api::groups::update_motion_ingestion),
            ),
        )
        .route("/api/audit-logs", ported(api::audit::list_audit_logs))
        .route(
            "/api/audit/stream-logs",
            ported(api::stream_logs::list_stream_logs),
        )
        .route(
            "/api/audit/stream-logs/stats",
            ported(api::stream_logs::stream_log_stats),
        )
        // Only the single-node read. GET /api/nodes itself is blocked on
        // release_cache — see api/nodes.rs.
        //
        // The static siblings MUST be declared alongside it. A route
        // pattern of `/api/nodes/{node_id}` matches `/api/nodes/plan`
        // with node_id="plan", so porting the parameterised route
        // silently captured three paths that still belong to Python and
        // answered them 404. FastAPI is saved from this by declaration
        // order; axum has no ordering between separately registered
        // paths, so the statics are pinned to the proxy explicitly.
        .route("/api/nodes/validate", still_python())
        .route("/api/nodes/register", still_python())
        .route("/api/nodes/heartbeat", still_python())
        .route("/api/nodes/plan", still_python())
        .route("/api/nodes/ws-status", still_python())
        .route("/api/nodes/{node_id}", ported(api::nodes::get_node))
        // /counts must be declared here too: it is a static sibling of
        // /{incident_id} and would otherwise be swallowed. It is ported
        // rather than pinned, so it is a real route, not a proxy pin.
        .route("/api/incidents", ported(api::incidents::list_incidents))
        .route("/api/incidents/counts", ported(api::incidents::incident_counts))
        .route(
            "/api/incidents/{incident_id}",
            served(
                get(api::incidents::get_incident)
                    .patch(api::incidents::update_incident)
                    .delete(api::incidents::delete_incident),
            ),
        )
        .route("/api/mcp/keys", ported(api::keys::list_mcp_keys))
        .route("/api/integration/keys", ported(api::keys::list_integration_keys))
        .route(
            "/api/integration/keys/{key_id}",
            served(axum::routing::delete(api::keys::revoke_integration_key)),
        )
        .route("/api/motion/events", ported(api::motion::list_motion_events))
        .route(
            "/api/motion/events/stats",
            ported(api::motion::motion_stats),
        )
        // Only the DB-backed MCP routes. /recent, /sessions and /stats
        // read an in-memory tracker inside the Python process and stay
        // proxied — see api/mcp_activity.rs.
        .route(
            "/api/mcp/activity/logs",
            ported(api::mcp_activity::list_mcp_logs),
        )
        .route(
            "/api/mcp/activity/logs/stats",
            ported(api::mcp_activity::mcp_log_stats),
        )
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
        // CORS for routes Rust answers itself. Applied to the whole
        // router but a no-op on proxied responses, which already carry
        // Python's headers — a second Access-Control-Allow-Origin makes
        // the browser reject the response outright.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::cors::layer,
        ))
        // Request id and the security header set. Outside the CORS layer
        // so it sees the final response, and a no-op on proxied
        // responses, which already carry Python's.
        .layer(axum::middleware::from_fn(crate::headers::layer))
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
    served(get(handler))
}

/// Finish a method router for a path Rust serves.
///
/// Every route goes through this, and it exists because forgetting
/// either line is silent:
///
/// * `.head(proxy::forward)` — axum answers HEAD from a GET handler
///   automatically and FastAPI returns 405, so a ported route quietly
///   starts accepting a method Python refuses. This was fixed once in
///   `ported()` alone, and the four routes registered by hand kept the
///   bug for another commit.
/// * `.fallback(proxy::forward)` — without it axum answers every
///   unported method on the path with 405 instead of forwarding it, so
///   porting `GET /api/cameras` would break `POST /api/cameras`.
fn served(router: axum::routing::MethodRouter<AppState>) -> axum::routing::MethodRouter<AppState> {
    router.head(proxy::forward).fallback(proxy::forward)
}

/// A path Rust must not answer, pinned so a parameterised sibling
/// cannot swallow it.
///
/// Needed because `/a/{id}` matches `/a/literal`. Without this, porting
/// a `{id}` route quietly takes over every static path beside it.
fn still_python() -> axum::routing::MethodRouter<AppState> {
    axum::routing::any(proxy::forward)
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
