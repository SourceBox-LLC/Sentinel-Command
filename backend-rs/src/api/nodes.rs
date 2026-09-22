//! Camera node routes.
//!
//! Only `GET /api/nodes/{node_id}` is ported. Most of `nodes.py` reads
//! state that lives inside the Python process and is not reachable from
//! here — see `tests/differential/in_process_state.md` for the full map.
//! In short:
//!
//! * `GET /api/nodes` adds `latest_node_version` / `update_available`
//!   from `release_cache`, an in-process TTL cache of the newest GitHub
//!   release tag. Rust would fall back to the env default and report a
//!   different version than the Python — and would do so *only in
//!   production*, because both caches are cold in a test environment.
//!   That is precisely the kind of divergence a differential cannot see.
//! * `GET /api/nodes/plan` reads the in-process viewer-second
//!   accumulator and the plan-resolution cache.
//! * `/ws-status`, `DELETE /{node_id}` and `/self/decommission` touch the
//!   WebSocket connection manager and the HLS segment cache.
//!
//! This route touches none of it: one row, one `to_dict()`.

use axum::extract::{ConnectInfo, Path, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::NaiveDateTime;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::audit::{python_json, write_audit};
use crate::auth::{AuthUser, RequireAdmin};
use crate::error::ApiError;
use crate::plans;
use crate::query::path_segment;
use crate::ratelimit::PerHour;
use crate::models::{iso_naive, now_naive};

/// A node is offline after three missed heartbeats.
const HEARTBEAT_GRACE_SECONDS: i64 = 90;

/// `CameraNode.effective_status` as a function of its two columns.
pub fn node_effective_status(status: Option<&str>, last_seen: Option<NaiveDateTime>) -> Option<String> {
    let status = status.map(str::to_string);
    let Some(last_seen) = last_seen else {
        return Some(status.unwrap_or_else(|| "offline".to_string()));
    };
    if matches!(status.as_deref(), Some("offline") | Some("pending")) {
        return Some(status.unwrap_or_else(|| "offline".to_string()));
    }
    let age = now_naive().signed_duration_since(last_seen);
    if age.num_seconds() > HEARTBEAT_GRACE_SECONDS {
        return Some("offline".to_string());
    }
    status
}

#[derive(Debug, sqlx::FromRow)]
pub struct CameraNodeRow {
    pub node_id: String,
    pub name: String,
    pub hostname: Option<String>,
    pub local_ip: Option<String>,
    pub http_port: Option<i32>,
    pub status: Option<String>,
    pub last_seen: Option<NaiveDateTime>,
    pub key_rotated_at: Option<NaiveDateTime>,
    pub camera_count: i64,
    pub created_at: Option<NaiveDateTime>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub last_register_error: Option<String>,
    pub last_register_error_at: Option<NaiveDateTime>,
    pub node_version: Option<String>,
    pub version_checked_at: Option<NaiveDateTime>,
    pub storage_used_bytes: Option<i64>,
    pub storage_max_bytes: Option<i64>,
    pub storage_disk_free_bytes: Option<i64>,
    pub storage_disk_total_bytes: Option<i64>,
    pub storage_reported_at: Option<NaiveDateTime>,
}

impl CameraNodeRow {
    /// Real-time status. Differs from the camera equivalent in one way
    /// worth noticing: `pending` is sticky, because a node that has
    /// registered but never heartbeated should read as pending rather
    /// than as something that went offline.
    pub fn effective_status(&self) -> Option<String> {
        node_effective_status(self.status.as_deref(), self.last_seen)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "node_id": self.node_id,
            "name": self.name,
            "hostname": self.hostname,
            "local_ip": self.local_ip,
            "http_port": self.http_port,
            "status": self.effective_status(),
            "last_seen": self.last_seen.map(iso_naive),
            "key_rotated_at": self.key_rotated_at.map(iso_naive),
            "camera_count": self.camera_count,
            "created_at": self.created_at.map(iso_naive),
            "video_codec": self.video_codec,
            "audio_codec": self.audio_codec,
            "last_register_error": self.last_register_error,
            "last_register_error_at": self.last_register_error_at.map(iso_naive),
            "node_version": self.node_version,
            "version_checked_at": self.version_checked_at.map(iso_naive),
            // The whole storage block is null until a node has reported
            // once — Python gates the dict on `storage_reported_at`, so
            // an absent report is one null rather than five.
            "storage": match self.storage_reported_at {
                None => Value::Null,
                Some(reported_at) => json!({
                    "used_bytes": self.storage_used_bytes,
                    "max_bytes": self.storage_max_bytes,
                    "disk_free_bytes": self.storage_disk_free_bytes,
                    "disk_total_bytes": self.storage_disk_total_bytes,
                    "reported_at": iso_naive(reported_at),
                }),
            },
        })
    }
}

