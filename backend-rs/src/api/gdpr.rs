//! GDPR Article 20 — the organisation's data, as a ZIP of JSON.
//!
//! Ported from `backend/app/api/gdpr.py` and the `export_org_data` half
//! of `backend/app/core/gdpr.py`, and now the erasure half too —
//! `delete_org_data`, which `POST /api/settings/danger/full-reset` and
//! the `organization.deleted` webhook share so that a customer
//! clicking "delete my data" and an operator clicking "full reset"
//! leave the organisation in the same state.
//!
//! Three things decide what this has to reproduce.
//!
//! **The rows are the API's own shapes.** Python serialises each row
//! with the model's `to_dict()` — the same method the routes return —
//! so this reuses the row types the ported routes already have rather
//! than writing an export-only shape that could drift from them. The
//! two models with no `to_dict` fall to `_introspect`, which is raw
//! columns with datetimes as `isoformat()`.
//!
//! **The order is the archive's contract.** Tables come in
//! `ORG_SCOPED_MODELS` order, then the cascade parents, and
//! `manifest.json` is written last so it can carry the row counts.
//! Within a table the rows are whatever the query returns, and the two
//! cascade children are loaded per parent — `for inc in incidents: for
//! ev in inc.evidence` — which is the order they land in, so they are
//! queried that way here too rather than in one join.
//!
//! **The audit row is written before a byte is streamed.** A browser
//! that disconnects mid-download must still leave the record of who
//! asked, which is why the Python commits it first and this does too.
//!
//! What is *not* reproduced is the archive's bytes: Python's zlib and
//! this DEFLATE need not emit the same stream, and every member carries
//! a local timestamp. The differential compares the archive's contents
//! — member names, in order, and each one's JSON — which is what a
//! reader of the export actually gets.

use std::io::Write;

use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use chrono::{NaiveDateTime, Utc};
use serde_json::{json, Map, Value};

use crate::app::AppState;
use crate::audit::{audit_label, python_json, write_audit};
use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::models::iso_naive;
use crate::ratelimit::PerHour;

/// `POST /api/gdpr/export`.
pub async fn export_organization_data(
    rate: PerHour<3>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    rate.check().await?;

    let exported_at = Utc::now();
    let filename = export_filename(&user.org_id, exported_at);

    // Before the archive is built, not after: the Python's own reason is
    // that a disconnect mid-stream would otherwise leave no record that
    // the export was attempted at all.
    write_audit(
        &state.pool,
        &user.org_id,
        "gdpr_export",
        &user.user_id,
        &audit_label(&user),
        Some(python_json(&[("filename", json!(filename))])),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    let archive = build_archive(&state, &user.org_id, exported_at).await?;

    // Starlette writes the explicit headers first and appends the
    // derived content-type; a streaming response carries no
    // content-length.
    let mut out = HeaderMap::new();
    out.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .map_err(|_| ApiError::internal("filename is not a header value"))?,
    );
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    out.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/zip"));
    Ok((out, archive).into_response())
}

/// `filename_for("gdpr-export", org_id).replace(".csv", ".zip")`.
///
/// `str.replace` replaces every occurrence, so an org id that itself
/// contains `.csv` is rewritten too — reproduced rather than tidied,
/// because the filename is what the browser saves.
fn export_filename(org_id: &str, now: chrono::DateTime<Utc>) -> String {
    let date = now.format("%Y%m%d");
    let org = safe_segment(org_id);
    format!("gdpr-export-{org}-{date}.csv").replace(".csv", ".zip")
}

// `safe_segment` lives in `crate::csv_export`, which is where the Python
// kept it: `gdpr.py` imported `filename_for` from `csv_export` rather
// than sanitising its own filename, and two copies of that rule would be
// two things to keep in step.
use crate::csv_export::safe_segment;

