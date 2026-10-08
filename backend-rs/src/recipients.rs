//! Who an email goes to.
//!
//! Ported from `backend/app/core/recipients.py`. A notification carries
//! an audience — `all` or `admin` — and this turns that plus an org id
//! into addresses, by asking Clerk for the org's memberships. There is
//! no local user table; Clerk is the source of truth.
//!
//! **An empty list and a failed lookup are different things, and the
//! difference is the whole design.** The enqueue path writes no outbox
//! rows when there are no recipients, so caching a failure would turn
//! one Clerk hiccup into five minutes of silently dropped alert mail —
//! with no retry, because the outbox *is* the retry and it was never
//! reached. A genuinely empty membership list is cached; a failure is
//! not, and the next notification tries again immediately.
//!
//! Suppression is not applied here. This module's job ends at "here are
//! the addresses according to Clerk"; the worker drops suppressed ones
//! before it sends.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

/// `_CACHE_TTL_SECONDS`. Long enough that a flapping camera does not
/// burn a Clerk call per transition, short enough that a member removed
/// from an org stops being emailed within five minutes — fine for
/// security-alert latency.
const CACHE_TTL: Duration = Duration::from_secs(300);

/// `_ADMIN_ROLES`. Clerk's defaults for org membership; broaden here if
/// custom roles are ever turned on.
const ADMIN_ROLES: [&str; 2] = ["org:admin", "admin"];

/// Clerk's maximum page size, and the cap the Python logs when it is
/// reached. Pagination is not implemented on either side — an org with
/// more than this many members would not have them all emailed.
const PAGE_LIMIT: usize = 100;

/// Keyed on `(org_id, audience)`, holding the expiry and the addresses
/// — the same shape as the Python's module-level dict.
type Cache = HashMap<(String, String), (Instant, Vec<String>)>;

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn with_cache<T>(f: impl FnOnce(&mut Cache) -> T) -> T {
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(Cache::default))
}

/// `_clear_cache`. Test-facing, and used by the differential harness
/// between cases so one case's membership never answers another's.
pub fn clear_cache() {
    with_cache(|cache| cache.clear());
}

/// `invalidate_org`. The Clerk membership webhooks call this so an
/// added or removed member takes effect at once rather than at the TTL.
pub fn invalidate_org(org_id: &str) {
    with_cache(|cache| cache.retain(|(org, _), _| org != org_id));
}

/// Where the addresses come from, so a caller can hand over the local
/// admin instead of a Clerk client without this module reading config.
pub struct Lookup<'a> {
    pub client: &'a reqwest::Client,
    pub clerk_base_url: &'a str,
    pub clerk_secret: &'a str,
    /// `settings.is_local_auth()`: no Clerk to ask, and exactly one
    /// possible recipient for either audience.
    pub local_admin_email: Option<&'a str>,
}

/// `get_recipient_emails(org_id, audience)`.
///
/// Deduplicated, in Clerk's own order, and empty on any failure.
pub async fn recipient_emails(lookup: &Lookup<'_>, org_id: &str, audience: &str) -> Vec<String> {
    // An unknown audience falls through to `all` rather than dropping
    // the mail on the floor.
    let audience = if audience == "admin" { "admin" } else { "all" };

    if let Some(admin) = lookup.local_admin_email {
        return if admin.is_empty() {
            Vec::new()
        } else {
            vec![admin.to_string()]
        };
    }

    let key = (org_id.to_string(), audience.to_string());
    let now = Instant::now();
    let cached = with_cache(|cache| {
        cache
            .get(&key)
            .and_then(|(expires_at, addrs)| (*expires_at > now).then(|| addrs.clone()))
    });
    if let Some(addrs) = cached {
        return addrs;
    }

    // `None` here is a failed fetch, `Some(vec![])` an org Clerk says
    // has no matching members. Only the second is cacheable.
    let Some(addrs) = fetch_from_clerk(lookup, org_id, audience).await else {
        return Vec::new();
    };
    with_cache(|cache| {
        cache.insert(key, (Instant::now() + CACHE_TTL, addrs.clone()));
    });
    addrs
}

