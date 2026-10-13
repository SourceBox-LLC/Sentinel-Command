//! Pulling a live JPEG off a camera, through the node socket.
//!
//! Ported from `_extract_snapshot_image_b64` and
//! `_capture_snapshot_bytes` in `backend/app/mcp/server.py`. They live
//! in the MCP module because the tools use them, but the Home
//! Assistant integration's still-image route calls them too — one node
//! round trip, one set of error messages.
//!
//! The error messages ARE the feature here. An earlier version checked
//! only for `image_b64` and, when it was missing, blamed the
//! CameraNode's version — even when the real cause was a dead FFmpeg
//! worker, a full disk or an unplugged camera. Every `status: "error"`
//! is now surfaced, and the two common causes get a hint that names
//! what to go and look at.

use serde_json::Value;

/// What went wrong, in the words the caller should see.
///
/// The MCP side raises these as `ToolError`; the integration route
/// turns them into a 503 so Home Assistant renders "unavailable"
/// rather than an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotError(pub String);

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `_extract_snapshot_image_b64` — the base64 JPEG, or why not.
pub fn extract_image_b64(result: &Value, camera_id: &str) -> Result<String, SnapshotError> {
    if result.get("status").and_then(Value::as_str) == Some("error") {
        // `(result.get("error") or "").strip()` — a null or absent
        // error is the empty string, not the word "None".
        let err = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let lower = err.to_lowercase();
        // The two patterns CameraNode actually sends when its pipeline
        // is dead; see `cmd_take_snapshot` in its websocket.rs.
        if lower.contains("no segments") || lower.contains("error opening input") {
            return Err(SnapshotError(format!(
                "Camera '{camera_id}' has no active video stream. The CameraNode \
                 is online but isn't producing HLS segments right now — common \
                 causes are a dead FFmpeg worker, a full disk on the node, or \
                 the camera being unplugged. Check the CameraNode dashboard for \
                 the underlying error, then restart the node if needed."
            )));
        }
        if lower.contains("ffmpeg") {
            let detail = if err.is_empty() {
                "no detail provided"
            } else {
                &err
            };
            return Err(SnapshotError(format!(
                "Camera '{camera_id}' snapshot failed inside FFmpeg on the \
                 CameraNode: {detail}. The video pipeline may need to be \
                 restarted on the node."
            )));
        }
        let detail = if err.is_empty() {
            "unspecified failure"
        } else {
            &err
        };
        return Err(SnapshotError(format!(
            "Snapshot failed on the CameraNode: {detail}"
        )));
    }

    // `result.get("data", {}).get("image_b64") or result.get("image_b64")`
    // — the envelope first, then the flat shape an older node sends.
    // An empty string is falsy and falls through to the flat lookup,
    // then to the version hint.
    let image_b64 = result
        .get("data")
        .and_then(Value::as_object)
        .and_then(|data| data.get("image_b64"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            result
                .get("image_b64")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        });
    match image_b64 {
        Some(value) => Ok(value.to_string()),
        // Truly unknown shape — most likely a node predating both the
        // envelope and the field.
        None => Err(SnapshotError(
            "Camera node did not return image data — update CameraNode to latest version"
                .to_string(),
        )),
    }
}

/// `_capture_snapshot_bytes` — `(jpeg, node_id)`, or why not.
pub async fn capture_bytes(
    pool: &crate::db::Pool,
    org_id: &str,
    camera_id: &str,
) -> Result<(Vec<u8>, String), SnapshotError> {
    capture(pool, org_id, camera_id, None)
        .await
        .map(|c| (c.jpeg, c.node_id))
}

/// A frame from a camera, and when it was recorded.
pub struct Capture {
    pub jpeg: Vec<u8>,
    pub node_id: String,
    /// Unix ms, from CameraNode 0.1.84 on; `None` from older nodes.
    pub frame_time_ms: Option<i64>,
}

/// The node's `frame_time_ms`, in the same two places `image_b64` may be.
pub fn frame_time_ms(result: &Value) -> Option<i64> {
    result
        .get("data")
        .and_then(|data| data.get("frame_time_ms"))
        .or_else(|| result.get("frame_time_ms"))
        .and_then(Value::as_i64)
}

