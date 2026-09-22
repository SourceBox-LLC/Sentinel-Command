//! The Home Assistant integration surface: `/api/integration/*`.
//!
//! Ported from `backend/app/api/integration.py` and
//! `backend/app/core/integration_auth.py`: the camera list, the
//! recording switch and the status rollup. Not ported: `/snapshot`,
//! which round-trips to the node over the WebSocket manager, and
//! `/motion/stream`, which subscribes to an in-process broadcaster.

use axum::extract::{ConnectInfo, FromRequestParts, Path, State};
use axum::http::{request::Parts, HeaderMap};
use axum::Json;
use chrono::NaiveDateTime;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::api::nodes::node_effective_status;
use crate::app::AppState;
use crate::audit::{audit_label, python_json, write_audit};
use crate::auth::claims::AuthUser;
use crate::error::ApiError;
use crate::models::{camera_effective_status, now_naive};
use crate::plans::{resolve_org_plan, PlanContext};
use crate::query::{parse_handler_json, path_segment};
use crate::ratelimit::PerMinute;

/// An org resolved from a Bearer integration key (`osi_`).
///
/// Integration keys share `mcp_api_keys` with MCP keys, and
/// `kind = 'integration'` is the boundary: an MCP key must not reach
/// this surface, just as an integration key must not reach the MCP
/// tools.
///
/// The key is hashed the way the CameraNode routes hash theirs —
/// `raw_key.encode()` of a header Starlette decoded as latin-1, so UTF-8
/// of that text — and trimmed with Python's `str.strip()`, which strips
/// Unicode whitespace: a trailing U+00A0, one latin-1 byte on the wire,
/// is removed.
pub struct IntegrationUser(pub AuthUser);

impl FromRequestParts<AppState> for IntegrationUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let auth: String = parts
            .headers
            .get("authorization")
            .map(|v| v.as_bytes().iter().map(|&b| b as char).collect())
            .unwrap_or_default();
        // `auth.lower().startswith("bearer ")`
        if !auth.to_lowercase().starts_with("bearer ") {
            return Err(ApiError::unauthorized("Missing Bearer integration key"));
        }
        // `auth.split(" ", 1)[1].strip()`
        let raw = auth.split_once(' ').map(|(_, rest)| rest).unwrap_or("");
        let raw = python_strip(raw);
        if raw.is_empty() {
            return Err(ApiError::unauthorized("Empty Bearer token"));
        }
        let hash: String = Sha256::digest(raw.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        let row: Option<(i32, String, String)> = sqlx::query_as(
            "SELECT id, org_id, name FROM mcp_api_keys
              WHERE key_hash = $1 AND revoked = false AND kind = 'integration' LIMIT 1",
        )
        .bind(&hash)
        .fetch_optional(&state.pool)
        .await?;
        let Some((id, org_id, name)) = row else {
            return Err(ApiError::unauthorized("Invalid or revoked integration key"));
        };

        // Not best-effort, unlike the agent key: the Python commits this
        // in the same try as the lookup, so a failure is a 500 there.
        sqlx::query("UPDATE mcp_api_keys SET last_used_at = $1 WHERE id = $2")
            .bind(now_naive())
            .bind(id)
            .execute(&state.pool)
            .await?;

        // org_role "integration" keeps is_admin false: an integration key
        // reads and drives cameras, and performs no admin action. The
        // plan is the AuthUser default and not authoritative — routes
        // that need it resolve it.
        Ok(IntegrationUser(AuthUser {
            user_id: format!("integration:{id}"),
            org_id,
            org_role: "integration".into(),
            org_permissions: vec![],
            email: String::new(),
            username: name,
            plan: "free_org".into(),
            features: vec![],
        }))
    }
}

/// `str.strip()` with no argument: Unicode whitespace, plus the four
/// C0 separators Python also treats as whitespace.
fn python_strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

#[derive(sqlx::FromRow)]
struct CameraRow {
    camera_id: String,
    name: String,
    status: Option<String>,
    last_seen: Option<NaiveDateTime>,
    video_codec: Option<String>,
    audio_codec: Option<String>,
    continuous_24_7: Option<bool>,
    scheduled_recording: Option<bool>,
    node_pk: Option<i32>,
}

#[derive(sqlx::FromRow, Clone)]
struct NodeRow {
    id: i32,
    node_id: String,
    name: String,
    status: Option<String>,
    last_seen: Option<NaiveDateTime>,
    local_ip: Option<String>,
    http_port: Option<i32>,
    node_version: Option<String>,
    storage_used_bytes: Option<i64>,
    storage_max_bytes: Option<i64>,
    storage_disk_free_bytes: Option<i64>,
    storage_disk_total_bytes: Option<i64>,
}

