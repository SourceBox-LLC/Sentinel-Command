//! Resolving an MCP bearer token to an org, a tool set and a budget.
//!
//! Ported from `_resolve_org`, `_resolve_via_agent_key` and
//! `ScopeMiddleware._lookup_allowed` in
//! `backend/app/mcp/server.py`.
//!
//! Three credentials reach this surface and they are not
//! interchangeable:
//!
//!   * a per-org `osc_` key from `mcp_api_keys`, which carries its own
//!     scope and is rate-limited per key;
//!   * the shared multi-tenant agent key, which may act for any
//!     eligible org and names it in a header;
//!   * a scoped per-org agent key from `sentinel_agent_keys`, which is
//!     bound to one org by its row — that is what makes it safe to
//!     hand to a customer running the agent themselves.
//!
//! `kind = "mcp"` is load-bearing on the first: an integration key
//! (`osi_`) lives in the same table and must not reach the tool
//! surface, exactly as an MCP key must not reach `/api/integration/*`.
//!
//! **The scope lookup and the org resolution are separate passes over
//! the same token, and they disagree.** That is Python's shape, not a
//! simplification: the middleware defers to the tool's own auth when it
//! cannot recognise a key, so an unknown token gets an unfiltered
//! `tools/list` and then a refusal when it calls one. Reproduced,
//! except where the disagreement was a hole: a scoped agent key used to
//! be unrecognised by the scope pass and authenticated by the org pass,
//! which let it call every tool. See `lookup_allowed` and PYTHON_BUGS.md
//! #13.

use std::collections::BTreeSet;

use axum::http::HeaderMap;

use crate::app::AppState;
use crate::mcp::scope::{self, Breach};

/// What a resolved caller may do.
pub struct Principal {
    pub org_id: String,
    /// Written to the activity log, so a tool call can be traced back
    /// to the credential that made it.
    pub key_name: String,
}

/// The message a refusal carries, which becomes a `ToolError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthError(pub String);

/// The per-key limiter, shared by every caller of this module.
pub static RATE_LIMITER: scope::RateLimiter = scope::RateLimiter::new_static();

/// `bearer` — the raw token, or why there isn't one.
fn bearer(headers: &HeaderMap) -> Result<String, AuthError> {
    // Starlette lower-cases header names, and the check is on the
    // scheme only — `BEARER` and `bearer` both pass.
    let auth = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !auth.to_lowercase().starts_with("bearer ") {
        return Err(AuthError("Unauthorized: missing Bearer token".to_string()));
    }
    // `auth.split(" ", 1)[1].strip()`.
    let raw = auth
        .split_once(' ')
        .map(|rest| rest.1)
        .unwrap_or("")
        .trim()
        .to_string();
    if raw.is_empty() {
        return Err(AuthError("Unauthorized: empty Bearer token".to_string()));
    }
    Ok(raw)
}

fn sha256_hex(raw: &str) -> String {
    use sha2::Digest;
    crate::crypto::hex(&sha2::Sha256::digest(raw.as_bytes()))
}