/// Build the whole archive in memory, as the Python does.
///
/// Its docstring is worth keeping in mind: the archive must not be
/// drained between members, because `ZipFile` records each member's
/// offset from the buffer's absolute position. Here the buffer is a
/// `Vec` that is never truncated, so the same hazard does not arise —
/// but the memory profile is the same, and bounded the same way, by
/// retention.
async fn build_archive(
    state: &AppState,
    org_id: &str,
    exported_at: chrono::DateTime<Utc>,
) -> Result<Vec<u8>, ApiError> {
    let mut buf = Vec::new();
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        // `zipfile.writestr` stamps a ZipInfo with 0o600 and the local
        // clock; neither is part of what the export means.
        .unix_permissions(0o600);

    let mut tables = Vec::new();
    for (name, rows) in export_org_data(state, org_id).await? {
        let payload = python_dumps_indented(&Value::Array(rows.clone()));
        zip.start_file(format!("{name}.json"), options)?;
        zip.write_all(payload.as_bytes())
            .map_err(|_| ApiError::internal("could not write the export archive"))?;
        tables.push(json!({
            "name": name,
            "rows": rows.len(),
            "filename": format!("{name}.json"),
        }));
    }

    // Last, so it can carry the row counts.
    let manifest = json!({
        "org_id": org_id,
        "exported_at": iso_aware(exported_at),
        "format_version": 1,
        "spec": "GDPR Article 20 — data portability export",
        "tables": tables,
        "excluded": {
            "recordings":
                "Local to your CameraNode device. Not stored on \
                 Command Center. Use the CameraNode TUI to export.",
            "incident_evidence_blobs":
                "Metadata exported here as 'incident_evidence.json'. \
                 Binary bytes available per-evidence via \
                 GET /api/incidents/{id}/evidence/{eid} during \
                 your portability window.",
        },
    });
    zip.start_file("manifest.json", options)?;
    zip.write_all(python_dumps_indented(&manifest).as_bytes())
        .map_err(|_| ApiError::internal("could not write the export manifest"))?;

    // `finish` consumes the writer, which is what releases its borrow
    // on the buffer.
    zip.finish()?;
    Ok(buf)
}

/// `json.dumps(value, indent=2, default=str)`.
///
/// Two spaces, `": "` between key and value, and a trailing newline
/// nowhere — which is what serde_json's pretty printer does. `default`
/// never fires: every value here is already JSON, because the row
/// shapes convert their own timestamps.
fn python_dumps_indented(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".into())
}

/// `datetime.now(tz=UTC).isoformat()` — aware, so it carries `+00:00`.
fn iso_aware(ts: chrono::DateTime<Utc>) -> String {
    format!("{}+00:00", iso_naive(ts.naive_utc()))
}

