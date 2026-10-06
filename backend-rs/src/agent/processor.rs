//! Draining the queue: the layer between a wakeup and the agent loop.
//!
//! Ported from `app/sentinel_agent/processor.py`.
//!
//! ```text
//! list pending ─► for each run: claim ─► connect MCP as its org ─► loop ─► complete
//!      ▲                                                                    │
//!      └──────────────────────── until the queue is empty ◄─────────────────┘
//! ```
//!
//! One deployment serves every org. The agent holds one shared MCP secret
//! and names the org on each connection; the server treats that header as
//! the authoritative scope. So the client is built and torn down inside a
//! single run — there is no cross-run state for a bug to leak.
//!
//! Nothing here raises. Every failure lands in the summary, because the
//! caller is a webhook handler and the alternative is a 500 that tells
//! Command Center nothing about which of twenty runs went wrong.
//!
//! The parts that look like over-engineering and are each a scar:
//!
//! * **Re-list until empty.** A wakeup that arrives mid-drain is
//!   acknowledged and dropped, so a run created thirty seconds into a
//!   long drain would otherwise wait for Command Center's five-minute
//!   re-fire.
//! * **One attempt per run per drain** (`failed`). A run can fail and
//!   still be `pending` — `/start` raised and the error-complete failed
//!   too, a partial outage where reads work and writes do not. It would
//!   re-list every pass, and since each failure counts as progress the
//!   guard below would never trip: forty passes hammering a server that
//!   is already unhappy.
//! * **The forward-progress guard.** Rows that can only be skipped stay
//!   pending and would be re-listed forever.
//! * **`/complete` is retried.** It is the single most valuable byte the
//!   agent sends. Losing it to a transient 5xx orphans the run and any
//!   incident it filed.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::agent::llm::Llm;
use crate::agent::mcp_client::Tools;
use crate::agent::queue::{Completion, RunQueue};
use crate::agent::run::run_agent;

/// Comfortably under `kill_timeout = 300` in fly.toml, which is Fly's
/// maximum. Command Center's wakeup client hangs up after 5 s, so Fly's
/// idle auto-stop signals the machine about 30 s into every drain and the
/// in-flight work survives only as long as the kill timeout allows. At
/// 540 s, drains in the 335–540 s band died at SIGKILL with the cleanup
/// below unreachable, and the run sat `running` until the 20-minute
/// reaper. At 270 the cleanup always wins.
pub const DRAIN_TIMEOUT_SECONDS: f64 = 270.0;
pub const MAX_RUNS_PER_LIST: usize = 20;

/// Opens the tool surface for one org, and closes it.
pub trait ToolConnector: Send + Sync {
    type Tools: Tools;
    fn connect(
        &self,
        org_id: &str,
    ) -> impl std::future::Future<Output = Result<Self::Tools, String>> + Send;
    fn disconnect(&self, tools: Self::Tools) -> impl std::future::Future<Output = ()> + Send;
}

/// The run being worked when a wall-clock timeout fires, so it can be
/// marked errored rather than stranded.
pub type InFlight = Arc<Mutex<Option<String>>>;

/// What the drain needs besides the three collaborators.
#[derive(Debug, Clone, Copy)]
pub struct DrainOptions {
    pub max_iterations: usize,
    pub max_runs: usize,
    /// First retry delay for `/complete`; doubles each attempt.
    pub complete_backoff: Duration,
}

impl DrainOptions {
    pub fn new(max_iterations: usize) -> Self {
        Self {
            max_iterations,
            max_runs: MAX_RUNS_PER_LIST,
            complete_backoff: Duration::from_secs(2),
        }
    }
}

fn bump(summary: &mut Map<String, Value>, key: &str, by: u64) {
    let current = summary.get(key).and_then(Value::as_u64).unwrap_or(0);
    summary.insert(key.to_string(), json!(current + by));
}