/// The node's own error, when its reply is one. `send_command` returns
/// error replies as results, leaving `status` for the caller to read.
pub fn node_error(result: &Value) -> Option<String> {
    (result.get("status").and_then(Value::as_str) == Some("error")).then(|| {
        result
            .get("error")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .unwrap_or("the camera node reported an error")
            .to_string()
    })
}

/// The recorded time of a frame asked for with `at`, or why there is none.
/// A node error comes first: a reply without `frame_time_ms` is only "too
/// old to look back" when it is a success. On the rig, a moment from
/// before the camera's first segment came back as the node's "no video
/// retained" error, and was misreported as an old node, which told the
/// agent to stop looking back instead of picking a later moment.
pub fn frame_time_for(result: &Value, at_requested: bool) -> Result<Option<i64>, String> {
    if let Some(err) = node_error(result) {
        return Err(err);
    }
    let recorded = frame_time_ms(result);
    if at_requested && recorded.is_none() {
        return Err(NO_REWIND.to_string());
    }
    Ok(recorded)
}

/// What a node too old to look back answers with: a current frame, which
/// must not be passed off as the moment that was asked for.
pub const NO_REWIND: &str = "This camera's node can't look back in time (it needs \
    CameraNode 0.1.84 or later), so there is no frame from that moment. \
    Omit `at` to see the camera now.";

