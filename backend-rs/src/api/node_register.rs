//! CameraNode registration and heartbeat.
//!
//! Ported from `backend/app/api/nodes.py`. These two are the CameraNode
//! side of the contract: `/register` reconciles the node's camera list
//! against the org's plan cap, `/heartbeat` keeps the row warm and
//! answers with what the node should be doing — which cameras the plan
//! has suspended, and which should be recording right now.
//!
//! Both were blocked until `enforce_camera_cap` was ported, and both
//! were *overdue* the moment the notification broadcaster moved: they
//! publish, and every subscriber is already on Rust's copy.

use axum::extract::{Request, State};
use axum::Json;
use serde_json::{json, Map, Value};

use crate::api::node_writes::{node_key_hash, sanitize_video_codec};
use crate::app::AppState;
use crate::error::ApiError;
use crate::models::now_naive;
use crate::notifications::{create_notification, NewNotification};
use crate::plans::{self, PlanContext};
use crate::query::{parse_model_body, BodyErrors};
use crate::ratelimit::PerMinute;

/// `_CAMERANODE_DISK_LOW_THRESHOLD_PERCENT`.
const DISK_LOW_THRESHOLD_PERCENT: f64 = 90.0;
/// `_CAMERANODE_DISK_LOW_REEMIT_INTERVAL_SECONDS` — six hours.
const DISK_LOW_REEMIT_SECONDS: i64 = 6 * 60 * 60;
/// `_PLAN_LIMIT_NOTIF_THROTTLE_SECONDS` — one hour.
const PLAN_LIMIT_THROTTLE_SECONDS: i64 = 3600;

fn plan_ctx(state: &AppState) -> PlanContext<'_> {
    PlanContext {
        pool: &state.pool,
        client: &state.http,
        clerk_base_url: &state.config.clerk_api_url,
        clerk_secret: &state.config.clerk_secret_key,
        local_auth: state.config.is_local_auth(),
    }
}

/// The node row both handlers authenticate against.
#[derive(sqlx::FromRow)]
struct NodeAuthRow {
    id: i32,
    node_id: String,
    org_id: String,
    name: Option<String>,
    hostname: Option<String>,
    api_key_hash: String,
    local_ip: Option<String>,
    http_port: Option<i32>,
}

const NODE_AUTH_SELECT: &str = "SELECT id, node_id, org_id, name, hostname, api_key_hash,
                                       local_ip, http_port
                                  FROM camera_nodes WHERE node_id = $1 LIMIT 1";

/// `CameraReport`, in Pydantic's field-declaration order — which is the
/// order its errors come out in.
struct CameraReport {
    camera_id: Option<String>,
    device_path: Option<String>,
    name: Option<String>,
    node_type: Option<String>,
    capabilities: Option<Vec<String>>,
}

/// `Optional[list[Model]]`: a non-list is `list_type` on the field, a
/// non-mapping element is `model_attributes_type` at its index, and
/// each element's own fields report under that index.
fn parse_camera_reports(errors: &mut BodyErrors, body: &Value) -> Vec<CameraReport> {
    let value = match body.get("cameras") {
        None | Some(Value::Null) => return Vec::new(),
        Some(value) => value,
    };
    let Some(items) = value.as_array() else {
        errors.list_type("cameras", value);
        return Vec::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        if item.as_object().is_none() {
            errors.model_attributes_type_at(&[json!("cameras"), json!(i)], item);
            continue;
        }
        let report = errors.within(&[json!("cameras"), json!(i)], |e| CameraReport {
            camera_id: e.optional_string(item, "camera_id", 150),
            device_path: e.optional_string(item, "device_path", 255),
            name: e.optional_string(item, "name", 100),
            node_type: e.optional_string(item, "node_type", 20),
            capabilities: e.optional_list_of_strings(item, "capabilities"),
            // width and height are validated for their side effect on
            // `errors` only — nothing stores them.
        });
        errors.within(&[json!("cameras"), json!(i)], |e| {
            e.optional_int_in_range(item, "width", 1, 7680);
            e.optional_int_in_range(item, "height", 1, 4320);
        });
        out.push(report);
    }
    out
}

