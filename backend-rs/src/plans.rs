//! Plan resolution and cap enforcement.
//!
//! Ported from `backend/app/core/plans.py`. This is the keystone for the
//! ~20 routes that gate on a plan, and the one piece of the port the
//! HTTP differential cannot see at all: `resolve_org_plan` opens with
//! `if settings.is_local_auth(): return "self_host"`, and both tiers in
//! that harness run `AUTH_PROVIDER=local`. Everything below is verified
//! instead by `tests/differential/plan_run.sh`, which drives the real
//! Python module and this module over the same fake Clerk and compares
//! the two.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::Value;

/// Grace window after a failed payment before caps tighten.
///
/// Seven days matches the industry norm and Clerk's default Stripe
/// dunning schedule — by day 7 the card has been retried three or four
/// times. The ToS and pricing page both quote this number.
///
/// It is a *soft* cap: banners and the MCP 402 fire immediately, and
/// only the camera rebase waits for the grace to expire.
pub const PAYMENT_GRACE_DAYS: i64 = 7;

/// Slugs that count as paid. A cached value in this set short-circuits
/// `resolve_org_plan` before it ever asks Clerk.
pub const PAID_PLAN_SLUGS: [&str; 3] = ["pro", "pro_plus", "self_host"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanLimits {
    pub max_cameras: i64,
    pub max_nodes: i64,
    pub max_seats: i64,
    pub max_viewer_hours_per_month: i64,
    pub max_sse_subscribers: i64,
    pub log_retention_days: i64,
}

impl PlanLimits {
    /// The dict `get_plan_limits` returns, in its key order — this is a
    /// response body, not a struct dump, and `/api/nodes/plan` hands it
    /// to the dashboard whole.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "max_cameras": self.max_cameras,
            "max_nodes": self.max_nodes,
            "max_seats": self.max_seats,
            "max_viewer_hours_per_month": self.max_viewer_hours_per_month,
            "max_sse_subscribers": self.max_sse_subscribers,
            "log_retention_days": self.log_retention_days,
        })
    }
}

/// Hardware caps are sized as abuse rails rather than product
/// differentiators — almost no legitimate customer hits them. The
/// binding constraint for an upgrade decision is
/// `max_viewer_hours_per_month`, because that is what drives egress
/// cost.
pub fn get_plan_limits(plan: &str) -> PlanLimits {
    match plan {
        "pro" => PlanLimits {
            max_cameras: 25,
            max_nodes: 10,
            max_seats: 10,
            max_viewer_hours_per_month: 300,
            max_sse_subscribers: 30,
            log_retention_days: 90,
        },
        "pro_plus" => PlanLimits {
            max_cameras: 200,
            max_nodes: 999,
            max_seats: 20,
            max_viewer_hours_per_month: 1500,
            max_sse_subscribers: 100,
            log_retention_days: 365,
        },
        "self_host" => PlanLimits {
            max_cameras: 999,
            max_nodes: 999,
            max_seats: 1,
            max_viewer_hours_per_month: 999_999,
            max_sse_subscribers: 50,
            log_retention_days: 365,
        },
        // Anything unrecognised — including a slug Clerk reports that
        // has no entry here — falls back to the free tier's limits
        // while still being *reported* as itself.
        _ => PlanLimits {
            max_cameras: 5,
            max_nodes: 2,
            max_seats: 2,
            max_viewer_hours_per_month: 30,
            max_sse_subscribers: 10,
            log_retention_days: 30,
        },
    }
}

pub fn get_plan_display_name(plan: &str) -> &'static str {
    match plan {
        "pro" => "Pro",
        "pro_plus" => "Pro Plus",
        "self_host" => "Self-Hosted",
        _ => "Free",
    }
}

// ---------------------------------------------------------------------
// The two in-process caches
// ---------------------------------------------------------------------

