//! Read-only org settings.
//!
//! Ported from the GET handlers around `/api/settings` in
//! `backend/app/api/cameras.py`. Writes are slice 4.

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::auth::RequireView;
use crate::error::ApiError;
use crate::settings;

/// Notification toggles default to on, for back-compat with orgs that
/// existed before the settings UI: the gate only starts filtering once
/// an admin explicitly turns something off.
const NOTIFICATION_KEYS: [&str; 3] = [
    "motion_notifications",
    "camera_transition_notifications",
    "node_transition_notifications",
];

/// Fetch the three notification toggles.
///
/// The comparison is `== "true"` exactly, matching Python — a stored
/// `"True"` or `"TRUE"` reads as off. That is worth preserving rather
/// than improving, because the value is written by the same service and
/// a looser read here would disagree with the Python still serving the
/// write path.
async fn notification_flags(
    pool: &sqlx::PgPool,
    org_id: &str,
) -> Result<serde_json::Map<String, Value>, ApiError> {
    let mut out = serde_json::Map::new();
    for key in NOTIFICATION_KEYS {
        let value = settings::get(pool, org_id, key, Some("true")).await?;
        out.insert(key.to_string(), json!(value.as_deref() == Some("true")));
    }
    Ok(out)
}

/// `GET /api/settings` — notifications plus the org timezone.
pub async fn get_all_settings(
    State(state): State<AppState>,
    RequireView(user): RequireView,
) -> Result<Json<Value>, ApiError> {
    let notifications = notification_flags(&state.pool, &user.org_id).await?;

    // `Setting.get(...) or "UTC"` in Python, so a row holding NULL or an
    // empty string also falls back rather than serving an empty zone —
    // the heartbeat handler parses this to decide when scheduled
    // recording fires, and an empty zone there would break the schedule.
    let timezone = settings::get(&state.pool, &user.org_id, "timezone", Some("UTC"))
        .await?
        .filter(|tz| !tz.is_empty())
        .unwrap_or_else(|| "UTC".to_string());

    Ok(Json(json!({
        "notifications": notifications,
        "timezone": timezone,
    })))
}

/// `GET /api/settings/notifications`.
pub async fn get_notification_settings(
    State(state): State<AppState>,
    RequireView(user): RequireView,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(Value::Object(
        notification_flags(&state.pool, &user.org_id).await?,
    )))
}

/// `GET /api/settings/motion-ingestion` — the server-side ingestion kill
/// switch, a safety valve for a runaway sensor flooding events.
///
/// Note the comparison differs from its siblings above: Python lowercases
/// this one before testing it, so `"TRUE"` counts as enabled here and as
/// disabled for the notification toggles. Inconsistent, but it is the
/// behaviour the stored data was written against.
pub async fn get_motion_ingestion(
    State(state): State<AppState>,
    RequireView(user): RequireView,
) -> Result<Json<Value>, ApiError> {
    let raw = settings::get(
        &state.pool,
        &user.org_id,
        "motion_ingestion_enabled",
        Some("true"),
    )
    .await?;

    // Python calls `.lower()` on the result unguarded, so a row whose
    // value is NULL raises AttributeError and returns 500. Defaulting to
    // enabled instead would silently re-open a kill switch an operator
    // set, so this treats an unreadable value as *disabled* — the safe
    // direction for a valve whose whole purpose is to stop a flood.
    // See tests/differential/expected_divergences.md.
    let enabled = match raw {
        Some(v) => v.to_lowercase() == "true",
        None => false,
    };

    Ok(Json(json!({ "motion_ingestion_enabled": enabled })))
}