/// `POST /api/nodes/register`.
pub async fn register_node(
    rate: PerMinute<10>,
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    rate.check().await?;
    let key = crate::api::node_writes::header_bytes(request.headers(), "x-node-api-key").to_vec();
    let bytes = crate::api::node_writes::body_bytes(request).await?;
    let body = parse_model_body(&bytes)?;

    let mut errors = BodyErrors::new();
    let node_id = errors.required_string(&body, "node_id", 50);
    errors.optional_string(&body, "name", 100);
    let hostname = errors.optional_string(&body, "hostname", 255);
    let local_ip = errors.optional_string(&body, "local_ip", 45);
    // `Field(8080, ge=1, le=65535)` on an `Optional[int]`: the default
    // applies when the key is ABSENT, so a register that does not
    // mention the port sets it to 8080 rather than leaving it alone.
    // An explicit null is None, which `or` then falls through.
    let http_port = match body.get("http_port") {
        None => Some(8080),
        Some(Value::Null) => None,
        Some(_) => errors.optional_int_in_range(&body, "http_port", 1, 65535),
    };
    let lan_streaming = errors.optional_bool(&body, "lan_streaming");
    let cameras = parse_camera_reports(&mut errors, &body);
    let video_codec = errors.optional_string(&body, "video_codec", 50);
    let audio_codec = errors.optional_string(&body, "audio_codec", 50);
    let node_version = errors.optional_string(&body, "node_version", 50);
    errors.finish()?;

    // The key check happens after validation, because the handler body
    // only runs once Pydantic is satisfied.
    if key.is_empty() {
        return Err(ApiError::unauthorized("API key required"));
    }
    let api_key_hash = node_key_hash(&key);
    // `node_secret` hands the key back. Starlette decoded the header as
    // latin-1, so every byte is its own code point — 0xFF is U+00FF,
    // not a UTF-8 error. `from_utf8_lossy` turned it into U+FFFD and
    // handed the node a key it could not authenticate with.
    let api_key_text: String = key.iter().map(|&b| b as char).collect();

    let sanitized_video_codec = video_codec.as_deref().map(sanitize_video_codec);

    let node: Option<NodeAuthRow> = sqlx::query_as(NODE_AUTH_SELECT)
        .bind(&node_id)
        .fetch_optional(&state.pool)
        .await?;
    let Some(node) = node else {
        return Err(ApiError::not_found(
            "Node not found. Create this node in the dashboard first.",
        ));
    };

    if node.api_key_hash != api_key_hash {
        crate::api::node_writes::record_node_register_error(
            &state.pool,
            node.id,
            "Invalid API key during registration — rotate the key in Settings and re-run the installer.",
        )
        .await;
        return Err(ApiError::forbidden("Invalid API key for this node"));
    }

    // The version is persisted whether or not it passes, so the
    // dashboard can show why a node is being refused.
    let latest = crate::versions::latest_node_version(&state.config.latest_node_version);
    let check = crate::versions::check_node_version(
        node_version.as_deref(),
        &state.config.min_supported_node_version,
        &latest,
    );
    let parsed = check["parsed"].as_str().unwrap_or_default().to_string();
    let min_supported = check["min_supported"].as_str().unwrap_or_default().to_string();
    let latest_str = check["latest"].as_str().unwrap_or_default().to_string();
    // `version_check["parsed"] if data.node_version else None` — an
    // absent version clears the column rather than storing "0.0.0".
    let stored_version = node_version.as_ref().map(|_| parsed.clone());
    let now = now_naive();
    sqlx::query(
        "UPDATE camera_nodes SET node_version = $1, version_checked_at = $2, updated_at = $2
          WHERE id = $3",
    )
    .bind(&stored_version)
    .bind(now)
    .bind(node.id)
    .execute(&state.pool)
    .await?;

    if !check["supported"].as_bool().unwrap_or(true) {
        crate::api::node_writes::record_node_register_error(
            &state.pool,
            node.id,
            &format!(
                "CameraNode version {parsed} is below the minimum supported \
                 {min_supported}. Update CameraNode to {latest_str} and re-register."
            ),
        )
        .await;
        return Err(ApiError::new(
            axum::http::StatusCode::UPGRADE_REQUIRED,
            json!({
                "message": format!(
                    "CameraNode {parsed} is no longer supported. \
                     Minimum: {min_supported}, latest: {latest_str}."
                ),
                "reported": check["reported"].clone(),
                "min_supported": min_supported,
                "latest": latest_str,
            }),
        ));
    }

    // `data.hostname or existing.hostname` — an empty string keeps the
    // stored value, because it is falsy.
    let hostname = hostname.filter(|h| !h.is_empty()).or(node.hostname);
    let local_ip = if lan_streaming == Some(false) {
        None
    } else {
        local_ip.filter(|ip| !ip.is_empty()).or(node.local_ip)
    };
    let http_port = http_port.filter(|p| *p != 0).map(|p| p as i32).or(node.http_port);

    sqlx::query(
        "UPDATE camera_nodes
            SET hostname = $1, local_ip = $2, http_port = $3, status = 'online',
                last_seen = $4, last_register_error = NULL,
                last_register_error_at = NULL, updated_at = $4
          WHERE id = $5",
    )
    .bind(&hostname)
    .bind(&local_ip)
    .bind(http_port)
    .bind(now)
    .bind(node.id)
    .execute(&state.pool)
    .await?;

    if video_codec.is_some() {
        sqlx::query(
            "UPDATE camera_nodes
                SET video_codec = $1, audio_codec = $2, codec_detected_at = $3, updated_at = $3
              WHERE id = $4",
        )
        .bind(&sanitized_video_codec)
        .bind(&audio_codec)
        .bind(now)
        .bind(node.id)
        .execute(&state.pool)
        .await?;
    }

    let org_id = node.org_id.clone();
    let ctx = plan_ctx(&state);
    let plan = plans::resolve_org_plan(&ctx, &org_id).await;
    let limits = plans::get_plan_limits(&plan);
    let (current_cameras,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM cameras WHERE org_id = $1")
            .bind(&org_id)
            .fetch_one(&state.pool)
            .await?;

    // `camera_mapping` is a dict keyed on device_path, so a repeated
    // path collapses to one entry — and insertion order is what the
    // response preserves.
    let mut camera_mapping: Map<String, Value> = Map::new();
    let mut new_camera_count: i64 = 0;
    let mut skipped_cameras: Vec<String> = Vec::new();

    for cam in &cameras {
        let device_path = cam
            .device_path
            .clone()
            .filter(|p| !p.is_empty())
            .or_else(|| cam.camera_id.clone().filter(|p| !p.is_empty()))
            .unwrap_or_else(|| "unknown".to_string());
        let sanitized_device = device_path
            .replace(['/', '\\', ' '], "_")
            .trim_matches('_')
            .to_string();
        let camera_id = format!("{node_id}_{sanitized_device}");
        camera_mapping.insert(device_path.clone(), json!(camera_id));

        let existing: Option<(i32, Option<String>)> =
            sqlx::query_as("SELECT id, name FROM cameras WHERE camera_id = $1 LIMIT 1")
                .bind(&camera_id)
                .fetch_optional(&state.pool)
                .await?;

        if let Some((existing_id, existing_name)) = existing {
            let name = cam.name.clone().filter(|n| !n.is_empty()).or(existing_name);
            sqlx::query(
                "UPDATE cameras SET name = $1, last_seen = $2, status = 'online', updated_at = $2
                  WHERE id = $3",
            )
            .bind(&name)
            .bind(now)
            .bind(existing_id)
            .execute(&state.pool)
            .await?;
            if video_codec.is_some() {
                sqlx::query(
                    "UPDATE cameras SET video_codec = $1, audio_codec = $2, updated_at = $3
                      WHERE id = $4",
                )
                .bind(&sanitized_video_codec)
                .bind(&audio_codec)
                .bind(now)
                .bind(existing_id)
                .execute(&state.pool)
                .await?;
            }
            continue;
        }

        if current_cameras + new_camera_count >= limits.max_cameras {
            tracing::warn!(
                org_id,
                camera_id,
                used = current_cameras + new_camera_count,
                cap = limits.max_cameras,
                "camera limit reached — skipping camera"
            );
            // `cam_data.name or sanitized_device`.
            skipped_cameras.push(
                cam.name
                    .clone()
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| sanitized_device.clone()),
            );
            continue;
        }

        let name = cam
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("Camera {sanitized_device}"));
        let node_type = cam
            .node_type
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "usb".to_string());
        // `",".join(caps) if caps else "streaming"` — an empty list is
        // falsy, so it takes the default rather than storing "".
        let capabilities = match &cam.capabilities {
            Some(caps) if !caps.is_empty() => caps.join(","),
            _ => "streaming".to_string(),
        };
        sqlx::query(
            "INSERT INTO cameras
                (camera_id, org_id, node_id, name, node_type, capabilities, status,
                 last_seen, video_codec, audio_codec, codec_detected_at,
                 disabled_by_plan, continuous_24_7, scheduled_recording,
                 created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, 'online', $7, $8, $9, $10,
                     false, false, false, $7, $7)",
        )
        .bind(&camera_id)
        .bind(&org_id)
        .bind(node.id)
        .bind(&name)
        .bind(&node_type)
        .bind(&capabilities)
        .bind(now)
        .bind(&sanitized_video_codec)
        .bind(&audio_codec)
        .bind(video_codec.as_ref().map(|_| now))
        .execute(&state.pool)
        .await?;
        new_camera_count += 1;
    }

    // Cameras this node no longer reports are removed, with their
    // segment cache — a rename or a sanitisation fix otherwise leaves
    // the old id behind forever.
    let reported: Vec<String> = camera_mapping
        .values()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let stale: Vec<(i32, String)> =
        sqlx::query_as("SELECT id, camera_id FROM cameras WHERE node_id = $1 ORDER BY id")
            .bind(node.id)
            .fetch_all(&state.pool)
            .await?;
    for (stale_id, stale_camera_id) in stale {
        if reported.contains(&stale_camera_id) {
            continue;
        }
        tracing::info!(camera_id = stale_camera_id, "removing stale camera record");
        state.hls.cleanup_camera(&stale_camera_id);
        sqlx::query("DELETE FROM cameras WHERE id = $1")
            .bind(stale_id)
            .execute(&state.pool)
            .await?;
    }

    // Safety net for a missed subscription webhook. Idempotent, so the
    // steady state is one indexed query.
    plans::enforce_camera_cap(&ctx, &state.pool, &org_id).await?;

    if !skipped_cameras.is_empty() {
        emit_plan_limit_notification(&state, &org_id, &plan, limits.max_cameras, &skipped_cameras)
            .await;
    }

    let plan_name = plans::get_plan_display_name(&plan);
    let mut response = json!({
        "success": true,
        "node_id": node.node_id,
        "node_secret": api_key_text,
        "status": "updated",
        "message": "Node re-registered successfully",
        "cameras": Value::Object(camera_mapping),
        "plan": plans::wire_plan_slug(&plan),
    });
    if !skipped_cameras.is_empty() {
        response["plan_limit_hit"] = json!({
            "plan": plan_name,
            "max_cameras": limits.max_cameras,
            "skipped": skipped_cameras,
            "detail": format!(
                "Plan limit reached ({} on {}). Upgrade to add: {}.",
                limits.max_cameras,
                plan_name,
                skipped_cameras.join(", ")
            ),
        });
    }
    if !check["update_available"].is_null() {
        response["update_available"] = check["update_available"].clone();
    }
    Ok(Json(response))
}

