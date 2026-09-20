//! Camera-group writes and the two settings toggles.
//!
//! Ported from `backend/app/api/cameras.py`. `POST /api/settings/timezone`
//! is **not** ported: it validates against `zoneinfo.available_timezones()`,
//! which reads the system tzdata, and a Rust port would have to match that
//! set exactly or start rejecting zones the Python accepts.

use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::audit::{audit_label, python_json, write_audit};
use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::models::now_naive;
use crate::pyint::PyInt;
use crate::query::{int4, parse_handler_json, path_int, BodyErrors, ModelBody, Query, path_segment};
use crate::ratelimit::PerMinute;
use crate::settings;

/// Defaults from `CameraGroupCreate`.
const DEFAULT_COLOR: &str = "#22c55e";
const DEFAULT_ICON: &str = "📁";

/// `POST /api/camera-groups`.
pub async fn create_camera_group(
    rate: PerMinute<20>,
    State(state): State<AppState>,
    ModelBody(RequireAdmin(user), body): ModelBody<RequireAdmin>,
) -> Result<Json<Value>, ApiError> {
    // Field lengths come from CameraGroupCreate. They are enforced here
    // rather than left to the column widths, because a varchar overflow
    // is a 500 from the database where Pydantic returns a 422 naming the
    // field.
    let mut errors = BodyErrors::new();
    let name = errors.required_string(&body, "name", 100);
    let color_field = errors.optional_string(&body, "color", 20);
    let icon_field = errors.optional_string(&body, "icon", 10);
    errors.finish()?;
    rate.check().await?;

    // Name uniqueness is per-org and checked in the handler, not by a
    // constraint — two orgs may both have an "Outdoor".
    let existing: Option<(i32,)> =
        sqlx::query_as("SELECT id FROM camera_groups WHERE org_id = $1 AND name = $2 LIMIT 1")
            .bind(&user.org_id)
            .bind(&name)
            .fetch_optional(&state.pool)
            .await?;
    if existing.is_some() {
        return Err(ApiError::bad_request("Group name already exists"));
    }

    // Pydantic substitutes its default when the field is absent *or*
    // explicitly null, because the annotations are Optional with a
    // non-None default.
    let color = color_field.unwrap_or_else(|| DEFAULT_COLOR.to_string());
    let icon = icon_field.unwrap_or_else(|| DEFAULT_ICON.to_string());

    let row: (i32, String) = sqlx::query_as(
        "INSERT INTO camera_groups (org_id, name, color, icon, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $5) RETURNING id, name",
    )
    .bind(&user.org_id)
    .bind(&name)
    .bind(&color)
    .bind(&icon)
    .bind(now_naive())
    .fetch_one(&state.pool)
    .await?;

    Ok(Json(json!({ "success": true, "id": row.0, "name": row.1 })))
}

