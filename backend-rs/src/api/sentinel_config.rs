//! Sentinel configuration and the operator's "Run now".
//!
//! Ported from `backend/app/api/sentinel.py` (`/config`,
//! `/runs/manual`) and the parts of `backend/app/core/sentinel_dispatch.py`
//! they reach: the plan caps, the fleet-wide gate, and the
//! count-after-insert that closes the cap race.
//!
//! Access has two gates and the difference between them is visible to
//! the caller. A plan that does not include Sentinel is
//! `plan_required`; a self-hosted install whose licence is not
//! currently valid is `license_required`. The frontend shows an
//! "upgrade to Pro" button for the first and something else entirely
//! for the second, so a port that collapsed them would send
//! self-hosted operators to a Clerk checkout page.

use axum::extract::{ConnectInfo, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::{Datelike, NaiveDateTime, TimeZone, Utc};
use serde_json::{json, Map, Value};

use crate::api::sentinel::{SentinelRunRow, RUN_SELECT};
use crate::app::AppState;
use crate::audit::{python_json, write_audit};
use crate::auth::{RequireAdmin, RequireView};
use crate::error::ApiError;
use crate::license::{sentinel_blocked_by_license, LicenseContext};
use crate::models::{iso_naive, now_naive};
use crate::plans::{effective_plan_for_caps, get_plan_display_name, PlanContext};
use crate::pyint::PyInt;
use crate::pyrepr;
use crate::query::{BodyErrors, ModelBody, Query};

/// Monthly run cap by plan. Sentinel is available on both paid tiers;
/// the cap is the differentiator. Absent from this table means no
/// Sentinel at all, and `cap_for_plan` returns 0 — fail closed, so any
/// path that reaches the cap check for an ineligible org sees nothing
/// remaining.
pub(crate) fn cap_for_plan(plan: &str) -> i64 {
    match plan {
        "pro" => 100,
        "pro_plus" | "self_host" => 500,
        _ => 0,
    }
}

pub(crate) fn plan_has_sentinel(plan: &str) -> bool {
    matches!(plan, "pro" | "pro_plus" | "self_host")
}

const VALID_SCHEDULE_MODES: [&str; 3] = ["always", "scheduled", "off"];
const VALID_DAY_KEYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
const DEFAULT_DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct ConfigRow {
    pub(crate) enabled: bool,
    pub(crate) motion_enabled: bool,
    pub(crate) incident_opened_enabled: bool,
    pub(crate) motion_cooldown_min: i32,
    pub(crate) schedule_mode: String,
    pub(crate) schedule_start: String,
    pub(crate) schedule_end: String,
    pub(crate) active_days: Option<String>,
    pub(crate) camera_scope: Option<String>,
    pub(crate) created_at: Option<NaiveDateTime>,
    pub(crate) updated_at: Option<NaiveDateTime>,
}

impl ConfigRow {
    /// `get_active_days`: the stored JSON list, or every day. A value
    /// that is not a JSON list — corrupt, or an object — falls back the
    /// same way rather than raising.
    pub(crate) fn active_days(&self) -> Value {
        match self.active_days.as_deref().filter(|s| !s.is_empty()) {
            Some(raw) => match serde_json::from_str::<Value>(raw) {
                Ok(Value::Array(items)) => {
                    Value::Array(items.iter().map(|v| json!(pyrepr::str_value(v))).collect())
                }
                _ => json!(DEFAULT_DAYS),
            },
            None => json!(DEFAULT_DAYS),
        }
    }

    /// `get_camera_scope`: keys stringified, values coerced with
    /// Python's `bool()`.
    pub(crate) fn camera_scope(&self) -> Value {
        let Some(raw) = self.camera_scope.as_deref().filter(|s| !s.is_empty()) else {
            return json!({});
        };
        match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(map)) => Value::Object(
                map.into_iter()
                    .map(|(k, v)| (k, json!(pyrepr::truthy(&v))))
                    .collect(),
            ),
            _ => json!({}),
        }
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "motion_enabled": self.motion_enabled,
            "incident_opened_enabled": self.incident_opened_enabled,
            "motion_cooldown_min": self.motion_cooldown_min,
            "schedule_mode": self.schedule_mode,
            "schedule_start": self.schedule_start,
            "schedule_end": self.schedule_end,
            "active_days": self.active_days(),
            "camera_scope": self.camera_scope(),
            "created_at": self.created_at.map(iso_naive),
            "updated_at": self.updated_at.map(iso_naive),
        })
    }
}