/// One-per-hour inbox notification when the cap rejected cameras.
///
/// Best-effort throughout: registration must not fail because the
/// notification layer did.
async fn emit_plan_limit_notification(
    state: &AppState,
    org_id: &str,
    plan: &str,
    max_cameras: i64,
    skipped: &[String],
) {
    let now = now_naive();
    let last = crate::settings::get(&state.pool, org_id, "plan_limit_notif_last_at", None)
        .await
        .unwrap_or_default()
        .unwrap_or_default();
    if !last.is_empty() {
        // A malformed timestamp is treated as never emitted, and
        // overwritten below.
        if let Ok(parsed) = crate::pydatetime::fromisoformat(&last) {
            if (now - parsed.naive).num_seconds() < PLAN_LIMIT_THROTTLE_SECONDS {
                return;
            }
        }
    }

    let plan_name = plans::get_plan_display_name(plan);
    let head: Vec<String> = skipped.iter().take(5).cloned().collect();
    let more = if skipped.len() > 5 {
        format!(" (+{} more)", skipped.len() - 5)
    } else {
        String::new()
    };
    let body = format!(
        "Your node reported cameras beyond the {max_cameras}-camera limit. Skipped: {}{}. \
         Upgrade to add them.",
        head.join(", "),
        more
    );

    let mut notification =
        NewNotification::new("plan_limit_reached", format!("Camera limit reached on {plan_name}"));
    notification.body = body;
    notification.severity = "warning".to_string();
    notification.audience = "admin".to_string();
    notification.link = Some("/settings".to_string());
    notification.meta = Some(json!({
        "plan": plan_name,
        "max_cameras": max_cameras,
        "skipped": skipped,
    }));
    create_notification(state, org_id, notification).await;

    if let Err(err) = crate::settings::set(
        &state.pool,
        org_id,
        "plan_limit_notif_last_at",
        &crate::models::iso_naive(now),
    )
    .await
    {
        tracing::error!(error = %err, org_id, "failed to record plan-limit notification time");
    }
}

