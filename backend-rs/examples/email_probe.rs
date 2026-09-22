//! Drive the Rust email worker over the same fake Resend the Python
//! probe uses.
//!
//! The counterpart of `tests/differential/py_email_probe.py`, reading
//! the same `email_cases.json`. Run both, diff the output: that is
//! `tests/differential/email_run.sh`.
//!
//! This exists because the worker has no HTTP surface. It is a loop,
//! and the write differential could only reach it by waiting for a
//! timer — the same race the harness pins every other loop out of the
//! way to avoid. `run_one_tick` is a function over a pool, so both
//! stacks can be driven directly and compared.
//!
//! Usage:
//!   cargo run --example email_probe -- \
//!     --resend http://127.0.0.1:18095 --db postgresql://…

use std::collections::BTreeMap;

use sentinel_command::config::Config;
use sentinel_command::email_worker::{
    reset_tick_for_tests, run_one_tick, seconds_since_last_tick, EmailContext,
};
use serde_json::{json, Value};
use sqlx::Row;

#[derive(serde::Deserialize)]
struct CaseFile {
    scenarios: Vec<Scenario>,
}

#[derive(serde::Deserialize)]
struct Scenario {
    name: String,
    #[serde(default = "one")]
    ticks: u32,
    #[serde(default)]
    rows: Vec<SeedRow>,
    #[serde(default)]
    suppressed: Vec<Value>,
    #[serde(default)]
    email_enabled: Option<bool>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    batch_size: Option<i64>,
}

fn one() -> u32 {
    1
}

#[derive(serde::Deserialize)]
struct SeedRow {
    id: i32,
    recipient: String,
    kind: String,
    status: String,
    attempts: i32,
    #[serde(default)]
    org_id: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body_text: Option<String>,
    #[serde(default)]
    body_html: Option<String>,
    #[serde(default)]
    created_offset: Option<i64>,
    #[serde(default)]
    last_attempt_offset: Option<i64>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut resend = String::new();
    let mut db = std::env::var("PROBE_DATABASE_URL").unwrap_or_default();
    let mut cases = "tests/differential/email_cases.json".to_string();
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--resend" => { resend = args[i + 1].clone(); i += 2; }
            "--db" => { db = args[i + 1].clone(); i += 2; }
            "--cases" => { cases = args[i + 1].clone(); i += 2; }
            _ => i += 1,
        }
    }
    if resend.is_empty() {
        eprintln!("--resend is required");
        std::process::exit(2);
    }

    // Read before Config::from_env, which reads the environment once.
    std::env::set_var("RESEND_API_URL", &resend);
    std::env::set_var("RESEND_API_KEY", "re_probe_key");
    std::env::set_var("EMAIL_ENABLED", "true");
    std::env::set_var("EMAIL_FROM_ADDRESS", "notifications@example.com");
    std::env::set_var("EMAIL_FROM_NAME", "Sentinel");
    std::env::set_var("EMAIL_MAX_ATTEMPTS", "3");
    std::env::set_var("EMAIL_WORKER_BATCH_SIZE", "20");
    if !db.is_empty() {
        std::env::set_var("DATABASE_URL", &db);
    }

    let base = Config::from_env();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&db)
        .await?;
    let client = reqwest::Client::new();

    let file: CaseFile = serde_json::from_str(&std::fs::read_to_string(&cases)?)?;
    for scenario in &file.scenarios {
        reset(&pool).await?;
        seed(&pool, scenario).await?;

        let mut config = Config::from_env();
        config.email_enabled = scenario.email_enabled.unwrap_or(true);
        if let Some(key) = &scenario.api_key {
            config.resend_api_key = key.clone();
        }
        config.email_worker_batch_size = scenario.batch_size.unwrap_or(20);
        config.email_max_attempts = base.email_max_attempts;

        let ctx = EmailContext { pool: &pool, config: &config, client: &client };
        // Reset per scenario: "did this tick stamp" must be a question
        // about this tick and not about an earlier one in the process.
        reset_tick_for_tests();
        let mut summaries = Vec::new();
        for _ in 0..scenario.ticks {
            summaries.push(run_one_tick(&ctx).await?);
        }
        // Wall-clock, so only its presence is compared.
        let ticked = seconds_since_last_tick().is_some();

        println!(
            "{}",
            serde_json::to_string(&json!({
                "scenario": scenario.name,
                "summaries": summaries,
                "ticked": ticked,
                "outbox": dump_outbox(&pool).await?,
                "log": dump_log(&pool).await?,
            }))?
        );
    }
    Ok(())
}

