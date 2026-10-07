//! Response headers every Python response carries, and every ported
//! route was losing.
//!
//! `main.py` stamps two things onto every response through middleware:
//! a request id, and a set of security headers. A ported route leaves
//! both behind — the Rust tier answered with `content-type`,
//! `content-length` and `date`, and nothing else.
//!
//! The security set is not decoration. `_apply_security_headers` carries
//! a comment about a previous regression where the SPA document shipped
//! without them and "the one response where frame-ancestors actually
//! matters (the document) was the one being skipped, leaving the
//! dashboard clickjackable". Porting routes reintroduced exactly that
//! shape of gap.

use axum::extract::Request;
use axum::http::{header::HeaderName, HeaderValue, Response};
use axum::middleware::Next;

/// Verbatim from `_apply_security_headers`.
const SECURITY_HEADERS: [(&str, &str); 4] = [
    ("x-content-type-options", "nosniff"),
    ("x-frame-options", "DENY"),
    ("referrer-policy", "strict-origin-when-cross-origin"),
    (
        "permissions-policy",
        "camera=(), microphone=(), geolocation=()",
    ),
];

const HSTS: &str = "max-age=63072000; includeSubDomains";

/// The Content-Security-Policy every response carries, set once at
/// start-up by [`configure_csp`]. Unset (unit tests that never build the
/// router), no policy is sent.
static CSP: std::sync::OnceLock<HeaderValue> = std::sync::OnceLock::new();

/// Build the dashboard's Content-Security-Policy.
///
/// Scripts run only from this origin and from the few services the
/// dashboard genuinely loads code from. That is what makes an injected
/// `<script>` inert, which is the point of the policy. Each outside
/// origin is here for a reason:
///
/// - `clerk_frontend` (Clerk mode): Clerk's sign-in script and its API,
///   derived from the publishable key, so it follows a key swap.
/// - `challenges.cloudflare.com`: the bot check on Clerk's sign-up form.
/// - `js.stripe.com`, `api.stripe.com`, `hooks.stripe.com`: card entry
///   in Clerk's billing screens.
/// - `img.clerk.com`: profile photos. `clerk-telemetry.com`: Clerk's
///   development-instance telemetry.
/// - Google Fonts, for the dashboard's typefaces.
///
/// Styles allow `'unsafe-inline'` because Clerk injects its own `<style>`
/// elements. `blob:` covers live video (HLS.js plays through a
/// MediaSource object URL) and downloaded snapshots. A self-hosted
/// install (`clerk_frontend` is `None`) gets the same policy without
/// Clerk, Cloudflare and Stripe.
pub fn content_security_policy(clerk_frontend: Option<&str>) -> String {
    let clerk = clerk_frontend.is_some();
    let fapi = clerk_frontend.map(|f| format!(" {f}")).unwrap_or_default();
    let (clerk_script, clerk_connect, clerk_img, stripe_script, stripe_connect) = if clerk {
        (
            format!("{fapi} https://challenges.cloudflare.com"),
            format!("{fapi} https://clerk-telemetry.com"),
            " https://img.clerk.com",
            " https://js.stripe.com https://*.js.stripe.com",
            " https://api.stripe.com",
        )
    } else {
        (String::new(), String::new(), "", "", "")
    };
    let frame_src = if clerk {
        "https://challenges.cloudflare.com https://js.stripe.com https://*.js.stripe.com https://hooks.stripe.com"
    } else {
        "'none'"
    };
    [
        "default-src 'self'".to_string(),
        format!("script-src 'self'{clerk_script}{stripe_script}"),
        "style-src 'self' 'unsafe-inline' https://fonts.googleapis.com".to_string(),
        "font-src 'self' data: https://fonts.gstatic.com".to_string(),
        format!("img-src 'self' data: blob:{clerk_img}"),
        "media-src 'self' blob:".to_string(),
        format!("connect-src 'self'{clerk_connect}{stripe_connect}"),
        format!("frame-src {frame_src}"),
        "worker-src 'self' blob:".to_string(),
        "object-src 'none'".to_string(),
        "base-uri 'self'".to_string(),
        "form-action 'self'".to_string(),
        "frame-ancestors 'none'".to_string(),
    ]
    .join("; ")
}

/// Set the policy [`stamp`] sends. Called once, from `build_router`;
/// later calls (tests building several routers) keep the first.
pub fn configure_csp(policy: &str) {
    if let Ok(value) = HeaderValue::from_str(policy) {
        let _ = CSP.set(value);
    }
}