const CONFIG_COLUMNS: &str = "enabled, motion_enabled, incident_opened_enabled, motion_cooldown_min,
     schedule_mode, schedule_start, schedule_end, active_days, camera_scope, created_at, updated_at";

/// Get-or-create the per-org config row.
///
/// Lazily created on first read, which makes a GET a write. The unique
/// index on `org_id` settles a concurrent create: the loser re-queries
/// and returns the winner's row.
async fn ensure_config_row(state: &AppState, org_id: &str) -> Result<ConfigRow, ApiError> {
    if let Some(row) = fetch_config(state, org_id).await? {
        return Ok(row);
    }
    let now = now_naive();
    let inserted = sqlx::query(
        "INSERT INTO sentinel_config
            (org_id, enabled, motion_enabled, incident_opened_enabled, motion_cooldown_min,
             schedule_mode, schedule_start, schedule_end, created_at, updated_at)
         VALUES ($1, true, true, true, 5, 'always', '22:00', '06:00', $2, $2)",
    )
    .bind(org_id)
    .bind(now)
    .execute(&state.pool)
    .await;
    if let Err(err) = inserted {
        if !matches!(&err, sqlx::Error::Database(db) if db.is_unique_violation()) {
            return Err(err.into());
        }
    }
    fetch_config(state, org_id)
        .await?
        .ok_or_else(|| ApiError::internal("sentinel config row vanished after insert"))
}

pub(crate) async fn fetch_config(
    state: &AppState,
    org_id: &str,
) -> Result<Option<ConfigRow>, ApiError> {
    Ok(sqlx::query_as(&format!(
        "SELECT {CONFIG_COLUMNS} FROM sentinel_config WHERE org_id = $1 LIMIT 1"
    ))
    .bind(org_id)
    .fetch_optional(&state.pool)
    .await?)
}

pub(crate) fn plan_ctx<'a>(state: &'a AppState) -> PlanContext<'a> {
    PlanContext {
        pool: &state.pool,
        client: &state.http,
        clerk_base_url: &state.config.clerk_api_url,
        clerk_secret: &state.config.clerk_secret_key,
        local_auth: state.config.is_local_auth(),
    }
}

/// The licence state lives under `LOCAL_ORG_ID`, not the caller's org:
/// a self-hosted install has exactly one organisation, and the licence
/// belongs to the install.
pub(crate) fn license_ctx(state: &AppState) -> LicenseContext<'_> {
    LicenseContext {
        pool: &state.pool,
        org_id: &state.config.local_org_id,
        local_auth: state.config.is_local_auth(),
        license_key: state.config.sentinel_license_key.as_deref(),
    }
}

/// Both "is Sentinel granted" and, when it is not, why — computed once
/// so a 402 body cannot disagree with the check that produced it.
pub(crate) async fn resolve_sentinel_access(
    state: &AppState,
    org_id: &str,
) -> Result<(bool, Value), ApiError> {
    let plan = effective_plan_for_caps(&plan_ctx(state), org_id, true).await;
    if !plan_has_sentinel(&plan) {
        return Ok((false, json!({"error": "plan_required", "plan": "pro"})));
    }
    if sentinel_blocked_by_license(&license_ctx(state), &plan).await {
        return Ok((false, json!({"error": "license_required"})));
    }
    Ok((true, json!({})))
}

/// `GET /api/sentinel/config` — always 200.
///
/// An org without access gets the same payload with `plan_gated: true`
/// so the page renders read-only behind a banner.
pub async fn get_config(
    State(state): State<AppState>,
    RequireView(user): RequireView,
) -> Result<Json<Value>, ApiError> {
    let cfg = ensure_config_row(&state, &user.org_id).await?;
    let plan = effective_plan_for_caps(&plan_ctx(&state), &user.org_id, true).await;
    let (has_access, denial) = resolve_sentinel_access(&state, &user.org_id).await?;
    Ok(Json(json!({
        "config": cfg.to_json(),
        "plan_gated": !has_access,
        "plan_gated_reason": if has_access { Value::Null } else { denial["error"].clone() },
        "plan_required": "pro",
        "plan_current": get_plan_display_name(&plan),
        // 0 when the org has no access at all, even though self_host
        // nominally carries a cap.
        "monthly_cap": if has_access { cap_for_plan(&plan) } else { 0 },
    })))
}

