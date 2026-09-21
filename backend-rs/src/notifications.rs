//! Emitting a notification: the inbox row, and the email beside it.
//!
//! Ported from the service half of `backend/app/api/notifications.py` —
//! the routes stay in `api/notifications.rs`; this is what every
//! emitter in the app calls.
//!
//! **Two gates, not one, and they are independent.** The inbox
//! preference decides whether a row is written and broadcast; the email
//! preference decides whether mail goes out. An operator who muted the
//! bell has not unsubscribed, and the Settings page presents the two as
//! separate switches — an earlier version of the Python returned early
//! on the inbox gate and so silently killed every "email on, inbox off"
//! combination, and blinded the agent whenever bell noise was muted.
//! When the inbox gate is closed the notification is still constructed,
//! just never inserted: the templates read only the fields the caller
//! passed, and `email_outbox.notification_id` is a soft reference that
//! is simply left null.
//!
//! **Nothing here can fail the caller.** A notification is a side
//! effect of something that already happened — a heartbeat that
//! lapsed, a key that was revoked — so every layer swallows its errors
//! and logs. The one thing that would be worse than a missing email is
//! a missing camera.
//!
//! **The defaults differ between the two gates on purpose.** The inbox
//! defaults an unknown kind to *on*, so a newly added kind does not
//! vanish before its setting exists; the email gate defaults it to
//! *off*, because the failure modes are not symmetrical — a kind that
//! silently starts mailing every org is a sender-reputation problem for
//! every other kind too.

use chrono::NaiveDateTime;
use serde_json::Value;

use crate::api::notifications::NotificationRow;
use crate::audit::python_json_value;
use crate::config::Config;
use crate::email_templates::{self, NotificationView};
use crate::email_unsubscribe;
use crate::models::{iso_naive, now_naive};
use crate::pyint::PyInt;
use crate::recipients::{self, Lookup};
use crate::settings;

/// `_NOTIFICATION_KIND_TO_SETTING`: kind → (setting key, default).
///
/// Kinds share keys where the audience decision is the same one —
/// a camera's offline and online events are one switch, because
/// whoever wanted the 3am alert wants the all-clear too, and the three
/// member-lifecycle events are one because an admin wants the whole
/// audit trail or none of it.
const INBOX_KIND_TO_SETTING: [(&str, &str, bool); 16] = [
    ("motion", "motion_notifications", true),
    ("motion_digest", "motion_notifications", true),
    ("camera_online", "camera_transition_notifications", true),
    ("camera_offline", "camera_transition_notifications", true),
    ("node_online", "node_transition_notifications", true),
    ("node_offline", "node_transition_notifications", true),
    ("mcp_key_created", "mcp_key_audit_notifications", true),
    ("mcp_key_revoked", "mcp_key_audit_notifications", true),
    // A separate toggle from the MCP keys above, because the blast
    // radius differs: an agent key authenticates an autonomous service
    // watching this org's cameras. Inbox only — there is deliberately
    // no email template for either.
    ("sentinel_agent_key_created", "sentinel_agent_key_audit_notifications", true),
    ("sentinel_agent_key_revoked", "sentinel_agent_key_audit_notifications", true),
    ("cameranode_disk_low", "cameranode_disk_notifications", true),
    ("member_added", "member_audit_notifications", true),
    ("member_role_changed", "member_audit_notifications", true),
    ("member_removed", "member_audit_notifications", true),
    ("member_promotion_requested", "member_audit_notifications", true),
    ("welcome", "welcome_notifications", true),
];

