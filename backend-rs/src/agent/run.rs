//! The agent loop: one LLM ↔ tool conversation for one run.
//!
//! Ported from `app/sentinel_agent/agent.py`. Stateless across runs, on
//! purpose: the run record on Command Center is the source of truth, and
//! this reads the trigger, investigates, and returns one complete result.
//!
//! The decisions in here that were each paid for:
//!
//! * **The incident id comes from a tool's return value, never from the
//!   model's prose.** A free-text fallback once let "…incident #7…" — a
//!   camera literally named that, on-screen text, or a hallucination —
//!   turn a clean `no_action` into an `incident` pointing at someone
//!   else's row. Only a JSON object with an integer `id`, returned by
//!   `create_incident` or `finalize_incident`, counts.
//! * **A run cut short still reports the incident it filed.** Timeout,
//!   model failure or budget exhaustion after a filing is `incident`
//!   with that id, not `error` — a blanket error dropped the id and
//!   orphaned a real incident from its run.
//! * **Frames are pruned from earlier iterations only.** Every base64
//!   frame is otherwise re-sent on every later call — quadratic, and
//!   enough to blow the context window mid-run. But the CURRENT batch
//!   stays whole: a sweep that views six cameras in one turn appends six
//!   image messages, and pruning to "newest only" would have the model
//!   assessing cameras whose frames it never saw.

use serde_json::{json, Map, Value};

use crate::agent::llm::{
    assistant_message, has_images, image_message, prune_images, system_message,
    tool_call_arguments, tool_result_message, user_message, Llm, LlmError,
};
use crate::agent::mcp_client::Tools;
use crate::agent::prompts::{initial_user_message, system_prompt_for_trigger};
use crate::agent::queue::Completion;

/// The write tools whose return value carries the incident's own id.
const INCIDENT_ID_BEARING_TOOLS: [&str; 2] = ["create_incident", "finalize_incident"];
const SEVERITIES: [&str; 4] = ["low", "medium", "high", "critical"];

/// Process one run to a terminal result.
pub async fn run_agent(
    llm: &impl Llm,
    tools: &impl Tools,
    run: &Value,
    max_iterations: usize,
) -> Completion {
    let trigger_type = run
        .get("trigger_type")
        .and_then(Value::as_str)
        .unwrap_or("manual");
    let mut messages = vec![
        system_message(system_prompt_for_trigger(trigger_type)),
        user_message(initial_user_message(run)),
    ];
    let definitions = tools.definitions();
    let mut tool_trace: Vec<Value> = Vec::new();
    let mut incident_id: Option<i64> = None;
    let mut severity: Option<String> = None;

    for iteration in 0..max_iterations {
        let turn = match llm.chat(&messages, &definitions).await {
            Ok(turn) => turn,
            Err(LlmError::Timeout) => {
                tracing::warn!(iteration, "agent: LLM call timed out");
                return truncated_result(
                    incident_id,
                    severity,
                    format!(
                        "LLM call timed out after {:.0}s at iteration {iteration}. \
                         Investigation incomplete.",
                        llm.timeout_seconds()
                    ),
                    tool_trace,
                );
            }
            Err(LlmError::Failed(reason)) => {
                tracing::error!(iteration, error = %reason, "agent: LLM call failed");
                return truncated_result(
                    incident_id,
                    severity,
                    format!("LLM call failed: {reason}"),
                    tool_trace,
                );
            }
        };

        let calls: Vec<_> = turn.tool_calls().into_iter().cloned().collect();

        // Terminal: the model answered without calling a tool.
        if calls.is_empty() {
            let summary = turn.text().trim().to_string();
            let filed = incident_id.is_some();
            return Completion {
                outcome: if filed { "incident" } else { "no_action" }.to_string(),
                severity: filed.then(|| severity.unwrap_or_else(|| "low".to_string())),
                incident_id,
                summary: if summary.is_empty() {
                    "(agent returned no summary)".to_string()
                } else {
                    summary
                },
                tool_call_count: tool_trace.len(),
                tool_trace,
            };
        }

        messages.push(assistant_message(&turn));

        // Everything appended from here on is THIS iteration's batch,
        // which the pruning pass below must not touch — the model has
        // not seen any of it yet.
        let batch_start = messages.len();
        let mut frames = Vec::new();
        for call in &calls {
            let name = call.function.name.as_str();
            let args = tool_call_arguments(call);
            tracing::info!(tool = name, "agent: tool call");

            let result = tools.call(name, args.clone()).await;

            // Short in the trace, full to the model: the trace has to fit
            // Command Center's row budget, the model needs the real thing.
            tool_trace.push(json!({
                "tool": name,
                "args": sanitize_args(&args),
                "result": result.text.chars().take(800).collect::<String>(),
            }));

            if matches!(name, "create_incident" | "update_incident") {
                if let Some(Value::String(chosen)) = args.get("severity") {
                    if SEVERITIES.contains(&chosen.as_str()) {
                        severity = Some(chosen.clone());
                    }
                }
            }

            // Most recent write wins; an error result carries no id and
            // leaves the last good one in place.
            if INCIDENT_ID_BEARING_TOOLS.contains(&name) {
                if let Some(parsed) = parse_id_from_tool_json(&result.text) {
                    incident_id = Some(parsed);
                }
            }

            messages.push(tool_result_message(call, &result.text));
            if !result.images.is_empty() {
                frames.push(image_message(name, &result.images));
            }
        }
        // Frames go AFTER the whole batch's tool results, not after the
        // call that produced them. The Python interleaved them, which put
        // a user message between two tool results of one assistant turn —
        // a sequence the OpenAI and Anthropic APIs both reject, and one
        // LiteLLM only repaired on the Anthropic wire. PYTHON_BUGS #19.
        messages.append(&mut frames);

        for stale in &mut messages[..batch_start] {
            if has_images(stale) {
                prune_images(stale);
            }
        }
    }

    truncated_result(
        incident_id,
        severity,
        format!(
            "Reached max iterations ({max_iterations}) without a terminal answer. \
             Investigation may be incomplete."
        ),
        tool_trace,
    )
}

