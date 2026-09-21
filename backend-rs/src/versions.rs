//! What CameraNode build a node is running, and what it should be.
//!
//! Ported from `backend/app/core/versions.py` and the cache in
//! `core/release_cache.py`. Register and heartbeat both run this, and
//! `GET /api/nodes` decorates every row with it.
//!
//! **The differential cannot see the cache.** Both stacks start cold, so
//! both answer from `LATEST_NODE_VERSION` and agree for the wrong
//! reason — the exact shape `in_process_state.md` calls the worst bug
//! this project can produce. What the harness *can* check is that the
//! answer is the same when neither has fetched; everything the cache
//! itself does is held by unit tests against a stub server instead.
//!
//! Nothing here fetches on a request path. The Python's
//! `latest_node_version()` is explicitly synchronous and never does
//! I/O — a heartbeat must not wait on GitHub — so the refresh belongs
//! to a background loop and the request path reads whatever it left.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// `CAMERANODE_GH_REPO`.
const GH_REPO: &str = "SourceBox-LLC/Sentinel-CameraNode";

/// `_RELEASE_TTL_S` — long enough to stay well inside GitHub's
/// unauthenticated hourly limit with a few replicas polling, short
/// enough that a new release reaches nodes within a refresh tick.
const RELEASE_TTL: Duration = Duration::from_secs(600);

struct CachedRelease {
    payload: Value,
    fetched_at: Instant,
}

fn cache() -> &'static Mutex<Option<CachedRelease>> {
    static CACHE: OnceLock<Mutex<Option<CachedRelease>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// `_strip_leading_v`: GitHub tags are conventionally `v0.1.39`, and
/// everything else here compares plain `X.Y.Z`.
fn strip_leading_v(tag: &str) -> &str {
    tag.strip_prefix('v').or_else(|| tag.strip_prefix('V')).unwrap_or(tag)
}

/// `latest_node_version()` — the freshest tag this process has cached,
/// or the configured fallback. Never does I/O.
pub fn latest_node_version(fallback: &str) -> String {
    let guard = cache().lock().expect("release cache poisoned");
    if let Some(cached) = guard.as_ref() {
        if let Some(tag) = cached.payload.get("tag_name").and_then(Value::as_str) {
            if !tag.is_empty() {
                return strip_leading_v(tag).to_string();
            }
        }
    }
    fallback.to_string()
}

/// The whole cached release payload, for callers that need more than
/// the tag — the download route resolves an asset URL from it.
pub fn cached_release() -> Option<Value> {
    let guard = cache().lock().expect("release cache poisoned");
    guard.as_ref().map(|cached| cached.payload.clone())
}

/// `get_latest_release(force_refresh=...)`.
///
/// Returns the payload, refreshing when the cached copy has aged out.
/// A non-200 or a transport failure serves whatever is cached, stale
/// and all: on the heartbeat path, a slightly old version number beats
/// no version number.
pub async fn refresh_release(client: &reqwest::Client, force: bool) -> Option<Value> {
    if !force {
        let guard = cache().lock().expect("release cache poisoned");
        if let Some(cached) = guard.as_ref() {
            if cached.fetched_at.elapsed() < RELEASE_TTL {
                return Some(cached.payload.clone());
            }
        }
    }

    let url = format!("https://api.github.com/repos/{GH_REPO}/releases/latest");
    let response = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(5))
        .send()
        .await;

    match response {
        Ok(response) if response.status().as_u16() == 200 => match response.json::<Value>().await {
            Ok(payload) => {
                let mut guard = cache().lock().expect("release cache poisoned");
                *guard = Some(CachedRelease {
                    payload: payload.clone(),
                    fetched_at: Instant::now(),
                });
                Some(payload)
            }
            Err(err) => {
                tracing::warn!(error = %err, %url, "[ReleaseCache] Failed to parse GitHub response");
                cached_release()
            }
        },
        Ok(response) => {
            tracing::warn!(status = response.status().as_u16(), %url, "[ReleaseCache] GitHub returned non-200");
            cached_release()
        }
        Err(err) => {
            tracing::warn!(error = %err, %url, "[ReleaseCache] Failed to fetch");
            cached_release()
        }
    }
}

/// `_release_cache_refresh_loop` in `main.py`.
///
/// Sleeps first, like the others, and takes its interval from the
/// environment so the harness can push it out of the way.
pub fn spawn_refresh_loop(client: reqwest::Client) {
    let interval = std::env::var("RELEASE_CACHE_REFRESH_INTERVAL_SECONDS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(600)
        .max(1);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            refresh_release(&client, true).await;
        }
    });
}