/// `_EMAIL_KIND_TO_SETTING`.
///
/// Shorter than the inbox map by design. The two `sentinel_agent_key_*`
/// kinds are absent, which makes them unemailable — the email gate
/// refuses a kind it does not know. `disk_critical` is absent for a
/// different reason: our own volume filling is platform state that an
/// org admin cannot act on, and routing it here is the multi-tenant
/// violation removed in 2026-05-04.
const EMAIL_KIND_TO_SETTING: [(&str, &str, bool); 15] = [
    ("camera_offline", "email_camera_offline", true),
    ("camera_online", "email_camera_offline", true),
    ("node_offline", "email_node_offline", true),
    ("node_online", "email_node_offline", true),
    ("incident_created", "email_incident_created", true),
    ("mcp_key_created", "email_mcp_key_audit", true),
    ("mcp_key_revoked", "email_mcp_key_audit", true),
    ("cameranode_disk_low", "email_cameranode_disk_low", true),
    ("member_added", "email_member_audit", true),
    ("member_role_changed", "email_member_audit", true),
    ("member_removed", "email_member_audit", true),
    ("member_promotion_requested", "email_member_audit", true),
    // Off by default, and the only entry that is. Motion volume varies
    // by orders of magnitude between one indoor doorbell and ten
    // outdoor cameras, so opting everyone in risks day-one spam marks
    // that would cost the sender reputation of every kind for every
    // customer. The per-camera cooldown and the digest below bound the
    // volume once a customer does opt in.
    ("motion", "email_motion", false),
    ("motion_digest", "email_motion", false),
    // Its own key with no UI toggle today, so a future marketing
    // opt-out has somewhere to land without conflating with the
    // operational kinds above. Last in the map, and so last in the
    // preferences response.
    ("welcome", "email_welcome", true),
];

/// The distinct setting keys of `EMAIL_KIND_TO_SETTING`, in the order
/// the map first mentions each, with its default.
///
/// This is what `GET /api/notifications/email/preferences` returns, and
/// it is derived rather than listed a second time: the Python builds
/// the response by walking the map and overwriting, so the two can only
/// agree if there is one list. `email_welcome` sits at the end of the
/// map and is exactly the entry a retyped copy drops.
pub fn email_pref_keys() -> Vec<(&'static str, bool)> {
    let mut distinct: Vec<(&str, bool)> = Vec::new();
    for (_, key, default) in EMAIL_KIND_TO_SETTING {
        if !distinct.iter().any(|(existing, _)| *existing == key) {
            distinct.push((key, default));
        }
    }
    distinct
}

/// `notification_broadcaster` — the module-level singleton the Python
/// has, for the same reason: every emitter in the app reaches the same
/// set of open bell streams.
pub static BROADCASTER: crate::sse::Broadcaster = crate::sse::Broadcaster::new("notifications");

/// The placeholder the templates render, substituted per recipient at
/// enqueue time. Rendering once and substituting is not only cheaper
/// than rendering per recipient — the token binds the address, so there
/// is no shared URL to render in the first place. No character in it is
/// HTML-special, so it survives both the text and the escaped HTML.
const UNSUB_PLACEHOLDER: &str = "UNSUB-URL-PLACEHOLDER-7f3a";

/// `_TRANSITION_DEBOUNCE_SECONDS`.
const TRANSITION_DEBOUNCE_SECONDS: u64 = 60;

/// What the emitters need to reach: the database, Clerk, and the
/// configuration that decides whether mail is on at all.
pub struct NotifyContext<'a> {
    pub pool: &'a sqlx::PgPool,
    pub http: &'a reqwest::Client,
    pub config: &'a Config,
}

impl NotifyContext<'_> {
    fn recipient_lookup(&self) -> Lookup<'_> {
        Lookup {
            client: self.http,
            clerk_base_url: &self.config.clerk_api_url,
            clerk_secret: &self.config.clerk_secret_key,
            local_admin_email: self
                .config
                .is_local_auth()
                .then_some(self.config.local_admin_email.as_str()),
        }
    }

    /// The base the unsubscribe token is signed with, already derived.
    fn unsubscribe_secret(&self) -> Option<String> {
        let base = if self.config.is_local_auth() {
            &self.config.app_secret_key
        } else {
            &self.config.clerk_secret_key
        };
        email_unsubscribe::derive_secret(base)
    }
}

/// The arguments `create_notification` takes, with the same defaults.
pub struct NewNotification {
    pub kind: String,
    pub title: String,
    pub body: String,
    pub severity: String,
    pub audience: String,
    pub link: Option<String>,
    pub camera_id: Option<String>,
    pub node_id: Option<String>,
    pub meta: Option<Value>,
}