/// `POST /api/nodes/heartbeat`.
pub async fn node_heartbeat(
    rate: PerMinute<60>,
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    rate.check().await?;
    let key = crate::api::node_writes::header_bytes(request.headers(), "x-node-api-key").to_vec();
    let bytes = crate::api::node_writes::body_bytes(request).await?;
    let body = parse_model_body(&bytes)?;

    let mut errors = BodyErrors::new();
    let node_id = errors.required_string(&body, "node_id", 50);
    let local_ip = errors.optional_string(&body, "local_ip", 45);
    let lan_streaming = errors.optional_bool(&body, "lan_streaming");
    let camera_updates = parse_camera_statuses(&mut errors, &body);
    let node_version = errors.optional_string(&body, "node_version", 50);
    let storage = parse_storage_stats(&mut errors, &body);
    errors.finish()?;

    if key.is_empty() {
        return Err(ApiError::unauthorized("API key required"));
    }
    let api_key_hash = node_key_hash(&key);

    let node: Option<NodeAuthRow> = sqlx::query_as(NODE_AUTH_SELECT)
        .bind(&node_id)
        .fetch_optional(&state.pool)
        .await?;
    let Some(node) = node else {
        return Err(ApiError::not_found("Node not found"));
    };
    if node.api_key_hash != api_key_hash {
        return Err(ApiError::forbidden("Invalid API key"));
    }

    let latest = crate::versions::latest_node_version(&state.config.latest_node_version);
    let check = crate::versions::check_node_version(
        node_version.as_deref(),
        &state.config.min_supported_node_version,
        &latest,
    );
    let now = now_naive();
    let stored_version = node_version
        .as_ref()
        .map(|_| check["parsed"].as_str().unwrap_or_default().to_string());

    // The version columns are written only once the gate passes.
    // Python assigns them before the check, but a 426 raises out of the
    // handler with the session never committed, so the assignment is
    // rolled back and the row keeps whatever it had. Registration
    // differs — there, `_record_node_register_error` commits, which
    // carries the pending assignment with it.
    if !check["supported"].as_bool().unwrap_or(true) {
        let parsed = check["parsed"].as_str().unwrap_or_default();
        let min_supported = check["min_supported"].as_str().unwrap_or_default();
        let latest_str = check["latest"].as_str().unwrap_or_default();
        return Err(ApiError::new(
            axum::http::StatusCode::UPGRADE_REQUIRED,
            json!({
                "message": format!(
                    "CameraNode {parsed} is no longer supported. \
                     Minimum: {min_supported}, latest: {latest_str}."
                ),
                "reported": check["reported"].clone(),
                "min_supported": min_supported,
                "latest": latest_str,
            }),
        ));
    }

    sqlx::query(
        "UPDATE camera_nodes SET node_version = $1, version_checked_at = $2, updated_at = $2
          WHERE id = $3",
    )
    .bind(&stored_version)
    .bind(now)
    .bind(node.id)
    .execute(&state.pool)
    .await?;

    let local_ip = if lan_streaming == Some(false) {
        None
    } else {
        local_ip.filter(|ip| !ip.is_empty()).or(node.local_ip.clone())
    };
    sqlx::query(
        "UPDATE camera_nodes SET status = 'online', last_seen = $1, local_ip = $2, updated_at = $1
          WHERE id = $3",
    )
    .bind(now)
    .bind(&local_ip)
    .bind(node.id)
    .execute(&state.pool)
    .await?;

    if let Some(stats) = &storage {
        // `node.storage_reported_at = node.last_seen` — the same
        // timestamp, not a second `now()`.
        sqlx::query(
            "UPDATE camera_nodes
                SET storage_used_bytes = $1, storage_max_bytes = $2,
                    storage_disk_free_bytes = $3, storage_disk_total_bytes = $4,
                    storage_reported_at = $5, updated_at = $5
              WHERE id = $6",
        )
        .bind(stats.used_bytes)
        .bind(stats.max_bytes)
        .bind(stats.disk_free_bytes)
        .bind(stats.disk_total_bytes)
        .bind(now)
        .bind(node.id)
        .execute(&state.pool)
        .await?;

        // Best-effort: node connectivity matters more than the alert.
        check_and_emit_disk_low(
            &state,
            &node,
            stats.disk_free_bytes,
            stats.disk_total_bytes,
            now,
        )
        .await;
    }

    if !camera_updates.is_empty() {
        let ids: Vec<String> = camera_updates.iter().map(|c| c.camera_id.clone()).collect();
        let known: Vec<(String,)> = sqlx::query_as(
            "SELECT camera_id FROM cameras WHERE camera_id = ANY($1) AND node_id = $2",
        )
        .bind(&ids)
        .bind(node.id)
        .fetch_all(&state.pool)
        .await?;
        // A camera named twice in one heartbeat is applied twice, in
        // order — the Python loops over the reports, not the rows.
        for update in &camera_updates {
            if !known.iter().any(|(id,)| *id == update.camera_id) {
                continue;
            }
            let last_error = if matches!(update.status.as_str(), "restarting" | "failed" | "error")
            {
                update.last_error.clone()
            } else {
                None
            };
            sqlx::query(
                "UPDATE cameras SET status = $1, last_seen = $2, last_error = $3, updated_at = $2
                  WHERE camera_id = $4 AND node_id = $5",
            )
            .bind(&update.status)
            .bind(now)
            .bind(&last_error)
            .bind(&update.camera_id)
            .bind(node.id)
            .execute(&state.pool)
            .await?;
        }
    }

    // The time-based past-due transition has no webhook behind it, so
    // the heartbeat is where "in grace" becomes "past grace". Gated on
    // the flag, so the happy path pays nothing.
    let past_due = crate::settings::get(&state.pool, &node.org_id, "payment_past_due", Some("false"))
        .await
        .unwrap_or_default()
        .unwrap_or_default()
        == "true";
    if past_due {
        let ctx = plan_ctx(&state);
        if let Err(err) = plans::enforce_camera_cap(&ctx, &state.pool, &node.org_id).await {
            tracing::warn!(error = %err, org_id = node.org_id, "heartbeat past-due sweep failed");
        }
    }

    // The badge plan is read straight from the Setting rather than
    // resolved: heartbeats are every ~30s per node, and resolving would
    // call Clerk for every free org.
    let cached_plan = crate::settings::get(&state.pool, &node.org_id, "org_plan", Some("free_org"))
        .await
        .unwrap_or_default()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "free_org".to_string());

    #[derive(sqlx::FromRow)]
    struct RecordingRow {
        camera_id: String,
        disabled_by_plan: Option<bool>,
        continuous_24_7: Option<bool>,
        scheduled_recording: Option<bool>,
        scheduled_start: Option<String>,
        scheduled_end: Option<String>,
    }
    let node_cameras: Vec<RecordingRow> = sqlx::query_as(
        "SELECT camera_id, disabled_by_plan, continuous_24_7, scheduled_recording,
                scheduled_start, scheduled_end
           FROM cameras WHERE node_id = $1 ORDER BY id",
    )
    .bind(node.id)
    .fetch_all(&state.pool)
    .await?;

    let disabled_cameras: Vec<String> = node_cameras
        .iter()
        .filter(|c| c.disabled_by_plan.unwrap_or(false))
        .map(|c| c.camera_id.clone())
        .collect();

    // Resolved once per heartbeat, not once per camera — and so is the
    // wall-clock reading, which keeps every camera in one heartbeat
    // answering against the same minute.
    let tz = resolve_org_timezone(&state, &node.org_id).await;
    let local = jiff::Timestamp::now().to_zoned(tz);
    let cur_minutes = i64::from(local.hour()) * 60 + i64::from(local.minute());
    let mut recording_state = Map::new();
    for cam in &node_cameras {
        recording_state.insert(
            cam.camera_id.clone(),
            json!(camera_should_record_now(
                cam.continuous_24_7.unwrap_or(false),
                cam.scheduled_recording.unwrap_or(false),
                cam.scheduled_start.as_deref(),
                cam.scheduled_end.as_deref(),
                cur_minutes,
            )),
        );
    }

    let mut response = json!({
        "success": true,
        "timestamp": crate::models::iso_naive(now_naive()),
        "plan": plans::wire_plan_slug(&cached_plan),
        "disabled_cameras": disabled_cameras,
        "recording_state": Value::Object(recording_state),
    });
    if !check["update_available"].is_null() {
        response["update_available"] = check["update_available"].clone();
    }
    Ok(Json(response))
}

