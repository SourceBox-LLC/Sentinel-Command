//! The notification inbox.
//!
//! Ported from `backend/app/api/notifications.py` — the reads and the
//! three writes that touch nothing but the database.
//!
//! Everything here is ported.

use axum::extract::{ConnectInfo, Request, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::{NaiveDateTime};
use serde_json::{json, Map, Value};

use crate::app::AppState;
use crate::audit::{audit_label, python_json, write_audit};
use crate::auth::{AuthUser, RequireAdmin, RequireView};
use crate::error::ApiError;
use crate::models::{iso_naive, now_naive, python_window_start};
use crate::pyint::PyInt;
use crate::query::{BodyErrors, ModelBody, Query};
use crate::ratelimit::PerMinute;
use crate::settings;

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
pub struct NotificationRow {
    pub id: i32,
    pub kind: String,
    pub audience: String,
    pub title: String,
    pub body: String,
    pub severity: String,
    pub link: Option<String>,
    pub camera_id: Option<String>,
    pub node_id: Option<String>,
    pub meta_json: Option<String>,
    pub created_at: Option<NaiveDateTime>,
}

impl NotificationRow {
    pub fn to_json(&self) -> Value {
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

/// `GET /api/notifications/stream` — the bell's live feed.
///
/// The audience filter is applied at broadcast time, not here, so an
/// admin-only event never reaches a viewer's socket at all.
///
/// Rate-limited on *connects*. The per-org subscriber cap already stops
/// streams accumulating, but without a connect limit a hostile client
/// could churn open → cap-hit → reject and burn a JWT verification on
/// every cycle. A browser tab reconnects on the order of seconds to
/// minutes, so sixty a minute is far above any real usage.
pub async fn stream_notifications(
    rate: PerMinute<60>,
    RequireView(user): RequireView,
) -> Result<axum::response::Response, ApiError> {
    // After auth, which is where slowapi's decorator runs: a refused
    // request must not spend the org's budget.
    rate.check().await?;
    let cap = crate::plans::get_plan_limits(&user.plan).max_sse_subscribers.max(0) as usize;
    let Some(subscription) =
        crate::notifications::BROADCASTER.subscribe(&user.org_id, user.is_admin(), cap)
    else {
        return Err(ApiError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            format!(
                "Too many open notification streams for this org (cap: {cap} on your \
                 current plan). Close unused tabs and retry, or upgrade for a higher cap."
            ),
        ));
    };

    // The first frame goes out before anything is awaited, so a client
    // knows it is connected rather than waiting up to 25 seconds for
    // the first keepalive to prove it.
    let hello = format!(
        "data: {}\n\n",
        python_json(&[
            ("type", json!("connected")),
            ("org_id", json!(user.org_id)),
        ])
    );

    let stream = futures_util::stream::unfold(
        (Some(hello), subscription),
        |(hello, mut subscription)| async move {
            if let Some(hello) = hello {
                return Some((Ok::<_, std::io::Error>(hello), (None, subscription)));
            }
            // A quiet stream still has to say something, or an
            // intermediary will time the connection out.
            let frame = match tokio::time::timeout(
                std::time::Duration::from_secs(25),
                subscription.recv(),
            )
            .await
            {
                Ok(Some(event)) => format!("data: {event}\n\n"),
                // Unreachable while the subscription holds its own
                // sender — see `sse::Subscription::keepalive`.
                Ok(None) => return None,
                Err(_) => ": keepalive\n\n".to_string(),
            };
            Some((Ok(frame), (None, subscription)))
        },
    );

    axum::response::Response::builder()
        .header("content-type", "text/event-stream; charset=utf-8")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        // Without this nginx buffers the whole response and the stream
        // never arrives.
        .header("x-accel-buffering", "no")
        .body(axum::body::Body::from_stream(stream))
        .map_err(|err| ApiError::internal(err.to_string()))
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
    for (key, default) in crate::notifications::email_pref_keys() {
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

/// The copy every admin reads when a member asks to be promoted.
///
/// Its own function for the same reason as the key-revoke body: the
/// Python assembles it from five adjacent f-strings, and the join
/// points — including two deliberate double spaces after full stops —
/// are where a difference hides in a `Text` column.
fn promotion_request_body(requester: &str) -> String {
    format!(
        "{requester} has requested to be promoted to admin in \
         your organization.  Review their access needs and, if \
         appropriate, promote them via Settings → Members.  This \
         is an in-app request — Clerk's role management is the \
         source of truth for who actually gets admin."
    )
}

/// `POST /api/notifications/request-admin-promotion`.
///
/// Three an hour, per org. A member spamming their admins does not get
/// through, and a member who double-clicked does not either — while
/// still leaving room for a retry after Clerk's role propagation lags.
pub async fn request_admin_promotion(
    rate: crate::ratelimit::PerHour<3>,
    State(state): State<AppState>,
    RequireView(user): RequireView,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
) -> Result<Json<Value>, ApiError> {
    rate.check().await?;
    // An admin asking to be promoted would otherwise emit "X requested
    // admin access" to themselves, which reads as a fault.
    if user.is_admin() {
        return Err(ApiError::bad_request(
            "You're already an admin in this organization — no need to request promotion.",
        ));
    }

    // `audit_label(user) or user.email or user.user_id or "a member"`.
    let requester = [
        audit_label(&user),
        user.email.clone(),
        user.user_id.clone(),
        "a member".to_string(),
    ]
    .into_iter()
    .find(|candidate| !candidate.is_empty())
    .unwrap_or_default();

    crate::notifications::create_notification(
        &state,
        &user.org_id,
        crate::notifications::NewNotification::new(
            "member_promotion_requested",
            format!("{requester} requested admin access"),
        )
        .body(promotion_request_body(&requester))
        .severity("info")
        .audience("admin")
        .link("/settings")
        .meta(json!({
            "requester_user_id": user.user_id,
            "requester_email": user.email,
            "requester_label": requester,
        })),
    )
    .await;

    write_audit(
        &state.pool,
        &user.org_id,
        "member_promotion_requested",
        &user.user_id,
        &audit_label(&user),
        Some(python_json(&[("requester_label", json!(requester))])),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    Ok(Json(json!({
        "success": true,
        "message": "Your request has been sent to your organization's admins. \
                    They'll be notified by email and in their dashboard.",
    })))
}

/// The page a click on an unsubscribe footer lands on.
///
/// HTML, not JSON: the person clicked a link in their email and expects
/// a web page. Built with `format!` rather than a template, exactly as
/// the Python builds it with `str.format` — which means no autoescaping,
/// so everything substituted is escaped by hand.
const UNSUBSCRIBE_OK: &str = r##"<!DOCTYPE html>
<html lang="en"><head><meta charset="UTF-8"><title>Unsubscribed</title>
<style>
  body {{ font-family: system-ui, sans-serif; max-width: 540px;
         margin: 80px auto; padding: 0 24px; color: #111; line-height: 1.5; }}
  h1 {{ font-size: 22px; margin: 0 0 12px; }}
  p {{ color: #555; }}
  .ok {{ color: #16a34a; font-weight: 600; }}
  a {{ color: #22c55e; }}
</style></head>
<body>
  <h1><span class="ok">✓</span> You're unsubscribed</h1>
  <p>This email address won't receive Sentinel notification emails
     anymore (you clicked unsubscribe on a <strong>{kind}</strong> alert).</p>
  <p>Other members of your organization are unaffected.  Org-wide email
     preferences can be fine-tuned any time in your
     <a href="{frontend}/settings#settings-notifications">notification settings</a>.</p>
</body></html>"##;

const UNSUBSCRIBE_ERROR: &str = r##"<!DOCTYPE html>
<html lang="en"><head><meta charset="UTF-8"><title>Link expired</title>
<style>
  body {{ font-family: system-ui, sans-serif; max-width: 540px;
         margin: 80px auto; padding: 0 24px; color: #111; line-height: 1.5; }}
  h1 {{ font-size: 22px; margin: 0 0 12px; }}
  p {{ color: #555; }}
  a {{ color: #22c55e; }}
</style></head>
<body>
  <h1>Link not recognised</h1>
  <p>This unsubscribe link looks invalid or has been superseded.
     Sign in to your <a href="{frontend}/settings#settings-notifications">
     notification settings</a> to manage email alerts directly.</p>
</body></html>"##;

/// `GET /api/notifications/email/unsubscribe`.
///
/// Public on purpose: the signed token in the URL is the authority, so
/// a click needs no session — which is the only way an unsubscribe link
/// can work from an inbox. That also makes an explicit rate limit
/// necessary, since the shared limiter applies no default: one leaked
/// link could otherwise be hammered to write rows in a tight loop.
///
/// **It suppresses the address, not the org's toggle.** Tokens sit in
/// inboxes for as long as the mail does, including the inbox of someone
/// since removed from the org — and flipping the org-wide setting let
/// any past recipient permanently disable a whole organisation's
/// security alerts from an old message. Suppressing the clicking
/// address is what the link actually promises.
pub async fn email_unsubscribe(
    rate: PerMinute<60>,
    State(state): State<AppState>,
    request: Request,
) -> Result<axum::response::Response, ApiError> {
    let mut query = Query::parse(request.uri().query());
    let token = query.required_str("t");
    query.finish()?;
    rate.check().await?;

    let frontend = state.config.frontend_url.trim_end_matches('/');
    let frontend_safe = crate::email_templates::html_escape(frontend);

    let secret = {
        let base = if state.config.is_local_auth() {
            &state.config.app_secret_key
        } else {
            &state.config.clerk_secret_key
        };
        crate::email_unsubscribe::derive_secret(base).unwrap_or_default()
    };

    let Some((org_id, kind, recipient)) =
        crate::email_unsubscribe::verify_token(&secret, &token)
    else {
        return Ok(html(
            axum::http::StatusCode::BAD_REQUEST,
            UNSUBSCRIBE_ERROR.replace("{frontend}", &frontend_safe),
        ));
    };

    // Idempotent: a second click on the same link finds the row and
    // adds nothing. The address is the key, not (address, org) — a
    // hard-bounced or unsubscribed address stops being mailed for
    // every org, which is the whole point of the table.
    let existing: Result<Option<(i32,)>, _> =
        sqlx::query_as("SELECT id FROM email_suppression WHERE address = $1")
            .bind(&recipient)
            .fetch_optional(&state.pool)
            .await;
    match existing {
        Ok(None) => {
            let inserted = sqlx::query(
                "INSERT INTO email_suppression (address, reason, source, created_at)
                 VALUES ($1, 'unsubscribe', 'unsubscribe_link', $2)",
            )
            .bind(&recipient)
            .bind(now_naive())
            .execute(&state.pool)
            .await;
            if let Err(err) = inserted {
                // Never surfaced: they clicked unsubscribe and want to
                // feel unsubscribed. The link is idempotent, so a retry
                // or the settings page still works.
                tracing::error!(error = %err, "[Unsubscribe] failed to write suppression row");
            }
        }
        Ok(Some(_)) => {}
        Err(err) => {
            tracing::error!(error = %err, "[Unsubscribe] failed to write suppression row");
        }
    }

    // No user id — this is a public link click — and the local part is
    // masked, because an audit trail should not double as an email
    // directory.
    let masked = match recipient.split_once('@') {
        Some((local, domain)) => {
            format!("{}***@{domain}", local.chars().take(1).collect::<String>())
        }
        None => "***".to_string(),
    };
    write_audit(
        &state.pool,
        &org_id,
        "email_unsubscribed",
        "",
        "",
        Some(python_json(&[
            ("kind", json!(kind)),
            ("via_link", json!(true)),
            ("address", json!(masked)),
        ])),
        &HeaderMap::new(),
        None,
    )
    .await;

    // "camera_offline" reads better to a person as "camera offline".
    let pretty = crate::email_templates::html_escape(&kind.replace('_', " "));
    Ok(html(
        axum::http::StatusCode::OK,
        UNSUBSCRIBE_OK
            .replace("{kind}", &pretty)
            .replace("{frontend}", &frontend_safe),
    ))
}

/// Starlette's `HTMLResponse`: the doubled braces in the templates above
/// are literal CSS, so the substitution is by name and not by `format!`.
fn html(status: axum::http::StatusCode, body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body.replace("{{", "{").replace("}}", "}"),
    )
        .into_response()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Five adjacent f-strings in the Python, two of the joins carrying
    /// a deliberate double space after a full stop. This is the string
    /// that lands in `notifications.body`.
    #[test]
    fn the_promotion_request_reads_as_one_paragraph() {
        assert_eq!(
            promotion_request_body("bob@example.com"),
            "bob@example.com has requested to be promoted to admin in your \
             organization.  Review their access needs and, if appropriate, \
             promote them via Settings → Members.  This is an in-app request — \
             Clerk's role management is the source of truth for who actually \
             gets admin."
        );
    }
}
