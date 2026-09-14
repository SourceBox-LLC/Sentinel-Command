//! The `settings` key/value table.
//!
//! Ported from `Setting` in `backend/app/models/models.py`. After auth
//! this is the most-executed query in the service — plan resolution,
//! feature toggles and anchors all read it, several times on some paths.

/// Fetch one setting, or `default` if it is not set.
///
/// `(org_id, key)` is indexed but deliberately **not** unique: `Setting.set`
/// is check-then-insert, so two concurrent first-writers can transiently
/// duplicate a pair, and a unique index would make the schema sync fail on
/// any existing duplicate. SQLAlchemy's `.first()` resolves that with a
/// bare `LIMIT 1` and no ordering; this matches it rather than imposing an
/// order the Python does not have.
pub async fn get(
    pool: &sqlx::PgPool,
    org_id: &str,
    key: &str,
    default: Option<&str>,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as(r#"SELECT value FROM settings WHERE org_id = $1 AND "key" = $2 LIMIT 1"#)
            .bind(org_id)
            .bind(key)
            .fetch_optional(pool)
            .await?;

    // A row whose `value` is NULL is a set-but-empty setting, not an
    // absent one — Python returns `setting.value` (None) in that case
    // rather than falling back to the default.
    Ok(match row {
        Some((value,)) => value,
        None => default.map(str::to_string),
    })
}

/// Whether this org's payment is past due.
///
/// Read as its own function because it gates every write in the service
/// and the string comparison is exact: anything other than `"true"` —
/// unset, `"false"`, `"TRUE"` — means not past due. Being generous here
/// would lock paying customers out of their own cameras.
pub async fn payment_past_due(pool: &sqlx::PgPool, org_id: &str) -> Result<bool, sqlx::Error> {
    Ok(get(pool, org_id, "payment_past_due", Some("false")).await? == Some("true".to_string()))
}
