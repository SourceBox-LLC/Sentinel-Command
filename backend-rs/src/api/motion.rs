//! `/api/motion/events` and its stats sibling.
//!
//! Ported from `backend/app/api/motion.py`. `/events/stream` is Server-
//! Sent Events backed by an in-process broadcaster and stays with
//! Python — it belongs with the WebSocket work, not here.

use axum::extract::{Request, State};
use chrono::{Duration, NaiveDateTime};
use serde_json::{json, Value};

use crate::app::AppState;
use crate::auth::RequireView;
use crate::error::ApiError;
use crate::models::{iso_naive, now_naive, python_window_start};
use crate::pyint::PyInt;
use crate::query::Query;

#[derive(Debug, sqlx::FromRow)]
struct MotionEventRow {
    id: i32,
    org_id: String,
    camera_id: String,
    node_id: String,
    score: i32,
    segment_seq: Option<i32>,
    timestamp: Option<NaiveDateTime>,
}

impl MotionEventRow {
    fn to_json(&self) -> Value {
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
