//! Turning a verified Clerk JWT into an `AuthUser`.
//!
//! Signature verification is the easy half. This is the half that breaks
//! silently: Clerk emits two claim layouts, and the V2 one encodes
//! permissions as a bitmap that has to be reconstructed. A subtly wrong
//! reconstruction does not error — it quietly grants or withholds admin.
//!
//! Ported from `backend/app/core/auth.py`. Everything here is pure
//! (claims in, `AuthUser` out) precisely so it can be diffed against the
//! Python implementation without a network, a database or a real session.

use serde_json::Value;

/// The authenticated caller, as the route guards see them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthUser {
    pub user_id: String,
    pub org_id: String,
    pub org_role: String,
    pub org_permissions: Vec<String>,
    pub email: String,
    pub username: String,
    pub plan: String,
    pub features: Vec<String>,
}

impl AuthUser {
    pub fn has_permission(&self, permission: &str) -> bool {
        self.org_permissions.iter().any(|p| p == permission)
    }

    /// Admin by role, or by holding the manage-cameras permission.
    ///
    /// Both spellings of the role are accepted because Clerk V1 sends
    /// `org:admin` and V2 sends a bare `admin`.
    pub fn is_admin(&self) -> bool {
        self.org_role == "org:admin"
            || self.org_role == "admin"
            || self.has_permission("org:cameras:manage_cameras")
    }

    /// Every org member can view cameras. Kept as a method rather than
    /// inlined at the call sites so that if it ever stops being true,
    /// there is one place to change.
    pub fn can_view_cameras(&self) -> bool {
        true
    }
}

