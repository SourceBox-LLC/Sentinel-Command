//! The readiness probes behind `/api/health/ready` and
//! `/api/health/detailed`.
//!
//! Ported from `backend/app/core/health_probes.py`. Both endpoints run
//! the SAME probe functions, deliberately: two implementations would
//! eventually disagree, and "ready says we are up, detailed says we are
//! down" is the one answer a status page cannot act on.
//!
//! Five statuses, and the distinctions between them carry weight:
//!
//!   ok            passed
//!   warn          degraded but serving — yellow, nothing pages
//!   critical      the readiness rollup answers 503 and someone is paged
//!   disabled      deliberately off, which is not a failure
//!   unconfigured  on but missing config, which is also not a failure
//!
//! `disabled` versus `unconfigured` is the pair worth keeping straight:
//! a self-hosted install has no Clerk account by design, and reporting
//! that as unconfigured would read to an operator as a setup step they
//! forgot.

use serde_json::{json, Map, Value};

use crate::config::Config;

/// `EMAIL_WORKER_STALE_AFTER_SECONDS` — twelve ticks' grace at the
/// default five-second interval. Below that a wedge is more likely one
/// slow Resend call than a dead loop.
const EMAIL_WORKER_STALE_AFTER_SECONDS: f64 = 60.0;
/// `EMAIL_WORKER_STARTUP_GRACE_SECONDS`.
const EMAIL_WORKER_STARTUP_GRACE_SECONDS: f64 = 30.0;
/// `SENTINEL_LICENSE_PROBE_STARTUP_GRACE_SECONDS`.
const LICENSE_STARTUP_GRACE_SECONDS: f64 = 30.0;

/// Matching `/api/health/detailed`'s thresholds, so the alert and the
/// dashboard cannot disagree about what "full" means.
const DISK_CRITICAL_PCT: f64 = 95.0;
const DISK_WARN_PCT: f64 = 80.0;

#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub status: &'static str,
    pub data: Map<String, Value>,
}

impl ProbeResult {
    fn new(status: &'static str, data: Value) -> Self {
        Self {
            status,
            data: data.as_object().cloned().unwrap_or_default(),
        }
    }

    /// `{"status": …, **data}` — the status first, then the payload
    /// flattened beside it rather than nested under a key.
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("status".to_string(), json!(self.status));
        for (key, value) in &self.data {
            out.insert(key.clone(), value.clone());
        }
        Value::Object(out)
    }

    pub fn is_critical(&self) -> bool {
        self.status == "critical"
    }
}

/// `SELECT 1`. The most pager-worthy signal there is: every meaningful
/// request reads or writes.
pub async fn probe_database(pool: &sqlx::PgPool) -> ProbeResult {
    let started = std::time::Instant::now();
    match sqlx::query("SELECT 1").execute(pool).await {
        Ok(_) => {
            let latency = crate::pyrepr::round_half_even(started.elapsed().as_secs_f64() * 1000.0);
            ProbeResult::new("ok", json!({ "latency_ms": latency }))
        }
        Err(err) => {
            // The exception text is NOT surfaced: a connection string
            // or a hostname would leak onto a public endpoint. The
            // class name is enough to triage from, and the detail goes
            // to the log.
            tracing::warn!(error = %err, "[Health] DB ping failed");
            ProbeResult::new("critical", json!({ "error_class": error_class(&err) }))
        }
    }
}

/// The name Python's `type(exc).__name__` would give for a SQLAlchemy
/// failure.
///
/// Every database error raised through SQLAlchemy's DBAPI wrapper
/// arrives as `OperationalError` — a dropped connection, a refused
/// one, a timeout. That is the only one this can produce in practice,
/// and guessing more precisely would be inventing a difference.
fn error_class(_err: &sqlx::Error) -> &'static str {
    "OperationalError"
}

/// Clerk reachability. Critical because no JWT verifies without it, so
/// the app is effectively down while `/api/health` still says ok.
pub async fn probe_clerk(config: &Config, client: &reqwest::Client) -> ProbeResult {
    if config.is_local_auth() {
        // By design, not a forgotten step.
        return ProbeResult::new("disabled", json!({}));
    }
    if config.clerk_secret_key.is_empty() {
        return ProbeResult::new("unconfigured", json!({}));
    }

    let base = if config.clerk_api_url.ends_with('/') {
        config.clerk_api_url.clone()
    } else {
        format!("{}/", config.clerk_api_url)
    };
    let Ok(url) = reqwest::Url::parse(&base).and_then(|b| b.join("organizations?limit=1")) else {
        return ProbeResult::new("critical", json!({ "error_class": "ValueError" }));
    };

    let started = std::time::Instant::now();
    let response = client
        .get(url)
        .bearer_auth(&config.clerk_secret_key)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => {
            let latency = crate::pyrepr::round_half_even(started.elapsed().as_secs_f64() * 1000.0);
            ProbeResult::new("ok", json!({ "latency_ms": latency }))
        }
        Ok(_) => {
            tracing::warn!("[Health] Clerk probe failed");
            ProbeResult::new("critical", json!({ "error_class": "SDKError" }))
        }
        Err(err) if err.is_timeout() => {
            tracing::warn!("[Health] Clerk probe timed out");
            ProbeResult::new(
                "critical",
                json!({ "error_class": "Timeout", "timeout_seconds": 5.0 }),
            )
        }
        Err(err) => {
            tracing::warn!(error = %err, "[Health] Clerk probe failed");
            ProbeResult::new("critical", json!({ "error_class": "SDKError" }))
        }
    }
}

