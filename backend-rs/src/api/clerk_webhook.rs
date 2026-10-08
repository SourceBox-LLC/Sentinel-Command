//! The Clerk webhook: subscription lifecycle, membership audit and org
//! deletion.
//!
//! Ported from `backend/app/api/webhooks.py`. Svix signs it, and the
//! signature is the whole security boundary — this endpoint moves an
//! org's plan, so a forged one is a free upgrade.
//!
//! Two orderings here are load-bearing and neither is obvious:
//!
//! The effective-plan cache is invalidated AFTER the Setting writes and
//! BEFORE the cap re-evaluation reads them. Invalidating first leaves a
//! window where a concurrent reader re-primes the cache with the old
//! plan.
//!
//! A third difference is structural rather than ordered. Python stages
//! its Setting writes with `commit=False` and commits once at the end,
//! so a raise anywhere in a branch discards everything that branch
//! wrote. These writes land immediately. Where a branch can fail AFTER
//! writing — the past-due timestamp is the one that can, on a value
//! big enough to overflow a C year — the fallible part is computed
//! first, so the failure happens before anything is persisted.
//!
//! And the dedup row is written LAST, in a commit of its own. A handler
//! that raises midway must not record itself as done — Svix retries and
//! the operations are built to be safe to re-run. An earlier Python
//! version committed the business writes inside the marker's blanket
//! except, which turned any commit failure into a silent rollback and a
//! 200: the plan change was dropped and Svix never retried.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Map, Value};

use crate::app::AppState;
use crate::error::ApiError;
use crate::notifications::{create_notification, NewNotification};
use crate::plans::{self, PlanContext};
use crate::ratelimit::PerMinute;

/// Member limits per plan. Mirrors `PLAN_LIMITS.max_seats`; the source
/// of truth is plans.rs, and this exists because the Clerk call wants
/// the integer directly.
pub(crate) fn plan_member_limit(slug: &str) -> i64 {
    match slug {
        "pro" => 10,
        "pro_plus" => 20,
        _ => 2,
    }
}

/// The paid slugs this handler treats as "entitled", which is NOT the
/// same set as `plans::PAID_PLAN_SLUGS` — `self_host` has no Clerk
/// subscription and cannot appear here. Kept local, as the Python does,
/// so webhook semantics stay self-contained.
const PAID_PLAN_SLUGS_WEBHOOK: [&str; 2] = ["pro", "pro_plus"];

fn plan_ctx(state: &AppState) -> PlanContext<'_> {
    PlanContext {
        pool: &state.pool,
        client: &state.http,
        clerk_base_url: &state.config.clerk_api_url,
        clerk_secret: &state.config.clerk_secret_key,
        local_auth: state.config.is_local_auth(),
    }
}