/// Why claim extraction refused to produce an `AuthUser`.
///
/// The two cases carry different HTTP statuses in the Python service and
/// the SPA branches on the difference — a signed-in user with no org gets
/// a 400 and a "create or join an organization" prompt, not a 401 that
/// would bounce them back to sign-in.
#[derive(Debug, PartialEq, Eq)]
pub enum ClaimError {
    /// No `sub` — the token is not usable at all. 401.
    NotAuthenticated,
    /// Valid user, but no organisation selected. 400.
    NoOrganization,
    /// A claim is present with a type this code does not expect: Clerk
    /// changed its token format under us. Becomes `AuthError::Failed`, a
    /// 503 rather than a sign-out. See `validate_claim_types`.
    Malformed(&'static str),
}

/// Python's notion of truthiness, for the claims where the original code
/// branches on it (`if plan_claim`, `if fpm_str`). `0`, `false`, `""`,
/// `[]`, `{}` and `null` are all falsy there and take the no-crash path.
fn is_falsy(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

/// `true` if the claim is absent or JSON null — both of which Python's
/// `.get(key, default)` turns into the default without complaint.
fn absent(claims: &Value, key: &str) -> bool {
    matches!(claims.get(key), None | Some(Value::Null))
}

/// Reject claim sets whose types this code does not expect.
///
/// Clerk signs these, so a wrong-typed claim cannot be forged — it means
/// either Clerk changed its wire format or something upstream is broken.
/// Either way the safe answer is to refuse, and refusing here keeps the
/// rest of this module free of type-coercion branches.
///
/// Python has no such check, and the two stacks therefore differ on
/// malformed input in two ways, both deliberate:
///
/// * Where Python raises (`pla`/`fea`/`o.per`/`o.fpm` of the wrong type,
///   or a non-object `o`), its blanket handler returns 401
///   "Authentication failed". This used to return the same 401; it is
///   now a 503 (see `AuthError::Failed`), so it no longer signs anyone out.
/// * Where Python silently coerces (`sub`, `org_id`, `o.id`, `o.rol`), it
///   builds an `AuthUser` around a non-string. That is worth diverging
///   from rather than copying: `org_permissions` arriving as a *string*
///   makes Python's `permission in self.org_permissions` a substring
///   test, so a value like `"xxorg:cameras:manage_cameras"` would pass
///   the admin check. Failing closed here cannot.
fn validate_claim_types(claims: &Value) -> Result<(), ClaimError> {
    // Plain string claims. Python coerces these; we refuse them.
    for key in ["sub", "org_id", "org_role", "email", "username"] {
        if !absent(claims, key) && !claims[key].is_string() {
            return Err(ClaimError::Malformed(key));
        }
    }

    // `pla` is read behind `if plan_claim`, so a falsy non-string takes
    // the "free_org" path in Python without raising. Only a truthy
    // non-string reaches `.split` and blows up.
    if let Some(pla) = claims.get("pla") {
        if !pla.is_string() && !is_falsy(pla) {
            return Err(ClaimError::Malformed("pla"));
        }
    }

    // `fea` has no such guard — `claims.get("fea", "").split(",")` runs
    // unconditionally, so any non-string, null included, raises.
    if let Some(fea) = claims.get("fea") {
        if !fea.is_string() {
            return Err(ClaimError::Malformed("fea"));
        }
    }

    // Permissions must be a list of strings or nothing at all.
    for key in ["org_permissions", "permissions"] {
        if let Some(v) = claims.get(key) {
            if v.is_null() {
                continue;
            }
            let ok = v.as_array().is_some_and(|a| a.iter().all(Value::is_string));
            if !ok {
                return Err(ClaimError::Malformed(key));
            }
        }
    }

    // `o` is dereferenced with `.get` in Python whatever it holds, so
    // anything that is not a dict raises — including null, because
    // `claims.get("o", {})` returns the null rather than the default.
    if let Some(o) = claims.get("o") {
        let Some(o) = o.as_object() else {
            return Err(ClaimError::Malformed("o"));
        };
        for key in ["id", "rol", "per", "fpm"] {
            match o.get(key) {
                None | Some(Value::Null) => {}
                Some(v) if v.is_string() => {}
                Some(_) => return Err(ClaimError::Malformed("o")),
            }
        }
    }

    Ok(())
}

fn string_claim(claims: &Value, key: &str) -> String {
    claims
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Read a claim that should be an array of strings.
///
/// A non-array, or a value Python would have accepted but that cannot be
/// read as a list of permission strings, yields empty — which sends the
/// caller down the V2 path and, failing that, grants nothing.
fn string_list(claims: &Value, key: &str) -> Vec<String> {
    claims
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Split a comma-separated claim, trimming and dropping the `o:` prefix.
///
/// Clerk scopes organisation-level entries with `o:`; the Python code
/// strips it so `o:pro` and `pro` are the same feature.
fn split_scoped_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.strip_prefix("o:").unwrap_or(s).to_string())
        .collect()
}

/// Reconstruct permission keys from Clerk's V2 bitmap encoding.
///
/// V2 sends three pieces: `o.per` is the ordered list of permission
/// names, `fea` the ordered list of features, and `o.fpm` one integer per
/// feature whose bit `j` means "this feature grants permission `j`".
/// The full key is `org:{feature}:{permission}`.
///
/// Returns empty rather than erroring on anything malformed — a
/// permission that cannot be proven is a permission the caller does not
/// have, which is the safe direction.
pub fn decode_v2_permissions(claims: &Value) -> Vec<String> {
    let o_claim = claims.get("o");
    let fea = string_claim(claims, "fea");

    let Some(o_claim) = o_claim else {
        return Vec::new();
    };
    if fea.is_empty() || !o_claim.is_object() {
        return Vec::new();
    }

    let per_str = o_claim.get("per").and_then(Value::as_str).unwrap_or("");
    if per_str.is_empty() {
        return Vec::new();
    }
    // NOTE: not trimmed and not filtered for empties — Python splits this
    // one with a bare `split(",")`, so the positional index of every
    // permission name must line up with the bits exactly as Python sees
    // them. Filtering here would shift every later bit.
    let permission_names: Vec<&str> = per_str.split(',').collect();

    // Same reasoning: Python does not trim or drop empties when building
    // the feature list inside this function, so neither do we.
    let features: Vec<String> = fea
        .split(',')
        .map(|f| f.strip_prefix("o:").unwrap_or(f).to_string())
        .collect();

    let fpm_str = o_claim.get("fpm").and_then(Value::as_str).unwrap_or("");
    let mut fpm_values: Vec<i64> = Vec::new();
    if !fpm_str.is_empty() {
        // Python wraps the whole parse in try/except and abandons the
        // entire list on the first bad value, rather than skipping it.
        match fpm_str
            .split(',')
            .map(|x| x.parse::<i64>())
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(v) => fpm_values = v,
            Err(_) => fpm_values = Vec::new(),
        }
    }

    let mut out = Vec::new();
    for (i, feature) in features.iter().enumerate() {
        // A feature with no corresponding bitmap entry grants nothing.
        let Some(&fpm) = fpm_values.get(i) else {
            continue;
        };
        for (j, perm) in permission_names.iter().enumerate() {
            // `i64` rather than `u64` because Python's `int()` accepts a
            // negative, and a negative in two's complement has every low
            // bit set — the same permissions `i64` yields here. The
            // shift is bounded at 63 where Python's arbitrary-precision
            // integers are not: a bitmap that fits in an i64 cannot
            // address a 64th permission anyway, so the bound only ever
            // withholds permissions that were never encoded.
            if j < 63 && fpm & (1i64 << j) != 0 {
                out.push(format!("org:{feature}:{perm}"));
            }
        }
    }
    out
}

/// Build an `AuthUser` from verified Clerk claims.
///
/// Handles both layouts: V1 puts `org_id` / `org_role` / `org_permissions`
/// at the top level, V2 packs them into a compact `o` claim and encodes
/// permissions as a bitmap.
pub fn auth_user_from_claims(claims: &Value) -> Result<AuthUser, ClaimError> {
    // Everything below assumes claims are the types Clerk documents.
    // This is what makes that assumption safe.
    validate_claim_types(claims)?;

    // V1 first, then V2's bitmap. `permissions` is accepted as an alias
    // because Clerk has used both spellings.
    //
    // Python chains these with `or`, which tests the *value*, not the
    // key: an `org_permissions` present but empty falls through to
    // `permissions`. Keying off presence instead would strand a caller
    // whose real permissions live under the alias.
    let mut org_permissions = string_list(claims, "org_permissions");
    if org_permissions.is_empty() {
        org_permissions = string_list(claims, "permissions");
    }
    if org_permissions.is_empty() {
        org_permissions = decode_v2_permissions(claims);
    }

    let user_id = string_claim(claims, "sub");
    let email = string_claim(claims, "email");
    let username = string_claim(claims, "username");

    // "o:pro" -> "pro"; absent -> free_org.
    let plan_claim = string_claim(claims, "pla");
    let plan = if plan_claim.is_empty() {
        "free_org".to_string()
    } else {
        plan_claim
            .rsplit(':')
            .next()
            .unwrap_or("free_org")
            .to_string()
    };

    let features = split_scoped_list(&string_claim(claims, "fea"));

    let o_claim = claims.get("o");
    let org_id = {
        let v1 = string_claim(claims, "org_id");
        if !v1.is_empty() {
            v1
        } else {
            o_claim
                .and_then(|o| o.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        }
    };
    let org_role = {
        let v1 = string_claim(claims, "org_role");
        if !v1.is_empty() {
            v1
        } else {
            o_claim
                .and_then(|o| o.get("rol"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        }
    };

    if user_id.is_empty() {
        return Err(ClaimError::NotAuthenticated);
    }
    if org_id.is_empty() {
        return Err(ClaimError::NoOrganization);
    }

    Ok(AuthUser {
        user_id,
        org_id,
        org_role,
        org_permissions,
        email,
        username,
        plan,
        features,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn v1_claims_are_read_from_the_top_level() {
        let c = json!({
            "sub": "user_1", "org_id": "org_1", "org_role": "org:admin",
            "org_permissions": ["org:cameras:manage_cameras"],
            "email": "a@b.c", "username": "ab",
        });
        let u = auth_user_from_claims(&c).unwrap();
        assert_eq!(u.org_id, "org_1");
        assert_eq!(u.org_role, "org:admin");
        assert!(u.is_admin());
        assert_eq!(u.plan, "free_org", "absent pla claim defaults to free_org");
    }

    #[test]
    fn v2_claims_are_read_from_the_compact_o_claim() {
        let c = json!({
            "sub": "user_2",
            "o": {"id": "org_2", "rol": "admin"},
            "pla": "o:pro",
            "fea": "o:cameras,o:admin",
        });
        let u = auth_user_from_claims(&c).unwrap();
        assert_eq!(u.org_id, "org_2");
        assert_eq!(u.org_role, "admin");
        assert!(u.is_admin(), "bare 'admin' is the V2 spelling");
        assert_eq!(u.plan, "pro", "o:pro -> pro");
        assert_eq!(u.features, vec!["cameras", "admin"]);
    }

    #[test]
    fn a_missing_subject_is_not_authenticated() {
        let c = json!({"org_id": "org_1"});
        assert_eq!(auth_user_from_claims(&c), Err(ClaimError::NotAuthenticated));
    }

    #[test]
    fn a_user_with_no_org_is_a_distinct_failure() {
        // 400 with "create or join an organization", not a 401 that would
        // bounce a signed-in user back to the sign-in page.
        let c = json!({"sub": "user_1"});
        assert_eq!(auth_user_from_claims(&c), Err(ClaimError::NoOrganization));
    }

    #[test]
    fn admin_can_come_from_a_permission_rather_than_a_role() {
        let c = json!({
            "sub": "u", "org_id": "o", "org_role": "org:member",
            "org_permissions": ["org:cameras:manage_cameras"],
        });
        assert!(auth_user_from_claims(&c).unwrap().is_admin());
    }

    #[test]
    fn a_plain_member_is_not_admin_but_can_still_view() {
        let c = json!({"sub": "u", "org_id": "o", "org_role": "org:member"});
        let u = auth_user_from_claims(&c).unwrap();
        assert!(!u.is_admin());
        assert!(u.can_view_cameras());
    }

    #[test]
    fn v2_bitmap_reconstructs_permission_keys() {
        // per = [read, write]; features = [cameras, billing]
        // fpm = [1, 2] -> cameras gets bit0 (read), billing gets bit1 (write)
        let c = json!({
            "sub": "u",
            "o": {"id": "org", "rol": "member", "per": "read,write", "fpm": "1,2"},
            "fea": "o:cameras,o:billing",
        });
        let perms = decode_v2_permissions(&c);
        assert_eq!(perms, vec!["org:cameras:read", "org:billing:write"]);
    }

    #[test]
    fn v2_bitmap_handles_a_feature_granting_several_permissions() {
        let c = json!({
            "sub": "u",
            "o": {"id": "org", "rol": "member", "per": "read,write,delete", "fpm": "7"},
            "fea": "o:cameras",
        });
        assert_eq!(
            decode_v2_permissions(&c),
            vec![
                "org:cameras:read",
                "org:cameras:write",
                "org:cameras:delete"
            ]
        );
    }

    #[test]
    fn v2_bitmap_yields_nothing_when_a_piece_is_missing_or_malformed() {
        // A permission that cannot be proven is one the caller does not
        // have — every one of these must fail closed.
        for c in [
            json!({"sub": "u", "o": {"id": "o"}, "fea": "o:cameras"}), // no per
            json!({"sub": "u", "o": {"id": "o", "per": "read"}}),      // no fea
            json!({"sub": "u", "fea": "o:cameras"}),                   // no o
            json!({"sub": "u", "o": {"per": "read", "fpm": "nope"}, "fea": "o:c"}), // bad fpm
            json!({"sub": "u", "o": {"per": "read"}, "fea": "o:c"}),   // no fpm
        ] {
            assert!(
                decode_v2_permissions(&c).is_empty(),
                "should be empty for {c}"
            );
        }
    }

    #[test]
    fn a_permission_list_that_is_a_string_cannot_grant_admin() {
        // This is the reason type validation exists rather than being
        // tidiness. Python's `permission in self.org_permissions` is a
        // membership test on a list and a *substring* test on a string,
        // so this claim set resolves to is_admin=true over there. It
        // must not here.
        let c = json!({
            "sub": "u", "org_id": "o",
            "org_permissions": "xxorg:cameras:manage_cameras",
        });
        assert_eq!(
            auth_user_from_claims(&c),
            Err(ClaimError::Malformed("org_permissions"))
        );
    }

    #[test]
    fn wrong_typed_claims_are_refused_rather_than_coerced() {
        for (claims, expected) in [
            (json!({"sub": 12345, "org_id": "o"}), "sub"),
            (json!({"sub": "u", "org_id": 999}), "org_id"),
            (
                json!({"sub": "u", "org_id": "o", "org_role": 7}),
                "org_role",
            ),
            (json!({"sub": "u", "org_id": "o", "email": 5}), "email"),
            (json!({"sub": "u", "org_id": "o", "pla": 42}), "pla"),
            (json!({"sub": "u", "org_id": "o", "fea": 42}), "fea"),
            (json!({"sub": "u", "org_id": "o", "fea": null}), "fea"),
            (json!({"sub": "u", "o": "not-an-object"}), "o"),
            (json!({"sub": "u", "o": null}), "o"),
            (json!({"sub": "u", "o": {"id": 5}}), "o"),
            (json!({"sub": "u", "o": {"id": "o", "per": ["read"]}}), "o"),
            (
                json!({"sub": "u", "org_id": "o", "org_permissions": [1]}),
                "org_permissions",
            ),
        ] {
            assert_eq!(
                auth_user_from_claims(&claims),
                Err(ClaimError::Malformed(expected)),
                "should refuse {claims}"
            );
        }
    }

    #[test]
    fn claims_that_are_absent_or_null_are_not_malformed() {
        // Python's `.get(key, default)` swallows both, and so must this
        // — refusing them would reject ordinary tokens.
        for claims in [
            json!({"sub": "u", "org_id": "o"}),
            json!({"sub": "u", "org_id": "o", "pla": null}),
            json!({"sub": "u", "org_id": "o", "org_permissions": null}),
            json!({"sub": "u", "org_id": "o", "org_role": null}),
            json!({"sub": "u", "o": {"id": "o", "rol": "admin", "per": null, "fpm": null}}),
            // falsy non-strings reach `pla`'s no-crash path in Python
            json!({"sub": "u", "org_id": "o", "pla": 0}),
            json!({"sub": "u", "org_id": "o", "pla": []}),
        ] {
            assert!(
                auth_user_from_claims(&claims).is_ok(),
                "should accept {claims}"
            );
        }
    }

    #[test]
    fn one_bad_fpm_value_abandons_the_whole_list() {
        // Python parses fpm inside a single try/except, so a bad entry
        // anywhere discards every value rather than skipping one and
        // shifting the rest onto the wrong features.
        let c = json!({
            "sub": "u",
            "o": {"id": "o", "per": "read", "fpm": "1,oops,4"},
            "fea": "o:a,o:b,o:c",
        });
        assert!(decode_v2_permissions(&c).is_empty());
    }
}