/// Whether an inbound `X-Request-Id` can be trusted.
///
/// 8–128 characters, alphanumerics and hyphens only. The Python comment
/// gives the reason: an unvalidated header would inject arbitrary text
/// into log lines and Sentry tags.
///
/// `isalnum()` in Python is Unicode-aware — "٣" is alphanumeric to it —
/// so this deliberately uses the same notion rather than ASCII-only,
/// which would reject ids Python accepts.
fn inbound_id_is_valid(value: &str) -> bool {
    let len = value.chars().count();
    if !(8..=128).contains(&len) {
        return false;
    }
    let stripped: String = value.chars().filter(|c| *c != '-').collect();
    !stripped.is_empty() && stripped.chars().all(char::is_alphanumeric)
}

/// 16 hex characters from a v4 UUID, matching `new_request_id`.
///
/// No `uuid` dependency for one call site: this needs unpredictability
/// for log correlation, not cryptographic strength, and the shape is
/// what matters.
fn new_request_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Mixed with the address of a local so two ids minted in the same
    // nanosecond on different tasks still differ.
    let local = 0u8;
    let salt = (&local as *const u8) as usize as u128;
    let mixed = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(salt);
    format!("{:016x}", mixed as u64)
}

pub async fn layer(request: Request, next: Next) -> Response<axum::body::Body> {
    let inbound = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let request_id = if inbound_id_is_valid(inbound) {
        inbound.to_string()
    } else {
        new_request_id()
    };

    // Python reads the scheme from the request URL; behind Fly the TLS
    // terminates at the edge, which is why FLY_APP_NAME is the other
    // half of the condition.
    let is_https = request
        .uri()
        .scheme_str()
        .is_some_and(|s| s.eq_ignore_ascii_case("https"))
        || std::env::var("FLY_APP_NAME").is_ok();

    // Every log line this request produces carries `req` and, once the
    // caller is known, `org`.
    //
    // This is `logging_setup.py`'s `ContextFilter`, which stamped both
    // onto every record from a contextvar — a launch-checklist item whose
    // stated purpose was "when a customer says I got a 500 at 3:14pm" and
    // "a single grep on the org_id surfaces the full request flow". The
    // `X-Request-Id` header was ported and this half was not, which left
    // the header pointing at an id that appeared in no log line.
    //
    // A span rather than a task-local: `tracing`'s fmt layer prints the
    // enclosing spans' fields on every event inside them, so this needs
    // no filter and no plumbing through call sites. `org` is declared
    // Empty and recorded by `auth::authenticate` once a token verifies —
    // background loops run outside any request span and simply have
    // neither field, which is what the Python's "-" rendered.
    let span = tracing::info_span!(
        "request",
        req = %request_id,
        org = tracing::field::Empty,
    );

    let mut response = {
        use tracing::Instrument;
        next.run(request).instrument(span).await
    };
    stamp(response.headers_mut(), &request_id, is_https);
    response
}