impl NewNotification {
    pub fn new(kind: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            title: title.into(),
            body: String::new(),
            severity: "info".to_string(),
            audience: "all".to_string(),
            link: None,
            camera_id: None,
            node_id: None,
            meta: None,
        }
    }

    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    pub fn severity(mut self, severity: impl Into<String>) -> Self {
        self.severity = severity.into();
        self
    }

    pub fn audience(mut self, audience: impl Into<String>) -> Self {
        self.audience = audience.into();
        self
    }

    pub fn link(mut self, link: impl Into<String>) -> Self {
        self.link = Some(link.into());
        self
    }

    pub fn camera(mut self, camera_id: impl Into<String>) -> Self {
        self.camera_id = Some(camera_id.into());
        self
    }

    pub fn node(mut self, node_id: impl Into<String>) -> Self {
        self.node_id = Some(node_id.into());
        self
    }

    pub fn meta(mut self, meta: Value) -> Self {
        self.meta = Some(meta);
        self
    }
}

/// `notifications_enabled(db, org_id, kind)`.
///
/// An unknown kind is enabled: a notification type added without a
/// settings migration must not silently disappear.
pub async fn inbox_enabled(pool: &sqlx::PgPool, org_id: &str, kind: &str) -> bool {
    let Some((_, key, default)) = INBOX_KIND_TO_SETTING.iter().find(|(k, ..)| *k == kind) else {
        return true;
    };
    setting_is_true(pool, org_id, key, *default).await
}

/// `email_enabled_for_kind(db, org_id, kind)`.
///
/// Two gates in series: the global kill switch, then the per-org
/// per-kind setting. An unknown kind is *disabled*, the opposite of the
/// inbox default — see the module note.
pub async fn email_enabled(config: &Config, pool: &sqlx::PgPool, org_id: &str, kind: &str) -> bool {
    if !config.email_enabled {
        return false;
    }
    let Some((_, key, default)) = EMAIL_KIND_TO_SETTING.iter().find(|(k, ..)| *k == kind) else {
        return false;
    };
    setting_is_true(pool, org_id, key, *default).await
}

/// A setting read as a boolean. Anything that is not exactly `"true"`
/// is false; an absent row takes the default.
///
/// A database error falls back to the default rather than propagating.
/// The Python's callers wrap these lookups in `try/except` and emit
/// anyway, and a gate that cannot be read must not be a gate that
/// blocks.
async fn setting_is_true(pool: &sqlx::PgPool, org_id: &str, key: &str, default: bool) -> bool {
    match settings::get(pool, org_id, key, None).await {
        Ok(Some(value)) => value == "true",
        Ok(None) => default,
        Err(err) => {
            tracing::error!(error = %err, org_id, key, "[Notifications] preference lookup failed; emitting anyway");
            default
        }
    }
}

// ---------------------------------------------------------------------
// The motion email cooldown
// ---------------------------------------------------------------------

/// `_motion_cooldown_anchor_key`.
///
/// The colon suffix is the same pattern `cameranode_disk_low_emit_at`
/// uses, and it is what lets the digest loop find every active anchor
/// with one `LIKE` and read the camera id back out of the key.
pub fn motion_cooldown_anchor_key(camera_id: &str) -> String {
    format!("motion_email_cooldown_start:{camera_id}")
}

/// `_motion_cooldown_minutes`. Hidden setting, default 15, floor 1 —
/// zero would be pointless since the immediate mail fires anyway. A
/// corrupt value falls back rather than disabling email outright.
pub async fn motion_cooldown_minutes(pool: &sqlx::PgPool, org_id: &str) -> i64 {
    let raw = settings::get(pool, org_id, "email_motion_cooldown_minutes", Some("15"))
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "15".to_string());
    // `max(1, int(raw))`, falling back on anything `int()` refuses.
    // A value beyond i64 is not a number this ever really holds — it is
    // a hand-edited row — but it still has to land somewhere, and the
    // Python's answer for a huge one is "a cooldown that never expires".
    match crate::pyint::str_as_int(raw.trim()) {
        Ok(PyInt::Small(minutes)) => minutes.max(1),
        Ok(PyInt::Big { negative: true }) => 1,
        Ok(PyInt::Big { negative: false }) => i64::MAX,
        Err(_) => 15,
    }
}

