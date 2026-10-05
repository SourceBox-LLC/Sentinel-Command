//! Per-camera recording controls.
//!
//! Ported from `backend/app/api/cameras.py`. Both routes flip fields on
//! the `cameras` row and audit the change; the heartbeat handler
//! reconciles the node from those fields within ~30 seconds, so there is
//! no WebSocket command and no in-process state behind either of them.
//!
//! The integration-key twin of the first route
//! (`POST /api/integration/cameras/{id}/recording`) is **not** ported: it
//! authenticates with `require_integration_org`, a scheme this crate does
//! not implement yet.

use axum::extract::{ConnectInfo, Path, State};
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::audit::{audit_label, python_json, write_audit};
use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::models::now_naive;
use crate::query::{parse_handler_json, path_segment, BodyErrors, ModelBody};
use crate::ratelimit::PerMinute;

/// Fetch a camera's current recording fields, scoped to the caller's org.
async fn owned_camera(
    pool: &crate::db::Pool,
    org_id: &str,
    camera_id: &str,
) -> Result<(bool, bool, Option<String>, Option<String>), ApiError> {
    sqlx::query_as(
        "SELECT continuous_24_7, scheduled_recording, scheduled_start, scheduled_end
           FROM cameras WHERE camera_id = $1 AND org_id = $2",
    )
    .bind(camera_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::not_found("Camera not found"))
}

/// `POST /api/cameras/{camera_id}/recording` — the dashboard's record
/// button.
///
/// A thin wrapper over `continuous_24_7`: the heartbeat reconciler picks
/// the change up on its next tick, which is why there is no WebSocket
/// command here and nothing to lose when a node restarts.
pub async fn toggle_recording(
    rate: PerMinute<30>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    Path(camera_id): Path<String>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let camera_id = path_segment(&camera_id)?;
    // Before the body: toggle_recording reads it with `await
    // request.json()` inside the function, after the decorator.
    rate.check().await?;
    let body = Value::Object(parse_handler_json(&body)?);
    // `bool(body.get("recording", False))` — Python truthiness over
    // whatever arrived, so a missing key is "stop recording".
    let recording = truthy(body.get("recording"));

    let current = owned_camera(&state.pool, &user.org_id, camera_id).await?;

    // SQLAlchemy emits no UPDATE when the value is unchanged, so
    // `updated_at` must not move for a press that changes nothing.
    if current.0 != recording {
        sqlx::query(
            "UPDATE cameras SET continuous_24_7 = $1, updated_at = $2
              WHERE camera_id = $3 AND org_id = $4",
        )
        .bind(recording)
        .bind(now_naive())
        .bind(camera_id)
        .bind(&user.org_id)
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
        ])),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    Ok(Json(json!({
        "success": true,
        "camera_id": camera_id,
        "recording": recording,
    })))
}

/// Python's `bool(...)` over an arbitrary JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `PATCH /api/cameras/{camera_id}/recording-settings`.
///
/// Every field is optional so a PATCH can flip one toggle without
/// re-asserting the others.
pub async fn update_recording_policy(
    rate: PerMinute<30>,
    State(state): State<AppState>,
    Path(camera_id): Path<String>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    ModelBody(RequireAdmin(user), body): ModelBody<RequireAdmin>,
) -> Result<Json<Value>, ApiError> {
    let camera_id = path_segment(&camera_id)?;

    let mut errors = BodyErrors::new();
    let continuous = errors.optional_bool(&body, "continuous_24_7");
    let scheduled = errors.optional_bool(&body, "scheduled_recording");
    let start = errors.optional_hhmm(&body, "scheduled_start");
    let end = errors.optional_hhmm(&body, "scheduled_end");
    errors.finish()?;
    rate.check().await?;

    let current = owned_camera(&state.pool, &user.org_id, camera_id).await?;

    // Validate the *resulting* state, not the patch, so the row can
    // never reach an impossible combination even via a direct API call.
    let next_continuous = continuous.unwrap_or(current.0);
    let next_scheduled = scheduled.unwrap_or(current.1);

    // At most one recording mode at a time. The heartbeat treats
    // `continuous OR (scheduled AND in-window)`, so both-true silently
    // makes the schedule a no-op — confusing enough to be worth a 422.
    if next_continuous && next_scheduled {
        return Err(ApiError::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "continuous_24_7 and scheduled_recording cannot both be true. \
             Pick one mode — continuous OR scheduled — or turn the existing \
             one off in the same PATCH.",
        ));
    }

    // `camera.scheduled_start = data.scheduled_start or None` — an empty
    // string clears the window rather than storing "".
    let next_start = match start {
        Some(ref s) => Some(s.clone()).filter(|s| !s.is_empty()),
        None => current.2.clone(),
    };
    let next_end = match end {
        Some(ref s) => Some(s.clone()).filter(|s| !s.is_empty()),
        None => current.3.clone(),
    };

    // A window from a time to the same time contains no minute at all —
    // the heartbeat's `[start, end)` check is false all day — so a camera
    // set to 08:00–08:00 silently never recorded. Refused, like the
    // mode conflict above, on the state the row would end up in.
    if next_scheduled && next_start.is_some() && next_start == next_end {
        return Err(ApiError::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "scheduled_start and scheduled_end must differ — a window that \
             starts and ends at the same time never records.",
        ));
    }

    let unchanged = next_continuous == current.0
        && next_scheduled == current.1
        && next_start == current.2
        && next_end == current.3;

    if !unchanged {
        sqlx::query(
            "UPDATE cameras
                SET continuous_24_7 = $1, scheduled_recording = $2,
                    scheduled_start = $3, scheduled_end = $4, updated_at = $5
              WHERE camera_id = $6 AND org_id = $7",
        )
        .bind(next_continuous)
        .bind(next_scheduled)
        .bind(&next_start)
        .bind(&next_end)
        .bind(now_naive())
        .bind(camera_id)
        .bind(&user.org_id)
        .execute(&state.pool)
        .await?;
    }

    write_audit(
        &state.pool,
        &user.org_id,
        "camera_recording_policy_updated",
        &user.user_id,
        &audit_label(&user),
        Some(python_json(&[
            ("camera_id", json!(camera_id)),
            ("continuous_24_7", json!(next_continuous)),
            ("scheduled_recording", json!(next_scheduled)),
            ("scheduled_start", json!(next_start)),
            ("scheduled_end", json!(next_end)),
        ])),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    Ok(Json(json!({
        "success": true,
        "camera_id": camera_id,
        "recording_policy": {
            "continuous_24_7": next_continuous,
            "scheduled_recording": next_scheduled,
            "scheduled_start": next_start,
            "scheduled_end": next_end,
        },
    })))
}
