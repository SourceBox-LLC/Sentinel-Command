//! Hold the agent's `/complete` body to what Command Center reads.
//!
//! **The failure this guards against is silent, which is the whole
//! point.** The agent builds the body in `agent::queue::complete_body`;
//! the handler reads named fields out of a JSON object and ignores the
//! rest. If one side renamed `tool_call_count`, the agent would keep
//! sending the old key, Command Center would drop it and default the
//! column to 0, and nothing would surface — no 422, no error, no log
//! line. Every run would record zero tool calls.
//!
//! This is the third form of this check. It began as a Python test that
//! imported Command Center's Pydantic model; when the web tier became
//! Rust it became `agent_contract.py`, which parsed BOTH sides as text
//! because no process could hold both. Now both are one crate, so the
//! agent's side is no longer parsed at all — the function is called and
//! its real output inspected. Only the handler is still read as source,
//! because what it reads is a property of its code, not of any value.

use std::collections::BTreeSet;

use sentinel_command::agent::queue::{complete_body, Completion};
use serde_json::json;

const HANDLER: &str = include_str!("../src/api/sentinel.rs");

/// Every field `post_run_complete` reads out of the body.
fn handler_keys() -> BTreeSet<String> {
    let start = HANDLER
        .find("pub async fn post_run_complete")
        .expect("post_run_complete has moved; this test reads its source");
    let rest = &HANDLER[start..];
    // Stop at the next handler so a later one's reads are not counted.
    let block = match rest[10..].find("\npub async fn ") {
        Some(end) => &rest[..end + 10],
        None => rest,
    };
    block
        .split("(&body, \"")
        .skip(1)
        .filter_map(|after| after.split('"').next())
        .map(str::to_string)
        .collect()
}

#[test]
fn every_key_the_agent_sends_is_read() {
    // Every optional field present, so the body is as wide as it gets.
    let body = complete_body(&Completion {
        outcome: "incident".into(),
        summary: "s".into(),
        tool_call_count: 3,
        tool_trace: vec![json!({"tool": "list_cameras"})],
        severity: Some("high".into()),
        incident_id: Some(42),
    });
    let sent: BTreeSet<String> = body.as_object().unwrap().keys().cloned().collect();
    let read = handler_keys();

    // The parse is the fragile half: if the handler's reads change shape
    // this must fail loudly rather than compare against an empty set and
    // report every key ignored — or, worse, be "fixed" by deleting it.
    assert!(
        read.contains("outcome") && read.len() >= 4,
        "parsed {read:?} out of post_run_complete — the pattern has rotted"
    );
    assert_eq!(sent.len(), 6, "the agent's body changed width: {sent:?}");

    let ignored: Vec<_> = sent.difference(&read).collect();
    assert!(
        ignored.is_empty(),
        "the agent sends {ignored:?} and Command Center does not read it. \
         Unknown fields are dropped without complaint, so this would surface \
         as a column silently holding its default. Rename on both sides."
    );
}
