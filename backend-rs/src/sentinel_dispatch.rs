//! Whether a notification should wake the Sentinel agent.
//!
//! Ported from the dispatch half of
//! `backend/app/core/sentinel_dispatch.py`. The manual "Run now" path
//! already lives in `api/sentinel_config.rs`, because it is a route;
//! this is the other entry point, called from `create_notification` for
//! every notification the app emits.
//!
//! **It can never fail its caller.** A run that should have queued is
//! regrettable; an exception that 500s a motion event is far worse. So
//! every error here is logged and swallowed, and the notification goes
//! out regardless.
//!
//! The gates run cheapest-first, which is not merely an optimisation:
//! the motion cooldown is a query against `sentinel_runs`, and running
//! it before the enabled/plan/trigger checks would put a table scan on
//! the motion path of every org that has Sentinel switched off.
//!
//! **The feedback-loop guard is the subtle one.** `incident_created`
//! fires whenever an incident is filed — including by the agent itself,
//! through MCP. Without the guard: motion → run → create_incident →
//! `incident_created` → another run → another incident, self-amplifying
//! until the org's monthly cap is gone, with an email to every member
//! per cycle. The trigger was designed around "a one-shot human
//! action", and that stopped being true the day agents became incident
//! authors.

use chrono::NaiveDateTime;
use serde_json::Value;

use crate::api::sentinel_config::{
    cap_for_plan, fetch_config, fire_wakeup_webhook, license_ctx, plan_ctx, plan_has_sentinel,
    runs_used_this_month, start_of_month, ConfigRow,
};
use crate::app::AppState;
use crate::license::sentinel_blocked_by_license;
use crate::models::now_naive;
use crate::plans::effective_plan_for_caps;

/// `_KIND_TO_TRIGGER_FIELD`: notification kind → the config flag that
/// has to be on.
///
/// `incident_created` is the kind a filed incident emits; the UI calls
/// the same trigger `incident_opened`, which is why the two names do
/// not match.
const KIND_TO_TRIGGER_FIELD: [(&str, &str); 2] = [
    ("motion", "motion_enabled"),
    ("incident_created", "incident_opened_enabled"),
];

/// `_FIELD_TO_TRIGGER_TYPE`: the label that lands in
/// `sentinel_runs.trigger_type`, which the dashboard colours by.
fn trigger_type_for(field: &str) -> &str {
    match field {
        "motion_enabled" => "motion",
        "incident_opened_enabled" => "incident_opened",
        other => other,
    }
}

const DAY_KEYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// Why a dispatch did not happen. Logged, never returned to a caller —
/// but each is a distinct branch, and naming them is what makes the
/// log readable when an operator asks why the agent stayed quiet.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    SentinelDisabled,
    PlanNotEligible,
    LicenseRequired,
    KindNotATrigger,
    TriggerOff(&'static str),
    CameraOutOfScope,
    OutsideScheduleWindow,
    MotionCooldownActive,
    MonthlyCapReached,
    /// Not one of Python's reasons: there, a failed lookup raises and
    /// the blanket handler logs "dispatch failed silently". Same
    /// outcome — no run — but the log says which it was.
    LookupFailed,
}

impl Refusal {
    fn as_str(&self) -> String {
        match self {
            Refusal::SentinelDisabled => "sentinel_disabled".into(),
            Refusal::PlanNotEligible => "plan_not_eligible".into(),
            Refusal::LicenseRequired => "license_required".into(),
            Refusal::KindNotATrigger => "kind_not_a_sentinel_trigger".into(),
            Refusal::TriggerOff(field) => format!("trigger_{field}_off"),
            Refusal::CameraOutOfScope => "camera_out_of_scope".into(),
            Refusal::OutsideScheduleWindow => "outside_schedule_window".into(),
            Refusal::MotionCooldownActive => "motion_cooldown_active".into(),
            Refusal::MonthlyCapReached => "monthly_cap_reached".into(),
            Refusal::LookupFailed => "lookup_failed".into(),
        }
    }
}

