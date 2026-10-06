//! Application state and the route table.
//!
//! The route table was the migration's progress bar: anything not
//! registered here fell through to `proxy::forward` and was still
//! Python. Nothing does now — the proxy is gone and this table is the
//! whole public surface. An unregistered path under `/api` is a 404 from
//! `spa::fallback`, which is what Python's router answered.

use std::sync::Arc;
use std::time::Instant;

use axum::{routing::get, Json, Router};
use serde_json::json;
use tower_http::services::{ServeDir, ServeFile};

use crate::config::Config;
use crate::{api, spa};

#[derive(Clone)]
pub struct AppState {
    pub pool: crate::db::Pool,
    pub config: Arc<Config>,
    /// Shared client for outbound API calls — Clerk, Resend, the licence
    /// and sync services. Reused rather than built per request so those
    /// connections stay pooled.
    pub http: reqwest::Client,
    /// Resolved once at startup: which credential scheme this deployment
    /// runs, and the JWKS cache behind it.
    pub auth: Arc<crate::auth::Authenticator>,
    /// Per-tenant rate limiting for ported routes.
    pub limiter: Arc<crate::ratelimit::Limiter>,
    /// The live video caches. One process owns these: the moment Rust
    /// serves `push-segment`, Python's copy is no longer the one with
    /// the segments in it. See `crate::hls`.
    pub hls: Arc<crate::hls::HlsCache>,
    /// Allowed origins for routes Rust serves; see `cors.rs`.
    pub cors: crate::cors::CorsConfig,
    pub started_at: Instant,
    /// The wall clock at boot, which `/api/health/detailed` reports as
    /// `started_at`. Separate from the monotonic `started_at` because
    /// uptime must not move when the clock is stepped, and a timestamp
    /// in the body must be a real one.
    pub started_at_wall: chrono::DateTime<chrono::Utc>,
}

/// `POST /mcp` — Starlette's mount redirect, reproduced.
///
/// The Location is ABSOLUTE, because that is what Starlette builds: it
/// reconstructs the request URL and appends the slash. A relative one
/// works in a browser and breaks a client that compares hosts.
///
/// **The scheme comes from `X-Forwarded-Proto` first**, and that is not
/// a nicety. `uri().scheme_str()` is `None` for an origin-form request —
/// which is every request from a proxy — so on its own it falls back to
/// `http` and emits `http://…/mcp/` for a request that arrived over
/// HTTPS. Strict MCP clients refuse the downgrade and the connection
/// fails with an unhelpful content-type error.
///
/// This is exactly what uvicorn's `--forwarded-allow-ips=*` does on the
/// Python side, and the Dockerfile's comment there records the same
/// symptom being hit for the same reason. The differential cannot catch
/// it: the harness speaks plain HTTP to both tiers, so both say `http`
/// and agree.
///
/// Trusting the header unconditionally is safe for the same reason
/// uvicorn is configured to: Fly's private network means only their edge
/// can reach this container.
async fn mcp_redirect(request: axum::extract::Request) -> axum::response::Response {
    use axum::response::IntoResponse;
    let forwarded = request
        .headers()
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        // A proxy chain sends a comma-separated list; the first entry is
        // the client-facing scheme.
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let scheme = forwarded
        .or_else(|| request.uri().scheme_str())
        .unwrap_or("http");
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let query = request
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let location = format!("{scheme}://{host}/mcp/{query}");
    (
        axum::http::StatusCode::TEMPORARY_REDIRECT,
        [(axum::http::header::LOCATION, location)],
    )
        .into_response()
}

/// The rmcp streamable-HTTP service, which replaces `fastmcp`.
fn mcp_service(
    state: AppState,
) -> rmcp::transport::streamable_http_server::StreamableHttpService<
    crate::mcp::server::SentinelMcp,
    rmcp::transport::streamable_http_server::session::local::LocalSessionManager,