/// `_claim_motion_cooldown_or_silence`.
///
/// True means "email now", and writes the anchor as it says so; false
/// means a window is open and the digest loop will summarise instead.
///
/// The read-then-write is not atomic, and deliberately so: two motion
/// events in the same instant could both claim, and one duplicate
/// first-event email is a better failure than a lost alert. The anchor
/// lives in `settings` rather than in memory because a deploy in the
/// middle of a window must not re-spam.
pub async fn claim_motion_cooldown_or_silence(
    pool: &sqlx::PgPool,
    org_id: &str,
    camera_id: Option<&str>,
) -> bool {
    // Motion always carries a camera today. A future caller without one
    // defaults to sending, not to silence — a missing camera id would
    // also break the digest loop's per-camera grouping, so silencing
    // would mean losing the event entirely.
    let Some(camera_id) = camera_id else {
        return true;
    };

    let key = motion_cooldown_anchor_key(camera_id);
    let existing = settings::get(pool, org_id, &key, Some("")).await.ok().flatten();
    let cooldown_minutes = motion_cooldown_minutes(pool, org_id).await;
    let now = now_naive();

    if let Some(existing) = existing.filter(|value| !value.is_empty()) {
        // A malformed value is treated as expired and overwritten below
        // — better than refusing to email forever because someone
        // hand-edited a row.
        match crate::pydatetime::fromisoformat(&existing) {
            // An anchor with an offset is one nobody here wrote. Python
            // cannot subtract it from a naive `now` and raises TypeError
            // — which the email side-channel catches, so the event is
            // neither mailed nor re-anchored.
            Ok(anchor) if anchor.offset_us.is_some() => return false,
            Ok(anchor) => {
                if (now - anchor.naive).num_seconds()
                    < cooldown_minutes.saturating_mul(60)
                {
                    return false;
                }
            }
            Err(_) => {}
        }
    }

    if let Err(err) = settings::set(pool, org_id, &key, &iso_naive(now)).await {
        tracing::error!(error = %err, org_id, camera_id, "[Notifications] could not write the motion cooldown anchor");
    }
    true
}

// ---------------------------------------------------------------------
// Creating one
// ---------------------------------------------------------------------

/// `create_notification(...)`.
///
/// Returns the persisted row, or `None` when the inbox preference
/// suppressed it — even though the email beside it may still have gone
/// out. That is the Python's long-standing contract and several callers
/// read it as "was anything written".
pub async fn create_notification(
    ctx: &NotifyContext<'_>,
    org_id: &str,
    notification: NewNotification,
) -> Option<NotificationRow> {
    let audience = match notification.audience.as_str() {
        "all" | "admin" => notification.audience.as_str(),
        _ => "all",
    };
    let severity = match notification.severity.as_str() {
        "info" | "warning" | "error" | "critical" => notification.severity.as_str(),
        _ => "info",
    };
    // `title[:200]` — characters, not bytes, and the column is
    // `varchar(200)`, which Postgres also counts in characters.
    let title: String = notification.title.chars().take(200).collect();
    // `json.dumps(meta) if meta else None`: an empty object is falsy in
    // Python, so it stores NULL rather than `{}`.
    let meta_json = notification
        .meta
        .as_ref()
        .filter(|meta| !is_falsy(meta))
        .map(python_json_value);

    let inbox = inbox_enabled(ctx.pool, org_id, &notification.kind).await;

    let mut row = NotificationRow {
        id: 0,
        kind: notification.kind.clone(),
        audience: audience.to_string(),
        title,
        body: notification.body.clone(),
        severity: severity.to_string(),
        link: notification.link.clone(),
        camera_id: notification.camera_id.clone(),
        node_id: notification.node_id.clone(),
        meta_json,
        created_at: Some(now_naive()),
    };

    let mut persisted = false;
    if inbox {
        match insert_notification(ctx.pool, org_id, &row).await {
            Ok((id, created_at)) => {
                row.id = id;
                row.created_at = Some(created_at);
                persisted = true;
                // After the insert, never before: a subscriber must
                // not see a row that could still be rolled back.
                BROADCASTER.notify(org_id, audience, &broadcast_payload(&row));
            }
            Err(err) => {
                tracing::error!(error = %err, org_id, kind = %row.kind, "[Notifications] Failed to create notification");
                return None;
            }
        }
    }

    // The email side-channel, which runs whatever the inbox gate said.
    if email_enabled(ctx.config, ctx.pool, org_id, &row.kind).await {
        // The cooldown gate applies to motion only: the first event per
        // camera per window mails, the rest reach the inbox and the SSE
        // above but skip the outbox, and the digest loop summarises
        // them when the window closes.
        let send = if row.kind == "motion" {
            claim_motion_cooldown_or_silence(ctx.pool, org_id, row.camera_id.as_deref()).await
        } else {
            true
        };
        if send {
            enqueue_email(ctx, org_id, &row, persisted, audience).await;
        }
    }

    persisted.then_some(row)
}