impl NodeRow {
    fn online(&self) -> bool {
        node_effective_status(self.status.as_deref(), self.last_seen).as_deref() == Some("online")
    }
}

const NODE_COLUMNS: &str = "id, node_id, name, status, last_seen, local_ip, http_port, node_version,
     storage_used_bytes, storage_max_bytes, storage_disk_free_bytes, storage_disk_total_bytes";

/// `GET /api/integration/cameras` — the one call Home Assistant polls
/// to build all its entities.
pub async fn list_cameras(
    rate: PerMinute<120>,
    State(state): State<AppState>,
    IntegrationUser(user): IntegrationUser,
) -> Result<Json<Value>, ApiError> {
    rate.check().await?;
    // Cameras alone, then their nodes — not a join. Python lazy-loads
    // `cam.node` per camera, and its list order rests on
    // `ORDER BY created_at` over rows whose created_at is often NULL; a
    // join is a different plan and can break those ties differently.
    let cameras: Vec<CameraRow> = sqlx::query_as(
        "SELECT camera_id, name, status, last_seen, video_codec, audio_codec,
                continuous_24_7, scheduled_recording, node_id AS node_pk
           FROM cameras WHERE org_id = $1 ORDER BY created_at ASC",
    )
    .bind(&user.org_id)
    .fetch_all(&state.pool)
    .await?;

    let ids: Vec<i32> = cameras.iter().filter_map(|c| c.node_pk).collect();
    let nodes: Vec<NodeRow> = sqlx::query_as(&format!(
        "SELECT {NODE_COLUMNS} FROM camera_nodes WHERE id = ANY($1)"
    ))
    .bind(&ids)
    .fetch_all(&state.pool)
    .await?;
    let node = |pk: Option<i32>| pk.and_then(|pk| nodes.iter().find(|n| n.id == pk));

    let items: Vec<Value> = cameras
        .iter()
        .map(|cam| {
            let n = node(cam.node_pk);
            let eff = camera_effective_status(cam.status.as_deref(), cam.last_seen);
            // LAN-direct HLS, only when the node is online and
            // advertising an address: HA on the same network pulls
            // video straight from it, uncapped on every tier.
            // `node.local_ip` is a truthiness test: an empty string builds nothing.
            let local_url = n.filter(|n| n.local_ip.as_deref().is_some_and(|ip| !ip.is_empty()) && n.online()).map(|n| {
                format!(
                    "http://{}:{}/hls/{}/stream.m3u8",
                    n.local_ip.as_deref().unwrap_or_default(),
                    n.http_port.filter(|p| *p != 0).unwrap_or(8080),
                    cam.camera_id
                )
            });
            json!({
                "id": cam.camera_id,
                "name": cam.name,
                "status": eff,
                "online": eff.as_deref() != Some("offline"),
                "video_codec": cam.video_codec,
                "audio_codec": cam.audio_codec,
                "node_id": n.map(|n| n.node_id.clone()),
                "node_name": n.map(|n| n.name.clone()),
                "node_online": n.is_some_and(NodeRow::online),
                "recording": cam.continuous_24_7.unwrap_or(false),
                "scheduled_recording": cam.scheduled_recording.unwrap_or(false),
                "snapshot_url": format!("/api/integration/cameras/{}/snapshot", cam.camera_id),
                "stream": {"local_url": local_url, "proxy_url": null},
            })
        })
        .collect();
    Ok(Json(json!({ "cameras": items })))
}