/// Minimum interval between live Clerk lookups for one org.
const RESOLVE_THROTTLE: Duration = Duration::from_secs(60);
/// How long a computed effective plan is reused.
///
/// The HLS segment path calls this on every segment of every viewer —
/// roughly once a second each — so without a cache it is two or three
/// Setting queries per request, and for an org whose cached slug is not
/// paid, a live Clerk lookup as well.
const EFFECTIVE_TTL: Duration = Duration::from_secs(30);
const RESOLVE_PRUNE_INTERVAL: Duration = Duration::from_secs(600);

#[derive(Default)]
struct Caches {
    /// org -> when its last live lookup was attempted.
    last_resolve_at: HashMap<String, Instant>,
    last_prune_at: Option<Instant>,
    /// org -> (expiry, slug).
    effective: HashMap<String, (Instant, String)>,
}

static CACHES: Mutex<Option<Caches>> = Mutex::new(None);

fn with_caches<T>(f: impl FnOnce(&mut Caches) -> T) -> T {
    let mut guard = CACHES.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(Caches::default))
}

/// Drop the cached effective plan for one org, or all of them.
///
/// Called by the webhook handlers after a plan write so an upgrade or a
/// cancellation applies immediately rather than after the TTL.
pub fn invalidate_effective_plan_cache(org_id: Option<&str>) {
    with_caches(|c| match org_id {
        Some(org) => {
            c.effective.remove(org);
        }
        None => c.effective.clear(),
    });
}

/// Drop the per-org live-lookup throttle. Test-facing: the plan probe
/// clears both caches between cases so each one exercises the path it
/// names rather than a leftover from the case before it.
pub fn reset_resolve_throttle() {
    with_caches(|c| {
        c.last_resolve_at.clear();
        c.last_prune_at = None;
    });
}

/// An entry older than the throttle window is inert, so it can go.
///
/// Without this sweep the map accumulates one entry per org that ever
/// took the live path and never releases them — a slow leak dominated
/// by free-tier orgs hammering MCP, which is exactly the population the
/// throttle targets.
fn prune_resolve_cache(c: &mut Caches, now: Instant) {
    if let Some(last) = c.last_prune_at {
        if now.duration_since(last) < RESOLVE_PRUNE_INTERVAL {
            return;
        }
    }
    c.last_prune_at = Some(now);
    c.last_resolve_at
        .retain(|_, at| now.duration_since(*at) < RESOLVE_THROTTLE);
}

// ---------------------------------------------------------------------
// The live Clerk lookup
// ---------------------------------------------------------------------

/// Best-effort parse of a subscription item's period end.
///
/// Clerk surfaces it as epoch **milliseconds** in webhook JSON and as
/// either a datetime or an integer on SDK objects. A value above 1e12
/// is milliseconds; below that it is seconds. Returns `None` when the
/// field is absent or unparseable rather than guessing.
pub fn item_period_end_utc(item: &Value) -> Option<DateTime<Utc>> {
    let raw = item
        .get("period_end")
        .or_else(|| item.get("periodEnd"))
        .filter(|v| !v.is_null())?;

    if let Some(s) = raw.as_str() {
        // A datetime on the SDK object arrives here as a string.
        return DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Utc));
    }
    let val = raw.as_f64()?;
    let seconds = if val > 1e12 { val / 1000.0 } else { val };
    DateTime::from_timestamp(seconds as i64, 0)
}

