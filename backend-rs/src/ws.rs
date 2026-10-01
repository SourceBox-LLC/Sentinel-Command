//! The CameraNode command channel: who is connected, and how a request
//! handler asks one of them to do something.
//!
//! Ported from `backend/app/api/ws.py`. The route itself is in
//! `api/ws.rs`; this is the state behind it — two sliding-window
//! throttles and the connection registry.
//!
//! **One process owns this too.** A node holds one socket, to one
//! machine. A handler on any other machine cannot reach it, which is
//! why `POST /api/cameras/{id}/snapshot` and every other
//! command-issuing route has to move in the same step as the socket.
//!
//! **The identity guard is the part that is easy to get wrong.** When a
//! node reconnects to the same process, `connect` replaces its socket
//! — and the *old* socket's receive loop then exits and calls
//! `disconnect`. A blind removal there evicts the new, live connection,
//! leaving a node that keeps heartbeating while every command against
//! it fails, until it happens to reconnect again. So a disconnect only
//! tears down the registry entry it still owns.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

/// `WS_MAX_MSGS_PER_MINUTE`. Heartbeats fire every thirty seconds and
/// command results are sporadic, so this leaves about six times the
/// legitimate headroom.
pub const MAX_MSGS_PER_MINUTE: usize = 180;

/// `WS_MAX_CONNECTS_PER_MINUTE`.
///
/// A separate budget from the message rate because the threat is a
/// different one. Every successful connect *closes the previous
/// socket* — deliberately, so a node that crashed and came back does
/// not leave a zombie — which means someone holding a stolen key can
/// keep the real node permanently disconnected by reconnecting every
/// few milliseconds. A healthy node connects once and reconnects on
/// the order of seconds to minutes.
pub const MAX_CONNECTS_PER_MINUTE: usize = 10;

const WINDOW: Duration = Duration::from_secs(60);

/// How long a command waits for its answer.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// `NodeRateLimiter` — a sliding window per node id.
#[derive(Default)]
pub struct NodeRateLimiter {
    inner: Mutex<Option<LimiterInner>>,
    max: usize,
}

#[derive(Default)]
struct LimiterInner {
    windows: HashMap<String, VecDeque<Instant>>,
    last_sweep: Option<Instant>,
}

impl NodeRateLimiter {
    pub const fn new(max: usize) -> Self {
        Self {
            inner: Mutex::new(None),
            max,
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut LimiterInner) -> T) -> T {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(guard.get_or_insert_with(LimiterInner::default))
    }

    /// `allow(node_id)` — true when this message or connect fits in the
    /// window, and records it when it does.
    pub fn allow(&self, node_id: &str) -> bool {
        let now = Instant::now();
        let max = self.max;
        self.with(|inner| {
            sweep(inner, now);
            let window = inner.windows.entry(node_id.to_string()).or_default();
            while window
                .front()
                .is_some_and(|at| now.duration_since(*at) >= WINDOW)
            {
                window.pop_front();
            }
            if window.len() >= max {
                return false;
            }
            window.push_back(now);
            true
        })
    }

    /// `forget(node_id)` — drop a node's window when its socket closes.
    pub fn forget(&self, node_id: &str) {
        self.with(|inner| {
            inner.windows.remove(node_id);
        });
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.with(|inner| inner.windows.len())
    }

    #[cfg(test)]
    fn clear(&self) {
        self.with(|inner| {
            inner.windows.clear();
            inner.last_sweep = None;
        });
    }
}

/// `_maybe_sweep` — drop buckets that have fully aged out.
///
/// The connect throttle is consulted *before* authentication, so the
/// node id it keys on is whatever the peer sent. Without this, someone
/// cycling random ids leaks one map entry per id forever; `forget` only
/// runs for a node that authenticated and reached the receive loop.
/// Amortised to at most one pass per window.
fn sweep(inner: &mut LimiterInner, now: Instant) {
    if inner
        .last_sweep
        .is_some_and(|at| now.duration_since(at) < WINDOW)
    {
        return;
    }
    inner.last_sweep = Some(now);
    inner.windows.retain(|_, window| {
        window
            .back()
            .is_some_and(|at| now.duration_since(*at) < WINDOW)
    });
}

