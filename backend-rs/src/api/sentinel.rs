//! The Sentinel agent data plane.
//!
//! Ported from `backend/app/api/sentinel.py`: the run lifecycle the
//! agent drives (`/runs/pending`, `/runs/{id}/start`,
//! `/runs/{id}/complete`), the operator's read of a single run, and the
//! agent-key listing.
//!
//! Not ported here: `/config`, `/runs` and `/runs/manual` read the plan
//! cache and the licence client, and minting or revoking a key sends
//! email. Those move with their primitives.

use axum::extract::{Path, Request, State};
use axum::http::request::Parts;
use axum::extract::FromRequestParts;
use axum::Json;
use chrono::NaiveDateTime;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::app::AppState;
use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::models::{iso_naive, now_naive};
use crate::query::{int4, BodyErrors, ModelBody, Query};

/// Who an authenticated agent request is acting as.
///
/// `scoped` rather than `org_id.is_none()` is what every call site
/// branches on, matching the Python: the first-party agent drains every
/// org's queue, and a customer-hosted one must never be able to widen
/// past the org its key row names.
#[derive(Debug, Clone)]
pub struct AgentPrincipal {
    pub org_id: Option<String>,
    pub scoped: bool,
}

impl FromRequestParts<AppState> for AgentPrincipal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Starlette decodes header values as latin-1 and the Python
        // re-encodes them the same way before hashing, so the bytes it
        // hashes are exactly the bytes on the wire. Taking them raw here
        // reproduces that without a round trip.
        //
        // It also sidesteps what the round trip was there to fix:
        // `hmac.compare_digest` on two `str`s raises TypeError when
        // either has a byte above 0x7F, so an unauthenticated probe with
        // one high byte used to 500 all three agent endpoints instead of
        // answering a clean 401.
        let presented = parts
            .headers
            .get("x-sentinel-agent-key")
            .map(|v| v.as_bytes())
            .unwrap_or_default();
        if presented.is_empty() {
            return Err(ApiError::unauthorized("invalid agent key"));
        }

        // 1. The shared first-party key. Guarded on the setting being
        //    non-empty so an unset key can never match an empty-ish
        //    header, and compared in constant time so a timing
        //    side-channel cannot walk a prefix.
        if let Some(shared) = state.config.sentinel_agent_key.as_deref() {
            if !shared.is_empty() && presented.ct_eq(shared.as_bytes()).unwrap_u8() == 1 {
                return Ok(AgentPrincipal {
                    org_id: None,
                    scoped: false,
                });
            }
        }

        // 2. A per-org scoped key. `org_id` comes from the row, never
        //    from anything the caller sends — that is the whole point of
        //    this path.
        let digest = Sha256::digest(presented);
        let key_hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();

        let row: Option<(i32, String)> = sqlx::query_as(
            "SELECT id, org_id FROM sentinel_agent_keys
              WHERE key_hash = $1 AND revoked = false",
        )
        .bind(&key_hash)
        .fetch_optional(&state.pool)
        .await?;

        // The same message and status as a bad shared key: do not tell a
        // caller which of the two key types they got wrong.
        let Some((id, org_id)) = row else {
            return Err(ApiError::unauthorized("invalid agent key"));
        };

        // Best-effort last-seen. An agent unable to work because a
        // bookkeeping write failed is a worse outcome than a stale
        // timestamp, so a failure here is logged and swallowed.
        if let Err(err) = sqlx::query("UPDATE sentinel_agent_keys SET last_used_at = $1 WHERE id = $2")
            .bind(now_naive())
            .bind(id)
            .execute(&state.pool)
            .await
        {
            tracing::warn!(error = %err, key_id = id, "sentinel: could not stamp last_used_at");
        }

        Ok(AgentPrincipal {
            org_id: Some(org_id),
            scoped: true,
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
pub struct SentinelRunRow {
    pub id: String,
    pub org_id: String,
    pub triggered_at: Option<NaiveDateTime>,
    pub trigger_type: String,
    pub camera_id: Option<String>,
    pub tool_call_count: i32,
    pub outcome: String,
    pub severity: Option<String>,
    pub incident_id: Option<i32>,
    pub started_at: Option<NaiveDateTime>,
    pub completed_at: Option<NaiveDateTime>,
    pub manual_prompt: Option<String>,
    pub summary: Option<String>,
    pub tool_trace: Option<String>,
}

pub const RUN_SELECT: &str = r#"
    SELECT id, org_id, triggered_at, trigger_type, camera_id, tool_call_count,
           outcome, severity, incident_id, started_at, completed_at,
           manual_prompt, summary, tool_trace
      FROM sentinel_runs
"#;

const TERMINAL: [&str; 3] = ["incident", "no_action", "error"];

impl SentinelRunRow {
    fn is_terminal(&self) -> bool {
        TERMINAL.contains(&self.outcome.as_str())
    }

    /// The stored trace, or `[]` for anything that does not parse as a
    /// JSON list. Python catches ValueError and TypeError and returns
    /// the empty list, so a corrupt column is invisible rather than a
    /// 500 — and a trace that parses to an object is empty too.
    fn tool_trace_json(&self) -> Value {
        let Some(raw) = self.tool_trace.as_deref() else {
            return json!([]);
        };
        if raw.is_empty() {
            return json!([]);
        }
        match serde_json::from_str::<Value>(raw) {
            Ok(Value::Array(items)) => Value::Array(items),
            _ => json!([]),
        }
    }

    pub fn to_json(&self, include_trace: bool) -> Value {
        let mut d = json!({
            "id": self.id,
            "triggered_at": self.triggered_at.map(iso_naive),
            "trigger_type": self.trigger_type,
            "camera_id": self.camera_id,
            "tool_call_count": self.tool_call_count,
            "outcome": self.outcome,
            "severity": self.severity,
            "incident_id": self.incident_id,
            "started_at": self.started_at.map(iso_naive),
            "completed_at": self.completed_at.map(iso_naive),
            "manual_prompt": self.manual_prompt,
            // `self.summary or ""` — a NULL column reads as the empty
            // string, not as null.
            "summary": self.summary.clone().unwrap_or_default(),
        });
        if include_trace {
            d["tool_trace"] = self.tool_trace_json();
        }
        d
    }
}

/// `GET /api/sentinel/runs/pending` — the agent's work queue.
pub async fn list_pending_runs(
    State(state): State<AppState>,
    agent: AgentPrincipal,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let mut query = Query::parse(request.uri().query());
    let limit = query.int("limit", 20, 1, 100);
    query.finish()?;

    // A scoped key sees only its own org's queue. Without this filter a
    // customer running their own agent receives other customers' pending
    // runs — org ids and incident context included — and the leak is
    // silent, because the agent simply processes what it is handed.
    let rows: Vec<SentinelRunRow> = if agent.scoped {
        sqlx::query_as(&format!(
            "{RUN_SELECT} WHERE outcome = 'pending' AND org_id = $1
              ORDER BY triggered_at ASC LIMIT $2"
        ))
        .bind(agent.org_id.as_deref().unwrap_or_default())
        .bind(limit)
        .fetch_all(&state.pool)
        .await?
    } else {
        sqlx::query_as(&format!(
            "{RUN_SELECT} WHERE outcome = 'pending'
              ORDER BY triggered_at ASC LIMIT $1"
        ))
        .bind(limit)
        .fetch_all(&state.pool)
        .await?
    };

    let runs: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut v = r.to_json(false);
            // The agent needs org_id to pick the right MCP key, and
            // to_dict() omits it because the dashboard does not.
            v["org_id"] = json!(r.org_id);
            v
        })
        .collect();
    Ok(Json(json!({ "runs": runs })))
}

