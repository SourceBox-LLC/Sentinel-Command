//! Cached JWKS fetching.
//!
//! Clerk signs session tokens with RS256 and publishes the public keys at
//! `{issuer}/.well-known/jwks.json`. Verification is then purely local —
//! the only network call is refreshing this cache.
//!
//! The Python SDK refreshes every 5 minutes, and a comment in
//! `app/core/auth.py` records what that cost: the refresh was a *sync*
//! fetch with a ten-deep retry ladder, and inline on the event loop it
//! froze every tenant's requests during a Clerk blip. That is the failure
//! this module is shaped to avoid.
//!
//! Two rules follow from it:
//!
//! * A refresh never blocks a request that could be served without one.
//!   An unknown `kid` is the only thing that forces an immediate fetch,
//!   because that genuinely cannot be verified from what we hold.
//! * A stale cache outlives a failed refresh. If Clerk is unreachable,
//!   keys we already have keep working; dropping them would turn a Clerk
//!   outage into a total outage here.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::DecodingKey;
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};

/// Matches the Python SDK's refresh interval.
const REFRESH_AFTER: Duration = Duration::from_secs(300);

/// How long an unknown `kid` is allowed to force a fresh fetch. Without
/// this, a flood of tokens bearing garbage `kid`s would be a free way to
/// make this service hammer Clerk.
const MIN_FORCED_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

/// Only RSA keys are modelled: Clerk signs with RS256. A key of any other
/// type is skipped rather than rejected, so an instance that also
/// publishes, say, an EdDSA key does not break RSA verification.
#[derive(Debug, Deserialize)]
struct Jwk {
    kid: String,
    #[serde(default)]
    kty: String,
    #[serde(default)]
    alg: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

struct Cached {
    keys: HashMap<String, Arc<DecodingKey>>,
    fetched_at: Instant,
}

pub struct JwksCache {
    inner: Arc<Inner>,
}

struct Inner {
    url: String,
    http: reqwest::Client,
    cached: RwLock<Option<Cached>>,
    /// Held across a fetch so a burst of concurrent misses produces one
    /// request to Clerk, not one per caller.
    fetch_lock: Mutex<Instant>,
}

#[derive(Debug, thiserror::Error)]
pub enum JwksError {
    #[error("no signing key for kid {0}")]
    UnknownKey(String),
    #[error("jwks fetch failed: {0}")]
    Fetch(String),
    /// Refused without trying: a fetch ran less than
    /// `MIN_FORCED_REFRESH_INTERVAL` ago.
    #[error("jwks fetch failed: rate limited")]
    RateLimited,
}

impl JwksCache {
    pub fn new(issuer: &str, http: reqwest::Client) -> Self {
        Self {
            inner: Arc::new(Inner {
                url: format!("{}/.well-known/jwks.json", issuer.trim_end_matches('/')),
                http,
                cached: RwLock::new(None),
                // Far enough in the past that the first forced refresh is
                // never rate-limited. `checked_sub` because `Instant` is
                // monotonic from boot on Linux, so plain subtraction panics
                // if the process starts within 10s of boot — which is
                // exactly what happens on a Fly machine cold start.
                fetch_lock: Mutex::new(
                    Instant::now()
                        .checked_sub(MIN_FORCED_REFRESH_INTERVAL)
                        .unwrap_or_else(Instant::now),
                ),
            }),
        }
    }

    /// The decoding key for `kid`.
    ///
    /// Only an UNKNOWN `kid` waits on Clerk. A known key whose set has
    /// aged past `REFRESH_AFTER` is returned at once and the refresh runs
    /// behind it — the rule this module's header states. It used to run
    /// inline: every five minutes, the next request (and every request
    /// queued behind it on the fetch lock) waited on a round trip to
    /// Clerk, up to its 5 s timeout when Clerk was slow.
    pub async fn key_for(&self, kid: &str) -> Result<Arc<DecodingKey>, JwksError> {
        let held = {
            let cached = self.inner.cached.read().await;
            cached.as_ref().and_then(|cached| {
                cached
                    .keys
                    .get(kid)
                    .map(|key| (Arc::clone(key), cached.fetched_at.elapsed() < REFRESH_AFTER))
            })
        };
        match held {
            Some((key, true)) => return Ok(key),
            Some((key, false)) => {
                self.refresh_in_background();
                return Ok(key);
            }
            None => {}
        }

        if let Err(err) = self.inner.refresh().await {
            // Another task may have filled the key in while this one was
            // refused (a refresh in flight, or the forced-refresh limit).
            if let Some(key) = self.inner.held(kid).await {
                return Ok(key);
            }
            return Err(err);
        }
        self.inner
            .held(kid)
            .await
            .ok_or_else(|| JwksError::UnknownKey(kid.to_string()))
    }

