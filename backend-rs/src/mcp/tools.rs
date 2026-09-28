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
    let rows: Vec<crate::models::CameraGroupRow> = sqlx::query_as(
        "SELECT id, org_id, name, color, icon, created_at
           FROM camera_groups WHERE org_id = $1 ORDER BY id",
    )
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
    let row: Option<(Option<bool>, Option<bool>, Option<String>, Option<String>)> =
        sqlx::query_as(
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
                    "expected": "HH:MM (24-hour), e.g. 22:00",
                }));
            }
        }
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

    let row: (Option<bool>, Option<bool>, Option<String>, Option<String>) = sqlx::query_as(
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
    let camera_id = opt_str(args, "camera_id");
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
