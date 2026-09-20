//! `POST /api/settings/timezone` — the org's IANA zone.
//!
//! Ported from `update_org_timezone` in `backend/app/api/cameras.py`.
//! The heartbeat handler reads this to interpret a camera's HH:MM
//! recording window in the operator's local time rather than UTC.
//!
//! Validity is `crate::zoneinfo::is_available` — the tzdata package's
//! list plus the system zoneinfo directories, as
//! `zoneinfo.available_timezones()` has it — not a Rust timezone
//! crate's idea of which names exist. On the Debian image that set
//! includes `localtime`, and a crate's would not.

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::audit::{audit_label, python_json, write_audit};
use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::pyrepr;
use crate::query::parse_handler_json;
use crate::ratelimit::PerMinute;

pub async fn update_org_timezone(
    rate: PerMinute<30>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    // `await request.json()` is inside the Python function, so the
    // slot is spent before the body is read.
    rate.check().await?;
    let body = parse_handler_json(&body)?;

    // `(body.get("timezone") or "").strip()`: a falsy value becomes "",
    // and a truthy non-string has no .strip() — AttributeError, a 500.
    let raw = body.get("timezone").cloned().unwrap_or(Value::Null);
    let tz_name = if !pyrepr::truthy(&raw) {
        String::new()
    } else if let Value::String(s) = &raw {
        python_strip(s).to_string()
    } else {
        return Err(ApiError::internal("timezone is not a string"));
    };

    // Both refusals are HTTPException(422) with a plain string detail —
    // not the validation envelope a Pydantic 422 carries.
    if tz_name.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "`timezone` is required (IANA name like 'America/Los_Angeles' or 'UTC')",
        ));
    }
    if !crate::zoneinfo::is_available(&tz_name) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "Unknown timezone {}. Use an IANA name like 'America/Los_Angeles', \
                 'Europe/London', or 'UTC'.",
                pyrepr::repr_str(&tz_name)
            ),
        ));
    }

    crate::settings::set(&state.pool, &user.org_id, "timezone", &tz_name).await?;
    write_audit(
        &state.pool,
        &user.org_id,
        "timezone_updated",
        &user.user_id,
        &audit_label(&user),
        Some(python_json(&[("timezone", json!(tz_name))])),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    Ok(Json(json!({ "success": true, "timezone": tz_name })))
}

/// `str.strip()`: Unicode whitespace plus the C0 separators.
fn python_strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}