struct CameraStatusReport {
    camera_id: String,
    status: String,
    last_error: Option<String>,
}

fn parse_camera_statuses(errors: &mut BodyErrors, body: &Value) -> Vec<CameraStatusReport> {
    let value = match body.get("cameras") {
        None | Some(Value::Null) => return Vec::new(),
        Some(value) => value,
    };
    let Some(items) = value.as_array() else {
        errors.list_type("cameras", value);
        return Vec::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        if item.as_object().is_none() {
            errors.model_attributes_type_at(&[json!("cameras"), json!(i)], item);
            continue;
        }
        out.push(errors.within(&[json!("cameras"), json!(i)], |e| CameraStatusReport {
            camera_id: e.required_string(item, "camera_id", 150),
            status: e.required_string(item, "status", 20),
            last_error: e.optional_string(item, "last_error", 500),
        }));
    }
    out
}

struct StorageStats {
    used_bytes: Option<i64>,
    max_bytes: Option<i64>,
    disk_free_bytes: Option<i64>,
    disk_total_bytes: Option<i64>,
}

fn parse_storage_stats(errors: &mut BodyErrors, body: &Value) -> Option<StorageStats> {
    let value = match body.get("storage_stats") {
        None | Some(Value::Null) => return None,
        Some(value) => value,
    };
    if value.as_object().is_none() {
        errors.model_attributes_type_at(&[json!("storage_stats")], value);
        return None;
    }
    // `ge=0` with no upper bound; i64::MAX stands in for "no le", and a
    // value past it fails the bound the same way Python's would not —
    // no CameraNode reports a disk that size.
    Some(errors.within(&[json!("storage_stats")], |e| StorageStats {
        used_bytes: e.optional_int_in_range(value, "used_bytes", 0, i64::MAX),
        max_bytes: e.optional_int_in_range(value, "max_bytes", 0, i64::MAX),
        disk_free_bytes: e.optional_int_in_range(value, "disk_free_bytes", 0, i64::MAX),
        disk_total_bytes: e.optional_int_in_range(value, "disk_total_bytes", 0, i64::MAX),
    }))
}