/// `PATCH /api/sentinel/config` — a partial update.
pub async fn patch_config(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    ModelBody(RequireAdmin(user), body): ModelBody<RequireAdmin>,
) -> Result<Json<Value>, ApiError> {
    let mut errors = BodyErrors::new();
    // Pydantic validates the whole model before the handler runs, and
    // in field-declaration order — which is also the order the loop
    // below applies them, and therefore which 400 a caller sees first.
    let enabled = errors.optional_bool(&body, "enabled");
    let motion = errors.optional_bool(&body, "motion_enabled");
    let incident = errors.optional_bool(&body, "incident_opened_enabled");
    let cooldown = errors.optional_int_in_range(&body, "motion_cooldown_min", 1, 60);
    let mode = errors.optional_string(&body, "schedule_mode", usize::MAX);
    let start = errors.optional_string(&body, "schedule_start", usize::MAX);
    let end = errors.optional_string(&body, "schedule_end", usize::MAX);
    let days = errors.optional_list_of_strings(&body, "active_days");
    let scope = errors.optional_object(&body, "camera_scope");
    errors.finish()?;

    let (has_access, denial) = resolve_sentinel_access(&state, &user.org_id).await?;
    if !has_access {
        return Err(ApiError::new(
            axum::http::StatusCode::PAYMENT_REQUIRED,
            denial,
        ));
    }

    // Created before any field is validated further, and it stays
    // created even if the loop below rejects the body.
    ensure_config_row(&state, &user.org_id).await?;

    // Values to write, and the audit's record of them. Both are built
    // in Pydantic's field-declaration order, because that is the order
    // the Python loop applies them — so it decides which 400 a caller
    // sees first, and the order of the `changes` list.
    let mut changes: Vec<String> = Vec::new();
    let mut days_json: Option<String> = None;
    let mut scope_json: Option<String> = None;

    // `if value is None: continue` — an explicit null is skipped, not
    // written as NULL.
    if let Some(v) = enabled {
        changes.push(format!("enabled={}", pyrepr::str_value(&json!(v))));
    }
    if let Some(v) = motion {
        changes.push(format!("motion_enabled={}", pyrepr::str_value(&json!(v))));
    }
    if let Some(v) = incident {
        changes.push(format!(
            "incident_opened_enabled={}",
            pyrepr::str_value(&json!(v))
        ));
    }
    if let Some(v) = cooldown {
        changes.push(format!("motion_cooldown_min={v}"));
    }
    if let Some(ref v) = mode {
        if !VALID_SCHEDULE_MODES.contains(&v.as_str()) {
            // `f"invalid schedule_mode: {value!r}"` — Python's repr, so
            // the value comes back quoted.
            return Err(ApiError::bad_request(format!(
                "invalid schedule_mode: {}",
                pyrepr::repr_str(v)
            )));
        }
        changes.push(format!("schedule_mode={v}"));
    }
    for (field, value) in [("schedule_start", &start), ("schedule_end", &end)] {
        if let Some(v) = value {
            validate_hhmm(v, field)?;
            changes.push(format!("{field}={v}"));
        }
    }
    if let Some(ref v) = days {
        // An unrecognised day is dropped rather than rejected.
        let cleaned: Vec<Value> = v
            .iter()
            .filter(|d| VALID_DAY_KEYS.contains(&d.as_str()))
            .map(|d| json!(d))
            .collect();
        days_json = Some(crate::audit::python_json_value(&Value::Array(cleaned)));
        changes.push(format!(
            "active_days={}",
            pyrepr::str_value(&Value::Array(v.iter().map(|d| json!(d)).collect()))
        ));
    }
    if let Some(ref v) = scope {
        // Keys stringified, values through Python's bool().
        let coerced: Map<String, Value> = v
            .iter()
            .map(|(k, val)| (k.clone(), json!(pyrepr::truthy(val))))
            .collect();
        scope_json = Some(crate::audit::python_json_value(&Value::Object(coerced)));
        changes.push(format!(
            "camera_scope={}",
            pyrepr::str_value(&Value::Object(v.clone()))
        ));
    }

    if !changes.is_empty() {
        // COALESCE per column: an absent field binds NULL and keeps what
        // is there, which is exactly "only fields present are touched".
        sqlx::query(
            "UPDATE sentinel_config SET
                enabled = COALESCE($1, enabled),
                motion_enabled = COALESCE($2, motion_enabled),
                incident_opened_enabled = COALESCE($3, incident_opened_enabled),
                motion_cooldown_min = COALESCE($4, motion_cooldown_min),
                schedule_mode = COALESCE($5, schedule_mode),
                schedule_start = COALESCE($6, schedule_start),
                schedule_end = COALESCE($7, schedule_end),
                active_days = COALESCE($8, active_days),
                camera_scope = COALESCE($9, camera_scope),
                updated_at = $10
              WHERE org_id = $11",
        )
        .bind(enabled)
        .bind(motion)
        .bind(incident)
        .bind(cooldown.map(|v| v as i32))
        .bind(mode.as_deref())
        .bind(start.as_deref())
        .bind(end.as_deref())
        .bind(days_json.as_deref())
        .bind(scope_json.as_deref())
        .bind(now_naive())
        .bind(&user.org_id)
        .execute(&state.pool)
        .await?;

        write_audit(
            &state.pool,
            &user.org_id,
            "sentinel_config_updated",
            &user.user_id,
            // Not audit_label: this route uses `user.email or user.username`.
            &python_or(&user.email, &user.username),
            Some(python_json(&[(
                "changes",
                Value::Array(changes.iter().map(|c| json!(c)).collect()),
            )])),
            &headers,
            Some(&peer.ip().to_string()),
        )
        .await;
    }

    let cfg = ensure_config_row(&state, &user.org_id).await?;
    Ok(Json(json!({ "config": cfg.to_json() })))
}

