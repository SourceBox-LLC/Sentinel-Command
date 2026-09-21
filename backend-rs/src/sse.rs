//! The per-org Server-Sent Events broadcasters.
//!
//! Ported from the three broadcaster classes in
//! `backend/app/api/{notifications,motion,mcp_activity}.py`, which are
//! the same object three times: a bounded queue per subscriber, a list
//! of them per org, and a fan-out that drops a subscriber rather than
//! waiting for it.
//!
//! **Only one process can own this.** A broadcaster is in-memory state,
//! so the moment Rust emits a notification, a bell stream still held by
//! Python is fed by nothing — and a fleet where both emit would
//! broadcast each event to half its subscribers. The stream route and
//! everything that notifies have to move together, which is why
//! `create_notification` and `GET /stream` are one slice.
//!
//! **A full queue costs the subscriber its subscription.** The fan-out
//! never blocks: a subscriber 100 events behind is dropped from the
//! list, because one stalled browser tab must not hold up an alert to
//! everyone else. What Python does *after* that is worth knowing, and
//! is reproduced here — see `PYTHON_BUGS.md` #11.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::mpsc;

/// `asyncio.Queue(maxsize=100)`.
const QUEUE_DEPTH: usize = 100;

/// `MAX_SSE_SUBSCRIBERS_PER_ORG` — the fallback when a caller has no
/// plan limit to hand. Route handlers pass the tier's own cap.
pub const DEFAULT_CAP: usize = 100;

struct Subscriber {
    id: u64,
    tx: mpsc::Sender<String>,
    is_admin: bool,
}

/// One broadcaster: subscribers grouped by org.
#[derive(Default)]
pub struct Broadcaster {
    name: &'static str,
    inner: Mutex<Option<Inner>>,
}

#[derive(Default)]
struct Inner {
    subscribers: HashMap<String, Vec<Subscriber>>,
    next_id: u64,
}

impl Broadcaster {
    pub const fn new(name: &'static str) -> Self {
        Self { name, inner: Mutex::new(None) }
    }

    /// The map is built on first use because `HashMap::new` is not a
    /// const function and these broadcasters are statics — the same
    /// shape the plan and recipient caches use.
    fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(guard.get_or_insert_with(Inner::default))
    }

    /// `subscribe(org_id, is_admin, cap)`.
    ///
    /// `None` when the org is already at its cap, which the route turns
    /// into a 429. The cap is per tier and passed in; without it a
    /// single org could hold every connection the process has.
    pub fn subscribe<'a>(
        &'a self,
        org_id: &str,
        is_admin: bool,
        cap: usize,
    ) -> Option<Subscription<'a>> {
        let name = self.name;
        self.with(|inner| {
            let existing = inner.subscribers.entry(org_id.to_string()).or_default();
            if existing.len() >= cap {
                let count = existing.len();
                tracing::warn!(broadcaster = name, org_id, count, cap, "SSE cap hit — rejecting");
                return None;
            }
            let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
            inner.next_id += 1;
            let id = inner.next_id;
            let existing = inner.subscribers.entry(org_id.to_string()).or_default();
            existing.push(Subscriber { id, tx: tx.clone(), is_admin });
            let count = existing.len();
            tracing::info!(broadcaster = name, org_id, is_admin, count, cap, "SSE subscriber added");
            Some((rx, id, tx))
        })
        .map(|(rx, id, keepalive)| Subscription {
            rx,
            broadcaster: self,
            org_id: org_id.to_string(),
            id,
            keepalive,
        })
    }

    /// `notify(org_id, event_data)` — one already-serialised frame body.
    ///
    /// `audience` is `"admin"` for events only admins may see, and the
    /// filter is applied *here* rather than in the stream, so an
    /// admin-only event never reaches a viewer's socket at all.
    pub fn notify(&self, org_id: &str, audience: &str, payload: &str) {
        self.with(|inner| {
            let Some(subscribers) = inner.subscribers.get_mut(org_id) else {
                return;
            };
            subscribers.retain(|subscriber| {
                if audience == "admin" && !subscriber.is_admin {
                    return true;
                }
                // Never blocks: a subscriber that has fallen a hundred
                // events behind loses its place rather than holding up
                // the fan-out to everyone else.
                !matches!(
                    subscriber.tx.try_send(payload.to_string()),
                    Err(mpsc::error::TrySendError::Full(_))
                )
            });
        });
    }

    fn unsubscribe(&self, org_id: &str, id: u64) {
        self.with(|inner| {
            if let Some(subscribers) = inner.subscribers.get_mut(org_id) {
                subscribers.retain(|subscriber| subscriber.id != id);
            }
        });
    }

    /// `(orgs with at least one subscriber, total subscribers)` — what
    /// `/api/health/detailed` reports.
    pub fn counts(&self) -> (usize, usize) {
        self.with(|inner| {
            let orgs = inner.subscribers.values().filter(|s| !s.is_empty()).count();
            let total = inner.subscribers.values().map(Vec::len).sum();
            (orgs, total)
        })
    }

    /// Drop every subscriber. Test-facing, and used by the harness
    /// between cases.
    pub fn clear(&self) {
        self.with(|inner| inner.subscribers.clear());
    }
}

