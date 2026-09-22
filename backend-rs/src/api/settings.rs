//! Read-only org settings.
//!
//! Ported from the GET handlers around `/api/settings` in
//! `backend/app/api/cameras.py`. Writes are slice 4.

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::audit::{audit_label, python_json, write_audit};
use crate::auth::{RequireAdmin, RequireView};
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

/// `POST /api/settings/danger/full-reset` — GDPR Article 17.
///
/// **No plan gate, deliberately.** Right-to-erasure is a legal
/// obligation and cannot sit behind a paid tier. Its sibling
/// `wipe-logs` *is* paid-only, because selective audit-log hygiene
/// that keeps the org running is an operator convenience and not an
/// obligation.
///
/// Three steps, and their order is the whole design. Each node is told
/// to wipe its own local data *first*, because after the rows are gone
/// there is no node id left to send to. Then the in-memory caches,
/// which no `DELETE` reaches. Then the cascade — committed before the
/// audit row is written, because the audit writer commits internally
/// and swallows a failure by rolling back, which with the erasure
/// still uncommitted would silently undo it while the handler returned
/// success. A false "your data was erased" on an Article 17 request is
/// the worst answer this endpoint could give.
pub async fn full_reset(
    rate: crate::ratelimit::PerHour<3>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    headers: axum::http::HeaderMap,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> Result<Json<Value>, ApiError> {
    rate.check().await?;

    let nodes: Vec<(i32, String)> =
        sqlx::query_as("SELECT id, node_id FROM camera_nodes WHERE org_id = $1")
            .bind(&user.org_id)
            .fetch_all(&state.pool)
            .await?;

    let mut nodes_wiped = 0i64;
    for (node_pk, node_id) in &nodes {
        // A node that is offline cannot acknowledge, and waiting for
        // one that may never come back is not a reason to leave a
        // customer's data in place.
        let acknowledged = matches!(
            crate::ws::MANAGER
                .send_command(
                    node_id,
                    "wipe_data",
                    serde_json::json!({}),
                    std::time::Duration::from_secs(10),
                )
                .await,
            Ok(result) if result.get("status") == Some(&Value::String("success".to_string()))
        );
        if acknowledged {
            nodes_wiped += 1;
        } else {
            tracing::warn!(node_id, "Could not send wipe_data to node");
        }

        let cameras: Vec<(String,)> =
            sqlx::query_as("SELECT camera_id FROM cameras WHERE node_id = $1")
                .bind(node_pk)
                .fetch_all(&state.pool)
                .await?;
        for (camera_id,) in cameras {
            state.hls.cleanup_camera(&camera_id);
        }
    }

    let mut tx = state.pool.begin().await?;
    let counts = crate::api::gdpr::delete_org_data(&mut tx, &user.org_id).await?;
    tx.commit().await?;

    let lookup = |table: &str| {
        counts
            .iter()
            .find(|(name, _)| *name == table)
            .map_or(0, |(_, count)| *count)
    };
    // The dashboard reads these five; the full breakdown goes to the
    // audit row.
    let results = json!({
        "success": true,
        "nodes_wiped": nodes_wiped,
        "nodes_deleted": lookup("camera_nodes"),
        "cameras_deleted": lookup("cameras"),
        "logs_deleted": lookup("stream_access_logs"),
        "mcp_logs_deleted": lookup("mcp_activity_logs"),
        "settings_deleted": lookup("settings"),
    });

    tracing::warn!(?counts, "Admin performed FULL RESET (org redacted)");
    let details: Vec<(&str, Value)> = counts
        .iter()
        .map(|(table, count)| (*table, json!(count)))
        .collect();
    write_audit(
        &state.pool,
        &user.org_id,
        "full_reset",
        &user.user_id,
        &audit_label(&user),
        Some(python_json(&details)),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    Ok(Json(results))
}