    /// Start a refresh unless one is already running. Its failure only
    /// logs: the keys already held keep working, which is the point — a
    /// Clerk outage must not sign every tenant out.
    /// Fetch the key set before any request needs it, retrying.
    ///
    /// Called once at start-up. Without it the first signed-in request
    /// on a fresh machine pays for the fetch, and if that fetch fails
    /// (networking still coming up) the forced-refresh rate limit then
    /// refuses every request for the next ten seconds. Retries are
    /// spaced past that limit; after the last one, requests fetch on
    /// demand as before.
    pub async fn warm(&self) {
        const ATTEMPTS: u32 = 6;
        for attempt in 1..=ATTEMPTS {
            match self.inner.refresh().await {
                Ok(()) => {
                    tracing::info!(attempt, "jwks fetched at start-up");
                    return;
                }
                Err(err) => {
                    tracing::warn!(attempt, error = %err, "jwks start-up fetch failed; retrying");
                }
            }
            tokio::time::sleep(MIN_FORCED_REFRESH_INTERVAL + Duration::from_secs(1)).await;
        }
        tracing::error!(
            "jwks could not be fetched at start-up; sign-in checks will retry on demand"
        );
    }

    fn refresh_in_background(&self) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            if inner.fetch_lock.try_lock().is_err() {
                return; // someone is already fetching
            }
            match inner.refresh().await {
                Ok(()) | Err(JwksError::RateLimited) => {}
                Err(err) => {
                    tracing::warn!(error = %err, "jwks refresh failed; serving the stale key set");
                }
            }
        });
    }
}

impl Inner {
    async fn held(&self, kid: &str) -> Option<Arc<DecodingKey>> {
        self.cached
            .read()
            .await
            .as_ref()
            .and_then(|c| c.keys.get(kid).map(Arc::clone))
    }

    async fn refresh(&self) -> Result<(), JwksError> {
        let mut last_fetch = self.fetch_lock.lock().await;

        // Another task may have refreshed while this one waited on the
        // lock; if the result is fresh, take it rather than fetch again.
        if let Some(cached) = self.cached.read().await.as_ref() {
            if cached.fetched_at.elapsed() < MIN_FORCED_REFRESH_INTERVAL {
                return Ok(());
            }
        }
        if last_fetch.elapsed() < MIN_FORCED_REFRESH_INTERVAL {
            return Err(JwksError::RateLimited);
        }
        *last_fetch = Instant::now();

        let resp = self
            .http
            .get(&self.url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| JwksError::Fetch(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(JwksError::Fetch(format!("status {}", resp.status())));
        }

        let set: JwkSet = resp
            .json()
            .await
            .map_err(|e| JwksError::Fetch(e.to_string()))?;

        let mut keys = HashMap::new();
        for jwk in set.keys {
            if jwk.kty != "RSA" {
                continue;
            }
            if let Some(alg) = jwk.alg.as_deref() {
                if alg != "RS256" {
                    continue;
                }
            }
            let (Some(n), Some(e)) = (jwk.n.as_deref(), jwk.e.as_deref()) else {
                continue;
            };
            match DecodingKey::from_rsa_components(n, e) {
                Ok(key) => {
                    keys.insert(jwk.kid, Arc::new(key));
                }
                Err(err) => {
                    tracing::warn!(kid = %jwk.kid, error = %err, "unusable jwk, skipped");
                }
            }
        }

        if keys.is_empty() {
            return Err(JwksError::Fetch("jwks contained no usable RSA keys".into()));
        }

        tracing::debug!(count = keys.len(), "jwks refreshed");
        *self.cached.write().await = Some(Cached {
            keys,
            fetched_at: Instant::now(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stale key set must not make a request wait on Clerk. The server
    /// here accepts the connection and never answers — the slow-Clerk
    /// case — and the held key still has to come back at once.
    #[tokio::test]
    async fn a_held_key_is_served_at_once_when_the_set_is_stale() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket); // never answer
            }
        });

        let cache = JwksCache::new(&format!("http://{addr}"), reqwest::Client::new());
        let stale = Instant::now()
            .checked_sub(REFRESH_AFTER + Duration::from_secs(1))
            .expect("the test machine has been up longer than five minutes");
        *cache.inner.cached.write().await = Some(Cached {
            keys: HashMap::from([(
                "kid-1".to_string(),
                Arc::new(DecodingKey::from_secret(b"placeholder")),
            )]),
            fetched_at: stale,
        });

        let started = Instant::now();
        cache.key_for("kid-1").await.expect("the held key");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "waited {:?} on a refresh that should run behind the request",
            started.elapsed()
        );
    }
}
