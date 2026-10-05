//! Audit-log writing.
//!
//! Ported from `backend/app/core/audit.py`. The audit trail is a paid
//! compliance surface, so this reproduces the Python byte for byte —
//! including the exact JSON formatting of the `details` column, which is
//! compared directly by the write differential and read back by the
//! dashboard.

use axum::http::HeaderMap;
use serde_json::Value;

use crate::auth::AuthUser;

/// Column widths from the model. Python truncates before inserting;
/// Postgres would otherwise reject an over-long value outright, turning
/// a long username into a failed request rather than a trimmed log line.
const EVENT_MAX: usize = 50;
const IP_MAX: usize = 45;
const USERNAME_MAX: usize = 80;
const USER_ID_MAX: usize = 100;

/// Best-effort human label for an actor: email, then username, then a
/// 32-character prefix of the user id.
pub fn audit_label(user: &AuthUser) -> String {
    if !user.email.is_empty() {
        return user.email.clone();
    }
    if !user.username.is_empty() {
        return user.username.clone();
    }
    truncate(&user.user_id, 32)
}

/// Truncate on character boundaries.
///
/// Python slices `str` by characters; slicing a Rust `String` by bytes
/// would panic mid-codepoint on, say, an emoji in a display name.
fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// The real client IP, resolved the same way the rate limiter does.
///
/// Behind Fly's edge `request.client.host` is a proxy hop, and an audit
/// trail that records the edge address instead of the source is useless
/// for the forensics it exists to support. Off Fly the proxy headers are
/// the caller's own words and are not recorded as its address — see
/// [`crate::config::trust_proxy_headers`].
pub fn client_ip(headers: &HeaderMap, peer: Option<&str>) -> String {
    crate::ratelimit::client_ip(headers, peer, crate::ratelimit::proxy_headers_trusted())
        .unwrap_or_default()
}

/// Serialise `details` the way `json.dumps` does.
///
/// Not `serde_json::to_string`. Python's default separators are `", "`
/// and `": "`, so `{"enabled": true}` is stored with spaces where
/// serde_json writes `{"enabled":true}`. The value lands in a `Text`
/// column that the dashboard renders and the differential compares
/// literally, so the spacing is part of the contract.
///
/// Key order is insertion order. Top-level pairs come in as an ordered
/// slice; nested objects rely on serde_json's `preserve_order` feature,
/// without which its `Map` is a `BTreeMap` and silently re-sorts them.
/// That was a real defect: `{"zebra": 1, "apple": 2}` serialised as
/// `{"apple":2,"zebra":1}` where Python writes the original order.
pub fn python_json(pairs: &[(&str, Value)]) -> String {
    let mut out = String::from("{");
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&python_json_string(key));
        out.push_str(": ");
        out.push_str(&python_json_value(value));
    }
    out.push('}');
    out
}

/// The same, with no spaces after the separators.
///
/// `json.dumps(x, separators=(",", ":"))`, which is what FastMCP uses
/// for a tool result's text block — the value is repeated as
/// `structuredContent` beside it, so the text is for a model to read
/// and the bytes are not padded for a human.
pub fn python_json_compact(value: &Value) -> String {
    match value {
        Value::String(s) => python_json_string(s),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(python_json_compact).collect();
            format!("[{}]", inner.join(","))
        }
        Value::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}:{}", python_json_string(k), python_json_compact(v)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        other => python_json_value(other),
    }
}

pub fn python_json_value(value: &Value) -> String {
    match value {
        Value::String(s) => python_json_string(s),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(python_json_value).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", python_json_string(k), python_json_value(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// A JSON string literal as `json.dumps` writes one.
///
/// `ensure_ascii=True` is the default and serde_json has no equivalent:
/// Python escapes **everything** outside printable ASCII to `\uXXXX`,
/// astral characters as a surrogate pair, where serde_json emits the
/// UTF-8 bytes. A camera named "Café" was therefore stored as
/// `{"name": "Café"}` by the port and `{"name": "Caf\u00e9"}` by
/// Python — into a `Text` column the differential compares literally.
/// No fixture had a non-ASCII name, so nothing caught it.
///
/// Python's own rule is the regex `([\\"]|[^ -~])`: escape backslash,
/// quote, and anything below U+0020 or above U+007E. That upper bound
/// includes DEL, which serde_json also passes through raw.
pub fn python_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (' '..='~').contains(&c) => out.push(c),
            c => {
                let cp = c as u32;
                if cp > 0xFFFF {
                    // Python emits a UTF-16 surrogate pair, the same
                    // way it stores the character internally.
                    let v = cp - 0x1_0000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xD800 + (v >> 10),
                        0xDC00 + (v & 0x3FF)
                    ));
                } else {
                    out.push_str(&format!("\\u{cp:04x}"));
                }
            }
        }
    }
    out.push('"');
    out
}