/// `GET /api/sentinel/runs/{run_id}` — one run with its full trace, for
/// the operator's drawer. Session auth, not agent auth.
pub async fn get_run(
    State(state): State<AppState>,
    RequireView(user): RequireView,
    Path(run_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let run_id = crate::query::path_segment(&run_id)?;
    let row: Option<SentinelRunRow> =
        sqlx::query_as(&format!("{RUN_SELECT} WHERE org_id = $1 AND id = $2"))
            .bind(&user.org_id)
            .bind(run_id)
            .fetch_optional(&state.pool)
            .await?;
    let row = row.ok_or_else(|| ApiError::not_found("run not found"))?;
    Ok(Json(row.to_json(true)))
}

/// `GET /api/sentinel/agent-keys`.
///
/// Deliberately not plan-gated, unlike minting: an org that downgrades
/// `osa_` — the prefix that tells an agent key from an MCP or
/// integration one at a glance, and in the auth path.
const AGENT_KEY_PREFIX: &str = "osa_";

/// What an admin reads when an agent key is created.
fn agent_key_create_body(actor: &str, name: &str) -> String {
    format!(
        "{actor} just created a Sentinel agent key \"{name}\".  Anyone \
         holding it can run the Sentinel agent against this organization's \
         cameras.  If this was you, no action needed.  If not, revoke it \
         from the MCP settings page immediately."
    )
}

/// `raw_key[-4:]` — what is stored and shown so an operator can tell
/// two keys apart without holding either.
///
/// Its own function because the differential cannot see it: four hex
/// characters are too short to substitute by value, so the harness
/// blanks the column by name, and a wrong suffix is invisible there.
/// The test below is the only thing that checks it, and it has to call
/// *this* rather than recompute the expression — a test that keeps its
/// own copy of the logic agrees with itself no matter what the handler
/// does.
fn key_last4(raw_key: &str) -> String {
    let chars: Vec<char> = raw_key.chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

/// `POST /api/sentinel/agent-keys` — mint one, return it once.
///
/// `require_active_billing` rather than `require_admin`: this hands out
/// a credential that spends money, since every run the agent completes
/// burns a cap slot and real model cost. Revoking stays on plain admin,
/// so a past-due org can still turn a key off.
///
/// The plan and licence gate here is a UX gate, not the security
/// boundary — the MCP surface re-checks both on every tool call,
/// because a plan can change long after a key is minted. Failing here
/// means a free org finds out now, with an upgrade prompt, instead of
/// at 3am through an opaque 401 from an agent they already configured.
pub async fn create_agent_key(
    rate: crate::ratelimit::PerHour<10>,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    ModelBody(crate::auth::RequireActiveBilling(user), body): ModelBody<
        crate::auth::RequireActiveBilling,
    >,
) -> Result<Json<Value>, ApiError> {
    let mut errors = BodyErrors::new();
    // `name: str = Field("Self-hosted agent", max_length=100)`.
    let name = match body.get("name") {
        None => "Self-hosted agent".to_string(),
        Some(Value::String(value)) => {
            if value.chars().count() > 100 {
                errors.too_long("name", value, 100);
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

    let (has_access, denial) =
        crate::api::sentinel_config::resolve_sentinel_access(&state, &user.org_id).await?;
    if !has_access {
        return Err(ApiError::new(
            axum::http::StatusCode::PAYMENT_REQUIRED,
            denial,
        ));
    }

    // `key_hash` is UNIQUE. A 128-bit collision will not happen, but an
    // unhandled conflict would be a 500, so it is absorbed and retried
    // once rather than leaving this endpoint's only failure path
    // uncovered.
    let mut minted: Option<(i32, String, Option<NaiveDateTime>)> = None;
    let mut raw_key = String::new();
    for attempt in 1..=2 {
        raw_key = format!("{AGENT_KEY_PREFIX}{}", crate::crypto::token_hex(16));
        let key_hash = crate::crypto::hex(&Sha256::digest(raw_key.as_bytes()));
        let last4 = key_last4(&raw_key);
        let inserted: Result<(i32, String, Option<NaiveDateTime>), _> = sqlx::query_as(
            "INSERT INTO sentinel_agent_keys
                (org_id, key_hash, key_last4, name, created_by, revoked, created_at)
             VALUES ($1, $2, $3, $4, $5, false, $6)
             RETURNING id, key_last4, created_at",
        )
        .bind(&user.org_id)
        .bind(&key_hash)
        .bind(&last4)
        .bind(&name)
        // The user id, not the label: this column is varchar(100) and an
        // email can overflow it. The readable actor is in the audit row.
        .bind(&user.user_id)
        .bind(now_naive())
        .fetch_one(&state.pool)
        .await;
        match inserted {
            Ok(row) => {
                minted = Some(row);
                break;
            }
            Err(err) if attempt == 1 && is_unique_violation(&err) => continue,
            Err(err) => return Err(err.into()),
        }
    }
    let Some((key_id, key_last4, created_at)) = minted else {
        return Err(ApiError::internal("could not mint an agent key"));
    };

    let label = crate::audit::audit_label(&user);
    crate::audit::write_audit(
        &state.pool,
        &user.org_id,
        "sentinel_agent_key_created",
        &user.user_id,
        &label,
        Some(crate::audit::python_json(&[
            ("key_id", json!(key_id)),
            ("name", json!(name)),
            ("key_last4", json!(key_last4)),
        ])),
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
            "sentinel_agent_key_created",
            format!("New Sentinel agent key created: {name}"),
        )
        .body(agent_key_create_body(&actor, &name))
        .severity("warning")
        .audience("admin")
        .link("/mcp")
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
        // The only time this value exists outside the caller's machine.
        "key": raw_key,
        "key_last4": key_last4,
        "created_at": created_at.map(iso_naive),
        "warning": "Save this key now. You won't be able to see it again.",
    })))
}

/// A UNIQUE constraint rejection, as opposed to any other database
/// failure — the retry above must not swallow the rest.
fn is_unique_violation(err: &sqlx::Error) -> bool {
    matches!(err.as_database_error().and_then(|e| e.code()), Some(code) if code == "23505")
}

/// What an admin reads when an agent key is revoked.
///
/// Extracted for the same reason as its MCP counterpart: the Python
/// joins two f-strings, and the join carries a deliberate double space
/// after the full stop.
fn agent_key_revoke_body(actor: &str, name: &str) -> String {
    format!(
        "{actor} revoked the Sentinel agent key \"{name}\".  \
         Any agent still using it will start failing immediately."
    )
}

/// `DELETE /api/sentinel/agent-keys/{key_id}`.
///
/// A soft revoke, matching the MCP keys: `last_used_at` stays as the
/// forensic answer to "when did this leaked credential last act?", and
/// the unique `key_hash` stays permanently burned.
///
/// The `org_id` in the filter is the security control, not a
/// convenience — without it any admin could revoke any org's key — and
/// a miss is a 404 rather than a 403 so a caller cannot probe which key
/// ids exist elsewhere.
pub async fn revoke_agent_key(
    rate: crate::ratelimit::PerHour<30>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    Path(key_id): Path<String>,
    headers: axum::http::HeaderMap,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> Result<Json<Value>, ApiError> {
    let key_id = crate::query::path_int("key_id", &key_id)?;
    rate.check().await?;
    let key_id = int4(key_id)?;

    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT name, revoked FROM sentinel_agent_keys WHERE id = $1 AND org_id = $2",
    )
    .bind(key_id)
    .bind(&user.org_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((name, revoked)) = row else {
        return Err(ApiError::not_found("agent key not found"));
    };

    // SQLAlchemy emits no UPDATE when the value is unchanged, so
    // re-revoking writes nothing — which the side-effect differential
    // counts.
    if !revoked {
        sqlx::query("UPDATE sentinel_agent_keys SET revoked = true WHERE id = $1 AND org_id = $2")
            .bind(key_id)
            .bind(&user.org_id)
            .execute(&state.pool)
            .await?;
    }

    let label = crate::audit::audit_label(&user);
    crate::audit::write_audit(
        &state.pool,
        &user.org_id,
        "sentinel_agent_key_revoked",
        &user.user_id,
        &label,
        Some(crate::audit::python_json(&[
            ("key_id", json!(key_id)),
            ("name", json!(name)),
        ])),
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
            "sentinel_agent_key_revoked",
            format!("Sentinel agent key revoked: {name}"),
        )
        .body(agent_key_revoke_body(&actor, &name))
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

/// must still be able to see and revoke credentials it already issued.
pub async fn list_agent_keys(
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
) -> Result<Json<Value>, ApiError> {
    #[derive(sqlx::FromRow)]
    struct KeyRow {
        id: i32,
        name: String,
        key_last4: Option<String>,
        created_at: Option<NaiveDateTime>,
        created_by: Option<String>,
        last_used_at: Option<NaiveDateTime>,
        revoked: bool,
    }

    // key_hash is never selected, let alone returned: it is the SHA-256
    // of a credential that reaches an org's cameras, and handing it to a
    // browser would put an offline-crackable digest in a network tab.
    let rows: Vec<KeyRow> = sqlx::query_as(
        "SELECT id, name, key_last4, created_at, created_by, last_used_at, revoked
           FROM sentinel_agent_keys
          WHERE org_id = $1 AND revoked = false
          ORDER BY created_at DESC",
    )
    .bind(&user.org_id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(Value::Array(
        rows.iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "name": r.name,
                    "key_last4": r.key_last4,
                    "created_at": r.created_at.map(iso_naive),
                    "created_by": r.created_by,
                    "last_used_at": r.last_used_at.map(iso_naive),
                    "revoked": r.revoked,
                })
            })
            .collect(),
    )))
}