/// Whether the presented token is the shared multi-tenant agent key.
///
/// Compared in constant time, and an unset key rejects everything:
/// `hmac.compare_digest("", anything)` is False, so leaving the
/// variable unset disables the path rather than opening it.
fn is_shared_agent_key(state: &AppState, raw: &str) -> bool {
    let Some(configured) = state.config.sentinel_agent_mcp_key.as_deref() else {
        return false;
    };
    if configured.is_empty() {
        return false;
    }
    constant_time_eq(configured.as_bytes(), raw.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// `ScopeMiddleware._lookup_allowed` — the set to filter `tools/list`
/// by and to gate `tools/call` on, or `None` to defer.
///
/// `None` is not "no access". It means the middleware did not
/// recognise the token and leaves the decision to the tool's own auth,
/// which is why an unknown key sees every tool listed and is refused
/// only when it calls one.
///
/// **Every agent credential gets the agent allowlist** — the shared key
/// and a scoped per-org `osa_` key alike. The Python consulted only the
/// shared key and `mcp_api_keys` here, so a scoped key fell through to
/// `None`, was authenticated a moment later by `resolve`, and could call
/// `set_camera_recording_policy`: the one tool the allowlist exists to
/// withhold, because an agent steered by what a camera sees must not be
/// able to switch the camera off. That key is the one handed to
/// customers running the agent on their own hardware. PYTHON_BUGS.md #13;
/// reproduced while the two stacks had to agree, closed now that the
/// Python is gone.
///
/// A lookup that fails on the database returns `None` and defers to
/// `resolve`, which fails closed on the same error.
pub async fn lookup_allowed(
    state: &AppState,
    headers: &HeaderMap,
) -> Option<BTreeSet<&'static str>> {
    let raw = bearer(headers).ok()?;
    if is_shared_agent_key(state, &raw) {
        return Some(scope::agent_allowed_tools());
    }
    let key_hash = sha256_hex(&raw);
    let scoped_agent: Option<(i32,)> = sqlx::query_as(
        "SELECT id FROM sentinel_agent_keys WHERE key_hash = $1 AND revoked = false LIMIT 1",
    )
    .bind(&key_hash)
    .fetch_optional(&state.pool)
    .await
    .ok()?;
    if scoped_agent.is_some() {
        return Some(scope::agent_allowed_tools());
    }
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT scope_mode, scope_tools FROM mcp_api_keys
          WHERE key_hash = $1 AND revoked = false AND kind = 'mcp' LIMIT 1",
    )
    .bind(&key_hash)
    .fetch_optional(&state.pool)
    .await
    .ok()?;
    let (scope_mode, scope_tools) = row?;
    // `mcp_key.get_scope_tools()` — a NULL or unparseable column is
    // `[]`, never an error.
    let tools = parse_scope_tools(scope_tools.as_deref());
    Some(scope::compute_allowed_tools(
        scope_mode.as_deref(),
        Some(&tools),
    ))
}

/// `get_scope_tools()` — the stored JSON list, or `[]`.
///
/// Non-string elements are stringified rather than dropped, which is
/// what `[str(v) for v in val]` does.
pub fn parse_scope_tools(stored: Option<&str>) -> Vec<String> {
    let Some(stored) = stored.filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    match serde_json::from_str::<serde_json::Value>(stored) {
        Ok(serde_json::Value::Array(items)) => items.iter().map(crate::pyrepr::str_value).collect(),
        _ => Vec::new(),
    }
}

/// `_resolve_org` — the org this call acts for, or why it may not.
pub async fn resolve(state: &AppState, headers: &HeaderMap) -> Result<Principal, AuthError> {
    let raw = bearer(headers)?;

    if is_shared_agent_key(state, &raw) {
        return resolve_via_agent_key(state, headers, None).await;
    }

    let key_hash = sha256_hex(&raw);

    // The scoped per-org agent key. Bound to one org by its row, which
    // is what makes it safe to give to a customer; it goes through the
    // same resolver so plan eligibility, licence state and per-org
    // limits are enforced identically.
    let scoped: Option<(String,)> = sqlx::query_as(
        "SELECT org_id FROM sentinel_agent_keys
          WHERE key_hash = $1 AND revoked = false LIMIT 1",
    )
    .bind(&key_hash)
    .fetch_optional(&state.pool)
    .await
    .map_err(|_| AuthError("Authentication error".to_string()))?;
    if let Some((org_id,)) = scoped {
        return resolve_via_agent_key(state, headers, Some(org_id)).await;
    }

    let key: Option<(i32, String, String)> = sqlx::query_as(
        "SELECT id, org_id, name FROM mcp_api_keys
          WHERE key_hash = $1 AND revoked = false AND kind = 'mcp' LIMIT 1",
    )
    .bind(&key_hash)
    .fetch_optional(&state.pool)
    .await
    .map_err(|_| AuthError("Authentication error".to_string()))?;
    let Some((key_id, org_id, key_name)) = key else {
        return Err(AuthError(
            "Unauthorized: invalid or revoked API key".to_string(),
        ));
    };

    // The EFFECTIVE plan, not the nominal one: an org past due beyond
    // the grace window is tightened to free for cameras and viewer
    // hours, and its MCP keys tighten with it rather than keeping Pro
    // limits until a cancellation webhook that may never arrive.
    let ctx = crate::api::sentinel_config::plan_ctx(state);
    let plan = crate::plans::effective_plan_for_caps(&ctx, &org_id, true).await;
    let Some((minute, daily)) = scope::rate_limits(&plan) else {
        return Err(AuthError(
            "MCP requires a Pro or Pro Plus plan. Upgrade at /pricing.".to_string(),
        ));
    };

    let (allowed, _remaining, breach) = RATE_LIMITER.check(&key_hash, minute, daily);
    if !allowed {
        let plan_name = crate::plans::get_plan_display_name(&plan);
        return Err(AuthError(match breach {
            Breach::Minute => format!(
                "Rate limit exceeded: {minute} calls/min allowed on the \
                 {plan_name} plan. Try again shortly."
            ),
            // The daily cap almost always means a runaway automation,
            // so it says so rather than inviting a retry.
            _ => format!(
                "Daily cap reached: {daily} calls/24h on the {plan_name} plan. \
                 This usually means an agent is stuck in a loop. The cap resets \
                 24h after the first call. Upgrade your plan for a higher ceiling."
            ),
        }));
    }

    if let Err(err) = sqlx::query("UPDATE mcp_api_keys SET last_used_at = $1 WHERE id = $2")
        .bind(crate::models::now_naive())
        .bind(key_id)
        .execute(&state.pool)
        .await
    {
        tracing::error!(error = %err, "failed to stamp mcp key last_used_at");
    }

    Ok(Principal { org_id, key_name })
}

