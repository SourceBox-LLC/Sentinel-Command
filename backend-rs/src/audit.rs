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
/// for the forensics it exists to support.
pub fn client_ip(headers: &HeaderMap, peer: Option<&str>) -> String {
    if let Some(ip) = headers.get("fly-client-ip").and_then(|v| v.to_str().ok()) {
        let ip = ip.trim();
        if !ip.is_empty() {
            return ip.to_string();
        }
    }
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = xff.split(',').next() {
            let first = first.trim();
            if !first.is_empty() {
                return first.to_string();
            }
        }
    }
    peer.unwrap_or_default().to_string()
}

/// Serialise `details` the way `json.dumps` does.
///
/// Not `serde_json::to_string`. Python's default separators are `", "`
/// and `": "`, so `{"enabled": true}` is stored with spaces where
/// serde_json writes `{"enabled":true}`. The value lands in a `Text`
/// column that the dashboard renders and the differential compares
/// literally, so the spacing is part of the contract.
///
/// Key order is insertion order, which `serde_json::Map` preserves only
/// with its `preserve_order` feature — so callers here pass an ordered
/// slice rather than a map, and the order is the order Python writes.
pub fn python_json(pairs: &[(&str, Value)]) -> String {
    let mut out = String::from("{");
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&serde_json::to_string(key).unwrap_or_default());
        out.push_str(": ");
        out.push_str(&python_json_value(value));
    }
    out.push('}');
    out
}

fn python_json_value(value: &Value) -> String {
    match value {
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(python_json_value).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}: {}",
                        serde_json::to_string(k).unwrap_or_default(),
                        python_json_value(v)
                    )
                })
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Persist one audit row.
///
/// Returns `Ok(())` even when the insert fails, matching the Python's
/// blanket `except`: losing an audit line must not fail the operation
/// that was already carried out. The failure is logged instead.
#[allow(clippy::too_many_arguments)]
pub async fn write_audit(
    pool: &sqlx::PgPool,
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
    fn a_null_detail_is_written_as_null_not_omitted() {
        assert_eq!(
            python_json(&[("scope_tool_count", Value::Null)]),
            r#"{"scope_tool_count": null}"#
        );
    }

    #[test]
    fn the_client_ip_prefers_the_header_fly_sets() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "2.2.2.2, 3.3.3.3".parse().unwrap());
        assert_eq!(client_ip(&h, Some("9.9.9.9")), "2.2.2.2");
        h.insert("fly-client-ip", "1.1.1.1".parse().unwrap());
        assert_eq!(client_ip(&h, Some("9.9.9.9")), "1.1.1.1");
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
