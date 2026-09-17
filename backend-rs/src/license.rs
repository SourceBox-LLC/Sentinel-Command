//! The self-hosted Sentinel licence gate.
//!
//! Ported from `backend/app/core/license_client.py` — the read side.
//! The periodic check-in that writes these Settings belongs with the
//! background loops.
//!
//! Four states, and the distinction between the last two is the whole
//! point of the module:
//!
//! 1. no licence key configured -> not licensed;
//! 2. last check-in reached the service and said valid -> licensed;
//! 3. last check-in did **not** reach the service -> lean on a 72-hour
//!    grace window measured from the last successful check-in;
//! 4. last check-in reached the service and said *invalid* -> fail
//!    closed immediately, with no grace at all.
//!
//! Whether the *current* attempt reached the service is tracked
//! separately from what the last successful answer was. Without that
//! split, a stale "yes" from days ago is indistinguishable from a fresh
//! "no" once check-ins start failing — in one direction that hands out
//! free access forever, and in the other it cuts off a paying customer
//! during an outage.
//!
//! Unlike `plans`, this code is **not** short-circuited under local
//! auth — it is the *hosted* path that short-circuits. So the ordinary
//! HTTP differential can see it, provided `SENTINEL_LICENSE_KEY` is set
//! on both tiers and the Setting rows are seeded.

use chrono::{DateTime, Duration, Utc};

/// Deliberately much shorter than the seven-day payment grace, and in
/// different units. That one absorbs a slow billing-dunning cycle
/// measured in days; this one absorbs a validation-service *outage*,
/// which is an infra-availability question measured in hours. Long
/// enough for a redeploy or a weekend incident, short enough that an
/// abandoned setup does not grant free access indefinitely.
pub const GRACE_HOURS: i64 = 72;

pub const LICENSE_VALID: &str = "sentinel_license_valid";
pub const LAST_CHECK_REACHABLE: &str = "sentinel_license_last_check_reachable";
pub const LAST_OK_AT: &str = "sentinel_license_last_ok_at";
/// A separate opt-in entitlement on the same licence: a licence can be
/// Sentinel-valid without having bought data sync, so this is tracked
/// independently rather than folded into `LICENSE_VALID`.
pub const SYNC_ENABLED: &str = "sentinel_data_sync_enabled";

/// The one plan slug the licence gate applies to.
///
/// `self_host` is Sentinel-eligible by plan alone but *also* needs a
/// valid licence. Every other plan is governed purely by the plan
/// membership check and never reaches this predicate.
const LICENSE_GATED_PLAN: &str = "self_host";

/// Everything the read-side gate needs.
pub struct LicenseContext<'a> {
    pub pool: &'a sqlx::PgPool,
    pub org_id: &'a str,
    /// `AUTH_PROVIDER=local`.
    pub local_auth: bool,
    /// `SENTINEL_LICENSE_KEY`, absent when unconfigured.
    pub license_key: Option<&'a str>,
}

/// Cheap, no network call, safe on every Sentinel-gated request.
///
/// Always true for hosted orgs: this module is a self-host-only
/// concern.
pub async fn is_sentinel_licensed(ctx: &LicenseContext<'_>) -> bool {
    if !ctx.local_auth {
        return true;
    }
    // Only the key's *presence* matters here; its value is checked by
    // the service at check-in time, not locally.
    if ctx.license_key.filter(|k| !k.is_empty()).is_none() {
        return false; // state 1
    }

    let reachable = setting(ctx, LAST_CHECK_REACHABLE).await;
    if reachable == "true" {
        // States 2 and 4: trust the most recent reachable answer
        // directly, with no grace in either direction.
        return setting(ctx, LICENSE_VALID).await == "true";
    }

    // State 3: never reached the service, or the most recent attempt
    // failed. Lean on the grace window.
    let last_ok_raw = setting(ctx, LAST_OK_AT).await;
    if last_ok_raw.is_empty() {
        return false; // no successful check-in to grant grace from
    }
    let Some(last_ok_at) = parse_iso_or_none(&last_ok_raw) else {
        return false;
    };
    Utc::now() - last_ok_at <= Duration::hours(GRACE_HOURS)
}

/// The read-side gate for the cloud data-sync tier.
///
/// Requires both the licence currently being trusted — same
/// reachability and grace semantics, since the risk of syncing a few
/// extra hours past a billing hiccup is minor — and the separate sync
/// entitlement bit.
pub async fn is_sync_enabled(ctx: &LicenseContext<'_>) -> bool {
    if !ctx.local_auth || ctx.license_key.filter(|k| !k.is_empty()).is_none() {
        return false;
    }
    if !is_sentinel_licensed(ctx).await {
        return false;
    }
    setting(ctx, SYNC_ENABLED).await == "true"
}

/// True iff `plan` requires a Sentinel licence and this org does not
/// currently have a valid one.
///
/// The single source of truth for the self-host licence gate. This
/// condition used to be hand-copied at five call sites across three
/// files with nothing keeping them in sync.
pub async fn sentinel_blocked_by_license(ctx: &LicenseContext<'_>, plan: &str) -> bool {
    plan == LICENSE_GATED_PLAN && !is_sentinel_licensed(ctx).await
}

async fn setting(ctx: &LicenseContext<'_>, key: &str) -> String {
    crate::settings::get(ctx.pool, ctx.org_id, key, Some(""))
        .await
        .unwrap_or_default()
        .unwrap_or_default()
}

/// Parse a stored ISO-8601 timestamp, normalising to UTC.
///
/// Every current writer stores a tz-aware string, so the naive branch
/// is unreachable today. It exists because in Python, mixing an aware
/// `now` with a naive value raises `TypeError` — which would turn a
/// fail-closed gate into an unhandled 500 the moment any future writer,
/// migration or manual DB edit stored a naive timestamp here.
fn parse_iso_or_none(raw: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&Utc));
    }
    chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|naive| naive.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_timestamps_parse_aware_and_naive_alike() {
        let aware = parse_iso_or_none("2026-09-01T12:00:00+00:00").unwrap();
        let naive = parse_iso_or_none("2026-09-01T12:00:00").unwrap();
        assert_eq!(aware, naive, "a naive timestamp is read as UTC");
        assert!(parse_iso_or_none("2026-09-01T12:00:00.123456+00:00").is_some());
        // An offset that is not UTC still normalises correctly.
        assert_eq!(
            parse_iso_or_none("2026-09-01T07:00:00-05:00").unwrap(),
            aware
        );
    }

    #[test]
    fn an_unparseable_timestamp_fails_the_gate_closed() {
        // `_parse_iso_or_none` returning None makes `is_sentinel_licensed`
        // return false. Failing open here would hand out Sentinel to any
        // install with a corrupt Setting row.
        for raw in ["", "not a date", "1789678921", "2026-13-45T99:00:00"] {
            assert!(parse_iso_or_none(raw).is_none(), "{raw:?}");
        }
    }

    #[test]
    fn the_grace_window_is_hours_not_days() {
        // Confusing this with PAYMENT_GRACE_DAYS would give a broken
        // install three days of free Sentinel instead of three hours
        // short of that — they are deliberately different units for
        // different failure modes.
        assert_eq!(GRACE_HOURS, 72);
        assert_eq!(Duration::hours(GRACE_HOURS), Duration::days(3));
    }

    #[test]
    fn only_self_host_is_licence_gated() {
        // free_org, pro and pro_plus are governed by plan membership
        // alone and must never reach the licence check.
        assert_eq!(LICENSE_GATED_PLAN, "self_host");
    }
}
