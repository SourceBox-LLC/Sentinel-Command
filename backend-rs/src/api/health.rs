//! `/api/health/ready` and `/api/health/detailed`.
//!
//! Ported from `backend/app/main.py`. Three endpoints, three audiences,
//! and the split is deliberate:
//!
//!   `/api/health`          liveness, no probes. Fly polls it per
//!                          machine, and it must never time out — a
//!                          slow database removing the machine from
//!                          rotation is the opposite of what anyone
//!                          wants, because there is no other machine
//!                          to fail over to.
//!   `/api/health/ready`    readiness, 503 when a critical probe
//!                          fails. Uptime monitors read HTTP status.
//!   `/api/health/detailed` always 200, everything in the body. A
//!                          status page reads JSON.
//!
//! `detailed` is public on purpose, and the surface is metric-shaped:
//! latencies, cache occupancy, queue depths — never an org id, a
//! camera id or an address. A privacy regression test on the Python
//! side pins that, and it is the reason nothing here interpolates a
//! tenant identifier into the response.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Map, Value};

use crate::app::{AppState, VERSION};
use crate::error::ApiError;
use crate::health_probes::{
    probe_clerk, probe_database, probe_disk, probe_email_worker, probe_sentinel_license,
    run_readiness_probes,
};

/// `_HEALTH_READY_CACHE_TTL_SECONDS`.
///
/// Long enough that a swarm of pollers — two uptime monitors, a status
/// page and the admin panel — does not hammer Clerk; short enough that
/// a real outage surfaces inside half a minute. Two concurrent misses
/// both run the probes, which is cheaper than locking the hot path.
const READY_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

struct ReadyCache {
    cached_at: std::time::Instant,
    body: Value,
    status: StatusCode,
}

static READY_CACHE: std::sync::Mutex<Option<ReadyCache>> = std::sync::Mutex::new(None);

/// Test-facing, matching `_reset_health_ready_cache_for_tests`.
pub fn reset_ready_cache() {
    *READY_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// `GET /api/health/ready`.
pub async fn health_ready(
    State(state): State<AppState>,
    request: Request,
) -> Result<axum::response::Response, ApiError> {
    // `nocache: bool = False` on the signature, so FastAPI coerces it
    // with the same lax rules as any other bool and a value it cannot
    // read is a 422 — not a silent false, which would quietly disable
    // the cache bypass an on-call engineer just asked for.
    let mut q = crate::query::Query::parse(request.uri().query());
    let nocache = q.bool("nocache", false);
    q.finish()?;

    if !nocache {
        let hit = {
            let guard = READY_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().and_then(|cached| {
                (cached.cached_at.elapsed() < READY_CACHE_TTL)
                    .then(|| (cached.body.clone(), cached.status))
            })
        };
        if let Some((body, status)) = hit {
            return Ok((status, Json(body)).into_response());
        }
    }

    let uptime = state.started_at.elapsed().as_secs_f64();
    let report =
        run_readiness_probes(&state.config, &state.pool, &state.http, uptime).await;

    let mut body = report.to_json();
    body["version"] = json!(VERSION);
    body["uptime_seconds"] = json!(crate::pyrepr::round_to(uptime, 3));
    let status = if report.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    *READY_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some(ReadyCache {
        cached_at: std::time::Instant::now(),
        body: body.clone(),
        status,
    });
    Ok((status, Json(body)).into_response())
}

/// `GET /api/health/detailed`.
pub async fn health_detailed(State(state): State<AppState>) -> Json<Value> {
    let uptime = crate::pyrepr::round_to(state.started_at.elapsed().as_secs_f64(), 3);

    // The same probe functions readiness uses. Two implementations
    // would eventually disagree, and "ready says up, detailed says
    // down" is the one answer a status page cannot act on.
    let (database, clerk) = tokio::join!(
        probe_database(&state.pool),
        probe_clerk(&state.config, &state.http),
    );
    let disk = probe_disk();
    let email_worker = probe_email_worker(&state.config, uptime);
    let sentinel_license = probe_sentinel_license(&state.config, &state.pool, uptime).await;

    // Read without locking anything for long: a momentary inconsistency
    // in a count is fine for a status page and not worth blocking the
    // serve path over.
    let (playlists_cached, segment_cameras) = state.hls.cache_occupancy();
    let hls_cache = json!({
        "status": "ok",
        "playlists_cached": playlists_cached,
        "segment_cameras": segment_cameras,
    });

    let pending_writes = state.hls.pending_viewer_seconds();
    let viewer_usage = json!({
        // The flush loop ticks every sixty seconds; a backlog past this
        // means it is failing quietly. `warn` does not gate liveness.
        "status": if pending_writes > 100_000 { "warn" } else { "ok" },
        "pending_writes": pending_writes,
    });

    let (subscriber_orgs, subscriber_total) = crate::notifications::BROADCASTER.counts();
    let sse = json!({
        "status": "ok",
        "subscriber_orgs": subscriber_orgs,
        "subscriber_total": subscriber_total,
    });

    // Three states, separated so an operator can tell "I forgot the
    // secret" from "I left the switch off" at a glance.
    let resend_status = if !state.config.email_enabled {
        "disabled"
    } else if !state.config.is_email_configured() {
        "unconfigured"
    } else {
        "ok"
    };
    // A count query must not fail the health endpoint; -1 says "we do
    // not know" rather than pretending the queue is empty.
    let queue_depth: i64 =
        match sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM email_outbox WHERE status = 'pending'")
            .fetch_one(&state.pool)
            .await
        {
            Ok((count,)) => count,
            Err(err) => {
                tracing::warn!(error = %err, "[Health] EmailOutbox count query failed");
                -1
            }
        };
    let resend = json!({ "status": resend_status, "queue_depth": queue_depth });

    // Critical-tier probes page; warn-tier signals only colour the
    // dashboard. Resend being off or unconfigured is a deliberate
    // choice and degrades nothing.
    let critical = [&database, &clerk, &disk, &email_worker];
    let overall = if critical.iter().any(|p| p.is_critical()) {
        "unhealthy"
    } else if viewer_usage["status"] == "warn"
        || disk.status == "warn"
        || sentinel_license.status == "warn"
    {
        "degraded"
    } else {
        "healthy"
    };

    let mut checks = Map::new();
    checks.insert("database".into(), database.to_json());
    checks.insert("clerk".into(), clerk.to_json());
    checks.insert("disk".into(), disk.to_json());
    checks.insert("email_worker".into(), email_worker.to_json());
    checks.insert("hls_cache".into(), hls_cache);
    checks.insert("viewer_usage".into(), viewer_usage);
    checks.insert("sse".into(), sse);
    checks.insert("resend".into(), resend);
    checks.insert("sentinel_license".into(), sentinel_license.to_json());

    Json(json!({
        "status": overall,
        "version": VERSION,
        "uptime_seconds": uptime,
        // Both are `datetime.now(tz=UTC).isoformat()` — AWARE, so
        // they carry a "+00:00" that a naive format would drop.
        "started_at": crate::api::nodes::iso_aware(state.started_at_wall.naive_utc(), 0),
        "time": crate::api::nodes::iso_aware(chrono::Utc::now().naive_utc(), 0),
        "checks": Value::Object(checks),
    }))
}
