//! `/api/mcp/activity/*` — the whole router.
//!
//! Ported from `backend/app/api/mcp_activity.py`.
//!
//! Two of these routes read the database and four read an in-memory
//! tracker, and the split mattered: while the MCP server was still
//! Python's, a Rust `/recent` would have answered from an empty tracker
//! and reported that nothing had happened — a difference no differential
//! could see, because both trackers are empty in a test environment
//! until something calls a tool.
//!
//! So the four moved WITH their producer, in the same slice that ported
//! `mcp/server.py`. `mcp::activity::TRACKER` is now the only writer and
//! the only reader; the Python's tracker is dead code behind the proxy.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use chrono::{NaiveDate, NaiveDateTime};
use serde_json::{json, Value};

use crate::app::AppState;
use crate::auth::RequireAdmin;
use crate::csv_export::Cell;
use crate::error::ApiError;
use crate::models::{iso_naive, python_window_start};
use crate::pyint::PyInt;
use crate::query::Query;
use crate::ratelimit::PerMinute;

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct McpActivityLogRow {
    pub(crate) id: i32,
    pub(crate) org_id: String,
    pub(crate) tool_name: String,
    pub(crate) key_name: String,
    pub(crate) status: String,
    pub(crate) duration_ms: Option<i32>,
    pub(crate) args_summary: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) timestamp: Option<NaiveDateTime>,
}

impl McpActivityLogRow {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "org_id": self.org_id,
            "tool_name": self.tool_name,
            "key_name": self.key_name,
            "status": self.status,
            "duration_ms": self.duration_ms,
            "args_summary": self.args_summary,
            "error": self.error,
            "timestamp": self.timestamp.map(iso_naive),
        })
    }
}

/// `GET /api/mcp/activity/logs`.
pub async fn list_mcp_logs(
    // Python: @limiter.limit("120/minute")
    rate: PerMinute<120>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    request: Request,
) -> Result<Response, ApiError> {
    let mut q = Query::parse(request.uri().query());
    let tool_name = q.optional_str("tool_name");
    let key_name = q.optional_str("key_name");
    let status = q.optional_str("status");
    let limit = q.int("limit", 100, 1, 500);
    let offset = q.int("offset", 0, 0, 1_000_000);
    let format = q.pattern("format", "json", "^(json|csv)$", &["json", "csv"]);
    q.finish()?;
    rate.check().await?;

    let mut where_sql = String::from(" WHERE org_id = $1");
    let mut binds: Vec<String> = vec![user.org_id.clone()];
    if let Some(ref t) = tool_name {
        binds.push(t.clone());
        where_sql.push_str(&format!(" AND tool_name = ${}", binds.len()));
    }
    if let Some(ref k) = key_name {
        binds.push(format!("%{}%", super::audit::escape_like(k)));
        // Escaped here, unlike /api/audit/stream-logs: key names are
        // operator-chosen and routinely contain underscores.
        where_sql.push_str(&format!(
            " AND key_name {} ${} ESCAPE '\\'",
            crate::db::ILIKE,
            binds.len()
        ));
    }
    if let Some(ref s) = status {
        binds.push(s.clone());
        where_sql.push_str(&format!(" AND status = ${}", binds.len()));
    }

    // CSV bypasses `limit`/`offset` for a flat 50,000-row window, with
    // the same filters applied.
    if format == "csv" {
        return csv_export(&state, &user.org_id, &where_sql, binds);
    }

    let count_sql = format!("SELECT COUNT(*) FROM mcp_activity_logs{where_sql}");
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql).bind(&user.org_id);
    if let Some(ref t) = tool_name {
        cq = cq.bind(t.clone());
    }
    if let Some(ref k) = key_name {
        cq = cq.bind(format!("%{}%", super::audit::escape_like(k)));
    }
    if let Some(ref s) = status {
        cq = cq.bind(s.clone());
    }
    let total: i64 = cq.fetch_one(&state.pool).await?;

    let page_sql = format!(
        "SELECT id, org_id, tool_name, key_name, status, duration_ms, args_summary, \
                error, timestamp \
           FROM mcp_activity_logs{where_sql} \
          ORDER BY timestamp DESC NULLS FIRST LIMIT {limit} OFFSET {offset}"
    );
    let mut pq = sqlx::query_as::<_, McpActivityLogRow>(&page_sql).bind(&user.org_id);
    if let Some(ref t) = tool_name {
        pq = pq.bind(t.clone());
    }
    if let Some(ref k) = key_name {
        pq = pq.bind(format!("%{}%", super::audit::escape_like(k)));
    }
    if let Some(ref s) = status {
        pq = pq.bind(s.clone());
    }
    let logs: Vec<McpActivityLogRow> = pq.fetch_all(&state.pool).await?;

    Ok(axum::Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "logs": logs.iter().map(McpActivityLogRow::to_json).collect::<Vec<_>>(),
    }))
    .into_response())
}

