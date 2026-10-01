//! The MCP activity tracker.
//!
//! Ported from `backend/app/mcp/activity.py`. A rolling window of the
//! last 500 events, a per-key session table, and a fan-out to the
//! dashboard's live stream — all in memory, and all of it read by
//! `/api/mcp/activity/*`.
//!
//! **One process owns this.** The producer is the MCP tool wrapper and
//! the consumers are the activity routes, so they move together: a
//! dashboard reading Rust's tracker while Python's server fills
//! Python's would show an empty stream and call it quiet.
//!
//! Exactly one event is logged per tool call — `completed` or `error`,
//! never both and never a `started` alongside them — so a call counts
//! once in the session table and once in the totals. The `status`
//! field's docstring mentions "started"; nothing emits it.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use serde_json::{json, Map, Value};

/// `deque(maxlen=500)`.
const MAX_EVENTS: usize = 500;

/// Sessions idle longer than this are not returned at all.
const SESSION_TIMEOUT_SECONDS: f64 = 300.0;
/// Below this a session is `active`; between the two it is `idle`.
const SESSION_ACTIVE_SECONDS: f64 = 60.0;

#[derive(Debug, Clone)]
pub struct McpEvent {
    pub id: String,
    pub timestamp: f64,
    pub tool_name: String,
    pub org_id: String,
    pub key_name: String,
    /// `completed` or `error`.
    pub status: String,
    pub duration_ms: Option<i64>,
    pub error: Option<String>,
    pub args_summary: Option<String>,
}

impl McpEvent {
    /// `{k: v for k, v in asdict(self).items() if v is not None}` — in
    /// the dataclass's field order, with absent fields dropped rather
    /// than sent as null.
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("id".into(), json!(self.id));
        out.insert("timestamp".into(), json!(self.timestamp));
        out.insert("tool_name".into(), json!(self.tool_name));
        out.insert("org_id".into(), json!(self.org_id));
        out.insert("key_name".into(), json!(self.key_name));
        out.insert("status".into(), json!(self.status));
        if let Some(duration) = self.duration_ms {
            out.insert("duration_ms".into(), json!(duration));
        }
        if let Some(error) = &self.error {
            out.insert("error".into(), json!(error));
        }
        if let Some(summary) = &self.args_summary {
            out.insert("args_summary".into(), json!(summary));
        }
        Value::Object(out)
    }
}

#[derive(Debug, Clone)]
struct Session {
    key_name: String,
    last_active: f64,
    call_count: i64,
}

#[derive(Default)]
struct Inner {
    events: VecDeque<McpEvent>,
    /// Per org, in the order keys first called — Python's dict
    /// preserves insertion order and its sort is stable, so ties in
    /// `last_active` come back in that order.
    sessions: HashMap<String, Vec<Session>>,
    total_calls: HashMap<String, i64>,
}

pub struct McpActivityTracker {
    /// `Option` because `HashMap::new` is not const and this is a
    /// static — the same shape `sse.rs` and the plan caches use.
    inner: Mutex<Option<Inner>>,
    /// The SSE fan-out, which is the same bounded-queue broadcaster the
    /// bell and the motion feed use — `sse.rs` was written from all
    /// three, this one included.
    pub broadcaster: crate::sse::Broadcaster,
}