/// `notif.to_dict()` plus `type`, serialised the way `json.dumps`
/// would.
///
/// The Python sets `payload["audience"]` after `to_dict()`, which
/// already has that key — so the assignment overwrites in place and
/// does not move it. `type` is new, so it goes last. The order is
/// visible in the frame the browser receives.
fn broadcast_payload(row: &NotificationRow) -> String {
    let mut payload = row.to_json();
    payload["type"] = Value::String("notification".to_string());
    python_json_value(&payload)
}

/// Python's truthiness for the `meta` argument: `None`, `{}`, `[]`,
/// `""`, `0` and `false` are all falsy, and each would store NULL.
fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
    }
}

async fn insert_notification(
    pool: &sqlx::PgPool,
    org_id: &str,
    row: &NotificationRow,
) -> Result<(i32, NaiveDateTime), sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO notifications
           (org_id, kind, audience, title, body, severity, link, camera_id, node_id,
            meta_json, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         RETURNING id, created_at",
    )
    .bind(org_id)
    .bind(&row.kind)
    .bind(&row.audience)
    .bind(&row.title)
    .bind(&row.body)
    .bind(&row.severity)
    .bind(&row.link)
    .bind(&row.camera_id)
    .bind(&row.node_id)
    .bind(&row.meta_json)
    .bind(row.created_at)
    .fetch_one(pool)
    .await
}

/// `_enqueue_email_for_notification` — one outbox row per address.
///
/// Never raises, and returns nothing: the inbox row is already
/// committed and a missing email is recoverable, where a failure
/// propagating out of here would take the notification with it.
///
/// The rows go in one transaction because the Python adds them all and
/// commits once: a row the database refuses — a subject past
/// `varchar(500)`, say — takes the whole batch with it rather than
/// leaving some recipients mailed and others not.
async fn enqueue_email(
    ctx: &NotifyContext<'_>,
    org_id: &str,
    row: &NotificationRow,
    persisted: bool,
    audience: &str,
) {
    let recipients = recipients::recipient_emails(&ctx.recipient_lookup(), org_id, audience).await;
    if recipients.is_empty() {
        return;
    }

    let view = NotificationView {
        title: row.title.clone(),
        body: row.body.clone(),
        severity: row.severity.clone(),
        link: row.link.clone(),
        camera_id: row.camera_id.clone(),
        node_id: row.node_id.clone(),
        meta_json: row.meta_json.clone(),
    };
    let (subject, body_text, body_html) = email_templates::render(
        &row.kind,
        &view,
        UNSUB_PLACEHOLDER,
        &ctx.config.frontend_url,
    );

    let Some(secret) = ctx.unsubscribe_secret() else {
        tracing::error!(
            org_id,
            kind = %row.kind,
            missing = email_unsubscribe::secret_base_name(ctx.config.is_local_auth()),
            "[Notifications] cannot sign unsubscribe tokens — no email enqueued"
        );
        return;
    };
    // `notification_id` is a soft reference, so a suppressed inbox row
    // leaves it null rather than blocking the mail.
    let notification_id = persisted.then_some(row.id);
    let now = chrono::Utc::now().timestamp();

    let mut tx = match ctx.pool.begin().await {
        Ok(tx) => tx,
        Err(err) => {
            tracing::error!(error = %err, org_id, kind = %row.kind, "[Notifications] outbox commit failed");
            return;
        }
    };
    for address in &recipients {
        let Some(unsub) = email_unsubscribe::build_unsubscribe_url(
            &secret,
            &ctx.config.frontend_url,
            org_id,
            &row.kind,
            address,
            now,
        ) else {
            continue;
        };
        let result = sqlx::query(
            "INSERT INTO email_outbox
               (org_id, recipient_email, subject, body_text, body_html, kind,
                notification_id, status, attempts, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'pending', 0, $8)",
        )
        .bind(org_id)
        .bind(address)
        .bind(&subject)
        .bind(body_text.replace(UNSUB_PLACEHOLDER, &unsub))
        .bind(body_html.replace(UNSUB_PLACEHOLDER, &unsub))
        .bind(&row.kind)
        .bind(notification_id)
        .bind(now_naive())
        .execute(&mut *tx)
        .await;
        if let Err(err) = result {
            tracing::error!(error = %err, org_id, kind = %row.kind, address, "[Notifications] enqueue failed");
            return;
        }
    }
    if let Err(err) = tx.commit().await {
        tracing::error!(error = %err, org_id, kind = %row.kind, "[Notifications] outbox commit failed");
    }
}