/// `shutil.disk_usage`, and the same path rule: `/data` where the
/// volume is mounted, the working directory otherwise, so the endpoint
/// stays informative in development.
pub fn probe_disk() -> ProbeResult {
    let path = if std::path::Path::new("/data").is_dir() { "/data" } else { "." };
    let Some((total, free, used)) = statvfs_usage(path) else {
        tracing::warn!(path, "[Health] disk_usage failed");
        return ProbeResult::new("critical", json!({ "path": path, "error_class": "OSError" }));
    };
    disk_result(path, total, free, used)
}

/// The thresholds and the arithmetic, separated from the syscall.
///
/// Split so a differential can feed both stacks the SAME numbers: the
/// real filesystem moves between two calls, and a harness that compares
/// live readings is either flaky or blind. Python's probe is driven the
/// same way, with `shutil.disk_usage` patched in the probe process.
pub fn disk_result(path: &str, total: u64, free: u64, used: u64) -> ProbeResult {
    let pct = if total > 0 {
        crate::pyrepr::round_to((used as f64 / total as f64) * 100.0, 1)
    } else {
        0.0
    };
    let status = if pct >= DISK_CRITICAL_PCT {
        "critical"
    } else if pct >= DISK_WARN_PCT {
        "warn"
    } else {
        "ok"
    };
    ProbeResult::new(
        status,
        json!({
            "path": path,
            "bytes_used": used,
            "bytes_free": free,
            "bytes_total": total,
            "percent_used": pct,
        }),
    )
}

/// `(total, free, used)`, defined the way `shutil.disk_usage` defines
/// them: free is what an unprivileged process can use (`f_bavail`),
/// while used counts the reserved blocks a privileged one still could.
fn statvfs_usage(path: &str) -> Option<(u64, u64, u64)> {
    let c_path = std::ffi::CString::new(path).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid NUL-terminated string and `stat` is a
    // fully owned, correctly sized buffer.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let frsize = stat.f_frsize as u64;
    let total = stat.f_blocks as u64 * frsize;
    let free = stat.f_bavail as u64 * frsize;
    let used = (stat.f_blocks as u64 - stat.f_bfree as u64) * frsize;
    Some((total, free, used))
}

/// A wedged email worker, read from the in-process tick stamp.
///
/// The criticality only applies while email is enabled: an install that
/// deliberately runs without it must not be paged about a loop that is
/// correctly idle.
pub fn probe_email_worker(config: &Config, uptime_seconds: f64) -> ProbeResult {
    probe_email_worker_with(
        config,
        uptime_seconds,
        crate::email_worker::seconds_since_last_tick(),
    )
}

/// The same, with the tick age supplied.
///
/// The age is process-local state, so a differential cannot arrange it
/// from outside — both stacks are driven through this seam instead,
/// with Python's `seconds_since_last_tick` patched in the probe.
pub fn probe_email_worker_with(
    config: &Config,
    uptime_seconds: f64,
    age: Option<f64>,
) -> ProbeResult {
    if !config.email_enabled {
        return ProbeResult::new("disabled", json!({}));
    }
    let Some(age) = age else {
        if uptime_seconds < EMAIL_WORKER_STARTUP_GRACE_SECONDS {
            // A fresh process whose loop has not had its first tick.
            return ProbeResult::new(
                "ok",
                json!({ "tick_age_seconds": Value::Null, "note": "startup grace" }),
            );
        }
        // Past the grace window with no tick at all: the loop never
        // started, or died before its first iteration.
        return ProbeResult::new(
            "critical",
            json!({
                "tick_age_seconds": Value::Null,
                "error_class": "WorkerNeverTicked",
                "uptime_seconds": uptime_seconds,
            }),
        );
    };
    if age > EMAIL_WORKER_STALE_AFTER_SECONDS {
        return ProbeResult::new(
            "critical",
            json!({
                "tick_age_seconds": crate::pyrepr::round_half_even(age),
                "stale_after_seconds": EMAIL_WORKER_STALE_AFTER_SECONDS,
                "error_class": "WorkerStale",
            }),
        );
    }
    ProbeResult::new(
        "ok",
        json!({ "tick_age_seconds": crate::pyrepr::round_half_even(age) }),
    )
}