/// `HH:MM`, 00-23 and 00-59, by position rather than by parsing.
fn validate_hhmm(value: &str, field: &str) -> Result<(), ApiError> {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() != 5 || chars[2] != ':' {
        return Err(ApiError::bad_request(format!("{field} must be HH:MM")));
    }
    let hh: String = chars[..2].iter().collect();
    let mm: String = chars[3..].iter().collect();
    // Python's int() on the two-character slices: whitespace and a sign
    // are accepted, so " 1" parses and "1:" does not.
    let (Some(h), Some(m)) = (python_int(&hh), python_int(&mm)) else {
        return Err(ApiError::bad_request(format!("{field} must be HH:MM")));
    };
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
        return Err(ApiError::bad_request(format!("{field} out of range")));
    }
    Ok(())
}

pub(crate) fn python_int(s: &str) -> Option<i64> {
    let t = s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c));
    let (neg, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse::<i64>().ok().map(|v| if neg { -v } else { v })
}

/// Start of the current UTC month, as a naive timestamp.
pub(crate) fn start_of_month() -> NaiveDateTime {
    let now = Utc::now();
    Utc.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .map(|dt| dt.naive_utc())
        .unwrap_or_else(now_naive)
}

pub(crate) async fn runs_used_this_month(state: &AppState, org_id: &str) -> Result<i64, ApiError> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sentinel_runs WHERE org_id = $1 AND triggered_at >= $2",
    )
    .bind(org_id)
    .bind(start_of_month())
    .fetch_one(&state.pool)
    .await?;
    Ok(n)
}

