//! RFC 9116 `security.txt`.
//!
//! Ported from `backend/app/api/well_known.py`. Generated per request
//! rather than served as a static file for the reason that module gives:
//! RFC 9116 requires an `Expires` field no more than a year out, and a
//! static file silently rots — a year after deploy you are serving an
//! expired file that scanners flag as broken.

use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use chrono::{Duration, Utc};

use crate::app::AppState;

/// Most-preferred first, per RFC 9116 §2.5.3.
const CONTACTS: [&str; 2] = [
    "mailto:security@sentinel-command.com",
    "https://github.com/SourceBox-LLC/Sentinel-Command/security/advisories/new",
];

/// ~11 months. RFC 9116 §2.5.5 caps it at a year; the buffer means a
/// slow deploy cadence never serves an expired file.
const EXPIRY_DAYS: i64 = 330;

/// Points at SECURITY.md in the repository, deliberately.
///
/// The Python carries the reasoning: this used to point at a page on the
/// standalone site that turned out never to have existed there — that
/// URL and every plausible variant 404s. A 404 here means no published
/// scope and no published safe harbour, which is worse than an
/// unglamorous link.
const POLICY_URL: &str =
    "https://github.com/SourceBox-LLC/Sentinel-Command/blob/master/SECURITY.md";

fn build(frontend_url: &str) -> String {
    let expires = (Utc::now() + Duration::days(EXPIRY_DAYS))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    // Trailing slash stripped so the result is a well-formed origin.
    let canonical = format!(
        "{}/.well-known/security.txt",
        frontend_url.trim_end_matches('/')
    );

    let mut lines = vec![
        "# Sentinel by SourceBox -- security contact information (RFC 9116).".to_string(),
        "# Public report channel + acknowledgement window for security".to_string(),
        "# researchers.  See the policy URL for in-scope/out-of-scope".to_string(),
        "# and our coordinated-disclosure expectations.".to_string(),
        String::new(),
    ];
    lines.extend(CONTACTS.iter().map(|c| format!("Contact: {c}")));
    lines.push(format!("Expires: {expires}"));
    lines.push(format!("Canonical: {canonical}"));
    lines.push(format!("Policy: {POLICY_URL}"));
    lines.push("Preferred-Languages: en".to_string());
    lines.push(String::new());
    lines.join("\n")
}

/// `GET /.well-known/security.txt`, and the legacy `/security.txt`
/// alias some older scanners still probe.
pub async fn security_txt(State(state): State<AppState>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            // Short, so the rolling Expires actually rolls rather than
            // being pinned at whatever a CDN cached last week.
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        build(&state.config.frontend_url),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_canonical_url_has_exactly_one_slash() {
        for base in ["https://app.example.com", "https://app.example.com/"] {
            assert!(build(base).contains("Canonical: https://app.example.com/.well-known/security.txt"));
        }
    }

    #[test]
    fn every_field_rfc_9116_requires_is_present() {
        let out = build("https://app.example.com");
        for field in ["Contact:", "Expires:", "Canonical:", "Policy:", "Preferred-Languages:"] {
            assert!(out.contains(field), "missing {field}");
        }
        // Most-preferred contact first.
        let first = out.lines().find(|l| l.starts_with("Contact:")).unwrap();
        assert!(first.contains("mailto:"));
    }

    #[test]
    fn the_expiry_is_in_the_future_and_inside_the_rfc_limit() {
        let out = build("https://x.test");
        let line = out.lines().find(|l| l.starts_with("Expires:")).unwrap();
        let value = line.trim_start_matches("Expires: ");
        let parsed = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%SZ")
            .expect("Expires must be RFC 3339 as RFC 9116 requires");
        let days = (parsed - Utc::now().naive_utc()).num_days();
        assert!(days > 300, "expiry too near: {days} days");
        assert!(days < 365, "RFC 9116 caps this at one year: {days} days");
    }

    #[test]
    fn the_file_ends_with_a_newline() {
        assert!(build("https://x.test").ends_with('\n'));
    }
}
