//! `/api/motion/events` and its stats sibling.
//!
//! Ported from `backend/app/api/motion.py`, including both Server-Sent
//! Event feeds.
//!
//! **There are two broadcasters, not one, and that is the point.** The
//! dashboard and the Home Assistant integration keep separate
//! subscriber pools so a Home Assistant install polling a handful of
//! `binary_sensor`s cannot consume the dashboard's per-tier slots — or
//! the other way round. They carry identical payloads; only the
//! budgets are separate.

use axum::extract::{Request, State};
use chrono::{Duration, NaiveDateTime};
use serde_json::{json, Value};

use crate::app::AppState;
use crate::auth::RequireView;
use crate::error::ApiError;
use crate::models::{iso_naive, now_naive, python_window_start};
use crate::pyint::PyInt;
use crate::query::Query;

/// `motion_broadcaster` — the dashboard's feed.
pub static BROADCASTER: crate::sse::Broadcaster = crate::sse::Broadcaster::new("motion");

/// `integration_motion_broadcaster` — Home Assistant's, with its own
/// budget.
pub static INTEGRATION_BROADCASTER: crate::sse::Broadcaster =
    crate::sse::Broadcaster::new("integration-motion");

/// The frame both feeds carry for one motion event.
///
/// `json.dumps` order, which is the order the dict was built in.
pub fn motion_frame(camera_id: &str, node_id: &str, score: i32, timestamp: &str) -> String {
    crate::audit::python_json(&[
        ("type", json!("motion")),
        ("camera_id", json!(camera_id)),
        ("node_id", json!(node_id)),
        ("score", json!(score)),
        ("timestamp", json!(timestamp)),
    ])
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct MotionEventRow {
    pub(crate) id: i32,
    pub(crate) org_id: String,
    pub(crate) camera_id: String,
    pub(crate) node_id: String,
    pub(crate) score: i32,
    pub(crate) segment_seq: Option<i32>,
    pub(crate) timestamp: Option<NaiveDateTime>,
}

impl MotionEventRow {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "org_id": self.org_id,
            "camera_id": self.camera_id,
            "node_id": self.node_id,
            "score": self.score,
            "segment_seq": self.segment_seq,
            "timestamp": self.timestamp.map(iso_naive),
        })
    }
}

/// `GET /api/motion/events` — recent motion, newest first.
pub async fn list_motion_events(
    State(state): State<AppState>,
    RequireView(user): RequireView,
    request: Request,
) -> Result<axum::Json<Value>, ApiError> {
    let mut q = Query::parse(request.uri().query());
    let camera_id = q.optional_str("camera_id");
    let hours = q.int("hours", 24, 1, 168);
    let limit = q.int("limit", 100, 1, 500);
    let offset = q.int("offset", 0, 0, 1_000_000);
    q.finish()?;

    let since = now_naive() - Duration::hours(hours);

    let mut where_sql = String::from(" WHERE org_id = $1 AND timestamp >= $2");
    if camera_id.is_some() {
        where_sql.push_str(" AND camera_id = $3");
    }

    let count_sql = format!("SELECT COUNT(*) FROM motion_events{where_sql}");
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql)
        .bind(&user.org_id)
        .bind(since);
    if let Some(ref c) = camera_id {
        cq = cq.bind(c.clone());
    }
    let total: i64 = cq.fetch_one(&state.pool).await?;

    let page_sql = format!(
        "SELECT id, org_id, camera_id, node_id, score, segment_seq, timestamp \
         FROM motion_events{where_sql} \
         ORDER BY timestamp DESC OFFSET {offset} LIMIT {limit}"
    );
    let mut pq = sqlx::query_as::<_, MotionEventRow>(&page_sql)
        .bind(&user.org_id)
        .bind(since);
    if let Some(ref c) = camera_id {
        pq = pq.bind(c.clone());
    }
    let events: Vec<MotionEventRow> = pq.fetch_all(&state.pool).await?;

    Ok(axum::Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        // `hours` is echoed back so the dashboard can label the window
        // it is showing without tracking what it asked for.
        "hours": hours,
        "events": events.iter().map(MotionEventRow::to_json).collect::<Vec<_>>(),
    })))
}

/// `GET /api/motion/events/stats` — per-camera aggregates.
///
/// `hours` is capped at 168 but has no lower bound in the Python, so a
/// negative value puts the window in the future and yields an empty
/// list. Reproduced rather than tightened: the two differ only for a
/// caller who is already sending nonsense, and tightening it here would
/// make the ported route reject input the Python accepts.
pub async fn motion_stats(
    State(state): State<AppState>,
    RequireView(user): RequireView,
    request: Request,
) -> Result<axum::Json<Value>, ApiError> {
    let mut q = Query::parse(request.uri().query());
    let hours = q.big_int("hours", PyInt::Small(24), None, Some(168));
    q.finish()?;

    let since = python_window_start(hours, 3_600)?;
    // `python_window_start` refused anything that does not fit, so the
    // value echoed back is the one Python echoes.
    let hours = hours.small().unwrap_or_default();

    // No ORDER BY, matching the Python's bare `.group_by(...).all()`.
    let rows: Vec<(String, i64, Option<i32>, Option<NaiveDateTime>)> = sqlx::query_as(
        "SELECT camera_id, COUNT(id) AS count, MAX(score) AS peak_score, \
                MAX(timestamp) AS latest \
           FROM motion_events WHERE org_id = $1 AND timestamp >= $2 \
          GROUP BY camera_id",
    )
    .bind(&user.org_id)
    .bind(since)
    .fetch_all(&state.pool)
    .await?;

    Ok(axum::Json(json!({
        "hours": hours,
        "cameras": rows.iter()
            .map(|(camera_id, count, peak, latest)| json!({
                "camera_id": camera_id,
                "event_count": count,
                "peak_score": peak,
                "latest": latest.map(iso_naive),
            }))
            .collect::<Vec<_>>(),
    })))
}

/// `GET /api/motion/events/stream` — the dashboard's live feed.
///
/// Rate-limited on connects for the same reason the notification
/// stream is: the per-org subscriber cap stops streams accumulating,
/// but without a connect limit a client can churn open → cap-hit →
/// reject and burn a JWT verification each cycle.
pub async fn stream_motion_events(
    rate: crate::ratelimit::PerMinute<60>,
    RequireView(user): RequireView,
) -> Result<axum::response::Response, ApiError> {
    rate.check().await?;
    let cap = crate::plans::get_plan_limits(&user.plan)
        .max_sse_subscribers
        .max(0) as usize;
    // No audience filter on this feed: every motion event is visible to
    // any member who can see the camera.
    let Some(subscription) = BROADCASTER.subscribe(&user.org_id, true, cap) else {
        return Err(ApiError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            format!(
                "Too many open motion streams for this org (cap: {cap} on your current \
                 plan). Close unused dashboard tabs and retry, or upgrade for a higher cap."
            ),
        ));
    };
    Ok(crate::sse::stream_response(
        subscription,
        crate::sse::connected_frame(&user.org_id),
    ))
}
