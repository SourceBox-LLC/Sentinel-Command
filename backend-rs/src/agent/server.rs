//! The agent's HTTP surface and its two ways of finding work.
//!
//! Ported from `app/sentinel_agent/main.py`.
//!
//! * `GET  /health` — liveness. Bare on purpose: an earlier body named
//!   the model, which secrets were set and whether HMAC was on, which is
//!   reconnaissance handed to anyone who asks.
//! * `POST /wakeup` — Command Center's webhook. Drains the queue.
//! * `POST /`       — a dev trigger, alive only with verification off.
//!
//! **A drain must outlive the request that started it**, and in axum that
//! has to be arranged. Command Center's wakeup client hangs up after
//! about five seconds; a drain takes minutes. Under uvicorn the handler
//! simply kept running after the client left. Under hyper a dropped
//! connection drops the handler's future — which would cancel every
//! drain five seconds in, mid-run, on every wakeup. So the drain is
//! spawned as its own task and the handler only waits on it; the client
//! leaving ends the wait and nothing else. Shutdown then waits for the
//! drain lock, so a signal arriving mid-drain still lets it finish.
//!
//! One drain at a time, enforced by that lock. Overlapping wakeups —
//! Command Center re-fires for stale runs, and motion comes in bursts —
//! otherwise ran concurrent drains over the same list: double the model
//! spend and, before `/start` reported `claimed`, duplicate incidents.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use subtle::ConstantTimeEq;

use crate::agent::config::{AgentConfig, AgentMode};
use crate::agent::llm::LlmProvider;
use crate::agent::mcp_client::McpClient;
use crate::agent::processor::{
    process_with_timeout, DrainOptions, ToolConnector, DRAIN_TIMEOUT_SECONDS,
};
use crate::agent::queue::SentinelClient;

/// Replay window for a signed wakeup, either side of now.
const MAX_SKEW_SECONDS: f64 = 300.0;

#[derive(Clone)]
pub struct AgentState {
    pub config: Arc<AgentConfig>,
    pub llm: Arc<LlmProvider>,
    drain_lock: Arc<tokio::sync::Mutex<()>>,
}