/// The `?format=csv` window for MCP activity.
///
/// `duration_ms` is the one non-text column any of these three exports
/// has, and it is why `Cell` distinguishes text from raw — see
/// `crate::csv_export`.
fn csv_export(
    state: &AppState,
    org_id: &str,
    where_sql: &str,
    binds: Vec<String>,
) -> Result<Response, ApiError> {
    let sql = format!(
        "SELECT timestamp, tool_name, key_name, status, duration_ms, args_summary, error \
           FROM mcp_activity_logs{where_sql} ORDER BY timestamp DESC NULLS FIRST LIMIT 50000"
    );
    let rows = crate::csv_export::stream_rows(state.pool.clone(), sql, binds, |row| {
        use sqlx::Row;
        Ok(vec![
            Cell::Text(
                row.try_get::<Option<NaiveDateTime>, _>("timestamp")?
                    .map(iso_naive)
                    .unwrap_or_default(),
            ),
            Cell::text(row.try_get("tool_name")?),
            Cell::text(row.try_get("key_name")?),
            Cell::text(row.try_get("status")?),
            Cell::int(row.try_get("duration_ms")?),
            Cell::text(row.try_get("args_summary")?),
            Cell::text(row.try_get("error")?),
        ])
    });
    crate::csv_export::stream_csv_response(
        &crate::csv_export::filename_for("mcp-activity-log", Some(org_id)),
        &[
            "timestamp",
            "tool_name",
            "key_name",
            "status",
            "duration_ms",
            "args_summary",
            "error",
        ],
        rows,
    )
}

/// `GET /api/mcp/activity/logs/stats`.
pub async fn mcp_log_stats(
    // Python: @limiter.limit("60/minute")
    rate: PerMinute<60>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    request: Request,
) -> Result<axum::Json<Value>, ApiError> {
    let mut q = Query::parse(request.uri().query());
    let days = q.big_int("days", PyInt::Small(7), None, Some(30));
    q.finish()?;
    rate.check().await?;

    let since = python_window_start(days, 86_400)?;
    // `python_window_start` refused anything that does not fit, so the
    // value echoed back is the one Python echoes.
    let days = days.small().unwrap_or_default();

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mcp_activity_logs WHERE org_id = $1 AND timestamp >= $2",
    )
    .bind(&user.org_id)
    .bind(since)
    .fetch_one(&state.pool)
    .await?;

    let errors: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mcp_activity_logs \
          WHERE org_id = $1 AND timestamp >= $2 AND status = 'error'",
    )
    .bind(&user.org_id)
    .bind(since)
    .fetch_one(&state.pool)
    .await?;

    let by_tool: Vec<(String, i64)> = sqlx::query_as(
        "SELECT tool_name, COUNT(id) AS count FROM mcp_activity_logs \
          WHERE org_id = $1 AND timestamp >= $2 \
          GROUP BY tool_name ORDER BY COUNT(id) DESC",
    )
    .bind(&user.org_id)
    .bind(since)
    .fetch_all(&state.pool)
    .await?;

    let by_key: Vec<(String, i64)> = sqlx::query_as(
        "SELECT key_name, COUNT(id) AS count FROM mcp_activity_logs \
          WHERE org_id = $1 AND timestamp >= $2 \
          GROUP BY key_name ORDER BY COUNT(id) DESC",
    )
    .bind(&user.org_id)
    .bind(since)
    .fetch_all(&state.pool)
    .await?;

    let by_day: Vec<(Option<NaiveDate>, i64)> = sqlx::query_as(
        "SELECT date(timestamp) AS date, COUNT(id) AS count FROM mcp_activity_logs \
          WHERE org_id = $1 AND timestamp >= $2 \
          GROUP BY date(timestamp) ORDER BY date(timestamp) DESC NULLS FIRST",
    )
    .bind(&user.org_id)
    .bind(since)
    .fetch_all(&state.pool)
    .await?;

    Ok(axum::Json(json!({
        "days": days,
        "total_calls": total,
        "total_errors": errors,
        "by_tool": by_tool.iter()
            .map(|(t, n)| json!({"tool_name": t, "count": n}))
            .collect::<Vec<_>>(),
        "by_key": by_key.iter()
            .map(|(k, n)| json!({"key_name": k, "count": n}))
            .collect::<Vec<_>>(),
        "by_day": by_day.iter()
            .map(|(d, n)| json!({
                "date": d.map(|d| d.format("%Y-%m-%d").to_string()),
                "count": n,
            }))
            .collect::<Vec<_>>(),
    })))
}

