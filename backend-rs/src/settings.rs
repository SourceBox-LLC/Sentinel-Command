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
    pool: &crate::db::Pool,
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
pub async fn payment_past_due(pool: &crate::db::Pool, org_id: &str) -> Result<bool, sqlx::Error> {
    Ok(get(pool, org_id, "payment_past_due", Some("false")).await? == Some("true".to_string()))
}

/// Upsert one setting, reproducing `Setting.set`.
///
/// Python selects the row, assigns, and commits — so when the value is
/// unchanged SQLAlchemy's dirty check emits no UPDATE and `updated_at`
/// (an `onupdate` column) does not move. Writing unconditionally would
/// bump it on every save of an unchanged toggle.
///
/// The `(org_id, key)` pair is deliberately not unique — `Setting.set`
/// is check-then-insert, so two concurrent first-writers can duplicate
/// it — which is why this updates by the row's own id rather than by the
/// pair, and inserts only when nothing was found.
pub async fn set(
    pool: &crate::db::Pool,
    org_id: &str,
    key: &str,
    value: &str,
) -> Result<(), sqlx::Error> {
    let existing: Option<(i32, Option<String>)> = sqlx::query_as(
        r#"SELECT id, value FROM settings WHERE org_id = $1 AND "key" = $2 LIMIT 1"#,
    )
    .bind(org_id)
    .bind(key)
    .fetch_optional(pool)
    .await?;

    match existing {
        Some((id, current)) => {
            if current.as_deref() == Some(value) {
                return Ok(());
            }
            sqlx::query("UPDATE settings SET value = $1, updated_at = $2 WHERE id = $3")
                .bind(value)
                .bind(crate::models::now_naive())
                .bind(id)
                .execute(pool)
                .await?;
        }
        None => {
            sqlx::query(
                r#"INSERT INTO settings (org_id, "key", value, updated_at)
                   VALUES ($1, $2, $3, $4)"#,
            )
            .bind(org_id)
            .bind(key)
            .bind(value)
            .bind(crate::models::now_naive())
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}
