//! Incident reports — reads, plus the two writes that touch nothing but
//! the database.
//!
//! Ported from `backend/app/api/incidents.py`.
//!
//! `POST /api/incidents` is **not** ported. It fires an inbox + email
//! notification through `create_notification`, which is a side effect
//! beyond this table and belongs with the email work in slice 7. Porting
//! the response without the notification would look correct in a
//! differential and silently stop telling operators that an incident was
//! filed.

use axum::extract::{Path, Request, State};
use axum::Json;
use chrono::NaiveDateTime;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::models::{iso_naive, now_naive};
use crate::query::{parse_body, path_int, Query};
use crate::ratelimit::PerMinute;

const SEVERITIES: [&str; 4] = ["low", "medium", "high", "critical"];
const STATUSES: [&str; 4] = ["open", "acknowledged", "resolved", "dismissed"];

#[derive(Debug, sqlx::FromRow)]
struct IncidentRow {
    id: i32,
    camera_id: Option<String>,
    title: String,
    summary: String,
    report: Option<String>,
    severity: String,
    status: String,
    created_by: String,
    created_at: Option<NaiveDateTime>,
    updated_at: Option<NaiveDateTime>,
    resolved_at: Option<NaiveDateTime>,
    resolved_by: Option<String>,
    evidence_count: i64,
}

impl IncidentRow {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "camera_id": self.camera_id,
            "title": self.title,
            "summary": self.summary,
            // `self.report or ""` — a NULL report is an empty string on
            // the wire, not null.
            "report": self.report.clone().unwrap_or_default(),
            "severity": self.severity,
            "status": self.status,
            "created_by": self.created_by,
            "created_at": self.created_at.map(iso_naive),
            "updated_at": self.updated_at.map(iso_naive),
            "resolved_at": self.resolved_at.map(iso_naive),
            "resolved_by": self.resolved_by,
            "evidence_count": self.evidence_count,
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
struct EvidenceRow {
    id: i32,
    incident_id: i32,
    kind: String,
    text: Option<String>,
    camera_id: Option<String>,
    data_mime: Option<String>,
    timestamp: Option<NaiveDateTime>,
}

impl EvidenceRow {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "incident_id": self.incident_id,
            "kind": self.kind,
            "text": self.text,
            "camera_id": self.camera_id,
            // Derived from data_mime, never from the blob: `data` is a
            // deferred column and touching it would load megabytes per
            // row, which is the whole reason it is deferred.
            "has_data": self.data_mime.is_some(),
            "data_mime": self.data_mime,
            "timestamp": self.timestamp.map(iso_naive),
        })
    }
}

const INCIDENT_SELECT: &str = r#"
    SELECT i.id, i.camera_id, i.title, i.summary, i.report, i.severity, i.status,
           i.created_by, i.created_at, i.updated_at, i.resolved_at, i.resolved_by,
           (SELECT COUNT(*) FROM incident_evidence e WHERE e.incident_id = i.id)
               AS evidence_count
      FROM incidents i
"#;

async fn evidence_for(pool: &sqlx::PgPool, incident_id: i32) -> Result<Vec<Value>, ApiError> {
    // Ordered by timestamp, matching the relationship's `order_by`.
    // Never selects `data`.
    let rows: Vec<EvidenceRow> = sqlx::query_as(
        "SELECT id, incident_id, kind, text, camera_id, data_mime, timestamp \
           FROM incident_evidence WHERE incident_id = $1 ORDER BY timestamp",
    )
    .bind(incident_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(EvidenceRow::to_json).collect())
}

