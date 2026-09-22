//! Read-only camera routes.
//!
//! Ported from the GET handlers in `backend/app/api/cameras.py`. Writes
//! are slice 4 and still proxy to Python.
//!
//! Every query here filters on `org_id` from the verified token. That is
//! the whole tenant isolation story for these routes — there is no row
//! level security behind it, so an unscoped query is a cross-tenant leak.

use axum::extract::{Path, State};
use axum::Json;
use serde_json::Value;

use crate::app::AppState;
use crate::auth::RequireView;
use crate::error::ApiError;
use crate::query::path_segment;
use crate::models::{CameraGroupRow, CameraRow, CAMERA_SELECT};

/// `GET /api/cameras` — every camera in the caller's organisation.
///
/// No ORDER BY, matching the Python's `filter_by(...).all()`. Adding one
/// would be a behaviour change while both stacks serve traffic; the
/// differential harness sorts before comparing instead.
pub async fn list_cameras(
    State(state): State<AppState>,
    RequireView(user): RequireView,
) -> Result<Json<Vec<Value>>, ApiError> {
    let rows: Vec<CameraRow> = sqlx::query_as(&format!("{CAMERA_SELECT} WHERE c.org_id = $1"))
        .bind(&user.org_id)
        .fetch_all(&state.pool)
        .await?;

    Ok(Json(rows.iter().map(CameraRow::to_json).collect()))
}

/// `GET /api/cameras/{camera_id}`.
///
/// `camera_id` is globally unique, but the org filter stays: without it
/// this would return any tenant's camera to any authenticated caller.
pub async fn get_camera(
    State(state): State<AppState>,
    RequireView(user): RequireView,
    Path(camera_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let camera_id = path_segment(&camera_id)?;
    let row: Option<CameraRow> =
        sqlx::query_as(&format!("{CAMERA_SELECT} WHERE c.camera_id = $1 AND c.org_id = $2"))
            .bind(camera_id)
            .bind(&user.org_id)
            .fetch_optional(&state.pool)
            .await?;

    // 404 whether the camera does not exist or belongs to someone else —
    // distinguishing them would confirm the existence of another
    // tenant's camera to anyone who can guess an id.
    let row = row.ok_or_else(|| ApiError::not_found("Camera not found"))?;
    Ok(Json(row.to_json()))
}

/// `GET /api/camera-groups`.
pub async fn list_camera_groups(
    State(state): State<AppState>,
    RequireView(user): RequireView,
) -> Result<Json<Vec<Value>>, ApiError> {
    // Python's `len(self.cameras)` loads the group's cameras to count
    // them; a correlated count does the same job in the query. Cameras
    // are counted without an org filter of their own because a group is
    // already org-scoped and a camera cannot join a group from another
    // organisation.
    let rows: Vec<CameraGroupRow> = sqlx::query_as(
        r#"
        SELECT g.id, g.name, g.color, g.icon,
               (SELECT COUNT(*) FROM cameras c WHERE c.group_id = g.id) AS camera_count
          FROM camera_groups g
         WHERE g.org_id = $1
        "#,
    )
    .bind(&user.org_id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(rows.iter().map(CameraGroupRow::to_json).collect()))
}

/// `POST /api/cameras/{camera_id}/snapshot` — ask the node to capture
/// one and keep it locally.
///
/// The only route whose answer is a *node's*, relayed. That makes the
/// failure modes its own: the node may be unassigned, may not hold a
/// socket to this machine, may never answer, or may go away
/// mid-command — 400, 503, 504 and 503 respectively, each carrying the
/// message the Python's exception carried.
pub async fn take_snapshot(
    rate: crate::ratelimit::PerMinute<30>,
    State(state): State<AppState>,
    RequireView(user): RequireView,
    Path(camera_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    rate.check().await?;
    let camera_id = path_segment(&camera_id)?;

    let camera: Option<(Option<i32>,)> =
        sqlx::query_as("SELECT node_id FROM cameras WHERE camera_id = $1 AND org_id = $2")
            .bind(camera_id)
            .bind(&user.org_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some((node_pk,)) = camera else {
        return Err(ApiError::not_found("Camera not found"));
    };
    // `if not camera.node_id` — an unassigned camera has nobody to ask,
    // and a zero primary key is as falsy as a null one.
    let Some(node_pk) = node_pk.filter(|pk| *pk != 0) else {
        return Err(ApiError::bad_request("Camera has no assigned node"));
    };

    let node: Option<(String,)> =
        sqlx::query_as("SELECT node_id FROM camera_nodes WHERE id = $1")
            .bind(node_pk)
            .fetch_optional(&state.pool)
            .await?;
    let Some((node_id,)) = node else {
        return Err(ApiError::bad_request("Camera node not found"));
    };
    if !crate::ws::MANAGER.is_connected(&node_id) {
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Camera node is offline",
        ));
    }

    match crate::ws::MANAGER
        .send_command(
            &node_id,
            "take_snapshot",
            serde_json::json!({"camera_id": camera_id}),
            std::time::Duration::from_secs(15),
        )
        .await
    {
        Ok(result) => Ok(Json(result)),
        Err(crate::ws::CommandError::Timeout { .. }) => Err(ApiError::new(
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            "Snapshot request timed out",
        )),
        // Every other failure is a `ValueError` in the Python, and the
        // route puts `str(e)` straight into the body.
        Err(other) => Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            other.to_string(),
        )),
    }
}