pub const NODE_SELECT: &str = r#"
    SELECT n.node_id, n.name, n.hostname, n.local_ip, n.http_port, n.status,
           n.last_seen, n.key_rotated_at,
           (SELECT COUNT(*) FROM cameras c WHERE c.node_id = n.id) AS camera_count,
           n.created_at, n.video_codec, n.audio_codec,
           n.last_register_error, n.last_register_error_at,
           n.node_version, n.version_checked_at,
           n.storage_used_bytes, n.storage_max_bytes, n.storage_disk_free_bytes,
           n.storage_disk_total_bytes, n.storage_reported_at
      FROM camera_nodes n
"#;

/// `GET /api/nodes` — every node, each decorated with what its build
/// compares to.
///
/// The decoration is why this could not move earlier: `check_node_version`
/// reads the release cache, and a port without one would answer from
/// the environment fallback while Python answered from a fetched tag.
/// Both are cold in a test environment, so the differential would pass
/// and production would not — see `in_process_state.md`. The cache is
/// Rust's now, refreshed by the same background loop on the same
/// cadence.
pub async fn list_nodes(
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
) -> Result<Json<Vec<Value>>, ApiError> {
    let rows: Vec<CameraNodeRow> =
        sqlx::query_as(&format!("{NODE_SELECT} WHERE n.org_id = $1"))
            .bind(&user.org_id)
            .fetch_all(&state.pool)
            .await?;

    let latest = crate::versions::latest_node_version(&state.config.latest_node_version);
    Ok(Json(
        rows.iter()
            .map(|row| {
                let mut out = row.to_json();
                let check = crate::versions::check_node_version(
                    row.node_version.as_deref(),
                    &state.config.min_supported_node_version,
                    &latest,
                );
                // Four keys mixed into the row, in the order the Python
                // assigns them.
                out["update_available"] = check["update_available"].clone();
                out["latest_node_version"] = check["latest"].clone();
                out["min_supported_node_version"] = check["min_supported"].clone();
                out["version_supported"] = check["supported"].clone();
                out
            })
            .collect(),
    ))
}

