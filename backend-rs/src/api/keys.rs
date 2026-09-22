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
use crate::query::{int4, path_int, ModelBody};
use crate::ratelimit::PerHour;

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct KeyRow {
    pub(crate) id: i32,
    pub(crate) name: String,
    pub(crate) created_at: Option<NaiveDateTime>,
    pub(crate) last_used_at: Option<NaiveDateTime>,
    pub(crate) revoked: Option<bool>,
    pub(crate) scope_mode: Option<String>,
    pub(crate) scope_tools: Option<String>,
    pub(crate) kind: Option<String>,
}

impl KeyRow {
    pub(crate) fn to_json(&self) -> Value {
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
) -> Result<String, ApiError> {
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

    Ok(name)
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
    let key_id = int4(key_id)?;
    revoke(
        &state,
        &user,
        key_id,
        "integration",
        "integration_key_revoked",
        &headers,
        &peer.ip().to_string(),
    )
    .await?;
    Ok(Json(json!({ "success": true })))
}

/// `osi_` — the prefix that separates an integration key from an MCP
/// one, both of which live in `mcp_api_keys` and are split by `kind`.
const INTEGRATION_KEY_PREFIX: &str = "osi_";

/// What an admin reads when an integration key is created.
fn integration_key_create_body(actor: &str, name: &str) -> String {
    format!(
        "{actor} just created a new integration API key \"{name}\" (used to \
         connect tools like Home Assistant to your cameras). If this was you, \
         no action needed. If not, revoke it immediately."
    )
}

/// `POST /api/integration/keys` — mint one, return it once.
///
/// Admin-only but *not* billing-gated: the integration control plane is
/// free on every tier, and the video it proxies still inherits the
/// viewer-hour cap downstream.
///
/// **`scope_mode` is stored as `all`, not null**, which reading the
/// Python handler will not tell you: it passes `scope_mode=None`
/// explicitly, with a comment saying integration keys have no per-tool
/// scoping — and SQLAlchemy then applies the column's own
/// `default="all"`, because a `None` attribute at flush time is exactly
/// what a Python-side default is for. Writing the null the handler
/// appears to ask for is a difference only a side-effect comparison
/// sees; nothing reads `scope_mode` on an integration key, so no
/// response would ever have shown it.
pub async fn create_integration_key(
    rate: PerHour<10>,
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    ModelBody(RequireAdmin(user), body): ModelBody<RequireAdmin>,
) -> Result<Json<Value>, ApiError> {
    let mut errors = crate::query::BodyErrors::new();
    // `name: str = Field("Home Assistant", max_length=100)`.
    let name = match body.get("name") {
        None => "Home Assistant".to_string(),
        Some(Value::String(value)) => {
            if value.chars().count() > 100 {
                errors.too_long("name", value.as_str(), 100);
            }
            value.clone()
        }
        Some(other) => {
            errors.string_type("name", other);
            String::new()
        }
    };
    errors.finish()?;
    rate.check().await?;

    let raw_key = format!("{INTEGRATION_KEY_PREFIX}{}", crate::crypto::token_hex(16));
    let key_hash = crate::crypto::hex(&{ use sha2::Digest; sha2::Sha256::digest(raw_key.as_bytes()) });

    let (key_id, created_at): (i32, Option<NaiveDateTime>) = sqlx::query_as(
        "INSERT INTO mcp_api_keys
            (org_id, key_hash, name, kind, scope_mode, scope_tools, revoked, created_at)
         VALUES ($1, $2, $3, 'integration', 'all', NULL, false, $4)
         RETURNING id, created_at",
    )
    .bind(&user.org_id)
    .bind(&key_hash)
    .bind(&name)
    .bind(crate::models::now_naive())
    .fetch_one(&state.pool)
    .await?;

    let label = audit_label(&user);
    write_audit(
        &state.pool,
        &user.org_id,
        "integration_key_created",
        &user.user_id,
        &label,
        Some(python_json(&[("key_id", json!(key_id)), ("name", json!(name))])),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    let actor = [label, user.user_id.clone(), "unknown user".to_string()]
        .into_iter()
        .find(|candidate| !candidate.is_empty())
        .unwrap_or_default();
    crate::notifications::create_notification(
        &state,
        &user.org_id,
        crate::notifications::NewNotification::new(
            "integration_key_created",
            format!("New integration key created: {name}"),
        )
        .body(integration_key_create_body(&actor, &name))
        .severity("warning")
        .audience("admin")
        .link("/integrations")
        .meta(json!({
            "key_id": key_id,
            "key_name": name,
            "actor_user_id": user.user_id,
        })),
    )
    .await;

    Ok(Json(json!({
        "id": key_id,
        "name": name,
        // Returned once; only the hash is stored.
        "key": raw_key,
        "created_at": created_at.map(crate::models::iso_naive),
        "kind": "integration",
        "warning": "Save this key now. You won't be able to see it again.",
    })))
}

/// What an admin reads in the bell panel after a key is revoked.
///
/// Its own function because the Python builds it from four adjacent
/// f-strings, and the join points are exactly where a space goes
/// missing. It lands in a `Text` column the differential compares
/// literally, so the test below holds the assembled sentence rather
/// than trusting the line continuations.
fn revoke_notification_body(actor: &str, name: &str) -> String {
    format!(
        "{actor} just revoked the MCP API key \"{name}\". Any AI client \
         that was using this key will start receiving 401 errors on its \
         next request and will need to be reconfigured."
    )
}

/// `DELETE /api/mcp/keys/{key_id}`.
///
/// The same revoke, a different response shape, and a notification the
/// integration surface deliberately has none of: an MCP key is full
/// programmatic access to the org's cameras, so its lifecycle is a
/// security audit signal and admins are told about both ends of it.
pub async fn revoke_mcp_key(
    rate: PerHour<30>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    Path(key_id): Path<String>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
) -> Result<Json<Value>, ApiError> {
    let key_id = path_int("key_id", &key_id)?;
    rate.check().await?;
    let key_id = int4(key_id)?;
    let name = revoke(
        &state,
        &user,
        key_id,
        "mcp",
        "mcp_key_revoked",
        &headers,
        &peer.ip().to_string(),
    )
    .await?;

    // `audit_label(user) or user.user_id or "unknown user"` — the first
    // of the three that is not empty.
    let actor = [audit_label(&user), user.user_id.clone(), "unknown user".to_string()]
        .into_iter()
        .find(|candidate| !candidate.is_empty())
        .unwrap_or_default();
    crate::notifications::create_notification(
        &state,
        &user.org_id,
        crate::notifications::NewNotification::new(
            "mcp_key_revoked",
            format!("MCP API key revoked: {name}"),
        )
        .body(revoke_notification_body(&actor, &name))
        .severity("info")
        .audience("admin")
        .link("/admin/audit-log")
        .meta(json!({
            "key_id": key_id,
            "key_name": name,
            "actor_user_id": user.user_id,
        })),
    )
    .await;

    Ok(Json(json!({ "success": true, "revoked": key_id })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The copy an admin reads in the bell panel, which the Python
    /// builds from four adjacent f-strings — the join points are where
    /// a space goes missing, and it is a `Text` column the differential
    /// compares literally.
    #[test]
    fn the_revoke_notification_reads_as_one_sentence() {
        assert_eq!(
            revoke_notification_body("alice@example.com", "ci-bot"),
            "alice@example.com just revoked the MCP API key \"ci-bot\". \
             Any AI client that was using this key will start receiving 401 \
             errors on its next request and will need to be reconfigured."
        );
    }

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
