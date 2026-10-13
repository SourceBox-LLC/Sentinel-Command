//! Draining the queue: the layer between a wakeup and the agent loop.
//!
//! Ported from `app/sentinel_agent/processor.py`.
//!
//! ```text
//! list pending ─► start what fits ─► each run: claim ─► connect MCP as its org ─► loop ─► complete
//!      ▲                                                                                  │
//!      └──────────── a run finished, or every few seconds, until the queue is empty ◄─────┘
//! ```
//!
//! **Runs are investigated concurrently,** up to [`MAX_CONCURRENT_RUNS`]
//! at once and [`MAX_CONCURRENT_RUNS_PER_ORG`] for any one org. One
//! deployment serves every org, and the drain used to work through the
//! queue one run at a time: on the test rig a three-minute investigation
//! held another camera's run for 96 s, and a run that waits past the
//! node's replay window (about a minute of video) can no longer see the
//! moment that triggered it. The per-org cap is what keeps one busy
//! customer from taking every slot while another customer's runs wait.
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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};

use serde_json::{json, Map, Value};

use crate::agent::llm::Llm;
use crate::agent::mcp_client::Tools;
use crate::agent::queue::{Completion, RunQueue};
use crate::agent::run::run_agent;

/// The wall clock for one run, claim to complete. It was the budget for a
/// whole drain while runs went one at a time (270 s, under the 300 s a
/// Fly machine gets to stop); per run, it bounds how long one stuck
/// investigation can hold a slot, and how long shutdown waits.
pub const RUN_TIMEOUT_SECONDS: f64 = 270.0;
pub const MAX_RUNS_PER_LIST: usize = 20;
/// Runs investigated at once, across every org.
pub const MAX_CONCURRENT_RUNS: usize = 4;
/// Runs investigated at once for one org. Two fits a Pro org's MCP budget
/// (30 tool calls a minute, a bucket per org) and lets two cameras that
/// fire together be looked at together.
pub const MAX_CONCURRENT_RUNS_PER_ORG: usize = 2;

/// Opens the tool surface for one org, and closes it.
pub trait ToolConnector: Send + Sync {
    type Tools: Tools;
    fn connect(
        &self,
        org_id: &str,
    ) -> impl std::future::Future<Output = Result<Self::Tools, String>> + Send;
    fn disconnect(&self, tools: Self::Tools) -> impl std::future::Future<Output = ()> + Send;
}

/// What the drain needs besides the three collaborators.
#[derive(Debug, Clone, Copy)]
pub struct DrainOptions {
    pub max_iterations: usize,
    pub max_runs: usize,
    /// First retry delay for `/complete`; doubles each attempt.
    pub complete_backoff: Duration,
    /// Runs investigated at the same time, across every org.
    pub max_concurrent: usize,
    /// Of those, at most this many for one org.
    pub max_per_org: usize,
    /// Wall clock for one run; past it the run is completed as an error.
    pub run_timeout: Duration,
    /// How often to re-list while runs are in flight, so a free slot is
    /// filled without waiting for one of them to finish.
    pub relist_every: Duration,
}

impl DrainOptions {
    pub fn new(max_iterations: usize) -> Self {
        Self {
            max_iterations,
            max_runs: MAX_RUNS_PER_LIST,
            complete_backoff: Duration::from_secs(2),
            max_concurrent: MAX_CONCURRENT_RUNS,
            max_per_org: MAX_CONCURRENT_RUNS_PER_ORG,
            run_timeout: Duration::from_secs_f64(RUN_TIMEOUT_SECONDS),
            relist_every: Duration::from_secs(5),
        }
    }
}

fn bump(summary: &mut Map<String, Value>, key: &str, by: u64) {
    let current = summary.get(key).and_then(Value::as_u64).unwrap_or(0);
    summary.insert(key.to_string(), json!(current + by));
}

fn push_result(summary: &mut Map<String, Value>, result: Value) {
    if let Some(Value::Array(results)) = summary.get_mut("results") {
        results.push(result);
    }
}

fn run_id_of(run: &Value) -> String {
    match run.get("id") {
        Some(Value::String(id)) => id.clone(),
        Some(Value::Number(id)) => id.to_string(),
        _ => "?".to_string(),
    }
}

fn org_of(run: &Value) -> Option<String> {
    match run.get("org_id") {
        Some(Value::String(org)) if !org.is_empty() => Some(org.clone()),
        Some(Value::Number(org)) => Some(org.to_string()),
        _ => None,
    }
}