/// `GET /api/nodes/{node_id}`.
pub async fn get_node(
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    Path(node_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let node_id = path_segment(&node_id)?;
    let row: Option<CameraNodeRow> =
        sqlx::query_as(&format!("{NODE_SELECT} WHERE n.node_id = $1 AND n.org_id = $2"))
            .bind(node_id)
            .bind(&user.org_id)
            .fetch_optional(&state.pool)
            .await?;

    let row = row.ok_or_else(|| ApiError::not_found("Node not found"))?;
    Ok(Json(row.to_json()))
}


/// `GET /api/nodes/plan` — what the dashboard's plan panel reads.
///
/// Two halves that come from different places. The caps and the plan
/// name come from the *token's* claim, not from a database lookup:
/// this is a display route, and `user.plan` is what the rest of the
/// session is being served under. The usage half is live — node and
/// camera counts from the database, and viewer-hours from the in-memory
/// counter the segment route maintains.
///
/// That counter is why this route had to move when the video path did.
/// It lives in whichever process serves segments, and Python's copy has
/// been empty since that became Rust: this route reading it there would
/// have shown every org zero hours used, which looks exactly like a
/// reset rather than a port.
pub async fn get_plan_info(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    let limits = plans::get_plan_limits(&user.plan);

    let (nodes,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM camera_nodes WHERE org_id = $1")
        .bind(&user.org_id)
        .fetch_one(&state.pool)
        .await?;
    let (cameras,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM cameras WHERE org_id = $1")
        .bind(&user.org_id)
        .fetch_one(&state.pool)
        .await?;

    let past_due = crate::settings::get(&state.pool, &user.org_id, "payment_past_due", Some("false"))
        .await?
        .as_deref()
        == Some("true");
    let cancel_pending =
        crate::settings::get(&state.pool, &user.org_id, "plan_cancel_pending", Some("false"))
            .await?
            .as_deref()
            == Some("true");

    // The grace countdown, so the banner can say how long is left
    // rather than repeating the static seven days the terms promise.
    let mut grace_days_remaining = Value::Null;
    let mut grace_expires_at = Value::Null;
    if past_due {
        let raw = crate::settings::get(&state.pool, &user.org_id, "payment_past_due_at", Some(""))
            .await?
            .unwrap_or_default();
        if !raw.is_empty() {
            // Same `fromisoformat` the rest of the service uses, and an
            // unparseable value leaves both fields null — the Python
            // catches ValueError and TypeError here and keeps the
            // nominal plan rather than guessing.
            if let Ok(parsed) = crate::pydatetime::fromisoformat(&raw.replace('Z', "+00:00")) {
                // A naive value is read as UTC, which is what
                // `.replace(tzinfo=UTC)` does to it.
                let offset_us = parsed.offset_us.unwrap_or(0);
                let expires = parsed.naive + chrono::Duration::days(plans::PAYMENT_GRACE_DAYS)
                    - chrono::Duration::microseconds(offset_us);
                let remaining = expires - now_naive();
                // `timedelta.days` floors, so a remainder of minus one
                // hour is minus one day, and `max(0, ...)` shows it as
                // suspended rather than as a negative countdown.
                grace_days_remaining = json!(remaining.num_days().max(0));
                grace_expires_at = json!(iso_aware(expires, offset_us));
            }
        }
    }

    let viewer_seconds = state.hls.warm_viewer_seconds(&state.pool, &user.org_id).await;
    Ok(Json(json!({
        "plan": user.plan,
        "plan_name": plans::get_plan_display_name(&user.plan),
        "features": user.features,
        "limits": limits.to_json(),
        "usage": {
            "nodes": nodes,
            "cameras": cameras,
            "viewer_hours_used": crate::pyrepr::round_half_even(viewer_seconds as f64 / 3600.0),
            "viewer_hours_limit": limits.max_viewer_hours_per_month,
        },
        "payment_past_due": past_due,
        "grace_days_remaining": grace_days_remaining,
        "grace_expires_at": grace_expires_at,
        "grace_window_days": plans::PAYMENT_GRACE_DAYS,
        "plan_cancel_pending": cancel_pending,
    })))
}

/// `datetime.isoformat()` for the aware value the grace maths produced.
///
/// The offset is carried through from whatever `payment_past_due_at`
/// was stored with, because that is the value Python added the grace
/// window to and then printed.
pub(crate) fn iso_aware(naive_utc: NaiveDateTime, offset_us: i64) -> String {
    let local = naive_utc + chrono::Duration::microseconds(offset_us);
    let total_minutes = offset_us / 60_000_000;
    let sign = if total_minutes < 0 { '-' } else { '+' };
    let (hours, minutes) = (total_minutes.abs() / 60, total_minutes.abs() % 60);
    format!("{}{sign}{hours:02}:{minutes:02}", iso_naive(local))
}

/// `POST /api/nodes/self/decommission` — the node asking to be removed.
///
/// Run from the CameraNode's own `/wipe confirm`, so a factory reset is
/// one action rather than two. Unlike the admin delete it sends no
/// `wipe_data` command back: the node is the one asking, and is already
/// committed to wiping itself whether or not this answer arrives.
///
/// It authenticates by key rather than by naming itself in the URL — a
/// stolen key could already heartbeat as that node, so there is no new
/// exposure, and it keeps working for a node that has forgotten its own
/// id part-way through a reset.
pub async fn decommission_self(
    rate: PerHour<10>,
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    // The key is read inside the function in Python, so the limiter has
    // already counted the request.
    rate.check().await?;
    let Some(key) = headers.get("x-node-api-key") else {
        return Err(ApiError::unauthorized("API key required"));
    };

    let node: Option<(i32, String, String, String)> = sqlx::query_as(
        "SELECT id, node_id, name, org_id FROM camera_nodes WHERE api_key_hash = $1",
    )
    .bind(crate::api::node_writes::node_key_hash(key.as_bytes()))
    .fetch_optional(&state.pool)
    .await?;
    let Some((node_pk, node_id, node_name, org_id)) = node else {
        return Err(ApiError::not_found("Node not found"));
    };

    // The same cache cleanup the admin delete does, so a camera's
    // segments do not outlive the node that was pushing them.
    let cameras: Vec<(String,)> = sqlx::query_as("SELECT camera_id FROM cameras WHERE node_id = $1")
        .bind(node_pk)
        .fetch_all(&state.pool)
        .await?;
    for (camera_id,) in &cameras {
        state.hls.cleanup_camera(camera_id);
    }

    // SQLAlchemy cascades the cameras through the relationship; the
    // foreign key itself is NO ACTION, so the rows go first here.
    sqlx::query("DELETE FROM cameras WHERE node_id = $1")
        .bind(node_pk)
        .execute(&state.pool)
        .await?;
    sqlx::query("DELETE FROM camera_nodes WHERE id = $1")
        .bind(node_pk)
        .execute(&state.pool)
        .await?;

    // Nobody is acting but the node itself, which is why the row names
    // it as the user and says who initiated it: a node disappearing is
    // security-relevant however it happened.
    write_audit(
        &state.pool,
        &org_id,
        "node_decommissioned",
        "",
        &format!("node:{node_id}"),
        Some(python_json(&[
            ("node_id", json!(node_id)),
            ("name", json!(node_name)),
            ("initiated_by", json!("node")),
        ])),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    Ok(Json(json!({ "success": true, "deleted": node_id })))
}

/// `GET /api/nodes/ws-status` — which of this org's nodes hold a live
/// socket.
///
/// Filtered by org, and the filter runs the other way round from what
/// you might write: the registry is walked in *its* order and each id
/// checked against the org's, so the answer keeps the order the nodes
/// connected in rather than the order the database returns them.
///
/// The registry is per process. On a fleet this answers for the machine
/// that took the request, which is also the only machine that could
/// send any of those nodes a command.
pub async fn ws_status(
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
) -> Result<Json<Value>, ApiError> {
    let owned: Vec<(String,)> =
        sqlx::query_as("SELECT node_id FROM camera_nodes WHERE org_id = $1")
            .bind(&user.org_id)
            .fetch_all(&state.pool)
            .await?;
    let owned: std::collections::HashSet<String> =
        owned.into_iter().map(|(node_id,)| node_id).collect();

    let connected: Vec<String> = crate::ws::MANAGER
        .connected_nodes()
        .into_iter()
        .filter(|node_id| owned.contains(node_id))
        .collect();

    Ok(Json(json!({
        "connected_nodes": connected,
        "count": connected.len(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn node(status: Option<&str>, last_seen: Option<NaiveDateTime>) -> CameraNodeRow {
        CameraNodeRow {
            node_id: "n1".into(),
            name: "Pi".into(),
            hostname: None,
            local_ip: None,
            http_port: None,
            status: status.map(str::to_string),
            last_seen,
            key_rotated_at: None,
            camera_count: 0,
            created_at: None,
            video_codec: None,
            audio_codec: None,
            last_register_error: None,
            last_register_error_at: None,
            node_version: None,
            version_checked_at: None,
            storage_used_bytes: None,
            storage_max_bytes: None,
            storage_disk_free_bytes: None,
            storage_disk_total_bytes: None,
            storage_reported_at: None,
        }
    }

    #[test]
    fn a_pending_node_stays_pending_even_with_a_fresh_heartbeat() {
        // Unlike a camera, `pending` is sticky here: a node that has
        // registered but not finished setup must not read as "online".
        let seen = now_naive() - Duration::seconds(5);
        assert_eq!(
            node(Some("pending"), Some(seen)).effective_status().as_deref(),
            Some("pending")
        );
    }

    #[test]
    fn a_node_never_seen_keeps_its_stored_status() {
        // Python returns `self.status or "offline"` on this branch, so a
        // pending node with no heartbeat is still pending.
        assert_eq!(
            node(Some("pending"), None).effective_status().as_deref(),
            Some("pending")
        );
        assert_eq!(node(None, None).effective_status().as_deref(), Some("offline"));
    }

    #[test]
    fn three_missed_heartbeats_mean_offline() {
        let seen = now_naive() - Duration::seconds(91);
        assert_eq!(
            node(Some("online"), Some(seen)).effective_status().as_deref(),
            Some("offline")
        );
    }

    #[test]
    fn the_grace_boundary_is_exclusive() {
        let seen = now_naive() - Duration::seconds(90);
        assert_eq!(
            node(Some("online"), Some(seen)).effective_status().as_deref(),
            Some("online")
        );
    }

    #[test]
    fn storage_is_one_null_until_a_node_has_reported() {
        let mut n = node(Some("online"), None);
        n.storage_used_bytes = Some(123);
        assert!(
            n.to_json()["storage"].is_null(),
            "without storage_reported_at the whole block is null, not five nulls"
        );

        n.storage_reported_at = Some(now_naive());
        assert_eq!(n.to_json()["storage"]["used_bytes"], 123);
    }
}