/// `_is_camera_in_scope`.
///
/// Absent means in scope, so a camera added after the scope was last
/// saved does not silently fall out of the agent's purview. Only an
/// explicit `false` excludes it. A trigger with no camera at all — a
/// scheduled sweep — is always in scope; the agent decides what to look
/// at.
pub fn is_camera_in_scope(scope: &Value, camera_id: Option<&str>) -> bool {
    let Some(camera_id) = camera_id.filter(|id| !id.is_empty()) else {
        return true;
    };
    !matches!(scope.get(camera_id), Some(Value::Bool(false)))
}

/// `_parse_hhmm_to_minutes`: `HH:MM`, or a bare `HH`, as minutes since
/// midnight, clamped to a day.
///
/// An earlier version read only the hour, so `22:30 → 23:00` quietly
/// behaved as `22:00 → 23:00`. Anything unparseable takes the default
/// rather than raising, because this runs on the motion path.
pub fn parse_hhmm_to_minutes(value: &str, default: i64) -> i64 {
    let mut parts = value.split(':');
    let Some(hours) = parts.next().and_then(crate::api::sentinel_config::python_int) else {
        return default;
    };
    // Only the second field is read; `22:30:45` is 22:30, as Python's
    // `parts[1]` is.
    let minutes = match parts.next() {
        Some(raw) => match crate::api::sentinel_config::python_int(raw) {
            Some(value) => value,
            None => return default,
        },
        None => 0,
    };
    hours
        .saturating_mul(60)
        .saturating_add(minutes)
        .clamp(0, 24 * 60)
}

/// `_schedule_allows_now`.
///
/// `always` and `off` answer without looking at the clock. `scheduled`
/// reads the org's own timezone, because "22:00 to 06:00" means the
/// operator's night, not UTC's — an org in UTC+13 would otherwise be
/// watched through the middle of its afternoon.
pub(crate) async fn schedule_allows_now(
    state: &AppState,
    org_id: &str,
    cfg: &ConfigRow,
    now: jiff::Timestamp,
) -> bool {
    match cfg.schedule_mode.as_str() {
        "off" => return false,
        "scheduled" => {}
        // `cfg.schedule_mode or "always"` — an empty string is falsy in
        // Python and reads as `always`, as does any other value.
        _ => return true,
    }

    let tz_name = crate::settings::get(&state.pool, org_id, "timezone", Some("UTC"))
        .await
        .ok()
        .flatten()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "UTC".to_string());
    // An unknown or malformed name falls back to UTC rather than
    // refusing to dispatch.
    let zone = crate::zoneinfo::load(&tz_name)
        .ok()
        .unwrap_or(jiff::tz::TimeZone::UTC);
    let local = now.to_zoned(zone);

    // `datetime.weekday()`: Monday is 0.
    let today = DAY_KEYS[local.weekday().to_monday_zero_offset() as usize];
    let active_days = cfg.active_days();
    let listed = active_days
        .as_array()
        .is_some_and(|days| days.iter().any(|day| day.as_str() == Some(today)));
    if !listed {
        return false;
    }

    let start = parse_hhmm_to_minutes(python_or(&cfg.schedule_start, "00:00"), 0);
    let end = parse_hhmm_to_minutes(python_or(&cfg.schedule_end, "24:00"), 24 * 60);
    let current = i64::from(local.hour()) * 60 + i64::from(local.minute());

    window_contains(start, end, current)
}

/// Is `current` inside `[start, end)` minutes-since-midnight?
///
/// Its own function because of the second branch: a window whose end is
/// not after its start wraps past midnight — 22:30 to 06:15 is the
/// night, not an empty set — and that is the half a port gets wrong
/// while the ordinary daytime window keeps working.
fn window_contains(start: i64, end: i64, current: i64) -> bool {
    if start < end {
        (start..end).contains(&current)
    } else {
        current >= start || current < end
    }
}

/// `a or b` for a stored string that may be empty.
fn python_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() {
        fallback
    } else {
        value
    }
}