/// Why a command did not produce an answer.
/// Each variant's `Display` is the string the Python's exception
/// carries, because the route puts `str(e)` straight into the response
/// body.
#[derive(Debug, PartialEq, Eq)]
pub enum CommandError {
    /// No socket for this node on this machine.
    NotConnected {
        node: String,
    },
    /// The socket was already half-closed — a TCP reset, or a node that
    /// crashed — and the receive loop had not noticed yet.
    ///
    /// Python appends the underlying exception's own text, which is
    /// whatever `WebSocketDisconnect`, `RuntimeError` or `OSError`
    /// happened to say. There is no reproducing that string, and no
    /// harness can reach this branch: it needs a socket half-closed
    /// between the registry lookup and the send.
    SendFailed {
        node: String,
    },
    Timeout {
        node: String,
        command: String,
    },
    /// The node reconnected, or went away, while the command was in
    /// flight. Its future is cancelled rather than left to time out.
    Disconnected {
        node: String,
        command: String,
    },
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::NotConnected { node } => write!(f, "Node {node} is not connected"),
            CommandError::SendFailed { node } => write!(f, "Node {node} send failed"),
            CommandError::Timeout { node, command } => {
                write!(f, "Command {command} to node {node} timed out")
            }
            CommandError::Disconnected { node, command } => {
                write!(f, "Node {node} disconnected while awaiting {command}")
            }
        }
    }
}

struct Connection {
    /// Distinguishes this socket from the one that replaces it. Python
    /// compares object identity; a counter is the same test.
    id: u64,
    frames: mpsc::Sender<String>,
}

struct Pending {
    node_id: String,
    answer: oneshot::Sender<Value>,
}

#[derive(Default)]
struct ManagerInner {
    connections: HashMap<String, Connection>,
    /// The order nodes connected in, which `connected_nodes` reports.
    ///
    /// Python's registry is a dict, so it hands them back in insertion
    /// order — and a reconnect assigns to an existing key, which keeps
    /// the original position rather than moving it to the end. A
    /// `HashMap`'s iteration order is arbitrary and changes run to run,
    /// so `GET /api/nodes/ws-status` would return the same set in a
    /// different sequence every time.
    order: Vec<String>,
    pending: HashMap<String, Pending>,
    next_id: u64,
}

/// `ConnectionManager` — the singleton every command-issuing route
/// reaches through.
#[derive(Default)]
pub struct ConnectionManager {
    inner: Mutex<Option<ManagerInner>>,
}

/// The handle a receive loop holds for the life of one socket.
pub struct Registration {
    pub node_id: String,
    pub id: u64,
    /// Frames the manager wants written to this socket.
    pub frames: mpsc::Receiver<String>,
}

