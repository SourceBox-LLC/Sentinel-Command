//! `GET /api/audit-logs`.
//!
//! Ported from `list_audit_logs` in `backend/app/api/cameras.py`,
//! including the `?format=csv` streaming export — see `csv_export`
//! below and `crate::csv_export` for the quoting and the headers.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use chrono::NaiveDateTime;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::auth::RequireAdmin;
use crate::csv_export::Cell;
use crate::error::ApiError;
use crate::models::iso_naive;
use crate::query::Query;
use crate::ratelimit::PerMinute;

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct AuditLogRow {
    pub(crate) id: i32,
    pub(crate) timestamp: Option<NaiveDateTime>,
    pub(crate) event: String,
    pub(crate) ip_address: Option<String>,
    pub(crate) username: Option<String>,
    pub(crate) details: Option<String>,
}

impl AuditLogRow {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            // Python calls `.isoformat()` unguarded here, so a NULL
            // timestamp is a 500 over there. The column is nullable and
            // every writer sets it, so this is unreachable in practice;
            // serving null beats crashing a whole page of audit history
            // because one row is odd. Recorded as an expected divergence.
            "timestamp": self.timestamp.map(iso_naive),
            "event": self.event,
            // The response key is "ip", the column is ip_address.
            "ip": self.ip_address,
            "username": self.username,
            "details": self.details,
        })
    }
}

/// Escape the wildcards a user did not intend.
///
/// Usernames routinely contain underscores (`clerk_user_xyz`), which
/// LIKE would treat as "any character" and quietly widen the filter.
/// This is an admin-only endpoint, so it is filter precision rather than
/// a security boundary — but a filter that matches the wrong rows in an
/// audit view is its own kind of problem.
pub(crate) fn escape_like(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// `GET /api/audit-logs` — paginated audit history for the caller's org.
///
/// Admin-only, and returns `{total, limit, offset, logs}`, the same shape
/// as `/api/audit/stream-logs` and `/api/mcp/activity/logs` so the
/// dashboard shares one pagination component across all three.
pub async fn list_audit_logs(
    // Python: @limiter.limit("120/minute")
    rate: PerMinute<120>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    request: Request,
) -> Result<Response, ApiError> {
    let mut q = Query::parse(request.uri().query());

    // Declaration order matters: FastAPI reports every failing parameter
    // and summarises the first, so these must be validated in the same
    // order the Python signature declares them.
    let event = q.optional_str("event");
    let username = q.optional_str("username");
    let limit = q.int("limit", 100, 1, 500);
    let offset = q.int("offset", 0, 0, 1_000_000);
    let format = q.pattern("format", "json", "^(json|csv)$", &["json", "csv"]);
    q.finish()?;
    rate.check().await?;

    // Build the filter clauses once and use them for the count, the
    // page and the CSV window, so a total can never disagree with the
    // rows beside it and an export can never cover different rows from
    // the page an auditor was looking at when they clicked download.
    let mut where_sql = String::from(" WHERE org_id = $1");
    let mut binds: Vec<String> = vec![user.org_id.clone()];
    if let Some(ref e) = event {
        binds.push(e.clone());
        where_sql.push_str(&format!(" AND event = ${}", binds.len()));
    }
    if let Some(ref u) = username {
        binds.push(format!("%{}%", escape_like(u)));
        where_sql.push_str(&format!(
            " AND username {} ${} ESCAPE '\\'",
            crate::db::ILIKE,
            binds.len()
        ));
    }

    // The CSV branch is bound by row count, not payload size: the JSON
    // `limit`/`offset` caps are bypassed and a flat 50,000-row window
    // takes their place, so an auditor gets a meaningful slice of
    // history in one call rather than a page. Validation above still
    // ran first, so a bad `limit` is rejected even though CSV ignores
    // its value.
    if format == "csv" {
        return csv_export(&state, &user.org_id, &where_sql, binds);
    }

    let count_sql = format!("SELECT COUNT(*) FROM audit_log{where_sql}");
    let mut count_q = sqlx::query_scalar::<_, i64>(&count_sql).bind(&user.org_id);
    if let Some(ref e) = event {
        count_q = count_q.bind(e.clone());
    }
    if let Some(ref u) = username {
        count_q = count_q.bind(format!("%{}%", escape_like(u)));
    }
    let total: i64 = count_q.fetch_one(&state.pool).await?;

    // `offset` and `limit` are interpolated rather than bound, which is
    // safe only because they came out of `Query::int` as `i64` — there
    // is no path by which a caller's string reaches this format!.
    let page_sql = format!(
        "SELECT id, timestamp, event, ip_address, username, details \
         FROM audit_log{where_sql} ORDER BY timestamp DESC LIMIT {limit} OFFSET {offset}"
    );
    let mut page_q = sqlx::query_as::<_, AuditLogRow>(&page_sql).bind(&user.org_id);
    if let Some(ref e) = event {
        page_q = page_q.bind(e.clone());
    }
    if let Some(ref u) = username {
        page_q = page_q.bind(format!("%{}%", escape_like(u)));
    }
    let logs: Vec<AuditLogRow> = page_q.fetch_all(&state.pool).await?;

    Ok(axum::Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "logs": logs.iter().map(AuditLogRow::to_json).collect::<Vec<_>>(),
    }))
    .into_response())
}

/// The `?format=csv` window: the same filters, no pagination, 50,000
/// rows.
///
/// Ordered `timestamp DESC` with no tiebreak, which is what the JSON
/// page beside it does. Ties are therefore in whatever order Postgres
/// returns them — adding an `id DESC` here and not there would make the
/// export and the page disagree about which rows a window contains,
/// which is worse than both being arbitrary in the same way.
fn csv_export(
    state: &AppState,
    org_id: &str,
    where_sql: &str,
    binds: Vec<String>,
) -> Result<Response, ApiError> {
    let sql = format!(
        "SELECT timestamp, event, username, user_id, ip_address, details \
           FROM audit_log{where_sql} ORDER BY timestamp DESC LIMIT 50000"
    );
    let rows = crate::csv_export::stream_rows(state.pool.clone(), sql, binds, |row| {
        use sqlx::Row;
        Ok(vec![
            // `log.timestamp.isoformat() if log.timestamp else ""` —
            // guarded here, unlike the JSON path's unguarded call.
            Cell::Text(
                row.try_get::<Option<NaiveDateTime>, _>("timestamp")?
                    .map(iso_naive)
                    .unwrap_or_default(),
            ),
            Cell::text(row.try_get("event")?),
            Cell::text(row.try_get("username")?),
            Cell::text(row.try_get("user_id")?),
            Cell::text(row.try_get("ip_address")?),
            Cell::text(row.try_get("details")?),
        ])
    });
    crate::csv_export::stream_csv_response(
        &crate::csv_export::filename_for("audit-log", Some(org_id)),
        &[
            "timestamp",
            "event",
            "username",
            "user_id",
            "ip_address",
            "details",
        ],
        rows,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_wildcards_a_user_did_not_type_are_escaped() {
        // clerk_user_xyz must match that user, not "clerk" + any char.
        assert_eq!(escape_like("clerk_user"), "clerk\\_user");
        assert_eq!(escape_like("50%"), "50\\%");
        // The escape character itself has to be escaped first, or
        // "a\\_b" would end up meaning something different again.
        assert_eq!(escape_like("a\\b"), "a\\\\b");
        assert_eq!(escape_like("a\\_b"), "a\\\\\\_b");
        assert_eq!(escape_like("plain"), "plain");
    }
}