/// The cached Sentinel AI licence state, for self-hosted installs.
///
/// Never `critical`, by design: an unlicensed AI feature is not a
/// Command Center outage and must never flip readiness to 503. It also
/// reports validity rather than reachability — a revoked licence behind
/// a perfectly reachable service is not "ok".
pub async fn probe_sentinel_license(
    config: &Config,
    pool: &sqlx::PgPool,
    uptime_seconds: f64,
) -> ProbeResult {
    if !config.is_local_auth() {
        return ProbeResult::new("disabled", json!({}));
    }
    let Some(license_key) = config.sentinel_license_key.as_deref() else {
        return ProbeResult::new("unconfigured", json!({}));
    };

    let ctx = crate::license::LicenseContext {
        pool,
        org_id: &config.local_org_id,
        local_auth: config.is_local_auth(),
        license_key: Some(license_key),
    };
    let licensed = crate::license::is_sentinel_licensed(&ctx).await;
    let org = &config.local_org_id;
    let reachable = setting(pool, org, crate::license::LAST_CHECK_REACHABLE).await;
    let last_check_at = setting(pool, org, crate::license::LAST_CHECK_AT).await;
    let last_ok_at = setting(pool, org, crate::license::LAST_OK_AT).await;

    let mut data = json!({
        "licensed": licensed,
        "last_check_reachable": reachable == "true",
        // `x or None` — an empty string is falsy and reports as null.
        "last_check_at": non_empty(&last_check_at),
        "last_ok_at": non_empty(&last_ok_at),
        "grace_hours": crate::license::GRACE_HOURS,
    });

    if last_check_at.is_empty() && uptime_seconds < LICENSE_STARTUP_GRACE_SECONDS {
        // The boot-time check-in has not landed yet, and reporting
        // "warn" on every restart would train an operator to ignore it.
        data["note"] = json!("startup grace");
        return ProbeResult::new("ok", data);
    }

    // Only a fresh AND valid answer is ok. Coasting on grace with an
    // unreachable service, and an explicit denial from a reachable one,
    // both read as warn — different causes, same "do not treat this as
    // healthy".
    let status = if licensed && reachable == "true" { "ok" } else { "warn" };
    ProbeResult::new(status, data)
}

async fn setting(pool: &sqlx::PgPool, org_id: &str, key: &str) -> String {
    crate::settings::get(pool, org_id, key, Some(""))
        .await
        .unwrap_or_default()
        .unwrap_or_default()
}

fn non_empty(value: &str) -> Value {
    if value.is_empty() {
        Value::Null
    } else {
        json!(value)
    }
}

pub struct ReadinessReport {
    pub ready: bool,
    /// Insertion order is the response's order, which is why this is a
    /// vector and not a map.
    pub probes: Vec<(&'static str, ProbeResult)>,
}

impl ReadinessReport {
    pub fn to_json(&self) -> Value {
        let mut checks = Map::new();
        for (name, probe) in &self.probes {
            checks.insert((*name).to_string(), probe.to_json());
        }
        json!({ "ready": self.ready, "checks": Value::Object(checks) })
    }
}

/// Every probe `/api/health/ready` rolls up. Run concurrently so a slow
/// Clerk does not serialise behind the database ping.
pub async fn run_readiness_probes(
    config: &Config,
    pool: &sqlx::PgPool,
    client: &reqwest::Client,
    uptime_seconds: f64,
) -> ReadinessReport {
    let (database, clerk) = tokio::join!(
        probe_database(pool),
        probe_clerk(config, client),
    );
    let disk = probe_disk();
    let email_worker = probe_email_worker(config, uptime_seconds);

    let probes = vec![
        ("database", database),
        ("clerk", clerk),
        ("disk", disk),
        ("email_worker", email_worker),
    ];
    let ready = !probes.iter().any(|(_, p)| p.is_critical());
    ReadinessReport { ready, probes }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_flattens_its_data_beside_the_status() {
        let probe = ProbeResult::new("ok", json!({ "latency_ms": 1.25 }));
        assert_eq!(probe.to_json(), json!({ "status": "ok", "latency_ms": 1.25 }));
        // An empty payload is just the status.
        assert_eq!(
            ProbeResult::new("disabled", json!({})).to_json(),
            json!({ "status": "disabled" })
        );
    }

    /// Only `critical` flips readiness. `warn` is explicitly not enough
    /// — a disk at 85% must not answer 503 and page someone.
    #[test]
    fn only_critical_flips_readiness() {
        for (status, ready) in [
            ("ok", true),
            ("warn", true),
            ("disabled", true),
            ("unconfigured", true),
            ("critical", false),
        ] {
            let probes = vec![("disk", ProbeResult::new(status, json!({})))];
            let report = ReadinessReport {
                ready: !probes.iter().any(|(_, p)| p.is_critical()),
                probes,
            };
            assert_eq!(report.ready, ready, "status {status}");
        }
    }

    #[test]
    fn the_disk_probe_reads_a_real_filesystem() {
        let probe = probe_disk();
        // The working directory always exists, so this must not be the
        // OSError path.
        assert!(matches!(probe.status, "ok" | "warn" | "critical"));
        assert!(probe.data.contains_key("bytes_total"));
        let total = probe.data["bytes_total"].as_u64().unwrap();
        let used = probe.data["bytes_used"].as_u64().unwrap();
        assert!(total > 0, "a mounted filesystem has a size");
        assert!(used <= total);
        // Percent is rounded to one place, like Python's round(x, 1).
        let pct = probe.data["percent_used"].as_f64().unwrap();
        assert_eq!(pct, crate::pyrepr::round_to(pct, 1));
    }
}
