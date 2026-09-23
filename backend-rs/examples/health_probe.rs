//! Drive the Rust health probes with every input supplied.
//!
//! The counterpart of `tests/differential/py_health_probe.py`, reading
//! the same `health_cases.json`. Run both, diff the output: that is
//! `tests/differential/health_run.sh`.
//!
//! The HTTP differential has these endpoints, but it cannot reach most
//! of what they report: the mutator restarts the tier before every run,
//! so the process is always inside its startup grace, and the harness
//! environment has email on, a valid licence and a healthy disk.
//! Seventeen mutations survived a full run for that reason.
//!
//! Usage:
//!   cargo run --example health_probe -- --db postgresql://…

use sentinel_command::config::Config;
use sentinel_command::health_probes as probes;
use serde_json::{json, Value};

#[derive(serde::Deserialize)]
struct CaseFile {
    scenarios: Vec<Scenario>,
}

#[derive(serde::Deserialize)]
struct Scenario {
    name: String,
    uptime: f64,
    local_auth: bool,
    email_enabled: bool,
    tick_age: Option<f64>,
    disk: Disk,
    #[serde(default)]
    license_key: Option<String>,
    #[serde(default)]
    settings: std::collections::BTreeMap<String, String>,
}

#[derive(serde::Deserialize)]
struct Disk {
    total: u64,
    free: u64,
    used: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut db = std::env::var("PROBE_DATABASE_URL").unwrap_or_default();
    let mut cases = "tests/differential/health_cases.json".to_string();
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => { db = args[i + 1].clone(); i += 2; }
            "--cases" => { cases = args[i + 1].clone(); i += 2; }
            _ => i += 1,
        }
    }
    if db.is_empty() {
        eprintln!("--db is required");
        std::process::exit(2);
    }
    std::env::set_var("DATABASE_URL", &db);
    std::env::set_var("LOCAL_ORG_ID", "self-host");

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&db)
        .await?;
    let client = reqwest::Client::new();

    // One live reading, reported first. It is the ONLY way to pin
    // which statvfs field each side reads: the scenarios inject
    // total/free/used directly and so cannot tell `f_bavail` from
    // `f_bfree`, and the HTTP differential normalises the numbers away
    // because a cargo build moves them between the two calls. The
    // runner compares these with a tolerance far below the difference
    // a filesystem reserve makes.
    if let Some((total, free, used)) = probes::statvfs_usage(disk_path()) {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "scenario": "@live-disk",
                "path": disk_path(),
                "bytes_total": total,
                "bytes_free": free,
                "bytes_used": used,
            }))?
        );
    }

    let file: CaseFile = serde_json::from_str(&std::fs::read_to_string(&cases)?)?;
    for scenario in &file.scenarios {
        // The licence probe reads Settings, so the rows are the fixture.
        sqlx::query("DELETE FROM settings").execute(&pool).await?;
        for (key, value) in &scenario.settings {
            // "@recent" is an hour ago; see health_cases.json.
            let value = if value == "@recent" {
                // Resolved by the runner and shared with the Python
                // probe; see health_run.sh.
                std::env::var("HEALTH_PROBE_RECENT").unwrap_or_else(|_| {
                    sentinel_command::models::iso_naive(
                        chrono::Utc::now().naive_utc() - chrono::Duration::hours(1),
                    )
                })
            } else {
                value.clone()
            };
            sqlx::query(
                r#"INSERT INTO settings (org_id, "key", value, updated_at)
                   VALUES ('self-host', $1, $2, now()::timestamp)"#,
            )
            .bind(key)
            .bind(&value)
            .execute(&pool)
            .await?;
        }

        let mut config = Config::from_env();
        config.auth_provider = if scenario.local_auth { "local".into() } else { "clerk".into() };
        config.email_enabled = scenario.email_enabled;
        config.sentinel_license_key = scenario
            .license_key
            .clone()
            .filter(|k| !k.is_empty());
        // The Clerk probe must not reach the network: local auth
        // short-circuits, and an empty secret reports unconfigured.
        config.clerk_secret_key = String::new();
        config.local_org_id = "self-host".into();

        let disk = probes::disk_result(
            disk_path(),
            scenario.disk.total,
            scenario.disk.free,
            scenario.disk.used,
        );
        let email_worker =
            probes::probe_email_worker_with(&config, scenario.uptime, scenario.tick_age);
        let clerk = probes::probe_clerk(&config, &client).await;
        let sentinel_license =
            probes::probe_sentinel_license(&config, &pool, scenario.uptime).await;
        // The injected probes go INTO the report rather than being
        // substituted afterwards, so `ready` is the rollup's own answer
        // and not something this probe recomputed — recomputing it
        // meant a mutation to the rollup changed nothing here.
        let report = probes::run_readiness_probes_with(
            &config,
            &pool,
            &client,
            scenario.uptime,
            Some(disk.clone()),
            Some(scenario.tick_age),
        )
        .await;

        let mut readiness = report.to_json();
        // Latency is a measurement, not a decision.
        if let Some(checks) = readiness["checks"].as_object_mut() {
            for value in checks.values_mut() {
                if let Some(map) = value.as_object_mut() {
                    map.remove("latency_ms");
                }
            }
        }

        println!(
            "{}",
            serde_json::to_string(&sorted(json!({
                "scenario": scenario.name,
                "disk": disk.to_json(),
                "email_worker": email_worker.to_json(),
                "clerk": clerk.to_json(),
                "sentinel_license": sentinel_license.to_json(),
                "readiness": readiness,
            })))?
        );
    }
    Ok(())
}

/// The same path rule the probe uses, so the reported `path` matches.
fn disk_path() -> &'static str {
    if std::path::Path::new("/data").is_dir() { "/data" } else { "." }
}

/// `json.dumps(..., sort_keys=True)` on the Python side; serde_json
/// preserves insertion order, so the keys are sorted here to match.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: serde_json::Map<String, Value> =
                map.into_iter().map(|(k, v)| (k, sorted(v))).collect();
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}