// ---------------------------------------------------------------------
// Status transitions
// ---------------------------------------------------------------------

/// `_transition_debounce`.
///
/// Cameras and nodes flap — a spotty uplink can toggle every few
/// seconds — so a repeat of the same `(kind, entity, direction)` inside
/// a minute is dropped. In memory only, because the worst case after a
/// restart is one extra notification and the inbox is append-only.
///
/// **Which process owns this map decides whether it works.** Once Rust
/// emits a transition, Python's copy of the map stops seeing that
/// entity, and a fleet where both emit would debounce nothing.
/// Keyed on `(kind, entity id, direction)`, holding when that exact
/// transition was last emitted.
type Debounce = std::collections::HashMap<(String, String, String), std::time::Instant>;

static DEBOUNCE: std::sync::Mutex<Option<Debounce>> = std::sync::Mutex::new(None);

/// `_should_emit_transition`.
///
/// The "never seen" sentinel has to be *absence*, not a zero instant:
/// on a freshly booted host a monotonic clock starts near zero, so a
/// zero sentinel puts the very first emit inside the debounce window
/// and drops it. The Python uses `-inf` for the same reason.
fn should_emit_transition(kind: &str, entity_id: &str, direction: &str) -> bool {
    let key = (kind.to_string(), entity_id.to_string(), direction.to_string());
    let now = std::time::Instant::now();
    let mut guard = DEBOUNCE.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(Debounce::default);
    if let Some(last) = map.get(&key) {
        if now.duration_since(*last).as_secs() < TRANSITION_DEBOUNCE_SECONDS {
            return false;
        }
    }
    map.insert(key, now);
    true
}

/// `clear_transition_debounce`. Test-facing, and used by the harness
/// between cases.
pub fn clear_transition_debounce() {
    let mut guard = DEBOUNCE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = guard.as_mut() {
        map.clear();
    }
}

/// `emit_camera_transition`. Audience `all` — every member cares when a
/// camera drops.
pub async fn emit_camera_transition(
    ctx: &NotifyContext<'_>,
    org_id: &str,
    camera_id: &str,
    display_name: &str,
    new_status: &str,
    node_id: Option<&str>,
) -> Option<NotificationRow> {
    if !matches!(new_status, "online" | "offline") {
        return None;
    }
    if !should_emit_transition("camera", camera_id, new_status) {
        return None;
    }

    let online = new_status == "online";
    let mut notification = NewNotification::new(
        if online { "camera_online" } else { "camera_offline" },
        if online {
            format!("{display_name} is online")
        } else {
            format!("{display_name} went offline")
        },
    )
    .body(if online {
        "Camera is streaming again."
    } else {
        "No heartbeat received in over 90 seconds."
    })
    .severity(if online { "info" } else { "warning" })
    .audience("all")
    .link(format!("/dashboard?camera={camera_id}"))
    .camera(camera_id);
    notification.node_id = node_id.map(str::to_string);

    create_notification(ctx, org_id, notification).await
}

