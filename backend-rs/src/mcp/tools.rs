//! The 23 MCP tools.
//!
//! Ported from `backend/app/mcp/server.py`. Each one is a function over
//! the pool and a JSON argument map, returning either a JSON value or
//! the message a `ToolError` would carry — the protocol layer in
//! `server.rs` turns the second into `isError: true` with the text as
//! the sole content block, which is what FastMCP puts on the wire.
//!
//! The bodies are thin. Almost every read maps onto a `to_dict()` shape
//! the REST routes already produce, so the work here is the argument
//! handling and the refusals, not the queries.

use serde_json::{json, Map, Value};

use crate::app::AppState;

/// `_MAX_SUMMARY_CHARS`.
const MAX_SUMMARY_CHARS: usize = 2_000;
/// `_MAX_OBSERVATION_CHARS`.
const MAX_OBSERVATION_CHARS: usize = 8_000;
/// `_MAX_REPORT_CHARS`.
const MAX_REPORT_CHARS: usize = 64_000;
/// `_MAX_INCIDENTS_PER_ORG_PER_DAY` — a looping or prompt-injected
/// agent writing at the rate limit would bury the inbox and bloat the
/// database. No legitimate workflow comes near this.
const MAX_INCIDENTS_PER_ORG_PER_DAY: i64 = 200;
/// `_MAX_CLIP_BYTES`.
const MAX_CLIP_BYTES: usize = 32 * 1024 * 1024;
/// `_APPROX_SEGMENT_SECONDS` — CameraNode emits one-second segments.
const APPROX_SEGMENT_SECONDS: f64 = 1.0;

pub const INCIDENT_STATUSES: [&str; 4] = ["open", "acknowledged", "resolved", "dismissed"];
pub const INCIDENT_SEVERITIES: [&str; 4] = ["low", "medium", "high", "critical"];

/// What a tool hands back: a value, or the text of a `ToolError`.
pub type ToolResult = Result<Value, String>;

/// The four recording columns, as they come back from a row.
type RecordingPolicy = (Option<bool>, Option<bool>, Option<String>, Option<String>);

/// One evidence row's metadata, without the blob.
type ClipMetadata = (
    String,
    Option<String>,
    Option<i32>,
    Option<String>,
    Option<String>,
    Option<chrono::NaiveDateTime>,
);

/// `_agent_label()` — the `created_by` stamp on anything an agent
/// writes, so a human reading an incident can tell which credential
/// authored it.
pub fn agent_label(key_name: &str) -> String {
    let name = if key_name.is_empty() { "unknown" } else { key_name };
    format!("mcp:{name}")
}

// ---------------------------------------------------------------------
// Argument helpers
//
// FastMCP validates arguments against the signature before the body
// runs, so a missing required argument or a wrong type never reaches
// these. What they reproduce is what the BODY does with a value that
// passed validation.
// ---------------------------------------------------------------------

/// `note.strip() if note else None`.
///
/// The truthiness test is on the UNSTRIPPED value, so an empty note is
/// NULL while a whitespace-only one is stored as the empty string. Both
/// read as "no caption" to a human and they are different rows.
fn python_note(note: Option<&str>) -> Option<&str> {
    note.filter(|n| !n.is_empty()).map(str::trim)
}

/// The daily incident-creation cap, as a refusal or nothing.
///
/// `>=`, so the 200th incident of the day is the last one allowed. The
/// off-by-one matters less for the extra row than for what the cap is:
/// a rail against a looping or prompt-injected agent, which is the one
/// place an off-by-one is a real regression rather than a rounding
/// difference.
///
/// A function rather than the comparison inline because reaching it
/// through the differential would mean a fixture carrying 200 incidents
/// in one day. Nothing else made it testable, and a `_unit_only` note
/// claiming it was already covered was simply wrong.
fn daily_cap_refusal(created_today: i64, cap: i64) -> Option<String> {
    if created_today >= cap {
        return Some(format!(
            "Daily incident-creation cap reached ({cap}/day). \
             Update an existing incident instead, or wait until tomorrow."
        ));
    }
    None
}

/// Concatenate the buffered segments, dropping the OLDEST until they fit.
///
/// MPEG-TS is byte-concatenation-safe, so the pieces play end to end
/// without remuxing. Which end the cap drops is the whole of the
/// behaviour: an agent asking for a clip wants the most RECENT video, so
/// a clip trimmed from the other end returns the beginning of the buffer
/// and reports the same duration for it.
///
/// A function rather than a loop inline in `attach_clip` so the cap can
/// be tested at all: the real cap is 32 MB, and reaching it through the
/// differential would mean pushing tens of megabytes into each tier's
/// segment cache per case. Here it takes three small chunks.
fn trim_to_cap(mut chunks: Vec<bytes::Bytes>, max_bytes: usize) -> (Vec<u8>, usize, bool) {
    let mut truncated = false;
    let mut total: usize = chunks.iter().map(|c| c.len()).sum();
    while !chunks.is_empty() && total > max_bytes {
        total -= chunks.remove(0).len();
        truncated = true;
    }
    let blob: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
    let count = chunks.len();
    (blob, count, truncated)
}