> {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };
    // Stateless, and deliberately: every request carries its own
    // bearer and resolves its own org, so a session would hold nothing
    // — and per-process session state is exactly what forces a
    // producer and its consumers to move together.
    let mut config = StreamableHttpServerConfig::default();
    config.legacy_session_mode = false;
    // JSON, not SSE. Both are legal for a request/response call and
    // FastMCP answers with JSON when the client accepts it, so a
    // client written against the Python and parsing the body directly
    // would break on an event-stream frame.
    config.json_response = true;
    // rmcp's DNS-rebinding guard allows only `localhost`, `127.0.0.1`
    // and `::1` in `Host` by default, and answers 403 to anything else.
    // That is the right default for an MCP server on a developer's
    // machine with no authentication, and it is fatal here: this server
    // is reached as sentinel-command.com, as *.flycast by the agent, and
    // as whatever a self-hoster calls their box — every one of which was
    // a 403 "Host header is not allowed".
    //
    // Every differential and every test addressed 127.0.0.1, so 150/150
    // MCP cases were identical and none of them could see it. It was
    // found by putting the agent in a container and pointing it at
    // `http://app:8000`. `tests/routing.rs` now sends a foreign Host.
    //
    // Off rather than configured with a list: the guard protects a
    // browser-reachable server that trusts its network position, and
    // this one trusts nothing but the bearer key on each request — which
    // a rebinding page does not have. FastMCP applied no Host check.
    let config = config.disable_allowed_hosts();
    StreamableHttpService::new(
        move || {
            Ok(crate::mcp::server::SentinelMcp {
                state: state.clone(),
            })
        },
        Arc::new(LocalSessionManager::default()),
        config,
    )
}

/// Reported by `/api/health`. Tracks the Python service's version so a
/// client cannot tell which stack answered — during the migration both
/// are the same application.
pub const VERSION: &str = "2.1.2";