/// Persist one audit row.
///
/// Returns `Ok(())` even when the insert fails, matching the Python's
/// blanket `except`: losing an audit line must not fail the operation
/// that was already carried out. The failure is logged instead.
#[allow(clippy::too_many_arguments)]
pub async fn write_audit(
    pool: &crate::db::Pool,
    org_id: &str,
    event: &str,
    user_id: &str,
    username: &str,
    details: Option<String>,
    headers: &HeaderMap,
    peer: Option<&str>,
) {
    let ip = truncate(&client_ip(headers, peer), IP_MAX);
    let ip = if ip.is_empty() { None } else { Some(ip) };
    let username = truncate(username, USERNAME_MAX);
    let username = if username.is_empty() {
        None
    } else {
        Some(username)
    };
    let user_id = truncate(user_id, USER_ID_MAX);
    let user_id = if user_id.is_empty() {
        None
    } else {
        Some(user_id)
    };

    let result = sqlx::query(
        "INSERT INTO audit_log (org_id, event, ip_address, username, user_id, details, timestamp)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(org_id)
    .bind(truncate(event, EVENT_MAX))
    .bind(ip)
    .bind(username)
    .bind(user_id)
    .bind(details)
    .bind(crate::models::now_naive())
    .execute(pool)
    .await;

    if let Err(err) = result {
        tracing::error!(error = %err, event, "failed to write audit row");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_escaped_exactly_as_json_dumps_escapes_them() {
        // Expected values generated by CPython's json.dumps with its
        // default ensure_ascii=True, then pasted here. serde_json
        // disagrees on every row below the first: it emits UTF-8 for
        // non-ASCII and leaves DEL raw.
        for (input, expected) in [
            ("plain", "\"plain\""),                   // plain
            ("Caf\u{e9}", "\"Caf\\u00e9\""),          // an accented letter
            ("\u{1f3a5}", "\"\\ud83c\\udfa5\""),      // an astral character -> surrogate pair
            ("quote\"inside", "\"quote\\\"inside\""), // a quote
            ("back\\slash", "\"back\\\\slash\""),     // a backslash
            ("tab\u{9}here", "\"tab\\there\""),       // a tab
            ("line\u{a}break", "\"line\\nbreak\""),   // a newline
            ("\u{d}", "\"\\r\""),                     // carriage return
            ("\u{8}", "\"\\b\""),                     // backspace
            ("\u{c}", "\"\\f\""),                     // form feed
            ("\u{0}", "\"\\u0000\""),                 // NUL
            ("\u{7f}", "\"\\u007f\""), // DEL — above Python's printable range, below serde_json's
            ("\u{1f}", "\"\\u001f\""), // unit separator
            ("~", "\"~\""),            // the top of the printable range
            (" ", "\" \""),            // the bottom of the printable range
            ("\u{a0}", "\"\\u00a0\""), // non-breaking space
            ("\u{2026}", "\"\\u2026\""), // ellipsis
            ("\u{ffff}", "\"\\uffff\""), // the top of the BMP
            ("\u{10ffff}", "\"\\udbff\\udfff\""), // the highest code point
            (
                "mixed \u{e9}\u{1f3a5}\"x\\",
                "\"mixed \\u00e9\\ud83c\\udfa5\\\"x\\\\\"",
            ), // several at once
        ] {
            assert_eq!(python_json_string(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn a_non_ascii_value_is_escaped_inside_a_details_object() {
        assert_eq!(
            python_json(&[("name", Value::String("Caf\u{e9}".into()))]),
            "{\"name\": \"Caf\\u00e9\"}"
        );
    }

    use serde_json::json;

    fn user(email: &str, username: &str, user_id: &str) -> AuthUser {
        AuthUser {
            user_id: user_id.into(),
            org_id: "org".into(),
            org_role: String::new(),
            org_permissions: vec![],
            email: email.into(),
            username: username.into(),
            plan: String::new(),
            features: vec![],
        }
    }

    #[test]
    fn the_label_prefers_email_then_username_then_id() {
        assert_eq!(user("a@b.c", "name", "u1").pipe_label(), "a@b.c");
        assert_eq!(user("", "name", "u1").pipe_label(), "name");
        assert_eq!(user("", "", "u1").pipe_label(), "u1");
    }

    #[test]
    fn a_long_user_id_label_is_cut_at_32_characters() {
        let long = "u".repeat(50);
        assert_eq!(user("", "", &long).pipe_label().len(), 32);
    }

    #[test]
    fn truncation_does_not_split_a_multibyte_character() {
        // Slicing by bytes here would panic mid-codepoint.
        let emoji = "🎥".repeat(40);
        let cut = truncate(&emoji, 32);
        assert_eq!(cut.chars().count(), 32);
    }

    #[test]
    fn details_are_formatted_the_way_python_writes_them() {
        // json.dumps defaults to ", " and ": " separators. serde_json
        // writes neither, and this string is stored and compared.
        assert_eq!(
            python_json(&[("enabled", json!(true))]),
            r#"{"enabled": true}"#
        );
        assert_eq!(
            python_json(&[("key_id", json!(3)), ("name", json!("k"))]),
            r#"{"key_id": 3, "name": "k"}"#
        );
        assert_eq!(python_json(&[]), "{}");
    }

    #[test]
    fn nested_values_use_the_same_separators() {
        assert_eq!(
            python_json(&[("tools", json!(["a", "b"]))]),
            r#"{"tools": ["a", "b"]}"#
        );
        assert_eq!(
            python_json(&[("meta", json!({"x": 1}))]),
            r#"{"meta": {"x": 1}}"#
        );
    }

    #[test]
    fn a_nested_object_keeps_the_order_it_was_written_in() {
        // serde_json's default Map is a BTreeMap and would emit
        // {"apple": 2, "zebra": 1} here. Python writes insertion order,
        // and this string is stored in the audit trail verbatim.
        assert_eq!(
            python_json(&[("meta", json!({"zebra": 1, "apple": 2}))]),
            r#"{"meta": {"zebra": 1, "apple": 2}}"#
        );
    }

    #[test]
    fn a_null_detail_is_written_as_null_not_omitted() {
        assert_eq!(
            python_json(&[("scope_tool_count", Value::Null)]),
            r#"{"scope_tool_count": null}"#
        );
    }

    #[test]
    fn the_client_ip_prefers_the_header_fly_sets_when_proxy_headers_are_trusted() {
        use crate::ratelimit::client_ip as resolve;
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "2.2.2.2, 3.3.3.3".parse().unwrap());
        assert_eq!(resolve(&h, Some("9.9.9.9"), true).as_deref(), Some("2.2.2.2"));
        h.insert("fly-client-ip", "1.1.1.1".parse().unwrap());
        assert_eq!(resolve(&h, Some("9.9.9.9"), true).as_deref(), Some("1.1.1.1"));
        assert_eq!(resolve(&h, Some("9.9.9.9"), false).as_deref(), Some("9.9.9.9"));
    }

    #[test]
    fn with_no_proxy_headers_the_peer_is_recorded() {
        assert_eq!(client_ip(&HeaderMap::new(), Some("10.0.0.1")), "10.0.0.1");
        assert_eq!(client_ip(&HeaderMap::new(), None), "");
    }

    // Small shim so the label tests read as `user(...).pipe_label()`.
    trait Label {
        fn pipe_label(&self) -> String;
    }
    impl Label for AuthUser {
        fn pipe_label(&self) -> String {
            audit_label(self)
        }
    }
}