impl AgentState {
    pub fn new(config: AgentConfig) -> Result<Self, String> {
        let llm = LlmProvider::new(&config)?;
        Ok(Self {
            config: Arc::new(config),
            llm: Arc::new(llm),
            drain_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }
}

/// Opens Command Center's MCP surface for one org.
struct McpConnector {
    config: Arc<AgentConfig>,
}

impl ToolConnector for McpConnector {
    type Tools = McpClient;
    async fn connect(&self, org_id: &str) -> Result<McpClient, String> {
        McpClient::connect(
            &self.config.mcp_url(),
            &self.config.opensentry_mcp_agent_key,
            org_id,
            self.config.mcp_tool_timeout_seconds,
        )
        .await
        .map_err(|err| format!("Failed to connect to MCP servers: {err}"))
    }
    async fn disconnect(&self, tools: McpClient) {
        tools.disconnect().await;
    }
}

/// Constant-time HMAC-SHA256 check of `sha256=<hex>` over the raw body.
/// False on any malformation; the caller makes that a 401.
pub fn verify_signature(raw_body: &[u8], signature_header: Option<&str>, secret: &str) -> bool {
    let Some(header) = signature_header else {
        return false;
    };
    if secret.is_empty() {
        return false;
    }
    let Some(given) = header.strip_prefix("sha256=") else {
        return false;
    };
    let expected = crate::crypto::hex(&crate::crypto::hmac_sha256(secret.as_bytes(), raw_body));
    given.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// Whether a signed body's timestamp is inside the replay window.
///
/// `Ok` for a body with no `ts` at all: an older Command Center sends a
/// static `{}`, and accepting it — the signature has already passed —
/// lets the two services deploy in either order. A `ts` that is present
/// and unusable is a rejection, not a pass.
pub fn timestamp_is_fresh(raw_body: &[u8], now: f64) -> bool {
    let body: Value = if raw_body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(raw_body) {
            Ok(body) => body,
            // Unparseable: Python treats it as "no ts" and accepts.
            Err(_) => return true,
        }
    };
    let ts = match body.get("ts") {
        None | Some(Value::Null) => return true,
        Some(Value::Number(n)) => n.as_f64(),
        // `float("1700000000")` succeeds in Python, so a numeric string
        // is a timestamp there too.
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        Some(_) => None,
    };
    match ts {
        Some(ts) => (now - ts).abs() <= MAX_SKEW_SECONDS,
        None => false,
    }
}

/// Drain the queue once. `None` if a drain is already running.
///
/// The single implementation behind the webhook and the poll loop, so
/// the concurrency guard exists in one place.
async fn drain_once(state: &AgentState, reason: &'static str) -> Option<Value> {
    // A drain already running lists fresh from Command Center and will
    // pick up whatever this wakeup was about. Acknowledging is correct;
    // starting a second one is not.
    let Ok(guard) = state.drain_lock.clone().try_lock_owned() else {
        tracing::info!(reason, "drain already in progress — acknowledged");
        return None;
    };
    let state = state.clone();
    // Spawned, so the caller going away cannot cancel it. See the module
    // note: this line is the difference between a drain and five seconds
    // of one.
    let task = tokio::spawn(async move {
        let _guard = guard;
        let queue = SentinelClient::new(
            &state.config.opensentry_api_base,
            &state.config.sentinel_agent_key,
        );
        let connector = McpConnector {
            config: state.config.clone(),
        };
        let summary = process_with_timeout(
            &queue,
            state.llm.as_ref(),
            &connector,
            DrainOptions::new(state.config.max_agent_iterations),
            DRAIN_TIMEOUT_SECONDS,
        )
        .await;
        tracing::info!(reason, summary = %summary, "drained");
        summary
    });
    match task.await {
        Ok(summary) => Some(summary),
        Err(err) => {
            tracing::error!(error = %err, "drain task failed");
            Some(json!({ "errored": 1, "fetched_error": "drain task panicked" }))
        }
    }
}

fn reply(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn wakeup(State(state): State<AgentState>, headers: HeaderMap, body: Bytes) -> Response {
    let config = &state.config;
    if config.webhook_verify_signature {
        let signature = headers
            .get("x-sentinel-signature")
            .and_then(|v| v.to_str().ok());
        if !verify_signature(&body, signature, &config.sentinel_agent_key) {
            tracing::warn!("wakeup: signature verification failed");
            return reply(
                StatusCode::UNAUTHORIZED,
                json!({ "error": "invalid signature" }),
            );
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        if !timestamp_is_fresh(&body, now) {
            tracing::warn!("wakeup: stale or invalid timestamp — possible replay");
            return reply(
                StatusCode::UNAUTHORIZED,
                json!({ "error": "stale timestamp" }),
            );
        }
    }
    if config.sentinel_agent_key.is_empty() {
        return reply(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": "agent key not configured" }),
        );
    }
    if config.opensentry_mcp_agent_key.is_empty() {
        return reply(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": "MCP agent key not configured" }),
        );
    }
    match drain_once(&state, "wakeup").await {
        None => reply(
            StatusCode::OK,
            json!({ "ok": true, "already_draining": true }),
        ),
        Some(summary) => reply(StatusCode::OK, json!({ "ok": true, "summary": summary })),
    }
}

/// DEV ONLY: a drain with no signature. Dead whenever verification is on,
/// and the config refuses to boot with a production key and verification
/// off, so this cannot be opened on a real deployment by one flag.
async fn dev_trigger(State(state): State<AgentState>) -> Response {
    if state.config.webhook_verify_signature {
        return reply(
            StatusCode::FORBIDDEN,
            json!({ "error": "dev trigger disabled — verify signature is on" }),
        );
    }
    if state.config.opensentry_mcp_agent_key.is_empty() {
        return reply(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": "MCP agent key not configured" }),
        );
    }
    match drain_once(&state, "dev").await {
        None => reply(
            StatusCode::OK,
            json!({ "ok": true, "already_draining": true }),
        ),
        Some(summary) => reply(
            StatusCode::OK,
            json!({ "ok": true, "summary": summary, "mode": "dev" }),
        ),
    }
}

pub fn router(state: AgentState) -> Router {
    Router::new()
        .route("/health", get(health))
        // Mounted in poll mode too: still a valid way to force a drain.
        .route("/wakeup", post(wakeup))
        .route("/", post(dev_trigger))
        .with_state(state)
}

/// Ask for work on an interval, for an agent Command Center cannot reach.
///
/// It has to outlive every individual failure — an unreachable Command
/// Center, a bad gateway, a DNS blip. A drain cannot raise here, so there
/// is nothing to guard; what remains is that the loop sleeps FIRST, as
/// the Python did, and stops promptly when told to.
async fn poll_loop(state: AgentState, mut stop: tokio::sync::watch::Receiver<bool>) {
    let interval = Duration::from_secs_f64(state.config.poll_interval_seconds);
    tracing::info!(
        every_seconds = state.config.poll_interval_seconds,
        base = %state.config.opensentry_api_base,
        "poll: starting"
    );
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = stop.changed() => break,
        }
        drain_once(&state, "poll").await;
        if *stop.borrow() {
            break;
        }
    }
    tracing::info!("poll: stopping");
}

/// Serve until signalled, then let a running drain finish.
pub async fn serve(state: AgentState) -> anyhow::Result<()> {
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let poller = match state.config.agent_mode {
        AgentMode::Poll => Some(tokio::spawn(poll_loop(state.clone(), stop_rx))),
        AgentMode::Push => {
            tracing::info!(base = %state.config.opensentry_api_base, "push mode — waiting for POST /wakeup");
            None
        }
    };

    let address = format!("{}:{}", state.config.host, state.config.port);
    let listener = tokio::net::TcpListener::bind(&address).await?;
    tracing::info!(%address, "sentinel agent listening");
    let lock = state.drain_lock.clone();

    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let ctrl_c = tokio::signal::ctrl_c();
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("a SIGTERM handler can always be installed");
            tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
        })
        .await?;

