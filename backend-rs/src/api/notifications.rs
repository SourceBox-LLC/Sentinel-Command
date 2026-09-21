//! The notification inbox.
//!
//! Ported from `backend/app/api/notifications.py` — the reads and the
//! three writes that touch nothing but the database.
//!
//! `POST /request-admin-promotion` is **not** ported: it sends email.
//! Neither is `/stream`, which is Server-Sent Events over an in-process
//! broadcaster.

use axum::extract::{ConnectInfo, Request, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::{NaiveDateTime};
use serde_json::{json, Map, Value};

use crate::app::AppState;
use crate::audit::{python_json, write_audit};
use crate::auth::{AuthUser, RequireAdmin, RequireView};
use crate::error::ApiError;
use crate::models::{iso_naive, now_naive, python_window_start};
use crate::pyint::PyInt;
use crate::query::{BodyErrors, ModelBody, Query};
use crate::settings;

/// Unique setting keys and their defaults, in the order
/// `_EMAIL_KIND_TO_SETTING` first mentions each one.
///
/// The Python map is keyed by *notification kind*, and several kinds
/// share a setting — `camera_offline` and `camera_online` are one
/// toggle, as are the three member-lifecycle events. Building the
/// response walks that map and overwrites, so what comes out is one
/// entry per setting key in first-appearance order. This list is that
/// result, extracted from the map rather than retyped from the parts of
/// it that happen to be readable in one screen: `email_welcome` sits at
/// the end and is easy to miss.
const EMAIL_PREF_KEYS: [(&str, bool); 8] = [
    ("email_camera_offline", true),
    ("email_node_offline", true),
    ("email_incident_created", true),
    ("email_mcp_key_audit", true),
    ("email_cameranode_disk_low", true),
    ("email_member_audit", true),
    // Deliberately off. Motion volume varies wildly per install, and
    // opting everyone in risks day-one spam marks that would damage the
    // Resend sender reputation for every kind, for every customer.
    ("email_motion", false),
    ("email_welcome", true),
];

/// The subset `EmailPreferences` accepts on the write side.
///
/// `email_welcome` is readable but not settable — the POST model has
/// seven fields, the GET response has eight.
const WRITABLE_PREF_KEYS: [&str; 7] = [
    "email_camera_offline",
    "email_node_offline",
    "email_incident_created",
    "email_mcp_key_audit",
    "email_cameranode_disk_low",
    "email_member_audit",
    "email_motion",
];

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct NotificationRow {
    pub(crate) id: i32,
    pub(crate) kind: String,
    pub(crate) audience: String,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) severity: String,
    pub(crate) link: Option<String>,
    pub(crate) camera_id: Option<String>,
    pub(crate) node_id: Option<String>,
    pub(crate) meta_json: Option<String>,
    pub(crate) created_at: Option<NaiveDateTime>,
}

impl NotificationRow {
    pub(crate) fn to_json(&self) -> Value {
        // Unparseable meta is null, not an error: the column is free-form
        // and a bad row must not take down the whole inbox.
        let meta = self
            .meta_json
            .as_deref()
            .filter(|m| !m.is_empty())
            .and_then(|m| serde_json::from_str::<Value>(m).ok())
            .unwrap_or(Value::Null);
        json!({
            "id": self.id,
            "kind": self.kind,
            "audience": self.audience,
            "title": self.title,
            "body": self.body,
            "severity": self.severity,
            "link": self.link,
            "camera_id": self.camera_id,
            "node_id": self.node_id,
            "meta": meta,
            "created_at": self.created_at.map(iso_naive),
        })
    }
}

/// The caller's read-state row, created on first access.
///
/// A write on a GET, deliberately: a brand-new user with every existing
/// notification marked unread is noise, so the row is initialised with
/// `last_viewed_at = now` and they only see what arrives afterwards.
async fn get_or_init_state(
    pool: &sqlx::PgPool,
    user_id: &str,
    org_id: &str,
) -> Result<(Option<NaiveDateTime>, Option<NaiveDateTime>), ApiError> {
    let existing: Option<(Option<NaiveDateTime>, Option<NaiveDateTime>)> = sqlx::query_as(
        "SELECT last_viewed_at, cleared_at FROM user_notification_state
          WHERE clerk_user_id = $1 AND org_id = $2",
    )
    .bind(user_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?;

    if let Some(row) = existing {
        return Ok(row);
    }

    let now = now_naive();
    sqlx::query(
        "INSERT INTO user_notification_state (clerk_user_id, org_id, last_viewed_at)
         VALUES ($1, $2, $3)",
    )
    .bind(user_id)
    .bind(org_id)
    .bind(now)
    .execute(pool)
    .await?;
    Ok((Some(now), None))
}

/// Non-admins never see `audience = "admin"` rows.
fn audience_sql(user: &AuthUser) -> &'static str {
    if user.is_admin() {
        ""
    } else {
        " AND audience = 'all'"
    }
}