/// Resolve the org's current entitlement from Clerk, live.
///
/// `None` means the lookup itself failed — a network or Clerk error —
/// so a caller can tell "Clerk says free" from "couldn't ask Clerk".
/// The difference matters: the second must never downgrade an org.
///
/// Entitlement rules, matching Clerk's billing semantics:
///   * the first `active` item wins;
///   * a `canceled` item whose `period_end` is still in the future
///     counts as entitled, because cancellation is *scheduled* — the
///     payer keeps the features until the period ends.
pub async fn fetch_live_plan_slug(
    client: &reqwest::Client,
    base_url: &str,
    secret: &str,
    org_id: &str,
) -> Option<String> {
    // Built through `Url` rather than `format!`: an org id is opaque
    // and percent-encoding it by hand is the kind of thing that works
    // until it doesn't.
    //
    // The trailing slash is load-bearing. `Url::join` treats the base's
    // last segment as a *file* and replaces it, so joining against
    // `https://api.clerk.com/v1` would quietly drop the `/v1` and send
    // every request to the wrong path.
    let base = if base_url.ends_with('/') {
        base_url.to_string()
    } else {
        format!("{base_url}/")
    };
    let url = match reqwest::Url::parse(&base)
        .and_then(|base| base.join(&format!("organizations/{org_id}/billing/subscription")))
    {
        Ok(url) => url,
        Err(err) => {
            tracing::warn!(error = %err, org_id, "could not build the Clerk billing URL");
            return None;
        }
    };
    let response = match client.get(url).bearer_auth(secret).send().await {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(error = %err, org_id, "live Clerk plan lookup failed");
            return None;
        }
    };
    if !response.status().is_success() {
        tracing::warn!(status = %response.status(), org_id, "live Clerk plan lookup failed");
        return None;
    }
    let sub: Value = match response.json().await {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, org_id, "live Clerk plan lookup failed");
            return None;
        }
    };

    let items = match sub.get("subscription_items") {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };

    let mut entitled_canceled: Option<String> = None;
    let now = Utc::now();
    for item in &items {
        let status = item.get("status").and_then(Value::as_str);
        let slug = item
            .get("plan")
            .and_then(|p| p.get("slug"))
            .and_then(Value::as_str);
        // Python's `if not slug: continue` — an absent plan and an
        // empty slug are both skipped rather than read as free.
        let Some(slug) = slug.filter(|s| !s.is_empty()) else {
            continue;
        };
        if status == Some("active") {
            return Some(slug.to_string());
        }
        if status == Some("canceled") && entitled_canceled.is_none() {
            if let Some(period_end) = item_period_end_utc(item) {
                if period_end > now {
                    entitled_canceled = Some(slug.to_string());
                }
            }
        }
    }
    Some(entitled_canceled.unwrap_or_else(|| "free_org".to_string()))
}

// ---------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------

/// Everything the resolvers need that is not the database.
pub struct PlanContext<'a> {
    pub pool: &'a sqlx::PgPool,
    pub client: &'a reqwest::Client,
    pub clerk_base_url: &'a str,
    pub clerk_secret: &'a str,
    /// `AUTH_PROVIDER=local`.
    pub local_auth: bool,
}

/// The current plan slug for an org, falling back to Clerk.
///
/// Read order:
///   1. the cached `Setting(org_plan)`, written by the Clerk webhook —
///      if it names a paid plan, return immediately;
///   2. a live lookup, which fixes orgs whose subscription webhook
///      never fired. The fresh slug is written back so later calls take
///      the fast path.
///
/// Live lookups are throttled to one per org per minute so a free-tier
/// caller hammering MCP cannot drive Clerk API spend.
///
/// The self-hosted short-circuit is the single choke point for every
/// non-JWT plan lookup — node registration, MCP, Sentinel dispatch, the
/// hourly reconcile. Patching only the JWT claim would miss all of
/// them, because none of them build an `AuthUser`.
pub async fn resolve_org_plan(ctx: &PlanContext<'_>, org_id: &str) -> String {
    if ctx.local_auth {
        return "self_host".to_string();
    }

    let cached = crate::settings::get(ctx.pool, org_id, "org_plan", Some(""))
        .await
        .unwrap_or_default()
        .unwrap_or_default();
    if PAID_PLAN_SLUGS.contains(&cached.as_str()) {
        return cached;
    }

    let now = Instant::now();
    let throttled = with_caches(|c| {
        prune_resolve_cache(c, now);
        if let Some(at) = c.last_resolve_at.get(org_id) {
            if now.duration_since(*at) < RESOLVE_THROTTLE {
                return true;
            }
        }
        // Recorded *before* the network call, matching the Python:
        // concurrent callers for the same org back off instead of
        // piling up on Clerk.
        c.last_resolve_at.insert(org_id.to_string(), now);
        false
    });
    if throttled {
        return non_empty_or_free(cached);
    }

    let Some(live_slug) =
        fetch_live_plan_slug(ctx.client, ctx.clerk_base_url, ctx.clerk_secret, org_id).await
    else {
        // The lookup failed. Keep whatever was cached rather than
        // downgrading an org on a Clerk hiccup.
        return non_empty_or_free(cached);
    };

    if live_slug != cached {
        if let Err(err) = crate::settings::set(ctx.pool, org_id, "org_plan", &live_slug).await {
            tracing::error!(error = %err, org_id, "failed to persist resolved plan");
        } else {
            tracing::info!(org_id, cached = %cached, live = %live_slug, "resolved org plan from Clerk");
        }
    }
    live_slug
}