    let _ = stop_tx.send(true);
    if let Some(poller) = poller {
        let _ = poller.await;
    }
    // A drain in flight holds this. Waiting for it is what uvicorn's
    // graceful shutdown did by keeping the handler alive; the bound is
    // the drain's own wall clock plus its cleanup, under Fly's 300 s.
    if tokio::time::timeout(Duration::from_secs(285), lock.lock())
        .await
        .is_err()
    {
        tracing::warn!("shutdown: a drain was still running at the deadline");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, body: &[u8]) -> String {
        format!(
            "sha256={}",
            crate::crypto::hex(&crate::crypto::hmac_sha256(secret.as_bytes(), body))
        )
    }

    #[test]
    fn a_signature_must_be_present_prefixed_and_correct() {
        let body = br#"{"ts": 1700000000}"#;
        let good = sign("secret", body);
        assert!(verify_signature(body, Some(&good), "secret"));
        assert!(!verify_signature(body, None, "secret"));
        assert!(
            !verify_signature(body, Some(&good), ""),
            "no secret configured"
        );
        assert!(!verify_signature(body, Some(&good), "other"));
        assert!(
            !verify_signature(b"{}", Some(&good), "secret"),
            "a different body"
        );
        assert!(!verify_signature(
            body,
            Some(good.trim_start_matches("sha256=")),
            "secret"
        ));
        assert!(!verify_signature(body, Some("sha256="), "secret"));
    }

    /// The value is `hmac.new(b"k", b'{"ts": 1700000000}', sha256).hexdigest()`,
    /// computed by Python — so this pins the scheme Command Center signs
    /// with, not merely that this function agrees with itself.
    #[test]
    fn the_signature_is_pythons_hmac() {
        assert_eq!(
            sign("k", br#"{"ts": 1700000000}"#),
            "sha256=86f23a1e3b346b552c69ea8ac1c74c9985498fc5a036cfdb67cc604440f28332"
        );
    }

    #[test]
    fn a_timestamp_is_checked_only_when_present() {
        let now = 1_700_000_000.0;
        assert!(timestamp_is_fresh(b"", now));
        assert!(timestamp_is_fresh(b"{}", now), "the legacy static body");
        assert!(timestamp_is_fresh(br#"{"ts": null}"#, now));
        assert!(timestamp_is_fresh(b"not json", now));
        assert!(timestamp_is_fresh(br#"{"ts": 1700000000}"#, now));
        assert!(
            timestamp_is_fresh(br#"{"ts": 1699999701}"#, now),
            "299s old"
        );
        assert!(
            timestamp_is_fresh(br#"{"ts": 1700000300}"#, now),
            "exactly at the edge"
        );
        assert!(
            !timestamp_is_fresh(br#"{"ts": 1699999699}"#, now),
            "301s old"
        );
        assert!(
            !timestamp_is_fresh(br#"{"ts": 1700000301}"#, now),
            "301s in the future"
        );
        assert!(
            timestamp_is_fresh(br#"{"ts": "1700000000"}"#, now),
            "float() takes a string"
        );
        assert!(!timestamp_is_fresh(br#"{"ts": "soon"}"#, now));
        assert!(!timestamp_is_fresh(br#"{"ts": [1]}"#, now));
    }
}