/// A frame from the camera, now or (`at_ms`, Unix ms) from the video the
/// node still holds: about the last three minutes (one before CameraNode 0.1.86).
pub async fn capture(
    pool: &crate::db::Pool,
    org_id: &str,
    camera_id: &str,
    at_ms: Option<i64>,
) -> Result<Capture, SnapshotError> {
    let row: Option<(Option<i32>,)> =
        sqlx::query_as("SELECT node_id FROM cameras WHERE org_id = $1 AND camera_id = $2 LIMIT 1")
            .bind(org_id)
            .bind(camera_id)
            .fetch_optional(pool)
            .await
            .map_err(|err| SnapshotError(err.to_string()))?;
    let Some((node_ref,)) = row else {
        return Err(SnapshotError(format!("Camera '{camera_id}' not found")));
    };
    // `db.query(CameraNode).filter_by(id=cam.node_id).first()` — a
    // camera whose node_id is NULL matches nothing, which is the same
    // "no assigned node" answer.
    let node: Option<(String,)> = match node_ref {
        Some(id) => sqlx::query_as("SELECT node_id FROM camera_nodes WHERE id = $1 LIMIT 1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .map_err(|err| SnapshotError(err.to_string()))?,
        None => None,
    };
    let Some((node_id,)) = node else {
        return Err(SnapshotError(format!(
            "Camera '{camera_id}' has no assigned node"
        )));
    };

    if !crate::ws::MANAGER.is_connected(&node_id) {
        return Err(SnapshotError(format!(
            "Node '{node_id}' is offline — cannot capture snapshot"
        )));
    }

    let mut payload = serde_json::json!({ "camera_id": camera_id });
    if let Some(at) = at_ms {
        payload["at_ms"] = serde_json::json!(at);
    }
    let result = crate::ws::MANAGER
        .send_command(
            &node_id,
            "take_snapshot",
            payload,
            std::time::Duration::from_secs(15),
        )
        .await;
    let result = match result {
        Ok(value) => value,
        Err(crate::ws::CommandError::Timeout { .. }) => {
            return Err(SnapshotError(
                "Snapshot timed out — camera node did not respond in time".to_string(),
            ))
        }
        // `except ValueError as e: raise ToolError(str(e))` — the
        // manager's own message, passed through.
        Err(other) => return Err(SnapshotError(other.to_string())),
    };

    // The image first: it turns a node error into the message a user
    // reads, which the rewind check would otherwise have replaced.
    let image_b64 = extract_image_b64(&result, camera_id)?;
    let frame_time = frame_time_for(&result, at_ms.is_some()).map_err(SnapshotError)?;
    // The decoder is CPython's lenient one, and its error text is
    // interpolated into the message a user reads — so "corrupt
    // snapshot data (…)" has to say what Python would have said,
    // down to the character count.
    match crate::crypto::python_b64decode(&image_b64) {
        Ok(jpeg) => Ok(Capture {
            jpeg,
            node_id,
            frame_time_ms: frame_time,
        }),
        Err(reason) => Err(SnapshotError(format!(
            "Camera '{camera_id}' returned corrupt snapshot data ({reason}). \
             Update the CameraNode and retry."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_error_is_not_mistaken_for_an_old_node() {
        let error = json!({"status": "error", "error": "no video retained from that moment"});
        assert_eq!(
            frame_time_for(&error, true),
            Err("no video retained from that moment".to_string())
        );
        let old_node = json!({"status": "success", "data": {"image_b64": "aGk="}});
        assert_eq!(frame_time_for(&old_node, true), Err(NO_REWIND.to_string()));
        assert_eq!(frame_time_for(&old_node, false), Ok(None));
        let new_node =
            json!({"status": "success", "data": {"image_b64": "aGk=", "frame_time_ms": 7}});
        assert_eq!(frame_time_for(&new_node, true), Ok(Some(7)));
    }
    use serde_json::json;

    #[test]
    fn the_envelope_is_preferred_over_the_flat_shape() {
        let result = json!({"status": "success", "data": {"image_b64": "aGk="}});
        assert_eq!(extract_image_b64(&result, "cam").unwrap(), "aGk=");
        // An older node sends it flat.
        let flat = json!({"image_b64": "aGk="});
        assert_eq!(extract_image_b64(&flat, "cam").unwrap(), "aGk=");
        // An empty envelope value falls through to the flat one, which
        // is what `or` does.
        let both = json!({"data": {"image_b64": ""}, "image_b64": "aGk="});
        assert_eq!(extract_image_b64(&both, "cam").unwrap(), "aGk=");
    }

    /// The bug this helper exists to fix: a dead pipeline used to be
    /// reported as an out-of-date CameraNode.
    #[test]
    fn a_dead_pipeline_says_so_instead_of_blaming_the_version() {
        for error in ["no segments available", "Error opening input file"] {
            let result = json!({"status": "error", "error": error});
            let message = extract_image_b64(&result, "cam-1").unwrap_err().0;
            assert!(message.contains("no active video stream"), "{message}");
            assert!(message.contains("cam-1"));
            assert!(!message.contains("update CameraNode"), "{message}");
        }
    }

    #[test]
    fn an_ffmpeg_failure_passes_the_detail_through() {
        let result = json!({"status": "error", "error": "ffmpeg exited with 1"});
        let message = extract_image_b64(&result, "cam-1").unwrap_err().0;
        assert!(message.contains("inside FFmpeg"), "{message}");
        assert!(message.contains("ffmpeg exited with 1"), "{message}");
    }

    /// Anything else is surfaced verbatim rather than guessed at.
    #[test]
    fn an_unrecognised_error_is_passed_through() {
        let result = json!({"status": "error", "error": "disk full"});
        assert_eq!(
            extract_image_b64(&result, "cam").unwrap_err().0,
            "Snapshot failed on the CameraNode: disk full"
        );
    }

    /// `(result.get("error") or "").strip()` — absent, null and blank
    /// all become the placeholder, not the word "None".
    #[test]
    fn a_missing_error_message_gets_a_placeholder() {
        for result in [
            json!({"status": "error"}),
            json!({"status": "error", "error": null}),
            json!({"status": "error", "error": "   "}),
        ] {
            assert_eq!(
                extract_image_b64(&result, "cam").unwrap_err().0,
                "Snapshot failed on the CameraNode: unspecified failure"
            );
        }
        // And inside the FFmpeg branch, which has its own placeholder.
        let result = json!({"status": "error", "error": "FFMPEG"});
        assert!(extract_image_b64(&result, "cam")
            .unwrap_err()
            .0
            .contains("FFMPEG"));
    }

    #[test]
    fn no_image_at_all_blames_the_version() {
        for result in [
            json!({"status": "success"}),
            json!({"status": "success", "data": {}}),
            json!({}),
            json!({"data": {"image_b64": ""}}),
        ] {
            assert_eq!(
                extract_image_b64(&result, "cam").unwrap_err().0,
                "Camera node did not return image data — update CameraNode to latest version"
            );
        }
    }
}