/// `_motion_cooldown_allows`.
///
/// A waving tree or a blinking light would otherwise burn a month's cap
/// in an afternoon. Every dispatch counts regardless of how it
/// resolved, so a stuck or errored run still holds the next one off —
/// which is deliberate: a camera whose runs keep failing is the last
/// one that should be retried every few seconds.
pub(crate) async fn motion_cooldown_allows(
    state: &AppState,
    org_id: &str,
    cfg: &ConfigRow,
    kind: &str,
    camera_id: Option<&str>,
) -> bool {
    if kind != "motion" {
        return true;
    }
    let Some(camera_id) = camera_id.filter(|id| !id.is_empty()) else {
        return true;
    };
    let cooldown = i64::from(cfg.motion_cooldown_min);
    if cooldown <= 0 {
        return true;
    }
    let Some(cutoff) = now_naive().checked_sub_signed(chrono::TimeDelta::minutes(cooldown)) else {
        return true;
    };

    let recent: Result<Option<(String,)>, _> = sqlx::query_as(
        "SELECT id FROM sentinel_runs
          WHERE org_id = $1 AND camera_id = $2 AND trigger_type = 'motion'
            AND triggered_at >= $3
          LIMIT 1",
    )
    .bind(org_id)
    .bind(camera_id)
    .bind(cutoff)
    .fetch_optional(&state.pool)
    .await;

    match recent {
        Ok(found) => found.is_none(),
        Err(err) => {
            tracing::error!(error = %err, org_id, camera_id, "sentinel: cooldown lookup failed");
            // The Python's exception would propagate to
            // `maybe_dispatch_for_notification`'s blanket handler and
            // skip the dispatch. Refusing here reaches the same place.
            false
        }
    }
}