async fn owned_incident(
    pool: &sqlx::PgPool,
    org_id: &str,
    incident_id: i32,
) -> Result<IncidentRow, ApiError> {
    sqlx::query_as(&format!(
        "{INCIDENT_SELECT} WHERE i.id = $1 AND i.org_id = $2"
    ))
    .bind(incident_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    // 404 for "not yours" as well as "not there" — distinguishing them
    // would confirm another tenant's incident ids.
    .ok_or_else(|| ApiError::not_found("Incident not found"))
}

/// `GET /api/incidents`.
pub async fn list_incidents(
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let mut q = Query::parse(request.uri().query());
    let status = q.optional_str("status");
    let severity = q.optional_str("severity");
    let camera_id = q.optional_str("camera_id");
    let limit = q.int("limit", 50, Some(1), Some(200));
    let offset = q.int("offset", 0, Some(0), Some(1_000_000));
    q.finish()?;

    // These two are validated in the handler body rather than by
    // Pydantic, so an unknown value is a 400 with a plain message — not
    // the 422 envelope the numeric bounds produce.
    if let Some(ref s) = status {
        if !STATUSES.contains(&s.as_str()) {
            return Err(ApiError::bad_request(format!("Invalid status: {s}")));
        }
    }
    if let Some(ref s) = severity {
        if !SEVERITIES.contains(&s.as_str()) {
            return Err(ApiError::bad_request(format!("Invalid severity: {s}")));
        }
    }

    let mut where_sql = String::from(" WHERE i.org_id = $1");
    let mut n = 1;
    for (present, column) in [
        (status.is_some(), "i.status"),
        (severity.is_some(), "i.severity"),
        (camera_id.is_some(), "i.camera_id"),
    ] {
        if present {
            n += 1;
            where_sql.push_str(&format!(" AND {column} = ${n}"));
        }
    }

    let count_sql = format!("SELECT COUNT(*) FROM incidents i{where_sql}");
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql).bind(&user.org_id);
    for v in [&status, &severity, &camera_id].into_iter().flatten() {
        cq = cq.bind(v.clone());
    }
    let total: i64 = cq.fetch_one(&state.pool).await?;

    let page_sql = format!(
        "{INCIDENT_SELECT}{where_sql} ORDER BY i.created_at DESC OFFSET {offset} LIMIT {limit}"
    );
    let mut pq = sqlx::query_as::<_, IncidentRow>(&page_sql).bind(&user.org_id);
    for v in [&status, &severity, &camera_id].into_iter().flatten() {
        pq = pq.bind(v.clone());
    }
    let rows: Vec<IncidentRow> = pq.fetch_all(&state.pool).await?;

    Ok(Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "incidents": rows.iter().map(IncidentRow::to_json).collect::<Vec<_>>(),
    })))
}

/// `GET /api/incidents/counts` — the stat bar.
pub async fn incident_counts(
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
) -> Result<Json<Value>, ApiError> {
    let row: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE status = 'open'),
                COUNT(*) FILTER (WHERE status = 'open' AND severity = 'critical'),
                COUNT(*) FILTER (WHERE status = 'open' AND severity = 'high'),
                COUNT(*)
           FROM incidents WHERE org_id = $1",
    )
    .bind(&user.org_id)
    .fetch_one(&state.pool)
    .await?;

    Ok(Json(json!({
        "open": row.0,
        "open_critical": row.1,
        "open_high": row.2,
        "total": row.3,
    })))
}

