//! MCP and integration API keys — the list and revoke routes.
//!
//! Ported from `backend/app/api/mcp_keys.py` and `app/api/integration.py`.
//! Both key kinds live in one table and are separated by `kind`, which
//! the Python is careful about: "an MCP key id passed here 404s rather
//! than crossing surfaces".
//!
//! **Creation is not ported.** Both create routes mint a secret and fire
//! a `create_notification` — an inbox entry plus an email telling admins
//! a key was created, which is a security signal, not a nicety. That
//! belongs with the email work in slice 7. Porting the response without
//! the notification would look right in a differential and quietly stop
//! telling anyone that a key had appeared.
//!
//! `POST /api/mcp/keys` additionally validates `scope_tools` against
//! `MCP_ALL_TOOLS`, which is defined in the Python MCP server this crate
//! does not own.

use axum::extract::{ConnectInfo, Path, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::NaiveDateTime;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::audit::{audit_label, python_json, write_audit};
use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::models::iso_naive;
use crate::query::path_int;
use crate::ratelimit::PerHour;

#[derive(Debug, sqlx::FromRow)]
struct KeyRow {
    id: i32,
    name: String,
    created_at: Option<NaiveDateTime>,
    last_used_at: Option<NaiveDateTime>,
    revoked: Option<bool>,
    scope_mode: Option<String>,
    scope_tools: Option<String>,
    kind: Option<String>,
}

impl KeyRow {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "created_at": self.created_at.map(iso_naive),
            "last_used_at": self.last_used_at.map(iso_naive),
            "revoked": self.revoked,
            // A NULL scope_mode behaves like "all" so legacy rows keep
            // working; same for a NULL kind, which predates the column.
            "scope_mode": self.scope_mode.clone().unwrap_or_else(|| "all".to_string()),
            "scope_tools": scope_tools(self.scope_tools.as_deref()),
            "kind": self.kind.clone().unwrap_or_else(|| "mcp".to_string()),
        })
    }
}

/// Parse the stored JSON list, or `[]` for anything unusable.
///
/// Python stringifies each element rather than requiring strings, so a
/// stored `[1, 2]` comes back as `["1", "2"]` — reproduced rather than
/// tightened, because the value is already in the database.
fn scope_tools(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw.filter(|r| !r.is_empty()) else {
        return Vec::new();
    };
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => python_str(other),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `str(v)` for the JSON scalars that can appear in a stored list.
fn python_str(value: &Value) -> String {
    match value {
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

const KEY_SELECT: &str = "SELECT id, name, created_at, last_used_at, revoked, \
                          scope_mode, scope_tools, kind FROM mcp_api_keys";

async fn list_keys(
    state: &AppState,
    org_id: &str,
    kind: &str,
) -> Result<Json<Value>, ApiError> {
    let rows: Vec<KeyRow> = sqlx::query_as(&format!(
        "{KEY_SELECT} WHERE org_id = $1 AND revoked = false AND kind = $2 \
         ORDER BY created_at DESC"
    ))
    .bind(org_id)
    .bind(kind)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Value::Array(
        rows.iter().map(KeyRow::to_json).collect(),
    )))
}

/// `GET /api/mcp/keys`.
pub async fn list_mcp_keys(
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
) -> Result<Json<Value>, ApiError> {
    list_keys(&state, &user.org_id, "mcp").await
}

/// `GET /api/integration/keys`.
pub async fn list_integration_keys(
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
) -> Result<Json<Value>, ApiError> {
    list_keys(&state, &user.org_id, "integration").await
}

/// Revoke one key of a given kind.
///
/// The `kind` filter is the point: an MCP key id passed to the
/// integration endpoint must 404, not revoke a key on the other surface.
async fn revoke(
    state: &AppState,
    user: &crate::auth::AuthUser,
    key_id: i32,
    kind: &str,
    event: &str,
    headers: &HeaderMap,
    peer: &str,
) -> Result<Json<Value>, ApiError> {
    let row: Option<(String, Option<bool>)> = sqlx::query_as(
        "SELECT name, revoked FROM mcp_api_keys WHERE id = $1 AND org_id = $2 AND kind = $3",
    )
    .bind(key_id)
    .bind(&user.org_id)
    .bind(kind)
    .fetch_optional(&state.pool)
    .await?;

    let Some((name, revoked)) = row else {
        return Err(ApiError::not_found("Key not found"));
    };

    // SQLAlchemy emits no UPDATE when the value is unchanged, so
    // re-revoking an already-revoked key must not move `updated_at`-like
    // state. This table has no such column, but the same rule keeps the
    // write count identical, which the side-effect differential sees.
    if revoked != Some(true) {
        sqlx::query("UPDATE mcp_api_keys SET revoked = true WHERE id = $1 AND org_id = $2")
            .bind(key_id)
            .bind(&user.org_id)
            .execute(&state.pool)
            .await?;
    }

    write_audit(
        &state.pool,
        &user.org_id,
        event,
        &user.user_id,
        &audit_label(user),
        Some(python_json(&[
            ("key_id", json!(key_id)),
            ("name", json!(name)),
        ])),
        headers,
        Some(peer),
    )
    .await;

    Ok(Json(json!({ "success": true })))
}

/// `DELETE /api/integration/keys/{key_id}`.
pub async fn revoke_integration_key(
    // Python: @limiter.limit("30/hour"). An hour window, not a minute —
    // the parity checker refused this route until the limiter could
    // express one, because a minute window here is sixty times the
    // intended budget.
    rate: PerHour<30>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    Path(key_id): Path<String>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
) -> Result<Json<Value>, ApiError> {
    let key_id = path_int("key_id", &key_id)?;
    rate.check().await?;
    revoke(
        &state,
        &user,
        key_id,
        "integration",
        "integration_key_revoked",
        &headers,
        &peer.ip().to_string(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_or_unparseable_scope_list_is_empty() {
        for raw in [None, Some(""), Some("not json"), Some("{}"), Some("null"), Some("5")] {
            assert!(scope_tools(raw).is_empty(), "{raw:?}");
        }
    }

    #[test]
    fn a_stored_list_comes_back_as_strings() {
        assert_eq!(
            scope_tools(Some(r#"["list_cameras","get_camera"]"#)),
            vec!["list_cameras", "get_camera"]
        );
    }

    #[test]
    fn non_string_elements_are_stringified_as_python_does() {
        // Python does `[str(v) for v in val]`, so a stored number or
        // bool survives rather than failing the parse. Reproduced
        // because the value is already in the database.
        assert_eq!(
            scope_tools(Some(r#"[1, true, null, "x"]"#)),
            vec!["1", "True", "None", "x"]
        );
    }
}
