//! `settings` table behaviour, against a real Postgres.
//!
//! Gated on `TEST_DATABASE_URL` so `cargo test` still passes without a
//! database — CI builds this crate with no Postgres container.
//!
//! ```
//! TEST_DATABASE_URL=postgresql://cc:cc@127.0.0.1:15434/cc cargo test --test settings_db
//! ```
//!
//! Each case here was run through the Python `Setting.get` over the same
//! database and produced the same answer. The two that are worth having
//! a test for are the ones that are easy to get wrong in a port: a row
//! whose value is NULL must return NULL rather than falling back to the
//! default, and the past-due comparison is exact, so `"TRUE"` is not
//! past due.

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

async fn pool() -> Option<PgPool> {
    let url = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|u| !u.is_empty())?;
    // Unset means skip; SET AND UNREACHABLE means fail. This used to end
    // in `.ok()`, which turned a wrong URL into a silent skip — so the CI
    // leg that exists to run these would have reported green with every
    // one of them returning early.
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("TEST_DATABASE_URL is set but the database is unreachable");
    // And the schema is applied here, not assumed. CI hands this an EMPTY
    // database; locally it was always the differential's, which had the
    // tables already, so the assumption held everywhere except the one
    // place these are meant to run. Idempotent, and sqlx takes an advisory
    // lock, so parallel tests racing to migrate is safe.
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations must apply to the test database");
    Some(pool)
}

/// Every test shares one table, so each uses its own org_id prefix and
/// cleans up after itself rather than truncating.
async fn seed(pool: &PgPool, org: &str, value: Option<&str>) {
    sqlx::query("DELETE FROM settings WHERE org_id = $1")
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"INSERT INTO settings (org_id, "key", value) VALUES ($1, 'payment_past_due', $2)"#,
    )
    .bind(org)
    .bind(value)
    .execute(pool)
    .await
    .unwrap();
}

macro_rules! require_db {
    () => {
        match pool().await {
            Some(p) => p,
            None => {
                eprintln!("skipped: set TEST_DATABASE_URL to run");
                return;
            }
        }
    };
}

#[tokio::test]
async fn an_absent_setting_returns_the_default() {
    let pool = require_db!();
    sqlx::query("DELETE FROM settings WHERE org_id = 'sdb_missing'")
        .execute(&pool)
        .await
        .unwrap();
    let got =
        sentinel_command::settings::get(&pool, "sdb_missing", "payment_past_due", Some("false"))
            .await
            .unwrap();
    assert_eq!(got.as_deref(), Some("false"));
    assert!(
        !sentinel_command::settings::payment_past_due(&pool, "sdb_missing")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_null_value_is_not_the_same_as_an_absent_row() {
    // Python returns `setting.value` — None — rather than the default
    // once a row exists. Falling back to the default here would silently
    // change what a set-but-empty setting means.
    let pool = require_db!();
    seed(&pool, "sdb_null", None).await;
    let got = sentinel_command::settings::get(&pool, "sdb_null", "payment_past_due", Some("false"))
        .await
        .unwrap();
    assert_eq!(got, None);
}

#[tokio::test]
async fn past_due_is_an_exact_string_match() {
    let pool = require_db!();
    for (value, expected) in [
        (Some("true"), true),
        (Some("false"), false),
        // Anything but a lowercase "true" means not past due. Being
        // generous here would lock paying customers out of their
        // cameras.
        (Some("TRUE"), false),
        (Some("True"), false),
        (Some(""), false),
        (Some("1"), false),
    ] {
        let org = format!("sdb_exact_{}", value.unwrap_or("none").len());
        seed(&pool, &org, value).await;
        assert_eq!(
            sentinel_command::settings::payment_past_due(&pool, &org)
                .await
                .unwrap(),
            expected,
            "value {value:?} should give past_due={expected}"
        );
        sqlx::query("DELETE FROM settings WHERE org_id = $1")
            .bind(&org)
            .execute(&pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_duplicated_pair_still_resolves() {
    // (org_id, key) is indexed but deliberately not unique: Setting.set
    // is check-then-insert, so concurrent first-writers can transiently
    // duplicate a pair. Both stacks take an arbitrary row; what matters
    // is that neither errors.
    let pool = require_db!();
    seed(&pool, "sdb_dup", Some("true")).await;
    sqlx::query(
        r#"INSERT INTO settings (org_id, "key", value) VALUES ('sdb_dup', 'payment_past_due', 'true')"#,
    )
    .execute(&pool)
    .await
    .unwrap();

    assert!(
        sentinel_command::settings::payment_past_due(&pool, "sdb_dup")
            .await
            .unwrap()
    );

    sqlx::query("DELETE FROM settings WHERE org_id = 'sdb_dup'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn one_org_cannot_read_another_orgs_setting() {
    let pool = require_db!();
    seed(&pool, "sdb_tenant_a", Some("true")).await;
    sqlx::query("DELETE FROM settings WHERE org_id = 'sdb_tenant_b'")
        .execute(&pool)
        .await
        .unwrap();

    assert!(
        sentinel_command::settings::payment_past_due(&pool, "sdb_tenant_a")
            .await
            .unwrap()
    );
    assert!(
        !sentinel_command::settings::payment_past_due(&pool, "sdb_tenant_b")
            .await
            .unwrap(),
        "tenant B must not inherit tenant A's billing state"
    );

    sqlx::query("DELETE FROM settings WHERE org_id LIKE 'sdb_tenant_%'")
        .execute(&pool)
        .await
        .unwrap();
}