// ---------------------------------------------------------------------
// The in-memory tracker's routes.
//
// These four are the reason the MCP server could not be left in Python:
// the tool wrapper is what fills the tracker, so the producer and every
// consumer had to move together. See `tests/differential/in_process_state.md`.
// ---------------------------------------------------------------------

/// `GET /api/mcp/activity/recent`.
pub async fn recent_activity(
    RequireAdmin(user): RequireAdmin,
    request: Request,
) -> Result<Response, ApiError> {
    let mut q = Query::parse(request.uri().query());
    // `le=500` matches the tracker's own ring size. Without it an
    // accidental `limit=10000` would be silently clamped inside the
    // tracker; a 422 keeps the contract honest.
    let limit = q.int("limit", 50, 1, 500);
    q.finish()?;

    let events = crate::mcp::activity::TRACKER.recent_events(&user.org_id, limit as usize);
    Ok(axum::Json(
        events
            .iter()
            .map(crate::mcp::activity::McpEvent::to_json)
            .collect::<Vec<_>>(),
    )
    .into_response())
}

/// `GET /api/mcp/activity/sessions`.
pub async fn active_sessions(RequireAdmin(user): RequireAdmin) -> Response {
    let sessions = crate::mcp::activity::TRACKER.active_sessions(&user.org_id, now_seconds());
    axum::Json(sessions).into_response()
}

/// `GET /api/mcp/activity/stats`.
pub async fn activity_stats(RequireAdmin(user): RequireAdmin) -> Response {
    let stats = crate::mcp::activity::TRACKER.stats(&user.org_id, now_seconds());
    axum::Json(stats).into_response()
}

/// `GET /api/mcp/activity/stream` — the live tool-call feed.
pub async fn stream_activity(
    // Python: @limiter.limit("60/minute") — connect attempts, not
    // frames. Same threat model as the notification bell's stream.
    rate: PerMinute<60>,
    RequireAdmin(user): RequireAdmin,
) -> Result<Response, ApiError> {
    rate.check().await?;
    let cap = crate::plans::get_plan_limits(&user.plan)
        .max_sse_subscribers
        .max(0) as usize;
    // `true` for the audience: this route is admin-only already, so
    // every event on the org's channel is for this subscriber.
    let Some(subscription) =
        crate::mcp::activity::TRACKER
            .broadcaster
            .subscribe(&user.org_id, true, cap)
    else {
        return Err(ApiError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            format!(
                "Too many open MCP activity streams for this org (cap: {cap} on your \
                 current plan). Close unused tabs and retry, or upgrade for a higher cap."
            ),
        ));
    };
    Ok(crate::sse::stream_response(
        subscription,
        crate::sse::connected_frame(&user.org_id),
    ))
}

/// `time.time()`, which is what the tracker's stored timestamps are.
fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}
