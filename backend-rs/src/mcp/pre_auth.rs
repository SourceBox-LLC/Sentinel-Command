//! The gates that run on `POST /mcp` **before** anything reads the body.
//!
//! Ported from `spa_middleware` in `main.py`, which is where these lived
//! because in Python `/mcp` was a Starlette *mount* and the middleware
//! was the only thing in front of it.
//!
//! **Both of these were missing from Rust and this module is the fix.**
//! Two separate holes, one cause:
//!
//! * The 2 MB body cap and its `Content-Length` requirement were ported
//!   into `spa::fallback` while `/mcp` was still forwarded to Python, so
//!   every MCP request passed through them. Registering `POST /mcp/` as
//!   a real route in the rmcp slice moved the endpoint *ahead* of the
//!   fallback, and the gate silently stopped applying to the only path
//!   it was written for. Nothing failed — the endpoint kept working,
//!   uncapped.
//! * The 600/minute pre-auth rate limit was never ported at all.
//!   `spa::json_error` has a `429` branch that sets `Retry-After`, which
//!   nothing called: the intent survived the slice and the check did
//!   not.
//!
//! Why pre-auth gates on this route specifically, from the Python's own
//! reasoning: `/mcp` is reachable without a credential — the bearer is
//! checked inside the tool call, after the transport has parsed a
//! JSON-RPC envelope — so an unauthenticated client can otherwise make
//! the process allocate an arbitrary body, 600+ times a minute, on a
//! 1 GB machine whose memory is already committed to the segment cache.
//! Everything else on the service is behind `PerMinute`/`PerHour` *and*
//! an auth extractor.
//!
//! The limiter here is **in-process**, unlike `crate::ratelimit`, which
//! uses Redis when `REDIS_URL` is set. That is not an oversight: the
//! Python's was a module-level dict too, so a multi-machine deployment
//! has always allowed N × 600. Matching it keeps the behaviour the same;
//! moving it to Redis would be a change to argue for on its own, and the
//! backstop it provides is per-process memory, which is per-process by
//! nature.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use axum::extract::{ConnectInfo, Request};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;

/// `_MCP_PRE_AUTH_LIMIT_PER_MINUTE`.
pub const LIMIT_PER_MINUTE: usize = 600;

/// The cap `main.py` applied before FastMCP buffered anything. 2 MB
/// dwarfs any legitimate tool call — text arguments are capped far lower
/// inside the tools themselves.
pub const MAX_BODY_BYTES: u64 = 2 * 1024 * 1024;

/// A sliding-window counter per tenant, swept at most once a window.
struct Buckets {
    /// Timestamps, in monotonic seconds, per tenant key.
    per_tenant: HashMap<String, Vec<f64>>,
    /// When the key sweep last ran.
    last_sweep: f64,
}

static BUCKETS: LazyLock<Mutex<Buckets>> = LazyLock::new(|| {
    Mutex::new(Buckets { per_tenant: HashMap::new(), last_sweep: 0.0 })
});

/// Process start, so the clock below is monotonic like `time.monotonic`.
static START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

fn monotonic() -> f64 {
    START.elapsed().as_secs_f64()
}

/// Whether this request is within the window. `false` means refuse.
///
/// The whole drop-then-append happens under one lock, as the Python's
/// docstring insisted, so two concurrent requests cannot both see a
/// bucket with one slot left.
pub fn check_rate(tenant: &str, now: f64) -> bool {
    check_rate_in(&BUCKETS, tenant, now)
}

fn check_rate_in(buckets: &Mutex<Buckets>, tenant: &str, now: f64) -> bool {
    let cutoff = now - 60.0;
    let mut guard = match buckets.lock() {
        Ok(guard) => guard,
        // A panic elsewhere while holding this lock must not turn into a
        // refusal of every subsequent request: the gate is a backstop,
        // and failing it closed would take the endpoint down.
        Err(poisoned) => poisoned.into_inner(),
    };

    // Bound the KEY count, not just the entries per key. The key is
    // attacker-controlled — an IP or a bearer — so a client spraying
    // addresses adds a map entry per key forever. Time-gated so the O(n)
    // sweep runs at most once a window.
    if now - guard.last_sweep > 60.0 {
        guard.last_sweep = now;
        guard
            .per_tenant
            .retain(|_, hits| hits.last().is_some_and(|last| *last > cutoff));
    }

    let hits = guard.per_tenant.entry(tenant.to_string()).or_default();
    // In place, so a noisy tenant's own bucket does not grow unbounded
    // either.
    hits.retain(|hit| *hit > cutoff);
    if hits.len() >= LIMIT_PER_MINUTE {
        return false;
    }
    hits.push(now);
    true
}