/// `GET /api/sentinel/runs` — the run history behind the dashboard,
/// with the small stats block that sits above it.
///
/// Three things here are more delicate than they look.
///
/// **`since`** is parsed by `datetime.fromisoformat`, whose C
/// implementation accepts a good deal more than ISO 8601 — see
/// `crate::pydatetime`. A ValueError from it is the 400 below; an
/// OverflowError from the `astimezone` that follows is *not* caught by
/// the Python and is a 500.
///
/// **"Today"** is midnight in the org's own timezone, so the window
/// depends on tzdata and on PEP 495's fold rules; `crate::zoneinfo`
/// resolves the name the way `ZoneInfo` does, including which failures
/// fall back to UTC and which are a 500.
///
/// **`offset`** has a lower bound and no upper one, so a value past i64
/// reaches Postgres and is refused there — a 500, after the count query
/// has already run.
pub async fn list_runs(
    State(state): State<AppState>,
    RequireView(user): RequireView,
    request: axum::extract::Request,
) -> Result<Json<Value>, ApiError> {
    let mut q = Query::parse(request.uri().query());
    let limit = q.int("limit", 50, 1, 200);
    let offset = q.big_int("offset", PyInt::Small(0), Some(0), None);
    let trigger = q.optional_str("trigger");
    let since = q.optional_str("since");
    q.finish()?;

    // `if since:` — an empty value is no filter at all.
    let since = match since {
        Some(raw) => Some(parse_since(&raw)?),
        None => None,
    };

    // The filtered query the list and its total share, and the
    // unfiltered one every stat below is counted from.
    let mut filters = String::from(" WHERE org_id = $1");
    if trigger.is_some() {
        filters.push_str(" AND trigger_type = $2");
    }
    if since.is_some() {
        let n = if trigger.is_some() { 3 } else { 2 };
        filters.push_str(&format!(" AND triggered_at >= ${n}"));
    }
    macro_rules! bind_filters {
        ($q:expr) => {{
            let mut query = $q.bind(&user.org_id);
            if let Some(ref trigger) = trigger {
                query = query.bind(trigger);
            }
            if let Some(since) = since {
                query = query.bind(since);
            }
            query
        }};
    }

    let count_sql = format!("SELECT COUNT(*) FROM sentinel_runs{filters}");
    let total: i64 = bind_filters!(sqlx::query_scalar(&count_sql))
        .fetch_one(&state.pool)
        .await?;

    // Python hands the offset straight to Postgres, which takes a
    // bigint and nothing wider.
    let offset = offset
        .small()
        .ok_or_else(|| ApiError::internal("bigint out of range"))?;
    let list_sql =
        format!("{RUN_SELECT}{filters} ORDER BY triggered_at DESC OFFSET {offset} LIMIT {limit}");
    let rows: Vec<SentinelRunRow> = bind_filters!(sqlx::query_as(&list_sql))
        .fetch_all(&state.pool)
        .await?;

    let today_start = org_midnight_utc(&state, &user.org_id).await?;
    let runs_today: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sentinel_runs WHERE org_id = $1 AND triggered_at >= $2",
    )
    .bind(&user.org_id)
    .bind(today_start)
    .fetch_one(&state.pool)
    .await?;

    let runs_total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sentinel_runs WHERE org_id = $1")
            .bind(&user.org_id)
            .fetch_one(&state.pool)
            .await?;
    let incident_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sentinel_runs WHERE org_id = $1 AND outcome = 'incident'",
    )
    .bind(&user.org_id)
    .fetch_one(&state.pool)
    .await?;
    let pending_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sentinel_runs \
          WHERE org_id = $1 AND outcome IN ('pending', 'running')",
    )
    .bind(&user.org_id)
    .fetch_one(&state.pool)
    .await?;

    let runs_month = runs_used_this_month(&state, &user.org_id).await?;
    let plan = effective_plan_for_caps(&plan_ctx(&state), &user.org_id, true).await;
    let cap = cap_for_plan(&plan);

    Ok(Json(json!({
        "runs": rows.iter().map(|r| r.to_json(false)).collect::<Vec<_>>(),
        "total": total,
        "stats": {
            "runs_today": runs_today,
            "runs_total": runs_total,
            "runs_this_month": runs_month,
            "incidents_filed": incident_count,
            "pending": pending_count,
            "monthly_cap": cap,
            "remaining_this_month": (cap - runs_month).max(0),
        },
    })))
}

/// `datetime.fromisoformat(since.replace("Z", "+00:00"))`, then
/// `astimezone(UTC)` for an aware result.
///
/// Python catches ValueError and answers 400. It does not catch the
/// OverflowError that `astimezone` raises when the shifted value leaves
/// the calendar, so that one stays a 500.
fn parse_since(raw: &str) -> Result<NaiveDateTime, ApiError> {
    crate::pydatetime::fromisoformat(&raw.replace('Z', "+00:00"))
        .and_then(crate::pydatetime::to_naive_utc)
        .map_err(|err| match err {
            crate::pydatetime::PyDateError::Value => {
                ApiError::bad_request("invalid `since` — expected ISO datetime")
            }
            crate::pydatetime::PyDateError::Overflow => {
                ApiError::internal("date value out of range")
            }
        })
}

