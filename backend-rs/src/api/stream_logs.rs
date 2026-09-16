//! `/api/audit/stream-logs` and its stats sibling.
//!
//! Ported from `backend/app/api/audit.py`. The `?format=csv` branch is a
//! streaming export and still falls through to Python, as it does for
//! `/api/audit-logs`.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use chrono::{Duration, NaiveDate, NaiveDateTime};
use serde_json::{json, Value};

use crate::app::AppState;
use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::models::{iso_naive, now_naive};
use crate::query::Query;
use crate::ratelimit::PerMinute;
use crate::AuthUser;

/// The audit dashboard is a paid feature.
///
/// Checked against the `fea` claim rather than a database lookup, so a
/// downgrade takes effect on the caller's next token refresh — the same
/// behaviour the Python has, because it reads the same claim.
fn require_admin_feature(user: &AuthUser) -> Result<(), ApiError> {
    if !user.features.iter().any(|f| f == "admin") {
        return Err(ApiError::forbidden(
            "Audit dashboard requires a Pro or Pro Plus plan. Upgrade at /pricing.",
        ));
    }
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct StreamAccessLogRow {
    id: i32,
    user_id: String,
    user_email: Option<String>,
    org_id: String,
    camera_id: String,
    node_id: String,
    ip_address: Option<String>,
    accessed_at: Option<NaiveDateTime>,
}

impl StreamAccessLogRow {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "user_id": self.user_id,
            // `or ""` in Python: a NULL email renders as empty string,
            // not null, and the dashboard renders it directly.
            "user_email": self.user_email.clone().unwrap_or_default(),
            "org_id": self.org_id,
            "camera_id": self.camera_id,
            "node_id": self.node_id,
            "ip_address": self.ip_address,
            // Unguarded `.isoformat()` in Python — same nullable-column
            // crash as AuditLog. See expected_divergences.md.
            "accessed_at": self.accessed_at.map(iso_naive),
        })
    }
}

/// `GET /api/audit/stream-logs`.
pub async fn list_stream_logs(
    // Python: @limiter.limit("120/minute")
    _rate: PerMinute<120>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    request: Request,
) -> Result<Response, ApiError> {
    require_admin_feature(&user)?;

    let mut q = Query::parse(request.uri().query());
    let camera_id = q.optional_str("camera_id");
    let user_filter = q.optional_str("user_id");
    let limit = q.int("limit", 100, Some(1), Some(500));
    let offset = q.int("offset", 0, Some(0), Some(1_000_000));
    let format = q.pattern("format", "json", "^(json|csv)$", &["json", "csv"]);
    q.finish()?;

    if format == "csv" {
        return Ok(crate::proxy::forward(State(state), request).await);
    }

    let mut where_sql = String::from(" WHERE org_id = $1");
    let mut n = 1;
    if camera_id.is_some() {
        n += 1;
        where_sql.push_str(&format!(" AND camera_id = ${n}"));
    }
    if user_filter.is_some() {
        n += 1;
        // Deliberately unescaped, and deliberately no ESCAPE clause:
        // this route matches `%{value}%` raw, so an underscore or a
        // percent the caller typed acts as a LIKE wildcard. Its two
        // sibling log routes (/api/audit-logs, /api/mcp/activity/logs)
        // escape both. Copying the inconsistency keeps the ported route
        // returning the same rows; it is not an endorsement.
        where_sql.push_str(&format!(" AND (user_email ILIKE ${n} OR user_id ILIKE ${n})"));
    }

    let count_sql = format!("SELECT COUNT(*) FROM stream_access_logs{where_sql}");
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql).bind(&user.org_id);
    if let Some(ref c) = camera_id {
        cq = cq.bind(c.clone());
    }
    if let Some(ref u) = user_filter {
        cq = cq.bind(format!("%{u}%"));
    }
    let total: i64 = cq.fetch_one(&state.pool).await?;

    // limit/offset are i64 straight out of Query::int, never caller text.
    let page_sql = format!(
        "SELECT id, user_id, user_email, org_id, camera_id, node_id, ip_address, accessed_at \
         FROM stream_access_logs{where_sql} \
         ORDER BY accessed_at DESC OFFSET {offset} LIMIT {limit}"
    );
    let mut pq = sqlx::query_as::<_, StreamAccessLogRow>(&page_sql).bind(&user.org_id);
    if let Some(ref c) = camera_id {
        pq = pq.bind(c.clone());
    }
    if let Some(ref u) = user_filter {
        pq = pq.bind(format!("%{u}%"));
    }
    let logs: Vec<StreamAccessLogRow> = pq.fetch_all(&state.pool).await?;

    Ok(axum::Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "logs": logs.iter().map(StreamAccessLogRow::to_json).collect::<Vec<_>>(),
    }))
    .into_response())
}