async fn reset(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    for table in ["email_log", "email_outbox", "email_suppression"] {
        sqlx::query(&format!("DELETE FROM {table}")).execute(pool).await?;
    }
    Ok(())
}

async fn seed(pool: &sqlx::PgPool, scenario: &Scenario) -> Result<(), sqlx::Error> {
    let now = sentinel_command::models::now_naive();
    for entry in &scenario.suppressed {
        let (address, reason) = match entry {
            Value::String(s) => (s.clone(), Some("bounce".to_string())),
            Value::Array(pair) => (
                pair[0].as_str().unwrap_or_default().to_string(),
                pair.get(1).and_then(Value::as_str).map(str::to_string),
            ),
            _ => continue,
        };
        sqlx::query(
            "INSERT INTO email_suppression (address, reason, source, created_at)
             VALUES ($1, $2, 'probe', $3)",
        )
        .bind(address.to_lowercase())
        .bind(reason)
        .bind(now)
        .execute(pool)
        .await?;
    }
    for row in &scenario.rows {
        sqlx::query(
            "INSERT INTO email_outbox
                (id, org_id, recipient_email, subject, body_text, body_html, kind,
                 status, attempts, created_at, last_attempt_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(row.id)
        .bind(row.org_id.clone().unwrap_or_else(|| "self-host".to_string()))
        .bind(&row.recipient)
        .bind(row.subject.clone().unwrap_or_else(|| "Subject".to_string()))
        .bind(row.body_text.clone().unwrap_or_else(|| "text body".to_string()))
        .bind(row.body_html.clone().unwrap_or_else(|| "<p>html body</p>".to_string()))
        .bind(&row.kind)
        .bind(&row.status)
        .bind(row.attempts)
        .bind(now - chrono::Duration::seconds(row.created_offset.unwrap_or(0)))
        .bind(row.last_attempt_offset.map(|o| now - chrono::Duration::seconds(o)))
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn dump_outbox(pool: &sqlx::PgPool) -> Result<Vec<BTreeMap<String, Value>>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, org_id, recipient_email, kind, status, attempts,
                resend_message_id, error, sent_at, last_attempt_at
           FROM email_outbox ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            BTreeMap::from([
                ("id".into(), json!(r.get::<i32, _>("id"))),
                ("org_id".into(), json!(r.get::<String, _>("org_id"))),
                ("recipient".into(), json!(r.get::<String, _>("recipient_email"))),
                ("kind".into(), json!(r.get::<String, _>("kind"))),
                ("status".into(), json!(r.get::<String, _>("status"))),
                ("attempts".into(), json!(r.get::<i32, _>("attempts"))),
                ("resend_message_id".into(),
                 json!(r.get::<Option<String>, _>("resend_message_id"))),
                ("error".into(), json!(r.get::<Option<String>, _>("error"))),
                ("sent".into(),
                 json!(r.get::<Option<chrono::NaiveDateTime>, _>("sent_at").is_some())),
                ("attempted".into(),
                 json!(r.get::<Option<chrono::NaiveDateTime>, _>("last_attempt_at").is_some())),
            ])
        })
        .collect())
}

async fn dump_log(pool: &sqlx::PgPool) -> Result<Vec<BTreeMap<String, Value>>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT org_id, recipient_email, kind, status, resend_message_id, error
           FROM email_log ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            BTreeMap::from([
                ("org_id".into(), json!(r.get::<String, _>("org_id"))),
                ("recipient".into(), json!(r.get::<String, _>("recipient_email"))),
                ("kind".into(), json!(r.get::<String, _>("kind"))),
                ("status".into(), json!(r.get::<String, _>("status"))),
                ("resend_message_id".into(),
                 json!(r.get::<Option<String>, _>("resend_message_id"))),
                ("error".into(), json!(r.get::<Option<String>, _>("error"))),
            ])
        })
        .collect())
}
