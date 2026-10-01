//! Camera-cap enforcement against a real Postgres.
//!
//! Gated on `TEST_DATABASE_URL`, like the other database tests, so
//! `cargo test` still passes without one.
//!
//! ```
//! TEST_DATABASE_URL=postgresql://cc:cc@127.0.0.1:15434/cc cargo test --test plans_db
//! ```
//!
//! The differential cannot reach this yet: every route that calls
//! `enforce_camera_cap` — registration, the heartbeat, the subscription
//! webhooks — is still Python's, so nothing exercises it over HTTP.
//! What it decides is also easy to get subtly wrong and hard to notice:
//! disabling the wrong cameras leaves the right *number* running, which
//! is what a count-based check would look at.

use sentinel_command::plans::{enforce_camera_cap, PlanContext};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::Mutex;

/// These tests share one database with the differential harness, and
/// they run serially against it for two reasons that are not about
/// each other:
///
///   * `cameras.camera_id` is globally unique, not per-org, so two
///     fixtures cannot hold the same name even under different orgs
///     (they also use distinct names, belt and braces);
///   * the harness inserts rows with explicit ids, which leaves the
///     `settings` id sequence pointing at an id that already exists.
///     `sync_sequences` repairs it, and that repair has to not race.
static DB: Mutex<()> = Mutex::const_new(());

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

/// Walk each id sequence past the largest id actually present.
///
/// Inserting with an explicit id does not advance the sequence, so a
/// database the harness has written to hands out ids that collide.
/// That is a property of the shared fixture, not of the code under
/// test — repair it rather than work around it.
async fn sync_sequences(pool: &PgPool) {
    for table in ["settings", "cameras"] {
        let sql = format!(
            "SELECT setval(pg_get_serial_sequence('{table}', 'id'),
                           GREATEST((SELECT COALESCE(MAX(id), 1) FROM {table}), 1))"
        );
        sqlx::query(&sql).execute(pool).await.unwrap();
    }
}

async fn cleanup(pool: &PgPool, org: &str) {
    for sql in [
        "DELETE FROM cameras WHERE org_id = $1",
        "DELETE FROM settings WHERE org_id = $1",
    ] {
        sqlx::query(sql).bind(org).execute(pool).await.unwrap();
    }
}

/// `created_at` decides which cameras survive, so each one is given an
/// explicit age rather than whatever the clock says.
async fn add_camera(pool: &PgPool, org: &str, camera_id: &str, age_days: i64, disabled: bool) {
    sqlx::query(
        "INSERT INTO cameras (camera_id, org_id, name, status, created_at, disabled_by_plan,
                              node_type, capabilities, continuous_24_7, scheduled_recording,
                              updated_at)
         VALUES ($1, $2, $1, 'online', now()::timestamp - make_interval(days => $3::int),
                 $4, 'rtsp', 'streaming', false, false, now()::timestamp)",
    )
    .bind(camera_id)
    .bind(org)
    .bind(age_days as i32)
    .bind(disabled)
    .execute(pool)
    .await
    .unwrap();
}

async fn set_plan(pool: &PgPool, org: &str, slug: &str) {
    sqlx::query(
        "INSERT INTO settings (org_id, key, value, updated_at)
         VALUES ($1, 'org_plan', $2, now()::timestamp)",
    )
    .bind(org)
    .bind(slug)
    .execute(pool)
    .await
    .unwrap();
}

async fn flags(pool: &PgPool, org: &str) -> Vec<(String, bool)> {
    sqlx::query_as(
        "SELECT camera_id, disabled_by_plan FROM cameras WHERE org_id = $1 ORDER BY camera_id",
    )
    .bind(org)
    .fetch_all(pool)
    .await
    .unwrap()
}

fn context(pool: &PgPool, client: &reqwest::Client) -> PlanContext<'static> {
    // The context borrows; leaking here keeps the test terse and the
    // process is about to end anyway.
    PlanContext {
        pool: Box::leak(Box::new(pool.clone())),
        client: Box::leak(Box::new(client.clone())),
        // An empty base URL makes the live lookup fail to build a
        // request, so `resolve_org_plan` keeps whatever
        // `Setting(org_plan)` says. That is the point: these tests are
        // about the cap, not about Clerk.
        clerk_base_url: "",
        clerk_secret: "",
        // NOT local auth. `resolve_org_plan` short-circuits to
        // `self_host` before it reads a setting at all, so every plan
        // these tests write would be ignored and every cap would be
        // self_host's 100.
        local_auth: false,
    }
}

