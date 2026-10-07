//! The GDPR export archive (`POST /api/gdpr/export`), against a real
//! database.
//!
//! ```
//! TEST_DATABASE_URL=postgresql://… cargo test --test gdpr_export_db
//! cargo test --features sqlite --test gdpr_export_db
//! ```
//!
//! What matters here is what the archive carries beyond the JSON
//! tables: the bytes of each piece of incident evidence, and each
//! Sentinel run's tool trace. Both were left out once, and the Privacy
//! Policy promises them.

use std::io::{Cursor, Read};

use sentinel_command::api::gdpr::write_archive;
use sentinel_command::db::Pool;
use serde_json::Value;

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

/// Every test shares the tables, so each org cleans up its own rows.
async fn clear(pool: &Pool, org: &str) {
    sqlx::query(
        "DELETE FROM incident_evidence WHERE incident_id IN
            (SELECT id FROM incidents WHERE org_id = $1)",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    for table in ["incidents", "sentinel_runs"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE org_id = $1"))
            .bind(org)
            .execute(pool)
            .await
            .unwrap();
    }
}

/// One incident with a JPEG, a clip and a text note; returns its id.
async fn seed_incident(pool: &Pool, org: &str, jpeg: &[u8], clip: &[u8]) -> i32 {
    let now = chrono::Utc::now().naive_utc();
    let (id,): (i32,) = sqlx::query_as(
        "INSERT INTO incidents
            (org_id, camera_id, title, summary, severity, status, created_by, created_at, updated_at)
         VALUES ($1, 'cam-1', 'Person at the gate', 'summary', 'high', 'open', 'sentinel', $2, $2)
         RETURNING id",
    )
    .bind(org)
    .bind(now)
    .fetch_one(pool)
    .await
    .unwrap();
    for (kind, data, mime) in [
        ("snapshot", Some(jpeg), Some("image/jpeg")),
        ("clip", Some(clip), Some("video/mp2t")),
        ("observation", None, None),
    ] {
        sqlx::query(
            "INSERT INTO incident_evidence
                (incident_id, kind, text, camera_id, data, data_mime, timestamp)
             VALUES ($1, $2, 'note', 'cam-1', $3, $4, $5)",
        )
        .bind(id)
        .bind(kind)
        .bind(data)
        .bind(mime)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }
    id
}

fn archive(bytes: Vec<u8>) -> zip::ZipArchive<Cursor<Vec<u8>>> {
    zip::ZipArchive::new(Cursor::new(bytes)).unwrap()
}

fn read(zip: &mut zip::ZipArchive<Cursor<Vec<u8>>>, name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    zip.by_name(name).unwrap().read_to_end(&mut out).unwrap();
    out
}

fn json(zip: &mut zip::ZipArchive<Cursor<Vec<u8>>>, name: &str) -> Value {
    serde_json::from_slice(&read(zip, name)).unwrap()
}

async fn export(pool: &Pool, org: &str) -> zip::ZipArchive<Cursor<Vec<u8>>> {
    let mut buf = Cursor::new(Vec::new());
    write_archive(pool, org, chrono::Utc::now(), &mut buf)
        .await
        .unwrap();
    archive(buf.into_inner())
}

// Multi-threaded, as the server is, so the writes take the
// `block_in_place` path; the other tests here cover the inline one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_bytes_are_in_the_archive_and_linked_from_their_rows() {
    let pool = require_db!();
    let org = "gdx_evidence";
    clear(&pool, org).await;
    let jpeg = b"\xff\xd8\xff\xe0 a jpeg".to_vec();
    let clip = vec![0x47u8; 188 * 3];
    let incident = seed_incident(&pool, org, &jpeg, &clip).await;

    let mut zip = export(&pool, org).await;

    let rows = json(&mut zip, "incident_evidence.json");
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    let mut found = 0;
    for row in rows {
        match row["kind"].as_str().unwrap() {
            "snapshot" | "clip" => {
                let path = row["file"].as_str().unwrap().to_string();
                assert!(path.starts_with(&format!("evidence/{incident}/")));
                let want = if row["kind"] == "snapshot" {
                    &jpeg
                } else {
                    &clip
                };
                assert_eq!(&read(&mut zip, &path), want, "{path}");
                found += 1;
            }
            // A text note has no bytes, and so no file.
            _ => assert!(row.get("file").is_none()),
        }
    }
    assert_eq!(found, 2);
    assert_eq!(json(&mut zip, "manifest.json")["evidence_files"], 2);

    clear(&pool, org).await;
}

#[tokio::test]
async fn another_orgs_evidence_never_appears() {
    let pool = require_db!();
    let (mine, theirs) = ("gdx_mine", "gdx_theirs");
    clear(&pool, mine).await;
    clear(&pool, theirs).await;
    seed_incident(&pool, theirs, b"their jpeg", b"their clip").await;

    let mut zip = export(&pool, mine).await;

    assert_eq!(
        json(&mut zip, "incident_evidence.json"),
        Value::Array(vec![])
    );
    assert!(zip.file_names().all(|n| !n.starts_with("evidence/")));

    clear(&pool, theirs).await;
}

#[tokio::test]
async fn sentinel_runs_carry_their_tool_trace() {
    let pool = require_db!();
    let org = "gdx_runs";
    clear(&pool, org).await;
    let now = chrono::Utc::now().naive_utc();
    sqlx::query(
        "INSERT INTO sentinel_runs
            (id, org_id, triggered_at, trigger_type, tool_call_count, outcome, tool_trace, updated_at)
         VALUES ('gdxrun1', $1, $2, 'motion', 1, 'no_action',
                 '[{\"tool\": \"view_camera\", \"ok\": true}]', $2)",
    )
    .bind(org)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    let mut zip = export(&pool, org).await;

    let runs = json(&mut zip, "sentinel_runs.json");
    assert_eq!(runs[0]["tool_trace"][0]["tool"], "view_camera");

    clear(&pool, org).await;
}