/// `global_dispatch_allowed`.
///
/// Checked before any per-org gate, so an operator can bound aggregate
/// model spend that per-org caps cannot: a kill switch, and a fleet
/// monthly ceiling.
pub async fn global_dispatch_allowed(state: &AppState) -> Result<bool, sqlx::Error> {
    if !state.config.sentinel_dispatch_enabled {
        return Ok(false);
    }
    let cap = state.config.sentinel_global_monthly_run_cap;
    if cap <= 0 {
        return Ok(true);
    }
    let (used,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM sentinel_runs WHERE triggered_at >= $1")
            .bind(start_of_month())
            .fetch_one(&state.pool)
            .await?;
    Ok(used < cap)
}

/// `_can_dispatch_for_kind`.
///
/// The plan gate sits second because a downgrade leaves
/// `SentinelConfig.enabled` set: without it, motion would keep queuing
/// pending rows that the agent's own auth would later reject, having
/// spent the cap to do so.
pub(crate) async fn can_dispatch_for_kind(
    state: &AppState,
    org_id: &str,
    cfg: &ConfigRow,
    kind: &str,
    camera_id: Option<&str>,
    now: jiff::Timestamp,
) -> Result<(), Refusal> {
    if !cfg.enabled {
        return Err(Refusal::SentinelDisabled);
    }

    let plan = effective_plan_for_caps(&plan_ctx(state), org_id, true).await;
    if !plan_has_sentinel(&plan) {
        return Err(Refusal::PlanNotEligible);
    }
    // Self-hosted resolves to `self_host`, which has no billing at all
    // — this is the separate licence that keeps the agent from being
    // free for everyone self-hosting. A no-op for a hosted org.
    if sentinel_blocked_by_license(&license_ctx(state), &plan).await {
        return Err(Refusal::LicenseRequired);
    }

    let Some((_, field)) = KIND_TO_TRIGGER_FIELD.iter().find(|(k, _)| *k == kind) else {
        return Err(Refusal::KindNotATrigger);
    };
    let on = match *field {
        "motion_enabled" => cfg.motion_enabled,
        "incident_opened_enabled" => cfg.incident_opened_enabled,
        _ => false,
    };
    if !on {
        return Err(Refusal::TriggerOff(field));
    }

    if !is_camera_in_scope(&cfg.camera_scope(), camera_id) {
        return Err(Refusal::CameraOutOfScope);
    }
    if !schedule_allows_now(state, org_id, cfg, now).await {
        return Err(Refusal::OutsideScheduleWindow);
    }
    // After the cheap gates, so the `sentinel_runs` query only happens
    // for an org that would otherwise dispatch.
    if !motion_cooldown_allows(state, org_id, cfg, kind, camera_id).await {
        return Err(Refusal::MotionCooldownActive);
    }

    // `cap_remaining(...) <= 0`, which is `max(0, cap - used) <= 0`.
    // A failed count is not a cap of zero: substituting a sentinel here
    // made `cap - used` underflow, which in debug is a panic on the
    // motion path. Python's exception simply means no dispatch, so this
    // says that instead.
    let cap = cap_for_plan(&plan);
    let Ok(used) = runs_used_this_month(state, org_id).await else {
        return Err(Refusal::LookupFailed);
    };
    if cap.saturating_sub(used) <= 0 {
        return Err(Refusal::MonthlyCapReached);
    }
    Ok(())
}

/// The feedback-loop guard: an incident the agent filed itself.
///
/// `created_by` is `mcp:<key name>` for an MCP-filed incident and
/// `sentinel…` for the agent's own. A human's is a Clerk user id.
pub fn is_agent_authored(kind: &str, meta: Option<&Value>) -> bool {
    if kind != "incident_created" {
        return false;
    }
    let Some(meta) = meta else {
        return false;
    };
    // `str(meta.get("created_by", ""))` — a non-string value is
    // stringified rather than skipped, so a numeric id reads as its
    // digits and matches neither prefix.
    let created_by = match meta.get("created_by") {
        Some(value) => crate::pyrepr::str_value(value),
        None => String::new(),
    };
    created_by.starts_with("mcp") || created_by.starts_with("sentinel")
}

/// `maybe_dispatch_for_notification` — the run id, when one was queued.
pub async fn maybe_dispatch_for_notification(
    state: &AppState,
    org_id: &str,
    kind: &str,
    camera_id: Option<&str>,
    meta: Option<&Value>,
) -> Option<String> {
    match global_dispatch_allowed(state).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!(org_id, "sentinel: dispatch globally blocked");
            return None;
        }
        Err(err) => {
            tracing::error!(error = %err, org_id, kind, "sentinel: dispatch failed silently");
            return None;
        }
    }

    if is_agent_authored(kind, meta) {
        tracing::debug!(org_id, "sentinel: dispatch skipped — agent-authored incident");
        return None;
    }

    // No config row means Sentinel was never configured, which is not
    // the same as configured-and-off: nothing is created here, so a
    // notification cannot bring a config row into being.
    let cfg = match fetch_config(state, org_id).await {
        Ok(Some(cfg)) => cfg,
        Ok(None) => return None,
        Err(err) => {
            tracing::error!(error = ?err, org_id, kind, "sentinel: dispatch failed silently");
            return None;
        }
    };

    let now = jiff::Timestamp::now();
    if let Err(refusal) = can_dispatch_for_kind(state, org_id, &cfg, kind, camera_id, now).await {
        // A closed gate is routine and logs at debug; a lookup that
        // failed is not, and is the one case an operator would want to
        // see without turning the level up.
        if refusal == Refusal::LookupFailed {
            tracing::error!(org_id, kind, camera_id, "sentinel: dispatch failed silently");
        } else {
            tracing::debug!(
                org_id, kind, camera_id, reason = %refusal.as_str(),
                "sentinel: dispatch skipped"
            );
        }
        return None;
    }

    let field = KIND_TO_TRIGGER_FIELD
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, field)| *field)
        .unwrap_or(kind);
    let trigger_type = trigger_type_for(field);
    let run_id = uuid::Uuid::new_v4().simple().to_string();

    // The gate above proved the plan is eligible, so this is non-zero;
    // it is resolved again rather than carried because the plan can
    // change between the gate and the insert.
    let plan = effective_plan_for_caps(&plan_ctx(state), org_id, true).await;
    let cap = cap_for_plan(&plan);
    match insert_run(state, &run_id, org_id, trigger_type, camera_id, cap).await {
        Ok(true) => {}
        Ok(false) => return None,
        Err(err) => {
            tracing::error!(error = %err, org_id, kind, "sentinel: dispatch failed silently");
            return None;
        }
    }

    tracing::info!(
        run_id, org_id, trigger_type, camera_id,
        "sentinel: dispatched pending run"
    );
    fire_wakeup_webhook(state);
    Some(run_id)
}