/// The gate, in the Python's order: rate, then `Content-Length`.
///
/// Returns the refusal, or `None` to continue. Split out from the layer
/// so `spa::fallback` — which still owns every `/mcp…` path that is not
/// one of the two real routes — applies exactly the same checks.
pub fn gate(headers: &HeaderMap, peer: Option<&str>) -> Option<Response> {
    let tenant = crate::ratelimit::tenant_key(headers, peer);
    if !check_rate(&tenant, monotonic()) {
        return Some(crate::spa::json_error(
            429,
            "Too many requests. Slow down and retry shortly.",
        ));
    }

    // A header-only check is bypassable with `Transfer-Encoding:
    // chunked` — no Content-Length at all — so the header is required
    // outright. Every legitimate client (httpx, the claude.ai connector,
    // the MCP SDKs) sends one for a JSON-RPC body.
    let Some(raw) = headers.get(axum::http::header::CONTENT_LENGTH) else {
        return Some(crate::spa::json_error(411, "Content-Length required."));
    };
    // Python: `int(content_length)` then `> cap`, with `ValueError`
    // caught as a 400. A value too large for u64 is a ValueError in
    // neither language, so it is capped rather than rejected — which is
    // the same answer, 413, by a different route.
    let Ok(text) = raw.to_str() else {
        return Some(crate::spa::json_error(400, "Invalid Content-Length."));
    };
    match text.trim().parse::<u64>() {
        Ok(length) if length > MAX_BODY_BYTES => {
            Some(crate::spa::json_error(413, "Request body too large (max 2 MB)."))
        }
        Ok(_) => None,
        Err(_) => Some(crate::spa::json_error(400, "Invalid Content-Length.")),
    }
}

/// The route layer for `POST /mcp` and `POST /mcp/`.
///
/// A `route_layer` rather than a global one, because these checks are
/// specific to the one path that is reachable pre-auth — and because a
/// `Content-Length` requirement applied service-wide would reject the
/// chunked uploads CameraNode is entitled to send.
pub async fn layer(request: Request, next: Next) -> Response {
    // GET /mcp is the dashboard page and carries no body; Python gated
    // only POST.
    if request.method() == axum::http::Method::POST {
        let peer = request
            .extensions()
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.0.ip().to_string());
        if let Some(refusal) = gate(request.headers(), peer.as_deref()) {
            return refusal;
        }
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    fn fresh() -> Mutex<Buckets> {
        Mutex::new(Buckets { per_tenant: HashMap::new(), last_sweep: 0.0 })
    }

    /// The window admits exactly the cap and refuses the next one.
    #[test]
    fn the_cap_is_inclusive_and_then_refuses() {
        let buckets = fresh();
        for i in 0..LIMIT_PER_MINUTE {
            assert!(check_rate_in(&buckets, "ip:1.2.3.4", 100.0), "refused at {i}");
        }
        assert!(!check_rate_in(&buckets, "ip:1.2.3.4", 100.0));
        // A different tenant is unaffected — this is per-tenant, not
        // global, so one noisy client cannot deny the rest.
        assert!(check_rate_in(&buckets, "ip:5.6.7.8", 100.0));
    }

    /// It slides: hits older than the window stop counting.
    #[test]
    fn the_window_slides() {
        let buckets = fresh();
        for _ in 0..LIMIT_PER_MINUTE {
            assert!(check_rate_in(&buckets, "t", 100.0));
        }
        assert!(!check_rate_in(&buckets, "t", 100.0));
        // A hair under the window and the old hits still count.
        assert!(!check_rate_in(&buckets, "t", 159.9));
        // The cutoff is `hit > now - 60`, so a hit exactly 60s old is
        // dropped — which is the boundary the Python's list
        // comprehension draws, and the reason to pin it.
        assert!(check_rate_in(&buckets, "t", 160.0));
    }

    /// The key sweep is what keeps an IP-spraying client from growing the
    /// map without bound, and it must not evict a live bucket.
    #[test]
    fn aged_out_keys_are_swept_but_live_ones_survive() {
        let buckets = fresh();
        for i in 0..1_000 {
            assert!(check_rate_in(&buckets, &format!("ip:{i}"), 100.0));
        }
        assert_eq!(buckets.lock().unwrap().per_tenant.len(), 1_000);

        // A live hit just before the sweep, then a request a window on.
        assert!(check_rate_in(&buckets, "live", 200.0));
        assert!(check_rate_in(&buckets, "newcomer", 220.0));
        let remaining = buckets.lock().unwrap().per_tenant.len();
        assert_eq!(
            remaining, 2,
            "the 1,000 aged-out keys should be gone and both live ones kept"
        );
    }

    /// The refusals, in the Python's order and with its bodies.
    #[test]
    fn the_gate_refuses_in_order() {
        // No Content-Length at all — 411, not a pass.
        let response = gate(&headers(&[("fly-client-ip", "10.0.0.1")]), None).unwrap();
        assert_eq!(response.status(), 411);

        let response = gate(
            &headers(&[("fly-client-ip", "10.0.0.2"), ("content-length", "9999999")]),
            None,
        )
        .unwrap();
        assert_eq!(response.status(), 413);

        let response = gate(
            &headers(&[("fly-client-ip", "10.0.0.3"), ("content-length", "not-a-number")]),
            None,
        )
        .unwrap();
        assert_eq!(response.status(), 400);

        // Exactly at the cap passes: `>` not `>=`.
        assert!(gate(
            &headers(&[
                ("fly-client-ip", "10.0.0.4"),
                ("content-length", &MAX_BODY_BYTES.to_string()),
            ]),
            None,
        )
        .is_none());
    }

    /// The tenant key is the shared one, so an MCP bearer buckets by org
    /// rather than by IP — which is what stops one org's traffic from
    /// spending another's budget behind a shared NAT.
    #[test]
    fn the_bucket_is_the_tenant_not_the_connection() {
        let node = crate::ratelimit::tenant_key(&headers(&[("x-node-api-key", "k")]), None);
        let ip = crate::ratelimit::tenant_key(&headers(&[("fly-client-ip", "1.1.1.1")]), None);
        assert_ne!(node, ip);
        assert_eq!(ip, "1.1.1.1");
    }
}