/// Midnight today in the org's configured timezone, as the naive UTC
/// timestamp the `triggered_at` column is compared against.
///
/// An unknown or malformed zone name falls back to UTC, because the
/// Python catches exactly `ZoneInfoNotFoundError` and `ValueError`. A
/// name that happens to be a *directory* of the tzdata package raises
/// IsADirectoryError instead, which nothing catches — a 500.
async fn org_midnight_utc(state: &AppState, org_id: &str) -> Result<NaiveDateTime, ApiError> {
    let name = crate::settings::get(&state.pool, org_id, "timezone", Some("UTC"))
        .await?
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "UTC".to_string());
    midnight_in_zone(&name, jiff::Timestamp::now())
}

/// The half of it that does not need the database: a zone name and an
/// instant, to the naive UTC timestamp `triggered_at` is compared
/// against.
///
/// Split out so it can be tested against a fixed clock. The route's own
/// composition — which zone, and which instant's midnight — is only
/// wrong on a day the zone changes offset, and both differentials run
/// against the real one, so they can only see it on the two or three
/// days a year a transition lands mid-fixture.
fn midnight_in_zone(name: &str, now: jiff::Timestamp) -> Result<NaiveDateTime, ApiError> {
    let tz = match crate::zoneinfo::load(name) {
        Ok(tz) => tz,
        Err(crate::zoneinfo::LoadError::NotFound) => crate::zoneinfo::load("UTC")
            .map_err(|_| ApiError::internal("no UTC zone on this machine"))?,
        Err(crate::zoneinfo::LoadError::IsADirectory) => {
            return Err(ApiError::internal("Is a directory"))
        }
    };
    Ok(crate::zoneinfo::local_midnight_utc(&tz, now))
}

/// `POST /api/sentinel/runs/manual` — the operator's "Run now".
///
/// Schedule and camera scope are deliberately not enforced: clicking
/// the button is the override. The plan cap still is.
pub async fn post_manual_run(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    ModelBody(RequireAdmin(user), body): ModelBody<RequireAdmin>,
) -> Result<Json<Value>, ApiError> {
    let mut errors = BodyErrors::new();
    let prompt = errors.string_with_default(&body, "prompt", 2000);
    let camera_id = errors.optional_string(&body, "camera_id", usize::MAX);
    errors.finish()?;

    let (has_access, denial) = resolve_sentinel_access(&state, &user.org_id).await?;
    if !has_access {
        return Err(ApiError::new(
            axum::http::StatusCode::PAYMENT_REQUIRED,
            denial,
        ));
    }

    // The fleet-wide gate applies to a manual run too: "Run now" still
    // spends model budget.
    if !state.config.sentinel_dispatch_enabled
        || (state.config.sentinel_global_monthly_run_cap > 0
            && global_runs_this_month(&state).await?
                >= state.config.sentinel_global_monthly_run_cap)
    {
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "sentinel_dispatch_disabled"}),
        ));
    }

    // The manual path works for an org that has never opened the page.
    ensure_config_row(&state, &user.org_id).await?;

    let plan = effective_plan_for_caps(&plan_ctx(&state), &user.org_id, true).await;
    if !plan_has_sentinel(&plan) {
        return Err(ApiError::new(
            axum::http::StatusCode::PAYMENT_REQUIRED,
            json!({"error": "plan_required", "plan": "pro"}),
        ));
    }
    if sentinel_blocked_by_license(&license_ctx(&state), &plan).await {
        return Err(ApiError::new(
            axum::http::StatusCode::PAYMENT_REQUIRED,
            json!({"error": "license_required"}),
        ));
    }
    let cap = cap_for_plan(&plan);
    if cap - runs_used_this_month(&state, &user.org_id).await? <= 0 {
        return Err(cap_reached(&state, &user.org_id, cap).await);
    }

    let run_id = uuid::Uuid::new_v4().simple().to_string();
    let triggered_at = now_naive();
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO sentinel_runs
            (id, org_id, triggered_at, trigger_type, camera_id, tool_call_count, outcome,
             manual_prompt, summary, updated_at)
         VALUES ($1, $2, $3, 'manual', $4, 0, 'pending', $5, '', $3)",
    )
    .bind(&run_id)
    .bind(&user.org_id)
    .bind(triggered_at)
    .bind(camera_id.as_deref())
    .bind(truncate_chars(&prompt, 2000))
    .execute(&mut *tx)
    .await?;
    // Recount inside the transaction, after the insert is visible to
    // it. The plain check above is a read-then-write race: two
    // dispatchers at cap-1 both pass it and the org overshoots by one.
    let (used,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sentinel_runs WHERE org_id = $1 AND triggered_at >= $2",
    )
    .bind(&user.org_id)
    .bind(start_of_month())
    .fetch_one(&mut *tx)
    .await?;
    if used > cap {
        tx.rollback().await?;
        return Err(cap_reached(&state, &user.org_id, cap).await);
    }
    tx.commit().await?;

    write_audit(
        &state.pool,
        &user.org_id,
        "sentinel_manual_run",
        &user.user_id,
        &if user.email.is_empty() {
            user.username.clone()
        } else {
            user.email.clone()
        },
        Some(python_json(&[
            ("run_id", json!(run_id)),
            (
                "camera_id",
                camera_id.clone().map(Value::String).unwrap_or(Value::Null),
            ),
            ("prompt_len", json!(prompt.chars().count())),
        ])),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    fire_wakeup_webhook(&state);

    let row: crate::api::sentinel::SentinelRunRow = sqlx::query_as(&format!(
        "{} WHERE id = $1",
        crate::api::sentinel::RUN_SELECT
    ))
    .bind(&run_id)
    .fetch_one(&state.pool)
    .await?;
    Ok(Json(row.to_json(false)))
}

