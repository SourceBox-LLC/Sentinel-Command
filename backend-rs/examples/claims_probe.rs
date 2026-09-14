//! The Rust half of the claim-extraction differential test.
//!
//! Reads one JSON claims object per line on stdin, writes one JSON result
//! per line on stdout, in the same shape as the Python probe beside it.
//! Run them over the same corpus and diff — see
//! `tests/differential/README.md`.
//!
//! An example rather than a binary so it never ships in the image.

use std::io::{self, BufRead, Write};

use sentinel_command::auth::{auth_user_from_claims, ClaimError};
use serde_json::{json, Value};

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let claims: Value = serde_json::from_str(&line).expect("corpus line is not valid JSON");

        let result = match auth_user_from_claims(&claims) {
            Err(ClaimError::NotAuthenticated) => json!({"error": "NotAuthenticated"}),
            Err(ClaimError::NoOrganization) => json!({"error": "NoOrganization"}),
            Ok(user) => json!({
                "user_id": user.user_id,
                "org_id": user.org_id,
                "org_role": user.org_role,
                "org_permissions": user.org_permissions,
                "email": user.email,
                "username": user.username,
                "plan": user.plan,
                "features": user.features,
                "is_admin": user.is_admin(),
                "can_view_cameras": user.can_view_cameras(),
            }),
        };
        writeln!(out, "{result}")?;
    }
    Ok(())
}
