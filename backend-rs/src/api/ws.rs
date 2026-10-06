//! What arrives on a CameraNode's channel.
//!
//! Ported from `backend/app/api/ws.py`. The connection registry and the
//! two throttles are in `crate::ws`; this is the route, the heartbeat
//! and the motion handler.
//!
//! **The motion handler has two callers, not one.** A node reports
//! motion over HTTP (`POST /api/cameras/{id}/motion`, in `api/hls.rs`)
//! and, on the WebSocket wire format, as an `event` frame. The HTTP
//! path is the one CameraNode actually uses; the frame is kept because
//! the Python still handles it, whatever the release notes say. Both
//! land here so the row, the two broadcasts and the notification cannot
//! differ by which way the event arrived.

use serde_json::{json, Value};

use axum::extract::State;

use crate::app::AppState;
use crate::models::now_naive;
use crate::pyint::{python_int_of_json, PyInt};

/// `_handle_motion_event` — reached only from `POST
/// /api/cameras/{id}/motion` (`api/hls.rs`), after its rate limit and
/// the org's ingestion switch.
///
/// Every failure is a silent return: a motion event is a report about
/// something that already happened, and a node that sends a malformed
/// one must not be answered with an error it will retry.
pub async fn handle_motion_event(state: &AppState, node_id: &str, org_id: &str, payload: &Value) {
    // `if not camera_id or score is None` — the empty string is as
    // absent as a missing key.
    let camera_id = payload
        .get("camera_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let score = payload.get("score").filter(|value| !value.is_null());
    let (Some(score), false) = (score, camera_id.is_empty()) else {
        tracing::warn!(node_id, "Motion event missing camera_id or score");
        return;
    };

    // `max(0, min(100, int(score)))`, where anything `int()` refuses is
    // a dropped event rather than an error.
    let Some(score) = python_int_of_json(score) else {
        tracing::warn!(node_id, %score, "Motion event has non-numeric score");
        return;
    };
    let score = match score {
        PyInt::Small(value) => value.clamp(0, 100),
        // `min(100, <huge>)` is 100 and `max(0, <huge negative>)` is 0.
        PyInt::Big { negative: false } => 100,
        PyInt::Big { negative: true } => 0,
    } as i32;

    // `datetime.fromisoformat(event_ts).replace(tzinfo=None)` — which
    // *drops* an offset rather than converting by it, so
    // `10:00:00+05:00` is stored as 10:00:00 and not as 05:00:00. That
    // is almost certainly not what anyone intended, and it is what the
    // Python does.
    let timestamp = payload
        .get("timestamp")
        .and_then(Value::as_str)
        .filter(|raw| !raw.is_empty())
        .and_then(|raw| crate::pydatetime::fromisoformat(raw).ok())
        .map_or_else(now_naive, |parsed| parsed.naive);

    // `int(segment_seq) if segment_seq is not None else None`, where
    // anything `int()` refuses becomes None and the event goes on.
    //
    // A value `int()` *accepts* but the column cannot hold is a
    // different thing entirely: Python binds it, the commit fails, and
    // the whole event is lost — row, broadcast and notification
    // together. Dropping the field and carrying on would record an
    // event Python never recorded, which is what this did at first.
    let raw_seq = payload.get("segment_seq").filter(|value| !value.is_null());
    let mut segment_seq = None;
    if let Some(parsed) = raw_seq.and_then(python_int_of_json) {
        let narrowed = match parsed {
            PyInt::Small(value) => i32::try_from(value).ok(),
            PyInt::Big { .. } => None,
        };
        let Some(narrowed) = narrowed else {
            tracing::error!(node_id, "Failed to save motion event");
            return;
        };
        segment_seq = Some(narrowed);
    }

    // The camera named in the payload has to belong to the
    // authenticated node in the authenticated org. Without this a
    // compromised node could fill its own org's inbox with events
    // about cameras it does not own. Cross-tenant is already blocked,
    // because the org comes from the session and not the payload.
    let owned: Result<Option<(String,)>, _> = sqlx::query_as(
        "SELECT c.name FROM cameras c
           JOIN camera_nodes n ON n.id = c.node_id
          WHERE c.camera_id = $1 AND c.org_id = $2
            AND n.node_id = $3 AND n.org_id = $2",
    )
    .bind(camera_id)
    .bind(org_id)
    .bind(node_id)
    .fetch_optional(&state.pool)
    .await;
    let name = match owned {
        Ok(Some((name,))) => name,
        Ok(None) => {
            tracing::warn!(
                node_id,
                camera_id,
                org_id,
                "Motion event rejected: camera not owned by node"
            );
            return;
        }
        Err(err) => {
            tracing::error!(error = %err, "Failed to save motion event");
            return;
        }
    };

    let inserted = sqlx::query(
        "INSERT INTO motion_events (org_id, camera_id, node_id, score, segment_seq, timestamp)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(org_id)
    .bind(camera_id)
    .bind(node_id)
    .bind(score)
    .bind(segment_seq)
    .bind(timestamp)
    .execute(&state.pool)
    .await;
    if let Err(err) = inserted {
        tracing::error!(error = %err, "Failed to save motion event");
        return;
    }
    tracing::info!(camera_id, score, node_id, "Motion event");

    // Both feeds, so a dashboard toast and a Home Assistant
    // `binary_sensor` see the same event — through separate subscriber
    // pools, so neither consumes the other's budget.
    let frame = crate::api::motion::motion_frame(
        camera_id,
        node_id,
        score,
        &crate::models::iso_naive(timestamp),
    );
    crate::api::motion::BROADCASTER.notify(org_id, "all", &frame);
    crate::api::motion::INTEGRATION_BROADCASTER.notify(org_id, "all", &frame);

    // The durable half: the bell panel's motion history. The camera's
    // name makes a friendlier title, falling back to its id.
    let display = if name.is_empty() { camera_id } else { &name };
    crate::notifications::create_notification(
        state,
        org_id,
        crate::notifications::NewNotification::new("motion", format!("Motion on {display}"))
            .body(format!("Scene change detected at {score}% intensity."))
            .severity("info")
            .audience("all")
            .link(format!("/dashboard?camera={camera_id}"))
            .camera(camera_id)
            .node(node_id)
            .meta(json!({
                "score": score,
                "segment_seq": segment_seq,
                "event_timestamp": crate::models::iso_naive(timestamp),
            })),
    )
    .await;
}

/// What a heartbeat tells the node back, mixed into its ack.
pub struct HeartbeatAck {
    pub update_available: Value,
    pub unsupported: bool,
}

/// A status change worth telling the org about, held until the write
/// has committed.
struct Transition {
    kind: &'static str,
    entity_id: String,
    display_name: String,
    new_status: String,
    node_id: Option<String>,
}

/// `_handle_heartbeat`.
///
/// The same work the HTTP heartbeat does, over the socket. Transitions
/// are collected during the write and emitted *after* it commits, so
/// the inbox never shows a change that was then rolled back.
///
/// Unlike the HTTP path this never drops the connection for a version
/// below the floor — disconnecting cascades into reconnect storms, and
/// the dashboard already flags the build while the next register call
/// answers 426.
pub async fn handle_heartbeat(
    state: &AppState,
    node_id: &str,
    node_pk: i32,
    org_id: &str,
    payload: &Value,
) -> HeartbeatAck {
    let mut ack = HeartbeatAck {
        update_available: Value::Null,
        unsupported: false,
    };
    let mut transitions: Vec<Transition> = Vec::new();

    let node: Option<(String, Option<String>)> =
        match sqlx::query_as("SELECT status, name FROM camera_nodes WHERE node_id = $1")
            .bind(node_id)
            .fetch_optional(&state.pool)
            .await
        {
            Ok(row) => row,
            Err(err) => {
                tracing::error!(error = %err, node_id, "Heartbeat DB error");
                return ack;
            }
        };
    let Some((previous_status, name)) = node else {
        return ack;
    };

    let reported = payload.get("node_version").and_then(Value::as_str);
    let latest = crate::versions::latest_node_version(&state.config.latest_node_version);
    let check = crate::versions::check_node_version(
        reported,
        &state.config.min_supported_node_version,
        &latest,
    );
    ack.update_available = check["update_available"].clone();
    ack.unsupported = check["supported"] == Value::Bool(false);
    // `node.node_version = parsed if reported else None` — a node that
    // reports nothing clears the column rather than keeping a stale
    // reading from a previous build.
    let stored_version = reported.map(|_| check["parsed"].as_str().unwrap_or("").to_string());

    let now = now_naive();
    // `local_ip` doubles as the "LAN-reachable HLS" signal Home
    // Assistant reads, and a loopback-bound node has to be able to
    // clear it. Without this gate the WS heartbeat kept re-populating
    // the address the HTTP heartbeat had just cleared, and the
    // advertised URL flapped between dead and alive. A missing key is
    // an old node: keep whatever is there.
    let local_ip = payload.get("local_ip").and_then(Value::as_str);
    let lan_streaming = payload.get("lan_streaming");
    let clear_ip = lan_streaming == Some(&Value::Bool(false));

    let updated = sqlx::query(
        // `updated_at` is stamped because the model declares
        // `onupdate=`: SQLAlchemy writes it on every UPDATE it emits,
        // and the data-sync tier reads it as a per-row high-water mark.
        // A heartbeat always changes `last_seen`, so Python always
        // emits an UPDATE here and always stamps it.
        "UPDATE camera_nodes
            SET status = 'online', last_seen = $1, node_version = $2,
                version_checked_at = $1, updated_at = $1,
                local_ip = CASE WHEN $3 THEN NULL
                                WHEN CAST($4 AS TEXT) IS NOT NULL THEN $4
                                ELSE local_ip END
          WHERE node_id = $5",
    )
    .bind(now)
    .bind(&stored_version)
    .bind(clear_ip)
    .bind(local_ip.filter(|ip| !ip.is_empty()))
    .bind(node_id)
    .execute(&state.pool)
    .await;
    if let Err(err) = updated {
        tracing::error!(error = %err, node_id, "Heartbeat DB error");
        return ack;
    }
    if previous_status != "online" {
        transitions.push(Transition {
            kind: "node",
            entity_id: node_id.to_string(),
            display_name: python_or(name.as_deref(), node_id),
            new_status: "online".to_string(),
            node_id: None,
        });
    }

    if let Some(cameras) = payload.get("cameras").and_then(Value::as_array) {
        // One read for the lot, as the Python's `in_(camera_ids)` is,
        // then a write each. Reading the previous status *before*
        // writing is the whole point: a transition is the difference
        // between the two, and asking the row after the update would
        // only ever say it had not changed.
        let reported: Vec<&str> = cameras
            .iter()
            .filter_map(|camera| camera.get("camera_id").and_then(Value::as_str))
            .collect();
        let known: Result<Vec<(String, String, Option<String>)>, _> = sqlx::query_as(&format!(
            "SELECT camera_id, status, name FROM cameras
              WHERE camera_id {} AND node_id = $2",
            crate::db::any(1)
        ))
        .bind(crate::db::list(&reported))
        .bind(node_pk)
        .fetch_all(&state.pool)
        .await;
        let known = match known {
            Ok(rows) => rows,
            Err(err) => {
                tracing::error!(error = %err, node_id, "Heartbeat DB error");
                return ack;
            }
        };

        for camera in cameras {
            let Some(camera_id) = camera.get("camera_id").and_then(Value::as_str) else {
                continue;
            };
            // Not this node's camera, or not a camera at all: the
            // Python's map lookup misses and the entry is skipped.
            let Some((_, previous_status, camera_name)) =
                known.iter().find(|(id, ..)| id == camera_id)
            else {
                continue;
            };

            // `cam_data.get("status", "online")` — absent means online.
            let new_status = camera
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("online");
            // A healthy state wipes the reason, so a stale error does
            // not linger once the supervisor recovers.
            let last_error = if matches!(new_status, "restarting" | "failed" | "error") {
                camera.get("last_error").and_then(Value::as_str)
            } else {
                None
            };

            let updated = sqlx::query(
                // Same as above: `last_seen` always moves, so the
                // Python always emits an UPDATE and always stamps
                // `updated_at`.
                "UPDATE cameras SET status = $1, last_seen = $2, last_error = $3,
                        updated_at = $2
                  WHERE camera_id = $4 AND node_id = $5",
            )
            .bind(new_status)
            .bind(now)
            .bind(last_error)
            .bind(camera_id)
            .bind(node_pk)
            .execute(&state.pool)
            .await;
            if let Err(err) = updated {
                tracing::error!(error = %err, node_id, "Heartbeat DB error");
                return ack;
            }

            // See `camera_transition`: `streaming` is online, and only a
            // return from an announced `offline` is announced.
            if let Some(direction) =
                crate::notifications::camera_transition(Some(previous_status), new_status)
            {
                transitions.push(Transition {
                    kind: "camera",
                    entity_id: camera_id.to_string(),
                    display_name: python_or(camera_name.as_deref(), camera_id),
                    new_status: direction.to_string(),
                    node_id: Some(node_id.to_string()),
                });
            }
        }
    }

    // Post-commit, and never able to fail the heartbeat.
    for transition in transitions {
        if transition.kind == "node" {
            crate::notifications::emit_node_transition(
                state,
                org_id,
                &transition.entity_id,
                &transition.display_name,
                &transition.new_status,
            )
            .await;
        } else {
            crate::notifications::emit_camera_transition(
                state,
                org_id,
                &transition.entity_id,
                &transition.display_name,
                &transition.new_status,
                transition.node_id.as_deref(),
            )
            .await;
        }
    }

    ack
}

/// `a or b` for a nullable display name.
fn python_or(name: Option<&str>, fallback: &str) -> String {
    match name.filter(|value| !value.is_empty()) {
        Some(value) => value.to_string(),
        None => fallback.to_string(),
    }
}

/// `WS /ws/node`.
///
/// **Credentials come from headers, and from the query string only for
/// nodes too old to know better.** A URL reaches far more log sinks
/// than a header does — the access log, the platform's own, whatever
/// ships them onward — and a custom client, which CameraNode is, can
/// set headers freely. The query path still authenticates, and says so
/// in the log each time, so it can be retired once the install base has
/// rolled forward.
pub async fn node_websocket(
    ws: axum::extract::WebSocketUpgrade,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    request_uri: axum::http::Uri,
) -> axum::response::Response {
    let query = crate::query::Query::parse(request_uri.query());
    let header_key = headers.get("x-node-api-key").and_then(|v| v.to_str().ok());
    let header_node = headers.get("x-node-id").and_then(|v| v.to_str().ok());

    // Headers win; the query string is the fallback. `auth_path` only
    // drives the deprecation log below.
    let from_header = header_key.is_some();
    let api_key = header_key
        .map(str::to_string)
        .or_else(|| query.last("api_key").map(str::to_string));
    let node_id = header_node
        .map(str::to_string)
        .or_else(|| query.last("node_id").map(str::to_string));

    let (Some(api_key), Some(node_id)) = (api_key, node_id) else {
        // 4001 is also what a wrong key gets, deliberately: a client
        // probing for "is auth required here" cannot tell a missing
        // credential from a wrong one.
        tracing::warn!("[WS] Connect rejected — missing api_key or node_id");
        return refuse_handshake(4001, "Missing api_key or node_id");
    };

    // Before authentication, so a flood costs no database work. 1013 is
    // the WebSocket spelling of 429.
    if !crate::ws::CONNECT_THROTTLE.allow(&node_id) {
        tracing::warn!(node_id, "[WS] Connect throttle hit — rejecting handshake");
        return refuse_handshake(1013, "Too many connection attempts");
    }

    let node: Option<(i32, String, String)> = match sqlx::query_as(
        "SELECT id, org_id, api_key_hash FROM camera_nodes WHERE node_id = $1",
    )
    .bind(&node_id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(row) => row,
        Err(err) => {
            tracing::error!(error = %err, node_id, "[WS] Auth lookup failed");
            return refuse_handshake(4001, "Invalid node_id or API key");
        }
    };
    let presented = crate::api::node_writes::node_key_hash(api_key.as_bytes());
    let Some((node_pk, org_id, _)) = node.filter(|(_, _, stored)| *stored == presented) else {
        tracing::warn!(node_id, "[WS] Auth failed");
        return refuse_handshake(4001, "Invalid node_id or API key");
    };

    // Logged after authentication, so an invalid-key probe on the old
    // path does not generate the signal — only a node that really is
    // still using it.
    if !from_header {
        tracing::warn!(
            node_id,
            "[WS] Node authenticated via deprecated query-string api_key — upgrade \
             CameraNode to v0.1.65+ to move the credential into request headers \
             (X-Node-API-Key / X-Node-Id). Query-string auth is still accepted but \
             logs the key in uvicorn / Fly access pipelines."
        );
    }

    ws.on_upgrade(move |socket| serve_node(socket, state, node_id, node_pk, org_id))
}

/// Refusing the handshake, the way Starlette refuses it.
///
/// `ws.close(code=…)` *before* `ws.accept()` does not complete the
/// upgrade and then close: there is no connection yet, so uvicorn
/// answers the HTTP request with a bare **403** and the close code is
/// never transmitted at all. Upgrading first and then sending a close
/// frame — which is what this did at first — is a different answer to
/// the same request, and the client sees 101 where it should see 403.
///
/// The codes the Python passes (4001 for a bad credential, 1013 for the
/// connect throttle) are therefore invisible to the client. They are
/// kept in the call sites because they are what the source says, and
/// because a future version that closes *after* accepting would send
/// them.
fn refuse_handshake(_code: u16, _reason: &'static str) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        axum::http::StatusCode::FORBIDDEN,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        "",
    )
        .into_response()
}

/// The receive loop for one authenticated socket.
async fn serve_node(
    socket: axum::extract::ws::WebSocket,
    state: AppState,
    node_id: String,
    node_pk: i32,
    org_id: String,
) {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};

    let registration = crate::ws::MANAGER.connect(&node_id);
    let connection_id = registration.id;
    let mut frames = registration.frames;
    let (mut sink, mut stream) = socket.split();

    loop {
        tokio::select! {
            // Anything the manager wants written — a command, or an
            // ack this loop queued for itself.
            outgoing = frames.recv() => {
                let Some(frame) = outgoing else { break };
                if sink.send(Message::Text(frame.into())).await.is_err() {
                    break;
                }
            }
            incoming = stream.next() => {
                let Some(Ok(message)) = incoming else { break };
                let Message::Text(text) = message else {
                    // Binary, ping and pong are not part of the wire
                    // format; axum answers pings itself.
                    continue;
                };
                let Ok(data) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                // A NUL in any string would reach PostgreSQL in the
                // heartbeat's UPDATE and fail it — the node's status
                // stops updating, for a reason nothing reports. Refused
                // here, with a reason, as the REST decoder does.
                if crate::query::nul_location(&data, &mut Vec::new()).is_some() {
                    let refusal = json!({
                        "type": "error",
                        "id": data.get("id").cloned().unwrap_or(Value::Null),
                        "payload": {"detail": "Message must not contain a NUL (\\u0000) character"},
                    });
                    let _ = sink.send(Message::Text(refusal.to_string().into())).await;
                    continue;
                }

                // Per-node message limit. An over-limit message is
                // answered with an error and the socket stays open:
                // dropping it would reset the node's status tracking
                // and cascade into a worse problem than a node that is
                // briefly too chatty.
                if !crate::ws::MESSAGE_LIMITER.allow(&node_id) {
                    tracing::warn!(node_id, "[WS] Rate limit exceeded — dropping message");
                    let refusal = json!({
                        "type": "error",
                        "id": data.get("id").cloned().unwrap_or(Value::Null),
                        "payload": {"detail": "Rate limit exceeded"},
                    });
                    let _ = sink.send(Message::Text(refusal.to_string().into())).await;
                    continue;
                }

                match data.get("type").and_then(Value::as_str) {
                    Some("heartbeat") => {
                        let payload = data.get("payload").cloned().unwrap_or(json!({}));
                        let ack = handle_heartbeat(&state, &node_id, node_pk, &org_id, &payload)
                            .await;
                        // The two version hints are omitted when there
                        // is nothing to say, so a node too old to parse
                        // them sees the payload it always saw.
                        let mut payload = serde_json::Map::new();
                        payload.insert(
                            "timestamp".to_string(),
                            json!(crate::models::iso_naive(now_naive())),
                        );
                        if !ack.update_available.is_null() {
                            payload.insert("update_available".to_string(), ack.update_available);
                        }
                        if ack.unsupported {
                            payload.insert("unsupported".to_string(), json!(true));
                        }
                        let frame = json!({
                            "type": "ack",
                            "id": data.get("id").cloned().unwrap_or(Value::Null),
                            "payload": Value::Object(payload),
                        });
                        if sink.send(Message::Text(frame.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Some("command_result") => {
                        if let Some(correlation_id) = data.get("id").and_then(Value::as_str) {
                            crate::ws::MANAGER.resolve_command(
                                correlation_id,
                                &node_id,
                                data.get("payload").cloned().unwrap_or(json!({})),
                            );
                        }
                    }
                    // No `event` arm. The Python accepted a `motion_detected`
                    // event here that no CameraNode ever sent, and it
                    // reached `handle_motion_event` without the checks
                    // the HTTP route makes first — so a node could write
                    // motion rows, inbox entries and Sentinel runs while
                    // its org had motion ingestion switched off. Motion is
                    // `POST /api/cameras/{id}/motion`; this is an unknown
                    // message type, answered as one.
                    other => {
                        // `f"Unknown message type: {msg_type}"` over
                        // whatever `.get("type")` returned — which is
                        // not always a string. Python stringifies an
                        // int as its digits and a missing key as
                        // "None"; treating a non-string as absent
                        // reported "None" where Python reported "5".
                        let _ = other;
                        let described = match data.get("type") {
                            Some(value) => crate::pyrepr::str_value(value),
                            None => "None".to_string(),
                        };
                        tracing::warn!(node_id, msg_type = %described, "Unknown WS message type");
                        let refusal = json!({
                            "type": "error",
                            "id": data.get("id").cloned().unwrap_or(Value::Null),
                            "payload": {"detail": format!("Unknown message type: {described}")},
                        });
                        if sink.send(Message::Text(refusal.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }

    // This socket's id, so a loop that has already been replaced
    // cannot evict the connection that replaced it.
    crate::ws::MANAGER.disconnect(&node_id, connection_id);
    crate::ws::MESSAGE_LIMITER.forget(&node_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The score reaches the row through `int()` and a clamp, and every
    /// one of these branches decides whether an event is recorded at
    /// all.
    #[test]
    fn the_score_is_clamped_the_way_python_clamps_it() {
        let score_of = |value: Value| {
            python_int_of_json(&value).map(|parsed| match parsed {
                PyInt::Small(v) => v.clamp(0, 100),
                PyInt::Big { negative: false } => 100,
                PyInt::Big { negative: true } => 0,
            })
        };
        assert_eq!(score_of(json!(50)), Some(50));
        assert_eq!(score_of(json!(0)), Some(0));
        assert_eq!(score_of(json!(100)), Some(100));
        // Out of range clamps rather than dropping the event.
        assert_eq!(score_of(json!(101)), Some(100));
        assert_eq!(score_of(json!(-1)), Some(0));
        // A float truncates toward zero first, then clamps.
        assert_eq!(score_of(json!(99.9)), Some(99));
        assert_eq!(score_of(json!("77")), Some(77));
        assert_eq!(score_of(json!(true)), Some(1));
        // And these are the ones `int()` refuses, which drop the event.
        for bad in [json!("abc"), json!("3.9"), json!([]), json!({})] {
            assert_eq!(score_of(bad.clone()), None, "{bad}");
        }
    }

    /// An offset is *dropped*, not applied. `replace(tzinfo=None)`
    /// keeps the wall-clock reading and throws the offset away, so an
    /// event stamped `10:00:00+05:00` is stored as 10:00:00.
    #[test]
    fn a_timestamps_offset_is_discarded_rather_than_converted() {
        let parsed = crate::pydatetime::fromisoformat("2026-09-21T10:00:00+05:00").unwrap();
        assert_eq!(
            crate::models::iso_naive(parsed.naive),
            "2026-09-21T10:00:00"
        );
        // Had it converted, this would read 05:00:00.
        assert_ne!(
            crate::models::iso_naive(parsed.naive),
            "2026-09-21T05:00:00"
        );

        // A naive stamp passes through unchanged.
        let naive = crate::pydatetime::fromisoformat("2026-09-21T10:00:00").unwrap();
        assert_eq!(crate::models::iso_naive(naive.naive), "2026-09-21T10:00:00");
        // Anything unparseable falls back to the server clock, which is
        // why this returns an error rather than a value.
        assert!(crate::pydatetime::fromisoformat("not a date").is_err());
    }

    /// Three outcomes, not two, and conflating the last two is what
    /// recorded an event Python had thrown away.
    ///
    /// A value `int()` refuses becomes NULL and the event is kept. A
    /// value it accepts but the `integer` column cannot hold makes
    /// Python's commit fail, which loses the row, the broadcast and the
    /// notification together.
    #[test]
    fn an_oversized_segment_sequence_loses_the_whole_event() {
        #[derive(Debug, PartialEq)]
        enum Outcome {
            Stored(Option<i32>),
            EventLost,
        }
        let seq_of = |value: Value| match python_int_of_json(&value) {
            None => Outcome::Stored(None),
            Some(PyInt::Small(v)) => match i32::try_from(v) {
                Ok(v) => Outcome::Stored(Some(v)),
                Err(_) => Outcome::EventLost,
            },
            Some(PyInt::Big { .. }) => Outcome::EventLost,
        };
        assert_eq!(seq_of(json!(42)), Outcome::Stored(Some(42)));
        assert_eq!(seq_of(json!(i32::MAX)), Outcome::Stored(Some(i32::MAX)));
        assert_eq!(seq_of(json!("7")), Outcome::Stored(Some(7)));
        // `int()` refuses these, so Python stores NULL and goes on.
        assert_eq!(seq_of(json!("nope")), Outcome::Stored(None));
        assert_eq!(seq_of(json!([])), Outcome::Stored(None));
        // These it accepts, and the column does not.
        assert_eq!(seq_of(json!(i64::from(i32::MAX) + 1)), Outcome::EventLost);
        assert_eq!(seq_of(json!(i64::from(i32::MIN) - 1)), Outcome::EventLost);
        assert_eq!(seq_of(json!("99999999999999999999")), Outcome::EventLost);
    }
}