/// Drain pending runs, several at a time, until the queue is empty.
///
/// `stop` ends the drain gracefully: nothing new is started, and the runs
/// in flight finish (each within its own wall clock).
pub async fn process_pending_runs<Q, L, C>(
    queue: &Q,
    llm: &L,
    connector: &C,
    options: DrainOptions,
    stop: Option<&AtomicBool>,
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

    // Every id this drain has seen, finished, or must not try again.
    let mut seen: HashSet<String> = HashSet::new();
    let mut finished: HashSet<String> = HashSet::new();
    let mut failed: HashSet<String> = HashSet::new();
    let mut in_flight: HashSet<String> = HashSet::new();
    let mut per_org: HashMap<String, usize> = HashMap::new();
    let mut active = FuturesUnordered::new();
    let stopping = || stop.is_some_and(|flag| flag.load(Ordering::Relaxed));

    loop {
        // Fill free slots, oldest run first (Command Center lists them so).
        if !stopping() && active.len() < options.max_concurrent.max(1) {
            match queue.list_pending(options.max_runs).await {
                Ok(pending) => {
                    for run in &pending {
                        if active.len() >= options.max_concurrent.max(1) {
                            break;
                        }
                        let run_id = run_id_of(run);
                        if failed.contains(&run_id)
                            || finished.contains(&run_id)
                            || in_flight.contains(&run_id)
                        {
                            continue;
                        }
                        let first_sight = seen.insert(run_id.clone());
                        if first_sight {
                            bump(&mut summary, "fetched", 1);
                        }
                        // Every pending row should carry an org, but one
                        // that does not cannot be scoped — skip it rather
                        // than act on a corrupted row under no org at all.
                        let Some(org) = org_of(run) else {
                            if first_sight {
                                tracing::warn!(run = %run_id, "processor: run has no org_id, skipping");
                                bump(&mut summary, "skipped_no_org", 1);
                            }
                            continue;
                        };
                        let running_for_org = per_org.entry(org.clone()).or_insert(0);
                        if *running_for_org >= options.max_per_org.max(1) {
                            continue; // this org's turn comes when one of its runs ends
                        }
                        *running_for_org += 1;
                        in_flight.insert(run_id.clone());
                        active.push(run_with_timeout(
                            run.clone(),
                            run_id,
                            org,
                            queue,
                            llm,
                            connector,
                            options,
                        ));
                    }
                }
                Err(err) => {
                    tracing::error!(error = %err, "processor: failed to fetch pending runs");
                    bump(&mut summary, "errored", 1);
                    summary.insert("fetched_error".into(), json!(err.to_string()));
                    if active.is_empty() {
                        return Value::Object(summary);
                    }
                }
            }
        }

        // Nothing running and nothing more could be started: the queue is
        // empty, or holds only rows this drain must skip (no org, already
        // failed once). Command Center's reaper owns those.
        if active.is_empty() {
            return Value::Object(summary);
        }

        // A run finishing frees a slot; the tick re-lists anyway, so a slot
        // that is already free takes a new run without waiting for one.
        let done = tokio::select! {
            done = active.next() => done,
            _ = tokio::time::sleep(options.relist_every) => None,
        };
        let Some((run_id, org, outcome)) = done else {
            continue;
        };
        in_flight.remove(&run_id);
        if let Some(count) = per_org.get_mut(&org) {
            *count = count.saturating_sub(1);
        }
        match outcome {
            Ok(()) => {
                finished.insert(run_id.clone());
                bump(&mut summary, "processed", 1);
                push_result(&mut summary, json!({ "id": run_id, "status": "ok" }));
            }
            Err(reason) => {
                failed.insert(run_id.clone());
                bump(&mut summary, "errored", 1);
                push_result(
                    &mut summary,
                    json!({ "id": run_id, "status": "error", "error": reason }),
                );
            }
        }
    }
}