/// `_check_and_emit_cameranode_disk_low`.
///
/// Debounced through a Setting row rather than memory, so a deploy does
/// not re-alert every node at once.
async fn check_and_emit_disk_low(
    state: &AppState,
    node: &NodeAuthRow,
    free_bytes: Option<i64>,
    total_bytes: Option<i64>,
    now: chrono::NaiveDateTime,
) {
    // `not free_bytes or not total_bytes` — zero is falsy, and zero
    // free bytes is how CameraNode says "I could not identify the
    // disk", not "the disk is full".
    let (Some(free), Some(total)) = (free_bytes, total_bytes) else {
        return;
    };
    if free == 0 || total <= 0 {
        return;
    }

    let key = format!("cameranode_disk_low_emit_at:{}", node.node_id);
    let pct_used = ((total - free) as f64 / total as f64) * 100.0;
    if pct_used < DISK_LOW_THRESHOLD_PERCENT {
        // Back under the threshold: clear the debounce so the next
        // crossing alerts at once instead of serving out a stale
        // six-hour cooldown.
        if let Err(err) = crate::settings::set(&state.pool, &node.org_id, &key, "").await {
            tracing::error!(error = %err, "failed to clear disk-low debounce");
        }
        return;
    }

    let last = crate::settings::get(&state.pool, &node.org_id, &key, Some(""))
        .await
        .unwrap_or_default()
        .unwrap_or_default();
    if !last.is_empty() {
        if let Ok(parsed) = crate::pydatetime::fromisoformat(&last) {
            if (now - parsed.naive).num_seconds() < DISK_LOW_REEMIT_SECONDS {
                return;
            }
        }
    }

    let pct_rounded = crate::pyrepr::round_to(pct_used, 1);
    let free_gb = crate::pyrepr::round_to(free as f64 / 1024f64.powi(3), 1);
    let total_gb = crate::pyrepr::round_to(total as f64 / 1024f64.powi(3), 1);
    let display = node
        .name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| node.node_id.clone());

    let mut notification = NewNotification::new(
        "cameranode_disk_low",
        format!("CameraNode disk low: {display}"),
    );
    // The numbers are interpolated with Python's `str(float)`, which is
    // `repr` — 90.0 prints as "90.0", not "90".
    notification.body = format!(
        "The host disk on CameraNode \"{display}\" is {pct}% full \
         ({free_gb} GB free of {total_gb} GB). Local recordings will fail to \
         write when the disk fills. Free up space (delete old recordings, \
         expand storage, or raise the recording retention cap on the node).",
        pct = crate::pyrepr::repr_float(pct_rounded),
        free_gb = crate::pyrepr::repr_float(free_gb),
        total_gb = crate::pyrepr::repr_float(total_gb),
    );
    notification.severity = "warning".to_string();
    notification.audience = "admin".to_string();
    notification.link = Some("/settings".to_string());
    notification.meta = Some(json!({
        "node_id": node.node_id,
        "percent_used": pct_rounded,
        "disk_free_bytes": free,
        "disk_total_bytes": total,
    }));
    create_notification(state, &node.org_id, notification).await;

    if let Err(err) = crate::settings::set(
        &state.pool,
        &node.org_id,
        &key,
        &crate::models::iso_naive(now_naive()),
    )
    .await
    {
        tracing::error!(error = %err, "failed to record disk-low emit time");
    }
}