fn opt_str(args: &Map<String, Value>, key: &str) -> Option<String> {
    args.get(key)
        .filter(|v| !v.is_null())
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn req_str(args: &Map<String, Value>, key: &str) -> String {
    opt_str(args, key).unwrap_or_default()
}

fn opt_i64(args: &Map<String, Value>, key: &str) -> Option<i64> {
    args.get(key).filter(|v| !v.is_null()).and_then(Value::as_i64)
}

fn int_or(args: &Map<String, Value>, key: &str, default: i64) -> i64 {
    opt_i64(args, key).unwrap_or(default)
}

// ---------------------------------------------------------------------
// Cameras, nodes and groups
// ---------------------------------------------------------------------

pub async fn list_cameras(state: &AppState, org_id: &str) -> ToolResult {
    let rows: Vec<crate::models::CameraRow> =
        sqlx::query_as(&format!("{} WHERE c.org_id = $1", crate::models::CAMERA_SELECT))
            .bind(org_id)
            .fetch_all(&state.pool)
            .await
            .map_err(db_error)?;
    Ok(Value::Array(rows.iter().map(|row| row.to_json()).collect()))
}

pub async fn get_camera(state: &AppState, org_id: &str, args: &Map<String, Value>) -> ToolResult {
    let camera_id = req_str(args, "camera_id");
    let row = camera_row(state, org_id, &camera_id).await?;
    match row {
        Some(row) => Ok(row.to_json()),
        None => Err(format!("Camera '{camera_id}' not found")),
    }
}

pub async fn get_stream_url(
    state: &AppState,
    org_id: &str,
    args: &Map<String, Value>,
) -> ToolResult {
    let camera_id = req_str(args, "camera_id");
    if camera_row(state, org_id, &camera_id).await?.is_none() {
        return Err(format!("Camera '{camera_id}' not found"));
    }
    Ok(json!({
        "camera_id": camera_id,
        "stream_url": format!("/api/cameras/{camera_id}/stream.m3u8"),
        "format": "HLS",
        "note": "Requires Bearer auth. Open in the dashboard or an HLS player with auth headers.",
    }))
}

pub async fn list_camera_groups(state: &AppState, org_id: &str) -> ToolResult {
    // The REST route's query verbatim, so the dashboard's list and the
    // agent's cannot drift — `camera_count` is the whole of the shape.
    let rows: Vec<crate::models::CameraGroupRow> =
        sqlx::query_as(crate::api::cameras::CAMERA_GROUP_SELECT)
            .bind(org_id)
            .fetch_all(&state.pool)
            .await
            .map_err(db_error)?;
    Ok(Value::Array(rows.iter().map(|row| row.to_json()).collect()))
}

pub async fn list_nodes(state: &AppState, org_id: &str) -> ToolResult {
    let rows: Vec<crate::api::nodes::CameraNodeRow> =
        sqlx::query_as(&format!("{} WHERE n.org_id = $1", crate::api::nodes::NODE_SELECT))
            .bind(org_id)
            .fetch_all(&state.pool)
            .await
            .map_err(db_error)?;
    Ok(Value::Array(rows.iter().map(|row| row.to_json()).collect()))
}

pub async fn get_node(state: &AppState, org_id: &str, args: &Map<String, Value>) -> ToolResult {
    let node_id = req_str(args, "node_id");
    let row: Option<crate::api::nodes::CameraNodeRow> = sqlx::query_as(&format!(
        "{} WHERE n.org_id = $1 AND n.node_id = $2",
        crate::api::nodes::NODE_SELECT
    ))
    .bind(org_id)
    .bind(&node_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(db_error)?;
    match row {
        Some(row) => Ok(row.to_json()),
        None => Err(format!("Node '{node_id}' not found")),
    }
}

/// A camera that is missing is NOT an error here — it is a value with
/// an `error` key, which is the shape this one tool answers with.
pub async fn get_camera_recording_policy(
    state: &AppState,
    org_id: &str,
    args: &Map<String, Value>,
) -> ToolResult {
    let camera_id = req_str(args, "camera_id");
    let row: Option<RecordingPolicy> = sqlx::query_as(
        "SELECT continuous_24_7, scheduled_recording, scheduled_start, scheduled_end
           FROM cameras WHERE camera_id = $1 AND org_id = $2 LIMIT 1",
    )
        .bind(&camera_id)
        .bind(org_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(db_error)?;
    let Some((continuous, scheduled, start, end)) = row else {
        return Ok(json!({ "error": "camera_not_found", "camera_id": camera_id }));
    };
    Ok(json!({
        "camera_id": camera_id,
        "continuous_24_7": continuous.unwrap_or(false),
        "scheduled_recording": scheduled.unwrap_or(false),
        "scheduled_start": start,
        "scheduled_end": end,
    }))
}

pub async fn set_camera_recording_policy(
    state: &AppState,
    org_id: &str,
    args: &Map<String, Value>,
) -> ToolResult {
    let camera_id = req_str(args, "camera_id");
    let continuous = args.get("continuous_24_7").filter(|v| !v.is_null()).and_then(Value::as_bool);
    let scheduled = args
        .get("scheduled_recording")
        .filter(|v| !v.is_null())
        .and_then(Value::as_bool);
    let start = opt_str(args, "scheduled_start");
    let end = opt_str(args, "scheduled_end");

    let existing: Option<(i32,)> =
        sqlx::query_as("SELECT id FROM cameras WHERE camera_id = $1 AND org_id = $2 LIMIT 1")
            .bind(&camera_id)
            .bind(org_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(db_error)?;
    if existing.is_none() {
        return Ok(json!({ "error": "camera_not_found", "camera_id": camera_id }));
    }

    // Validated before assignment, so a bad value from an agent never
    // reaches the column the heartbeat's window check reads.
    for (label, value) in [("scheduled_start", &start), ("scheduled_end", &end)] {
        if let Some(value) = value {
            if !value.is_empty() && !is_hhmm(value) {
                return Ok(json!({
                    "error": "invalid_time_format",
                    "field": label,
                    "value": value,
                    "expected": "HH:MM 24-hour, e.g. 08:30",
                }));
            }
        }
    }

    // Continuous and scheduled cannot both be on: the heartbeat's
    // window check would silently ignore the schedule. An agent that
    // wants to switch modes has to pass the OFF for the old one in the
    // same call, which is also what the REST route requires.
    let current: (Option<bool>, Option<bool>) = sqlx::query_as(
        "SELECT continuous_24_7, scheduled_recording FROM cameras
          WHERE camera_id = $1 AND org_id = $2",
    )
    .bind(&camera_id)
    .bind(org_id)
    .fetch_one(&state.pool)
    .await
    .map_err(db_error)?;
    let next_continuous = continuous.unwrap_or(current.0.unwrap_or(false));
    let next_scheduled = scheduled.unwrap_or(current.1.unwrap_or(false));
    if next_continuous && next_scheduled {
        return Ok(json!({
            "error": "modes_conflict",
            "message": "continuous_24_7 and scheduled_recording can't both \
be true. Pass one as false in the same call to switch.",
        }));
    }

    let mut sets: Vec<String> = Vec::new();
    if continuous.is_some() {
        sets.push("continuous_24_7 = $3".into());
    }
    if scheduled.is_some() {
        sets.push("scheduled_recording = $4".into());
    }
    if start.is_some() {
        sets.push("scheduled_start = $5".into());
    }
    if end.is_some() {
        sets.push("scheduled_end = $6".into());
    }
    if !sets.is_empty() {
        let sql = format!(
            "UPDATE cameras SET {}, updated_at = $7 WHERE camera_id = $1 AND org_id = $2",
            sets.join(", ")
        );
        sqlx::query(&sql)
            .bind(&camera_id)
            .bind(org_id)
            .bind(continuous)
            .bind(scheduled)
            .bind(start.as_deref().map(empty_to_null))
            .bind(end.as_deref().map(empty_to_null))
            .bind(crate::models::now_naive())
            .execute(&state.pool)
            .await
            .map_err(db_error)?;
    }

    let row: RecordingPolicy = sqlx::query_as(
        "SELECT continuous_24_7, scheduled_recording, scheduled_start, scheduled_end
           FROM cameras WHERE camera_id = $1 AND org_id = $2",
    )
    .bind(&camera_id)
    .bind(org_id)
    .fetch_one(&state.pool)
    .await
    .map_err(db_error)?;
    Ok(json!({
        "success": true,
        "camera_id": camera_id,
        "continuous_24_7": row.0.unwrap_or(false),
        "scheduled_recording": row.1.unwrap_or(false),
        "scheduled_start": row.2,
        "scheduled_end": row.3,
    }))
}

/// `""` clears the window; anything else is stored as given.
fn empty_to_null(value: &str) -> Option<&str> {
    if value.is_empty() { None } else { Some(value) }
}

fn is_hhmm(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 5
        && bytes[2] == b':'
        && matches!(bytes[0], b'0' | b'1' | b'2')
        && bytes[1].is_ascii_digit()
        && !(bytes[0] == b'2' && bytes[1] > b'3')
        && (b'0'..=b'5').contains(&bytes[3])
        && bytes[4].is_ascii_digit()
}

// ---------------------------------------------------------------------
// Audit and status
// ---------------------------------------------------------------------

pub async fn get_stream_logs(
    state: &AppState,
    org_id: &str,
    args: &Map<String, Value>,
) -> ToolResult {
    // `if camera_id:` — an empty string is falsy, so it is no filter
    // at all rather than a filter that matches nothing.
    let camera_id = opt_str(args, "camera_id").filter(|c| !c.is_empty());
    let limit = int_or(args, "limit", 50);
    // The same row the audit route serves, so the two agree on the
    // shape a client sees.
    let rows: Vec<crate::api::stream_logs::StreamAccessLogRow> = sqlx::query_as(
        "SELECT id, user_id, user_email, org_id, camera_id, node_id, ip_address, accessed_at
           FROM stream_access_logs
          WHERE org_id = $1 AND ($2::text IS NULL OR camera_id = $2)
          ORDER BY accessed_at DESC
          LIMIT $3",
    )
    .bind(org_id)
    .bind(&camera_id)
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(db_error)?;
    Ok(Value::Array(rows.iter().map(|row| row.to_json()).collect()))
}

pub async fn get_stream_stats(
    state: &AppState,
    org_id: &str,
    args: &Map<String, Value>,
) -> ToolResult {
    let days = int_or(args, "days", 7);
    let cutoff = crate::models::now_naive() - chrono::Duration::days(days);

    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM stream_access_logs WHERE org_id = $1 AND accessed_at >= $2",
    )
    .bind(org_id)
    .bind(cutoff)
    .fetch_one(&state.pool)
    .await
    .map_err(db_error)?;

    // GROUP BY with no ORDER BY, like the Python — the row order is
    // whatever the planner returns, and the SPA sorts what it renders.
    let by_camera: Vec<(Option<String>, i64)> = sqlx::query_as(
        "SELECT camera_id, COUNT(id) FROM stream_access_logs
          WHERE org_id = $1 AND accessed_at >= $2 GROUP BY camera_id",
    )
    .bind(org_id)
    .bind(cutoff)
    .fetch_all(&state.pool)
    .await
    .map_err(db_error)?;

    let by_user: Vec<(Option<String>, Option<String>, i64)> = sqlx::query_as(
        "SELECT user_id, user_email, COUNT(id) FROM stream_access_logs
          WHERE org_id = $1 AND accessed_at >= $2 GROUP BY user_id, user_email",
    )
    .bind(org_id)
    .bind(cutoff)
    .fetch_all(&state.pool)
    .await
    .map_err(db_error)?;

    Ok(json!({
        "days": days,
        "total_views": total,
        "by_camera": by_camera
            .iter()
            .map(|(camera_id, views)| json!({ "camera_id": camera_id, "views": views }))
            .collect::<Vec<_>>(),
        "by_user": by_user
            .iter()
            .map(|(user_id, email, views)| json!({
                "user_id": user_id,
                // `email or ""` — a NULL column reports as empty.
                "email": email.clone().unwrap_or_default(),
                "views": views,
            }))
            .collect::<Vec<_>>(),
    }))
}

pub async fn get_system_status(state: &AppState, org_id: &str) -> ToolResult {
    let cameras: Vec<crate::models::CameraRow> =
        sqlx::query_as(&format!("{} WHERE c.org_id = $1", crate::models::CAMERA_SELECT))
            .bind(org_id)
            .fetch_all(&state.pool)
            .await
            .map_err(db_error)?;
    let nodes: Vec<crate::api::nodes::CameraNodeRow> =
        sqlx::query_as(&format!("{} WHERE n.org_id = $1", crate::api::nodes::NODE_SELECT))
            .bind(org_id)
            .fetch_all(&state.pool)
            .await
            .map_err(db_error)?;

    // `effective_status`, not the stored column: a camera whose last
    // heartbeat is 90 seconds old reads offline whatever the row says.
    let online_cameras = cameras
        .iter()
        .filter(|row| row.to_json()["status"].as_str() != Some("offline"))
        .count() as i64;
    let online_nodes = nodes
        .iter()
        .filter(|row| {
            !matches!(row.to_json()["status"].as_str(), Some("offline") | Some("pending"))
        })
        .count() as i64;

    // The NOMINAL plan — what the org pays for — not the effective one
    // the rate limiter used.
    let ctx = crate::api::sentinel_config::plan_ctx(state);
    let plan = crate::plans::resolve_org_plan(&ctx, org_id).await;

    Ok(json!({
        "org_id": org_id,
        "plan": plan,
        "cameras": {
            "total": cameras.len(),
            "online": online_cameras,
            "offline": cameras.len() as i64 - online_cameras,
        },
        "nodes": {
            "total": nodes.len(),
            "online": online_nodes,
            "offline": nodes.len() as i64 - online_nodes,
        },
    }))
}

async fn camera_row(
    state: &AppState,
    org_id: &str,
    camera_id: &str,
) -> Result<Option<crate::models::CameraRow>, String> {
    sqlx::query_as(&format!(
        "{} WHERE c.org_id = $1 AND c.camera_id = $2",
        crate::models::CAMERA_SELECT
    ))
    .bind(org_id)
    .bind(camera_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(db_error)
}

/// Python's bare `except Exception` around the resolver turns any
/// database failure into this, and nothing more specific is surfaced.
fn db_error(err: sqlx::Error) -> String {
    tracing::error!(error = %err, "mcp tool query failed");
    "Authentication error".to_string()
}

// ---------------------------------------------------------------------
// Incidents
//
// The agent's only write surface, and the reason the allowlist lets it
// write at all: authoring an incident is the hand-off from automated
// triage to a human, and everything here is additive.
// ---------------------------------------------------------------------

pub async fn list_incidents(
    state: &AppState,
    org_id: &str,
    args: &Map<String, Value>,
) -> ToolResult {
    let status = opt_str(args, "status");
    let severity = opt_str(args, "severity");
    let camera_id = opt_str(args, "camera_id");
    if let Some(status) = &status {
        if !INCIDENT_STATUSES.contains(&status.as_str()) {
            return Err(format!(
                "Invalid status '{status}'. Must be one of: {}",
                INCIDENT_STATUSES.join(", ")
            ));
        }
    }
    if let Some(severity) = &severity {
        if !INCIDENT_SEVERITIES.contains(&severity.as_str()) {
            return Err(format!(
                "Invalid severity '{severity}'. Must be one of: {}",
                INCIDENT_SEVERITIES.join(", ")
            ));
        }
    }
    let limit = int_or(args, "limit", 20);
    let offset = int_or(args, "offset", 0);

    let filters = "WHERE i.org_id = $1
          AND ($2::text IS NULL OR i.status = $2)
          AND ($3::text IS NULL OR i.severity = $3)
          AND ($4::text IS NULL OR i.camera_id = $4)";
    let (total,): (i64,) = sqlx::query_as(&format!(
        "SELECT COUNT(*) FROM incidents i {filters}"
    ))
    .bind(org_id)
    .bind(&status)
    .bind(&severity)
    .bind(&camera_id)
    .fetch_one(&state.pool)
    .await
    .map_err(db_error)?;

    let rows: Vec<crate::api::incidents::IncidentRow> = sqlx::query_as(&format!(
        "{} {filters} ORDER BY i.created_at DESC OFFSET $5 LIMIT $6",
        crate::api::incidents::INCIDENT_SELECT
    ))
    .bind(org_id)
    .bind(&status)
    .bind(&severity)
    .bind(&camera_id)
    .bind(offset)
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(db_error)?;

    let incidents: Vec<Value> = rows
        .iter()
        .map(|row| {
            let mut value = row.to_json();
            // The body is stripped from the list view — `get_incident`
            // reads it — but a flag stays so an agent can tell whether
            // a second call is worth making.
            // `shift_remove`, not `remove`: with `preserve_order`,
            // serde_json's `remove` is a SWAP-remove, which would move
            // the last key (`evidence_count`) into the hole `report`
            // left. Python's `pop` closes the hole instead, and the key
            // order is on the wire.
            let report = value
                .as_object_mut()
                .and_then(|map| map.shift_remove("report"))
                .unwrap_or(Value::Null);
            let has_report = report.as_str().is_some_and(|text| !text.trim().is_empty());
            value["has_report"] = json!(has_report);
            value
        })
        .collect();

    Ok(json!({
        "total": total,
        "returned": incidents.len(),
        "offset": offset,
        "limit": limit,
        "incidents": incidents,
    }))
}

pub async fn get_incident(
    state: &AppState,
    org_id: &str,
    args: &Map<String, Value>,
) -> ToolResult {
    let incident_id = int_or(args, "incident_id", 0);
    let row = owned_incident(state, org_id, incident_id).await?;
    let mut value = row.to_json();
    value["evidence"] = Value::Array(evidence_for(state, incident_id).await?);
    Ok(value)
}

pub async fn create_incident(
    state: &AppState,
    org_id: &str,
    key_name: &str,
    args: &Map<String, Value>,
) -> ToolResult {
    let title = req_str(args, "title");
    let summary = req_str(args, "summary");
    let severity = opt_str(args, "severity").unwrap_or_else(|| "medium".to_string());
    let camera_id = opt_str(args, "camera_id");

    // Validated BEFORE auth in the Python, so these refusals happen
    // whatever the key's plan or budget.
    if !INCIDENT_SEVERITIES.contains(&severity.as_str()) {
        return Err(format!(
            "Invalid severity '{severity}'. Must be one of: {}",
            INCIDENT_SEVERITIES.join(", ")
        ));
    }
    if title.trim().is_empty() {
        return Err("title is required".to_string());
    }
    if summary.trim().is_empty() {
        return Err("summary is required".to_string());
    }
    if summary.chars().count() > MAX_SUMMARY_CHARS {
        return Err(format!(
            "summary too long ({} chars; max {MAX_SUMMARY_CHARS}). \
             Put detail in the report via finalize_incident.",
            summary.chars().count()
        ));
    }

    // `datetime.now().replace(hour=0, ...)` — midnight by the
    // process's clock, which is UTC everywhere this runs.
    let day_start = crate::models::now_naive()
        .date()
        .and_hms_opt(0, 0, 0)
        .unwrap_or_else(crate::models::now_naive);
    let (created_today,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM incidents WHERE org_id = $1 AND created_at >= $2",
    )
    .bind(org_id)
    .bind(day_start)
    .fetch_one(&state.pool)
    .await
    .map_err(db_error)?;
    if let Some(refusal) = daily_cap_refusal(created_today, MAX_INCIDENTS_PER_ORG_PER_DAY) {
        return Err(refusal);
    }

    // A camera named on an incident has to be this org's — otherwise
    // the audit trail carries a reference that resolves to nothing.
    if let Some(camera_id) = camera_id.as_deref().filter(|c| !c.is_empty()) {
        if camera_row(state, org_id, camera_id).await?.is_none() {
            return Err(format!("Camera '{camera_id}' not found"));
        }
    }

    let now = crate::models::now_naive();
    // `title.strip()[:200]` — trimmed, then cut to the column width in
    // characters.
    let stored_title: String = title.trim().chars().take(200).collect();
    let (incident_id,): (i32,) = sqlx::query_as(
        // `report` is named explicitly: SQLAlchemy fills it from
        // `default=""` on insert, and leaving it out stores NULL — which
        // `finalize_incident` then reads as "no report yet" correctly by
        // accident and `list_incidents` reports differently.
        "INSERT INTO incidents
            (org_id, camera_id, title, summary, report, severity, status, created_by,
             created_at, updated_at)
         VALUES ($1, $2, $3, $4, '', $5, 'open', $6, $7, $7)
         RETURNING id",
    )
    .bind(org_id)
    // Verbatim, INCLUDING an empty string: Python's `if camera_id:`
    // guards the existence check above and nothing else — the column
    // is assigned the argument as given, so `""` is stored as `""`.
    .bind(&camera_id)
    .bind(&stored_title)
    .bind(summary.trim())
    .bind(&severity)
    .bind(agent_label(key_name))
    .bind(now)
    .fetch_one(&state.pool)
    .await
    .map_err(db_error)?;

    // Audience `all`, not `admin`: any member should know an agent
    // created an incident in their environment — this is the hand-off
    // from automated triage to human review.
    let notif_severity = if matches!(severity.as_str(), "high" | "critical") {
        "critical"
    } else {
        "warning"
    };
    let mut notification = crate::notifications::NewNotification::new(
        "incident_created",
        format!("Incident #{incident_id}: {stored_title}"),
    );
    notification.body = format!("[{}] {}", severity.to_uppercase(), summary.trim());
    notification.severity = notif_severity.to_string();
    notification.audience = "all".to_string();
    notification.link = Some(format!("/incidents/{incident_id}"));
    notification.camera_id = camera_id.clone();
    // `created_by` is what lets the dispatcher refuse to re-trigger on
    // an agent-authored incident. Without it: motion, run, incident,
    // notification, NEW run — self-amplifying until the monthly cap is
    // spent, with an email per cycle.
    notification.meta = Some(json!({
        "incident_id": incident_id,
        "severity": severity,
        "created_by": "mcp",
    }));
    crate::notifications::create_notification(state, org_id, notification).await;

    let row = owned_incident(state, org_id, i64::from(incident_id)).await?;
    Ok(row.to_json())
}

pub async fn add_observation(
    state: &AppState,
    org_id: &str,
    args: &Map<String, Value>,
) -> ToolResult {
    let incident_id = int_or(args, "incident_id", 0);
    let text = req_str(args, "text");
    let camera_id = opt_str(args, "camera_id");

    if text.trim().is_empty() {
        return Err("text is required".to_string());
    }
    if text.chars().count() > MAX_OBSERVATION_CHARS {
        return Err(format!(
            "text too long ({} chars; max {MAX_OBSERVATION_CHARS}).",
            text.chars().count()
        ));
    }

    owned_incident(state, org_id, incident_id).await?;

    // Same org scope as the attach tools: an agent must not record a
    // foreign camera id on this org's incident. Not a leak — the
    // camera is never queried — but it pollutes the audit trail with
    // references that do not resolve here.
    if let Some(camera_id) = &camera_id {
        if camera_row(state, org_id, camera_id).await?.is_none() {
            return Err(format!(
                "Camera '{camera_id}' not found in this organization"
            ));
        }
    }

    let now = crate::models::now_naive();
    let (evidence_id,): (i32,) = sqlx::query_as(
        "INSERT INTO incident_evidence (incident_id, kind, text, camera_id, timestamp)
         VALUES ($1, 'observation', $2, $3, $4) RETURNING id",
    )
    .bind(incident_id as i32)
    .bind(text.trim())
    .bind(&camera_id)
    .bind(now)
    .fetch_one(&state.pool)
    .await
    .map_err(db_error)?;
    touch_incident(state, org_id, incident_id, now).await?;

    let row: crate::api::incidents::EvidenceRow = sqlx::query_as(
        "SELECT id, incident_id, kind, text, camera_id, data_mime, timestamp
           FROM incident_evidence WHERE id = $1",
    )
    .bind(evidence_id)
    .fetch_one(&state.pool)
    .await
    .map_err(db_error)?;
    Ok(row.to_json())
}

pub async fn update_incident(
    state: &AppState,
    org_id: &str,
    key_name: &str,
    args: &Map<String, Value>,
) -> ToolResult {
    let incident_id = int_or(args, "incident_id", 0);
    let status = opt_str(args, "status");
    let severity = opt_str(args, "severity");
    let summary = opt_str(args, "summary");
    let report = opt_str(args, "report");

    if let Some(status) = &status {
        if !INCIDENT_STATUSES.contains(&status.as_str()) {
            return Err(format!(
                "Invalid status '{status}'. Must be one of: {}",
                INCIDENT_STATUSES.join(", ")
            ));
        }
    }
    if let Some(severity) = &severity {
        if !INCIDENT_SEVERITIES.contains(&severity.as_str()) {
            return Err(format!(
                "Invalid severity '{severity}'. Must be one of: {}",
                INCIDENT_SEVERITIES.join(", ")
            ));
        }
    }
    // A blank report is refused rather than treated as "leave it": the
    // agent has to be explicit, because the alternative silently
    // destroys a body it would have to regenerate.
    if report.as_ref().is_some_and(|r| r.trim().is_empty()) {
        return Err(
            "report cannot be empty — pass None to leave the existing report \
             unchanged, or pass the full revised markdown body"
                .to_string(),
        );
    }

    let existing = owned_incident(state, org_id, incident_id).await?;
    let existing_status = existing.to_json()["status"].as_str().unwrap_or("").to_string();

    let now = crate::models::now_naive();
    if let Some(status) = &status {
        // Entering a terminal state stamps who closed it; reopening
        // clears both, so a reopened incident does not claim to have
        // been resolved by someone.
        if matches!(status.as_str(), "resolved" | "dismissed")
            && !matches!(existing_status.as_str(), "resolved" | "dismissed")
        {
            sqlx::query(
                "UPDATE incidents SET resolved_at = $1, resolved_by = $2, updated_at = $1
                  WHERE id = $3",
            )
            .bind(now)
            .bind(agent_label(key_name))
            .bind(incident_id as i32)
            .execute(&state.pool)
            .await
            .map_err(db_error)?;
        } else if status == "open" {
            sqlx::query(
                "UPDATE incidents SET resolved_at = NULL, resolved_by = NULL, updated_at = $1
                  WHERE id = $2",
            )
            .bind(now)
            .bind(incident_id as i32)
            .execute(&state.pool)
            .await
            .map_err(db_error)?;
        }
        sqlx::query("UPDATE incidents SET status = $1, updated_at = $2 WHERE id = $3")
            .bind(status)
            .bind(now)
            .bind(incident_id as i32)
            .execute(&state.pool)
            .await
            .map_err(db_error)?;
    }
    if let Some(severity) = &severity {
        sqlx::query("UPDATE incidents SET severity = $1, updated_at = $2 WHERE id = $3")
            .bind(severity)
            .bind(now)
            .bind(incident_id as i32)
            .execute(&state.pool)
            .await
            .map_err(db_error)?;
    }
    if let Some(summary) = &summary {
        if summary.trim().is_empty() {
            return Err("summary cannot be blank — pass None to leave it unchanged".to_string());
        }
        if summary.chars().count() > MAX_SUMMARY_CHARS {
            return Err(format!(
                "summary too long ({} chars; max {MAX_SUMMARY_CHARS})",
                summary.chars().count()
            ));
        }
        sqlx::query("UPDATE incidents SET summary = $1, updated_at = $2 WHERE id = $3")
            .bind(summary.trim())
            .bind(now)
            .bind(incident_id as i32)
            .execute(&state.pool)
            .await
            .map_err(db_error)?;
    }
    if let Some(report) = &report {
        if report.chars().count() > MAX_REPORT_CHARS {
            return Err(format!(
                "report too long ({} chars; max {MAX_REPORT_CHARS})",
                report.chars().count()
            ));
        }
        sqlx::query("UPDATE incidents SET report = $1, updated_at = $2 WHERE id = $3")
            .bind(report.trim())
            .bind(now)
            .bind(incident_id as i32)
            .execute(&state.pool)
            .await
            .map_err(db_error)?;
    }

    // `updated_at` has an onupdate, so any of the writes above moved
    // it; a call that changed nothing leaves it alone.
    let row = owned_incident(state, org_id, incident_id).await?;
    Ok(row.to_json())
}

pub async fn finalize_incident(
    state: &AppState,
    org_id: &str,
    args: &Map<String, Value>,
) -> ToolResult {
    let incident_id = int_or(args, "incident_id", 0);
    let report = req_str(args, "report");
    if report.trim().is_empty() {
        return Err("report is required".to_string());
    }
    if report.chars().count() > MAX_REPORT_CHARS {
        return Err(format!(
            "report too long ({} chars; max {MAX_REPORT_CHARS})",
            report.chars().count()
        ));
    }

    let existing = owned_incident(state, org_id, incident_id).await?;
    // The contract is "the FIRST report write". A silent overwrite here
    // destroys a body the agent would have to hold in context to
    // reproduce, which is exactly what update_incident requires.
    if existing
        .to_json()["report"]
        .as_str()
        .is_some_and(|text| !text.is_empty())
    {
        return Err(format!(
            "Incident {incident_id} already has a report. Use update_incident \
             with the full revised text to change it."
        ));
    }

    let now = crate::models::now_naive();
    sqlx::query("UPDATE incidents SET report = $1, updated_at = $2 WHERE id = $3")
        .bind(report.trim())
        .bind(now)
        .bind(incident_id as i32)
        .execute(&state.pool)
        .await
        .map_err(db_error)?;
    let row = owned_incident(state, org_id, incident_id).await?;
    Ok(row.to_json())
}

async fn owned_incident(
    state: &AppState,
    org_id: &str,
    incident_id: i64,
) -> Result<crate::api::incidents::IncidentRow, String> {
    let row: Option<crate::api::incidents::IncidentRow> = sqlx::query_as(&format!(
        "{} WHERE i.id = $1 AND i.org_id = $2",
        crate::api::incidents::INCIDENT_SELECT
    ))
    .bind(incident_id as i32)
    .bind(org_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(db_error)?;
    row.ok_or_else(|| format!("Incident {incident_id} not found"))
}

async fn evidence_for(state: &AppState, incident_id: i64) -> Result<Vec<Value>, String> {
    let rows: Vec<crate::api::incidents::EvidenceRow> = sqlx::query_as(
        "SELECT id, incident_id, kind, text, camera_id, data_mime, timestamp
           FROM incident_evidence WHERE incident_id = $1 ORDER BY timestamp",
    )
    .bind(incident_id as i32)
    .fetch_all(&state.pool)
    .await
    .map_err(db_error)?;
    Ok(rows.iter().map(|row| row.to_json()).collect())
}

/// The org filter on the touch is belt and braces — ownership was
/// checked before — but it makes a future where `org_id` becomes
/// mutable a silent no-op rather than a write to the wrong row.
async fn touch_incident(
    state: &AppState,
    org_id: &str,
    incident_id: i64,
    now: chrono::NaiveDateTime,
) -> Result<(), String> {
    sqlx::query("UPDATE incidents SET updated_at = $1 WHERE id = $2 AND org_id = $3")
        .bind(now)
        .bind(incident_id as i32)
        .bind(org_id)
        .execute(&state.pool)
        .await
        .map_err(db_error)?;
    Ok(())
}

// ---------------------------------------------------------------------
// Media: what the agent can actually see, and what it attaches
// ---------------------------------------------------------------------

/// A tool that answers with media rather than JSON.
///
/// FastMCP's `Image` return becomes an image content block; a list of
/// them becomes several, with a plain string for any frame that
/// failed. Modelled here so the protocol layer can build the same
/// blocks without every tool knowing about the wire format.
pub enum ToolOutput {
    Json(Value),
    /// `(jpeg bytes, mime)`.
    Image(Vec<u8>, String),
    /// A mixed sequence, which only `watch_camera` produces.
    Frames(Vec<Frame>),
}

pub enum Frame {
    Image(Vec<u8>, String),
    Text(String),
}

pub async fn view_camera(
    state: &AppState,
    org_id: &str,
    args: &Map<String, Value>,
) -> Result<ToolOutput, String> {
    let camera_id = req_str(args, "camera_id");
    // The camera is checked before the node, so an unknown id says so
    // rather than blaming a node that has nothing to do with it.
    if camera_row(state, org_id, &camera_id).await?.is_none() {
        return Err(format!("Camera '{camera_id}' not found"));
    }
    let (jpeg, _node_id) = crate::mcp::snapshot::capture_bytes(&state.pool, org_id, &camera_id)
        .await
        .map_err(|err| err.0)?;
    Ok(ToolOutput::Image(jpeg, "image/jpeg".to_string()))
}

pub async fn watch_camera(
    state: &AppState,
    org_id: &str,
    args: &Map<String, Value>,
) -> Result<ToolOutput, String> {
    let camera_id = req_str(args, "camera_id");
    let count = int_or(args, "count", 3).max(0) as usize;
    let interval = int_or(args, "interval_seconds", 5).max(0) as u64;

    if camera_row(state, org_id, &camera_id).await?.is_none() {
        return Err(format!("Camera '{camera_id}' not found"));
    }
    let node_id = node_for_camera(state, org_id, &camera_id).await?;
    if !crate::ws::MANAGER.is_connected(&node_id) {
        // Plural here, singular in `view_camera` — the messages differ
        // and both are what a user reads.
        return Err(format!(
            "Node '{node_id}' is offline — cannot capture snapshots"
        ));
    }

    let mut frames = Vec::with_capacity(count);
    for index in 0..count {
        if index > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        }
        let result = crate::ws::MANAGER
            .send_command(
                &node_id,
                "take_snapshot",
                json!({ "camera_id": camera_id }),
                std::time::Duration::from_secs(15),
            )
            .await;
        match result {
            Ok(value) => {
                // Deliberately NOT `extract_image_b64`: this tool reads
                // the field directly and reports a frame with no image
                // as text, rather than failing the whole burst on one
                // bad frame.
                let image = value
                    .get("data")
                    .and_then(Value::as_object)
                    .and_then(|data| data.get("image_b64"))
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .or_else(|| {
                        value
                            .get("image_b64")
                            .and_then(Value::as_str)
                            .filter(|v| !v.is_empty())
                    });
                match image.map(crate::crypto::python_b64decode) {
                    Some(Ok(bytes)) => frames.push(Frame::Image(bytes, "image/jpeg".into())),
                    // A frame whose base64 will not decode raises out of
                    // the loop in Python — `b64decode` is outside the
                    // `except (TimeoutError, ValueError)` it catches.
                    Some(Err(reason)) => return Err(reason),
                    None => frames.push(Frame::Text(format!(
                        "[Frame {}] No image data returned",
                        index + 1
                    ))),
                }
            }
            Err(err) => frames.push(Frame::Text(format!(
                "[Frame {}] Failed: {err}",
                index + 1
            ))),
        }
    }

    if !frames.iter().any(|f| matches!(f, Frame::Image(..))) {
        return Err("Failed to capture any snapshots — check node status".to_string());
    }
    Ok(ToolOutput::Frames(frames))
}

pub async fn attach_snapshot(
    state: &AppState,
    org_id: &str,
    args: &Map<String, Value>,
) -> ToolResult {
    let incident_id = int_or(args, "incident_id", 0);
    let camera_id = req_str(args, "camera_id");
    let note = opt_str(args, "note");

    // Ownership before the node round trip: there is no point waking a
    // camera for an incident that is not this org's.
    owned_incident(state, org_id, incident_id).await?;
    let (jpeg, _node_id) = crate::mcp::snapshot::capture_bytes(&state.pool, org_id, &camera_id)
        .await
        .map_err(|err| err.0)?;

    let now = crate::models::now_naive();
    let (evidence_id,): (i32,) = sqlx::query_as(
        "INSERT INTO incident_evidence
            (incident_id, kind, text, camera_id, data, data_mime, timestamp)
         VALUES ($1, 'snapshot', $2, $3, $4, 'image/jpeg', $5) RETURNING id",
    )
    .bind(incident_id as i32)
    .bind(python_note(note.as_deref()))
    .bind(&camera_id)
    .bind(&jpeg)
    .bind(now)
    .fetch_one(&state.pool)
    .await
    .map_err(db_error)?;
    touch_incident(state, org_id, incident_id, now).await?;

    let row: crate::api::incidents::EvidenceRow = sqlx::query_as(
        "SELECT id, incident_id, kind, text, camera_id, data_mime, timestamp
           FROM incident_evidence WHERE id = $1",
    )
    .bind(evidence_id)
    .fetch_one(&state.pool)
    .await
    .map_err(db_error)?;
    Ok(row.to_json())
}

pub async fn attach_clip(
    state: &AppState,
    org_id: &str,
    args: &Map<String, Value>,
) -> ToolResult {
    let incident_id = int_or(args, "incident_id", 0);
    let camera_id = req_str(args, "camera_id");
    let duration = int_or(args, "duration_seconds", 15);
    let note = opt_str(args, "note");

    owned_incident(state, org_id, incident_id).await?;
    if camera_row(state, org_id, &camera_id).await?.is_none() {
        return Err(format!("Camera '{camera_id}' not found"));
    }

    // `max(1, round(duration / 1.0))`.
    let wanted = (crate::pyrepr::round_half_even(duration as f64 / APPROX_SEGMENT_SECONDS) as i64)
        .max(1) as usize;
    let chunks = match state.hls.snapshot_recent(&camera_id, wanted) {
        crate::hls::Snapshot::NoCamera => {
            return Err(format!(
                "No buffered segments for camera '{camera_id}'. The stream must be \
                 live (or have been live very recently) for attach_clip to work."
            ))
        }
        crate::hls::Snapshot::Segments(chunks) if chunks.is_empty() => {
            return Err(format!(
                "Buffer entries for '{camera_id}' were evicted before they could \
                 be read; try again."
            ))
        }
        crate::hls::Snapshot::Segments(chunks) => chunks,
    };

    let (blob, segment_count, truncated) = trim_to_cap(chunks, MAX_CLIP_BYTES);
    let approx_duration =
        crate::pyrepr::round_to(segment_count as f64 * APPROX_SEGMENT_SECONDS, 1);

    let now = crate::models::now_naive();
    // The duration rides along as a MIME parameter so the playback
    // route can fill in EXTINF without a schema migration; browsers
    // ignore parameters they do not know on `video/mp2t`.
    let mime = format!(
        "video/mp2t;duration={}",
        crate::pyrepr::repr_float(approx_duration)
    );
    let (evidence_id,): (i32,) = sqlx::query_as(
        "INSERT INTO incident_evidence
            (incident_id, kind, text, camera_id, data, data_mime, timestamp)
         VALUES ($1, 'clip', $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(incident_id as i32)
    .bind(python_note(note.as_deref()))
    .bind(&camera_id)
    .bind(&blob)
    .bind(&mime)
    .bind(now)
    .fetch_one(&state.pool)
    .await
    .map_err(db_error)?;
    touch_incident(state, org_id, incident_id, now).await?;

    // No re-SELECT of the row: that would pull the multi-megabyte blob
    // back out to build a dict from values already in hand.
    let mut result = json!({
        "id": evidence_id,
        "incident_id": incident_id,
        "kind": "clip",
        "text": python_note(note.as_deref()),
        "camera_id": camera_id,
        // `data_mime is not None`, not "a blob was written" — the two
        // come apart on a row with data and no MIME, which the fixture
        // now carries and `get_incident_clip` below reads.
        "has_data": true,
        "data_mime": mime,
        "timestamp": crate::models::iso_naive(now),
        "segment_count": segment_count,
        "approx_duration_seconds": approx_duration,
        "bytes": blob.len(),
    });
    if truncated {
        result["truncated"] = json!(true);
        result["note"] = json!(format!(
            "Clip truncated to the newest ~{}s to fit the {} MB evidence cap.",
            crate::pyrepr::repr_float(approx_duration),
            MAX_CLIP_BYTES / (1024 * 1024)
        ));
    }
    Ok(result)
}

pub async fn get_incident_snapshot(
    state: &AppState,
    org_id: &str,
    args: &Map<String, Value>,
) -> Result<ToolOutput, String> {
    let incident_id = int_or(args, "incident_id", 0);
    let evidence_id = int_or(args, "evidence_id", 0);
    owned_incident(state, org_id, incident_id).await?;

    let row: Option<(String, Option<String>, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT kind, data_mime, data FROM incident_evidence
          WHERE id = $1 AND incident_id = $2 LIMIT 1",
    )
    .bind(evidence_id as i32)
    .bind(incident_id as i32)
    .fetch_optional(&state.pool)
    .await
    .map_err(db_error)?;
    let Some((kind, mime, data)) = row else {
        return Err(format!(
            "Evidence {evidence_id} not found on incident {incident_id}"
        ));
    };
    let Some(data) = data.filter(|_| kind == "snapshot") else {
        return Err(format!(
            "Evidence {evidence_id} is not a snapshot with attached image data"
        ));
    };

    // The stored mime maps onto the format FastMCP's Image takes;
    // anything unrecognised falls back to jpeg, because the bytes
    // most likely still decode.
    let mime = mime.unwrap_or_else(|| "image/jpeg".to_string()).to_lowercase();
    let format = match mime.as_str() {
        "image/png" => "image/png",
        "image/webp" => "image/webp",
        _ => "image/jpeg",
    };
    Ok(ToolOutput::Image(data, format.to_string()))
}

pub async fn get_incident_clip(
    state: &AppState,
    org_id: &str,
    args: &Map<String, Value>,
) -> ToolResult {
    let incident_id = int_or(args, "incident_id", 0);
    let evidence_id = int_or(args, "evidence_id", 0);
    owned_incident(state, org_id, incident_id).await?;

    // The byte length comes from SQL. This is a METADATA tool, and
    // touching the deferred blob would pull megabytes out just to
    // measure them.
    let row: Option<ClipMetadata> = sqlx::query_as(
        "SELECT kind, data_mime, length(data), text, camera_id, timestamp
           FROM incident_evidence WHERE id = $1 AND incident_id = $2 LIMIT 1",
    )
        .bind(evidence_id as i32)
        .bind(incident_id as i32)
        .fetch_optional(&state.pool)
        .await
        .map_err(db_error)?;
    let Some((kind, mime, byte_len, text, camera_id, timestamp)) = row else {
        return Err(format!(
            "Evidence {evidence_id} not found on incident {incident_id}"
        ));
    };
    let Some(byte_len) = byte_len.filter(|_| kind == "clip") else {
        return Err(format!(
            "Evidence {evidence_id} is not a clip with attached video data"
        ));
    };

    let raw_mime = mime.clone().unwrap_or_else(|| "video/mp2t".to_string());
    let (base_mime, approx_duration) = split_clip_mime(&raw_mime);

    Ok(json!({
        "id": evidence_id,
        "incident_id": incident_id,
        "kind": kind,
        "text": text,
        "camera_id": camera_id,
        // Both from the STORED value, which may be NULL even on a row
        // that has a blob: `to_dict` reports `has_data` from the MIME
        // and not from the data. Only `mime` below takes the fallback.
        "has_data": mime.is_some(),
        "data_mime": mime,
        "timestamp": timestamp.map(crate::models::iso_naive),
        "mime": base_mime,
        "approx_duration_seconds": approx_duration,
        "bytes": byte_len,
        "playback_hint": "A human reviewer can play this clip from the dashboard's \
             incident detail view; agents cannot watch video directly.",
    }))
}

/// `video/mp2t;duration=12.0` becomes `("video/mp2t", 12.0)`.
///
/// A parameter that will not parse is skipped rather than fatal, and a
/// later `duration=` wins over an earlier one — the Python loop keeps
/// going.
fn split_clip_mime(raw: &str) -> (String, Option<f64>) {
    if !raw.contains(';') {
        return (raw.to_string(), None);
    }
    let mut parts = raw.split(';').map(str::trim);
    let base = parts.next().unwrap_or("").to_string();
    let mut duration = None;
    for part in parts {
        if let Some(value) = part.strip_prefix("duration=") {
            if let Some(parsed) = crate::pyrepr::python_float(value) {
                duration = Some(parsed);
            }
        }
    }
    (base, duration)
}

async fn node_for_camera(
    state: &AppState,
    org_id: &str,
    camera_id: &str,
) -> Result<String, String> {
    let row: Option<(Option<i32>,)> =
        sqlx::query_as("SELECT node_id FROM cameras WHERE org_id = $1 AND camera_id = $2 LIMIT 1")
            .bind(org_id)
            .bind(camera_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(db_error)?;
    let node_ref = row.and_then(|(node_id,)| node_id);
    let node: Option<(String,)> = match node_ref {
        Some(id) => sqlx::query_as("SELECT node_id FROM camera_nodes WHERE id = $1 LIMIT 1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await
            .map_err(db_error)?,
        None => None,
    };
    node.map(|(node_id,)| node_id)
        .ok_or_else(|| format!("Camera '{camera_id}' has no assigned node"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(byte: u8, len: usize) -> bytes::Bytes {
        bytes::Bytes::from(vec![byte; len])
    }

    /// The cap drops the OLDEST segments, keeping the newest video —
    /// which is what an agent asking for a clip wants.
    ///
    /// Unreachable through the differential: the real cap is 32 MB, so a
    /// case that triggered it would have to push tens of megabytes into
    /// each tier's segment cache. Tested here against a cap of ten
    /// bytes instead.
    #[test]
    fn a_clip_over_the_cap_loses_its_oldest_segments() {
        let chunks = vec![chunk(1, 4), chunk(2, 4), chunk(3, 4)];
        let (blob, count, truncated) = trim_to_cap(chunks, 10);
        assert!(truncated);
        assert_eq!(count, 2, "two of the three fit under ten bytes");
        // The 2s and 3s, not the 1s and 2s.
        assert_eq!(blob, vec![2, 2, 2, 2, 3, 3, 3, 3]);
    }

    /// Exactly at the cap is not over it: `>` and not `>=`, so a clip
    /// that fits perfectly keeps every segment and is not reported
    /// truncated.
    #[test]
    fn a_clip_exactly_at_the_cap_is_untouched() {
        let chunks = vec![chunk(1, 4), chunk(2, 4)];
        let (blob, count, truncated) = trim_to_cap(chunks, 8);
        assert!(!truncated);
        assert_eq!(count, 2);
        assert_eq!(blob.len(), 8);
    }

    /// A single segment larger than the cap leaves nothing — the loop
    /// stops at an empty list rather than underflowing the running
    /// total, which is what `!chunks.is_empty()` is for.
    #[test]
    fn one_oversized_segment_leaves_an_empty_clip() {
        let (blob, count, truncated) = trim_to_cap(vec![chunk(9, 100)], 10);
        assert!(truncated);
        assert_eq!(count, 0);
        assert!(blob.is_empty());
    }

    /// `>=` and not `>`: the 200th incident of the day is the last one
    /// allowed, so a count already AT the cap is refused.
    #[test]
    fn the_daily_cap_refuses_at_the_limit_not_past_it() {
        assert_eq!(daily_cap_refusal(0, 3), None);
        assert_eq!(daily_cap_refusal(2, 3), None, "the third is still allowed");
        assert!(daily_cap_refusal(3, 3).is_some(), "the fourth is not");
        assert!(daily_cap_refusal(9, 3).is_some());
        // The message names the cap, because an agent reading it has to
        // know whether to wait or to update an existing incident.
        assert_eq!(
            daily_cap_refusal(3, 3).unwrap(),
            "Daily incident-creation cap reached (3/day). Update an existing \
             incident instead, or wait until tomorrow."
        );
    }

    /// `note.strip() if note else None`: the empty string is falsy and
    /// becomes NULL, a whitespace-only one is truthy and becomes "".
    #[test]
    fn an_empty_note_is_null_and_a_blank_one_is_empty() {
        assert_eq!(python_note(None), None);
        assert_eq!(python_note(Some("")), None);
        assert_eq!(python_note(Some("   ")), Some(""));
        assert_eq!(python_note(Some("  hi  ")), Some("hi"));
    }
}