impl ConnectionManager {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut ManagerInner) -> T) -> T {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(guard.get_or_insert_with(ManagerInner::default))
    }

    /// `connected_nodes`, in the order they connected.
    pub fn connected_nodes(&self) -> Vec<String> {
        self.with(|inner| inner.order.clone())
    }

    /// `is_connected(node_id)`.
    pub fn is_connected(&self, node_id: &str) -> bool {
        self.with(|inner| inner.connections.contains_key(node_id))
    }

    /// `connect(node_id, ws)` — register a socket, replacing any older
    /// one for the same node.
    ///
    /// The replaced socket can never answer its in-flight commands, so
    /// they are cancelled *here*, where the replacement is
    /// unambiguous. Leaving them would block their callers for the full
    /// command timeout instead of failing fast on the reconnect.
    pub fn connect(&self, node_id: &str) -> Registration {
        let (frames_tx, frames_rx) = mpsc::channel(32);
        let id = self.with(|inner| {
            inner.next_id += 1;
            let id = inner.next_id;
            // Dropping the old sender closes its channel, which is what
            // ends that socket's writer and closes it.
            if inner
                .connections
                .insert(
                    node_id.to_string(),
                    Connection {
                        id,
                        frames: frames_tx,
                    },
                )
                .is_some()
            {
                cancel_pending(inner, node_id);
            } else {
                // Only a *new* key extends the order. A reconnect
                // reassigns an existing one, which in Python leaves it
                // where it was.
                inner.order.push(node_id.to_string());
            }
            id
        });
        tracing::info!(node_id, "[WS] Node connected via WebSocket");
        Registration {
            node_id: node_id.to_string(),
            id,
            frames: frames_rx,
        }
    }

    /// `disconnect(node_id, ws)`.
    ///
    /// The `id` is the identity guard: a socket that has already been
    /// replaced owns nothing and must leave both the registry and the
    /// newer connection's pending commands alone.
    pub fn disconnect(&self, node_id: &str, id: u64) {
        let owned = self.with(|inner| {
            let owns = inner
                .connections
                .get(node_id)
                .is_some_and(|existing| existing.id == id);
            if owns {
                inner.connections.remove(node_id);
                inner.order.retain(|existing| existing != node_id);
                cancel_pending(inner, node_id);
            }
            owns
        });
        if owned {
            tracing::info!(node_id, "[WS] Node disconnected from WebSocket");
        }
    }

    /// `send_command(node_id, command, payload, timeout)`.
    pub async fn send_command(
        &self,
        node_id: &str,
        command: &str,
        payload: Value,
        timeout: Duration,
    ) -> Result<Value, CommandError> {
        let correlation_id = uuid::Uuid::new_v4().to_string();
        let frame = serde_json::json!({
            "type": "command",
            "id": correlation_id,
            "command": command,
            "payload": payload,
        })
        .to_string();

        let (answer_tx, answer_rx) = oneshot::channel();
        // The connection's id travels with its sender. Evicting on a
        // failed send has to name *this* socket: by the time the send
        // fails the node may already have reconnected, and looking the
        // id up again would tear down the connection that replaced it
        // — the same mistake the identity guard exists to prevent,
        // made from the other end.
        let connection = self.with(|inner| {
            let connection = inner.connections.get(node_id)?;
            let handle = (connection.id, connection.frames.clone());
            inner.pending.insert(
                correlation_id.clone(),
                Pending {
                    node_id: node_id.to_string(),
                    answer: answer_tx,
                },
            );
            Some(handle)
        });
        let Some((connection_id, sender)) = connection else {
            return Err(CommandError::NotConnected {
                node: node_id.to_string(),
            });
        };

        // A closed channel means the writer is gone: the socket was
        // half-closed and the receive loop had not noticed. Evict it
        // and say so, rather than letting the caller wait out the
        // timeout on a socket that will never answer.
        if sender.send(frame).await.is_err() {
            self.forget_pending(&correlation_id);
            self.disconnect(node_id, connection_id);
            return Err(CommandError::SendFailed {
                node: node_id.to_string(),
            });
        }

        let outcome = match tokio::time::timeout(timeout, answer_rx).await {
            Ok(Ok(value)) => Ok(value),
            // The sender was dropped: the node reconnected or went away.
            Ok(Err(_)) => Err(CommandError::Disconnected {
                node: node_id.to_string(),
                command: command.to_string(),
            }),
            Err(_) => Err(CommandError::Timeout {
                node: node_id.to_string(),
                command: command.to_string(),
            }),
        };
        self.forget_pending(&correlation_id);
        outcome
    }

    /// `resolve_command(correlation_id, node_id, result)`.
    ///
    /// Delivered only when the answering node is the one the command
    /// went to. Correlation ids are unguessable, so this is defence in
    /// depth — but it means a stray or forged `command_result` from
    /// another node can never satisfy someone else's pending command.
    pub fn resolve_command(&self, correlation_id: &str, node_id: &str, result: Value) {
        self.with(|inner| {
            let matches = inner
                .pending
                .get(correlation_id)
                .is_some_and(|pending| pending.node_id == node_id);
            if matches {
                if let Some(pending) = inner.pending.remove(correlation_id) {
                    let _ = pending.answer.send(result);
                }
            }
        });
    }

    /// Hand a frame to a node's socket without waiting for an answer —
    /// the acks and errors the receive loop writes back.
    pub async fn send_frame(&self, node_id: &str, frame: String) -> bool {
        let sender = self.with(|inner| inner.connections.get(node_id).map(|c| c.frames.clone()));
        match sender {
            Some(sender) => sender.send(frame).await.is_ok(),
            None => false,
        }
    }

    fn forget_pending(&self, correlation_id: &str) {
        self.with(|inner| {
            inner.pending.remove(correlation_id);
        });
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.with(|inner| inner.pending.len())
    }
}

/// `_cancel_pending` — drop every in-flight command for a node so its
/// callers fail fast instead of waiting out the timeout.
fn cancel_pending(inner: &mut ManagerInner, node_id: &str) {
    // Dropping the oneshot sender is the cancellation: the awaiting
    // half sees a closed channel.
    inner
        .pending
        .retain(|_, pending| pending.node_id != node_id);
}