/// `POST /api/integration/cameras/{camera_id}/recording` — the HA switch.
pub async fn set_recording(
    rate: PerMinute<60>,
    State(state): State<AppState>,
    IntegrationUser(user): IntegrationUser,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Path(camera_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let camera_id = path_segment(&camera_id)?.to_string();
    // Everything below is inside the Python function, body included —
    // `await request.json()` — so the slot is spent first.
    rate.check().await?;
    let body = parse_handler_json(&body)?;
    let recording = body
        .get("recording")
        .map(crate::pyrepr::truthy)
        .unwrap_or(false);

    let row: Option<(i32, bool)> = sqlx::query_as(
        "SELECT id, continuous_24_7 FROM cameras WHERE camera_id = $1 AND org_id = $2 LIMIT 1",
    )
    .bind(&camera_id)
    .bind(&user.org_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((id, current)) = row else {
        return Err(ApiError::not_found("Camera not found"));
    };
    // SQLAlchemy emits no UPDATE for an unchanged value, so updated_at
    // only moves when the switch actually flips.
    if current != recording {
        sqlx::query("UPDATE cameras SET continuous_24_7 = $1, updated_at = $2 WHERE id = $3")
            .bind(recording)
            .bind(now_naive())
            .bind(id)
            .execute(&state.pool)
            .await?;
    }

    write_audit(
        &state.pool,
        &user.org_id,
        "recording_toggled",
        &user.user_id,
        &audit_label(&user),
        Some(python_json(&[
            ("camera_id", json!(camera_id)),
            ("recording", json!(recording)),
            ("via", json!("integration")),
        ])),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    Ok(Json(json!({ "camera_id": camera_id, "recording": recording })))
}

/// `GET /api/integration/status` — the org rollup HA sensors read, and
/// the call its config flow uses to validate a URL and key.
pub async fn status(
    rate: PerMinute<120>,
    State(state): State<AppState>,
    IntegrationUser(user): IntegrationUser,
) -> Result<Json<Value>, ApiError> {
    rate.check().await?;
    let cameras: Vec<(Option<String>, Option<NaiveDateTime>)> =
        sqlx::query_as("SELECT status, last_seen FROM cameras WHERE org_id = $1")
            .bind(&user.org_id)
            .fetch_all(&state.pool)
            .await?;
    let nodes: Vec<NodeRow> = sqlx::query_as(&format!(
        "SELECT {NODE_COLUMNS} FROM camera_nodes WHERE org_id = $1"
    ))
    .bind(&user.org_id)
    .fetch_all(&state.pool)
    .await?;

    let ctx = PlanContext {
        pool: &state.pool,
        client: &state.http,
        clerk_base_url: &state.config.clerk_api_url,
        clerk_secret: &state.config.clerk_secret_key,
        local_auth: state.config.is_local_auth(),
    };
    let plan = resolve_org_plan(&ctx, &user.org_id).await;

    let cameras_online = cameras
        .iter()
        .filter(|(s, seen)| camera_effective_status(s.as_deref(), *seen).as_deref() != Some("offline"))
        .count();
    let items: Vec<Value> = nodes
        .iter()
        .map(|n| {
            json!({
                "node_id": n.node_id,
                "name": n.name,
                "online": n.online(),
                "local_ip": n.local_ip,
                "version": n.node_version,
                "storage": {
                    "used_bytes": n.storage_used_bytes,
                    "max_bytes": n.storage_max_bytes,
                    "disk_free_bytes": n.storage_disk_free_bytes,
                    "disk_total_bytes": n.storage_disk_total_bytes,
                },
            })
        })
        .collect();

    Ok(Json(json!({
        "org_id": user.org_id,
        "plan": plan,
        "cameras": {"total": cameras.len(), "online": cameras_online},
        "nodes": {
            "total": nodes.len(),
            "online": nodes.iter().filter(|n| n.online()).count(),
            "items": items,
        },
    })))
}

/// `INTEGRATION_MAX_SSE_SUBSCRIBERS` — a small fixed cap, not a
/// per-tier one. A home runs one or two Home Assistant instances, and
/// this bounds memory against a scripted connect loop.
const MAX_SSE_SUBSCRIBERS: usize = 10;

/// `GET /api/integration/motion/stream` — the feed behind Home
/// Assistant's motion `binary_sensor`s.
///
/// The same org-wide motion pipeline the dashboard consumes, through a
/// *separate* subscriber pool, so a persistent Home Assistant
/// connection never eats into the dashboard's per-tier cap.
pub async fn motion_stream(
    rate: crate::ratelimit::PerMinute<60>,
    user: IntegrationUser,
) -> Result<axum::response::Response, ApiError> {
    rate.check().await?;
    let Some(subscription) = crate::api::motion::INTEGRATION_BROADCASTER.subscribe(
        &user.0.org_id,
        true,
        MAX_SSE_SUBSCRIBERS,
    ) else {
        return Err(ApiError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            format!(
                "Too many open integration motion streams for this org (cap: \
                 {MAX_SSE_SUBSCRIBERS}). Close unused connections and retry."
            ),
        ));
    };
    Ok(crate::sse::stream_response(
        subscription,
        crate::sse::connected_frame(&user.0.org_id),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_matches_python_on_latin1_whitespace() {
        // U+00A0 and U+0085 are whitespace to str.strip(), and each is a
        // single byte in a latin-1 header.
        assert_eq!(python_strip("\u{a0}osi_key\u{85}"), "osi_key");
        assert_eq!(python_strip("\u{1f}osi_key\t"), "osi_key");
        assert_eq!(python_strip("osi key"), "osi key");
    }
}