/// `parse_version`: the leading numeric triple, and nothing else.
///
/// Anything unparseable is `(0, 0, 0)`, which sorts below every real
/// release — a malformed version is treated as ancient rather than as
/// an error. Suffixes are tolerated and ignored, so `1.2.3-rc1` is
/// `(1, 2, 3)`.
pub fn parse_version(version: Option<&str>) -> (u64, u64, u64) {
    let Some(version) = version.filter(|v| !v.is_empty()) else {
        return (0, 0, 0);
    };
    // `^\s*v?(\d+)\.(\d+)\.(\d+)` — Python's `\s` and `\d` are Unicode,
    // but a version string that reaches here came from a Cargo
    // manifest, so this stays with the regex's own ASCII reading of it
    // and the corpus test below covers what that accepts.
    let rest = version.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let rest = rest.strip_prefix('v').unwrap_or(rest);
    let mut parts = [0u64; 3];
    let mut chars = rest;
    for (index, part) in parts.iter_mut().enumerate() {
        if index > 0 {
            match chars.strip_prefix('.') {
                Some(rest) => chars = rest,
                None => return (0, 0, 0),
            }
        }
        let digits: String = chars.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return (0, 0, 0);
        }
        chars = &chars[digits.len()..];
        *part = digits.parse().unwrap_or(0);
    }
    (parts[0], parts[1], parts[2])
}

/// `format_version`.
pub fn format_version(parts: (u64, u64, u64)) -> String {
    format!("{}.{}.{}", parts.0, parts.1, parts.2)
}

/// `check_node_version`: what to tell a node about its own build.
///
/// A *missing* version is always supported, so CameraNodes old enough
/// to pre-date version reporting can still register — they are simply
/// flagged as having an update available. A present-but-unparseable one
/// is not given that benefit: it parses as `0.0.0` and is gated.
pub fn check_node_version(reported: Option<&str>, min_supported: &str, latest: &str) -> Value {
    let parsed = parse_version(reported);
    let min_parts = parse_version(Some(min_supported));
    let latest_parts = parse_version(Some(latest));

    let supported = match reported.filter(|v| !v.is_empty()) {
        Some(_) => parsed >= min_parts,
        None => true,
    };
    let update_available = if parsed < latest_parts {
        json!(format_version(latest_parts))
    } else {
        Value::Null
    };

    json!({
        "reported": reported,
        "parsed": format_version(parsed),
        "supported": supported,
        "min_supported": format_version(min_parts),
        "latest": format_version(latest_parts),
        "update_available": update_available,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_like_the_python_regex() {
        // Each of these was run through `parse_version` in the backend.
        for (input, want) in [
            (Some("1.2.3"), (1, 2, 3)),
            (Some("v1.2.3"), (1, 2, 3)),
            (Some("V1.2.3"), (0, 0, 0)),
            (Some("  1.2.3"), (1, 2, 3)),
            (Some("1.2.3-rc1"), (1, 2, 3)),
            (Some("1.2.3.4"), (1, 2, 3)),
            (Some("01.02.03"), (1, 2, 3)),
            (Some("1.2"), (0, 0, 0)),
            (Some("1.2.x"), (0, 0, 0)),
            (Some("x1.2.3"), (0, 0, 0)),
            (Some(""), (0, 0, 0)),
            (None, (0, 0, 0)),
        ] {
            assert_eq!(parse_version(input), want, "{input:?}");
        }
    }

    #[test]
    fn a_missing_version_is_supported_but_flagged() {
        let out = check_node_version(None, "0.1.0", "0.2.0");
        assert_eq!(out["supported"], json!(true));
        assert_eq!(out["reported"], Value::Null);
        assert_eq!(out["parsed"], json!("0.0.0"));
        assert_eq!(out["update_available"], json!("0.2.0"));
    }

    #[test]
    fn a_version_below_the_floor_is_not_supported() {
        let out = check_node_version(Some("0.0.9"), "0.1.0", "0.2.0");
        assert_eq!(out["supported"], json!(false));
        assert_eq!(out["update_available"], json!("0.2.0"));

        // Exactly the floor is supported — the comparison is `>=`.
        let out = check_node_version(Some("0.1.0"), "0.1.0", "0.2.0");
        assert_eq!(out["supported"], json!(true));

        // Unparseable is 0.0.0, which is below any real floor.
        let out = check_node_version(Some("garbage"), "0.1.0", "0.2.0");
        assert_eq!(out["supported"], json!(false));
        assert_eq!(out["parsed"], json!("0.0.0"));
    }

    #[test]
    fn a_current_node_has_no_update() {
        let out = check_node_version(Some("0.2.0"), "0.1.0", "0.2.0");
        assert_eq!(out["update_available"], Value::Null);
        // Newer than the latest release is still "no update".
        let out = check_node_version(Some("9.9.9"), "0.1.0", "0.2.0");
        assert_eq!(out["update_available"], Value::Null);
        assert_eq!(out["latest"], json!("0.2.0"));
    }

    #[test]
    fn the_reported_string_is_echoed_as_sent() {
        // Not the parsed form: the node should see what it told us.
        let out = check_node_version(Some("v1.2.3-rc1"), "0.1.0", "0.2.0");
        assert_eq!(out["reported"], json!("v1.2.3-rc1"));
        assert_eq!(out["parsed"], json!("1.2.3"));
    }
}
