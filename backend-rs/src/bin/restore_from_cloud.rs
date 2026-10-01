//! Restore a self-hosted install's data from its cloud mirror.
//!
//! The other half of `sync.rs`. That pushes rows up to
//! Sentinel-Sync-Service every thirty minutes; this pulls them back
//! down, which is what makes the mirror a backup rather than a
//! write-only archive.
//!
//! Replaces `backend/scripts/restore_from_cloud.py`, which
//! `docs/runbooks/DISASTER_RECOVERY.md` names in three places as THE
//! recovery procedure for a self-hosted install. It could not go with
//! the rest of the Python.
//!
//! ```
//! sentinel-restore-from-cloud --list
//! sentinel-restore-from-cloud --dry-run
//! sentinel-restore-from-cloud
//! sentinel-restore-from-cloud --table cameras --overwrite
//! ```
//!
//! **Non-destructive by default.** A row whose primary key already
//! exists locally is skipped, not replaced. Whoever runs this is usually
//! mid-incident, and a recovery tool's worst failure mode is making
//! things worse than it found them. `--overwrite` opts into replacement.
//!
//! **One bad row does not sink the restore.** Each row is its own
//! statement, so a mirror record missing a NOT NULL column is counted
//! and stepped over. The Python used a SAVEPOINT per row for the same
//! reason, and its comment says why: without it a single bad record
//! rolls back the whole transaction and a restore of ten thousand rows
//! yields nothing, which is the worst possible outcome for a tool
//! someone reaches for after losing a disk.
//!
//! The table list comes from `sync::SYNC_TABLES`, so the two halves
//! cannot drift: anything mirrored is restorable and anything restorable
//! is mirrored.

use std::collections::BTreeMap;

use sentinel_command::sync::SYNC_TABLES;

/// `camera_nodes.api_key_hash` is NOT NULL and deliberately never
/// mirrored — it authenticates a node to THIS Command Center, so it is
/// useless to a restore and dangerous in a cloud copy.
///
/// The sentinel is deliberately not valid hex, so it can never collide
/// with a real SHA-256 hash: a restored node fails authentication until
/// it re-registers, which is the correct outcome. Re-registration mints
/// a fresh key anyway, so nothing is lost but the illusion that the old
/// credential survived.
const UNRESTORABLE_CREDENTIAL: &str = "restored-node-must-re-register";

const PAGE_SIZE: usize = 500;
const TIMEOUT_SECONDS: u64 = 60;

struct Args {
    list: bool,
    dry_run: bool,
    overwrite: bool,
    table: Option<String>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = parse_args();
    match run(args).await {
        Ok(code) => code,
        Err(message) => {
            eprintln!("{message}");
            std::process::ExitCode::from(1)
        }
    }
}

/// Printed by `--help`, and on a bad argument.
///
/// Spelled out rather than listing flags, because each one's effect on a
/// database during a recovery is the thing the operator needs to know and
/// `[--overwrite]` does not say it.
const USAGE: &str = "\
usage: sentinel-restore-from-cloud [--list] [--dry-run] [--table NAME] [--overwrite]

Pull this install's rows back down from Sentinel-Sync-Service, the one-way
cloud mirror, into the local database. Non-destructive by default.

  --list        Show what the mirror holds, per table, and exit. Also the
                quickest way to confirm sync is working — run it BEFORE
                you need it.
  --dry-run     Report what would be written without writing anything.
  --table NAME  Restore one table instead of all of them.
  --overwrite   Replace rows whose primary key already exists. Without
                this, existing rows are skipped and counted, so a run
                against a database that still has data is safe.
  --help, -h    This text.

Reads DATABASE_URL, SENTINEL_SYNC_SERVICE_URL and SENTINEL_LICENSE_KEY
from the environment, the same as the service does.

It cannot bring back node API keys (re-register each node), incident
evidence blobs, or recordings (those never left the camera). Exits 2 on a
partial restore, after naming every row it could not write.
See docs/runbooks/DISASTER_RECOVERY.md.
";