fn non_empty_or_free(cached: String) -> String {
    if cached.is_empty() {
        "free_org".to_string()
    } else {
        cached
    }
}

/// The plan slug to use for **cap enforcement**, accounting for the
/// past-due grace period.
///
/// Use this everywhere a runtime cap is checked. Do *not* use it for
/// the status-bar badge: operators want to see the plan they pay for,
/// not a silent downgrade during a brief card failure.
///
/// `require_active_billing` still refuses MCP credential creation
/// immediately on past-due with no grace, because issuing fresh
/// credentials to a failing card is a different risk from letting
/// existing cameras keep streaming for a week.
pub async fn effective_plan_for_caps(ctx: &PlanContext<'_>, org_id: &str, use_cache: bool) -> String {
    if use_cache {
        let now = Instant::now();
        let hit = with_caches(|c| {
            c.effective
                .get(org_id)
                .filter(|(expiry, _)| *expiry > now)
                .map(|(_, slug)| slug.clone())
        });
        if let Some(slug) = hit {
            return slug;
        }
    }

    let nominal = resolve_org_plan(ctx, org_id).await;

    // A self-hosted install is never past-due: nothing writes
    // payment_past_due for the local org, because there is no billing
    // webhook in that mode. Short-circuited explicitly rather than
    // relying on the key merely being unset, so it holds even if
    // something stray writes it.
    if ctx.local_auth {
        return cache_effective(org_id, nominal);
    }

    let past_due = crate::settings::get(ctx.pool, org_id, "payment_past_due", Some("false"))
        .await
        .unwrap_or_default()
        .unwrap_or_default()
        == "true";
    if !past_due {
        return cache_effective(org_id, nominal);
    }

    let past_due_at = crate::settings::get(ctx.pool, org_id, "payment_past_due_at", Some(""))
        .await
        .unwrap_or_default()
        .unwrap_or_default();
    if past_due_at.is_empty() {
        // Flagged with no timestamp: conservative, since there is no
        // way to tell how long it has been past due. The banner still
        // fires and MCP is still blocked.
        return cache_effective(org_id, nominal);
    }

    let Some(dt) = parse_past_due_at(&past_due_at) else {
        // Surfacing a bug loudly beats silently suspending cameras on a
        // parse error.
        tracing::warn!(
            org_id,
            value = %past_due_at,
            "unparseable payment_past_due_at — keeping nominal plan"
        );
        return cache_effective(org_id, nominal);
    };

    let age = Utc::now() - dt;
    if age > chrono::Duration::days(PAYMENT_GRACE_DAYS) {
        tracing::info!(org_id, "past due beyond grace — tightening caps to free tier");
        return cache_effective(org_id, "free_org".to_string());
    }
    cache_effective(org_id, nominal)
}

/// Timestamps from Clerk arrive as ISO 8601, with or without a `Z`.
/// A naive one is read as UTC, matching Python's
/// `dt.replace(tzinfo=UTC)` when `dt.tzinfo is None`.
fn parse_past_due_at(raw: &str) -> Option<DateTime<Utc>> {
    let normalised = raw.replace('Z', "+00:00");
    if let Ok(dt) = DateTime::parse_from_rfc3339(&normalised) {
        return Some(dt.with_timezone(&Utc));
    }
    chrono::NaiveDateTime::parse_from_str(&normalised, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|naive| naive.and_utc())
}