impl Default for McpActivityTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl McpActivityTracker {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(None),
            broadcaster: crate::sse::Broadcaster::new("mcp-activity"),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(guard.get_or_insert_with(Inner::default))
    }

    /// Record one call, then publish it.
    ///
    /// Persistence is spawned rather than awaited: Python starts a
    /// daemon thread per event, and a slow database must not be in the
    /// path of a tool call that has already done its work.
    pub fn log_event(&self, pool: &sqlx::PgPool, event: McpEvent) {
        self.with(|inner| {
            if inner.events.len() == MAX_EVENTS {
                inner.events.pop_front();
            }
            inner.events.push_back(event.clone());

            let sessions = inner.sessions.entry(event.org_id.clone()).or_default();
            match sessions.iter_mut().find(|s| s.key_name == event.key_name) {
                Some(session) => {
                    session.last_active = event.timestamp;
                    session.call_count += 1;
                }
                None => sessions.push(Session {
                    key_name: event.key_name.clone(),
                    last_active: event.timestamp,
                    call_count: 1,
                }),
            }
            *inner.total_calls.entry(event.org_id.clone()).or_insert(0) += 1;
        });

        let pool = pool.clone();
        let persisted = event.clone();
        tokio::spawn(async move {
            persist_event(&pool, &persisted).await;
        });

        // `payload["type"] = "tool_call"` — assigned AFTER `to_dict()`,
        // so it is the LAST key rather than the first. The frame is a
        // serialised dict either way, and the SSE differential compares
        // the bytes.
        let mut payload = event.to_json();
        if let Some(map) = payload.as_object_mut() {
            map.insert("type".into(), json!("tool_call"));
        }
        // `"all"`, not `"admin"`: the activity stream's route is
        // already admin-only, and the audience filter here is for
        // events that reach a mixed set of subscribers.
        self.broadcaster.notify(
            &event.org_id,
            "all",
            &crate::audit::python_json_value(&payload),
        );
    }

    /// The most recent events for an org, oldest first.
    pub fn recent_events(&self, org_id: &str, limit: usize) -> Vec<McpEvent> {
        self.with(|inner| {
            let matching: Vec<&McpEvent> =
                inner.events.iter().filter(|e| e.org_id == org_id).collect();
            // `org_events[-limit:]` — the LAST `limit`, and a limit of zero
            // is the whole list in Python, because `[-0:]` is `[0:]`.
            let start = if limit == 0 {
                0
            } else {
                matching.len().saturating_sub(limit)
            };
            matching[start..].iter().map(|e| (*e).clone()).collect()
        })
    }

    /// Sessions seen within the timeout, most recently active first.
    pub fn active_sessions(&self, org_id: &str, now: f64) -> Vec<Value> {
        self.with(|inner| {
            let Some(sessions) = inner.sessions.get(org_id) else {
                return Vec::new();
            };
            let mut rows: Vec<(f64, Value)> = sessions
                .iter()
                .map(|session| {
                    let age = now - session.last_active;
                    let status = if age < SESSION_ACTIVE_SECONDS {
                        "active"
                    } else if age < SESSION_TIMEOUT_SECONDS {
                        "idle"
                    } else {
                        "disconnected"
                    };
                    (
                        session.last_active,
                        json!({
                            "key_name": session.key_name,
                            "last_active": session.last_active,
                            // `round(age)` — to an int, ties to even.
                            "last_active_ago": crate::pyrepr::round_half_even(age) as i64,
                            "call_count": session.call_count,
                            "status": status,
                        }),
                    )
                })
                .collect();
            // Stable, so ties keep the order the keys first called in.
            rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            rows.into_iter()
                .map(|(_, row)| row)
                .filter(|row| row["status"] != "disconnected")
                .collect()
        })
    }

    pub fn stats(&self, org_id: &str, now: f64) -> Value {
        self.with(|inner| {
            let events: Vec<&McpEvent> =
                inner.events.iter().filter(|e| e.org_id == org_id).collect();
            let recent = events.iter().filter(|e| now - e.timestamp < 60.0).count();
            let recent_5m = events.iter().filter(|e| now - e.timestamp < 300.0).count();
            let errors = events.iter().filter(|e| e.status == "error").count();
            let active = inner
                .sessions
                .get(org_id)
                .map(|sessions| {
                    sessions
                        .iter()
                        .filter(|s| now - s.last_active < SESSION_TIMEOUT_SECONDS)
                        .count()
                })
                .unwrap_or(0);
            json!({
                "total_calls": inner.total_calls.get(org_id).copied().unwrap_or(0),
                "calls_per_min": recent,
                "calls_5m": recent_5m,
                "error_count": errors,
                "active_clients": active,
                "recent_event_count": events.len(),
            })
        })
    }

    /// Test-facing: forget everything.
    pub fn clear(&self) {
        self.with(|inner| {
            inner.events.clear();
            inner.sessions.clear();
            inner.total_calls.clear();
        });
        self.broadcaster.clear();
    }
}