/// `_fetch_from_clerk`. `None` on a failure, so the caller knows not to
/// cache it.
async fn fetch_from_clerk(
    lookup: &Lookup<'_>,
    org_id: &str,
    audience: &str,
) -> Option<Vec<String>> {
    let Some(url) = crate::clerk_api::url(
        lookup.clerk_base_url,
        &["organizations", org_id, "memberships"],
    ) else {
        tracing::warn!(org_id, "[Recipients] could not build the Clerk URL");
        return None;
    };

    let response = client_get(lookup, url, org_id, audience).await?;
    let body: Value = match response.json().await {
        Ok(body) => body,
        Err(err) => {
            tracing::warn!(error = %err, org_id, audience, "[Recipients] Clerk list failed");
            return None;
        }
    };

    let members = match body.get("data") {
        Some(Value::Array(members)) => members.clone(),
        _ => Vec::new(),
    };
    if members.is_empty() {
        tracing::info!(org_id, "[Recipients] org has zero memberships per Clerk");
        return Some(Vec::new());
    }
    if members.len() >= PAGE_LIMIT {
        tracing::info!(
            org_id,
            "[Recipients] org hit the 100-member page cap — additional members \
             will not receive emails until pagination ships"
        );
    }

    let mut addrs = Vec::new();
    let mut seen = HashSet::new();
    for member in &members {
        if audience == "admin" {
            let role = member.get("role").and_then(Value::as_str).unwrap_or("");
            if !ADMIN_ROLES.contains(&role) {
                continue;
            }
        }
        let Some(addr) = extract_email(member) else {
            continue;
        };
        // Dedup folded to lower case so `Alice@Example.com` and
        // `alice@example.com` are one recipient — but the address that
        // goes out is the one Clerk gave, in its own casing.
        if seen.insert(addr.to_lowercase()) {
            addrs.push(addr.to_string());
        }
    }
    Some(addrs)
}

async fn client_get(
    lookup: &Lookup<'_>,
    url: reqwest::Url,
    org_id: &str,
    audience: &str,
) -> Option<reqwest::Response> {
    let response = lookup
        .client
        .get(url)
        .query(&[("limit", PAGE_LIMIT.to_string())])
        .bearer_auth(lookup.clerk_secret)
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => Some(response),
        Ok(response) => {
            tracing::warn!(
                status = %response.status(), org_id, audience,
                "[Recipients] Clerk list failed"
            );
            None
        }
        Err(err) => {
            tracing::warn!(error = %err, org_id, audience, "[Recipients] Clerk list failed");
            None
        }
    }
}

