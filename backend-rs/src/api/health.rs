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

/// 200 or 503 — the only thing an uptime monitor reads.
fn ready_status(ready: bool) -> StatusCode {
    if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Whether this request may be answered from the cache, and with what.
///
/// Pulled out of the handler so the decision can be tested without a
/// database, a Clerk probe and a thirty-second wall clock — and so a
/// mutation to it has somewhere to be caught. The endpoint's own
/// response cannot show it: a cached answer differs from a fresh one
/// only in uptime and latency, and the differential normalises both.
fn serve_from_cache(
    nocache: bool,
    entry: Option<(std::time::Duration, Value, StatusCode)>,
) -> Option<(Value, StatusCode)> {
    if nocache {
        return None;
    }
    let (age, body, status) = entry?;
    cache_is_fresh(age).then_some((body, status))
}

/// Whether a cache entry of this age may still be served.
fn cache_is_fresh(age: std::time::Duration) -> bool {
    age < READY_CACHE_TTL
}

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

    let entry = {
        let guard = READY_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|cached| {
            (
                cached.cached_at.elapsed(),
                cached.body.clone(),
                cached.status,
            )
        })
    };
    if let Some((body, status)) = serve_from_cache(nocache, entry) {
        return Ok((status, Json(body)).into_response());
    }

    let uptime = state.started_at.elapsed().as_secs_f64();
    let report = run_readiness_probes(&state.config, &state.pool, &state.http, uptime).await;

    let mut body = report.to_json();
    body["version"] = json!(VERSION);
    body["uptime_seconds"] = json!(crate::pyrepr::round_to(uptime, 3));
    let status = ready_status(report.ready);

    *READY_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some(ReadyCache {
        cached_at: std::time::Instant::now(),
        body: body.clone(),
        status,
    });
    Ok((status, Json(body)).into_response())
}

/// The viewer-usage backlog's status.
///
/// The flush loop ticks every sixty seconds, so a backlog past a
/// hundred thousand pending writes means it is failing quietly — which
/// is the only symptom that failure has. `warn` colours the dashboard
/// and gates nothing.
///
/// Extracted for the same reason as `rollup`: reaching a six-figure
/// backlog through a harness would mean serving a hundred thousand
/// segments first.
pub fn viewer_usage_status(pending_writes: i64) -> &'static str {
    if pending_writes > 100_000 {
        "warn"
    } else {
        "ok"
    }
}

/// The rolled-up status.
///
/// Critical-tier probes page; warn-tier signals only colour the
/// dashboard. Resend being off or unconfigured is deliberately NOT an
/// input: an install running without email is a configuration choice,
/// not a failure mode, and letting it degrade the rollup would paint
/// every self-hosted install yellow forever.
///
/// Extracted so this is decided by a function with four boolean inputs
/// rather than inside a handler that needs a live database, a real
/// filesystem and a process past its startup grace to reach at all.
pub fn rollup(
    any_critical: bool,
    viewer_warn: bool,
    disk_warn: bool,
    license_warn: bool,
) -> &'static str {
    if any_critical {
        "unhealthy"
    } else if viewer_warn || disk_warn || license_warn {
        "degraded"
    } else {
        "healthy"
    }
}

/// What the queue depth reports when the count query fails.
///
/// -1 says "we do not know". Zero says "the queue is empty", which is a
/// claim, and the wrong one — a status page would render a healthy
/// backlog while the database is unreachable.
pub const UNKNOWN_QUEUE_DEPTH: i64 = -1;

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
        "status": viewer_usage_status(pending_writes),
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
    let queue_depth: i64 = match sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM email_outbox WHERE status = 'pending'",
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok((count,)) => count,
        Err(err) => {
            tracing::warn!(error = %err, "[Health] EmailOutbox count query failed");
            UNKNOWN_QUEUE_DEPTH
        }
    };
    let resend = json!({ "status": resend_status, "queue_depth": queue_depth });

    let critical = [&database, &clerk, &disk, &email_worker];
    let overall = rollup(
        critical.iter().any(|p| p.is_critical()),
        viewer_usage["status"] == "warn",
        disk.status == "warn",
        sentinel_license.status == "warn",
    );

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Critical pages, warn colours, and resend is not an input at all.
    #[test]
    fn the_rollup_separates_paging_from_colouring() {
        assert_eq!(rollup(false, false, false, false), "healthy");
        assert_eq!(rollup(true, false, false, false), "unhealthy");
        // Any one warn degrades.
        assert_eq!(rollup(false, true, false, false), "degraded");
        assert_eq!(rollup(false, false, true, false), "degraded");
        assert_eq!(rollup(false, false, false, true), "degraded");
        // Critical wins over warn — a dead database is not "degraded"
        // just because the disk is also filling.
        assert_eq!(rollup(true, true, true, true), "unhealthy");
    }

    /// A hundred thousand is the line, and it is exclusive.
    #[test]
    fn the_viewer_backlog_warns_only_past_its_threshold() {
        assert_eq!(viewer_usage_status(0), "ok");
        assert_eq!(viewer_usage_status(99_999), "ok");
        assert_eq!(
            viewer_usage_status(100_000),
            "ok",
            "the threshold itself is not past it"
        );
        assert_eq!(viewer_usage_status(100_001), "warn");
        // A negative backlog is not a thing, but it must not warn.
        assert_eq!(viewer_usage_status(-1), "ok");
    }

    /// An unknown queue depth is -1, not 0. A status page tells them
    /// apart and only one of them is a claim about the queue.
    #[test]
    fn an_unknown_queue_depth_is_not_zero() {
        assert_eq!(UNKNOWN_QUEUE_DEPTH, -1);
        assert_ne!(UNKNOWN_QUEUE_DEPTH, 0);
    }

    /// The readiness endpoint's whole reason for existing beside
    /// `detailed`: uptime monitors read the status code.
    #[test]
    fn readiness_maps_ready_to_a_status_code() {
        assert_eq!(ready_status(true), StatusCode::OK);
        assert_eq!(ready_status(false), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// `?nocache=1` bypasses a perfectly fresh entry — that is the
    /// whole point of the parameter, and the reason an on-call
    /// engineer can trust what they get back mid-incident.
    #[test]
    fn the_cache_is_consulted_unless_the_caller_says_not_to() {
        let fresh = || {
            Some((
                std::time::Duration::from_secs(1),
                json!({"ready": true}),
                StatusCode::OK,
            ))
        };
        assert!(
            serve_from_cache(false, fresh()).is_some(),
            "a fresh entry is served"
        );
        assert!(
            serve_from_cache(true, fresh()).is_none(),
            "nocache bypasses it"
        );
        // Nothing cached yet.
        assert!(serve_from_cache(false, None).is_none());
        // Stale.
        let stale = Some((
            READY_CACHE_TTL + std::time::Duration::from_secs(1),
            json!({"ready": true}),
            StatusCode::OK,
        ));
        assert!(serve_from_cache(false, stale).is_none());
    }

    /// Thirty seconds, and the entry is used only inside it. A cache
    /// that never expires freezes the first answer the process gave,
    /// which is the worst thing a health endpoint can do.
    #[test]
    fn the_ready_cache_is_used_only_inside_its_window() {
        assert!(cache_is_fresh(std::time::Duration::from_secs(0)));
        assert!(cache_is_fresh(std::time::Duration::from_secs(29)));
        assert!(!cache_is_fresh(READY_CACHE_TTL));
        assert!(!cache_is_fresh(std::time::Duration::from_secs(31)));
        assert!(!cache_is_fresh(std::time::Duration::from_secs(3600)));
    }
}