fn parse_args() -> Args {
    let mut args = Args {
        list: false,
        dry_run: false,
        overwrite: false,
        table: None,
    };
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--list" => args.list = true,
            "--dry-run" => args.dry_run = true,
            "--overwrite" => args.overwrite = true,
            "--table" if i + 1 < raw.len() => {
                args.table = Some(raw[i + 1].clone());
                i += 1;
            }
            // `--help` is the first thing an operator reaching for a
            // recovery tool types, and the Python this replaces had it for
            // free from argparse. Without it the answer was "unknown
            // argument: --help" above a usage line — which is information
            // delivered as a rebuke, during an incident.
            "--help" | "-h" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}\n");
                eprint!("{USAGE}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    args
}

async fn run(args: Args) -> Result<std::process::ExitCode, String> {
    let config = sentinel_command::config::Config::from_env();
    let key = config
        .sentinel_license_key
        .clone()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| {
            "SENTINEL_LICENSE_KEY is not set — the cloud mirror is licence-gated,\n\
             so there is nothing to restore from without the key this install syncs with."
                .to_string()
        })?;
    let base = config
        .sentinel_sync_service_url
        .trim_end_matches('/')
        .to_string();
    if base.is_empty() {
        return Err("SENTINEL_SYNC_SERVICE_URL is not set.".to_string());
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECONDS))
        .build()
        .map_err(|err| format!("could not build an HTTP client: {err}"))?;

    let summaries = fetch_tables(&client, &base, &key).await?;
    if summaries.is_empty() {
        println!("Cloud mirror is empty — nothing to restore.");
        println!(
            "(If this install should be syncing, check that the licence has the \
             data-sync entitlement and that a sync tick has run.)"
        );
        return Ok(std::process::ExitCode::SUCCESS);
    }

    if args.list {
        println!("{:<24} {:>8} {:>8}", "table", "rows", "deleted");
        for summary in &summaries {
            println!(
                "{:<24} {:>8} {:>8}",
                summary.table, summary.rows, summary.deleted
            );
        }
        return Ok(std::process::ExitCode::SUCCESS);
    }

    let wanted: Vec<&TableSummary> = summaries
        .iter()
        .filter(|s| args.table.as_deref().is_none_or(|t| t == s.table))
        .collect();
    if let Some(table) = &args.table {
        if wanted.is_empty() {
            return Err(format!("No mirrored data for table {table:?}."));
        }
    }

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&config.database_url)
        .await
        .map_err(|err| format!("could not reach the local database: {err}"))?;

    // A restore is most likely run against a machine that has never
    // started the service — that is the whole scenario. Bringing the
    // schema up here means "install, restore, start" works, instead of
    // dying on a raw "relation does not exist" and forcing the operator
    // to discover they must boot the app first.
    //
    // A dry run promises to touch nothing, so it does not migrate. That
    // also means a dry run against a fresh machine reports every row as
    // restorable, which is correct: nothing exists to skip.
    if !args.dry_run {
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|err| format!("schema bring-up failed: {err}"))?;
    }

    let known: BTreeMap<&str, &sentinel_command::sync::SyncTableSpec> =
        SYNC_TABLES.iter().map(|spec| (spec.table, spec)).collect();

    let mut total_written = 0u64;
    let mut total_skipped = 0u64;
    let mut total_failed = 0u64;
    for summary in wanted {
        if !known.contains_key(summary.table.as_str()) {
            // Mirrored by a Command Center that syncs a table this build
            // does not know. Skipped loudly rather than guessed at.
            println!(
                "  {:<22} skipped — not a table this build knows",
                summary.table
            );
            continue;
        }
        let columns = table_columns(&pool, &summary.table).await?;
        let (written, skipped, failed) =
            restore_table(&client, &base, &key, &pool, &summary.table, &columns, &args).await?;
        total_written += written;
        total_skipped += skipped;
        total_failed += failed;
        let verb = if args.dry_run {
            "would restore"
        } else {
            "restored"
        };
        let mut line = format!(
            "  {:<22} {verb} {written:>6}, skipped {skipped:>6}",
            summary.table
        );
        if failed > 0 {
            line.push_str(&format!(", FAILED {failed:>4}"));
        }
        println!("{line}");
    }

    println!();
    if args.dry_run {
        println!("Dry run: would write {total_written} row(s), skip {total_skipped}.");
        println!("Re-run without --dry-run to apply.");
        return Ok(std::process::ExitCode::SUCCESS);
    }

    println!("Restored {total_written} row(s), skipped {total_skipped}.");
    if total_skipped > 0 && !args.overwrite {
        println!("Skipped rows already existed locally. Use --overwrite to replace them.");
    }
    if total_failed > 0 {
        // Loud, and reflected in the exit code: a partial restore that
        // looks like a clean one is how someone discovers months later
        // that data they believed was recovered never came back.
        println!();
        println!("WARNING: {total_failed} row(s) could not be restored (listed above).");
        println!("Everything else was restored — this was not an all-or-nothing failure.");
    }
    println!();
    println!("Note: camera nodes restore WITHOUT their API keys — those are");
    println!("never mirrored. Each node must re-register to get a fresh key.");

    Ok(if total_failed > 0 {
        std::process::ExitCode::from(2)
    } else {
        std::process::ExitCode::SUCCESS
    })
}