/// One subscription's receiving end.
///
/// Unsubscribing happens on drop, which is where Python's `finally`
/// does it — and drop runs whether the client disconnected, the request
/// was cancelled, or the handler returned. Tying it to the value rather
/// than to a code path means there is no way to leak a slot.
pub struct Subscription<'a> {
    rx: mpsc::Receiver<String>,
    broadcaster: &'a Broadcaster,
    org_id: String,
    id: u64,
    /// A sender kept alive alongside the receiver, so the channel does
    /// not close when the broadcaster drops this subscriber for being
    /// slow. Python's queue has no closed state, so its generator goes
    /// on emitting keepalives to a queue nobody feeds; holding this
    /// reproduces that rather than ending the response early. See
    /// `PYTHON_BUGS.md` #11 — the behaviour is wrong in both stacks, and
    /// is meant to be fixed in one place, deliberately.
    #[allow(dead_code)]
    keepalive: mpsc::Sender<String>,
}

impl Subscription<'_> {
    /// The next event, or `None` only once this subscription has been
    /// dropped — which cannot happen while `recv` borrows it.
    pub async fn recv(&mut self) -> Option<String> {
        self.rx.recv().await
    }
}

impl Drop for Subscription<'_> {
    fn drop(&mut self) {
        self.broadcaster.unsubscribe(&self.org_id, self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A broadcaster of its own per test. Sharing one static made a
    /// parallel run clear another test's subscribers mid-await, and
    /// because a dropped subscriber still holds its sender, the waiting
    /// `recv` never returned — the deadlock the `keepalive` field
    /// exists to reproduce, reached by accident.
    fn broadcaster() -> Broadcaster {
        Broadcaster::new("test")
    }

    /// The next event, or `None` if none arrives promptly.
    ///
    /// **Every read in these tests goes through this.** A bare
    /// `recv().await` cannot fail here, only hang: the subscription
    /// holds its own sender on purpose, so a subscriber the broadcaster
    /// has dropped waits forever rather than seeing the channel close.
    /// `cargo test` has no timeout, so one such await stalls the whole
    /// run — which is exactly what happened, and it took a mutation
    /// run wedged for twenty minutes to show it. Bounded, the same
    /// case fails in fifty milliseconds and names itself.
    async fn next(sub: &mut Subscription<'_>) -> Option<String> {
        tokio::time::timeout(Duration::from_millis(50), sub.recv())
            .await
            .unwrap_or(None)
    }

    async fn idle(sub: &mut Subscription<'_>) -> bool {
        next(sub).await.is_none()
    }

    #[tokio::test]
    async fn an_event_reaches_every_subscriber_in_its_org() {
        let b = broadcaster();
        let mut a1 = b.subscribe("org_a", false, 10).unwrap();
        let mut a2 = b.subscribe("org_a", true, 10).unwrap();
        let mut other = b.subscribe("org_b", true, 10).unwrap();

        b.notify("org_a", "all", r#"{"n":1}"#);
        assert_eq!(next(&mut a1).await.as_deref(), Some(r#"{"n":1}"#));
        assert_eq!(next(&mut a2).await.as_deref(), Some(r#"{"n":1}"#));
        // Tenant isolation: the other org's stream saw nothing.
        assert!(idle(&mut other).await);
        assert_eq!(b.counts(), (2, 3));
    }

    #[tokio::test]
    async fn an_admin_event_never_reaches_a_viewers_socket() {
        let b = broadcaster();
        let mut viewer = b.subscribe("org_a", false, 10).unwrap();
        let mut admin = b.subscribe("org_a", true, 10).unwrap();

        b.notify("org_a", "admin", r#"{"secret":1}"#);
        assert_eq!(next(&mut admin).await.as_deref(), Some(r#"{"secret":1}"#));
        // The viewer was skipped, not dropped, so the next public event
        // still reaches it.
        b.notify("org_a", "all", r#"{"public":1}"#);
        assert_eq!(next(&mut viewer).await.as_deref(), Some(r#"{"public":1}"#));
        assert_eq!(b.counts(), (1, 2));
    }

    #[tokio::test]
    async fn a_subscriber_that_falls_behind_is_dropped_not_waited_for() {
        let b = broadcaster();
        let mut keeping_up = b.subscribe("org_a", false, 10).unwrap();
        let _behind = b.subscribe("org_a", false, 10).unwrap();

        // Python's is `asyncio.Queue(maxsize=100)`, and this test would
        // otherwise follow the constant wherever it went.
        assert_eq!(QUEUE_DEPTH, 100);
        // Exactly the queue depth fits; the next one is what costs the
        // slow subscriber its place.
        for i in 0..=QUEUE_DEPTH {
            b.notify("org_a", "all", &format!(r#"{{"n":{i}}}"#));
            assert!(next(&mut keeping_up).await.is_some(), "event {i} was not delivered");
        }
        assert_eq!(b.counts(), (1, 1));
        // And the one that kept up is still being fed.
        b.notify("org_a", "all", r#"{"n":"after"}"#);
        assert_eq!(next(&mut keeping_up).await.as_deref(), Some(r#"{"n":"after"}"#));
    }

    #[tokio::test]
    async fn a_dropped_subscriber_idles_rather_than_ending() {
        // Python's queue has no closed state, so a subscriber the
        // broadcaster gave up on goes on waiting rather than having its
        // response ended. Reproduced deliberately — PYTHON_BUGS #11.
        let b = broadcaster();
        let mut slow = b.subscribe("org_a", false, 10).unwrap();
        for i in 0..=QUEUE_DEPTH {
            b.notify("org_a", "all", &format!(r#"{{"n":{i}}}"#));
        }
        assert_eq!(b.counts(), (0, 0), "the slow subscriber should have been dropped");

        // Everything that was queued before the drop is still readable.
        for _ in 0..QUEUE_DEPTH {
            assert!(next(&mut slow).await.is_some());
        }
        // Then it idles forever instead of returning None.
        assert!(idle(&mut slow).await, "the stream ended instead of idling");
    }

    #[tokio::test]
    async fn the_cap_is_per_org_and_refuses_rather_than_evicting() {
        let b = broadcaster();
        let one = b.subscribe("org_a", false, 2).unwrap();
        let _two = b.subscribe("org_a", false, 2).unwrap();
        assert!(b.subscribe("org_a", false, 2).is_none());
        // The existing two kept their places — a new connection is
        // refused, it does not displace anyone.
        assert_eq!(b.counts(), (1, 2));
        // Another org has its own budget.
        let _elsewhere = b.subscribe("org_b", false, 2).unwrap();
        // And a freed slot is reusable.
        drop(one);
        assert!(b.subscribe("org_a", false, 2).is_some());
    }

    #[tokio::test]
    async fn dropping_one_leaves_the_others() {
        let b = broadcaster();
        let first = b.subscribe("org_a", false, 10).unwrap();
        let mut second = b.subscribe("org_a", false, 10).unwrap();
        drop(first);
        b.notify("org_a", "all", r#"{"n":1}"#);
        assert_eq!(next(&mut second).await.as_deref(), Some(r#"{"n":1}"#));
        assert_eq!(b.counts(), (1, 1));
    }
}