/// `GET /api/audit/stream-logs/stats`.
///
/// `days` has an upper bound but no lower one in the Python, so a
/// negative value is accepted and puts the window in the future — which
/// simply returns zeroes. Reproduced rather than tightened.
pub async fn stream_log_stats(
    // Python: @limiter.limit("60/minute")
    _rate: PerMinute<60>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    request: Request,
) -> Result<axum::Json<Value>, ApiError> {
    require_admin_feature(&user)?;

    let mut q = Query::parse(request.uri().query());
    let days = q.int("days", 7, None, Some(30));
    q.finish()?;

    let since = now_naive() - Duration::days(days);

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM stream_access_logs WHERE org_id = $1 AND accessed_at >= $2",
    )
    .bind(&user.org_id)
    .bind(since)
    .fetch_one(&state.pool)
    .await?;

    let by_camera: Vec<(String, i64)> = sqlx::query_as(
        "SELECT camera_id, COUNT(id) AS count FROM stream_access_logs \
          WHERE org_id = $1 AND accessed_at >= $2 \
          GROUP BY camera_id ORDER BY COUNT(id) DESC LIMIT 10",
    )
    .bind(&user.org_id)
    .bind(since)
    .fetch_all(&state.pool)
    .await?;

    let by_user: Vec<(String, Option<String>, i64)> = sqlx::query_as(
        "SELECT user_id, user_email, COUNT(id) AS count FROM stream_access_logs \
          WHERE org_id = $1 AND accessed_at >= $2 \
          GROUP BY user_id, user_email ORDER BY COUNT(id) DESC LIMIT 10",
    )
    .bind(&user.org_id)
    .bind(since)
    .fetch_all(&state.pool)
    .await?;

    let by_day: Vec<(Option<NaiveDate>, i64)> = sqlx::query_as(
        "SELECT date(accessed_at) AS date, COUNT(id) AS count FROM stream_access_logs \
          WHERE org_id = $1 AND accessed_at >= $2 \
          GROUP BY date(accessed_at) ORDER BY date(accessed_at) DESC",
    )
    .bind(&user.org_id)
    .bind(since)
    .fetch_all(&state.pool)
    .await?;

    Ok(axum::Json(json!({
        "days": days,
        "total_accesses": total,
        "by_camera": by_camera.iter()
            .map(|(c, n)| json!({"camera_id": c, "count": n}))
            .collect::<Vec<_>>(),
        "by_user": by_user.iter()
            .map(|(u, e, n)| json!({
                "user_id": u,
                "user_email": e.clone().unwrap_or_default(),
                "count": n,
            }))
            .collect::<Vec<_>>(),
        // Python stringifies the date object, which gives ISO yyyy-mm-dd.
        "by_day": by_day.iter()
            .map(|(d, n)| json!({
                "date": d.map(|d| d.format("%Y-%m-%d").to_string()),
                "count": n,
            }))
            .collect::<Vec<_>>(),
    })))
}