struct TableSummary {
    table: String,
    rows: i64,
    deleted: i64,
}

async fn fetch_tables(
    client: &reqwest::Client,
    base: &str,
    key: &str,
) -> Result<Vec<TableSummary>, String> {
    let body: serde_json::Value = get(client, base, key, "/v1/sync/tables", &[]).await?;
    Ok(body["tables"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            Some(TableSummary {
                table: entry["table"].as_str()?.to_string(),
                rows: entry["rows"].as_i64().unwrap_or(0),
                deleted: entry["deleted"].as_i64().unwrap_or(0),
            })
        })
        .collect())
}

/// One GET, with the licence-specific failures named rather than dumped
/// as a status code: 403 and 401 mean different things to whoever is
/// reading this at three in the morning.
async fn get(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    path: &str,
    query: &[(&str, String)],
) -> Result<serde_json::Value, String> {
    let response = client
        .get(format!("{base}{path}"))
        .bearer_auth(key)
        .query(query)
        .send()
        .await
        .map_err(|err| format!("sync service unreachable: {err}"))?;
    match response.status().as_u16() {
        403 => {
            return Err("Sync service rejected the licence key (403).\n\
                        Either the licence has no data-sync entitlement, or it has been \
                        revoked or expired."
                .to_string())
        }
        401 => {
            return Err("Sync service rejected the request as unauthenticated (401).".to_string())
        }
        status if status >= 400 => {
            return Err(format!("sync service returned {status} for {path}"))
        }
        _ => {}
    }
    response
        .json()
        .await
        .map_err(|err| format!("sync service sent an unreadable body: {err}"))
}