/// `_resolve_via_agent_key`.
async fn resolve_via_agent_key(
    state: &AppState,
    headers: &HeaderMap,
    forced_org: Option<String>,
) -> Result<Principal, AuthError> {
    let header_org = headers
        .get("x-agent-org-override")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();

    let org_id = match forced_org {
        Some(forced) => {
            // The header is permitted — the agent sends it for every
            // run — but only when it AGREES. Silently ignoring a
            // mismatch would be worse than refusing: the agent would
            // believe it was acting for one org while acting for
            // another.
            if !header_org.is_empty() && header_org != forced {
                return Err(AuthError(
                    "Unauthorized: scoped agent key cannot act for another org".to_string(),
                ));
            }
            forced
        }
        None => {
            if header_org.is_empty() {
                return Err(AuthError(
                    "Unauthorized: agent key requires X-Agent-Org-Override header".to_string(),
                ));
            }
            header_org
        }
    };

    let ctx = crate::api::sentinel_config::plan_ctx(state);
    let plan = crate::plans::effective_plan_for_caps(&ctx, &org_id, true).await;
    if !crate::api::sentinel_config::plan_has_sentinel(&plan) {
        return Err(AuthError(format!(
            "Agent override target org is not on a Sentinel-eligible plan (plan={})",
            crate::pyrepr::repr_str(&plan)
        )));
    }

    // A self-hosted install is unconditionally eligible by plan, so the
    // licence is checked here too — without it a leaked key or a stale
    // pending run could let the agent act for an unlicensed org even
    // though dispatch itself is blocked.
    let license = crate::api::sentinel_config::license_ctx(state);
    if crate::license::sentinel_blocked_by_license(&license, &plan).await {
        return Err(AuthError(
            "Agent override target org is self-hosted without a valid Sentinel license".to_string(),
        ));
    }

    // Defence in depth: the dispatcher gates on `enabled` before
    // creating a run, but an operator can disable Sentinel between
    // dispatch and the agent picking the run up. The exemption is
    // exactly as wide as the operator action — a manual "Run now" on a
    // paused agent is deliberately allowed, and refusing tools for it
    // turned every such run into a junk error that still spent a
    // monthly-cap slot and the LLM budget.
    let enabled: Option<(bool,)> =
        sqlx::query_as("SELECT enabled FROM sentinel_config WHERE org_id = $1 LIMIT 1")
            .bind(&org_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(|_| AuthError("Authentication error".to_string()))?;
    let Some((enabled,)) = enabled else {
        return Err(AuthError("Sentinel disabled for this org".to_string()));
    };
    if !enabled {
        let manual: Option<(String,)> = sqlx::query_as(
            "SELECT id FROM sentinel_runs
              WHERE org_id = $1 AND trigger_type = 'manual' AND outcome = 'running'
              LIMIT 1",
        )
        .bind(&org_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|_| AuthError("Authentication error".to_string()))?;
        if manual.is_none() {
            return Err(AuthError("Sentinel disabled for this org".to_string()));
        }
    }

    // The org's OWN tier's limits, so a Pro org does not get Pro Plus's
    // budget just because the agent is acting on its behalf.
    let (minute, daily) = scope::rate_limits(&plan).ok_or_else(|| {
        AuthError("Agent override target org is not on a Sentinel-eligible plan".to_string())
    })?;
    // A per-org bucket, distinct from the per-key ones, so agent
    // traffic for one org throttles neither another org nor the
    // dashboard's own MCP usage on the same org.
    let bucket = format!("sentinel-agent:{org_id}");
    let (allowed, _remaining, breach) = RATE_LIMITER.check(&bucket, minute, daily);
    if !allowed {
        return Err(AuthError(match breach {
            Breach::Minute => "Sentinel agent rate limit: too many tool calls in one \
                 minute for this org. Tune the per-camera cooldown or narrow the scope."
                .to_string(),
            _ => "Sentinel agent daily cap reached for this org — check the agent's \
                 run log for a stuck loop."
                .to_string(),
        }));
    }

    Ok(Principal {
        org_id,
        key_name: "<sentinel-agent>".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn the_scheme_is_matched_case_insensitively_and_the_token_trimmed() {
        assert_eq!(
            bearer(&headers(&[("authorization", "Bearer abc")])).unwrap(),
            "abc"
        );
        assert_eq!(
            bearer(&headers(&[("authorization", "bearer abc")])).unwrap(),
            "abc"
        );
        assert_eq!(
            bearer(&headers(&[("authorization", "BEARER   abc  ")])).unwrap(),
            "abc"
        );
    }

    #[test]
    fn a_missing_or_empty_token_says_which() {
        assert_eq!(
            bearer(&HeaderMap::new()).unwrap_err().0,
            "Unauthorized: missing Bearer token"
        );
        assert_eq!(
            bearer(&headers(&[("authorization", "Basic abc")]))
                .unwrap_err()
                .0,
            "Unauthorized: missing Bearer token"
        );
        // The scheme is there but nothing follows it.
        assert_eq!(
            bearer(&headers(&[("authorization", "Bearer ")]))
                .unwrap_err()
                .0,
            "Unauthorized: empty Bearer token"
        );
        assert_eq!(
            bearer(&headers(&[("authorization", "Bearer    ")]))
                .unwrap_err()
                .0,
            "Unauthorized: empty Bearer token"
        );
        // No space at all is not the Bearer scheme.
        assert_eq!(
            bearer(&headers(&[("authorization", "Bearer")]))
                .unwrap_err()
                .0,
            "Unauthorized: missing Bearer token"
        );
    }

    /// A NULL, empty or unparseable column is `[]` — never an error,
    /// and never a reason to refuse a key.
    #[test]
    fn stored_scope_tools_parse_leniently() {
        assert_eq!(parse_scope_tools(None), Vec::<String>::new());
        assert_eq!(parse_scope_tools(Some("")), Vec::<String>::new());
        assert_eq!(parse_scope_tools(Some("not json")), Vec::<String>::new());
        assert_eq!(parse_scope_tools(Some("{}")), Vec::<String>::new());
        assert_eq!(
            parse_scope_tools(Some(r#"["list_cameras","get_camera"]"#)),
            vec!["list_cameras".to_string(), "get_camera".to_string()]
        );
        // `[str(v) for v in val]` — non-strings are stringified, using
        // Python's str(), so a bool is "True".
        assert_eq!(
            parse_scope_tools(Some(r#"[1, true, null, "x"]"#)),
            vec![
                "1".to_string(),
                "True".to_string(),
                "None".to_string(),
                "x".to_string()
            ]
        );
    }

    /// Constant time, and an unset key rejects everything rather than
    /// matching an empty token.
    #[test]
    fn the_shared_key_comparison_rejects_when_unset() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"anything"));
        // Two empties DO compare equal, which is why the caller checks
        // for an unset key separately rather than relying on this.
        assert!(constant_time_eq(b"", b""));
    }
}
