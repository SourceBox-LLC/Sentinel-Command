//! Resolve plans through the Rust `plans` module, over the same fake
//! Clerk the Python probe uses.
//!
//! The counterpart of `tests/differential/py_plan_probe.py`, reading
//! the same `plan_cases.json`. Run both, diff the output: that is
//! `tests/differential/plan_run.sh`.
//!
//! This exists because the HTTP differential is structurally blind to
//! this code. `resolve_org_plan` short-circuits to `"self_host"` under
//! `AUTH_PROVIDER=local`, and both tiers there run local auth — so the
//! entitlement rules, the caches, and the grace window have never
//! executed under any harness in this directory.
//!
//! Usage:
//!   cargo run --example plan_probe -- --clerk http://127.0.0.1:18080/v1

use std::collections::BTreeMap;

use chrono::{Duration, Utc};
use sentinel_command::plans::{
    self, effective_plan_for_caps, get_plan_display_name, get_plan_limits, PlanContext,
};
use serde_json::{json, Value};

#[derive(serde::Deserialize)]
struct CaseFile {
    org_id: String,
    cases: Vec<Case>,
}

#[derive(serde::Deserialize)]
struct Case {
    case: String,
    scenario: String,
    settings: BTreeMap<String, String>,
}

/// Expand the `@-Nd` shorthand into an ISO timestamp, the same three
/// spellings the Python probe produces: aware, `Z`-suffixed, and naive.
fn resolve_timestamp(spec: &str) -> String {
    let Some(body) = spec.strip_prefix('@') else {
        return spec.to_string();
    };
    let (body, z) = match body.strip_suffix('Z') {
        Some(rest) => (rest, true),
        None => (body, false),
    };
    let (body, naive) = match body.strip_suffix("naive") {
        Some(rest) => (rest, true),
        None => (body, false),
    };
    let days: f64 = body.trim_end_matches('d').parse().unwrap_or(0.0);
    let dt = Utc::now() + Duration::milliseconds((days * 86_400_000.0) as i64);
    if naive {
        return dt.naive_utc().format("%Y-%m-%dT%H:%M:%S%.6f").to_string();
    }
    if z {
        return format!("{}Z", dt.naive_utc().format("%Y-%m-%dT%H:%M:%S%.6f"));
    }
    dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut clerk = None;
    let mut database_url = std::env::var("PROBE_DATABASE_URL").ok();
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--clerk" => {
                clerk = args.get(i + 1).cloned();
                i += 2;
            }
            "--db" => {
                database_url = args.get(i + 1).cloned();
                i += 2;
            }
            _ => i += 1,
        }
    }
    let clerk = clerk.ok_or("--clerk is required")?;
    let database_url = database_url.ok_or("--db or PROBE_DATABASE_URL is required")?;
    let root = clerk.rsplit_once("/v1").map(|(a, _)| a.to_string()).unwrap_or_default();

    let spec: CaseFile = serde_json::from_str(&std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/differential/plan_cases.json"),
    )?)?;

    let pool = sqlx::PgPool::connect(&database_url).await?;
    let client = reqwest::Client::new();
    client.post(format!("{root}/__reset")).send().await?;

    let ctx = PlanContext {
        pool: &pool,
        client: &client,
        clerk_base_url: &clerk,
        clerk_secret: "sk_test_fake",
        // Deliberately false. The whole point is to exercise the path
        // the local-auth short-circuit hides.
        local_auth: false,
    };

    for case in &spec.cases {
        client
            .post(format!("{root}/__scenario"))
            .json(&json!({ &spec.org_id: &case.scenario }))
            .send()
            .await?;

        for (key, value) in &case.settings {
            sentinel_command::settings::set(&pool, &spec.org_id, key, &resolve_timestamp(value))
                .await?;
        }
        // Clear anything a previous case left behind, so cases cannot
        // leak into one another.
        for key in ["payment_past_due", "payment_past_due_at"] {
            if !case.settings.contains_key(key) {
                sentinel_command::settings::set(&pool, &spec.org_id, key, "").await?;
            }
        }

        plans::invalidate_effective_plan_cache(None);
        plans::reset_resolve_throttle();

        let plan = effective_plan_for_caps(&ctx, &spec.org_id, false).await;
        let limits = get_plan_limits(&plan);
        // Key order matches the Python's `json.dumps(sort_keys=True)`
        // so the two outputs diff literally.
        let out = json!({
            "case": case.case,
            "display": get_plan_display_name(&plan),
            "limits": {
                "log_retention_days": limits.log_retention_days,
                "max_cameras": limits.max_cameras,
                "max_nodes": limits.max_nodes,
                "max_seats": limits.max_seats,
                "max_sse_subscribers": limits.max_sse_subscribers,
                "max_viewer_hours_per_month": limits.max_viewer_hours_per_month,
            },
            "plan": plan,
        });
        println!("{}", serde_json::to_string(&out)?);
    }

    // The same coverage guard the Python probe applies: if the fake was
    // never called, every case took a cached fast path and a green diff
    // would mean nothing.
    let calls: Value = client
        .get(format!("{root}/__calls"))
        .send()
        .await?
        .json()
        .await?;
    let live = calls.get(&spec.org_id).and_then(Value::as_u64).unwrap_or(0);
    let expected: u64 = spec
        .cases
        .iter()
        .filter(|c| {
            !["pro", "pro_plus", "self_host"]
                .contains(&c.settings.get("org_plan").map(String::as_str).unwrap_or(""))
        })
        .count() as u64;
    if live < expected {
        eprintln!(
            "REFUSING: fake Clerk was called {live} times but {expected} cases \
             should have gone live — the live path is not being exercised"
        );
        std::process::exit(2);
    }
    eprintln!(
        "coverage: {live} live Clerk lookups across {} cases",
        spec.cases.len()
    );
    Ok(())
}