/// `_persist_event` — best-effort, and never in the caller's way.
async fn persist_event(pool: &sqlx::PgPool, event: &McpEvent) {
    // `datetime.fromtimestamp(ts, tz=UTC).replace(tzinfo=None)`.
    let Some(stamped) = chrono::DateTime::from_timestamp_micros((event.timestamp * 1e6) as i64)
    else {
        tracing::error!("[Activity] event timestamp out of range");
        return;
    };
    let result = sqlx::query(
        "INSERT INTO mcp_activity_logs
            (org_id, tool_name, key_name, status, duration_ms, args_summary, error, timestamp)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(&event.org_id)
    .bind(&event.tool_name)
    .bind(&event.key_name)
    .bind(&event.status)
    // `int(duration) if duration else None` — a zero-millisecond call
    // is falsy and stores NULL, not 0.
    .bind(event.duration_ms.filter(|d| *d != 0).map(|d| d as i32))
    .bind(&event.args_summary)
    .bind(&event.error)
    .bind(stamped.naive_utc())
    .execute(pool)
    .await;
    if let Err(err) = result {
        tracing::error!(error = %err, "[Activity] Failed to persist MCP event to DB");
    }
}

/// The singleton the tool wrapper writes to and the routes read.
pub static TRACKER: McpActivityTracker = McpActivityTracker::new();

#[cfg(test)]
mod tests {
    use super::*;

    fn event(org: &str, key: &str, tool: &str, status: &str, at: f64) -> McpEvent {
        McpEvent {
            id: "abcd1234".into(),
            timestamp: at,
            tool_name: tool.into(),
            org_id: org.into(),
            key_name: key.into(),
            status: status.into(),
            duration_ms: Some(12),
            error: None,
            args_summary: None,
        }
    }

    /// Absent fields are dropped, not sent as null — the dashboard
    /// renders what is present.
    #[test]
    fn an_event_drops_the_fields_it_does_not_have() {
        let mut e = event("o", "k", "list_cameras", "completed", 1.0);
        e.duration_ms = None;
        let json = e.to_json();
        assert!(json.get("duration_ms").is_none());
        assert!(json.get("error").is_none());
        assert!(json.get("args_summary").is_none());
        assert_eq!(json["status"], "completed");

        e.error = Some("boom".into());
        e.args_summary = Some("camera_id=cam-1".into());
        let json = e.to_json();
        assert_eq!(json["error"], "boom");
        assert_eq!(json["args_summary"], "camera_id=cam-1");
    }

    #[test]
    fn events_are_scoped_to_their_org() {
        let tracker = McpActivityTracker::new();
        tracker.with(|inner| {
            inner
                .events
                .push_back(event("a", "k1", "list_cameras", "completed", 1.0));
            inner
                .events
                .push_back(event("b", "k2", "get_camera", "completed", 2.0));
            inner
                .events
                .push_back(event("a", "k1", "get_camera", "completed", 3.0));
        });
        let mine = tracker.recent_events("a", 50);
        assert_eq!(mine.len(), 2);
        assert!(mine.iter().all(|e| e.org_id == "a"));
        // Oldest first within the window.
        assert_eq!(mine[0].timestamp, 1.0);
        assert_eq!(mine[1].timestamp, 3.0);
    }

    /// `org_events[-limit:]` keeps the NEWEST, and a limit of zero is
    /// the whole list because `[-0:]` is `[0:]` — which is the one
    /// place a naive port answers "nothing" instead.
    #[test]
    fn the_limit_keeps_the_newest_and_zero_keeps_everything() {
        let tracker = McpActivityTracker::new();
        tracker.with(|inner| {
            for i in 0..5 {
                inner
                    .events
                    .push_back(event("a", "k", "t", "completed", i as f64));
            }
        });
        let two = tracker.recent_events("a", 2);
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].timestamp, 3.0);
        assert_eq!(two[1].timestamp, 4.0);
        assert_eq!(tracker.recent_events("a", 0).len(), 5);
        assert_eq!(tracker.recent_events("a", 99).len(), 5);
    }

    /// The window is 500 and it drops from the front.
    #[test]
    fn the_window_holds_five_hundred_events() {
        let tracker = McpActivityTracker::new();
        tracker.with(|inner| {
            for i in 0..600 {
                if inner.events.len() == MAX_EVENTS {
                    inner.events.pop_front();
                }
                inner
                    .events
                    .push_back(event("a", "k", "t", "completed", i as f64));
            }
        });
        let all = tracker.recent_events("a", 1000);
        assert_eq!(all.len(), 500);
        assert_eq!(all[0].timestamp, 100.0, "the oldest hundred were dropped");
        assert_eq!(all[499].timestamp, 599.0);
    }

    #[test]
    fn sessions_are_active_then_idle_then_gone() {
        let tracker = McpActivityTracker::new();
        tracker.with(|inner| {
            inner.sessions.insert(
                "a".into(),
                vec![
                    Session {
                        key_name: "fresh".into(),
                        last_active: 1000.0,
                        call_count: 3,
                    },
                    Session {
                        key_name: "idling".into(),
                        last_active: 900.0,
                        call_count: 1,
                    },
                    Session {
                        key_name: "gone".into(),
                        last_active: 500.0,
                        call_count: 9,
                    },
                ],
            );
        });
        let rows = tracker.active_sessions("a", 1000.0);
        // `gone` is 500 seconds idle, past the timeout, and is not
        // returned at all.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["key_name"], "fresh");
        assert_eq!(rows[0]["status"], "active");
        assert_eq!(rows[0]["call_count"], 3);
        assert_eq!(rows[1]["key_name"], "idling");
        assert_eq!(rows[1]["status"], "idle");
        assert_eq!(rows[1]["last_active_ago"], 100);
    }

    /// Sixty seconds is the active line and three hundred the timeout,
    /// both exclusive on the low side.
    #[test]
    fn the_session_thresholds_are_where_python_puts_them() {
        let tracker = McpActivityTracker::new();
        tracker.with(|inner| {
            inner.sessions.insert(
                "a".into(),
                vec![
                    Session {
                        key_name: "just-active".into(),
                        last_active: 1000.0 - 59.9,
                        call_count: 1,
                    },
                    Session {
                        key_name: "just-idle".into(),
                        last_active: 1000.0 - 60.0,
                        call_count: 1,
                    },
                    Session {
                        key_name: "just-kept".into(),
                        last_active: 1000.0 - 299.9,
                        call_count: 1,
                    },
                    Session {
                        key_name: "just-dropped".into(),
                        last_active: 1000.0 - 300.0,
                        call_count: 1,
                    },
                ],
            );
        });
        let rows = tracker.active_sessions("a", 1000.0);
        let by_name: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| {
                (
                    r["key_name"].as_str().unwrap(),
                    r["status"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            by_name,
            vec![
                ("just-active", "active"),
                ("just-idle", "idle"),
                ("just-kept", "idle")
            ]
        );
    }

    #[test]
    fn stats_count_the_windows_separately() {
        let tracker = McpActivityTracker::new();
        tracker.with(|inner| {
            inner
                .events
                .push_back(event("a", "k", "t", "completed", 1000.0));
            inner.events.push_back(event("a", "k", "t", "error", 900.0));
            inner
                .events
                .push_back(event("a", "k", "t", "completed", 100.0));
            inner
                .events
                .push_back(event("b", "k", "t", "completed", 1000.0));
            inner.total_calls.insert("a".into(), 42);
            inner.sessions.insert(
                "a".into(),
                vec![Session {
                    key_name: "k".into(),
                    last_active: 1000.0,
                    call_count: 3,
                }],
            );
        });
        let stats = tracker.stats("a", 1000.0);
        assert_eq!(stats["total_calls"], 42);
        assert_eq!(
            stats["calls_per_min"], 1,
            "only the one inside sixty seconds"
        );
        assert_eq!(stats["calls_5m"], 2);
        assert_eq!(stats["error_count"], 1);
        assert_eq!(stats["active_clients"], 1);
        assert_eq!(
            stats["recent_event_count"], 3,
            "this org's events, not every org's"
        );
    }

    /// An org nobody has called for reports zeroes rather than
    /// nothing at all.
    #[test]
    fn an_unknown_org_has_empty_stats() {
        let tracker = McpActivityTracker::new();
        let stats = tracker.stats("nobody", 1000.0);
        assert_eq!(stats["total_calls"], 0);
        assert_eq!(stats["active_clients"], 0);
        assert_eq!(stats["recent_event_count"], 0);
        assert!(tracker.active_sessions("nobody", 1000.0).is_empty());
        assert!(tracker.recent_events("nobody", 50).is_empty());
    }
}