/// Every org-scoped table, in the order `export_org_data` yields them.
async fn export_org_data(
    state: &AppState,
    org_id: &str,
) -> Result<Vec<(&'static str, Vec<Value>)>, ApiError> {
    let pool = &state.pool;
    let mut out: Vec<(&'static str, Vec<Value>)> = Vec::new();

    // --- ORG_SCOPED_MODELS, in their own order ----------------------
    out.push(("settings", introspect_rows(pool, "settings", org_id).await?));

    let rows: Vec<crate::api::audit::AuditLogRow> =
        org_rows(pool, "SELECT * FROM audit_log WHERE org_id = $1", org_id).await?;
    out.push(("audit_log", rows.iter().map(|r| r.to_json()).collect()));

    let rows: Vec<crate::api::stream_logs::StreamAccessLogRow> =
        org_rows(pool, "SELECT * FROM stream_access_logs WHERE org_id = $1", org_id).await?;
    out.push(("stream_access_logs", rows.iter().map(|r| r.to_json()).collect()));

    let rows: Vec<crate::api::mcp_activity::McpActivityLogRow> =
        org_rows(pool, "SELECT * FROM mcp_activity_logs WHERE org_id = $1", org_id).await?;
    out.push(("mcp_activity_logs", rows.iter().map(|r| r.to_json()).collect()));

    // Both kinds, `mcp` and `integration`: the export is the table, not
    // one surface's view of it.
    let rows: Vec<crate::api::keys::KeyRow> =
        org_rows(pool, "SELECT * FROM mcp_api_keys WHERE org_id = $1", org_id).await?;
    out.push(("mcp_api_keys", rows.iter().map(|r| r.to_json()).collect()));

    let rows: Vec<OrgMonthlyUsageRow> =
        org_rows(pool, "SELECT * FROM org_monthly_usage WHERE org_id = $1", org_id).await?;
    out.push(("org_monthly_usage", rows.iter().map(|r| r.to_json()).collect()));

    let rows: Vec<EmailLogRow> =
        org_rows(pool, "SELECT * FROM email_log WHERE org_id = $1", org_id).await?;
    out.push(("email_log", rows.iter().map(|r| r.to_json()).collect()));

    let rows: Vec<EmailOutboxRow> =
        org_rows(pool, "SELECT * FROM email_outbox WHERE org_id = $1", org_id).await?;
    out.push(("email_outbox", rows.iter().map(|r| r.to_json()).collect()));

    out.push((
        "user_notification_state",
        introspect_rows(pool, "user_notification_state", org_id).await?,
    ));

    let rows: Vec<crate::api::notifications::NotificationRow> =
        org_rows(pool, "SELECT * FROM notifications WHERE org_id = $1", org_id).await?;
    out.push(("notifications", rows.iter().map(|r| r.to_json()).collect()));

    let rows: Vec<crate::api::motion::MotionEventRow> =
        org_rows(pool, "SELECT * FROM motion_events WHERE org_id = $1", org_id).await?;
    out.push(("motion_events", rows.iter().map(|r| r.to_json()).collect()));

    // `camera_count` is a relationship length in Python, so it counts
    // the group's cameras rather than reading a column.
    let rows: Vec<crate::models::CameraGroupRow> = org_rows(
        pool,
        "SELECT g.*, (SELECT COUNT(*) FROM cameras c WHERE c.group_id = g.id) AS camera_count \
           FROM camera_groups g WHERE g.org_id = $1",
        org_id,
    )
    .await?;
    out.push(("camera_groups", rows.iter().map(|r| r.to_json()).collect()));

    let rows: Vec<crate::api::sentinel_config::ConfigRow> =
        org_rows(pool, "SELECT * FROM sentinel_config WHERE org_id = $1", org_id).await?;
    out.push(("sentinel_config", rows.iter().map(|r| r.to_json()).collect()));

    let rows: Vec<crate::api::sentinel::SentinelRunRow> =
        org_rows(pool, "SELECT * FROM sentinel_runs WHERE org_id = $1", org_id).await?;
    // `_serialize` calls `to_dict()` with no arguments, and the trace is
    // off by default.
    out.push(("sentinel_runs", rows.iter().map(|r| r.to_json(false)).collect()));

    let rows: Vec<SentinelAgentKeyRow> =
        org_rows(pool, "SELECT * FROM sentinel_agent_keys WHERE org_id = $1", org_id).await?;
    out.push(("sentinel_agent_keys", rows.iter().map(|r| r.to_json()).collect()));

    // --- the cascade parents, and their children per parent ---------
    let incidents: Vec<crate::api::incidents::IncidentRow> = org_rows(
        pool,
        "SELECT i.*, (SELECT COUNT(*) FROM incident_evidence e WHERE e.incident_id = i.id) \
              AS evidence_count \
           FROM incidents i WHERE i.org_id = $1",
        org_id,
    )
    .await?;
    let mut evidence = Vec::new();
    for incident in &incidents {
        // One query per incident, because that is how the relationship
        // loads and therefore the order the rows arrive in.
        let rows: Vec<crate::api::incidents::EvidenceRow> = sqlx::query_as(
            "SELECT id, incident_id, kind, text, camera_id, data_mime, timestamp \
               FROM incident_evidence WHERE incident_id = $1",
        )
        .bind(incident.id)
        .fetch_all(pool)
        .await?;
        evidence.extend(rows.iter().map(|r| r.to_json()));
    }
    out.push(("incidents", incidents.iter().map(|r| r.to_json()).collect()));
    out.push(("incident_evidence", evidence));

    let nodes: Vec<crate::api::nodes::CameraNodeRow> = org_rows(
        pool,
        "SELECT n.*, (SELECT COUNT(*) FROM cameras c WHERE c.node_id = n.id) AS camera_count \
           FROM camera_nodes n WHERE n.org_id = $1",
        org_id,
    )
    .await?;
    let mut cameras = Vec::new();
    for node in &nodes {
        // Scoped through the node, not by the camera's own `org_id`:
        // the Python walks `node.cameras`, so a camera whose row names a
        // different org is still exported with its node.
        let rows: Vec<crate::models::CameraRow> = sqlx::query_as(&format!(
            "{} WHERE n.node_id = $1",
            crate::models::CAMERA_SELECT
        ))
        .bind(&node.node_id)
        .fetch_all(pool)
        .await?;
        cameras.extend(rows.iter().map(|r| r.to_json()));
    }
    out.push(("camera_nodes", nodes.iter().map(|r| r.to_json()).collect()));
    out.push(("cameras", cameras));

    Ok(out)
}

async fn org_rows<T>(pool: &sqlx::PgPool, sql: &str, org_id: &str) -> Result<Vec<T>, ApiError>
where
    T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin,
{
    Ok(sqlx::query_as(sql).bind(org_id).fetch_all(pool).await?)
}

/// `_introspect`: every column by name, datetimes as `isoformat()`.
///
/// For the two models with no `to_dict` — `settings` and
/// `user_notification_state`. Both have only text, integer and
/// timestamp columns, so the `bytes` branch of the Python's
/// introspection (`<binary N bytes>`) cannot arise here.
async fn introspect_rows(
    pool: &sqlx::PgPool,
    table: &str,
    org_id: &str,
) -> Result<Vec<Value>, ApiError> {
    use sqlx::{Column, Row, TypeInfo};

    let sql = format!("SELECT * FROM {table} WHERE org_id = $1");
    let rows = sqlx::query(&sql).bind(org_id).fetch_all(pool).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut map = Map::new();
        for column in row.columns() {
            let name = column.name();
            let value = match column.type_info().name() {
                "INT4" => row
                    .try_get::<Option<i32>, _>(name)
                    .map(|v| v.map_or(Value::Null, |v| json!(v))),
                "INT8" => row
                    .try_get::<Option<i64>, _>(name)
                    .map(|v| v.map_or(Value::Null, |v| json!(v))),
                "BOOL" => row
                    .try_get::<Option<bool>, _>(name)
                    .map(|v| v.map_or(Value::Null, |v| json!(v))),
                "TIMESTAMP" => row
                    .try_get::<Option<NaiveDateTime>, _>(name)
                    .map(|v| v.map_or(Value::Null, |v| json!(iso_naive(v)))),
                _ => row
                    .try_get::<Option<String>, _>(name)
                    .map(|v| v.map_or(Value::Null, |v| json!(v))),
            }
            .map_err(|_| ApiError::internal("could not read a column for the export"))?;
            map.insert(name.to_string(), value);
        }
        out.push(Value::Object(map));
    }
    Ok(out)
}