/// `POST /api/sentinel/runs/{run_id}/start` — the agent claims a run.
pub async fn post_run_start(
    State(state): State<AppState>,
    agent: AgentPrincipal,
    Path(run_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let run_id = crate::query::path_segment(&run_id)?;
    let row = agent_visible_run(&state, &agent, run_id).await?;

    if row.outcome != "pending" {
        // Past pending already: accept idempotently, but say the caller
        // did NOT win the claim. Without the flag two overlapping drains
        // both got an indistinguishable 200 and both ran the full agent
        // loop — duplicate incidents and double LLM spend.
        let mut result = row.to_json(false);
        result["claimed"] = json!(false);
        return Ok(Json(result));
    }

    let started = now_naive();
    sqlx::query(
        "UPDATE sentinel_runs SET outcome = 'running', started_at = $1, updated_at = $2
          WHERE id = $3",
    )
    .bind(started)
    .bind(started)
    .bind(&row.id)
    .execute(&state.pool)
    .await?;

    let row = reload(&state, &row.id).await?;
    let mut result = row.to_json(false);
    result["claimed"] = json!(true);
    Ok(Json(result))
}

/// `POST /api/sentinel/runs/{run_id}/complete` — the agent reports a
/// terminal outcome.
pub async fn post_run_complete(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    ModelBody(agent, body): ModelBody<AgentPrincipal>,
) -> Result<Json<Value>, ApiError> {
    let run_id = crate::query::path_segment(&run_id)?.to_string();

    let mut errors = BodyErrors::new();
    let outcome = errors.required_string(&body, "outcome", usize::MAX);
    let severity = errors.optional_string(&body, "severity", usize::MAX);
    let incident_id = errors.optional_int(&body, "incident_id");
    let summary = errors.string_with_default(&body, "summary", 8000);
    let tool_call_count = errors.int_with_default(&body, "tool_call_count", 0);
    let tool_trace = errors.optional_list_of_objects(&body, "tool_trace");
    errors.finish()?;

    if !TERMINAL.contains(&outcome.as_str()) {
        // `{body.outcome!r}` — Python's repr, so the value comes back
        // in single quotes.
        return Err(ApiError::bad_request(format!(
            "invalid outcome: {}",
            python_repr(&outcome)
        )));
    }
    // "critical" is included on purpose: the MCP create_incident enum
    // and the agent's own prompt both allow it, and rejecting it here
    // 400'd exactly the highest-urgency completions, downgrading those
    // runs to error.
    if outcome == "incident"
        && !matches!(
            severity.as_deref(),
            Some("low") | Some("medium") | Some("high") | Some("critical")
        )
    {
        return Err(ApiError::bad_request("severity required for outcome=incident"));
    }

    let row = agent_visible_run(&state, &agent, &run_id).await?;

    if row.is_terminal() {
        // One-way upgrade error -> real outcome is allowed: the
        // wall-clock timeout wrapper marks an in-flight run `error` at
        // 270s, and a run that finishes afterwards should land its real
        // result rather than stay behind that defensive stamp. The
        // reverse is refused, so a reported outcome cannot be
        // downgraded.
        let upgrading = row.outcome == "error" && matches!(outcome.as_str(), "incident" | "no_action");
        if !upgrading {
            return Ok(Json(row.to_json(true)));
        }
    }

    // Defence in depth: a leaked agent key must not be able to point a
    // run at another org's incident, which would surface as a foreign
    // deep-link in that org's run drawer.
    if outcome == "incident" {
        if let Some(incident_id) = incident_id {
            // Narrowed to the column's own width, because SQLAlchemy
            // types the bind from the column: an id no `incidents.id`
            // could hold is the 500 Postgres raises, not a miss. Nor is
            // it a clamp — that would let a caller's 3000000000 match
            // incident 2147483647.
            let owned: Option<(i32,)> =
                sqlx::query_as("SELECT id FROM incidents WHERE id = $1 AND org_id = $2")
                    .bind(int4(incident_id)?)
                    .bind(&row.org_id)
                    .fetch_optional(&state.pool)
                    .await?;
            if owned.is_none() {
                return Err(ApiError::bad_request(
                    "incident_id does not belong to this run's org",
                ));
            }
        }
    }

    let now = now_naive();
    let is_incident = outcome == "incident";
    let stored_trace = tool_trace.as_ref().map(|t| serialise_tool_trace(t));

    // A Python int beyond i64 cannot be bound at all, and would have
    // been refused by the column anyway; `max(0)` is the Python's own
    // clamp of a negative count.
    // Narrowed only on the branch that stores it: any other outcome
    // discards `incident_id` before it reaches the column, so a value
    // too large for one is not an error there.
    let incident_bind = match incident_id.filter(|_| is_incident) {
        Some(v) => Some(int4(v)?),
        None => None,
    };
    let tool_call_bind = i32::try_from(
        tool_call_count
            .max_zero()
            .ok_or_else(|| ApiError::internal("integer out of range"))?,
    )
    .map_err(|_| ApiError::internal("integer out of range"))?;

    sqlx::query(
        "UPDATE sentinel_runs
            SET outcome = $1,
                severity = $2,
                incident_id = $3,
                summary = $4,
                tool_call_count = $5,
                tool_trace = COALESCE($6, tool_trace),
                started_at = COALESCE(started_at, $7),
                completed_at = $7,
                updated_at = $7
          WHERE id = $8",
    )
    .bind(&outcome)
    .bind(if is_incident { severity.as_deref() } else { None })
    // Both of these are bigints on the way in, so a value too large for
    // the `integer` column is the "integer out of range" Postgres raises
    // for the Python too — a 500, not a silently clamped row.
    .bind(incident_bind)
    .bind(truncate_chars(&summary, 8000))
    .bind(tool_call_bind)
    .bind(stored_trace)
    .bind(now)
    .bind(&row.id)
    .execute(&state.pool)
    .await?;

    let row = reload(&state, &row.id).await?;
    tracing::info!(
        id = %row.id, org = %row.org_id, outcome = %row.outcome,
        severity = ?row.severity, "sentinel: run completed"
    );
    Ok(Json(row.to_json(true)))
}

/// Fetch a run an agent principal is allowed to touch.
///
/// A scoped key may only reach its own org's runs, and the refusal is a
/// 404 rather than a 403 so it cannot be used to probe which run ids
/// exist. `/complete` writes an incident reference, so this is guarding
/// a write into another tenant's data, not just a read.
async fn agent_visible_run(
    state: &AppState,
    agent: &AgentPrincipal,
    run_id: &str,
) -> Result<SentinelRunRow, ApiError> {
    let row: Option<SentinelRunRow> = sqlx::query_as(&format!("{RUN_SELECT} WHERE id = $1"))
        .bind(run_id)
        .fetch_optional(&state.pool)
        .await?;
    let row = row.ok_or_else(|| ApiError::not_found("run not found"))?;
    if agent.scoped && Some(row.org_id.as_str()) != agent.org_id.as_deref() {
        return Err(ApiError::not_found("run not found"));
    }
    Ok(row)
}

async fn reload(state: &AppState, run_id: &str) -> Result<SentinelRunRow, ApiError> {
    sqlx::query_as(&format!("{RUN_SELECT} WHERE id = $1"))
        .bind(run_id)
        .fetch_one(&state.pool)
        .await
        .map_err(Into::into)
}

/// `repr()` of a Python string: single quotes unless the value contains
/// one and no double quote, and backslash-escaped otherwise.
fn python_repr(s: &str) -> String {
    let has_single = s.contains('\'');
    let has_double = s.contains('"');
    let quote = if has_single && !has_double { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Cut to `limit` **characters**, the way Python slices a `str`.
fn truncate_chars(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    s.chars().take(limit).collect()
}

/// `set_tool_trace` — two layers of cap, then `json.dumps`.
///
/// The caps are not cosmetic: without the per-entry limits a leaked key
/// could post fifty multi-megabyte results into one row.
fn serialise_tool_trace(trace: &[Value]) -> String {
    let mut sanitised: Vec<Value> = Vec::new();
    // Last fifty entries, protecting against a runaway agent.
    let start = trace.len().saturating_sub(50);
    for entry in &trace[start..] {
        let Some(map) = entry.as_object() else {
            continue;
        };
        let tool = cap_str(map.get("tool"), 200);
        let result = cap_str(map.get("result"), 1000);
        let raw_args = match map.get("args") {
            Some(Value::Object(m)) => Value::Object(m.clone()),
            _ => json!({}),
        };
        let args_json = crate::audit::python_json_value(&raw_args);
        let args_payload = if args_json.chars().count() > 1500 {
            // Re-emitted as a string blob: the truncated JSON would no
            // longer parse if it were stuffed back as an object, and the
            // run drawer treats `args` as opaque anyway.
            json!({ "_truncated": format!("{}\u{2026}", truncate_chars(&args_json, 1500)) })
        } else {
            raw_args
        };
        sanitised.push(json!({
            "tool": tool,
            "args": args_payload,
            "result": result,
        }));
    }
    crate::audit::python_json_value(&Value::Array(sanitised))
}

/// `_cap_str`: stringify anything that is not already a string, then cut
/// to `limit` characters with an ellipsis.
fn cap_str(value: Option<&Value>, limit: usize) -> String {
    let s = match value {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => python_str(other),
    };
    if s.chars().count() <= limit {
        s
    } else {
        format!("{}\u{2026}", truncate_chars(&s, limit))
    }
}

/// `str()` of a value that arrived as JSON.
///
/// Python's spelling differs from JSON's for three of them: `True`,
/// `False` and `None`. A dict or list stringifies with Python's repr,
/// which uses single quotes.
fn python_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(python_repr_value).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", python_repr(k), python_repr_value(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

fn python_repr_value(value: &Value) -> String {
    match value {
        Value::String(s) => python_repr(s),
        other => python_str(other),
    }
}

use crate::auth::RequireView;

#[cfg(test)]
mod tests {
    use super::*;

    /// `key_last4` is stored and returned separately from the key, and
    /// the differential blanks it by name — substituting four hex
    /// characters by value would rewrite unrelated runs inside other
    /// hashes. That it really is the key's own last four is held here
    /// instead, which is the only place that has the key.
    #[test]
    fn the_stored_suffix_is_the_keys_own_last_four() {
        for _ in 0..32 {
            let key = format!("{AGENT_KEY_PREFIX}{}", crate::crypto::token_hex(16));
            let last4 = key_last4(&key);
            assert_eq!(last4.len(), 4);
            assert!(key.ends_with(&last4), "{key} does not end with {last4}");
            // `osa_` plus 32 hex characters.
            assert_eq!(key.len(), 4 + 32);
            assert!(key.strip_prefix("osa_").unwrap().chars().all(|c| c.is_ascii_hexdigit()));
        }
        // Shorter than four characters takes the saturating branch
        // rather than panicking on the slice.
        assert_eq!(key_last4("ab"), "ab");
        assert_eq!(key_last4(""), "");
        // Counted in characters, not bytes — slicing bytes here would
        // split one and panic.
        assert_eq!(key_last4("aé🎥bc"), "é🎥bc");
    }

    /// Two f-strings joined with a deliberate double space after the
    /// full stop — the string that lands in `notifications.body`.
    #[test]
    fn the_agent_key_revoke_notification_keeps_its_double_space() {
        assert_eq!(
            agent_key_revoke_body("alice@example.com", "prod agent"),
            "alice@example.com revoked the Sentinel agent key \"prod agent\".  \
             Any agent still using it will start failing immediately."
        );
    }

    /// Expected strings produced by running `set_tool_trace`'s own body
    /// under CPython, not reasoned out. `json.dumps` defaults apply:
    /// `", "` / `": "` separators and `ensure_ascii=True`.
    #[test]
    fn a_trace_is_serialised_the_way_set_tool_trace_serialises_it() {
        let t = |v: Value| serialise_tool_trace(v.as_array().unwrap());

        assert_eq!(t(json!([])), "[]");
        assert_eq!(
            t(json!([{"tool": "get_camera", "args": {"camera_id": "cam-live"}, "result": "ok"}])),
            r#"[{"tool": "get_camera", "args": {"camera_id": "cam-live"}, "result": "ok"}]"#
        );
        // Absent keys become "" and {}, not null.
        assert_eq!(
            t(json!([{}])),
            r#"[{"tool": "", "args": {}, "result": ""}]"#
        );
        // _cap_str stringifies with Python's spelling, and a non-dict
        // `args` is replaced wholesale rather than coerced.
        assert_eq!(
            t(json!([{"tool": 7, "args": "not a dict", "result": null}])),
            r#"[{"tool": "7", "args": {}, "result": "None"}]"#
        );
        // Non-ASCII is escaped, astral characters as a surrogate pair —
        // `json.dumps` defaults to ensure_ascii=True. The expected
        // value here is the literal text CPython produces, so the
        // backslashes below are real characters in the output, not
        // Rust escapes.
        assert_eq!(
            t(json!([{"tool": "caf\u{e9}", "args": {"n": "\u{e9}"}, "result": "\u{1F3A5}"}])),
            r#"[{"tool": "caf\u00e9", "args": {"n": "\u00e9"}, "result": "\ud83c\udfa5"}]"#
        );
        // Entries that are not objects are dropped, not coerced.
        assert_eq!(
            t(json!([1, "a", {"tool": "ok"}])),
            r#"[{"tool": "ok", "args": {}, "result": ""}]"#
        );
        // JSON scalars keep their JSON spelling inside `args`, because
        // that path goes through json.dumps rather than str().
        assert_eq!(
            t(json!([{"tool": "x", "args": {"a": true, "b": null, "c": [1, 2]}, "result": "y"}])),
            r#"[{"tool": "x", "args": {"a": true, "b": null, "c": [1, 2]}, "result": "y"}]"#
        );
    }

    #[test]
    fn only_the_last_fifty_trace_entries_are_kept() {
        let trace: Vec<Value> = (0..60)
            .map(|i| json!({"tool": format!("t{i}"), "args": {"i": i}, "result": "r"}))
            .collect();
        let out = serialise_tool_trace(&trace);
        // CPython keeps [-50:], so t0..t9 are gone and t10 leads.
        assert!(out.starts_with(r#"[{"tool": "t10", "args": {"i": 10}, "result": "r"}"#), "{out}");
        assert!(out.ends_with(r#"{"tool": "t59", "args": {"i": 59}, "result": "r"}]"#), "{out}");
        assert_eq!(out.matches("\"tool\": ").count(), 50);
        assert_eq!(out.chars().count(), 2550, "byte-for-byte length CPython produces");
    }

    #[test]
    fn oversized_trace_fields_are_cut_with_an_ellipsis() {
        let out = serialise_tool_trace(&[json!({
            "tool": "n".repeat(300),
            "args": {"blob": "a".repeat(2000)},
            "result": "r".repeat(1500),
        })]);
        // tool -> 200 chars + U+2026, result -> 1000 + U+2026, and args
        // is re-emitted as {"_truncated": "<1500 chars>…"} because its
        // JSON is over the limit.
        assert!(out.contains(&format!("\"tool\": \"{}\\u2026\"", "n".repeat(200))));
        assert!(out.contains("\"_truncated\""));
        assert!(out.contains(&format!("\"result\": \"{}\\u2026\"", "r".repeat(1000))));
        assert_eq!(out.chars().count(), 2777, "length CPython produces");
    }

    #[test]
    fn a_corrupt_trace_column_reads_as_an_empty_list() {
        // Python catches ValueError and TypeError and returns []. A
        // 500 here would take out the operator's run drawer for one bad
        // row.
        let mut row = run_row();
        for raw in [None, Some(""), Some("not json at all"), Some(r#"{"not": "a list"}"#),
                    Some("null"), Some("7")] {
            row.tool_trace = raw.map(str::to_string);
            assert_eq!(row.tool_trace_json(), json!([]), "raw {raw:?}");
        }
        row.tool_trace = Some(r#"[{"tool": "x"}]"#.into());
        assert_eq!(row.tool_trace_json(), json!([{"tool": "x"}]));
    }

    #[test]
    fn a_null_summary_column_reads_as_the_empty_string() {
        // `self.summary or ""` — null is "", not null, and the SPA
        // renders it directly.
        let mut row = run_row();
        row.summary = None;
        assert_eq!(row.to_json(false)["summary"], json!(""));
        row.summary = Some("said something".into());
        assert_eq!(row.to_json(false)["summary"], json!("said something"));
    }

    #[test]
    fn the_trace_is_only_included_when_asked_for() {
        // /runs/pending and /start omit it; /runs/{id} and /complete
        // include it. An extra key would be a body diff on every run.
        let row = run_row();
        assert!(row.to_json(false).get("tool_trace").is_none());
        assert!(row.to_json(true).get("tool_trace").is_some());
    }

    #[test]
    fn only_the_three_terminal_outcomes_are_terminal() {
        let mut row = run_row();
        for (outcome, terminal) in [
            ("pending", false), ("running", false),
            ("incident", true), ("no_action", true), ("error", true),
        ] {
            row.outcome = outcome.to_string();
            assert_eq!(row.is_terminal(), terminal, "{outcome}");
        }
    }

    #[test]
    fn the_bad_outcome_message_quotes_like_pythons_repr() {
        // The 400 body interpolates `{body.outcome!r}`, so the value
        // comes back in single quotes — and in double quotes when it
        // contains a single one.
        assert_eq!(python_repr("nope"), "'nope'");
        assert_eq!(python_repr("it's bad"), "\"it's bad\"");
        assert_eq!(python_repr("both ' and \""), "'both \\' and \"'");
        assert_eq!(python_repr("line\nbreak"), "'line\\nbreak'");
        assert_eq!(python_repr("back\\slash"), "'back\\\\slash'");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        // Python slices a str by characters; a four-byte emoji is one.
        assert_eq!(truncate_chars("\u{1F3A5}\u{1F3A5}\u{1F3A5}", 2), "\u{1F3A5}\u{1F3A5}");
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("caf\u{e9}s", 4), "caf\u{e9}");
    }

    fn run_row() -> SentinelRunRow {
        SentinelRunRow {
            id: "run1".into(),
            org_id: "self-host".into(),
            triggered_at: Some(now_naive()),
            trigger_type: "motion".into(),
            camera_id: None,
            tool_call_count: 0,
            outcome: "pending".into(),
            severity: None,
            incident_id: None,
            started_at: None,
            completed_at: None,
            manual_prompt: None,
            summary: Some(String::new()),
            tool_trace: None,
        }
    }
}