/// `emit_node_transition`. Audience `admin` — node health is an
/// operator concern, and a viewer has nothing to do with an uplink.
pub async fn emit_node_transition(
    ctx: &NotifyContext<'_>,
    org_id: &str,
    node_id: &str,
    display_name: &str,
    new_status: &str,
) -> Option<NotificationRow> {
    if !matches!(new_status, "online" | "offline") {
        return None;
    }
    if !should_emit_transition("node", node_id, new_status) {
        return None;
    }

    let online = new_status == "online";
    let notification = NewNotification::new(
        if online { "node_online" } else { "node_offline" },
        if online {
            format!("Node '{display_name}' is online")
        } else {
            format!("Node '{display_name}' went offline")
        },
    )
    .body(if online {
        "CameraNode is connected and reporting."
    } else {
        "No heartbeat received in over 90 seconds."
    })
    .severity(if online { "info" } else { "warning" })
    .audience("admin")
    .link("/admin")
    .node(node_id);

    create_notification(ctx, org_id, notification).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every kind either has an inbox setting or deliberately does not,
    /// and the two maps agree about which is which.
    #[test]
    fn the_two_gates_cover_the_kinds_they_should() {
        // Everything the email map knows is also in the inbox map,
        // except `incident_created`, which has no inbox toggle — an
        // incident always reaches the bell.
        for (kind, ..) in EMAIL_KIND_TO_SETTING {
            if kind == "incident_created" {
                continue;
            }
            assert!(
                INBOX_KIND_TO_SETTING.iter().any(|(k, ..)| *k == kind),
                "{kind} can email but has no inbox entry"
            );
        }
        // The agent-key kinds are inbox-only, and that is what makes
        // them unemailable — the email gate refuses a kind it does not
        // know.
        for kind in ["sentinel_agent_key_created", "sentinel_agent_key_revoked"] {
            assert!(INBOX_KIND_TO_SETTING.iter().any(|(k, ..)| *k == kind));
            assert!(!EMAIL_KIND_TO_SETTING.iter().any(|(k, ..)| *k == kind));
        }
        // Our own disk filling is operator state, not an org's.
        assert!(!EMAIL_KIND_TO_SETTING.iter().any(|(k, ..)| *k == "disk_critical"));
        assert!(!INBOX_KIND_TO_SETTING.iter().any(|(k, ..)| *k == "disk_critical"));
    }

    /// Only motion defaults to off, and the whole sender reputation
    /// argument rests on that staying true.
    #[test]
    fn motion_is_the_only_kind_that_defaults_to_no_email() {
        let off: Vec<&str> = EMAIL_KIND_TO_SETTING
            .iter()
            .filter(|(.., default)| !default)
            .map(|(kind, ..)| *kind)
            .collect();
        assert_eq!(off, vec!["motion", "motion_digest"]);
        assert!(INBOX_KIND_TO_SETTING.iter().all(|(.., default)| *default));
    }

    /// The shared keys are shared on purpose; a port that gave each
    /// kind its own key would make an operator opt in twice.
    #[test]
    fn the_paired_kinds_share_one_setting() {
        let key_for = |map: &[(&str, &str, bool)], kind: &str| -> String {
            map.iter().find(|(k, ..)| *k == kind).unwrap().1.to_string()
        };
        for (a, b) in [("camera_offline", "camera_online"), ("node_offline", "node_online")] {
            assert_eq!(key_for(&EMAIL_KIND_TO_SETTING, a), key_for(&EMAIL_KIND_TO_SETTING, b));
            assert_eq!(key_for(&INBOX_KIND_TO_SETTING, a), key_for(&INBOX_KIND_TO_SETTING, b));
        }
        for kind in ["member_role_changed", "member_removed", "member_promotion_requested"] {
            assert_eq!(
                key_for(&EMAIL_KIND_TO_SETTING, kind),
                key_for(&EMAIL_KIND_TO_SETTING, "member_added")
            );
        }
        assert_eq!(
            key_for(&EMAIL_KIND_TO_SETTING, "motion_digest"),
            key_for(&EMAIL_KIND_TO_SETTING, "motion")
        );
    }

    /// What `GET /email/preferences` returns: eight keys, in the order
    /// the map first mentions each, `email_welcome` last. The write
    /// side accepts seven of them — welcome is readable but not
    /// settable — so this order is also what that endpoint's shape
    /// depends on.
    #[test]
    fn the_preference_keys_are_the_maps_distinct_keys_in_order() {
        assert_eq!(
            email_pref_keys(),
            vec![
                ("email_camera_offline", true),
                ("email_node_offline", true),
                ("email_incident_created", true),
                ("email_mcp_key_audit", true),
                ("email_cameranode_disk_low", true),
                ("email_member_audit", true),
                ("email_motion", false),
                ("email_welcome", true),
            ]
        );
    }

    #[test]
    fn python_truthiness_decides_whether_meta_is_stored() {
        // Each of these is falsy in Python and stores NULL.
        for falsy in [json!(null), json!({}), json!([]), json!(""), json!(0), json!(false)] {
            assert!(is_falsy(&falsy), "{falsy}");
        }
        for truthy in [json!({"a": 1}), json!([0]), json!("0"), json!(1), json!(true)] {
            assert!(!is_falsy(&truthy), "{truthy}");
        }
        // And what is stored is `json.dumps`-shaped, with the spaces.
        assert_eq!(python_json_value(&json!({"score": 87})), r#"{"score": 87}"#);
    }

    /// The frame the bell receives: `to_dict()`'s keys in order, with
    /// `type` appended and `audience` left where it already was, and
    /// `json.dumps` spacing throughout.
    #[test]
    fn the_broadcast_payload_is_json_dumps_shaped() {
        let row = NotificationRow {
            id: 7,
            kind: "camera_offline".into(),
            audience: "all".into(),
            title: "Café went offline".into(),
            body: String::new(),
            severity: "warning".into(),
            link: Some("/dashboard?camera=cam-1".into()),
            camera_id: Some("cam-1".into()),
            node_id: None,
            meta_json: Some(r#"{"score": 87}"#.into()),
            created_at: Some(
                chrono::NaiveDate::from_ymd_opt(2026, 9, 21)
                    .unwrap()
                    .and_hms_opt(4, 5, 6)
                    .unwrap(),
            ),
        };
        assert_eq!(
            broadcast_payload(&row),
            concat!(
                r#"{"id": 7, "kind": "camera_offline", "audience": "all", "#,
                // Non-ASCII escaped, because json.dumps defaults to
                // ensure_ascii.
                r#""title": "Caf\u00e9 went offline", "body": "", "#,
                r#""severity": "warning", "link": "/dashboard?camera=cam-1", "#,
                r#""camera_id": "cam-1", "node_id": null, "meta": {"score": 87}, "#,
                r#""created_at": "2026-09-21T04:05:06", "type": "notification"}"#
            )
        );
    }

    #[test]
    fn a_transition_is_debounced_per_entity_and_direction() {
        clear_transition_debounce();
        assert!(should_emit_transition("camera", "cam-a", "offline"));
        // The same transition again is dropped.
        assert!(!should_emit_transition("camera", "cam-a", "offline"));
        // The other direction is a different event.
        assert!(should_emit_transition("camera", "cam-a", "online"));
        // So is another camera, and another kind with the same id.
        assert!(should_emit_transition("camera", "cam-b", "offline"));
        assert!(should_emit_transition("node", "cam-a", "offline"));
        clear_transition_debounce();
        // Cleared means the first emit goes through again, which is the
        // "never seen" case the -inf sentinel exists for.
        assert!(should_emit_transition("camera", "cam-a", "offline"));
        clear_transition_debounce();
    }
}
