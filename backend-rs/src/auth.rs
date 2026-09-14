//! Clerk session verification.
//!
//! There is no Rust Clerk SDK, and none is needed for the hot path. The
//! Python service's comment says it plainly — verification "is local RS256
//! most of the time", wrapped in `asyncio.to_thread` only because the
//! Python SDK is synchronous. Locally verifying an RS256 JWT against a
//! cached JWKS is the whole mechanism.
//!
//! This module currently carries only the pieces slice 0 needs. The
//! verifier itself is slice 1 and is the keystone for the migration: 75 of
//! the 100 routes are Clerk-gated, so nothing else moves until this
//! reproduces `backend/app/core/auth.py` exactly — including organisation
//! membership, the admin/viewer split, and the billing gate.

/// Derive the Clerk issuer from a publishable key.
///
/// A Clerk publishable key is `pk_test_` / `pk_live_` followed by the
/// base64 of the Frontend API host with a `$` terminator, e.g.
/// `pk_test_<base64("example-42.clerk.accounts.dev$")>`. Session tokens
/// are issued by `https://<that host>`, so the key alone is enough to know
/// what issuer to trust — no configuration and no network call.
///
/// Returns `None` for an empty or malformed key rather than guessing: a
/// wrong issuer must fail closed at verification, not silently trust the
/// wrong tenant.
pub fn issuer_from_publishable_key(key: &str) -> Option<String> {
    let encoded = key
        .strip_prefix("pk_test_")
        .or_else(|| key.strip_prefix("pk_live_"))?;
    if encoded.is_empty() {
        return None;
    }

    let decoded = base64_decode(encoded)?;
    let host = String::from_utf8(decoded).ok()?;
    let host = host.strip_suffix('$').unwrap_or(&host).trim().to_string();
    if host.is_empty() || host.contains('/') || host.contains(' ') {
        return None;
    }
    Some(format!("https://{host}"))
}

/// Minimal standard-alphabet base64 decoder, padding optional.
///
/// Clerk keys are the only base64 this crate handles, so a dependency for
/// it would be a dependency for one call site.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_the_issuer_from_a_test_key() {
        // base64("example-42.clerk.accounts.dev$")
        let key = "pk_test_ZXhhbXBsZS00Mi5jbGVyay5hY2NvdW50cy5kZXYk";
        assert_eq!(
            issuer_from_publishable_key(key).as_deref(),
            Some("https://example-42.clerk.accounts.dev")
        );
    }

    #[test]
    fn handles_live_keys_the_same_way() {
        let key = "pk_live_ZXhhbXBsZS00Mi5jbGVyay5hY2NvdW50cy5kZXYk";
        assert_eq!(
            issuer_from_publishable_key(key).as_deref(),
            Some("https://example-42.clerk.accounts.dev")
        );
    }

    #[test]
    fn tolerates_a_missing_dollar_terminator() {
        // base64("example-42.clerk.accounts.dev")
        let key = "pk_test_ZXhhbXBsZS00Mi5jbGVyay5hY2NvdW50cy5kZXY=";
        assert_eq!(
            issuer_from_publishable_key(key).as_deref(),
            Some("https://example-42.clerk.accounts.dev")
        );
    }

    #[test]
    fn fails_closed_on_anything_malformed() {
        // A wrong issuer must never be guessed — it would mean trusting
        // tokens from somewhere other than this tenant's Clerk instance.
        for key in [
            "",
            "pk_test_",
            "sk_test_abc",
            "not-a-key",
            "pk_test_!!!!",
            // decodes to text containing a slash — not a bare host
            "pk_test_aHR0cDovL2V2aWwuZXhhbXBsZS8k",
        ] {
            assert!(
                issuer_from_publishable_key(key).is_none(),
                "should reject {key:?}"
            );
        }
    }
}