/// `GET /api/notifications`.
pub async fn list_notifications(
    State(state): State<AppState>,
    RequireView(user): RequireView,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let mut q = Query::parse(request.uri().query());
    let limit = q.int("limit", 50, 1, 200);
    let offset = q.int("offset", 0, 0, 1_000_000);
    // `le=720` with no lower bound in the Python signature.
    let hours = q.big_int("hours", PyInt::Small(168), None, Some(720));
    q.finish()?;

    let (last_viewed, cleared_at) =
        get_or_init_state(&state.pool, &user.user_id, &user.org_id).await?;
    let since = python_window_start(hours, 3_600)?;

    let mut where_sql = String::from(" WHERE org_id = $1 AND created_at >= $2");
    if cleared_at.is_some() {
        where_sql.push_str(" AND created_at > $3");
    }
    where_sql.push_str(audience_sql(&user));

    let count_sql = format!("SELECT COUNT(*) FROM notifications{where_sql}");
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql)
        .bind(&user.org_id)
        .bind(since);
    if let Some(c) = cleared_at {
        cq = cq.bind(c);
    }
    let total: i64 = cq.fetch_one(&state.pool).await?;

    let page_sql = format!(
        "SELECT id, kind, audience, title, body, severity, link, camera_id, node_id, \
                meta_json, created_at FROM notifications{where_sql} \
         ORDER BY created_at DESC OFFSET {offset} LIMIT {limit}"
    );
    let mut pq = sqlx::query_as::<_, NotificationRow>(&page_sql)
        .bind(&user.org_id)
        .bind(since);
    if let Some(c) = cleared_at {
        pq = pq.bind(c);
    }
    let rows: Vec<NotificationRow> = pq.fetch_all(&state.pool).await?;

    let items: Vec<Value> = rows
        .iter()
        .map(|n| {
            let mut d = n.to_json();
            // Unread when the state row has never been stamped, or the
            // notification is newer than the last look.
            let unread = match (last_viewed, n.created_at) {
                (None, _) => true,
                (Some(seen), Some(created)) => created > seen,
                (Some(_), None) => false,
            };
            d["unread"] = json!(unread);
            d
        })
        .collect();

    Ok(Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "last_viewed_at": last_viewed.map(iso_naive),
        "notifications": items,
    })))
}

/// `GET /api/notifications/unread-count` — the bell badge.
pub async fn unread_count(
    State(state): State<AppState>,
    RequireView(user): RequireView,
) -> Result<Json<Value>, ApiError> {
    let (last_viewed, _) = get_or_init_state(&state.pool, &user.user_id, &user.org_id).await?;

    let sql = format!(
        "SELECT COUNT(*) FROM notifications WHERE org_id = $1 AND created_at > $2{}",
        audience_sql(&user)
    );
    let count: i64 = sqlx::query_scalar(&sql)
        .bind(&user.org_id)
        .bind(last_viewed)
        .fetch_one(&state.pool)
        .await?;

    Ok(Json(json!({
        "unread": count,
        // `capped` is a display hint; the count itself is NOT clamped,
        // so a client showing "99+" does the clamping itself.
        "capped": count > 99,
        "last_viewed_at": last_viewed.map(iso_naive),
    })))
}

/// `POST /api/notifications/mark-viewed`.
pub async fn mark_viewed(
    State(state): State<AppState>,
    RequireView(user): RequireView,
) -> Result<Json<Value>, ApiError> {
    get_or_init_state(&state.pool, &user.user_id, &user.org_id).await?;
    let now = now_naive();
    sqlx::query(
        "UPDATE user_notification_state SET last_viewed_at = $1
          WHERE clerk_user_id = $2 AND org_id = $3",
    )
    .bind(now)
    .bind(&user.user_id)
    .bind(&user.org_id)
    .execute(&state.pool)
    .await?;

    Ok(Json(json!({ "success": true, "last_viewed_at": iso_naive(now) })))
}

