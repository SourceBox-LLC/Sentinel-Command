//! Viewer-hour accounting against a real Postgres.
//!
//! Gated on `TEST_DATABASE_URL`, like `settings_db.rs`, so `cargo test`
//! still passes with no database.
//!
//! ```
//! TEST_DATABASE_URL=postgresql://cc:cc@127.0.0.1:15434/cc cargo test --test hls_db
//! ```
//!
//! The flush is the half of the cache the differential cannot see: the
//! HTTP harness compares what a segment request answers, and this is
//! what happens a minute later. Getting it wrong is invisible until an
//! org's usage silently stops counting — or counts twice.

use sentinel_command::db::Pool as PgPool;
use sentinel_command::hls::HlsCache;

async fn pool() -> Option<PgPool> {
    // Skips without TEST_DATABASE_URL on the PostgreSQL build; always
    // runs, on a fresh file, on the SQLite build. See `db::test_pool`.
    sentinel_command::db::test_pool(2).await
}

fn year_month() -> String {
    chrono::Utc::now().format("%Y-%m").to_string()
}

async fn cleanup(pool: &PgPool, org: &str) {
    sqlx::query("DELETE FROM org_monthly_usage WHERE org_id = $1")
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
}

async fn stored(pool: &PgPool, org: &str) -> Option<i32> {
    sqlx::query_scalar(
        "SELECT viewer_seconds FROM org_monthly_usage WHERE org_id = $1 AND year_month = $2",
    )
    .bind(org)
    .bind(year_month())
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_flush_inserts_then_accumulates() {
    let Some(pool) = pool().await else { return };
    let org = "hls-db-accumulate";
    cleanup(&pool, org).await;

    let cache = HlsCache::new();
    for _ in 0..3 {
        cache.record_viewer_second(org);
    }
    assert_eq!(cache.flush_viewer_usage(&pool).await, 1);
    assert_eq!(stored(&pool, org).await, Some(3));

    // A second flush adds to the row rather than replacing it, and the
    // in-memory total follows the database.
    for _ in 0..2 {
        cache.record_viewer_second(org);
    }
    assert_eq!(cache.flush_viewer_usage(&pool).await, 1);
    assert_eq!(stored(&pool, org).await, Some(5));
    assert_eq!(cache.warm_viewer_seconds(&pool, org).await, 5);

    cleanup(&pool, org).await;
}

#[tokio::test]
async fn nothing_pending_writes_nothing() {
    let Some(pool) = pool().await else { return };
    let org = "hls-db-empty";
    cleanup(&pool, org).await;

    let cache = HlsCache::new();
    assert_eq!(cache.flush_viewer_usage(&pool).await, 0);
    assert_eq!(stored(&pool, org).await, None);

    cleanup(&pool, org).await;
}

/// The counter a segment request reads is the stored total plus what
/// has not been written yet — otherwise the cap would forget every
/// segment served since the last flush.
#[tokio::test]
async fn the_warm_total_includes_what_is_still_pending() {
    let Some(pool) = pool().await else { return };
    let org = "hls-db-pending";
    cleanup(&pool, org).await;
    sqlx::query(
        "INSERT INTO org_monthly_usage (org_id, year_month, viewer_seconds, updated_at)
              VALUES ($1, $2, 100, $3)",
    )
    .bind(org)
    .bind(year_month())
    .bind(chrono::Utc::now().naive_utc())
    .execute(&pool)
    .await
    .unwrap();

    let cache = HlsCache::new();
    assert_eq!(cache.warm_viewer_seconds(&pool, org).await, 100);
    cache.record_viewer_second(org);
    cache.record_viewer_second(org);
    assert_eq!(cache.warm_viewer_seconds(&pool, org).await, 102);

    // And the flush lands on top of the row that was already there.
    assert_eq!(cache.flush_viewer_usage(&pool).await, 1);
    assert_eq!(stored(&pool, org).await, Some(102));

    cleanup(&pool, org).await;
}

/// An org nobody has served a segment for reads as zero rather than as
/// a missing row — the dashboard shows "0 hours used", not an error.
#[tokio::test]
async fn an_org_with_no_row_warms_to_zero() {
    let Some(pool) = pool().await else { return };
    let org = "hls-db-absent";
    cleanup(&pool, org).await;

    let cache = HlsCache::new();
    assert_eq!(cache.warm_viewer_seconds(&pool, org).await, 0);
    assert_eq!(stored(&pool, org).await, None);
}