/// `_resolve_org_timezone` — the org's IANA zone, defaulting to UTC.
///
/// A bad name falls back rather than raising: an exception here would
/// 500 every heartbeat for that org and stop its recording entirely.
async fn resolve_org_timezone(state: &AppState, org_id: &str) -> jiff::tz::TimeZone {
    let name = crate::settings::get(&state.pool, org_id, "timezone", Some("UTC"))
        .await
        .unwrap_or_default()
        .filter(|tz| !tz.is_empty())
        .unwrap_or_else(|| "UTC".to_string());
    match crate::zoneinfo::load(&name) {
        Ok(tz) => tz,
        Err(_) => {
            tracing::warn!(org_id, timezone = %name, "invalid timezone setting — falling back to UTC");
            crate::zoneinfo::load("UTC").unwrap_or(jiff::tz::TimeZone::UTC)
        }
    }
}

/// `_camera_should_record_now`.
///
/// Continuous wins outright. A schedule is inclusive of its start and
/// exclusive of its end, so `08:00–08:00` means never, not always, and
/// an end before the start is an overnight window.
fn camera_should_record_now(
    continuous: bool,
    scheduled: bool,
    start: Option<&str>,
    end: Option<&str>,
    cur_minutes: i64,
) -> bool {
    if continuous {
        return true;
    }
    if !scheduled {
        return false;
    }
    let (Some(start), Some(end)) = (start, end) else {
        return false;
    };
    if start.is_empty() || end.is_empty() {
        return false;
    }
    let (Some(start_minutes), Some(end_minutes)) = (hhmm_minutes(start), hhmm_minutes(end)) else {
        return false;
    };

    if start_minutes <= end_minutes {
        start_minutes <= cur_minutes && cur_minutes < end_minutes
    } else {
        cur_minutes >= start_minutes || cur_minutes < end_minutes
    }
}