async fn global_runs_this_month(state: &AppState) -> Result<i64, ApiError> {
    let (n,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM sentinel_runs WHERE triggered_at >= $1")
            .bind(start_of_month())
            .fetch_one(&state.pool)
            .await?;
    Ok(n)
}

async fn cap_reached(state: &AppState, org_id: &str, cap: i64) -> ApiError {
    let used = runs_used_this_month(state, org_id).await.unwrap_or(0);
    ApiError::new(
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        json!({"error": "monthly_cap_reached", "cap": cap, "used": used}),
    )
}

/// `a or b` for two strings: the first non-empty one.
fn python_or(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_string()
    } else {
        a.to_string()
    }
}

fn truncate_chars(s: &str, limit: usize) -> String {
    s.chars().take(limit).collect()
}

/// Nudge the agent so it drains the queue now rather than at its next
/// poll. Fire-and-forget: the run is already committed, and the agent
/// re-fetches pending runs itself, so a failure here costs latency
/// rather than work.
///
/// The body is a timestamp rather than a constant, and signed: the
/// signature of a fixed body under a fixed key never changes, so one
/// captured request could be replayed forever to force cold starts.
pub(crate) fn fire_wakeup_webhook(state: &AppState) {
    let Some(url) = state.config.sentinel_agent_webhook_url.clone() else {
        return; // no agent configured — the run waits to be polled
    };
    let Some(secret) = state.config.sentinel_agent_key.clone() else {
        tracing::warn!(
            "sentinel wakeup: webhook URL set but SENTINEL_AGENT_KEY is empty — skipping"
        );
        return;
    };
    let client = state.http.clone();
    tokio::spawn(async move {
        let body = wakeup_payload(chrono::Utc::now().timestamp());
        let signature = wakeup_signature(body.as_bytes(), &secret);
        let sent = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("X-Sentinel-Signature", signature)
            .body(body)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
        match sent {
            Ok(resp) if resp.status().is_client_error() || resp.status().is_server_error() => {
                tracing::warn!(%url, status = %resp.status(), "sentinel wakeup rejected");
            }
            Ok(_) => {}
            // Usually the agent's machine is cold-starting; the next
            // wakeup, or the agent's own drain, picks the run up.
            Err(err) => tracing::info!(%url, error = %err, "sentinel wakeup unreachable"),
        }
    });
}

fn wakeup_payload(ts: i64) -> String {
    format!("{{\"ts\": {ts}}}")
}

