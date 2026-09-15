//! `GET /api/audit-logs`.
//!
//! Ported from `list_audit_logs` in `backend/app/api/cameras.py`. The
//! `?format=csv` branch is a streaming download and is *not* ported —
//! it still falls through to Python. See `list_audit_logs` below.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use chrono::NaiveDateTime;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::models::iso_naive;
use crate::query::Query;
use crate::ratelimit::RateLimit;

#[derive(Debug, sqlx::FromRow)]
struct AuditLogRow {
    id: i32,
    timestamp: Option<NaiveDateTime>,
    event: String,
    ip_address: Option<String>,
    username: Option<String>,
    details: Option<String>,
}

impl AuditLogRow {
    fn to_json(&self) -> Value {
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
    _rate: RateLimit<120>,
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
    let limit = q.int("limit", 100, Some(1), Some(500));
    let offset = q.int("offset", 0, Some(0), Some(1_000_000));
    let format = q.pattern("format", "json", "^(json|csv)$", &["json", "csv"]);
    q.finish()?;

    // The CSV branch is a streaming export with a 50,000-row window and
    // its own filename convention. Streaming it is a different shape of
    // work from this handler, and getting the download headers subtly
    // wrong would break an auditor's export silently — so it stays with
    // Python until it can be ported and diffed on its own terms.
    // Validation above still runs first, so a bad `limit` is rejected
    // here exactly as it would have been there.
    if format == "csv" {
        return Ok(crate::proxy::forward(State(state), request).await);
    }

    // Build the filter clauses once and use them for both the count and
    // the page, so a total can never disagree with the rows beside it.
    let mut where_sql = String::from(" WHERE org_id = $1");
    if event.is_some() {
        where_sql.push_str(" AND event = $2");
    }
    if username.is_some() {
        let n = if event.is_some() { 3 } else { 2 };
        where_sql.push_str(&format!(" AND username ILIKE ${n} ESCAPE '\\'"));
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
         FROM audit_log{where_sql} ORDER BY timestamp DESC OFFSET {offset} LIMIT {limit}"
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
