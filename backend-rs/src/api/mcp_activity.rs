//! `/api/mcp/activity/logs` and its stats sibling.
//!
//! Ported from `backend/app/api/mcp_activity.py` — but only the two
//! database-backed routes.
//!
//! `/recent`, `/sessions` and `/stats` on the same router are **not**
//! portable: they read an in-memory tracker that lives in the Python
//! process alongside the MCP server. There is no shared store behind
//! them, so a Rust handler would answer from an empty tracker and report
//! that nothing had happened. They stay proxied for as long as MCP stays
//! Python, which the plan puts out of scope entirely.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use chrono::{Duration, NaiveDate, NaiveDateTime};
use serde_json::{json, Value};

use crate::app::AppState;
use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::models::{iso_naive, now_naive};
use crate::query::Query;
use crate::ratelimit::RateLimit;

#[derive(Debug, sqlx::FromRow)]
struct McpActivityLogRow {
    id: i32,
    org_id: String,
    tool_name: String,
    key_name: String,
    status: String,
    duration_ms: Option<i32>,
    args_summary: Option<String>,
    error: Option<String>,
    timestamp: Option<NaiveDateTime>,
}

impl McpActivityLogRow {
    fn to_json(&self) -> Value {
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
    _rate: RateLimit<120>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    request: Request,
) -> Result<Response, ApiError> {
    let mut q = Query::parse(request.uri().query());
    let tool_name = q.optional_str("tool_name");
    let key_name = q.optional_str("key_name");
    let status = q.optional_str("status");
    let limit = q.int("limit", 100, Some(1), Some(500));
    let offset = q.int("offset", 0, Some(0), Some(1_000_000));
    let format = q.pattern("format", "json", "^(json|csv)$", &["json", "csv"]);
    q.finish()?;

    if format == "csv" {
        return Ok(crate::proxy::forward(State(state), request).await);
    }

    let mut where_sql = String::from(" WHERE org_id = $1");
    let mut n = 1;
    if tool_name.is_some() {
        n += 1;
        where_sql.push_str(&format!(" AND tool_name = ${n}"));
    }
    if key_name.is_some() {
        n += 1;
        // Escaped here, unlike /api/audit/stream-logs: key names are
        // operator-chosen and routinely contain underscores.
        where_sql.push_str(&format!(" AND key_name ILIKE ${n} ESCAPE '\\'"));
    }
    if status.is_some() {
        n += 1;
        where_sql.push_str(&format!(" AND status = ${n}"));
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
          ORDER BY timestamp DESC OFFSET {offset} LIMIT {limit}"
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

/// `GET /api/mcp/activity/logs/stats`.
pub async fn mcp_log_stats(
    // Python: @limiter.limit("60/minute")
    _rate: RateLimit<60>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    request: Request,
) -> Result<axum::Json<Value>, ApiError> {
    let mut q = Query::parse(request.uri().query());
    let days = q.int("days", 7, None, Some(30));
    q.finish()?;

    let since = now_naive() - Duration::days(days);

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
          GROUP BY date(timestamp) ORDER BY date(timestamp) DESC",
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