/// `POST /api/notifications/clear-all`.
///
/// Nothing is deleted — `cleared_at` hides rows from this user's inbox
/// while other members of the org still see them, and audit queries
/// still see every row.
pub async fn clear_all(
    State(state): State<AppState>,
    RequireView(user): RequireView,
) -> Result<Json<Value>, ApiError> {
    get_or_init_state(&state.pool, &user.user_id, &user.org_id).await?;
    let now = now_naive();
    sqlx::query(
        "UPDATE user_notification_state SET cleared_at = $1, last_viewed_at = $1
          WHERE clerk_user_id = $2 AND org_id = $3",
    )
    .bind(now)
    .bind(&user.user_id)
    .bind(&user.org_id)
    .execute(&state.pool)
    .await?;

    Ok(Json(json!({
        "success": true,
        "cleared_at": iso_naive(now),
        "last_viewed_at": iso_naive(now),
    })))
}

async fn current_email_prefs(
    pool: &sqlx::PgPool,
    org_id: &str,
) -> Result<Map<String, Value>, ApiError> {
    let mut out = Map::new();
    for (key, default) in EMAIL_PREF_KEYS {
        // An absent row reads as the default ON/OFF state, not as
        // "unset" — the toggle UI has no third state.
        let value = match settings::get(pool, org_id, key, None).await? {
            None => default,
            Some(v) => v == "true",
        };
        out.insert(key.to_string(), json!(value));
    }
    Ok(out)
}

/// `GET /api/notifications/email/preferences`.
pub async fn get_email_preferences(
    State(state): State<AppState>,
    RequireView(user): RequireView,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({
        // The operator's platform-wide kill switch, reported alongside
        // the org's own toggles: the UI needs both to render "emails are
        // off globally, but your prefs are still set".
        "email_globally_enabled": state.config.email_enabled,
        "preferences": current_email_prefs(&state.pool, &user.org_id).await?,
    })))
}

/// `POST /api/notifications/email/preferences`.
pub async fn update_email_preferences(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    ModelBody(RequireAdmin(user), body): ModelBody<RequireAdmin>,
) -> Result<Json<Value>, ApiError> {

    let mut errors = BodyErrors::new();
    let mut changes: Vec<String> = Vec::new();
    let mut updates: Vec<(&str, bool)> = Vec::new();
    for key in WRITABLE_PREF_KEYS {
        // `exclude_unset` plus an explicit `if value is None: continue`,
        // so both an absent field and a null are no-ops.
        match body.get(key) {
            None | Some(Value::Null) => {}
            Some(_) => {
                if let Some(v) = errors.optional_bool(&body, key) {
                    updates.push((key, v));
                    // Python formats the bool with str(), giving
                    // "True"/"False" in the audit detail, not "true".
                    changes.push(format!("{key}={}", if v { "True" } else { "False" }));
                }
            }
        }
    }
    errors.finish()?;

    for (key, value) in updates {
        settings::set(
            &state.pool,
            &user.org_id,
            key,
            if value { "true" } else { "false" },
        )
        .await?;
    }

    // Only audited when something actually changed.
    if !changes.is_empty() {
        write_audit(
            &state.pool,
            &user.org_id,
            // Not "email_preferences_updated" — read the source, do not
            // infer the name from the route.
            "email_prefs_updated",
            &user.user_id,
            // `user.email or user.username`, NOT audit_label(): this
            // route has no user_id fallback, so an actor with neither
            // set is written as NULL rather than an id prefix.
            &if user.email.is_empty() {
                user.username.clone()
            } else {
                user.email.clone()
            },
            Some(python_json(&[("changes", json!(changes))])),
            &headers,
            Some(&peer.ip().to_string()),
        )
        .await;
    }

    // The same shape as the GET, plus what changed — so the frontend
    // needs no follow-up read.
    Ok(Json(json!({
        "email_globally_enabled": state.config.email_enabled,
        "preferences": current_email_prefs(&state.pool, &user.org_id).await?,
        "changes": changes,
    })))
}
