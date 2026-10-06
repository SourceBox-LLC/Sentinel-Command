//! The agent's client for Command Center's run queue.
//!
//! Ported from `app/sentinel_agent/sentinel_client.py`. Three endpoints
//! under `/api/sentinel/runs`, authenticated by `X-Sentinel-Agent-Key`:
//!
//! * `GET  /pending`        — the queue, across every org, oldest first
//! * `POST /{id}/start`     — claim a run
//! * `POST /{id}/complete`  — the terminal callback
//!
//! One `reqwest::Client` for the whole drain, for the reason the Python
//! held one httpx client: N runs is 1 + 2N requests to one host with one
//! credential, and a TLS handshake per call is real money on a machine
//! billed by the second.
//!
//! The `/complete` body is the contract `tests/agent_contract.rs` holds against
//! the handler. Command Center ignores unknown fields, so a key renamed
//! on one side alone records its column's default forever with no 422 and
//! no log line. The keys are built in [`complete_body`] and nowhere else.

use std::time::Duration;

use serde_json::{json, Map, Value};

/// What went wrong talking to Command Center.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("{0}")]
    Transport(#[from] reqwest::Error),
    #[error("Command Center answered {status} for {what}")]
    Status { status: u16, what: String },
}

/// The terminal result of a run, as posted to `/complete`.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    /// `incident` | `no_action` | `error`.
    pub outcome: String,
    pub summary: String,
    pub tool_call_count: usize,
    pub tool_trace: Vec<Value>,
    /// Sent only when present; required by Command Center for `incident`.
    pub severity: Option<String>,
    pub incident_id: Option<i64>,
}

impl Completion {
    /// A bare error outcome, for the paths where the run never produced
    /// a result of its own.
    pub fn error(summary: impl Into<String>) -> Self {
        Self {
            outcome: "error".to_string(),
            summary: summary.into(),
            tool_call_count: 0,
            tool_trace: Vec::new(),
            severity: None,
            incident_id: None,
        }
    }
}

/// The `/complete` body. `severity` and `incident_id` are omitted rather
/// than sent as null, and the summary is capped at 8,000 characters —
/// code points, as Python's slice counts them, not bytes.
pub fn complete_body(completion: &Completion) -> Value {
    let mut body = Map::new();
    body.insert("outcome".into(), json!(completion.outcome));
    body.insert(
        "summary".into(),
        json!(completion.summary.chars().take(8000).collect::<String>()),
    );
    body.insert("tool_call_count".into(), json!(completion.tool_call_count));
    body.insert(
        "tool_trace".into(),
        Value::Array(completion.tool_trace.clone()),
    );
    if let Some(severity) = &completion.severity {
        body.insert("severity".into(), json!(severity));
    }
    if let Some(incident_id) = completion.incident_id {
        body.insert("incident_id".into(), json!(incident_id));
    }
    Value::Object(body)
}

/// The surface the processor needs, as a trait so the drain logic can be
/// driven by a scripted queue in tests — every interesting behaviour in
/// it is about what Command Center answers.
pub trait RunQueue: Send + Sync {
    fn list_pending(
        &self,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<Value>, QueueError>> + Send;
    /// `None` for a 404: the row is gone, or was never ours.
    fn start(
        &self,
        run_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<Value>, QueueError>> + Send;
    fn complete(
        &self,
        run_id: &str,
        completion: &Completion,
    ) -> impl std::future::Future<Output = Result<Option<Value>, QueueError>> + Send;
}

pub struct SentinelClient {
    base_url: String,
    agent_key: String,
    client: reqwest::Client,
}

impl SentinelClient {
    pub fn new(base_url: &str, agent_key: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            agent_key: agent_key.to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("a reqwest client with only a timeout always builds"),
        }
    }

    fn url(&self, tail: &str) -> String {
        format!("{}/api/sentinel/runs/{tail}", self.base_url)
    }

    /// `raise_for_status`, after the 404 the two POSTs treat as "gone".
    async fn checked(
        response: reqwest::Response,
        what: &str,
        gone_is_none: bool,
    ) -> Result<Option<Value>, QueueError> {
        let status = response.status();
        if gone_is_none && status == reqwest::StatusCode::NOT_FOUND {
            tracing::warn!(what, "sentinel_client: run not found");
            return Ok(None);
        }
        if status.is_client_error() || status.is_server_error() {
            return Err(QueueError::Status {
                status: status.as_u16(),
                what: what.to_string(),
            });
        }
        Ok(Some(response.json().await?))
    }
}

impl RunQueue for SentinelClient {
    async fn list_pending(&self, limit: usize) -> Result<Vec<Value>, QueueError> {
        let response = self
            .client
            .get(self.url("pending"))
            .header("X-Sentinel-Agent-Key", &self.agent_key)
            .header("Content-Type", "application/json")
            .query(&[("limit", limit)])
            .send()
            .await?;
        let data = Self::checked(response, "GET /pending", false)
            .await?
            .unwrap_or(Value::Null);
        Ok(data
            .get("runs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    async fn start(&self, run_id: &str) -> Result<Option<Value>, QueueError> {
        let response = self
            .client
            .post(self.url(&format!("{run_id}/start")))
            .header("X-Sentinel-Agent-Key", &self.agent_key)
            .header("Content-Type", "application/json")
            .send()
            .await?;
        Self::checked(response, "POST /start", true).await
    }

    async fn complete(
        &self,
        run_id: &str,
        completion: &Completion,
    ) -> Result<Option<Value>, QueueError> {
        let response = self
            .client
            .post(self.url(&format!("{run_id}/complete")))
            .header("X-Sentinel-Agent-Key", &self.agent_key)
            .json(&complete_body(completion))
            .send()
            .await?;
        Self::checked(response, "POST /complete", true).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The optional pair is absent, not null — Command Center reads a
    /// present-but-null `severity` differently from a missing one.
    #[test]
    fn optional_fields_are_omitted_rather_than_null() {
        let body = complete_body(&Completion::error("boom"));
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["outcome", "summary", "tool_call_count", "tool_trace"]
        );

        let body = complete_body(&Completion {
            outcome: "incident".into(),
            summary: "s".into(),
            tool_call_count: 3,
            tool_trace: vec![json!({"tool": "view_camera"})],
            severity: Some("high".into()),
            incident_id: Some(42),
        });
        assert_eq!(body["severity"], "high");
        assert_eq!(body["incident_id"], 42);
        assert_eq!(body["tool_call_count"], 3);
    }

    /// 8,000 code points, not bytes: a summary of multi-byte characters
    /// must not be cut mid-character or cut short.
    #[test]
    fn the_summary_is_capped_in_characters() {
        let body = complete_body(&Completion::error("é".repeat(9000)));
        let summary = body["summary"].as_str().unwrap();
        assert_eq!(summary.chars().count(), 8000);
        assert_eq!(summary.len(), 16000);
    }
}
