//! Per-tenant rate limiting.
//!
//! Ported from `backend/app/core/limiter.py` and the 429 handler in
//! `main.py`. This exists because porting a route to Rust would
//! otherwise **remove** its rate limit: the Python decorators live on
//! the Python handlers, and once Rust owns a path Python never sees
//! those requests at all. The slice-2 differential found exactly that —
//! five ported routes had silently lost their limits.
//!
//! Because Rust owns a ported route exclusively, its counter does not
//! need to be shared with Python's; nothing else is counting those
//! requests. It does need to be shared between Rust *instances*, which
//! is what the Redis store is for. Without `REDIS_URL` the counters are
//! per-process and a caller round-robining across machines gets N× the
//! nominal limit — the same caveat the Python module documents about
//! itself, and it warns the same way.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, FromRequestParts, MatchedPath};
use axum::http::{request::Parts, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::app::AppState;

/// The 429 handler advertises a flat 60-second `Retry-After` whatever
/// the window is — including the hour-scoped routes, where it is plainly
/// too short. That is the Python's behaviour and its stated reasoning
/// ("our tightest rate windows are minute-scoped"), so it is reproduced
/// rather than corrected.
const RETRY_AFTER_SECONDS: u64 = 60;

/// How long a Redis round trip may take before the limiter gives up and
/// allows the request. The limiter sits in front of every segment push;
/// a Redis that hangs must cost each request this long, not forever.
/// Upstash answers in single-digit milliseconds from the same region.
const REDIS_RESPONSE_TIMEOUT: Duration = Duration::from_millis(250);

/// How long boot waits to reach Redis before falling back to in-process
/// counters. Without it an unreachable Redis held startup before the port
/// was bound — a deploy that never became healthy.
const REDIS_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// The in-process store sweeps expired buckets at most this often. It
/// used to sweep on EVERY request once it held 10k buckets, which is a
/// full-map walk per request exactly when something is spraying it.
const MEMORY_SWEEP_EVERY: Duration = Duration::from_secs(10);

/// Counter storage.
pub enum Store {
    Redis(redis::aio::ConnectionManager),
    /// Fixed-window counters held in this process only.
    Memory(Mutex<MemoryCounters>),
}

#[derive(Default)]
pub struct MemoryCounters {
    buckets: HashMap<String, (u32, Instant)>,
    last_sweep: Option<Instant>,
}

pub struct Limiter {
    store: Store,
}

impl Limiter {
    pub async fn from_env(redis_url: &str) -> Self {
        if redis_url.is_empty() {
            tracing::warn!(
                "REDIS_URL not set — rate-limit counters are per-process. They will NOT \
                 hold across multiple machines; set REDIS_URL in production to close this gap."
            );
            return Self {
                store: Store::Memory(Mutex::new(MemoryCounters::default())),
            };
        }
        match redis::Client::open(redis_url) {
            Ok(client) => match tokio::time::timeout(
                REDIS_CONNECT_TIMEOUT,
                redis::aio::ConnectionManager::new_with_config(
                    client,
                    redis::aio::ConnectionManagerConfig::new()
                        .set_connection_timeout(Some(REDIS_CONNECT_TIMEOUT))
                        .set_response_timeout(Some(REDIS_RESPONSE_TIMEOUT)),
                ),
            )
            .await
            .unwrap_or_else(|_| {
                Err(redis::RedisError::from((
                    redis::ErrorKind::Io,
                    "timed out connecting",
                )))
            }) {
                Ok(conn) => {
                    tracing::info!("rate limiter using redis storage");
                    return Self {
                        store: Store::Redis(conn),
                    };
                }
                Err(err) => tracing::error!(error = %err, "redis connect failed"),
            },
            Err(err) => tracing::error!(error = %err, "REDIS_URL is not a valid redis url"),
        }
        // Falling back rather than refusing to start: a Redis outage
        // should degrade the sharing of counters, not take the service
        // down. The limit still applies, just per-process.
        tracing::warn!("falling back to in-process rate-limit counters");
        Self {
            store: Store::Memory(Mutex::new(MemoryCounters::default())),
        }
    }

    /// Count one request against `bucket`, returning false when the
    /// caller has exceeded `limit` within `window`.
    ///
    /// Fixed window, matching slowapi's default strategy: the counter
    /// resets on a wall-clock boundary rather than sliding, so a caller
    /// can burst across a boundary. That is the behaviour the Python has
    /// and the behaviour its documented limits were chosen against.
    pub async fn check(&self, bucket: &str, limit: u32, window: Duration) -> bool {
        match &self.store {
            Store::Redis(conn) => {
                let mut conn = conn.clone();
                // INCR then EXPIRE on first write. A crash between the
                // two would leave a key without a TTL, so the EXPIRE is
                // re-issued whenever the count is at 1.
                let count: Result<u32, _> =
                    redis::cmd("INCR").arg(bucket).query_async(&mut conn).await;
                match count {
                    Ok(count) => {
                        if count == 1 {
                            let _: Result<(), _> = redis::cmd("EXPIRE")
                                .arg(bucket)
                                .arg(window.as_secs())
                                .query_async(&mut conn)
                                .await;
                        }
                        count <= limit
                    }
                    Err(err) => {
                        // Fail open. A limiter that rejects traffic when
                        // its own storage is unreachable converts a
                        // Redis blip into a full outage, which is a
                        // worse failure than briefly unlimited reads.
                        // Once a minute at error, the rest at debug. Every
                        // `error!` is a Sentry event, and this sits in
                        // front of every segment push: a Redis outage
                        // logged per request is an event per camera per
                        // second, burning the quota that the alerts
                        // which matter come out of.
                        if storage_error_is_due() {
                            tracing::error!(error = %err, "rate limiter storage unavailable; allowing");
                        } else {
                            tracing::debug!(error = %err, "rate limiter storage unavailable; allowing");
                        }
                        true
                    }
                }
            }
            Store::Memory(map) => {
                let mut counters = match map.lock() {
                    Ok(m) => m,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let now = Instant::now();
                let entry = match counters.buckets.get_mut(bucket) {
                    Some(entry) => entry,
                    None => counters
                        .buckets
                        .entry(bucket.to_string())
                        .or_insert((0, now)),
                };
                if now.duration_since(entry.1) >= window {
                    *entry = (0, now);
                }
                entry.0 += 1;
                let allowed = entry.0 <= limit;

                // Opportunistic sweep so a long-lived process does not
                // accumulate one entry per tenant per route forever —
                // throttled, so a full map is not walked per request.
                let due = counters
                    .last_sweep
                    .is_none_or(|at| now.duration_since(at) >= MEMORY_SWEEP_EVERY);
                if counters.buckets.len() > 10_000 && due {
                    counters.last_sweep = Some(now);
                    // The longest window any route uses is an hour, so
                    // nothing older can still be counting.
                    counters.buckets.retain(|_, (_, started)| {
                        now.duration_since(*started) < Duration::from_secs(3600)
                    });
                }
                allowed
            }
        }
    }
}

/// Whether a storage failure should be logged at error level now: the
/// first one, then at most one a minute.
fn storage_error_is_due() -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    // Seconds since START, plus one so that 0 can mean "never".
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = START.get_or_init(Instant::now).elapsed().as_secs() + 1;
    let last = LAST.load(Ordering::Relaxed);
    if last != 0 && now - last < 60 {
        return false;
    }
    LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

/// Whether proxy headers name the client in this process — read once
/// from `TRUST_PROXY_HEADERS` / `FLY_APP_NAME`. See
/// [`crate::config::trust_proxy_headers`].
pub fn proxy_headers_trusted() -> bool {
    static TRUSTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TRUSTED.get_or_init(|| {
        crate::config::trust_proxy_headers(
            std::env::var("TRUST_PROXY_HEADERS").ok().as_deref(),
            std::env::var("FLY_APP_NAME").ok().as_deref(),
        )
    })
}

/// The client's address: `Fly-Client-IP`, then the left-most
/// `X-Forwarded-For` entry — but only when `trust_proxy` says a proxy
/// put them there. Otherwise, and when neither is present, the TCP peer.
pub fn client_ip(headers: &HeaderMap, peer: Option<&str>, trust_proxy: bool) -> Option<String> {
    if trust_proxy {
        if let Some(ip) = headers.get("fly-client-ip").and_then(|v| v.to_str().ok()) {
            let ip = ip.trim();
            if !ip.is_empty() {
                return Some(ip.to_string());
            }
        }
        // X-Forwarded-For is a chain appended at each hop; the left-most
        // entry is the originating client.
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(first) = xff.split(',').next() {
                let first = first.trim();
                if !first.is_empty() {
                    return Some(first.to_string());
                }
            }
        }
    }
    peer.map(str::to_string)
}

/// Which bucket a request counts against.
///
/// Ported from `tenant_aware_key`. The order matters: a CameraNode gets
/// its own bucket, an authenticated user shares one per organisation,
/// and everything else falls back to the real client IP.
pub fn tenant_key(headers: &HeaderMap, peer: Option<&str>) -> String {
    tenant_key_with(headers, peer, false, proxy_headers_trusted())
}

/// [`tenant_key`], with the two decisions it reads from the process
/// made explicit.
///
/// `ip_only` skips the credential buckets. They are read *unverified*,
/// so a caller picks its own: a fresh `X-Node-API-Key`, or a token
/// naming a fresh org, on every request and no limit is ever met. Where
/// a credential is checked before anything happens that is harmless — a
/// node key is not guessable — but the local login checks a password a
/// person chose, and its limit is the only thing between that and an
/// online guessing run. That route is limited per client address alone.
pub fn tenant_key_with(
    headers: &HeaderMap,
    peer: Option<&str>,
    ip_only: bool,
    trust_proxy: bool,
) -> String {
    if ip_only {
        return client_ip(headers, peer, trust_proxy).unwrap_or_else(|| "unknown".to_string());
    }
    // CameraNodes — bucketed on a hash prefix, never the raw key.
    if let Some(node_key) = headers.get("x-node-api-key").and_then(|v| v.to_str().ok()) {
        let digest = Sha256::digest(node_key.as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        return format!("node:{}", &hex[..16]);
    }

    // End users — one bucket per org, read from the JWT *without*
    // verifying it. Safe for this purpose: forging a different org_id
    // only moves the caller into another bucket, it does not escape the
    // limit, and real verification still happens before the handler.
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(token) = auth.strip_prefix("Bearer ") {
            if let Some(org_id) = org_from_unverified_jwt(token) {
                return format!("org:{org_id}");
            }
        }
    }

    client_ip(headers, peer, trust_proxy).unwrap_or_else(|| "unknown".to_string())
}

/// Pull `org_id` out of a JWT payload without verifying the signature.
fn org_from_unverified_jwt(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64url_decode(parts[1])?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    let org_id = claims
        .get("org_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            claims
                .get("o")
                .and_then(|o| o.get("id"))
                .and_then(serde_json::Value::as_str)
        })?;
    if org_id.is_empty() {
        return None;
    }
    Some(org_id.to_string())
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for ch in input.bytes() {
        if ch == b'=' {
            break;
        }
        let val = TABLE.iter().position(|&c| c == ch)? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// The 429 the Python emits.
///
/// Deliberately **not** the `{"detail": ...}` envelope the rest of the
/// API errors use — `rate_limit_exceeded_handler` returns a flat object,
/// and integrators read `error` and `retry_after_seconds` off the top
/// level.
pub(crate) fn too_many_requests(limit: u32, window_secs: u64) -> Response {
    // slowapi renders the limit as e.g. "120 per 1 minute" or
    // "30 per 1 hour" — both measured against the running service.
    let window = if window_secs >= 3600 {
        "hour"
    } else {
        "minute"
    };
    let body = json!({
        "error": "rate_limit_exceeded",
        "message": "Too many requests. Back off and retry after the Retry-After window. \
                    Per-route limits: https://sentinel-command.com/documentation/",
        "limit": format!("{limit} per 1 {window}"),
        "retry_after_seconds": RETRY_AFTER_SECONDS,
    });
    (
        StatusCode::TOO_MANY_REQUESTS,
        [("retry-after", "60")],
        Json(body),
    )
        .into_response()
}

/// A route's rate limit: `LIMIT` requests per tenant per route within
/// `WINDOW_SECS`.
///
/// Both are const parameters so each route states its own budget at the
/// point it is registered, the way the Python decorator does. Use the
/// `PerMinute` / `PerHour` aliases rather than spelling the window out.
///
/// **Extracting this spends nothing. The handler must call
/// [`RateLimit::check`].** The extractor used to spend the slot itself,
/// as the first argument of every handler, and that disagreed with
/// Python about *which requests count*. slowapi's `@limiter.limit` wraps
/// the endpoint function, and FastAPI resolves every dependency before
/// calling it — authentication, and path, query and body validation.
/// So in Python a request refused with 401, 403 or 422 never reaches the
/// limiter, while an `HTTPException` raised inside the function has
/// already spent its slot. Measured: six member 403s against the 5/hour
/// wipe-logs route, then an admin call — 200 from Python.
///
/// Checking first let a member exhaust an organisation's budget on an
/// admin route and lock the admin out. `check()` goes exactly where the
/// decorator runs: after auth and validation, before anything the
/// handler itself raises. `tests/differential/ratelimit_order.py` holds
/// every limited route to both halves of that — refusals are free, and
/// the limit still fires, so a handler that forgets to call `check()`
/// fails there.
pub struct RateLimit<const LIMIT: u32, const WINDOW_SECS: u64, const IP_ONLY: bool = false> {
    limiter: std::sync::Arc<Limiter>,
    bucket: String,
}

impl<const LIMIT: u32, const WINDOW_SECS: u64, const IP_ONLY: bool>
    RateLimit<LIMIT, WINDOW_SECS, IP_ONLY>
{
    /// Spend one slot, or refuse with slowapi's 429.
    pub async fn check(&self) -> Result<(), crate::error::ApiError> {
        if self
            .limiter
            .check(&self.bucket, LIMIT, Duration::from_secs(WINDOW_SECS))
            .await
        {
            Ok(())
        } else {
            tracing::info!(bucket = %self.bucket, limit = LIMIT, window = WINDOW_SECS, "rate limit exceeded");
            Err(crate::error::ApiError::rate_limited(LIMIT, WINDOW_SECS))
        }
    }
}

/// `@limiter.limit("N/minute")`.
pub type PerMinute<const N: u32> = RateLimit<N, 60>;

/// `N/minute` per client address, whatever credentials the request
/// carries. For a route whose limit stands between a caller and guessing
/// a secret a person chose; see [`tenant_key_with`].
pub type PerMinuteByIp<const N: u32> = RateLimit<N, 60, true>;

/// `@limiter.limit("N/hour")`.
///
/// Worth having as its own alias: a route limited at 30/hour that is
/// ported with a minute window silently gets sixty times the budget,
/// which is why `ratelimit_parity.py` fails rather than passes on an
/// hour-scoped route it cannot account for.
pub type PerHour<const N: u32> = RateLimit<N, 3600>;

impl<const LIMIT: u32, const WINDOW_SECS: u64, const IP_ONLY: bool> FromRequestParts<AppState>
    for RateLimit<LIMIT, WINDOW_SECS, IP_ONLY>
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.0.ip().to_string());
        let tenant = tenant_key_with(
            &parts.headers,
            peer.as_deref(),
            IP_ONLY,
            proxy_headers_trusted(),
        );

        // Bucket per route as well as per tenant, matching slowapi's
        // per-endpoint limits. The matched path template is used rather
        // than the raw URI so every camera id shares one bucket.
        let route = parts
            .extensions
            .get::<MatchedPath>()
            .map(|m| m.as_str().to_string())
            .unwrap_or_else(|| parts.uri.path().to_string());

        // The window is part of the key: changing a route's window must
        // start a fresh counter, not inherit the old one's count.
        let bucket = format!("rl:{tenant}:{route}:{LIMIT}:{WINDOW_SECS}");
        Ok(RateLimit {
            limiter: state.limiter.clone(),
            bucket,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn a_node_key_gets_its_own_bucket_and_is_never_the_raw_key() {
        let key = tenant_key(&headers(&[("x-node-api-key", "super-secret")]), None);
        assert!(key.starts_with("node:"));
        assert!(!key.contains("super-secret"));
        assert_eq!(key.len(), "node:".len() + 16);
    }

    #[test]
    fn a_node_key_outranks_an_authorization_header() {
        // A CameraNode that also carries a bearer token is still a node.
        let key = tenant_key(
            &headers(&[
                ("x-node-api-key", "k"),
                ("authorization", "Bearer a.b.c"),
                ("fly-client-ip", "1.2.3.4"),
            ]),
            None,
        );
        assert!(key.starts_with("node:"));
    }

    #[test]
    fn authenticated_requests_bucket_by_org() {
        // payload = {"org_id":"org_42"}, base64url, unsigned — the
        // limiter never verifies, by design.
        let token = "x.eyJvcmdfaWQiOiJvcmdfNDIifQ.y";
        let key = tenant_key(
            &headers(&[("authorization", &format!("Bearer {token}"))]),
            None,
        );
        assert_eq!(key, "org:org_42");
    }

    #[test]
    fn the_v2_compact_claim_is_read_too() {
        // payload = {"o":{"id":"org_99"}}
        let token = "x.eyJvIjp7ImlkIjoib3JnXzk5In19.y";
        let key = tenant_key(
            &headers(&[("authorization", &format!("Bearer {token}"))]),
            None,
        );
        assert_eq!(key, "org:org_99");
    }

    #[test]
    fn an_unparseable_token_falls_back_to_the_client_ip() {
        for token in ["not-a-jwt", "a.b", "a.!!!.c", "a..c"] {
            let key = tenant_key_with(
                &headers(&[
                    ("authorization", &format!("Bearer {token}")),
                    ("fly-client-ip", "9.9.9.9"),
                ]),
                None,
                false,
                true,
            );
            assert_eq!(key, "9.9.9.9", "{token}");
        }
    }

    #[test]
    fn fly_client_ip_beats_forwarded_for() {
        // Fly strips the former from inbound requests, so it is the only
        // one of the two we can trust.
        let key = tenant_key_with(
            &headers(&[
                ("fly-client-ip", "1.1.1.1"),
                ("x-forwarded-for", "2.2.2.2, 3.3.3.3"),
            ]),
            None,
            false,
            true,
        );
        assert_eq!(key, "1.1.1.1");
    }

    #[test]
    fn forwarded_for_takes_the_left_most_entry() {
        let key = tenant_key_with(
            &headers(&[("x-forwarded-for", "2.2.2.2, 3.3.3.3")]),
            None,
            false,
            true,
        );
        assert_eq!(key, "2.2.2.2");
    }

    #[test]
    fn untrusted_proxy_headers_are_ignored_for_the_peer() {
        // Off Fly a caller can send either header; the peer is the client.
        let spoofed = headers(&[("fly-client-ip", "1.1.1.1"), ("x-forwarded-for", "2.2.2.2")]);
        assert_eq!(
            tenant_key_with(&spoofed, Some("10.0.0.1"), false, false),
            "10.0.0.1"
        );
        assert_eq!(
            client_ip(&spoofed, Some("10.0.0.1"), false).as_deref(),
            Some("10.0.0.1")
        );
    }

    #[test]
    fn an_ip_only_limit_ignores_the_credentials_a_caller_chooses() {
        // Each of these used to open a fresh bucket per request.
        let token = "x.eyJvcmdfaWQiOiJvcmdfNDIifQ.y";
        for pairs in [
            vec![("x-node-api-key", "guess-1")],
            vec![("authorization", format!("Bearer {token}").leak() as &str)],
        ] {
            assert_eq!(
                tenant_key_with(&headers(&pairs), Some("10.0.0.1"), true, false),
                "10.0.0.1",
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn with_no_headers_the_peer_address_is_the_bucket() {
        assert_eq!(tenant_key(&headers(&[]), Some("10.0.0.1")), "10.0.0.1");
        assert_eq!(tenant_key(&headers(&[]), None), "unknown");
    }

    #[tokio::test]
    async fn the_counter_allows_exactly_the_limit_then_rejects() {
        let limiter = Limiter {
            store: Store::Memory(Mutex::new(MemoryCounters::default())),
        };
        let minute = Duration::from_secs(60);
        for i in 1..=5 {
            assert!(
                limiter.check("b", 5, minute).await,
                "request {i} should be allowed"
            );
        }
        assert!(
            !limiter.check("b", 5, minute).await,
            "the 6th must be rejected"
        );
    }

    #[tokio::test]
    async fn buckets_do_not_interfere() {
        let limiter = Limiter {
            store: Store::Memory(Mutex::new(MemoryCounters::default())),
        };
        let minute = Duration::from_secs(60);
        for _ in 0..5 {
            limiter.check("tenant-a", 5, minute).await;
        }
        assert!(!limiter.check("tenant-a", 5, minute).await);
        assert!(
            limiter.check("tenant-b", 5, minute).await,
            "one loud tenant must not starve another"
        );
    }

    #[test]
    fn the_429_body_matches_the_python_handler() {
        // Flat, not the {"detail": ...} envelope the other errors use.
        let resp = too_many_requests(120, 60);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        // Retry-After stays 60 even for an hour window, matching Python.
        assert_eq!(resp.headers().get("retry-after").unwrap(), "60");
        let hourly = too_many_requests(30, 3600);
        assert_eq!(hourly.headers().get("retry-after").unwrap(), "60");
    }

    #[tokio::test]
    async fn an_hour_window_does_not_reset_after_a_minute() {
        // A 30/hour route ported with a minute window gets sixty times
        // the budget. The window is part of the bucket key so the two
        // can never share a counter either.
        let limiter = Limiter {
            store: Store::Memory(Mutex::new(MemoryCounters::default())),
        };
        let hour = Duration::from_secs(3600);
        for _ in 0..3 {
            assert!(limiter.check("hourly", 3, hour).await);
        }
        assert!(!limiter.check("hourly", 3, hour).await);
        // A different window is a different bucket.
        assert!(limiter.check("minutely", 3, Duration::from_secs(60)).await);
    }

    /// The Redis store against a real server. Skips unless
    /// `TEST_REDIS_URL` is set (e.g. `podman run -p 6379:6379 redis`),
    /// as the database tests skip without `TEST_DATABASE_URL`.
    #[tokio::test]
    async fn the_redis_store_counts_and_expires() {
        let Ok(url) = std::env::var("TEST_REDIS_URL") else {
            return;
        };
        let limiter = Limiter::from_env(&url).await;
        assert!(
            matches!(limiter.store, Store::Redis(_)),
            "could not connect to {url}"
        );
        let bucket = format!("test:{}", uuid::Uuid::new_v4());
        let window = Duration::from_secs(1);
        for _ in 0..3 {
            assert!(limiter.check(&bucket, 3, window).await);
        }
        assert!(!limiter.check(&bucket, 3, window).await);
        // The first hit set the TTL, so the window ends.
        tokio::time::sleep(Duration::from_millis(2100)).await;
        assert!(limiter.check(&bucket, 3, window).await);
    }

    #[tokio::test]
    async fn an_unreachable_redis_falls_back_to_process_counters() {
        let limiter = Limiter::from_env("redis://127.0.0.1:1").await;
        assert!(matches!(limiter.store, Store::Memory(_)));
    }
}