/// One run under its own wall clock. A failure, including the clock, is
/// reported to Command Center here, inside the run's own future, so its
/// `/complete` retries never hold up the other runs in flight.
async fn run_with_timeout<Q, L, C>(
    run: Value,
    run_id: String,
    org: String,
    queue: &Q,
    llm: &L,
    connector: &C,
    options: DrainOptions,
) -> (String, String, Result<(), String>)
where
    Q: RunQueue,
    L: Llm,
    C: ToolConnector,
{
    let attempt = process_one_run(&run, &run_id, &org, queue, llm, connector, options);
    let outcome = match tokio::time::timeout(options.run_timeout, attempt).await {
        Ok(outcome) => outcome.map_err(|reason| format!("Agent harness failure: {reason}")),
        Err(_) => Err(format!(
            "Agent hit the {:.0}s wall-clock timeout — investigation incomplete.",
            options.run_timeout.as_secs_f64()
        )),
    };
    if let Err(reason) = &outcome {
        tracing::error!(run = %run_id, error = %reason, "processor: run failed");
        // Best effort: tell Command Center, so the UI shows an error
        // instead of a run that is pending or running forever.
        let completion = Completion::error(reason.clone());
        if let Err(err) =
            complete_with_retry(queue, &run_id, &completion, options.complete_backoff).await
        {
            tracing::error!(run = %run_id, error = %err, "processor: complete() also failed");
        }
    }
    (run_id, org, outcome)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::llm::{AssistantTurn, LlmError};
    use crate::agent::mcp_client::ToolOutput;
    use crate::agent::queue::QueueError;
    use rig_core::completion::ToolDefinition;
    use rig_core::message::{AssistantContent, Message};
    use std::sync::Mutex;

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
            max_concurrent: 4,
            max_per_org: 2,
            run_timeout: Duration::from_secs(5),
            relist_every: Duration::from_millis(20),
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
        // Both started from one listing; the re-list after the first
        // finishes finds the queue empty, and the drain stops when the
        // second is done. (A fake run finishes inside its first poll, so
        // `a` completes before `b` is first polled.)
        assert_eq!(
            *queue.calls.lock().unwrap(),
            [
                "list",
                "start a",
                "complete a",
                "list",
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

    /// The wall clock is per run: a stuck run is completed as an error by
    /// id, and does not hold up the run beside it.
    #[tokio::test]
    async fn a_stuck_run_times_out_alone() {
        let queue = FakeQueue::default();
        *queue.lists.lock().unwrap() = vec![Ok(vec![run("stuck", "org-1")])];
        let summary = process_pending_runs(
            &queue,
            &Model { hang: true },
            &Connector::default(),
            DrainOptions {
                run_timeout: Duration::from_millis(50),
                ..options()
            },
            None,
        )
        .await;
        assert_eq!(summary["errored"], 1);
        let completed = queue.completed.lock().unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].0, "stuck");
        assert_eq!(completed[0].1.outcome, "error");
        assert!(completed[0].1.summary.contains("wall-clock timeout"));
    }

    /// A model that takes a while, and counts how many runs were inside it
    /// at once, and the order runs reached it.
    #[derive(Default)]
    struct SlowModel {
        now: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
        order: Mutex<Vec<String>>,
    }
    impl Llm for SlowModel {
        async fn chat(
            &self,
            messages: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<AssistantTurn, LlmError> {
            use std::sync::atomic::Ordering::SeqCst;
            let seen = format!("{messages:?}");
            let id = seen
                .split("Run id: ")
                .nth(1)
                .and_then(|rest| rest.split(['\\', '"']).next())
                .unwrap_or("?")
                .to_string();
            self.order.lock().unwrap().push(id);
            let now = self.now.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            tokio::time::sleep(Duration::from_millis(60)).await;
            self.now.fetch_sub(1, SeqCst);
            Ok(AssistantTurn {
                content: vec![AssistantContent::text("All clear.")],
            })
        }
        fn timeout_seconds(&self) -> f64 {
            120.0
        }
    }

    /// The cloud service serves every org from one queue: runs must not
    /// wait for each other one at a time.
    #[tokio::test]
    async fn runs_from_different_orgs_are_investigated_at_once() {
        let queue = FakeQueue::default();
        *queue.lists.lock().unwrap() = vec![Ok(vec![
            run("a", "org-1"),
            run("b", "org-2"),
            run("c", "org-3"),
            run("d", "org-4"),
        ])];
        let model = SlowModel::default();
        let summary =
            process_pending_runs(&queue, &model, &Connector::default(), options(), None).await;
        assert_eq!(summary["processed"], 4);
        assert_eq!(
            model.peak.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "all four in flight together"
        );
    }

    /// The per-org cap: a busy org cannot take every slot. Its third run
    /// waits; the other org's run, listed after it, does not.
    #[tokio::test]
    async fn a_busy_org_cannot_make_another_org_wait() {
        let queue = FakeQueue::default();
        let listing = vec![
            run("busy-1", "busy"),
            run("busy-2", "busy"),
            run("busy-3", "busy"),
            run("other-1", "other"),
        ];
        // Command Center keeps listing a run until it is claimed.
        *queue.lists.lock().unwrap() = vec![Ok(listing); 100];
        let model = SlowModel::default();
        let summary =
            process_pending_runs(&queue, &model, &Connector::default(), options(), None).await;
        assert_eq!(summary["processed"], 4);
        let order = model.order.lock().unwrap().clone();
        let at = |id: &str| order.iter().position(|o| o == id).unwrap_or(usize::MAX);
        assert!(at("other-1") < at("busy-3"), "{order:?}");
        assert_eq!(
            model.peak.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "two of busy's, one of other's"
        );
    }

    /// Shutdown: nothing new starts once `stop` is set.
    #[tokio::test]
    async fn a_stopping_drain_starts_nothing() {
        let queue = FakeQueue::default();
        *queue.lists.lock().unwrap() = vec![Ok(vec![run("a", "org-1")])];
        let stop = AtomicBool::new(true);
        let summary = process_pending_runs(
            &queue,
            &Model { hang: false },
            &Connector::default(),
            options(),
            Some(&stop),
        )
        .await;
        assert_eq!(summary["processed"], 0);
        assert!(queue.calls.lock().unwrap().is_empty());
    }
}
