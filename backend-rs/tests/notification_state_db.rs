//! The read-cursor row under concurrency, against a real Postgres.
//!
//! Gated on `TEST_DATABASE_URL` like the other database tests.
//!
//! This exists because of a 500 on the first page a new user ever sees.
//! The dashboard asks for the inbox, the unread count and the SSE stream
//! at once; all three create the caller's `user_notification_state` row
//! if it is missing; and check-then-insert let two of them both insert.
//! Nothing serial can reproduce it — the differential ran one request at
//! a time and scored this path identical for the whole port — so the test
//! is twenty callers at once, which is what a browser does.

use sentinel_command::api::notifications::get_or_init_state;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

async fn pool() -> Option<PgPool> {
    let url = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|u| !u.is_empty())?;
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&url)
        .await
        .expect("TEST_DATABASE_URL is set but the database is unreachable");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations must apply to the test database");
    Some(pool)
}

#[tokio::test]
async fn twenty_first_requests_at_once_make_one_row_and_no_error() {
    let Some(pool) = pool().await else {
        eprintln!("skipped: set TEST_DATABASE_URL to run");
        return;
    };
    let org = "race-org";
    let user = format!("race-user-{}", uuid::Uuid::new_v4());

    let mut tasks = Vec::new();
    for _ in 0..20 {
        let (pool, user) = (pool.clone(), user.clone());
        tasks.push(tokio::spawn(async move {
            get_or_init_state(&pool, &user, org).await
        }));
    }
    let mut seen = Vec::new();
    for task in tasks {
        // The old code failed here: a unique violation surfaced as an
        // ApiError, which the handler turned into a 500.
        let (last_viewed, cleared) = task
            .await
            .unwrap()
            .unwrap_or_else(|_| panic!("a concurrent first request errored"));
        assert!(cleared.is_none());
        seen.push(last_viewed.expect("a fresh row has a last_viewed_at"));
    }

    // One row, and every caller saw the WINNER's timestamp — two tabs
    // that disagreed about it would disagree about what is unread.
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_notification_state WHERE clerk_user_id = $1 AND org_id = $2",
    )
    .bind(&user)
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 1);
    assert!(
        seen.iter().all(|ts| *ts == seen[0]),
        "callers saw different cursors"
    );

    sqlx::query("DELETE FROM user_notification_state WHERE clerk_user_id = $1")
        .bind(&user)
        .execute(&pool)
        .await
        .unwrap();
}
