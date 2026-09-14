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
}

impl JwksCache {
    pub fn new(issuer: &str, http: reqwest::Client) -> Self {
        Self {
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
        }
    }

    /// The decoding key for `kid`, fetching only if it is not already
    /// held or the cache has aged out.
    pub async fn key_for(&self, kid: &str) -> Result<Arc<DecodingKey>, JwksError> {
        if let Some(cached) = self.cached.read().await.as_ref() {
            if let Some(key) = cached.keys.get(kid) {
                if cached.fetched_at.elapsed() < REFRESH_AFTER {
                    return Ok(Arc::clone(key));
                }
            }
        }

        // Either the key is unknown or the cache is stale. Both are
        // handled the same way — try a refresh, then look again.
        match self.refresh().await {
            Ok(()) => {}
            Err(err) => {
                // A failed refresh must not invalidate keys we hold: a
                // Clerk outage would otherwise sign every tenant out.
                if let Some(cached) = self.cached.read().await.as_ref() {
                    if let Some(key) = cached.keys.get(kid) {
                        tracing::warn!(
                            error = %err,
                            "jwks refresh failed; serving the stale key set"
                        );
                        return Ok(Arc::clone(key));
                    }
                }
                return Err(err);
            }
        }

        self.cached
            .read()
            .await
            .as_ref()
            .and_then(|c| c.keys.get(kid).map(Arc::clone))
            .ok_or_else(|| JwksError::UnknownKey(kid.to_string()))
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
            return Err(JwksError::Fetch("rate limited".into()));
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