/// Apply the headers to a response that does not already have them.
///
/// Split out from the layer so the "leave a proxied response alone" rule
/// can be tested directly. It cannot be caught by comparing responses:
/// the id is deterministic from the inbound header when one is supplied,
/// and when one is not, Python's minted id and Rust's are both sixteen
/// hex characters and indistinguishable to a differential. The rule
/// still matters — restamping would mean the header the client sees no
/// longer matches the id Python wrote to its own log line.
pub fn stamp(headers: &mut axum::http::HeaderMap, request_id: &str, is_https: bool) {
    if headers.contains_key("x-request-id") {
        return;
    }
    if let Ok(value) = HeaderValue::from_str(request_id) {
        headers.insert(HeaderName::from_static("x-request-id"), value);
    }
    for (name, value) in SECURITY_HEADERS {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    if is_https {
        headers.insert(
            HeaderName::from_static("strict-transport-security"),
            HeaderValue::from_static(HSTS),
        );
    }
    // A handler that sets its own policy keeps it: the API docs pages
    // load Swagger UI and ReDoc from a CDN.
    if let Some(csp) = CSP.get() {
        headers
            .entry(axum::http::header::CONTENT_SECURITY_POLICY)
            .or_insert_with(|| csp.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directive<'a>(policy: &'a str, name: &str) -> &'a str {
        policy
            .split("; ")
            .find(|d| d.starts_with(&format!("{name} ")))
            .unwrap_or_else(|| panic!("no {name} in {policy}"))
    }

    #[test]
    fn the_hosted_policy_allows_clerk_and_nothing_inline_for_scripts() {
        let p = content_security_policy(Some("https://clerk.example.test"));
        let script = directive(&p, "script-src");
        assert!(script.contains("'self'"));
        assert!(script.contains("https://clerk.example.test"));
        assert!(script.contains("https://challenges.cloudflare.com"));
        // Stripe Elements loads from js.stripe.com's subdomains too.
        assert!(script.contains("https://*.js.stripe.com"));
        assert!(directive(&p, "frame-src").contains("https://*.js.stripe.com"));
        assert!(!script.contains("unsafe-inline"), "{script}");
        assert!(!script.contains("unsafe-eval"), "{script}");
        assert!(directive(&p, "connect-src").contains("https://clerk.example.test"));
        assert_eq!(directive(&p, "frame-ancestors"), "frame-ancestors 'none'");
        assert_eq!(directive(&p, "object-src"), "object-src 'none'");
        assert!(directive(&p, "media-src").contains("blob:"));
    }

    #[test]
    fn the_self_hosted_policy_names_no_outside_service_but_fonts() {
        let p = content_security_policy(None);
        for gone in ["clerk", "stripe", "cloudflare"] {
            assert!(!p.contains(gone), "{gone} in {p}");
        }
        assert_eq!(directive(&p, "script-src"), "script-src 'self'");
        assert_eq!(directive(&p, "frame-src"), "frame-src 'none'");
    }

    #[test]
    fn a_well_formed_inbound_id_is_honoured() {
        for id in [
            "abcd1234",
            "9b681f7596b74624",
            "a-b-c-d-e-f-g-h",
            &"x".repeat(128),
        ] {
            assert!(inbound_id_is_valid(id), "{id:?} should be accepted");
        }
    }

    #[test]
    fn a_suspect_inbound_id_is_replaced() {
        // The Python comment: "we don't want a malicious header injecting
        // weird characters into our log lines or Sentry tags".
        for id in [
            "",
            "short",          // under 8
            &"x".repeat(129), // over 128
            "has space",
            "semi;colon",
            "new\nline",
            "quote\"mark",
            "----------", // hyphens only, nothing left after stripping
        ] {
            assert!(!inbound_id_is_valid(id), "{id:?} should be replaced");
        }
    }

    #[test]
    fn non_ascii_alphanumerics_are_accepted_as_python_accepts_them() {
        // str.isalnum() is Unicode-aware, so an ASCII-only check here
        // would reject ids the Python tier passes straight through.
        assert!(inbound_id_is_valid("café1234"));
        assert!(inbound_id_is_valid("٠١٢٣٤٥٦٧"));
    }

    #[test]
    fn a_minted_id_has_the_shape_python_mints() {
        let id = new_request_id();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_response_that_already_has_an_id_is_left_alone() {
        // A proxied response has been through Python's middleware.
        // Restamping it would hand the client an id that appears in no
        // log line — and no differential can see that, because two
        // minted ids look alike.
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-request-id"),
            HeaderValue::from_static("from-python-1234"),
        );
        headers.insert(
            HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("DENY"),
        );
        stamp(&mut headers, "rust-would-use-this", true);
        assert_eq!(headers["x-request-id"], "from-python-1234");
        assert_eq!(headers.get_all("x-request-id").iter().count(), 1);
    }

    #[test]
    fn a_bare_response_gets_the_whole_set() {
        let mut headers = axum::http::HeaderMap::new();
        stamp(&mut headers, "abcd1234abcd1234", false);
        assert_eq!(headers["x-request-id"], "abcd1234abcd1234");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["x-frame-options"], "DENY");
        assert_eq!(
            headers["referrer-policy"],
            "strict-origin-when-cross-origin"
        );
        assert_eq!(
            headers["permissions-policy"],
            "camera=(), microphone=(), geolocation=()"
        );
        // http, so no HSTS — advertising it over plaintext is how a
        // downgrade gets pinned.
        assert!(!headers.contains_key("strict-transport-security"));
    }

    #[test]
    fn hsts_is_added_only_over_https() {
        let mut headers = axum::http::HeaderMap::new();
        stamp(&mut headers, "abcd1234abcd1234", true);
        assert_eq!(
            headers["strict-transport-security"],
            "max-age=63072000; includeSubDomains"
        );
    }

    #[test]
    fn minted_ids_differ() {
        let ids: std::collections::HashSet<String> = (0..100).map(|_| new_request_id()).collect();
        assert!(ids.len() > 90, "ids should not collide in bulk");
    }
}
