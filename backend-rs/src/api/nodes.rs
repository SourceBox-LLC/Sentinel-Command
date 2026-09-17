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

use axum::extract::{Path, State};
use axum::Json;
use chrono::NaiveDateTime;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::query::path_segment;
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