/// The result for a run cut short.
fn truncated_result(
    incident_id: Option<i64>,
    severity: Option<String>,
    summary: String,
    tool_trace: Vec<Value>,
) -> Completion {
    let tool_call_count = tool_trace.len();
    match incident_id {
        Some(id) => Completion {
            outcome: "incident".to_string(),
            severity: Some(severity.unwrap_or_else(|| "low".to_string())),
            incident_id: Some(id),
            summary: format!("{summary} An incident was filed before the cutoff."),
            tool_call_count,
            tool_trace,
        },
        None => Completion {
            outcome: "error".to_string(),
            severity: None,
            incident_id: None,
            summary,
            tool_call_count,
            tool_trace,
        },
    }
}

/// The `id` from a JSON tool return, if it is one.
///
/// Refuses anything that is not an object with an integer `id`: an error
/// envelope, a list, a string id, and — because Python's `bool` is an
/// `int` and the original had to say so — a boolean. `1.0` is a float in
/// Python and is refused there, so it is refused here.
pub fn parse_id_from_tool_json(text: &str) -> Option<i64> {
    if text.is_empty() {
        return None;
    }
    let payload: Value = serde_json::from_str(text).ok()?;
    let object = payload.as_object()?;
    match object.get("id") {
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => n.as_i64(),
        _ => None,
    }
}