pub fn build_router(state: AppState) -> Router {
    let static_dir = state.config.static_dir.clone();
    let index = format!("{static_dir}/index.html");
    let local_auth = state.config.is_local_auth();

    let mut router = Router::new()
        // ---- served by Rust --------------------------------------------
        // `ported`, not a bare `get`: axum answers HEAD from a GET
        // handler and FastAPI returns 405, so even the health check
        // diverged on HEAD.
        .route("/api/health", ported(health))
        .route("/api/health/ready", ported(api::health::health_ready))
        .route("/api/health/detailed", ported(api::health::health_detailed))
        // RFC 9116 requires the .well-known path; the root alias is kept
        // because some older scanners only probe there.
        .route(
            "/.well-known/security.txt",
            ported(api::well_known::security_txt),
        )
        .route("/security.txt", ported(api::well_known::security_txt))
        .route("/install.sh", ported(api::install::install_sh))
        .route("/mcp-setup.sh", ported(api::install::mcp_setup_sh))
        .route("/mcp-setup.ps1", ported(api::install::mcp_setup_ps1))
        // Unblocked by the release cache moving: this one resolves an
        // asset URL from it, and is allowed to fetch — a person waiting
        // on a download can wait, where a node's heartbeat cannot.
        .route(
            "/downloads/{os_name}/{arch}",
            ported(api::install::download_binary),
        )
        .route(
            "/api/incidents/{incident_id}/evidence/{evidence_id}",
            ported(api::incidents::get_evidence_blob),
        )
        .route(
            "/api/incidents/{incident_id}/evidence/{evidence_id}/playlist.m3u8",
            ported(api::incidents::get_evidence_playlist),
        )
        // The Sentinel agent data plane. `/runs/pending` must be
        // registered before `/runs/{run_id}` — FastAPI matches in
        // declaration order and would otherwise read "pending" as a run
        // id; axum prefers the static segment either way, but the order
        // here keeps the two files reading the same.
        .route(
            "/api/sentinel/runs/pending",
            ported(api::sentinel::list_pending_runs),
        )
        .route(
            "/api/sentinel/runs/{run_id}",
            ported(api::sentinel::get_run),
        )
        .route(
            "/api/sentinel/runs/{run_id}/start",
            served(axum::routing::post(api::sentinel::post_run_start)),
        )
        .route(
            "/api/sentinel/runs/{run_id}/complete",
            served(axum::routing::post(api::sentinel::post_run_complete)),
        )
        .route(
            "/api/sentinel/agent-keys",
            served(get(api::sentinel::list_agent_keys).post(api::sentinel::create_agent_key)),
        )
        .route(
            "/api/sentinel/agent-keys/{key_id}",
            served(axum::routing::delete(api::sentinel::revoke_agent_key)),
        )
        .route(
            "/api/sentinel/config",
            served(
                axum::routing::get(api::sentinel_config::get_config)
                    .patch(api::sentinel_config::patch_config),
            ),
        )
        .route(
            "/api/sentinel/runs/manual",
            served(axum::routing::post(api::sentinel_config::post_manual_run)),
        )
        // Registered after the two literal paths above, as in the
        // Python, so `/runs/manual` and `/runs/pending` keep winning.
        .route(
            "/api/sentinel/runs",
            ported(api::sentinel_config::list_runs),
        )
        // Read-only camera routes (slice 2). Writes on these same paths
        // are slice 4 and must still reach Python — hence `ported`
        // rather than a bare `get`.
        .route("/api/cameras", ported(api::cameras::list_cameras))
        .route("/api/cameras/{camera_id}", ported(api::cameras::get_camera))
        .route(
            "/api/camera-groups",
            served(get(api::cameras::list_camera_groups).post(api::groups::create_camera_group)),
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
        .route(
            "/api/cameras/{camera_id}/codec",
            served(axum::routing::post(api::node_writes::report_camera_codec)),
        )
        // Motion stays: it shares hls.py with the two below but not
        // their caches — it reaches the WebSocket module's motion
        // handling, which has not moved.
        .route(
            "/api/cameras/{camera_id}/motion",
            served(axum::routing::post(api::hls::push_motion_event)),
        )
        .route(
            "/api/cameras/{camera_id}/playlist",
            served(axum::routing::post(api::hls::update_hls_playlist)),
        )
        .route(
            "/api/cameras/{camera_id}/push-segment",
            served(axum::routing::post(api::hls::push_segment)),
        )
        .route(
            "/api/cameras/{camera_id}/recording",
            served(axum::routing::post(api::recording::toggle_recording)),
        )
        .route(
            "/api/cameras/{camera_id}/recording-settings",
            served(axum::routing::patch(
                api::recording::update_recording_policy,
            )),
        )
        .route(
            "/api/cameras/{camera_id}/snapshot",
            served(axum::routing::post(api::cameras::take_snapshot)),
        )
        .route(
            "/api/cameras/{camera_id}/stream.m3u8",
            ported(api::hls::get_hls_playlist),
        )
        .route(
            "/api/cameras/{camera_id}/segment/{filename}",
            ported(api::hls::get_hls_segment),
        )
        .route("/api/settings", ported(api::settings::get_all_settings))
        .route(
            "/api/settings/notifications",
            served(
                get(api::settings::get_notification_settings)
                    .post(api::groups::update_notification_settings),
            ),
        )
        .route(
            "/api/settings/timezone",
            served(axum::routing::post(api::timezone::update_org_timezone)),
        )
        .route(
            "/api/settings/motion-ingestion",
            served(
                get(api::settings::get_motion_ingestion).post(api::groups::update_motion_ingestion),
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
        // paths, so every static sibling has to be named out loud.
        .route(
            "/api/nodes/validate",
            served(axum::routing::post(api::node_writes::validate_node)),
        )
        .route(
            "/api/nodes/register",
            served(axum::routing::post(api::node_register::register_node)),
        )
        // Same reason: it clears the caches for the node's cameras.
        .route(
            "/api/nodes/self/decommission",
            served(axum::routing::post(api::nodes::decommission_self)),
        )
        .route(
            "/api/nodes/heartbeat",
            served(axum::routing::post(api::node_register::node_heartbeat)),
        )
        // Moved with the video path, not before it: this reads the
        // viewer-second counter, which lives in whichever process
        // serves segments.
        .route("/api/nodes/plan", ported(api::nodes::get_plan_info))
        .route("/api/nodes/ws-status", ported(api::nodes::ws_status))
        .route("/ws/node", served(get(api::ws::node_websocket)))
        .route(
            "/api/nodes/{node_id}",
            served(get(api::nodes::get_node).delete(api::node_writes::delete_node)),
        )
        .route(
            "/api/nodes/{node_id}/rotate-key",
            served(axum::routing::post(api::node_writes::rotate_api_key)),
        )
        .route(
            "/api/nodes",
            served(get(api::nodes::list_nodes).post(api::node_writes::create_node)),
        )
        .route(
            "/api/settings/danger/wipe-logs",
            served(axum::routing::post(api::node_writes::wipe_stream_logs)),
        )
        // Article 17's erasure, which shares its cascade with the
        // `organization.deleted` webhook so a customer deleting their
        // data and an operator resetting the org end up in the same
        // state.
        .route(
            "/api/settings/danger/full-reset",
            served(axum::routing::post(api::settings::full_reset)),
        )
        // Article 20's export.
        .route(
            "/api/gdpr/export",
            served(axum::routing::post(api::gdpr::export_organization_data)),
        )
        // /counts must be declared here too: it is a static sibling of
        // /{incident_id} and would otherwise be swallowed by it.
        .route(
            "/api/incidents",
            served(get(api::incidents::list_incidents).post(api::incidents::create_incident)),
        )
        .route(
            "/api/incidents/counts",
            ported(api::incidents::incident_counts),
        )
        .route(
            "/api/incidents/{incident_id}",
            served(
                get(api::incidents::get_incident)
                    .patch(api::incidents::update_incident)
                    .delete(api::incidents::delete_incident),
            ),
        )
        .route(
            "/api/mcp/keys",
            served(axum::routing::get(api::keys::list_mcp_keys).post(api::keys::create_mcp_key)),
        )
        // Declared because `/api/mcp/keys/{key_id}` would otherwise be a
        // candidate for it: axum matches a parameterised segment against
        // a literal one, so a static sibling has to be registered too.
        .route("/api/mcp/tools", ported(api::keys::list_mcp_tools))
        .route(
            "/api/mcp/keys/{key_id}",
            served(axum::routing::delete(api::keys::revoke_mcp_key)),
        )
        .route(
            "/api/integration/motion/stream",
            ported(api::integration::motion_stream),
        )
        .route(
            "/api/integration/keys",
            served(get(api::keys::list_integration_keys).post(api::keys::create_integration_key)),
        )
        .route(
            "/api/integration/cameras",
            ported(api::integration::list_cameras),
        )
        .route(
            "/api/integration/cameras/{camera_id}/recording",
            served(axum::routing::post(api::integration::set_recording)),
        )
        .route(
            "/api/integration/cameras/{camera_id}/snapshot",
            ported(api::integration::snapshot),
        )
        .route("/api/integration/status", ported(api::integration::status))
        .route(
            "/api/integration/keys/{key_id}",
            served(axum::routing::delete(api::keys::revoke_integration_key)),
        )
        .route(
            "/api/notifications",
            ported(api::notifications::list_notifications),
        )
        .route(
            "/api/notifications/unread-count",
            ported(api::notifications::unread_count),
        )
        .route(
            "/api/notifications/stream",
            ported(api::notifications::stream_notifications),
        )
        .route(
            "/api/notifications/mark-viewed",
            served(axum::routing::post(api::notifications::mark_viewed)),
        )
        .route(
            "/api/notifications/clear-all",
            served(axum::routing::post(api::notifications::clear_all)),
        )
        .route(
            "/api/notifications/request-admin-promotion",
            served(axum::routing::post(
                api::notifications::request_admin_promotion,
            )),
        )
        .route(
            "/api/notifications/email/unsubscribe",
            ported(api::notifications::email_unsubscribe),
        )
        .route(
            "/api/notifications/email/preferences",
            served(
                get(api::notifications::get_email_preferences)
                    .post(api::notifications::update_email_preferences),
            ),
        )
        .route(
            "/api/motion/events",
            ported(api::motion::list_motion_events),
        )
        .route(
            "/api/motion/events/stream",
            ported(api::motion::stream_motion_events),
        )
        .route(
            "/api/motion/events/stats",
            ported(api::motion::motion_stats),
        )
        // The whole activity router, including the four routes that
        // read the in-memory tracker — which only became portable when
        // the MCP server that FILLS that tracker moved too, in this
        // same slice. See api/mcp_activity.rs.
        .route(
            "/api/mcp/activity/logs",
            ported(api::mcp_activity::list_mcp_logs),
        )
        .route(
            "/api/mcp/activity/logs/stats",
            ported(api::mcp_activity::mcp_log_stats),
        )
        .route(
            "/api/mcp/activity/recent",
            ported(api::mcp_activity::recent_activity),
        )
        .route(
            "/api/mcp/activity/sessions",
            ported(api::mcp_activity::active_sessions),
        )
        .route(
            "/api/mcp/activity/stats",
            ported(api::mcp_activity::activity_stats),
        )
        .route(
            "/api/mcp/activity/stream",
            ported(api::mcp_activity::stream_activity),
        )
        // ---- the MCP protocol surface -----------------------------------
        //
        // Python mounts FastMCP's ASGI app at `/mcp`, and Starlette's
        // mount redirects the un-slashed path — so `POST /mcp` is a 307
        // to `/mcp/` and only `/mcp/` carries the protocol. Both are
        // reproduced, because a client that follows the redirect once
        // and caches it would otherwise break.
        //
        // GET is NOT claimed. Python's SPA middleware answers every GET
        // under this path with the React page, `/mcp/` included — which
        // is why these two use `served_spa`: their method fallback is the
        // dashboard document, not a 405.
        //
        // Both carry `mcp::pre_auth::layer`, which is where the
        // `Content-Length` requirement and the 2 MB cap now live. They
        // used to sit in `spa::fallback` and applied because `/mcp` was
        // forwarded through it; registering these routes moved the
        // endpoint in front of that fallback and silently disabled them.
        // A `route_layer` keeps them on the two paths that need them and
        // off the CameraNode uploads, which are entitled to send a body
        // without declaring its length.
        .route(
            "/mcp",
            served_spa(axum::routing::post(mcp_redirect))
                .route_layer(axum::middleware::from_fn(crate::mcp::pre_auth::layer)),
        )
        .route(
            "/mcp/",
            served_spa(axum::routing::post_service(mcp_service(state.clone())))
                .route_layer(axum::middleware::from_fn(crate::mcp::pre_auth::layer)),
        )
        // ---- SPA --------------------------------------------------------
        // Static assets are files on disk, served directly rather than
        // through the fallback's path-walking.
        .nest_service("/assets", ServeDir::new(format!("{static_dir}/assets")))
        .route_service(
            "/favicon.svg",
            ServeFile::new(format!("{static_dir}/favicon.svg")),
        );

    // Registered only in local mode, matching main.py: the Python
    // mounts this router in the `else` branch of is_clerk_auth(), so
    // under Clerk these paths do not exist. Claiming them here would
    // answer 503 where Python answers 404 — and would advertise a
    // self-hosted login on a hosted deployment.
    if !local_auth {
        // The mirror image: main.py mounts the webhooks router only
        // under Clerk. A self-hosted install has no Clerk account to
        // send webhooks and no secret to verify them, so the path must
        // not exist there — 404, from the fallback, as Python answered.
        router = router
            .route(
                "/api/webhooks/resend",
                served(axum::routing::post(api::webhooks::resend_webhook)),
            )
            .route(
                "/api/webhooks/clerk",
                served(axum::routing::post(api::clerk_webhook::clerk_webhook)),
            );
    }
    if local_auth {
        router = router
            .route(
                "/api/auth/local/login",
                served(axum::routing::post(api::local_auth::login)),
            )
            .route(
                "/api/auth/local/refresh",
                served(axum::routing::post(api::local_auth::refresh)),
            );
    }

    // ---- the API documentation surface ---------------------------------
    // The last four routes Python answered. The schema is FastAPI's own,
    // harvested and compiled in rather than rewritten — see api/docs.rs
    // for why that is the honest option and what keeps the snapshot from
    // going stale. OFF in production (`config::api_docs_enabled`): not
    // registering them is what turns them off, exactly as FastAPI's
    // `docs_url=None` did: all three then answer the API 404, because
    // the SPA fallback's `/api` pass-through covers `/api-docs` too.
    if state.config.api_docs_enabled {
        router = router
            .route(
                "/api/openapi.json",
                served(axum::routing::get(api::docs::openapi_json)),
            )
            .route(
                "/api-docs",
                served(axum::routing::get(api::docs::swagger_ui)),
            )
            .route("/api-redoc", served(axum::routing::get(api::docs::redoc)));
    }

    router
        // ---- the SPA ---------------------------------------------------
        // Deliberately last. `spa::fallback` serves the React document
        // and the files beside it, and answers the router's 404 for the
        // prefixes the SPA must not swallow.
        .fallback(spa::fallback)
        // FastAPI's envelope on the 405s axum's method fallback produces.
        // Inside the header layers so those still see the final response.
        .layer(axum::middleware::from_fn(method_not_allowed_body))
        // A NUL byte in the path or query is refused before any handler
        // can bind it — see `refuse_nul_bytes`.
        .layer(axum::middleware::from_fn(refuse_nul_bytes))
        // CORS for routes Rust answers itself.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::cors::layer,
        ))
        // Request id and the security header set. Outside the CORS layer
        // so it sees the final response.
        .layer(axum::middleware::from_fn(crate::headers::layer))
        .with_state(state)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        // `index` for the SPA fallback, which owns every client-side
        // route.
        .layer(axum::Extension(IndexPath(index)))
        // Sentry, outermost, and in this order because the order is the
        // whole point:
        //
        // * `NewSentryLayer` gives each request its own hub, which is
        //   what makes `sentry::set_user_context` from the auth
        //   extractor safe. Without it the tags go on a thread-local
        //   scope, and under a multi-threaded runtime one request's
        //   org_id would end up on the next error raised by whatever
        //   task landed on that thread. A mislabelled error in a
        //   multi-tenant tracker is worse than an unlabelled one.
        // * `SentryHttpLayer` attaches the request (method, path) and
        //   starts a transaction. It reads `send_default_pii` off the
        //   initialised client, so with PII off it strips the sensitive
        //   headers itself — `sentry::scrub` is the second line, not the
        //   first.
        //
        // Both are no-ops with no DSN: no client, nothing captured.
        .layer(sentry::integrations::tower::SentryHttpLayer::new().enable_transaction())
        .layer(sentry::integrations::tower::NewSentryLayer::new_from_top())
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
/// Every route goes through this, and what it adds is the HEAD handler.
/// axum answers HEAD from a GET handler automatically; FastAPI's
/// `APIRoute` does not declare HEAD at all, so Starlette raises 405. A
/// ported route without this line quietly starts accepting a method the
/// service never accepted — which was fixed once in `ported()` alone, and
/// the four routes registered by hand kept the bug for another commit.
///
/// There is deliberately **no** method fallback any more. It used to
/// forward unported verbs to Python; axum's own fallback is what should
/// answer now, because it computes the `Allow` header from the methods
/// actually registered. `method_not_allowed_body` gives that response
/// FastAPI's body without taking the header away.
fn served(router: axum::routing::MethodRouter<AppState>) -> axum::routing::MethodRouter<AppState> {
    router.head(head_not_allowed)
}

/// `served`, for the two paths whose other methods belong to the SPA.
///
/// `/mcp` and `/mcp/` are POST-only routes sitting under a path the React
/// app owns: Python's SPA middleware is outermost, so `GET /mcp` is the
/// dashboard page and never reaches a router at all. A 405 there would
/// break the MCP page in the dashboard.
fn served_spa(
    router: axum::routing::MethodRouter<AppState>,
) -> axum::routing::MethodRouter<AppState> {
    router
        .head(spa::method_fallback)
        .fallback(spa::method_fallback)
}

/// HEAD on a route that declares only GET.
///
/// Returns a bare 405: `method_not_allowed_body` fills in the envelope on
/// the way out, the same as it does for axum's own method fallback. The
/// `Allow` header is missing here and present there, which is the one
/// place this service does not reproduce Starlette exactly — see
/// `expected_divergences.md`.
async fn head_not_allowed() -> axum::http::StatusCode {
    axum::http::StatusCode::METHOD_NOT_ALLOWED
}

/// A `%00` anywhere in the path or query is a 400, for every route.
///
/// PostgreSQL refuses a NUL byte in a text value, so a request that
/// carried one into a query was a 500 — an alert for what is only a
/// malformed URL — while the SQLite build stored it and answered. A NUL
/// can only reach a URL percent-encoded, so checking the raw text here
/// covers every handler at once and makes the two builds agree.
async fn refuse_nul_bytes(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let has_nul = |text: &str| {
        text.as_bytes()
            .windows(3)
            .any(|w| w[0] == b'%' && w[1] == b'0' && w[2] == b'0')
    };
    let uri = request.uri();
    if has_nul(uri.path()) || uri.query().is_some_and(has_nul) {
        use axum::response::IntoResponse;
        return crate::error::ApiError::bad_request("Invalid request: the URL contains a NUL byte")
            .into_response();
    }
    next.run(request).await
}

/// Give a bodyless 405 the envelope FastAPI would have sent.
///
/// axum's method fallback answers `405` with an `Allow` header and no
/// body; Starlette raises `HTTPException(405)` and FastAPI serialises it
/// as `{"detail": "Method Not Allowed"}`. Rather than hand-write an
/// `Allow` at each of ~110 routes — which axum already computes and
/// which Python builds from a `set`, so its order is not even stable
/// across processes — the body is filled in here.
///
/// The discriminator is the absence of a content type. Every response a
/// handler produces sets one, and no handler in this service returns 405,
/// so an untyped 405 is always the router's.
async fn method_not_allowed_body(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let response = next.run(request).await;
    if response.status() != axum::http::StatusCode::METHOD_NOT_ALLOWED
        || response
            .headers()
            .contains_key(axum::http::header::CONTENT_TYPE)
    {
        return response;
    }
    let (mut parts, _) = response.into_parts();
    let body = serde_json::json!({ "detail": "Method Not Allowed" }).to_string();
    parts.headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    axum::response::Response::from_parts(parts, axum::body::Body::from(body))
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

#[cfg(test)]
mod tests {
    /// The redirect's scheme comes from `X-Forwarded-Proto` when there
    /// is one, because `uri().scheme_str()` is None for every request a
    /// proxy forwards. Without this the port emits an HTTPS->HTTP
    /// downgrade that strict MCP clients refuse — and the differential
    /// cannot see it, since the harness speaks plain HTTP to both tiers
    /// and they agree on `http`.
    #[tokio::test]
    async fn the_mcp_redirect_honours_the_forwarded_scheme() {
        async fn location(headers: &[(&str, &str)]) -> String {
            let mut builder = axum::http::Request::builder().method("POST").uri("/mcp");
            for (name, value) in headers {
                builder = builder.header(*name, *value);
            }
            let request = builder.body(axum::body::Body::empty()).unwrap();
            let response = super::mcp_redirect(request).await;
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        }

        assert_eq!(
            location(&[
                ("host", "sentinel-command.com"),
                ("x-forwarded-proto", "https")
            ])
            .await,
            "https://sentinel-command.com/mcp/"
        );
        // A proxy chain sends a list; the first entry is the
        // client-facing scheme.
        assert_eq!(
            location(&[
                ("host", "sentinel-command.com"),
                ("x-forwarded-proto", "https, http"),
            ])
            .await,
            "https://sentinel-command.com/mcp/"
        );
        // No header: http, which is what a direct plaintext request is.
        assert_eq!(
            location(&[("host", "127.0.0.1:8000")]).await,
            "http://127.0.0.1:8000/mcp/"
        );
        // An empty header is not an answer.
        assert_eq!(
            location(&[("host", "h"), ("x-forwarded-proto", "")]).await,
            "http://h/mcp/"
        );
    }
}