#[derive(Debug, sqlx::FromRow)]
struct OrgMonthlyUsageRow {
    org_id: String,
    year_month: String,
    viewer_seconds: i32,
    updated_at: Option<NaiveDateTime>,
}

impl OrgMonthlyUsageRow {
    fn to_json(&self) -> Value {
        json!({
            "org_id": self.org_id,
            "year_month": self.year_month,
            "viewer_seconds": self.viewer_seconds,
            "viewer_hours": crate::pyrepr::round_half_even(f64::from(self.viewer_seconds) / 3600.0),
            "updated_at": self.updated_at.map(iso_naive),
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
struct EmailLogRow {
    id: i32,
    timestamp: Option<NaiveDateTime>,
    recipient_email: String,
    kind: String,
    status: String,
    resend_message_id: Option<String>,
    error: Option<String>,
}

impl EmailLogRow {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "timestamp": self.timestamp.map(iso_naive),
            "recipient_email": self.recipient_email,
            "kind": self.kind,
            "status": self.status,
            "resend_message_id": self.resend_message_id,
            "error": self.error,
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
struct EmailOutboxRow {
    id: i32,
    org_id: String,
    recipient_email: String,
    subject: String,
    kind: String,
    status: String,
    attempts: i32,
    resend_message_id: Option<String>,
    error: Option<String>,
    created_at: Option<NaiveDateTime>,
    sent_at: Option<NaiveDateTime>,
}

impl EmailOutboxRow {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "org_id": self.org_id,
            "recipient_email": self.recipient_email,
            "subject": self.subject,
            "kind": self.kind,
            "status": self.status,
            "attempts": self.attempts,
            "resend_message_id": self.resend_message_id,
            "error": self.error,
            "created_at": self.created_at.map(iso_naive),
            "sent_at": self.sent_at.map(iso_naive),
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
struct SentinelAgentKeyRow {
    id: i32,
    name: String,
    key_last4: Option<String>,
    created_at: Option<NaiveDateTime>,
    created_by: Option<String>,
    last_used_at: Option<NaiveDateTime>,
    revoked: bool,
}

impl SentinelAgentKeyRow {
    /// Never `key_hash`, and never `org_id` — the same two omissions the
    /// management API makes, for the same reasons.
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "key_last4": self.key_last4,
            "created_at": self.created_at.map(iso_naive),
            "created_by": self.created_by,
            "last_used_at": self.last_used_at.map(iso_naive),
            "revoked": self.revoked,
        })
    }
}

/// The 500 a failure here becomes. Starlette renders an unhandled
/// exception mid-stream as a bare `Internal Server Error`, and the
/// archive is built before the response starts, so a failure is an
/// ordinary 500 rather than a truncated download.
impl From<zip::result::ZipError> for ApiError {
    fn from(_: zip::result::ZipError) -> Self {
        ApiError::internal("could not build the export archive")
    }
}

/// The tables `delete_org_data` empties, in the order it empties them,
/// and the order their counts appear in the audit row.
///
/// Order is not cosmetic. `cameras.group_id` references
/// `camera_groups` with no `ON DELETE` clause, so every camera has to
/// be gone before the groups are — Python gets there by flushing its
/// pending cascade deletes before the bulk pass, and a port that
/// simply ran the list would hit a foreign key violation and abort the
/// whole erasure. An aborted erasure is an Article 17 failure that
/// reports success.
const ORG_SCOPED_TABLES: [&str; 15] = [
    "settings",
    "audit_log",
    "stream_access_logs",
    "mcp_activity_logs",
    "mcp_api_keys",
    "org_monthly_usage",
    "email_log",
    "email_outbox",
    "user_notification_state",
    "notifications",
    "motion_events",
    "camera_groups",
    "sentinel_config",
    "sentinel_runs",
    // A surviving agent key would still authenticate after the org is
    // gone, and its `org_id` is a customer identifier in its own right.
    "sentinel_agent_keys",
];

/// `delete_org_data(db, org_id)` — every row this organisation owns.
///
/// Returns `(table, rows deleted)` in the order the Python's dict
/// records them, because that dict is serialised into the audit row.
///
/// Two counts are not the number of rows the statement reported.
/// `cameras` is counted *before* anything is deleted, because in
/// Python most of them go through the CameraNode cascade, which
/// surfaces no count of its own; and `incident_evidence` is absent
/// entirely, having no `org_id` and no deletion path but its parent's.
///
/// The caller is responsible for what lives outside the database: the
/// `wipe_data` command to each node, which has to go out while the
/// node ids still exist, and the in-memory caches, which no `DELETE`
/// can reach.
pub async fn delete_org_data(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org_id: &str,
) -> Result<Vec<(&'static str, i64)>, sqlx::Error> {
    let mut counts: Vec<(&'static str, i64)> = Vec::new();

    // Before anything is removed: the cascade never reports how many
    // children it took.
    let (cameras_before,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM cameras WHERE org_id = $1")
            .bind(org_id)
            .fetch_one(&mut **tx)
            .await?;

    // `incident_evidence` follows its parent through the foreign key's
    // own ON DELETE CASCADE.
    let incidents = sqlx::query("DELETE FROM incidents WHERE org_id = $1")
        .bind(org_id)
        .execute(&mut **tx)
        .await?
        .rows_affected() as i64;
    counts.push(("incidents", incidents));

    // The CameraNode cascade, which the database will not do for us:
    // `cameras.node_id` carries no ON DELETE clause.
    sqlx::query(
        "DELETE FROM cameras WHERE node_id IN
            (SELECT id FROM camera_nodes WHERE org_id = $1)",
    )
    .bind(org_id)
    .execute(&mut **tx)
    .await?;
    let nodes = sqlx::query("DELETE FROM camera_nodes WHERE org_id = $1")
        .bind(org_id)
        .execute(&mut **tx)
        .await?
        .rows_affected() as i64;
    counts.push(("camera_nodes", nodes));

    // The mop-up: a camera whose `node_id` is null was never reachable
    // through a node, and is still this organisation's.
    sqlx::query("DELETE FROM cameras WHERE org_id = $1")
        .bind(org_id)
        .execute(&mut **tx)
        .await?;
    counts.push(("cameras", cameras_before));

    for table in ORG_SCOPED_TABLES {
        // The table names are a fixed list in this file, never input.
        let deleted = sqlx::query(&format!("DELETE FROM {table} WHERE org_id = $1"))
            .bind(org_id)
            .execute(&mut **tx)
            .await?
            .rows_affected() as i64;
        counts.push((table, deleted));
    }

    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filename_is_sanitised_and_dated() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-19T22:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(export_filename("self-host", now), "gdpr-export-self-host-20260919.zip");
        // One dash per code point the regex refuses.
        assert_eq!(export_filename("org/1 2", now), "gdpr-export-org-1-2-20260919.zip");
        assert_eq!(export_filename("org_é.x", now), "gdpr-export-org_-.x-20260919.zip");
        // `str.replace` replaces every occurrence, including one the org
        // id brought with it.
        assert_eq!(export_filename("a.csvb", now), "gdpr-export-a.zipb-20260919.zip");
    }

    #[test]
    fn the_dump_is_shaped_like_json_dumps_indent_2() {
        let v = json!({"a": 1, "b": [1, 2], "c": {}, "d": []});
        assert_eq!(
            python_dumps_indented(&v),
            "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2\n  ],\n  \"c\": {},\n  \"d\": []\n}"
        );
    }
}