/// Arguments small enough for the trace: long strings are cut, and a
/// nested value whose JSON is long is replaced outright.
///
/// The 200 is measured the way the Python measured it — characters for a
/// string, and the length of `json.dumps(value)` for a container, which
/// is ASCII-escaped, so a short list of non-ASCII text can exceed it.
pub fn sanitize_args(args: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for (key, value) in args {
        let kept = match value {
            Value::String(s) if s.chars().count() > 200 => {
                json!(format!("{}…", s.chars().take(200).collect::<String>()))
            }
            Value::Object(_) | Value::Array(_)
                if crate::audit::python_json_value(value).len() > 200 =>
            {
                json!("<truncated>")
            }
            other => other.clone(),
        };
        out.insert(key.clone(), kept);
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::llm::AssistantTurn;
    use crate::agent::mcp_client::ToolOutput;
    use rig_core::completion::ToolDefinition;
    use rig_core::message::{AssistantContent, Message, ToolName, UserContent};
    use std::sync::Mutex;

    /// A model that plays back a script, recording what it was sent.
    struct Script {
        turns: Mutex<Vec<Result<AssistantTurn, LlmError>>>,
        seen: Mutex<Vec<Vec<Message>>>,
    }

    impl Script {
        fn new(turns: Vec<Result<AssistantTurn, LlmError>>) -> Self {
            Self {
                turns: Mutex::new(turns),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Llm for Script {
        async fn chat(
            &self,
            messages: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<AssistantTurn, LlmError> {
            self.seen.lock().unwrap().push(messages.to_vec());
            let mut turns = self.turns.lock().unwrap();
            if turns.is_empty() {
                // Keep calling the same tool forever: the budget test.
                return Ok(calls(&[("list_cameras", json!({}))]));
            }
            turns.remove(0)
        }
        fn timeout_seconds(&self) -> f64 {
            120.0
        }
    }

    /// Tools that answer from a table.
    struct Table(Vec<(&'static str, ToolOutput)>);

    impl Tools for Table {
        fn definitions(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }
        async fn call(&self, name: &str, _arguments: Map<String, Value>) -> ToolOutput {
            self.0
                .iter()
                .find(|(tool, _)| *tool == name)
                .map(|(_, out)| out.clone())
                .unwrap_or_else(|| ToolOutput::error(format!("Unknown tool: {name}")))
        }
    }

    fn text(s: &str) -> Result<AssistantTurn, LlmError> {
        Ok(AssistantTurn {
            content: vec![AssistantContent::text(s)],
        })
    }

    fn calls(list: &[(&str, Value)]) -> AssistantTurn {
        AssistantTurn {
            content: list
                .iter()
                .enumerate()
                .map(|(i, (name, args))| {
                    AssistantContent::tool_call(
                        format!("call_{i}"),
                        ToolName::new(*name).unwrap(),
                        args.clone(),
                    )
                })
                .collect(),
        }
    }

    fn out(text: &str) -> ToolOutput {
        ToolOutput {
            text: text.to_string(),
            images: Vec::new(),
        }
    }

    fn frames(text: &str, n: usize) -> ToolOutput {
        ToolOutput {
            text: text.to_string(),
            images: vec!["QUJD".to_string(); n],
        }
    }

    fn run() -> Value {
        json!({"id": "r1", "trigger_type": "motion", "camera_id": "cam-1"})
    }

    #[tokio::test]
    async fn an_answer_with_no_tool_call_is_no_action() {
        let llm = Script::new(vec![text("  A cat. Nothing to log.  ")]);
        let result = run_agent(&llm, &Table(vec![]), &run(), 10).await;
        assert_eq!(result.outcome, "no_action");
        assert_eq!(result.summary, "A cat. Nothing to log.");
        assert_eq!(result.severity, None);
        assert_eq!(result.incident_id, None);
        assert_eq!(result.tool_call_count, 0);
    }

    #[tokio::test]
    async fn an_empty_answer_still_has_a_summary() {
        let llm = Script::new(vec![text("   ")]);
        let result = run_agent(&llm, &Table(vec![]), &run(), 10).await;
        assert_eq!(result.summary, "(agent returned no summary)");
    }

    /// The id comes from the tool's JSON, the severity from the call's
    /// arguments, and a later `update_incident` moves the severity.
    #[tokio::test]
    async fn a_filed_incident_is_reported_with_its_id_and_last_severity() {
        let llm = Script::new(vec![
            Ok(calls(&[(
                "create_incident",
                json!({"title": "t", "severity": "medium"}),
            )])),
            Ok(calls(&[(
                "update_incident",
                json!({"incident_id": 42, "severity": "high"}),
            )])),
            text("Filed."),
        ]);
        let tools = Table(vec![
            ("create_incident", out(r#"{"id":42,"title":"t"}"#)),
            ("update_incident", out(r#"{"id":42}"#)),
        ]);
        let result = run_agent(&llm, &tools, &run(), 10).await;
        assert_eq!(result.outcome, "incident");
        assert_eq!(result.incident_id, Some(42));
        assert_eq!(result.severity.as_deref(), Some("high"));
        assert_eq!(result.tool_call_count, 2);
        assert_eq!(result.tool_trace[0]["tool"], "create_incident");
    }

    /// The rule that replaced the regex: prose naming an incident is not
    /// an incident.
    #[tokio::test]
    async fn an_incident_named_only_in_prose_is_not_an_incident() {
        let llm = Script::new(vec![text("I looked at incident #7 and it seems fine.")]);
        let result = run_agent(&llm, &Table(vec![]), &run(), 10).await;
        assert_eq!(result.outcome, "no_action");
        assert_eq!(result.incident_id, None);
    }

    #[tokio::test]
    async fn a_failed_create_does_not_count_and_an_invalid_severity_is_ignored() {
        let llm = Script::new(vec![
            Ok(calls(&[(
                "create_incident",
                json!({"severity": "catastrophic"}),
            )])),
            text("Could not file."),
        ]);
        let tools = Table(vec![(
            "create_incident",
            out(r#"{"error": "plan cap reached"}"#),
        )]);
        let result = run_agent(&llm, &tools, &run(), 10).await;
        assert_eq!(result.outcome, "no_action");
        assert_eq!(result.severity, None);
    }

    /// Cut short AFTER a filing: the incident is not orphaned.
    #[tokio::test]
    async fn a_run_cut_short_after_filing_still_reports_the_incident() {
        let llm = Script::new(vec![
            Ok(calls(&[("create_incident", json!({"severity": "high"}))])),
            Err(LlmError::Timeout),
        ]);
        let tools = Table(vec![("create_incident", out(r#"{"id": 9}"#))]);
        let result = run_agent(&llm, &tools, &run(), 10).await;
        assert_eq!(result.outcome, "incident");
        assert_eq!(result.incident_id, Some(9));
        assert_eq!(result.severity.as_deref(), Some("high"));
        assert_eq!(
            result.summary,
            "LLM call timed out after 120s at iteration 1. Investigation incomplete. \
             An incident was filed before the cutoff."
        );
    }

    #[tokio::test]
    async fn a_model_failure_before_any_filing_is_an_error() {
        let llm = Script::new(vec![Err(LlmError::Failed("401 unauthorized".into()))]);
        let result = run_agent(&llm, &Table(vec![]), &run(), 10).await;
        assert_eq!(result.outcome, "error");
        assert_eq!(result.summary, "LLM call failed: 401 unauthorized");
        assert_eq!(result.incident_id, None);
    }

    #[tokio::test]
    async fn the_iteration_budget_is_a_hard_stop() {
        let llm = Script::new(vec![]);
        let tools = Table(vec![("list_cameras", out("[]"))]);
        let result = run_agent(&llm, &tools, &run(), 3).await;
        assert_eq!(result.outcome, "error");
        assert_eq!(result.tool_call_count, 3);
        assert_eq!(
            result.summary,
            "Reached max iterations (3) without a terminal answer. Investigation may be incomplete."
        );
        assert_eq!(llm.seen.lock().unwrap().len(), 3);
    }

    /// The pruning rule, both halves: the batch the model has not seen
    /// keeps every frame, and everything older loses them.
    #[tokio::test]
    async fn frames_are_pruned_from_earlier_iterations_only() {
        let llm = Script::new(vec![
            // A sweep: two cameras viewed in ONE turn.
            Ok(calls(&[
                ("view_camera", json!({"camera_id": "a"})),
                ("view_camera", json!({"camera_id": "b"})),
            ])),
            Ok(calls(&[("view_camera", json!({"camera_id": "c"}))])),
            text("All clear."),
        ]);
        let tools = Table(vec![("view_camera", frames("frame", 1))]);
        run_agent(&llm, &tools, &run(), 10).await;

        let seen = llm.seen.lock().unwrap();
        let images_in = |messages: &[Message]| -> usize {
            messages
                .iter()
                .map(|m| match m {
                    Message::User { content } => content
                        .iter()
                        .filter(|p| matches!(p, UserContent::Image(_)))
                        .count(),
                    _ => 0,
                })
                .sum()
        };
        // Second call: BOTH frames from the first batch are present —
        // pruning to "newest only" would have left one.
        assert_eq!(images_in(&seen[1]), 2);
        // Third call: the first batch is pruned, the second batch's one
        // frame is intact.
        assert_eq!(images_in(&seen[2]), 1);
        let pruned = seen[2]
            .iter()
            .filter(|m| format!("{m:?}").contains("frames pruned"))
            .count();
        assert_eq!(pruned, 2);
    }

    /// Held to the Python's own `_parse_id_from_tool_json` and
    /// `_sanitize_args`, case by case.
    #[test]
    fn the_two_helpers_match_the_python() {
        let oracle: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/agent_helpers.json")).unwrap();
        let ids = oracle["parse_id"].as_array().unwrap();
        assert!(ids.len() >= 12);
        for case in ids {
            let text = case[0].as_str().unwrap();
            assert_eq!(
                parse_id_from_tool_json(text),
                case[1].as_i64(),
                "parse_id({text:?})"
            );
        }
        let args = oracle["sanitize"].as_array().unwrap();
        assert!(args.len() >= 6);
        for case in args {
            assert_eq!(
                sanitize_args(case[0].as_object().unwrap()),
                case[1],
                "sanitize({})",
                case[0]
            );
        }
    }
}