/// `data["payer"]["organization_id"]`, absent unless both are objects.
fn payer_org(data: &Map<String, Value>) -> Option<String> {
    data.get("payer")?
        .as_object()?
        .get("organization_id")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `get_active_plan_slug` — what the org is entitled to right now.
///
/// The first `active` item wins. A `canceled` item whose period has not
/// ended also counts: Clerk fires the cancellation the moment the payer
/// clicks, but they keep the features until the period ends, and a
/// `subscription.updated` snapshot taken after that click contains only
/// the canceled-but-paid-through item. Without this rule that snapshot
/// downgraded paying customers on the spot.
fn active_plan_slug(items: &[Value], now: chrono::DateTime<chrono::Utc>) -> String {
    let mut entitled_canceled: Option<String> = None;
    for item in items {
        let Some(item) = item.as_object() else {
            continue;
        };
        let slug = item
            .get("plan")
            .and_then(Value::as_object)
            .and_then(|p| p.get("slug"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let Some(slug) = slug else { continue };
        let status = item
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if status == "active" {
            return slug.to_string();
        }
        if status == "canceled" && entitled_canceled.is_none() {
            if let Some(end) = plans::item_period_end_utc(&Value::Object(item.clone())) {
                if end > now {
                    entitled_canceled = Some(slug.to_string());
                }
            }
        }
    }
    entitled_canceled.unwrap_or_else(|| "free_org".to_string())
}

/// Whether any item is genuinely ACTIVE, as opposed to canceled and
/// still paid through. Both yield a paid slug; only the former is a
/// real (re-)subscription, and only it clears a pending cancellation.
fn has_active_item(items: &[Value]) -> bool {
    items.iter().any(|item| {
        let Some(item) = item.as_object() else {
            return false;
        };
        item.get("status").and_then(Value::as_str) == Some("active")
            && item
                .get("plan")
                .and_then(Value::as_object)
                .and_then(|p| p.get("slug"))
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
    })
}

/// `set_org_member_limit` — PATCH the org's seat cap at Clerk.
///
/// Best-effort by design: a failure here must not fail the webhook, or
/// Svix retries the whole handler over a seat count.
pub(crate) async fn set_org_member_limit(state: &AppState, org_id: &str, limit: i64) {
    let Some(url) = crate::clerk_api::url(&state.config.clerk_api_url, &["organizations", org_id])
    else {
        tracing::error!(org_id, "could not build the Clerk organization URL");
        return;
    };
    let result = state
        .http
        .patch(url)
        .bearer_auth(&state.config.clerk_secret_key)
        .json(&json!({ "max_allowed_memberships": limit }))
        .send()
        .await;
    match result {
        Ok(response) if response.status().is_success() => {
            tracing::info!(org_id, limit, "set org member limit");
        }
        Ok(response) => {
            tracing::error!(org_id, status = %response.status(), "failed to set member limit");
        }
        Err(err) => {
            tracing::error!(org_id, error = %err, "failed to set member limit");
        }
    }
}

async fn set_setting(state: &AppState, org_id: &str, key: &str, value: &str) {
    if let Err(err) = crate::settings::set(&state.pool, org_id, key, value).await {
        tracing::error!(error = %err, org_id, key, "failed to write setting");
    }
}

async fn setting_is_true(state: &AppState, org_id: &str, key: &str) -> bool {
    crate::settings::get(&state.pool, org_id, key, Some("false"))
        .await
        .unwrap_or_default()
        .unwrap_or_default()
        == "true"
}

/// Re-run the cap and log what moved, which is what the Python does in
/// every branch that changes an entitlement.
async fn reenforce(state: &AppState, org_id: &str, what: &str) -> Result<(), ApiError> {
    let ctx = plan_ctx(state);
    let outcome = plans::enforce_camera_cap(&ctx, &state.pool, org_id).await?;
    if outcome.changed {
        tracing::info!(
            org_id,
            what,
            disabled = outcome.disabled.len(),
            enabled = outcome.enabled.len(),
            "plan change moved cameras"
        );
    }
    Ok(())
}

/// `POST /api/webhooks/clerk`.
pub async fn clerk_webhook(
    rate: PerMinute<120>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    rate.check().await?;

    let Some(secret) = state.config.clerk_webhook_secret.as_deref() else {
        tracing::error!("CLERK_WEBHOOK_SECRET not set — cannot verify webhook signatures");
        return Err(ApiError::bad_request("Webhook processing unavailable"));
    };
    if !crate::api::webhooks::verify_svix(secret, &headers, &body, chrono::Utc::now().timestamp()) {
        return Err(ApiError::bad_request("Invalid signature"));
    }
    let Ok(event) = serde_json::from_slice::<Value>(&body) else {
        return Err(ApiError::bad_request("Malformed payload"));
    };
    let Some(event) = event.as_object() else {
        return Err(ApiError::bad_request("Malformed payload"));
    };

    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // `event.get("data", {})` — only an ABSENT key takes the default,
    // so an explicit null stays null and a branch reading it raises.
    let data_value = event.get("data").cloned().unwrap_or_else(|| json!({}));
    let data = data_value.as_object().cloned();

    // Svix retries on any non-2xx, so without this a transient failure
    // re-runs every side effect. Both header conventions are read:
    // verification accepts either, so exactly one namespace is
    // populated afterwards, and reading only `svix-id` would silently
    // disable dedup if Clerk ever migrated.
    let msg_id = headers
        .get("svix-id")
        .or_else(|| headers.get("webhook-id"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !msg_id.is_empty() {
        let seen: Option<(String,)> = sqlx::query_as(
            "SELECT COALESCE(event_type, '') FROM processed_webhooks WHERE svix_msg_id = $1",
        )
        .bind(&msg_id)
        .fetch_optional(&state.pool)
        .await?;
        if let Some((previous,)) = seen {
            tracing::info!(
                msg_id,
                previous,
                "clerk webhook already processed — skipping"
            );
            return Ok(Json(json!({ "status": "duplicate", "svix_id": msg_id })));
        }
    }
    tracing::info!(event_type, "webhook received");

    let dispatched = dispatch(&state, &event_type, data.clone()).await;

    // Recorded only once the branches have run. A handler that raised
    // midway must not mark itself done: Svix retries, and every
    // operation above is built to be safe to re-run, so a lost plan
    // change is strictly worse than a repeated one. Writing the marker
    // first would tell Svix the delivery is finished while the change
    // it carried was rolled back.
    dispatched?;
    record_processed(&state, &msg_id, &event_type).await?;
    Ok(Json(json!({ "received": true })))
}

/// The event branches, split out so the caller can order the dedup
/// write against them in one statement.
async fn dispatch(
    state: &AppState,
    event_type: &str,
    data: Option<Map<String, Value>>,
) -> Result<(), ApiError> {
    // A branch that reads `data` on a non-object payload raises
    // AttributeError, which nothing catches.
    let data_object = || -> Result<Map<String, Value>, ApiError> {
        data.clone()
            .ok_or_else(|| ApiError::internal("webhook data is not an object"))
    };

    match event_type {
        "subscription.created" | "subscription.updated" | "subscription.active" => {
            let data = data_object()?;
            if let Some(org_id) = payer_org(&data) {
                let items: Vec<Value> = data
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let plan_slug = active_plan_slug(&items, chrono::Utc::now());
                let active = has_active_item(&items);
                set_org_member_limit(state, &org_id, plan_member_limit(&plan_slug)).await;
                set_setting(state, &org_id, "org_plan", &plan_slug).await;

                // A paid slug here does NOT mean the card is good.
                // During dunning the item stays active with its paid
                // slug — past-due is a payment state, not an item
                // status. Clearing a HELD flag here would reset the
                // seven-day grace clock on every routine snapshot,
                // including the ones this handler triggers itself via
                // the member-limit PATCH, and let an org with a dead
                // card ride paid caps indefinitely. The authoritative
                // recovery signal is paymentAttempt.updated status=paid.
                if PAID_PLAN_SLUGS_WEBHOOK.contains(&plan_slug.as_str()) {
                    if !setting_is_true(state, &org_id, "payment_past_due").await {
                        set_setting(state, &org_id, "payment_past_due", "false").await;
                        set_setting(state, &org_id, "payment_past_due_at", "").await;
                    }
                    if active {
                        // Only an ACTIVE paid item supersedes a pending
                        // cancellation. Clerk emits subscription.updated
                        // alongside subscriptionItem.canceled, and
                        // clearing it there made the marker useless for
                        // a "cancels at period end" banner.
                        set_setting(state, &org_id, "plan_cancel_pending", "").await;
                    }
                }
                plans::invalidate_effective_plan_cache(Some(&org_id));
                reenforce(state, &org_id, "subscription").await?;
                tracing::info!(org_id, plan = %plan_slug, "subscription active");
            }
        }

        // Clerk's authoritative "this plan's payment went through" is
        // the item-level event. An upgrade delivered only via
        // subscriptionItem.* self-heals the org's plan through
        // resolve_org_plan, but the member limit never moved — a paying
        // org stayed capped at two seats until some subscription.*
        // event happened to fire.
        "subscriptionItem.active" => {
            let data = data_object()?;
            let org_id = payer_org(&data);
            let item_slug = data
                .get("plan")
                .and_then(Value::as_object)
                .and_then(|p| p.get("slug"))
                .and_then(Value::as_str)
                .map(str::to_string);
            if let (Some(org_id), Some(item_slug)) = (org_id, item_slug) {
                // `item_slug in PLAN_MEMBER_LIMITS` — an unknown slug
                // takes no branch at all, rather than the free default.
                if matches!(item_slug.as_str(), "free_org" | "pro" | "pro_plus") {
                    set_org_member_limit(state, &org_id, plan_member_limit(&item_slug)).await;
                    set_setting(state, &org_id, "org_plan", &item_slug).await;
                    set_setting(state, &org_id, "plan_cancel_pending", "").await;
                    if PAID_PLAN_SLUGS_WEBHOOK.contains(&item_slug.as_str()) {
                        set_setting(state, &org_id, "payment_past_due", "false").await;
                        set_setting(state, &org_id, "payment_past_due_at", "").await;
                    }
                    plans::invalidate_effective_plan_cache(Some(&org_id));
                    reenforce(state, &org_id, "item-activated").await?;
                    tracing::info!(org_id, plan = %item_slug, "subscription item active");
                }
            }
        }

        "subscription.pastDue" | "subscriptionItem.pastDue" => {
            let data = data_object()?;
            if let Some(org_id) = payer_org(&data) {
                // The anchor is stamped only when ENTERING past-due.
                // Clerk re-emits this per dunning retry, and
                // overwriting on each one restarted the grace clock
                // every cycle.
                let already = setting_is_true(state, &org_id, "payment_past_due").await;

                // The stamp is computed BEFORE anything is written,
                // although Python computes it after. Python stages its
                // Setting writes and commits at the end, so a raise in
                // here discards them; these writes land immediately, so
                // the same raise would leave `payment_past_due` set to
                // true with no anchor beside it — an org past due with
                // no grace clock. Found by a case carrying a timestamp
                // big enough to raise OverflowError.
                let stamp = if already {
                    None
                } else {
                    Some(past_due_stamp(&data)?)
                };

                set_setting(state, &org_id, "payment_past_due", "true").await;
                plans::invalidate_effective_plan_cache(Some(&org_id));
                if let Some(stamp) = stamp {
                    set_setting(state, &org_id, "payment_past_due_at", &stamp).await;
                }
                tracing::warn!(org_id, "subscription is past due — payment failed");
            }
        }

        "paymentAttempt.updated" => {
            let data = data_object()?;
            let org_id = payer_org(&data);
            let status = data
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(org_id) = org_id {
                if status == "paid" {
                    // The timestamp is cleared too, so a future
                    // past-due starts a fresh window rather than
                    // counting from whenever the old one began.
                    set_setting(state, &org_id, "payment_past_due", "false").await;
                    set_setting(state, &org_id, "payment_past_due_at", "").await;
                    plans::invalidate_effective_plan_cache(Some(&org_id));
                    // Cameras suspended when the grace window expired
                    // come back immediately.
                    reenforce(state, &org_id, "payment restored").await?;
                    tracing::info!(org_id, "payment succeeded — past-due cleared");
                } else if status == "failed" {
                    tracing::warn!(org_id, "payment attempt failed");
                }
            }
        }

        // Scheduled: the payer keeps the features until the period
        // ends. An earlier version downgraded right here, revoking a
        // paid-through month on day one and turning a scheduled
        // pro_plus→pro step into a drop to free.
        "subscriptionItem.canceled" => {
            let data = data_object()?;
            if let Some(org_id) = payer_org(&data) {
                set_setting(state, &org_id, "plan_cancel_pending", "true").await;
                tracing::info!(
                    org_id,
                    "cancellation scheduled — plan retained until period end"
                );
            }
        }

        "subscriptionItem.ended" => {
            let data = data_object()?;
            if let Some(org_id) = payer_org(&data) {
                // One item ending is not necessarily "free": a
                // scheduled pro_plus→pro downgrade ends the pro_plus
                // item while a pro item becomes active. Ask Clerk what
                // the org has now instead of assuming.
                let live = plans::fetch_live_plan_slug(
                    &state.http,
                    &state.config.clerk_api_url,
                    &state.config.clerk_secret_key,
                    &org_id,
                )
                .await;
                let live_slug = live.unwrap_or_else(|| {
                    // The revenue-safe direction. The hourly reconciler
                    // restores a paid slug within the hour if this was
                    // really a downgrade.
                    tracing::warn!(
                        org_id,
                        "item ended but live plan lookup failed — defaulting to free"
                    );
                    "free_org".to_string()
                });
                set_org_member_limit(state, &org_id, plan_member_limit(&live_slug)).await;
                set_setting(state, &org_id, "org_plan", &live_slug).await;
                set_setting(state, &org_id, "plan_cancel_pending", "").await;
                set_setting(state, &org_id, "payment_past_due", "false").await;
                plans::invalidate_effective_plan_cache(Some(&org_id));
                // Rows are suspended, never deleted, so a re-subscribe
                // restores them with their history intact.
                reenforce(state, &org_id, "period end").await?;
                tracing::info!(org_id, plan = %live_slug, "subscription period ended");
            }
        }

        "subscriptionItem.freeTrialEnding" => {
            let data = data_object()?;
            if let Some(org_id) = payer_org(&data) {
                tracing::info!(org_id, "free trial ending in 3 days");
            }
        }

        // The three membership events. Each answers "did someone just
        // add themselves to my org?" in seconds rather than at the next
        // audit-log read. The actor is not always in the payload, so
        // the body describes the result rather than the cause.
        "organizationMembership.created"
        | "organizationMembership.updated"
        | "organizationMembership.deleted" => {
            let data = data_object()?;
            membership_notification(state, event_type, &data).await;
            if event_type == "organizationMembership.deleted" {
                erase_if_account_gone(state, &data).await;
                delete_org_if_empty(state, &data).await;
            }
        }

        // First touch. The creator is automatically the org's first
        // admin, so an admin-audience notification lands on them.
        "organization.created" => {
            let data = data_object()?;
            let org_id = data.get("id").and_then(Value::as_str).unwrap_or_default();
            let org_name = data
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .unwrap_or("your organization");
            if !org_id.is_empty() {
                let mut notification =
                    NewNotification::new("welcome", format!("Welcome to Sentinel, {org_name}"));
                notification.body = "Your Sentinel workspace is ready.  Three steps to your \
                    first live feed: in Settings, click Add Your First Node; run the install \
                    command it shows on the computer your cameras are plugged into; your \
                    cameras appear on the dashboard within about a minute.  Full docs at \
                    https://sentinel-command.com/documentation/."
                    .to_string();
                notification.audience = "admin".to_string();
                notification.link = Some("/dashboard".to_string());
                notification.meta = Some(json!({
                    "org_name": org_name,
                    "created_by": data.get("created_by").cloned().unwrap_or(Value::Null),
                }));
                create_notification(state, org_id, notification).await;
            }
        }

        "organization.deleted" => {
            let data = data_object()?;
            let org_id = data.get("id").and_then(Value::as_str).unwrap_or_default();
            if !org_id.is_empty() {
                // The same helper account deletion uses, so every way an
                // organization disappears reaches the same end state.
                // This branch once cleared seven tables and left motion
                // events, notifications, incidents and the rest behind.
                crate::api::gdpr::erase_org(state, org_id).await?;
            }
        }

        // An account was deleted: in the app, which has already erased
        // it (this run then finds nothing), or in Clerk's own screens,
        // which have not. The payload carries only the id, so the
        // addresses to erase are the ones this database recorded.
        "user.deleted" => {
            let data = data_object()?;
            let user_id = data.get("id").and_then(Value::as_str).unwrap_or_default();
            if !user_id.is_empty() {
                let emails = crate::api::gdpr::recorded_emails(&state.pool, user_id).await?;
                let counts =
                    crate::api::gdpr::erase_user_data(&state.pool, user_id, &emails).await?;
                tracing::info!(user_id, counts = ?counts, "user data erased");
            }
        }

        _ => {}
    }
    Ok(())
}

/// Mark this delivery done so a Svix retry short-circuits.
async fn record_processed(
    state: &AppState,
    msg_id: &str,
    event_type: &str,
) -> Result<(), ApiError> {
    if msg_id.is_empty() {
        return Ok(());
    }
    let inserted = sqlx::query(
        "INSERT INTO processed_webhooks (svix_msg_id, event_type, processed_at)
         VALUES ($1, $2, $3)",
    )
    .bind(msg_id)
    .bind(event_type)
    .bind(crate::models::now_naive())
    .execute(&state.pool)
    .await;
    if let Err(err) = inserted {
        // A concurrent worker recording the same id is benign: both
        // runs reach the same final state, and the caller still tells
        // Svix we are done. Anything else is not swallowed.
        match &err {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                tracing::info!(msg_id, "dedup insert raced with a concurrent worker");
            }
            _ => return Err(err.into()),
        }
    }
    Ok(())
}

/// `past_due_at`, normalised to ISO — or an error, which is a 500.
///
/// Clerk's billing payloads carry epoch MILLISECONDS as snake_case
/// ints; `effective_plan_for_caps` parses ISO. camelCase is read too,
/// in case the shape ever shifts.
///
/// The Python catches `(TypeError, ValueError)` around this and keeps
/// `str(raw)` when it fires — but `datetime.fromtimestamp` has two
/// other failure modes it does NOT catch, and they answer 500 rather
/// than storing a string:
///
///   * a value CPython cannot even hand to the platform's clock
///     conversion (`inf`, or roughly 6.8e16 seconds, where the year
///     overflows a C int) raises OverflowError or OSError;
///   * a value inside that range but outside year 1..9999 raises
///     ValueError, which IS caught.
///
/// So `1e16` stores the string "1e+16" and `1e17` is a 500. The exact
/// cliff is the platform's, not Python's; a case probing its edge is
/// testing libc.
fn past_due_stamp(data: &Map<String, Value>) -> Result<String, ApiError> {
    /// Where CPython stops raising ValueError and starts raising
    /// OSError, measured: the year no longer fits an int.
    const TM_YEAR_LIMIT_SECONDS: f64 = 6.776803619167679e16;

    let now = chrono::Utc::now();
    let now_iso = || crate::api::nodes::iso_aware(now.naive_utc(), 0);

    // `data.get("past_due_at") or data.get("pastDueAt")`. The `or`
    // falls through on any FALSY value, not just an absent key — but
    // what it falls through TO is the second lookup's result whatever
    // that is, including another falsy one. So a payload carrying
    // `past_due_at: 0` and `pastDueAt: 0` parses zero and stamps 1970,
    // while `past_due_at: 0` alone leaves None and stamps now. Reading
    // this as "first truthy wins" gets the second case wrong.
    let first = data.get("past_due_at");
    let raw = match first {
        Some(v) if crate::pyrepr::truthy(v) => Some(v),
        _ => data.get("pastDueAt"),
    };
    let Some(raw) = raw.filter(|v| !v.is_null()) else {
        return Ok(now_iso());
    };

    // `float(raw)`. True is 1.0 in Python; a list or a dict is a
    // TypeError, which the caller catches.
    let as_float = match raw {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => crate::pyrepr::python_float(s),
        _ => None,
    };
    let Some(mut value) = as_float else {
        return Ok(crate::pyrepr::str_value(raw));
    };

    if value.is_nan() {
        // ValueError, caught.
        return Ok(crate::pyrepr::str_value(raw));
    }
    if value > 1e12 {
        value /= 1000.0;
    }
    if !value.is_finite() || value.abs() > TM_YEAR_LIMIT_SECONDS {
        return Err(ApiError::internal("past_due_at is out of range"));
    }

    // Rounded to the microsecond a datetime holds; truncating would
    // put a millisecond value one microsecond early.
    let micros = (value * 1e6).round();
    let stamped = if micros.abs() <= i64::MAX as f64 {
        chrono::DateTime::from_timestamp_micros(micros as i64)
    } else {
        None
    };
    match stamped {
        // Year 1..9999 is what a datetime holds; chrono's range is
        // wider, so the year is checked rather than inferred from a
        // None.
        Some(dt) if (1..=9999).contains(&chrono::Datelike::year(&dt)) => {
            Ok(crate::api::nodes::iso_aware(dt.naive_utc(), 0))
        }
        _ => Ok(crate::pyrepr::str_value(raw)),
    }
}

/// The three membership events, which differ only in wording.
///
/// Every one is best-effort: a notification fault must not make Svix
/// retry, because the membership change happened either way and the
/// retry would just re-notify.
/// After a membership ends, delete the organization if nobody is left.
///
/// An organization with no members can never be opened again, so its
/// data would sit in the database for good. This happens when the last
/// member's account is deleted outside the app's own deletion flow,
/// which deletes such organizations itself. Deleting it at Clerk sends
/// `organization.deleted`, which erases the data.
///
/// Best-effort: a failure is logged, not returned, so Svix does not
/// replay the membership notification over it.
/// When a membership ends because the account was deleted, erase the
/// person's data using the email address this event carries.
///
/// `user.deleted` carries only the user id, so its backstop can find
/// email rows only through addresses this database happened to record
/// next to that id (viewing and audit logs). Clerk also sends a
/// membership deletion for each of the person's organizations, and
/// that one names them (`public_user_data.identifier`). A membership
/// also ends when an admin removes someone, so Clerk is asked first:
/// only an account that no longer exists is erased.
///
/// Best-effort, like the other side effects here: a failure is logged,
/// and the `user.deleted` backstop still runs.
pub async fn erase_if_account_gone(state: &AppState, data: &Map<String, Value>) {
    let user = data.get("public_user_data");
    let Some(user_id) = user
        .and_then(|u| u.get("user_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let clerk = crate::api::account::Clerk::new(state);
    match clerk.user_exists(user_id).await {
        Ok(true) => return,
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(user_id, error = %err, "could not check whether an account was deleted");
            return;
        }
    }
    let mut emails = match crate::api::gdpr::recorded_emails(&state.pool, user_id).await {
        Ok(emails) => emails,
        Err(err) => {
            tracing::error!(user_id, error = %err, "could not read recorded emails");
            Vec::new()
        }
    };
    if let Some(identifier) = user
        .and_then(|u| u.get("identifier"))
        .and_then(Value::as_str)
        .filter(|s| s.contains('@'))
    {
        emails.push(identifier.to_lowercase());
    }
    match crate::api::gdpr::erase_user_data(&state.pool, user_id, &emails).await {
        Ok(counts) => tracing::info!(user_id, counts = ?counts, "deleted account's data erased"),
        Err(err) => {
            tracing::error!(user_id, error = %err, "could not erase a deleted account's data")
        }
    }
}

async fn delete_org_if_empty(state: &AppState, data: &Map<String, Value>) {
    let Some(org_id) = data
        .get("organization")
        .and_then(|o| o.get("id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let clerk = crate::api::account::Clerk::new(state);
    match clerk.organization_member_count(org_id).await {
        Ok(0) => match clerk.delete_organization(org_id).await {
            Ok(()) => tracing::info!(org_id, "deleted an organization left with no members"),
            Err(err) => {
                tracing::error!(org_id, error = %err, "could not delete an empty organization")
            }
        },
        Ok(_) => {}
        // Already deleted (for example, by the account-deletion flow).
        Err(crate::api::account::ClerkError::NotFound) => {}
        Err(err) => {
            tracing::error!(org_id, error = %err, "could not count an organization's members")
        }
    }
}

async fn membership_notification(state: &AppState, event_type: &str, data: &Map<String, Value>) {
    // `data.get("organization") or {}` — a falsy value takes the
    // default, so a null organization is an empty map rather than a
    // raise.
    let org = data
        .get("organization")
        .filter(|v| crate::pyrepr::truthy(v))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let user = data
        .get("public_user_data")
        .filter(|v| crate::pyrepr::truthy(v))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let org_id = org.get("id").and_then(Value::as_str).unwrap_or_default();
    if org_id.is_empty() {
        return;
    }

    let identifier = user
        .get("identifier")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            user.get("user_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("unknown user")
        .to_string();
    // `.replace("org:", "")` replaces every occurrence, not just a
    // prefix, and an empty result falls back to "member".
    let role = data
        .get("role")
        .filter(|v| crate::pyrepr::truthy(v))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .replace("org:", "");
    let role = if role.is_empty() {
        "member".to_string()
    } else {
        role
    };
    let user_id = user.get("user_id").cloned().unwrap_or(Value::Null);
    // A promotion to admin is always worth a warning; a demotion or a
    // member-tier change is informational.
    let severity = if role == "admin" { "warning" } else { "info" };

    let mut notification = match event_type {
        "organizationMembership.created" => {
            let mut n = NewNotification::new("member_added", format!("Member added: {identifier}"));
            n.body = format!(
                "{identifier} was just added to your organization with the {role} role.  \
                 If this was via an invite you sent, no action needed.  If you don't \
                 recognize this user, audit the org's member list and remove any \
                 unexpected accounts."
            );
            n.severity = severity.to_string();
            n.meta = Some(json!({
                "user_id": user_id,
                "identifier": identifier,
                "role": role,
            }));
            n
        }
        "organizationMembership.updated" => {
            let mut n = NewNotification::new(
                "member_role_changed",
                format!("Member role changed: {identifier}"),
            );
            n.body = format!(
                "{identifier}'s role in your organization is now {role}.  Role changes — \
                 especially promotions to admin — are security-relevant.  If you didn't \
                 authorize this change, audit your org's member list immediately."
            );
            n.severity = severity.to_string();
            n.meta = Some(json!({
                "user_id": user_id,
                "identifier": identifier,
                "new_role": role,
            }));
            n
        }
        _ => {
            let mut n =
                NewNotification::new("member_removed", format!("Member removed: {identifier}"));
            n.body = format!(
                "{identifier} was just removed from your organization.  No further access \
                 from this user.  If you didn't expect this removal, audit the org's recent \
                 admin activity."
            );
            // Always informational: a removal cannot be an escalation.
            n.severity = "info".to_string();
            n.meta = Some(json!({
                "user_id": user_id,
                "identifier": identifier,
            }));
            n
        }
    };
    notification.audience = "admin".to_string();
    notification.link = Some("/settings".to_string());
    create_notification(state, org_id, notification).await;
}