fn cache_effective(org_id: &str, slug: String) -> String {
    with_caches(|c| {
        c.effective
            .insert(org_id.to_string(), (Instant::now() + EFFECTIVE_TTL, slug.clone()));
        // Opportunistic bound: prune expired entries once the map grows
        // past a sane fleet size.
        if c.effective.len() > 10_000 {
            let now = Instant::now();
            c.effective.retain(|_, (expiry, _)| *expiry > now);
        }
    });
    slug
}

/// `wire_plan_slug` — the plan string CameraNode renders in its status
/// bar.
///
/// The `_org` suffix is internal, so the node shows `[ FREE ]` rather
/// than `[ FREE_ORG ]`. An unknown slug passes through untouched, so a
/// tier shipped before a node update still shows its own name instead
/// of a fallback. The node treats the field as advisory — enforcement
/// is here — so a stale value costs a label and nothing else.
pub fn wire_plan_slug(plan: &str) -> String {
    let plan = plan.trim().to_lowercase();
    if plan.is_empty() {
        return "free".to_string();
    }
    match plan.strip_suffix("_org") {
        Some(stripped) => stripped.to_string(),
        None => plan,
    }
}

/// What `enforce_camera_cap` decided.
pub struct CapOutcome {
    pub plan: String,
    pub max_cameras: i64,
    pub enabled: Vec<String>,
    pub disabled: Vec<String>,
    pub changed: bool,
}

