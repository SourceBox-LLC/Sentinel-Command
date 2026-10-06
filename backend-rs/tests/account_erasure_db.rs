//! Erasing one person's data when their account is deleted
//! (`gdpr::erase_user_data`), against a real database.
//!
//! ```
//! TEST_DATABASE_URL=postgresql://… cargo test --test account_erasure_db
//! cargo test --features sqlite --test account_erasure_db
//! ```
//!
//! The person belongs to two organizations; a bystander shares one of
//! them. Everything personal about the person must go, in both, and
//! nothing of the bystander's may.

use sentinel_command::api::gdpr::{erase_user_data, recorded_emails, DELETED_USER};
use sentinel_command::db::Pool;

async fn pool() -> Option<Pool> {
    sentinel_command::db::test_pool(2).await
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

const ORGS: [&str; 2] = ["aer_org_a", "aer_org_b"];
const GONE: &str = "user_aer_gone";
const GONE_EMAIL: &str = "Gone@Example.test";
const STAYS: &str = "user_aer_stays";
const STAYS_EMAIL: &str = "stays@example.test";

async fn clear(pool: &Pool) {
    for org in ORGS {
        for table in [
            "stream_access_logs",
            "audit_log",
            "email_log",
            "email_outbox",
            "user_notification_state",
            "incidents",
            "sentinel_agent_keys",
        ] {
            sqlx::query(&format!("DELETE FROM {table} WHERE org_id = $1"))
                .bind(org)
                .execute(pool)
                .await
                .unwrap();
        }
    }
    sqlx::query("DELETE FROM email_suppression WHERE address LIKE '%@example.test'")
        .execute(pool)
        .await
        .unwrap();
}

async fn seed_person(pool: &Pool, org: &str, user_id: &str, email: &str) {
    let now = chrono::Utc::now().naive_utc();
    sqlx::query(
        "INSERT INTO stream_access_logs
            (user_id, user_email, org_id, camera_id, node_id, ip_address, user_agent, accessed_at)
         VALUES ($1, $2, $3, 'cam', 'node', '203.0.113.9', 'Firefox', $4)",
    )
    .bind(user_id)
    .bind(email)
    .bind(org)
    .bind(now)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO audit_log (org_id, event, ip_address, username, user_id, details, timestamp)
         VALUES ($1, 'mcp_key_created', '203.0.113.9', $2, $3, NULL, $4)",
    )
    .bind(org)
    .bind(email)
    .bind(user_id)
    .bind(now)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO email_log (org_id, recipient_email, kind, status, timestamp)
         VALUES ($1, $2, 'camera_offline', 'sent', $3)",
    )
    .bind(org)
    .bind(email)
    .bind(now)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO email_outbox
            (org_id, recipient_email, subject, body_text, body_html, kind, status, attempts, created_at)
         VALUES ($1, $2, 's', 't', 'h', 'camera_offline', 'pending', 0, $3)",
    )
    .bind(org)
    .bind(email)
    .bind(now)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO user_notification_state (clerk_user_id, org_id) VALUES ($1, $2)")
        .bind(user_id)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO incidents
            (org_id, title, summary, severity, status, created_by, resolved_by, created_at, updated_at)
         VALUES ($1, 't', 's', 'low', 'resolved', $2, $2, $3, $3)",
    )
    .bind(org)
    .bind(email)
    .bind(now)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sentinel_agent_keys (org_id, key_hash, name, created_by, created_at, revoked)
         VALUES ($1, $2, 'k', $3, $4, false)",
    )
    .bind(org)
    .bind(format!("hash-{user_id}-{org}"))
    .bind(email)
    .bind(now)
    .execute(pool)
    .await
    .unwrap();
}

async fn count(pool: &Pool, sql: &str, value: &str) -> i64 {
    let (n,): (i64,) = sqlx::query_as(sql)
        .bind(value)
        .fetch_one(pool)
        .await
        .unwrap();
    n
}

/// One test, because every step shares the same fixed ids.
#[tokio::test]
async fn a_deleted_account_leaves_nothing_personal_behind() {
    let pool = require_db!();
    clear(&pool).await;
    for org in ORGS {
        seed_person(&pool, org, GONE, GONE_EMAIL).await;
    }
    seed_person(&pool, ORGS[0], STAYS, STAYS_EMAIL).await;
    sqlx::query(
        "INSERT INTO email_suppression (address, reason, source, created_at)
         VALUES ('gone@example.test', 'bounce', 'resend', $1)",
    )
    .bind(chrono::Utc::now().naive_utc())
    .execute(&pool)
    .await
    .unwrap();

    // What the `user.deleted` webhook would find, with only the id.
    let emails = recorded_emails(&pool, GONE).await.unwrap();
    assert_eq!(emails, vec!["gone@example.test".to_string()]);

    erase_user_data(&pool, GONE, &emails).await.unwrap();

    let gone_rows = [
        "SELECT COUNT(*) FROM stream_access_logs WHERE user_id = $1",
        "SELECT COUNT(*) FROM user_notification_state WHERE clerk_user_id = $1",
        "SELECT COUNT(*) FROM audit_log WHERE user_id = $1",
    ];
    for sql in gone_rows {
        assert_eq!(count(&pool, sql, GONE).await, 0, "{sql}");
    }
    let gone_addresses = [
        "SELECT COUNT(*) FROM email_log WHERE LOWER(recipient_email) = LOWER($1)",
        "SELECT COUNT(*) FROM email_outbox WHERE LOWER(recipient_email) = LOWER($1)",
        "SELECT COUNT(*) FROM email_suppression WHERE LOWER(address) = LOWER($1)",
        "SELECT COUNT(*) FROM audit_log WHERE LOWER(username) = LOWER($1)",
        "SELECT COUNT(*) FROM incidents WHERE created_by = $1 OR resolved_by = $1",
        "SELECT COUNT(*) FROM sentinel_agent_keys WHERE created_by = $1",
    ];
    for sql in gone_addresses {
        assert_eq!(count(&pool, sql, GONE_EMAIL).await, 0, "{sql}");
    }

    // The organization's records survive, under "deleted user" and
    // without an IP address.
    let (events, without_ip): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(*) FILTER (WHERE ip_address IS NULL)
           FROM audit_log WHERE username = $1",
    )
    .bind(DELETED_USER)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((events, without_ip), (2, 2));
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM incidents WHERE created_by = $1 AND resolved_by = $1",
            DELETED_USER
        )
        .await,
        2
    );

    // The bystander is untouched.
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM stream_access_logs WHERE user_id = $1",
            STAYS
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM audit_log WHERE user_id = $1 AND ip_address IS NOT NULL",
            STAYS
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM email_outbox WHERE recipient_email = $1",
            STAYS_EMAIL
        )
        .await,
        1
    );

    // Running it again is harmless.
    erase_user_data(&pool, GONE, &emails).await.unwrap();
    clear(&pool).await;
}