#[tokio::test]
async fn the_oldest_cameras_are_the_ones_kept() {
    let _guard = DB.lock().await;
    let org = "plans-db-oldest";
    let Some(pool) = pool().await else { return };
    sync_sequences(&pool).await;
    cleanup(&pool, org).await;
    let client = reqwest::Client::new();

    // Free allows five. Eight cameras, oldest first by age.
    set_plan(&pool, org, "free_org").await;
    for (id, age) in [
        ("keep-1", 80),
        ("keep-2", 70),
        ("keep-3", 60),
        ("keep-4", 50),
        ("keep-5", 40),
        ("keep-6", 30),
        ("keep-7", 20),
        ("keep-8", 10),
    ] {
        add_camera(&pool, org, id, age, false).await;
    }

    let ctx = context(&pool, &client);
    let outcome = enforce_camera_cap(&ctx, &pool, org).await.unwrap();
    assert!(outcome.changed);
    assert_eq!(outcome.plan, "free");
    assert_eq!(outcome.max_cameras, 5);
    assert_eq!(
        outcome.enabled,
        vec!["keep-1", "keep-2", "keep-3", "keep-4", "keep-5"]
    );
    assert_eq!(outcome.disabled, vec!["keep-6", "keep-7", "keep-8"]);
    assert_eq!(
        flags(&pool, org).await,
        vec![
            ("keep-1".into(), false),
            ("keep-2".into(), false),
            ("keep-3".into(), false),
            ("keep-4".into(), false),
            ("keep-5".into(), false),
            ("keep-6".into(), true),
            ("keep-7".into(), true),
            ("keep-8".into(), true),
        ]
    );

    // Idempotent: a second call flips nothing.
    let again = enforce_camera_cap(&ctx, &pool, org).await.unwrap();
    assert!(!again.changed, "a second call should have nothing to flip");
    assert_eq!(again.disabled, outcome.disabled);

    cleanup(&pool, org).await;
}

#[tokio::test]
async fn raising_the_cap_lights_the_same_rows_back_up() {
    let _guard = DB.lock().await;
    let org = "plans-db-upgrade";
    let Some(pool) = pool().await else { return };
    sync_sequences(&pool).await;
    cleanup(&pool, org).await;
    let client = reqwest::Client::new();

    // Everything already suspended under the free cap.
    set_plan(&pool, org, "free_org").await;
    for (id, age) in [
        ("up-a", 70),
        ("up-b", 60),
        ("up-c", 50),
        ("up-d", 40),
        ("up-e", 30),
        ("up-f", 20),
        ("up-g", 10),
    ] {
        add_camera(&pool, org, id, age, true).await;
    }
    let ctx = context(&pool, &client);
    enforce_camera_cap(&ctx, &pool, org).await.unwrap();
    assert_eq!(
        flags(&pool, org).await,
        vec![
            ("up-a".into(), false),
            ("up-b".into(), false),
            ("up-c".into(), false),
            ("up-d".into(), false),
            ("up-e".into(), false),
            ("up-f".into(), true),
            ("up-g".into(), true),
        ]
    );

    // The upgrade clears the rest in the same call — nothing was
    // deleted, so the rows come back with their metadata intact.
    sqlx::query("UPDATE settings SET value = 'pro' WHERE org_id = $1 AND key = 'org_plan'")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    let outcome = enforce_camera_cap(&ctx, &pool, org).await.unwrap();
    assert!(outcome.changed);
    assert_eq!(outcome.plan, "pro");
    assert!(outcome.disabled.is_empty());
    assert!(flags(&pool, org)
        .await
        .iter()
        .all(|(_, disabled)| !disabled));

    cleanup(&pool, org).await;
}

/// A null `created_at` must sort *last*, not first. Postgres orders
/// nulls first on an ascending sort by default, which would disable the
/// oldest cameras and keep the newest — the exact inverse, and with the
/// right number of cameras running either way.
#[tokio::test]
async fn a_camera_with_no_creation_time_sorts_last() {
    let _guard = DB.lock().await;
    let org = "plans-db-nulls";
    let Some(pool) = pool().await else { return };
    sync_sequences(&pool).await;
    cleanup(&pool, org).await;
    let client = reqwest::Client::new();

    set_plan(&pool, org, "free_org").await;
    for (id, age) in [
        ("null-1", 60),
        ("null-2", 50),
        ("null-3", 40),
        ("null-4", 30),
        ("null-5", 20),
    ] {
        add_camera(&pool, org, id, age, false).await;
    }
    add_camera(&pool, org, "null-new", 10, false).await;
    sqlx::query(
        "UPDATE cameras SET created_at = NULL WHERE camera_id = 'null-new' AND org_id = $1",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();

    let ctx = context(&pool, &client);
    let outcome = enforce_camera_cap(&ctx, &pool, org).await.unwrap();
    // Sorted nulls-first this would keep `null-new` and disable
    // `null-5` — five cameras running either way.
    assert_eq!(
        outcome.enabled,
        vec!["null-1", "null-2", "null-3", "null-4", "null-5"]
    );
    assert_eq!(outcome.disabled, vec!["null-new"]);

    cleanup(&pool, org).await;
}