/// The local table's columns, each with the SQL type to cast to.
///
/// Every value is bound as text and cast on the way in, because the
/// mirror is JSON: it has no date type (so timestamps arrive as ISO
/// strings) and no way to distinguish an integer column from a numeric
/// one. Postgres will not implicitly cast text to integer in an INSERT,
/// so binding everything as text and hoping — which is what the first
/// version of this did — fails on every row of every table with an
/// integer primary key. Found by running it: `column "id" is of type
/// integer but expression is of type text`, four times out of four.
///
/// `format_type` rather than `information_schema.data_type` because the
/// former returns a directly castable spelling. `data_type` says
/// "character varying" where the cast wants `character varying(100)` or
/// `varchar`, and "ARRAY" for anything array-typed, which is not a type
/// at all.
async fn table_columns(pool: &sqlx::PgPool, table: &str) -> Result<Vec<(String, String)>, String> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT a.attname::text,
                pg_catalog.format_type(a.atttypid, a.atttypmod) AS cast_to
           FROM pg_catalog.pg_attribute a
           JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = 'public' AND c.relname = $1
            AND a.attnum > 0 AND NOT a.attisdropped
          ORDER BY a.attnum",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(|err| format!("could not read {table}'s columns: {err}"))?;
    // Empty means no table: a dry run against a machine that has never
    // booted, where nothing exists locally to skip.
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
async fn restore_table(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    pool: &sqlx::PgPool,
    table: &str,
    columns: &[(String, String)],
    args: &Args,
) -> Result<(u64, u64, u64), String> {
    let mut written = 0u64;
    let mut skipped = 0u64;
    let mut failed = 0u64;
    let mut cursor: Option<String> = None;
    // No columns means no table: a dry run on a fresh machine. Every row
    // counts as restorable, because nothing is there to collide with.
    let table_exists = !columns.is_empty();

    loop {
        let mut query = vec![
            ("table", table.to_string()),
            ("limit", PAGE_SIZE.to_string()),
        ];
        if let Some(cursor) = &cursor {
            query.push(("cursor", cursor.clone()));
        }
        let body = get(client, base, key, "/v1/sync/rows", &query).await?;
        let rows = body["rows"].as_array().cloned().unwrap_or_default();

        for row in &rows {
            let data = &row["data"];
            let Some(map) = data.as_object() else {
                skipped += 1;
                continue;
            };
            let Some(id) = map.get("id").filter(|v| !v.is_null()) else {
                skipped += 1;
                continue;
            };

            if !table_exists {
                written += 1;
                continue;
            }

            if !args.overwrite && row_exists(pool, table, id).await? {
                skipped += 1;
                continue;
            }
            if args.dry_run {
                written += 1;
                continue;
            }

            match write_row(pool, table, columns, map, args.overwrite).await {
                Ok(()) => written += 1,
                Err(err) => {
                    // Its own statement, so the failure is contained and
                    // the remaining rows still get their chance.
                    failed += 1;
                    eprintln!("    row {id} could not be restored: {err}");
                }
            }
        }

        cursor = body["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }

    Ok((written, skipped, failed))
}

async fn row_exists(
    pool: &sqlx::PgPool,
    table: &str,
    id: &serde_json::Value,
) -> Result<bool, String> {
    let raw = match id {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    // `id::text` so one comparison works for an integer key and the
    // Sentinel run's hex string alike.
    let found: Option<(i32,)> = sqlx::query_as(&format!(
        "SELECT 1 FROM {table} WHERE id::text = $1 LIMIT 1"
    ))
    .bind(&raw)
    .fetch_optional(pool)
    .await
    .map_err(|err| format!("{err}"))?;
    Ok(found.is_some())
}

async fn write_row(
    pool: &sqlx::PgPool,
    table: &str,
    columns: &[(String, String)],
    data: &serde_json::Map<String, serde_json::Value>,
    overwrite: bool,
) -> Result<(), String> {
    let mut names: Vec<&str> = Vec::new();
    let mut casts: Vec<String> = Vec::new();
    let mut values: Vec<Option<String>> = Vec::new();

    for (name, cast_to) in columns {
        // A key the mirror does not carry is left to its column default.
        // Unknown keys in the payload are dropped rather than raising: a
        // mirror written by a newer or older Command Center than the one
        // restoring must not hard-fail the whole restore.
        let Some(value) = data.get(name) else {
            continue;
        };
        names.push(name);
        let index = values.len() + 1;
        // Cast to the column's own type. Every value is bound as text
        // because the mirror is JSON; without the cast Postgres refuses
        // the insert outright rather than coercing.
        casts.push(format!("${index}::{cast_to}"));
        values.push(match value {
            serde_json::Value::Null => None,
            serde_json::Value::String(s) => Some(s.clone()),
            // A JSON object or array is a jsonb column's value; its
            // serialised form is what the cast wants.
            other => Some(other.to_string()),
        });
    }

    if names.is_empty() {
        return Err("the mirrored row had no column this build knows".to_string());
    }

    // The unmirrored credential, which is NOT NULL and has to hold
    // something. See `UNRESTORABLE_CREDENTIAL`.
    if table == "camera_nodes" && !names.contains(&"api_key_hash") {
        names.push("api_key_hash");
        casts.push(format!("${}::text", values.len() + 1));
        values.push(Some(UNRESTORABLE_CREDENTIAL.to_string()));
    }

    let quoted: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
    let conflict = if overwrite {
        let sets: Vec<String> = names
            .iter()
            .filter(|n| **n != "id")
            .map(|n| format!("\"{n}\" = EXCLUDED.\"{n}\""))
            .collect();
        if sets.is_empty() {
            "DO NOTHING".to_string()
        } else {
            format!("DO UPDATE SET {}", sets.join(", "))
        }
    } else {
        // Belt and braces beside the existence check above: a row that
        // appeared between the two must not error the restore.
        "DO NOTHING".to_string()
    };

    let sql = format!(
        "INSERT INTO {table} ({}) VALUES ({}) ON CONFLICT (id) {conflict}",
        quoted.join(", "),
        casts.join(", ")
    );
    let mut query = sqlx::query(&sql);
    for value in &values {
        query = query.bind(value.as_deref());
    }
    query.execute(pool).await.map(|_| ()).map_err(|err| {
        err.to_string()
            .lines()
            .next()
            .unwrap_or("write failed")
            .to_string()
    })
}