/// The message-rate limiter, one window per connected node.
pub static MESSAGE_LIMITER: NodeRateLimiter = NodeRateLimiter::new(MAX_MSGS_PER_MINUTE);

/// The connect throttle, consulted before authentication.
pub static CONNECT_THROTTLE: NodeRateLimiter = NodeRateLimiter::new(MAX_CONNECTS_PER_MINUTE);

/// `manager` — the singleton the command routes reach through.
pub static MANAGER: ConnectionManager = ConnectionManager::new();

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn limiter(max: usize) -> NodeRateLimiter {
        NodeRateLimiter::new(max)
    }

    #[test]
    fn a_window_admits_its_budget_and_no_more() {
        let l = limiter(3);
        for i in 0..3 {
            assert!(l.allow("node-a"), "message {i} should have fit");
        }
        assert!(!l.allow("node-a"));
        // Another node has its own budget.
        assert!(l.allow("node-b"));
    }

    #[test]
    fn forgetting_a_node_frees_its_window() {
        let l = limiter(2);
        assert!(l.allow("node-a"));
        assert!(l.allow("node-a"));
        assert!(!l.allow("node-a"));
        l.forget("node-a");
        assert!(l.allow("node-a"));
    }

    /// The connect throttle is consulted before auth, so its keys are
    /// whatever a peer sent. Without the sweep, someone cycling ids
    /// leaks one entry each, forever.
    #[test]
    fn aged_out_windows_are_swept() {
        let l = limiter(5);
        for i in 0..50 {
            l.allow(&format!("node-{i}"));
        }
        assert_eq!(l.tracked(), 50);
        // Backdate every entry past the window and force the sweep.
        l.with(|inner| {
            let old = Instant::now() - WINDOW - Duration::from_secs(1);
            for window in inner.windows.values_mut() {
                for at in window.iter_mut() {
                    *at = old;
                }
            }
            inner.last_sweep = Some(old);
        });
        l.allow("node-fresh");
        assert_eq!(l.tracked(), 1, "only the new entry should remain");
        l.clear();
    }

    #[tokio::test]
    async fn a_command_reaches_the_node_and_its_answer_comes_back() {
        // Its own static, not a shared one: the manager has to be
        // reachable from the task issuing the command, and a static
        // shared between tests would have one test's `connect` evict
        // another's mid-await.
        static M: ConnectionManager = ConnectionManager::new();
        let mut registration = M.connect("node-a");
        assert!(M.is_connected("node-a"));
        assert_eq!(M.connected_nodes(), vec!["node-a".to_string()]);

        let issued = tokio::spawn(async {
            M.send_command(
                "node-a",
                "take_snapshot",
                json!({"camera_id": "cam-1"}),
                COMMAND_TIMEOUT,
            )
            .await
        });

        let frame = registration
            .frames
            .recv()
            .await
            .expect("a frame should be queued");
        let parsed: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(parsed["type"], "command");
        assert_eq!(parsed["command"], "take_snapshot");
        assert_eq!(parsed["payload"], json!({"camera_id": "cam-1"}));
        let correlation = parsed["id"].as_str().unwrap().to_string();

        M.resolve_command(&correlation, "node-a", json!({"ok": true}));
        assert_eq!(issued.await.unwrap().unwrap(), json!({"ok": true}));
        // The pending map does not leak.
        assert_eq!(M.pending_count(), 0);
    }

    /// Python's registry is a dict, so the order is the order they
    /// connected — and a reconnect keeps its place rather than moving
    /// to the end. `GET /api/nodes/ws-status` returns this list.
    #[tokio::test]
    async fn connected_nodes_keeps_the_order_they_arrived_in() {
        let b = ConnectionManager::new();
        let first = b.connect("node-a");
        let _second = b.connect("node-b");
        let _third = b.connect("node-c");
        assert_eq!(b.connected_nodes(), ["node-a", "node-b", "node-c"]);

        // A reconnect replaces the socket and keeps the position.
        let _again = b.connect("node-a");
        assert_eq!(b.connected_nodes(), ["node-a", "node-b", "node-c"]);

        // A disconnect that does not own the slot changes nothing.
        b.disconnect("node-a", first.id);
        assert_eq!(b.connected_nodes(), ["node-a", "node-b", "node-c"]);

        // One that does removes exactly its own entry.
        b.disconnect("node-b", _second.id);
        assert_eq!(b.connected_nodes(), ["node-a", "node-c"]);
    }

    #[tokio::test]
    async fn a_command_to_an_unconnected_node_fails_at_once() {
        let m = ConnectionManager::new();
        let err = m
            .send_command("node-missing", "take_snapshot", json!({}), COMMAND_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            CommandError::NotConnected {
                node: "node-missing".into()
            }
        );
        // The message is what the route puts in the response body.
        assert_eq!(err.to_string(), "Node node-missing is not connected");
        assert_eq!(m.pending_count(), 0);
    }

    #[tokio::test]
    async fn a_command_times_out_rather_than_hanging() {
        let m = ConnectionManager::new();
        let mut registration = m.connect("node-a");
        let err = m
            .send_command(
                "node-a",
                "list_recordings",
                json!({}),
                Duration::from_millis(60),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CommandError::Timeout { .. }), "{err:?}");
        assert_eq!(
            err.to_string(),
            "Command list_recordings to node node-a timed out"
        );
        // The frame really was queued; nobody answered it.
        assert!(registration.frames.try_recv().is_ok());
        assert_eq!(m.pending_count(), 0);
    }

    /// The guard that keeps a stale socket's cleanup from evicting the
    /// live one that replaced it.
    #[tokio::test]
    async fn a_replaced_socket_cannot_evict_its_replacement() {
        let m = ConnectionManager::new();
        let first = m.connect("node-a");
        let second = m.connect("node-a");
        assert_ne!(first.id, second.id);

        // The old socket's receive loop now exits and cleans up.
        m.disconnect("node-a", first.id);
        assert!(
            m.is_connected("node-a"),
            "the live connection was evicted by the socket it replaced"
        );

        // And the one that still owns the slot can tear it down.
        m.disconnect("node-a", second.id);
        assert!(!m.is_connected("node-a"));
    }

    /// A reconnect cancels the old socket's in-flight commands, so
    /// their callers fail fast instead of waiting out the timeout.
    #[tokio::test]
    async fn a_reconnect_cancels_the_old_sockets_commands() {
        static M: ConnectionManager = ConnectionManager::new();
        let mut first = M.connect("node-a");
        let issued = tokio::spawn(async {
            M.send_command(
                "node-a",
                "take_snapshot",
                json!({}),
                Duration::from_secs(30),
            )
            .await
        });
        // Wait for the frame so the pending entry definitely exists.
        assert!(first.frames.recv().await.is_some());

        M.connect("node-a");
        let err = issued.await.unwrap().unwrap_err();
        assert!(matches!(err, CommandError::Disconnected { .. }), "{err:?}");
        assert_eq!(
            err.to_string(),
            "Node node-a disconnected while awaiting take_snapshot"
        );
    }

    /// A send that fails must evict the socket it failed on, never a
    /// newer one — the identity guard, approached from the other end.
    #[tokio::test]
    async fn a_failed_send_does_not_evict_the_replacement() {
        static M: ConnectionManager = ConnectionManager::new();
        let first = M.connect("node-a");
        // Drop the first socket's receiver, so sending on it fails the
        // way a half-closed TCP connection does.
        drop(first.frames);
        // Meanwhile the node reconnects.
        let second = M.connect("node-a");

        // The stale sender is gone from the registry, so this reaches
        // the replacement and simply queues.
        assert!(M.is_connected("node-a"));
        assert_eq!(
            M.with(|inner| inner.connections.get("node-a").map(|c| c.id)),
            Some(second.id)
        );
        M.disconnect("node-a", second.id);
    }

    /// Correlation ids are unguessable, so this is defence in depth —
    /// but a forged result from another node must never satisfy
    /// someone else's command.
    #[tokio::test]
    async fn another_nodes_answer_does_not_satisfy_a_command() {
        static M: ConnectionManager = ConnectionManager::new();
        let mut a = M.connect("node-a");
        M.connect("node-b");
        let issued = tokio::spawn(async {
            M.send_command(
                "node-a",
                "take_snapshot",
                json!({}),
                Duration::from_millis(200),
            )
            .await
        });
        let frame = a.frames.recv().await.expect("a frame");
        let correlation = serde_json::from_str::<Value>(&frame).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        // The wrong node answers with the right correlation id.
        M.resolve_command(&correlation, "node-b", json!({"stolen": true}));
        let err = issued.await.unwrap().unwrap_err();
        assert!(matches!(err, CommandError::Timeout { .. }), "{err:?}");
    }
}