fn wakeup_signature(body: &[u8], secret: &str) -> String {
    format!(
        "sha256={}",
        crate::crypto::hex(&crate::crypto::hmac_sha256(secret.as_bytes(), body))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wakeup_signature_matches_pythons_hmac() {
        // Checked against hmac.new(key, body, sha256).hexdigest().
        assert_eq!(
            wakeup_signature(b"{\"ts\": 1700000000}", "test-agent-key"),
            "sha256=d8d69079da8018fcf0f299cd1396f0445ce4023b2554cae02282d60a6704ef76"
        );
        // A key longer than the 64-byte block is hashed first.
        assert_eq!(
            wakeup_signature(b"{\"ts\": 1700000000}", &"k".repeat(100)),
            "sha256=7156dc62e67bd499330d3e7ee935d377bfc4ce7c41600b3bee801de0a6e1d434"
        );
    }

    #[test]
    fn the_wakeup_body_is_json_dumps_shaped() {
        // `json.dumps({"ts": ...})` — one space after the colon.
        assert_eq!(wakeup_payload(1700000000), "{\"ts\": 1700000000}");
    }

    #[test]
    fn hhmm_validation_accepts_what_python_accepts() {
        assert!(validate_hhmm("08:30", "schedule_start").is_ok());
        assert!(validate_hhmm("23:59", "schedule_start").is_ok());
        assert!(validate_hhmm("00:00", "schedule_start").is_ok());
        // int(" 8") is 8, so a space-padded hour parses.
        assert!(validate_hhmm(" 8:30", "schedule_start").is_ok());
        for bad in ["8:30", "0830", "", "08-30", "aa:bb", "08:3x"] {
            assert!(validate_hhmm(bad, "schedule_start").is_err(), "{bad:?}");
        }
        // Parses, but out of range — a different message.
        let err = validate_hhmm("24:00", "schedule_start").unwrap_err();
        assert_eq!(err.detail, json!("schedule_start out of range"));
    }

    #[test]
    fn a_plan_outside_the_table_gets_no_runs() {
        assert_eq!(cap_for_plan("pro"), 100);
        assert_eq!(cap_for_plan("pro_plus"), 500);
        assert_eq!(cap_for_plan("self_host"), 500);
        assert_eq!(cap_for_plan("free_org"), 0);
        assert!(!plan_has_sentinel("free_org"));
    }

    /// What the route makes of a stored zone name at a fixed instant.
    ///
    /// The values are CPython's, from
    /// `tests/differential/midnight_probe.py`, and the first four are
    /// days a zone changes offset — where midnight's offset is not
    /// now's, and using the wrong one moves the whole "today" window by
    /// an hour. Neither differential can send those: both run against
    /// the real clock, so a transition has to fall on the day the
    /// harness happens to run.
    #[test]
    fn midnight_is_read_in_the_org_zone_at_a_fixed_instant() {
        fn naive(s: &str) -> NaiveDateTime {
            NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
        }
        for (zone, now, want) in [
            (
                "America/Los_Angeles",
                "2026-03-08T20:00:00Z",
                "2026-03-08 08:00:00",
            ),
            (
                "America/Los_Angeles",
                "2026-11-01T20:00:00Z",
                "2026-11-01 07:00:00",
            ),
            (
                "Europe/London",
                "2026-03-29T15:00:00Z",
                "2026-03-29 00:00:00",
            ),
            (
                "Australia/Lord_Howe",
                "2026-04-05T06:00:00Z",
                "2026-04-04 13:00:00",
            ),
            ("UTC", "2026-05-07T15:00:00Z", "2026-05-07 00:00:00"),
            (
                "Asia/Kolkata",
                "2026-05-07T20:00:00Z",
                "2026-05-07 18:30:00",
            ),
            (
                "America/Havana",
                "2026-11-01T05:30:00Z",
                "2026-11-01 05:00:00",
            ),
            // Not a zone: Python catches the lookup and uses UTC, so
            // this is the UTC answer for the same instant.
            (
                "Mars/Olympus_Mons",
                "2026-05-07T15:00:00Z",
                "2026-05-07 00:00:00",
            ),
            (
                "../etc/passwd",
                "2026-05-07T15:00:00Z",
                "2026-05-07 00:00:00",
            ),
            ("", "2026-05-07T15:00:00Z", "2026-05-07 00:00:00"),
        ] {
            let got = midnight_in_zone(zone, now.parse().unwrap()).unwrap();
            assert_eq!(got, naive(want), "{zone} at {now}");
        }

        // A directory of the tzdata package is the one name that is not
        // a fallback: IsADirectoryError, which nothing catches.
        let err = midnight_in_zone("America", "2026-05-07T15:00:00Z".parse().unwrap()).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }
}