/// `DELETE /api/camera-groups/{group_id}`.
pub async fn delete_camera_group(
    rate: PerMinute<60>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    Path(group_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let group_id = path_int("group_id", &group_id)?;
    rate.check().await?;
    let group_id = int4(group_id)?;

    let group: Option<(String,)> =
        sqlx::query_as("SELECT name FROM camera_groups WHERE id = $1 AND org_id = $2")
            .bind(group_id)
            .bind(&user.org_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some((name,)) = group else {
        return Err(ApiError::not_found("Group not found"));
    };

    // Python clears each member's group_id through the ORM before
    // deleting, which fires `onupdate` on every affected camera. The
    // schema has no ON DELETE SET NULL behind this, so the cameras must
    // be updated explicitly — and their `updated_at` must move, because
    // the data-sync tier selects on it to find changed rows.
    sqlx::query("UPDATE cameras SET group_id = NULL, updated_at = $1 WHERE group_id = $2")
        .bind(now_naive())
        .bind(group_id)
        .execute(&state.pool)
        .await?;

    sqlx::query("DELETE FROM camera_groups WHERE id = $1 AND org_id = $2")
        .bind(group_id)
        .bind(&user.org_id)
        .execute(&state.pool)
        .await?;

    Ok(Json(json!({ "success": true, "deleted": name })))
}

/// `PUT /api/cameras/{camera_id}/group`.
///
/// `group_id` is a **query** parameter, not a body field — it is a bare
/// `int = None` in the Python signature, which FastAPI reads from the
/// query string.
pub async fn assign_camera_group(
    rate: PerMinute<60>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    Path(camera_id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let camera_id = path_segment(&camera_id)?;
    let mut q = Query::parse(request.uri().query());
    // No bounds in the Python signature, so no bounds here — and with
    // none, a value beyond i64 gets through validation the way it does
    // in Python, to be answered for below.
    let raw_group_id = q.optional_str("group_id");
    let group_id = match raw_group_id {
        Some(ref raw) if !raw.is_empty() => {
            Some(q.big_int("group_id", PyInt::Small(0), None, None))
        }
        Some(_) => None,
        None => None,
    };
    q.finish()?;
    rate.check().await?;

    let camera: Option<(i32,)> =
        sqlx::query_as("SELECT id FROM cameras WHERE camera_id = $1 AND org_id = $2")
            .bind(camera_id)
            .bind(&user.org_id)
            .fetch_optional(&state.pool)
            .await?;
    if camera.is_none() {
        return Err(ApiError::not_found("Camera not found"));
    }

    // `if group_id:` in Python — a falsy value takes the unassign path,
    // so `?group_id=0` clears the group rather than looking for group 0.
    // The response still echoes the value that was sent.
    let assign = match group_id {
        Some(id) if id.truthy() => {
            // Narrowed to the column, which is what SQLAlchemy binds:
            // `group_id=4294967297` is a 500 from Postgres, not a miss.
            // Narrowing it here in Rust instead would wrap it round to
            // group 1 and quietly reassign the camera to a real group.
            let group: Option<(i32,)> =
                sqlx::query_as("SELECT id FROM camera_groups WHERE id = $1 AND org_id = $2")
                    .bind(int4(id)?)
                    .bind(&user.org_id)
                    .fetch_optional(&state.pool)
                    .await?;
            let Some((group_id,)) = group else {
                return Err(ApiError::not_found("Group not found"));
            };
            // The id the row actually has, so what is written back is
            // whatever the lookup matched.
            Some(group_id)
        }
        _ => None,
    };

    // Same dirty-check rule as the incident patch: SQLAlchemy emits no
    // UPDATE when the value is unchanged, so `updated_at` must not move
    // for a no-op reassignment.
    let current: (Option<i32>,) = sqlx::query_as("SELECT group_id FROM cameras WHERE camera_id = $1")
        .bind(camera_id)
        .fetch_one(&state.pool)
        .await?;
    if current.0 != assign {
        sqlx::query("UPDATE cameras SET group_id = $1, updated_at = $2 WHERE camera_id = $3 AND org_id = $4")
            .bind(assign)
            .bind(now_naive())
            .bind(camera_id)
            .bind(&user.org_id)
            .execute(&state.pool)
            .await?;
    }

    Ok(Json(json!({
        "success": true,
        "camera_id": camera_id,
        // A value beyond i64 never reaches here: it is truthy, so it
        // took the lookup above and found no group.
        "group_id": group_id.and_then(PyInt::small),
    })))
}

/// `POST /api/settings/motion-ingestion` — the ingestion kill switch.
pub async fn update_motion_ingestion(
    rate: PerMinute<30>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    // Before the body: this handler reads it with `await request.json()`
    // inside the function, after the decorator.
    rate.check().await?;
    let body = Value::Object(parse_handler_json(&body)?);
    // `bool(payload.get("enabled"))` — Python truthiness, so any
    // non-empty string, any non-zero number and any non-empty container
    // all mean enabled, and an absent key means disabled.
    let enabled = match body.get("enabled") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    };

    settings::set(
        &state.pool,
        &user.org_id,
        "motion_ingestion_enabled",
        if enabled { "true" } else { "false" },
    )
    .await?;

    write_audit(
        &state.pool,
        &user.org_id,
        "motion_ingestion_toggled",
        &user.user_id,
        &audit_label(&user),
        Some(python_json(&[("enabled", json!(enabled))])),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    Ok(Json(json!({ "motion_ingestion_enabled": enabled })))
}

/// `POST /api/settings/notifications`.
pub async fn update_notification_settings(
    rate: PerMinute<30>,
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    ModelBody(RequireAdmin(user), body): ModelBody<RequireAdmin>,
) -> Result<Json<Value>, ApiError> {
    // All three default to on, for back-compat with orgs that predate
    // the settings UI.
    let mut errors = BodyErrors::new();
    let motion = errors.bool_with_default(&body, "motion_notifications", true);
    let camera = errors.bool_with_default(&body, "camera_transition_notifications", true);
    let node = errors.bool_with_default(&body, "node_transition_notifications", true);
    errors.finish()?;
    rate.check().await?;
    // `str(bool).lower()` in Python — "true" / "false", which is what
    // the GET side compares against with a bare `==`.
    for (key, value) in [
        ("motion_notifications", motion),
        ("camera_transition_notifications", camera),
        ("node_transition_notifications", node),
    ] {
        settings::set(
            &state.pool,
            &user.org_id,
            key,
            if value { "true" } else { "false" },
        )
        .await?;
    }

    write_audit(
        &state.pool,
        &user.org_id,
        "notification_settings_updated",
        &user.user_id,
        &audit_label(&user),
        Some(python_json(&[
            ("motion_notifications", json!(motion)),
            ("camera_transition_notifications", json!(camera)),
            ("node_transition_notifications", json!(node)),
        ])),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    // The handler returns only {"success": true} — the toggles are not
    // echoed back, and the SPA re-reads them from GET /api/settings.
    Ok(Json(json!({ "success": true })))
}