/// `_commit_run_with_cap_check`.
///
/// The plain check before the insert is a read-then-write race: two
/// dispatchers at cap-1 both pass it and the org overshoots. Counting
/// again *after* the insert, inside the same transaction, catches the
/// second writer — it sees its own row. `false` means it lost that
/// race, which the caller treats exactly like a closed gate.
async fn insert_run(
    state: &AppState,
    run_id: &str,
    org_id: &str,
    trigger_type: &str,
    camera_id: Option<&str>,
    cap: i64,
) -> Result<bool, sqlx::Error> {
    let triggered_at: NaiveDateTime = now_naive();
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO sentinel_runs
            (id, org_id, triggered_at, trigger_type, camera_id, tool_call_count, outcome,
             summary, updated_at)
         VALUES ($1, $2, $3, $4, $5, 0, 'pending', '', $3)",
    )
    .bind(run_id)
    .bind(org_id)
    .bind(triggered_at)
    .bind(trigger_type)
    .bind(camera_id)
    .execute(&mut *tx)
    .await?;

    let (used,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sentinel_runs WHERE org_id = $1 AND triggered_at >= $2",
    )
    .bind(org_id)
    .bind(start_of_month())
    .fetch_one(&mut *tx)
    .await?;
    if used > cap {
        tx.rollback().await?;
        tracing::info!(org_id, cap, used, "sentinel: dispatch lost cap race");
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_camera_is_in_scope_unless_it_is_explicitly_out() {
        let scope = json!({"cam-off": false, "cam-on": true});
        assert!(!is_camera_in_scope(&scope, Some("cam-off")));
        assert!(is_camera_in_scope(&scope, Some("cam-on")));
        // Absent, so in scope — a camera added since the scope was
        // saved must not silently disappear from the agent's purview.
        assert!(is_camera_in_scope(&scope, Some("cam-new")));
        // An empty scope is everything.
        assert!(is_camera_in_scope(&json!({}), Some("cam-off")));
        // No camera at all — a sweep — is always in scope.
        assert!(is_camera_in_scope(&scope, None));
        assert!(is_camera_in_scope(&scope, Some("")));
    }

    #[test]
    fn a_schedule_bound_keeps_its_minutes() {
        // The bug this replaced read only the hour, so 22:30 was 22:00.
        assert_eq!(parse_hhmm_to_minutes("22:30", 0), 22 * 60 + 30);
        assert_eq!(parse_hhmm_to_minutes("22", 0), 22 * 60);
        assert_eq!(parse_hhmm_to_minutes("00:00", 999), 0);
        assert_eq!(parse_hhmm_to_minutes("24:00", 0), 24 * 60);
        // Only the second field is read, as Python's `parts[1]` is.
        assert_eq!(parse_hhmm_to_minutes("22:30:45", 0), 22 * 60 + 30);
        // Clamped to a day at both ends.
        assert_eq!(parse_hhmm_to_minutes("99:00", 0), 24 * 60);
        assert_eq!(parse_hhmm_to_minutes("-5:00", 0), 0);
        // Unparseable takes the default rather than raising.
        for bad in ["", "abc", "12:xy", ":", "12:"] {
            assert_eq!(parse_hhmm_to_minutes(bad, 7), 7, "{bad:?}");
        }
    }

    #[test]
    fn a_window_that_wraps_past_midnight_is_the_night() {
        // An ordinary daytime window: half-open, so the end minute is
        // outside it.
        let nine_to_five = |m| window_contains(9 * 60, 17 * 60, m);
        assert!(!nine_to_five(8 * 60 + 59));
        assert!(nine_to_five(9 * 60));
        assert!(nine_to_five(16 * 60 + 59));
        assert!(!nine_to_five(17 * 60));

        // 22:30 → 06:15 covers the night on both sides of midnight.
        let night = |m| window_contains(22 * 60 + 30, 6 * 60 + 15, m);
        assert!(night(22 * 60 + 30));
        assert!(night(23 * 60 + 59));
        assert!(night(0));
        assert!(night(6 * 60 + 14));
        assert!(!night(6 * 60 + 15));
        assert!(!night(12 * 60));
        assert!(!night(22 * 60 + 29));

        // The whole day, which is what the defaults parse to.
        assert!(window_contains(0, 24 * 60, 0));
        assert!(window_contains(0, 24 * 60, 24 * 60 - 1));
        // Start equal to end takes the wrapping branch, and so covers
        // everything rather than nothing — which is what Python's
        // `cur >= start or cur < end` does too.
        assert!(window_contains(9 * 60, 9 * 60, 0));
        assert!(window_contains(9 * 60, 9 * 60, 9 * 60));
    }

    #[test]
    fn the_trigger_labels_are_the_ones_the_dashboard_colours() {
        assert_eq!(trigger_type_for("motion_enabled"), "motion");
        assert_eq!(trigger_type_for("incident_opened_enabled"), "incident_opened");
        // The kind and the trigger name differ on purpose: the UI says
        // `incident_opened` where the notification kind is
        // `incident_created`.
        assert_eq!(
            KIND_TO_TRIGGER_FIELD.iter().find(|(k, _)| *k == "incident_created").unwrap().1,
            "incident_opened_enabled"
        );
    }

    #[test]
    fn an_agent_authored_incident_does_not_retrigger() {
        // Both prefixes, because the agent files through MCP and the
        // dispatcher also writes its own.
        for author in ["mcp:my-key", "mcp", "sentinel-agent", "sentinel"] {
            assert!(
                is_agent_authored("incident_created", Some(&json!({"created_by": author}))),
                "{author}"
            );
        }
        // A human's is a Clerk user id.
        for author in ["user_2abc", "", "MCP:upper", "not-mcp"] {
            assert!(
                !is_agent_authored("incident_created", Some(&json!({"created_by": author}))),
                "{author}"
            );
        }
        // The guard is scoped to the one kind that can loop.
        assert!(!is_agent_authored("motion", Some(&json!({"created_by": "mcp:x"}))));
        assert!(!is_agent_authored("incident_created", None));
        assert!(!is_agent_authored("incident_created", Some(&json!({}))));
        // A non-string value is stringified, not skipped.
        assert!(!is_agent_authored("incident_created", Some(&json!({"created_by": 12}))));
    }

    #[test]
    fn every_refusal_has_the_pythons_name() {
        for (refusal, want) in [
            (Refusal::SentinelDisabled, "sentinel_disabled"),
            (Refusal::PlanNotEligible, "plan_not_eligible"),
            (Refusal::LicenseRequired, "license_required"),
            (Refusal::KindNotATrigger, "kind_not_a_sentinel_trigger"),
            (Refusal::TriggerOff("motion_enabled"), "trigger_motion_enabled_off"),
            (Refusal::CameraOutOfScope, "camera_out_of_scope"),
            (Refusal::OutsideScheduleWindow, "outside_schedule_window"),
            (Refusal::MotionCooldownActive, "motion_cooldown_active"),
            (Refusal::MonthlyCapReached, "monthly_cap_reached"),
            (Refusal::LookupFailed, "lookup_failed"),
        ] {
            assert_eq!(refusal.as_str(), want);
        }
    }
}
