//! `/api/audit/stream-logs` and its stats sibling.
//!
//! Ported from `backend/app/api/audit.py`, including the `?format=csv`
//! streaming export — `crate::csv_export` holds the mechanics it shares
//! with the two sibling log routes.

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
pub struct StreamAccessLogRow {
    pub(crate) id: i32,
    pub(crate) user_id: String,
    pub(crate) user_email: Option<String>,
    pub(crate) org_id: String,
    pub(crate) camera_id: String,
    pub(crate) node_id: String,
    pub(crate) ip_address: Option<String>,
    pub(crate) accessed_at: Option<NaiveDateTime>,
}

impl StreamAccessLogRow {
    pub fn to_json(&self) -> Value {
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
    rate: PerMinute<120>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    request: Request,
) -> Result<Response, ApiError> {
    let mut q = Query::parse(request.uri().query());
    let camera_id = q.optional_str("camera_id");
    let user_filter = q.optional_str("user_id");
    let limit = q.int("limit", 100, 1, 500);
    let offset = q.int("offset", 0, 0, 1_000_000);
    let format = q.pattern("format", "json", "^(json|csv)$", &["json", "csv"]);
    q.finish()?;
    // Query parameters are validated before the decorator runs; the
    // feature check is a call inside the Python function, after it. The
    // order also decides which of two errors a caller sees. Spent before
    // the CSV hand-off too, so JSON and CSV share one budget as they do
    // in Python.
    rate.check().await?;
    require_admin_feature(&user)?;

    let mut where_sql = String::from(" WHERE org_id = $1");
    let mut binds: Vec<String> = vec![user.org_id.clone()];
    if let Some(ref c) = camera_id {
        binds.push(c.clone());
        where_sql.push_str(&format!(" AND camera_id = ${}", binds.len()));
    }
    if let Some(ref u) = user_filter {
        binds.push(format!("%{u}%"));
        let n = binds.len();
        // Deliberately unescaped, and deliberately no ESCAPE clause:
        // this route matches `%{value}%` raw, so an underscore or a
        // percent the caller typed acts as a LIKE wildcard. Its two
        // sibling log routes (/api/audit-logs, /api/mcp/activity/logs)
        // escape both. Copying the inconsistency keeps the ported route
        // returning the same rows; it is not an endorsement.
        //
        // The explicit ESCAPE changes nothing on Postgres, whose default
        // escape is already a backslash. It is there for SQLite, which
        // has no default, so that the same typed backslash means the
        // same thing in both builds.
        where_sql.push_str(&format!(
            " AND (user_email {like} ${n} ESCAPE '\\' OR user_id {like} ${n} ESCAPE '\\')",
            like = crate::db::ILIKE
        ));
    }

    // CSV bypasses `limit`/`offset` for a flat 50,000-row window — an
    // auditor wants a window, not a page — and both formats have already
    // spent the same rate-limit budget and passed the same feature gate.
    if format == "csv" {
        return csv_export(&state, &user.org_id, &where_sql, binds);
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
         ORDER BY accessed_at DESC LIMIT {limit} OFFSET {offset}"
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

/// The `?format=csv` window for stream access.
///
/// The column order is the Python's and is not the JSON body's: the
/// export leads with `accessed_at` because a spreadsheet sorted by its
/// first column should be sorted by time.
fn csv_export(
    state: &AppState,
    org_id: &str,
    where_sql: &str,
    binds: Vec<String>,
) -> Result<Response, ApiError> {
    let sql = format!(
        "SELECT accessed_at, camera_id, node_id, user_email, user_id, ip_address \
           FROM stream_access_logs{where_sql} ORDER BY accessed_at DESC LIMIT 50000"
    );
    let rows = crate::csv_export::stream_rows(state.pool.clone(), sql, binds, |row| {
        use sqlx::Row;
        Ok(vec![
            Cell::Text(
                row.try_get::<Option<NaiveDateTime>, _>("accessed_at")?
                    .map(iso_naive)
                    .unwrap_or_default(),
            ),
            Cell::text(row.try_get("camera_id")?),
            Cell::text(row.try_get("node_id")?),
            Cell::text(row.try_get("user_email")?),
            Cell::text(row.try_get("user_id")?),
            Cell::text(row.try_get("ip_address")?),
        ])
    });
    crate::csv_export::stream_csv_response(
        &crate::csv_export::filename_for("stream-access-log", Some(org_id)),
        &[
            "accessed_at",
            "camera_id",
            "node_id",
            "user_email",
            "user_id",
            "ip_address",
        ],
        rows,
    )
}

/// `GET /api/audit/stream-logs/stats`.
///
/// `days` has an upper bound but no lower one in the Python, so a
/// negative value is accepted and puts the window in the future — which
/// simply returns zeroes. Reproduced rather than tightened.
pub async fn stream_log_stats(
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
    require_admin_feature(&user)?;

    let since = python_window_start(days, 86_400)?;
    // `python_window_start` refused anything that does not fit, so the
    // value echoed back is the one Python echoes.
    let days = days.small().unwrap_or_default();

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