fn count(summary: &Map<String, Value>, key: &str) -> u64 {
    summary.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn push_result(summary: &mut Map<String, Value>, result: Value) {
    if let Some(Value::Array(results)) = summary.get_mut("results") {
        results.push(result);
    }
}

/// Drain pending runs, one after another, until the queue is empty.
pub async fn process_pending_runs<Q, L, C>(
    queue: &Q,
    llm: &L,
    connector: &C,
    options: DrainOptions,
    in_flight: Option<&InFlight>,
) -> Value
where
    Q: RunQueue,
    L: Llm,
    C: ToolConnector,
{
    let mut summary = Map::new();
    summary.insert("fetched".into(), json!(0));
    summary.insert("processed".into(), json!(0));
    summary.insert("errored".into(), json!(0));
    summary.insert("skipped_no_org".into(), json!(0));
    summary.insert("results".into(), json!([]));

    let mut failed: HashSet<String> = HashSet::new();
    loop {
        let pending = match queue.list_pending(options.max_runs).await {
            Ok(pending) => pending,
            Err(err) => {
                tracing::error!(error = %err, "processor: failed to fetch pending runs");
                bump(&mut summary, "errored", 1);
                summary.insert("fetched_error".into(), json!(err.to_string()));
                return Value::Object(summary);
            }
        };
        if pending.is_empty() {
            return Value::Object(summary);
        }
        bump(&mut summary, "fetched", pending.len() as u64);
        let terminal_before = count(&summary, "processed") + count(&summary, "errored");

        for run in &pending {
            let run_id = match run.get("id") {
                Some(Value::String(id)) => id.clone(),
                Some(Value::Number(id)) => id.to_string(),
                _ => "?".to_string(),
            };
            if failed.contains(&run_id) {
                continue;
            }
            // Every pending row should carry an org, but one that does
            // not cannot be scoped — skip it rather than act on a
            // corrupted row under no org at all.
            let org = match run.get("org_id") {
                Some(Value::String(org)) if !org.is_empty() => org.clone(),
                Some(Value::Number(org)) => org.to_string(),
                _ => {
                    tracing::warn!(run = %run_id, "processor: run has no org_id, skipping");
                    bump(&mut summary, "skipped_no_org", 1);
                    continue;
                }
            };

            // Stamped BEFORE the first await, so the timeout wrapper sees
            // a current id even if it fires during the claim. It is never
            // cleared: if it points at a row that already finished, the
            // wrapper's complete() is an idempotent no-op server-side.
            if let Some(handle) = in_flight {
                *handle.lock().expect("in-flight handle poisoned") = Some(run_id.clone());
            }

            match process_one_run(run, &run_id, &org, queue, llm, connector, options).await {
                Ok(()) => {
                    bump(&mut summary, "processed", 1);
                    push_result(&mut summary, json!({ "id": run_id, "status": "ok" }));
                }
                Err(reason) => {
                    tracing::error!(run = %run_id, error = %reason, "processor: run failed");
                    failed.insert(run_id.clone());
                    bump(&mut summary, "errored", 1);
                    push_result(
                        &mut summary,
                        json!({ "id": run_id, "status": "error", "error": reason }),
                    );
                    // Best effort: tell Command Center, so the UI shows
                    // an error instead of a run that is pending forever.
                    let outcome = Completion::error(format!("Agent harness failure: {reason}"));
                    if let Err(err) =
                        complete_with_retry(queue, &run_id, &outcome, options.complete_backoff)
                            .await
                    {
                        tracing::error!(run = %run_id, error = %err, "processor: complete() also failed");
                    }
                }
            }
        }

        if count(&summary, "processed") + count(&summary, "errored") == terminal_before {
            // A full pass moved nothing to a terminal state: every row
            // was skip-only. Command Center's reaper owns those.
            return Value::Object(summary);
        }
    }
}

/// `POST /complete`, three attempts with a doubling backoff.
pub async fn complete_with_retry<Q: RunQueue>(
    queue: &Q,
    run_id: &str,
    completion: &Completion,
    backoff: Duration,
) -> Result<(), String> {
    const ATTEMPTS: u32 = 3;
    let mut delay = backoff;
    for attempt in 1..=ATTEMPTS {
        match queue.complete(run_id, completion).await {
            Ok(_) => return Ok(()),
            Err(err) if attempt == ATTEMPTS => return Err(err.to_string()),
            Err(err) => {
                tracing::warn!(
                    run = run_id, attempt, error = %err,
                    "processor: complete() failed — retrying"
                );
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
        }
    }
    Ok(())
}

async fn process_one_run<Q, L, C>(
    run: &Value,
    run_id: &str,
    org_id: &str,
    queue: &Q,
    llm: &L,
    connector: &C,
    options: DrainOptions,
) -> Result<(), String>
where
    Q: RunQueue,
    L: Llm,
    C: ToolConnector,
{
    tracing::info!(run = run_id, org = org_id, "processor: start run");

    // Claim it. A 404 means there is nothing to do.
    let Some(started) = queue.start(run_id).await.map_err(|err| err.to_string())? else {
        return Ok(());
    };
    // `claimed: false` — an overlapping drain won. `/start` is idempotent
    // and used to answer an indistinguishable 200 either way, so both
    // drains ran the whole loop and filed DUPLICATE incidents at double
    // the spend.
    if started.get("claimed") == Some(&Value::Bool(false)) {
        tracing::info!(
            run = run_id,
            "processor: already claimed by another drain — skipping"
        );
        return Ok(());
    }

    let tools = connector.connect(org_id).await?;
    let result = run_agent(llm, &tools, run, options.max_iterations).await;
    // Bounded: a hung streamable-HTTP teardown must not be able to hold
    // the drain — and with it the whole wakeup — open.
    if tokio::time::timeout(Duration::from_secs(10), connector.disconnect(tools))
        .await
        .is_err()
    {
        tracing::error!(run = run_id, "processor: MCP disconnect timed out");
    }

    complete_with_retry(queue, run_id, &result, options.complete_backoff).await?;
    tracing::info!(
        run = run_id,
        outcome = %result.outcome,
        incident = ?result.incident_id,
        "processor: completed run"
    );
    Ok(())
}

/// The drain, under a hard wall clock.
///
/// On timeout the in-flight run is marked errored. Without that it is
/// stranded: it is no longer `pending`, so the next wakeup's list does
/// not return it, and `/start` does not re-claim a `running` row.
pub async fn process_with_timeout<Q, L, C>(
    queue: &Q,
    llm: &L,
    connector: &C,
    options: DrainOptions,
    timeout_seconds: f64,
) -> Value
where
    Q: RunQueue,
    L: Llm,
    C: ToolConnector,
{
    let in_flight: InFlight = Arc::new(Mutex::new(None));
    let drain = process_pending_runs(queue, llm, connector, options, Some(&in_flight));
    match tokio::time::timeout(Duration::from_secs_f64(timeout_seconds), drain).await {
        Ok(summary) => summary,
        Err(_) => {
            let stranded = in_flight.lock().expect("in-flight handle poisoned").clone();
            tracing::warn!(stranded = ?stranded, "processor: hit the wall-clock timeout");
            if let Some(run_id) = &stranded {
                let outcome = Completion::error(format!(
                    "Agent hit the {timeout_seconds:.0}s wall-clock timeout — \
                     investigation incomplete."
                ));
                match complete_with_retry(queue, run_id, &outcome, options.complete_backoff).await {
                    Ok(()) => {
                        tracing::info!(run = %run_id, "processor: marked stranded run as errored")
                    }
                    Err(err) => {
                        tracing::error!(run = %run_id, error = %err, "processor: failed to clean up stranded run")
                    }
                }
            }
            json!({
                "timeout": true,
                "timeout_seconds": timeout_seconds,
                "stranded_run_id": stranded,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::llm::{AssistantTurn, LlmError};
    use crate::agent::mcp_client::ToolOutput;
    use crate::agent::queue::QueueError;
    use rig_core::completion::ToolDefinition;
    use rig_core::message::{AssistantContent, Message};

    /// A queue that plays back lists and records every call.
    #[derive(Default)]
    struct FakeQueue {
        lists: Mutex<Vec<Result<Vec<Value>, ()>>>,
        /// run id → what `/start` answers. Missing = `{"claimed": true}`.
        starts: Mutex<std::collections::HashMap<String, Result<Option<Value>, ()>>>,
        /// How many `/complete` calls to fail before succeeding.
        complete_failures: Mutex<u32>,
        calls: Mutex<Vec<String>>,
        completed: Mutex<Vec<(String, Completion)>>,
    }

    fn broken() -> QueueError {
        QueueError::Status {
            status: 503,
            what: "test".into(),
        }
    }

    impl RunQueue for FakeQueue {
        async fn list_pending(&self, _limit: usize) -> Result<Vec<Value>, QueueError> {
            self.calls.lock().unwrap().push("list".into());
            let mut lists = self.lists.lock().unwrap();
            if lists.is_empty() {
                return Ok(Vec::new());
            }
            lists.remove(0).map_err(|_| broken())
        }
        async fn start(&self, run_id: &str) -> Result<Option<Value>, QueueError> {
            self.calls.lock().unwrap().push(format!("start {run_id}"));
            match self.starts.lock().unwrap().get(run_id) {
                Some(Ok(answer)) => Ok(answer.clone()),
                Some(Err(())) => Err(broken()),
                None => Ok(Some(json!({ "claimed": true }))),
            }
        }
        async fn complete(
            &self,
            run_id: &str,
            completion: &Completion,
        ) -> Result<Option<Value>, QueueError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("complete {run_id}"));
            let mut failures = self.complete_failures.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                return Err(broken());
            }
            self.completed
                .lock()
                .unwrap()
                .push((run_id.to_string(), completion.clone()));
            Ok(Some(json!({})))
        }
    }

    /// A model that answers at once, or never.
    struct Model {
        hang: bool,
    }
    impl Llm for Model {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<AssistantTurn, LlmError> {
            if self.hang {
                std::future::pending::<()>().await;
            }
            Ok(AssistantTurn {
                content: vec![AssistantContent::text("All clear.")],
            })
        }
        fn timeout_seconds(&self) -> f64 {
            120.0
        }
    }

    struct NoTools;
    impl Tools for NoTools {
        fn definitions(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }
        async fn call(&self, name: &str, _arguments: Map<String, Value>) -> ToolOutput {
            ToolOutput::error(format!("Unknown tool: {name}"))
        }
    }

    /// Records which org each connection was opened for.
    #[derive(Default)]
    struct Connector {
        orgs: Mutex<Vec<String>>,
        fail: bool,
    }
    impl ToolConnector for Connector {
        type Tools = NoTools;
        async fn connect(&self, org_id: &str) -> Result<NoTools, String> {
            self.orgs.lock().unwrap().push(org_id.to_string());
            if self.fail {
                Err("Failed to connect to MCP servers: refused".into())
            } else {
                Ok(NoTools)
            }
        }
        async fn disconnect(&self, _tools: NoTools) {}
    }

    fn options() -> DrainOptions {
        DrainOptions {
            max_iterations: 10,
            max_runs: 20,
            complete_backoff: Duration::from_millis(1),
        }
    }

    fn run(id: &str, org: &str) -> Value {
        json!({ "id": id, "org_id": org, "trigger_type": "motion" })
    }

    #[tokio::test]
    async fn each_run_is_claimed_scoped_to_its_org_and_completed() {
        let queue = FakeQueue::default();
        *queue.lists.lock().unwrap() = vec![Ok(vec![run("a", "org-1"), run("b", "org-2")])];
        let connector = Connector::default();

        let summary =
            process_pending_runs(&queue, &Model { hang: false }, &connector, options(), None).await;

        assert_eq!(summary["fetched"], 2);
        assert_eq!(summary["processed"], 2);
        assert_eq!(summary["errored"], 0);
        assert_eq!(*connector.orgs.lock().unwrap(), ["org-1", "org-2"]);
        let completed = queue.completed.lock().unwrap();
        assert_eq!(completed.len(), 2);
        assert_eq!(completed[0].1.outcome, "no_action");
        // Listed again after the pass, found empty, stopped.
        assert_eq!(
            *queue.calls.lock().unwrap(),
            [
                "list",
                "start a",
                "complete a",
                "start b",
                "complete b",
                "list"
            ]
        );
    }

    /// The duplicate-incident bug: a lost claim must not run the loop.
    #[tokio::test]
    async fn a_run_claimed_by_another_drain_is_skipped() {
        let queue = FakeQueue::default();
        *queue.lists.lock().unwrap() = vec![Ok(vec![run("a", "org-1")])];
        queue
            .starts
            .lock()
            .unwrap()
            .insert("a".into(), Ok(Some(json!({ "claimed": false }))));
        let connector = Connector::default();

        let summary =
            process_pending_runs(&queue, &Model { hang: false }, &connector, options(), None).await;

        assert!(
            connector.orgs.lock().unwrap().is_empty(),
            "no MCP connection for a lost claim"
        );
        assert!(queue.completed.lock().unwrap().is_empty());
        // Counted as processed — it reached a terminal state for THIS
        // drain — which is what lets the loop list again and finish.
        assert_eq!(summary["processed"], 1);
    }

    #[tokio::test]
    async fn a_run_with_no_org_is_skipped_and_the_drain_stops() {
        let queue = FakeQueue::default();
        let orphan = json!({ "id": "x", "trigger_type": "motion" });
        // The same skip-only row would be listed forever.
        *queue.lists.lock().unwrap() = vec![
            Ok(vec![orphan.clone()]),
            Ok(vec![orphan.clone()]),
            Ok(vec![orphan]),
        ];
        let summary = process_pending_runs(
            &queue,
            &Model { hang: false },
            &Connector::default(),
            options(),
            None,
        )
        .await;
        assert_eq!(summary["skipped_no_org"], 1);
        assert_eq!(
            *queue.calls.lock().unwrap(),
            ["list"],
            "one pass, then the guard stops it"
        );
    }

    /// A run that fails and stays pending is attempted once per drain.
    #[tokio::test]
    async fn a_failing_run_is_tried_once_and_reported() {
        let queue = FakeQueue::default();
        *queue.lists.lock().unwrap() =
            vec![Ok(vec![run("a", "org-1")]), Ok(vec![run("a", "org-1")])];
        let connector = Connector {
            fail: true,
            ..Default::default()
        };

        let summary =
            process_pending_runs(&queue, &Model { hang: false }, &connector, options(), None).await;

        assert_eq!(summary["errored"], 1);
        assert_eq!(summary["results"][0]["status"], "error");
        assert_eq!(
            connector.orgs.lock().unwrap().len(),
            1,
            "not retried within the drain"
        );
        let completed = queue.completed.lock().unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].1.outcome, "error");
        assert!(completed[0]
            .1
            .summary
            .starts_with("Agent harness failure: "));
    }

    #[tokio::test]
    async fn a_failed_listing_is_reported_not_raised() {
        let queue = FakeQueue::default();
        *queue.lists.lock().unwrap() = vec![Err(())];
        let summary = process_pending_runs(
            &queue,
            &Model { hang: false },
            &Connector::default(),
            options(),
            None,
        )
        .await;
        assert_eq!(summary["errored"], 1);
        assert!(summary["fetched_error"].as_str().unwrap().contains("503"));
    }

    #[tokio::test]
    async fn complete_is_retried_and_gives_up_after_three() {
        let queue = FakeQueue::default();
        *queue.complete_failures.lock().unwrap() = 2;
        let outcome = Completion::error("x");
        assert!(
            complete_with_retry(&queue, "a", &outcome, Duration::from_millis(1))
                .await
                .is_ok()
        );
        assert_eq!(queue.calls.lock().unwrap().len(), 3);

        let queue = FakeQueue::default();
        *queue.complete_failures.lock().unwrap() = 3;
        assert!(
            complete_with_retry(&queue, "a", &outcome, Duration::from_millis(1))
                .await
                .is_err()
        );
        assert_eq!(
            queue.calls.lock().unwrap().len(),
            3,
            "exactly three attempts"
        );
    }

    /// The wall clock: the run in flight is marked errored, by id.
    #[tokio::test]
    async fn a_timed_out_drain_marks_the_in_flight_run_errored() {
        let queue = FakeQueue::default();
        *queue.lists.lock().unwrap() = vec![Ok(vec![run("stuck", "org-1")])];
        let summary = process_with_timeout(
            &queue,
            &Model { hang: true },
            &Connector::default(),
            options(),
            0.05,
        )
        .await;
        assert_eq!(summary["timeout"], true);
        assert_eq!(summary["stranded_run_id"], "stuck");
        let completed = queue.completed.lock().unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].0, "stuck");
        assert_eq!(completed[0].1.outcome, "error");
        assert!(completed[0].1.summary.contains("wall-clock timeout"));
    }
}