/// `_extract_email`. `public_user_data.identifier` is the user's
/// primary identifier, which for an email-auth user is the address.
/// A username or a phone number is not one, and is skipped.
fn extract_email(membership: &Value) -> Option<&str> {
    membership
        .get("public_user_data")?
        .get("identifier")?
        .as_str()
        .filter(|identifier| identifier.contains('@'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn members() -> Value {
        json!([
            {"role": "org:admin", "public_user_data": {"identifier": "Admin@Example.com"}},
            {"role": "org:member", "public_user_data": {"identifier": "member@example.com"}},
            // The same address in another casing is one recipient.
            {"role": "admin", "public_user_data": {"identifier": "admin@example.com"}},
            // A username identifier is not an address.
            {"role": "org:admin", "public_user_data": {"identifier": "just-a-username"}},
            // No public data at all.
            {"role": "org:admin"},
        ])
    }

    /// `_fetch_from_clerk`'s filtering, without the HTTP around it.
    fn filter(audience: &str) -> Vec<String> {
        let members = members();
        let mut addrs = Vec::new();
        let mut seen = HashSet::new();
        for member in members.as_array().unwrap() {
            if audience == "admin" {
                let role = member.get("role").and_then(Value::as_str).unwrap_or("");
                if !ADMIN_ROLES.contains(&role) {
                    continue;
                }
            }
            let Some(addr) = extract_email(member) else {
                continue;
            };
            if seen.insert(addr.to_lowercase()) {
                addrs.push(addr.to_string());
            }
        }
        addrs
    }

    #[test]
    fn the_admin_audience_keeps_only_admin_roles() {
        // Both spellings of the role count, and the first casing wins.
        assert_eq!(filter("admin"), vec!["Admin@Example.com"]);
        assert_eq!(
            filter("all"),
            vec!["Admin@Example.com", "member@example.com"]
        );
    }

    #[test]
    fn a_non_email_identifier_is_skipped() {
        assert_eq!(
            extract_email(&json!({"public_user_data": {"identifier": "nobody"}})),
            None
        );
        assert_eq!(extract_email(&json!({"public_user_data": {}})), None);
        assert_eq!(extract_email(&json!({})), None);
        assert_eq!(
            extract_email(&json!({"public_user_data": {"identifier": "a@b"}})),
            Some("a@b")
        );
    }

    #[tokio::test]
    async fn the_local_admin_is_the_only_recipient_when_self_hosted() {
        let client = reqwest::Client::new();
        let lookup = Lookup {
            client: &client,
            // Deliberately unreachable: self-hosted must not call out.
            clerk_base_url: "http://127.0.0.1:1/v1",
            clerk_secret: "",
            local_admin_email: Some("admin@self.host"),
        };
        for audience in ["all", "admin", "nonsense"] {
            assert_eq!(
                recipient_emails(&lookup, "self-host", audience).await,
                vec!["admin@self.host".to_string()],
                "{audience}"
            );
        }

        // An unset address is no recipient, not an empty one.
        let lookup = Lookup {
            local_admin_email: Some(""),
            ..lookup
        };
        assert!(recipient_emails(&lookup, "self-host", "all")
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn a_failed_lookup_is_not_cached() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let lookup = Lookup {
            client: &client,
            clerk_base_url: "http://127.0.0.1:1/v1",
            clerk_secret: "sk_test",
            local_admin_email: None,
        };
        assert!(recipient_emails(&lookup, "org_x", "all").await.is_empty());
        // Nothing was written, so the next notification tries again
        // rather than being answered from a cached failure.
        // Counted for this org only: the cache is process-wide, and other
        // tests run alongside this one.
        let cached = with_cache(|cache| cache.keys().filter(|(org, _)| org == "org_x").count());
        assert_eq!(cached, 0);
    }

    #[test]
    fn invalidating_one_org_leaves_the_others() {
        // Org names of its own, and only those inspected: the cache is
        // process-wide and other tests use it concurrently. Comparing the
        // whole key set (and clearing the whole cache) made this flaky.
        let soon = Instant::now() + CACHE_TTL;
        with_cache(|cache| {
            cache.insert(
                ("inv_org_a".into(), "all".into()),
                (soon, vec!["a@x".into()]),
            );
            cache.insert(
                ("inv_org_a".into(), "admin".into()),
                (soon, vec!["a@x".into()]),
            );
            cache.insert(
                ("inv_org_b".into(), "all".into()),
                (soon, vec!["b@x".into()]),
            );
        });
        invalidate_org("inv_org_a");
        // Both audiences of the named org go; the other org stays.
        let mut keys = with_cache(|cache| {
            cache
                .keys()
                .filter(|(org, _)| org.starts_with("inv_org_"))
                .cloned()
                .collect::<Vec<_>>()
        });
        keys.sort();
        assert_eq!(keys, vec![("inv_org_b".to_string(), "all".to_string())]);
        invalidate_org("inv_org_b");
    }
}