/// `GET /api/incidents/{incident_id}`.
pub async fn get_incident(
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    Path(incident_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let incident_id = path_int("incident_id", &incident_id)?;
    let row = owned_incident(&state.pool, &user.org_id, incident_id).await?;
    let mut out = row.to_json();
    out["evidence"] = Value::Array(evidence_for(&state.pool, incident_id).await?);
    Ok(Json(out))
}

#[derive(Debug, Deserialize, Default)]
pub struct IncidentPatch {
    status: Option<String>,
    severity: Option<String>,
    summary: Option<String>,
    report: Option<String>,
}

/// `PATCH /api/incidents/{incident_id}` — acknowledge, resolve, dismiss.
pub async fn update_incident(
    _rate: PerMinute<120>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    Path(incident_id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let patch: IncidentPatch = serde_json::from_value(parse_body(&body)?)
        .unwrap_or_default();
    let incident_id = path_int("incident_id", &incident_id)?;
    let incident = owned_incident(&state.pool, &user.org_id, incident_id).await?;

    let mut status = incident.status.clone();
    let mut resolved_at = incident.resolved_at;
    let mut resolved_by = incident.resolved_by.clone();

    if let Some(ref new_status) = patch.status {
        if !STATUSES.contains(&new_status.as_str()) {
            return Err(ApiError::bad_request(format!("Invalid status: {new_status}")));
        }
        let terminal = matches!(new_status.as_str(), "resolved" | "dismissed");
        let was_terminal = matches!(incident.status.as_str(), "resolved" | "dismissed");
        if terminal && !was_terminal {
            // Stamped only on the *transition* into a terminal state, so
            // re-resolving an already-resolved incident keeps the
            // original resolver and time.
            resolved_at = Some(now_naive());
            resolved_by = Some(format!("user:{}", user.user_id));
        } else if new_status == "open" {
            // Re-opening clears the resolution outright.
            resolved_at = None;
            resolved_by = None;
        }
        status = new_status.clone();
    }

    let mut severity = incident.severity.clone();
    if let Some(ref new_severity) = patch.severity {
        if !SEVERITIES.contains(&new_severity.as_str()) {
            return Err(ApiError::bad_request(format!(
                "Invalid severity: {new_severity}"
            )));
        }
        severity = new_severity.clone();
    }

    let summary = patch.summary.clone().unwrap_or_else(|| incident.summary.clone());
    let report = match patch.report {
        Some(ref r) => Some(r.clone()),
        None => incident.report.clone(),
    };

    // SQLAlchemy emits no UPDATE at all when nothing actually changed,
    // so `updated_at` — an `onupdate` column — keeps its old value. An
    // unconditional UPDATE here would bump it on every no-op PATCH, and
    // the dashboard sorts and badges on that field.
    //
    // This also covers patching a field to the value it already holds:
    // the session's dirty check compares old against new, so setting
    // severity="high" on an already-high incident is not a change
    // either. Comparing computed values against the current row
    // reproduces both cases.
    let unchanged = status == incident.status
        && severity == incident.severity
        && summary == incident.summary
        && report == incident.report
        && resolved_at == incident.resolved_at
        && resolved_by == incident.resolved_by;

    if !unchanged {
        sqlx::query(
            "UPDATE incidents
                SET status = $1, severity = $2, summary = $3, report = $4,
                    resolved_at = $5, resolved_by = $6, updated_at = $7
              WHERE id = $8 AND org_id = $9",
        )
        .bind(&status)
        .bind(&severity)
        .bind(&summary)
        .bind(&report)
        .bind(resolved_at)
        .bind(&resolved_by)
        .bind(now_naive())
        .bind(incident_id)
        .bind(&user.org_id)
        .execute(&state.pool)
        .await?;
    }

    let updated = owned_incident(&state.pool, &user.org_id, incident_id).await?;
    let mut out = updated.to_json();
    out["evidence"] = Value::Array(evidence_for(&state.pool, incident_id).await?);
    Ok(Json(out))
}

/// `DELETE /api/incidents/{incident_id}`.
///
/// Evidence rows go with it via `ON DELETE CASCADE`, which is on the
/// foreign key in the schema — SQLAlchemy's `cascade="all, delete-orphan"`
/// would otherwise do it in Python, and only one of the two needs to.
pub async fn delete_incident(
    _rate: PerMinute<60>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    Path(incident_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let incident_id = path_int("incident_id", &incident_id)?;
    // Fetched first so a missing or other-tenant incident 404s before
    // anything is deleted.
    owned_incident(&state.pool, &user.org_id, incident_id).await?;

    sqlx::query("DELETE FROM incidents WHERE id = $1 AND org_id = $2")
        .bind(incident_id)
        .bind(&user.org_id)
        .execute(&state.pool)
        .await?;

    Ok(Json(json!({ "deleted": incident_id })))
}