/// `int(h) * 60 + int(m)` over a `"HH:MM"` split.
///
/// Python's `int()` accepts surrounding space, a sign and underscores,
/// and `split(":")` on a value with more than one colon yields three
/// parts — which unpacks into two names with a ValueError the caller
/// catches. Both end up as "do not record".
fn hhmm_minutes(value: &str) -> Option<i64> {
    let mut parts = value.split(':');
    let hours = parts.next()?;
    let minutes = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let hours = crate::pyint::python_int(hours)?.small()?;
    let minutes = crate::pyint::python_int(minutes)?.small()?;
    Some(hours * 60 + minutes)
}

#[cfg(test)]
mod tests {
    use super::{camera_should_record_now, hhmm_minutes};

    /// Continuous wins outright, and a camera with no policy records
    /// nothing. Neither reads the clock.
    #[test]
    fn continuous_beats_everything_and_off_means_off() {
        assert!(camera_should_record_now(true, false, None, None, 0));
        // Even against a window that excludes the current minute.
        assert!(camera_should_record_now(true, true, Some("01:00"), Some("02:00"), 600));
        assert!(!camera_should_record_now(false, false, Some("00:00"), Some("23:59"), 600));
    }

    /// A schedule with nothing configured is off, not always-on.
    #[test]
    fn a_schedule_without_times_records_nothing() {
        assert!(!camera_should_record_now(false, true, None, None, 600));
        assert!(!camera_should_record_now(false, true, Some("08:00"), None, 600));
        assert!(!camera_should_record_now(false, true, None, Some("17:00"), 600));
        // The empty string is what clearing a window stores.
        assert!(!camera_should_record_now(false, true, Some(""), Some("17:00"), 600));
    }

    #[test]
    fn a_daytime_window_includes_its_start_and_excludes_its_end() {
        let window = |m| camera_should_record_now(false, true, Some("08:00"), Some("17:00"), m);
        assert!(!window(7 * 60 + 59));
        assert!(window(8 * 60), "inclusive of the start");
        assert!(window(16 * 60 + 59));
        assert!(!window(17 * 60), "exclusive of the end");
    }

    /// An end before the start is an overnight window, not an empty
    /// one — 22:00–06:00 covers the night, both sides of midnight.
    #[test]
    fn an_end_before_the_start_wraps_around_midnight() {
        let night = |m| camera_should_record_now(false, true, Some("22:00"), Some("06:00"), m);
        assert!(night(22 * 60));
        assert!(night(23 * 60 + 59));
        assert!(night(0));
        assert!(night(5 * 60 + 59));
        assert!(!night(6 * 60), "exclusive of the end");
        assert!(!night(12 * 60));
    }

    /// Equal times mean never. This is the case the exclusive end
    /// exists for: read the other way round, an operator setting
    /// 08:00–08:00 would get continuous recording.
    #[test]
    fn a_zero_length_window_records_nothing() {
        for minute in [0, 8 * 60, 12 * 60, 23 * 60 + 59] {
            assert!(!camera_should_record_now(
                false,
                true,
                Some("08:00"),
                Some("08:00"),
                minute
            ));
        }
    }

    /// The column is validated at write time, but a hand-edited row is
    /// not. Python's `int()` raises on these and the caller catches it,
    /// so every one is "do not record" rather than a 500 on a heartbeat.
    #[test]
    fn an_unparseable_window_records_nothing() {
        for (start, end) in [
            ("abc", "17:00"),
            ("08:00", "xx:yy"),
            ("0800", "17:00"),
            ("08:00:00", "17:00"),
            ("", ""),
        ] {
            assert!(
                !camera_should_record_now(false, true, Some(start), Some(end), 600),
                "{start}–{end} should not record"
            );
        }
    }

    /// `int()` is not `str::parse`: it takes surrounding space, a plus
    /// sign and underscores. A row holding " 8": "00" is a window
    /// Python honours, so this one does too.
    #[test]
    fn hhmm_parses_what_python_int_parses() {
        assert_eq!(hhmm_minutes("08:00"), Some(480));
        assert_eq!(hhmm_minutes(" 8 : 00 "), Some(480));
        assert_eq!(hhmm_minutes("+8:00"), Some(480));
        assert_eq!(hhmm_minutes("0_8:00"), Some(480));
        // Out-of-range values are arithmetic, not an error — Python
        // does not range-check either.
        assert_eq!(hhmm_minutes("25:00"), Some(1500));
        // Three parts unpack into two names and raise.
        assert_eq!(hhmm_minutes("08:00:00"), None);
        assert_eq!(hhmm_minutes("0800"), None);
        assert_eq!(hhmm_minutes("abc:00"), None);
    }
}