/// `enforce_camera_cap(db, org_id)` — keep the oldest `max_cameras`,
/// flag the rest.
///
/// Oldest-first is deterministic, needs no input from anyone, and
/// preserves the cameras most likely to have history someone cares
/// about; a camera plugged in this week is the easier one to re-add.
///
/// Nothing is deleted. The flag is read at upload time, where
/// `push-segment` answers 402 — so raising the cap lights the same rows
/// back up with their metadata intact.
///
/// **The plan is read with the cache bypassed.** This runs immediately
/// after a plan write — a webhook, the reconciler, a registration — and
/// a thirty-second-stale slug would flip cameras against the plan the
/// org just left. That TTL exists for the per-segment serve path, not
/// for writes.
pub async fn enforce_camera_cap(
    ctx: &PlanContext<'_>,
    pool: &sqlx::PgPool,
    org_id: &str,
) -> Result<CapOutcome, sqlx::Error> {
    let plan_slug = effective_plan_for_caps(ctx, org_id, false).await;
    let cap = get_plan_limits(&plan_slug).max_cameras;

    // `created_at ASC NULLS LAST, id ASC`. The null case should not
    // arise — the column has a default — but ordering by it silently
    // puts nulls first in Postgres, which would disable the oldest
    // cameras rather than the newest.
    let cameras: Vec<(String, Option<bool>)> = sqlx::query_as(
        "SELECT camera_id, disabled_by_plan FROM cameras
          WHERE org_id = $1
          ORDER BY created_at ASC NULLS LAST, id ASC",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;

    let keep = cap.max(0) as usize;
    let keep_ids: std::collections::HashSet<&str> = cameras
        .iter()
        .take(keep)
        .map(|(camera_id, _)| camera_id.as_str())
        .collect();
    // The returned `disabled` is this slice, not the list the loop
    // below builds. They differ only if two cameras in one org share a
    // camera_id, which nothing prevents.
    let disabled: Vec<String> = cameras
        .iter()
        .skip(keep)
        .map(|(camera_id, _)| camera_id.clone())
        .collect();

    let mut changed = false;
    let mut enabled = Vec::new();
    let mut flip_to = Vec::new();
    for (camera_id, currently) in &cameras {
        let should_disable = !keep_ids.contains(camera_id.as_str());
        // `bool(cam.disabled_by_plan)` — a null column is false.
        if currently.unwrap_or(false) != should_disable {
            flip_to.push((camera_id.clone(), should_disable));
            changed = true;
        }
        if !should_disable {
            enabled.push(camera_id.clone());
        }
    }

    // Only the rows that actually flip, so an idempotent call writes
    // nothing — which is what makes this safe on every registration and
    // every subscription webhook.
    for (camera_id, disable) in flip_to {
        sqlx::query(
            "UPDATE cameras SET disabled_by_plan = $1, updated_at = $2
              WHERE camera_id = $3 AND org_id = $4",
        )
        .bind(disable)
        .bind(crate::models::now_naive())
        .bind(&camera_id)
        .bind(org_id)
        .execute(pool)
        .await?;
    }

    if changed {
        tracing::info!(
            org_id, plan = %plan_slug, cap,
            enabled = enabled.len(), disabled = disabled.len(),
            "enforce_camera_cap"
        );
    }

    Ok(CapOutcome {
        plan: wire_plan_slug(&plan_slug),
        max_cameras: cap,
        enabled,
        disabled,
        changed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The label CameraNode paints in its status bar.
    #[test]
    fn the_wire_slug_drops_the_internal_suffix() {
        use super::wire_plan_slug;
        assert_eq!(wire_plan_slug("free_org"), "free");
        assert_eq!(wire_plan_slug("pro"), "pro");
        assert_eq!(wire_plan_slug("pro_plus"), "pro_plus");
        assert_eq!(wire_plan_slug("self_host"), "self_host");
        // Case and surrounding space are normalised away.
        assert_eq!(wire_plan_slug("  FREE_ORG "), "free");
        // Empty is the free tier, not an empty label.
        assert_eq!(wire_plan_slug(""), "free");
        assert_eq!(wire_plan_slug("   "), "free");
        // An unknown tier passes through, so a plan shipped before a
        // node update still shows its own name.
        assert_eq!(wire_plan_slug("enterprise"), "enterprise");
        // Only a *suffix* is stripped.
        assert_eq!(wire_plan_slug("org_free"), "org_free");
        assert_eq!(wire_plan_slug("_org"), "");
    }

    #[test]
    fn an_unknown_slug_gets_free_tier_limits_but_keeps_its_name() {
        // `PLAN_LIMITS.get(plan, PLAN_LIMITS["free_org"])` — the limits
        // fall back, the slug does not. An org on a bespoke Clerk plan
        // is reported as itself and capped conservatively, rather than
        // silently renamed to free_org.
        assert_eq!(get_plan_limits("enterprise_custom"), get_plan_limits("free_org"));
        assert_eq!(get_plan_display_name("enterprise_custom"), "Free");
        assert_eq!(get_plan_display_name("pro_plus"), "Pro Plus");
        assert_eq!(get_plan_display_name("self_host"), "Self-Hosted");
    }

    #[test]
    fn the_paid_set_is_exactly_the_three_that_short_circuit() {
        // A slug in this set skips the live Clerk lookup entirely, so
        // adding one here silently stops an org from being re-checked.
        assert_eq!(PAID_PLAN_SLUGS, ["pro", "pro_plus", "self_host"]);
        assert!(!PAID_PLAN_SLUGS.contains(&"free_org"));
    }

    #[test]
    fn period_end_is_read_as_milliseconds_above_the_1e12_line() {
        // Clerk sends epoch milliseconds in webhook JSON and sometimes
        // seconds on SDK objects; 1e12 is the discriminator Python
        // uses. Read the wrong way round, a live subscription looks
        // like one that expired in 1970 — or one that expires in the
        // year 33000.
        let ms = item_period_end_utc(&json!({"period_end": 1_789_678_921_046_i64})).unwrap();
        assert_eq!(ms.timestamp(), 1_789_678_921);

        let secs = item_period_end_utc(&json!({"period_end": 1_789_678_921_i64})).unwrap();
        assert_eq!(secs.timestamp(), 1_789_678_921);

        // camelCase is accepted too, matching the dict branch.
        assert!(item_period_end_utc(&json!({"periodEnd": 1_789_678_921_i64})).is_some());
    }

    #[test]
    fn an_absent_or_unusable_period_end_is_none_rather_than_a_guess() {
        // None means "not entitled via this item". Guessing a value
        // would either strand a payer or extend a cancelled plan.
        for item in [
            json!({}),
            json!({"period_end": null}),
            json!({"period_end": "not a date"}),
            json!({"period_end": true}),
        ] {
            assert!(item_period_end_utc(&item).is_none(), "{item}");
        }
        // A datetime on an SDK object arrives as an RFC 3339 string.
        assert!(item_period_end_utc(&json!({"period_end": "2027-01-01T00:00:00Z"})).is_some());
    }

    #[test]
    fn past_due_timestamps_parse_in_all_three_spellings_clerk_emits() {
        // Python does `datetime.fromisoformat(v.replace("Z", "+00:00"))`
        // and then `dt.replace(tzinfo=UTC)` when the result is naive.
        let aware = parse_past_due_at("2026-09-01T12:00:00+00:00").unwrap();
        let zulu = parse_past_due_at("2026-09-01T12:00:00Z").unwrap();
        let naive = parse_past_due_at("2026-09-01T12:00:00").unwrap();
        assert_eq!(aware, zulu);
        assert_eq!(aware, naive, "a naive timestamp is read as UTC, not local");

        // Microseconds appear on timestamps this service writes itself.
        assert!(parse_past_due_at("2026-09-01T12:00:00.123456").is_some());
        assert!(parse_past_due_at("2026-09-01T12:00:00.123456Z").is_some());
    }

    #[test]
    fn an_unparseable_past_due_timestamp_is_none_so_caps_stay_nominal() {
        // Deliberate: surfacing a bug loudly beats silently suspending
        // an org's cameras because a timestamp did not parse.
        for raw in ["", "not a date", "2026-13-45T99:00:00", "1789678921"] {
            assert!(parse_past_due_at(raw).is_none(), "{raw:?}");
        }
    }

    #[test]
    fn a_failed_lookup_keeps_the_cached_value_rather_than_downgrading() {
        // `cached or "free_org"`: an empty cache becomes free, but a
        // cached slug survives a Clerk hiccup. Downgrading an org
        // because Clerk was briefly unreachable would disable their
        // cameras.
        assert_eq!(non_empty_or_free(String::new()), "free_org");
        assert_eq!(non_empty_or_free("pro".into()), "pro");
        assert_eq!(non_empty_or_free("nonsense".into()), "nonsense");
    }

    #[test]
    fn the_effective_cache_holds_and_can_be_invalidated() {
        let org = "org_cache_test";
        invalidate_effective_plan_cache(None);
        cache_effective(org, "pro".into());
        let hit = with_caches(|c| c.effective.get(org).map(|(_, s)| s.clone()));
        assert_eq!(hit.as_deref(), Some("pro"));

        // Webhook handlers call this after a plan write so an upgrade
        // applies immediately rather than after the 30s TTL.
        invalidate_effective_plan_cache(Some(org));
        assert!(with_caches(|c| !c.effective.contains_key(org)));
    }

    #[test]
    fn the_resolve_throttle_prune_drops_only_expired_entries() {
        reset_resolve_throttle();
        let now = Instant::now();
        with_caches(|c| {
            c.last_resolve_at.insert("fresh".into(), now);
            c.last_resolve_at
                .insert("stale".into(), now - RESOLVE_THROTTLE - Duration::from_secs(1));
            // Not yet due: the sweep is time-gated so the common path
            // is a single comparison.
            c.last_prune_at = Some(now);
            prune_resolve_cache(c, now);
            assert_eq!(c.last_resolve_at.len(), 2, "the sweep ran before it was due");

            c.last_prune_at = Some(now - RESOLVE_PRUNE_INTERVAL - Duration::from_secs(1));
            prune_resolve_cache(c, now);
            assert!(c.last_resolve_at.contains_key("fresh"));
            assert!(!c.last_resolve_at.contains_key("stale"));
        });
        reset_resolve_throttle();
    }
}
