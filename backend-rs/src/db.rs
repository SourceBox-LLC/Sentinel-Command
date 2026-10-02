//! Which database this build talks to.
//!
//! One backend per BUILD, chosen by the `sqlite` cargo feature — not per
//! process. sqlx's typed queries are generic over the driver, so a pool
//! that could be either at runtime would mean every one of ~330 query
//! sites written twice (or `sqlx::Any`, which cannot carry a timestamp).
//! With an alias the sites are written once and compiled for each.
//!
//! * default build — PostgreSQL. The hosted service, and anyone who
//!   wants it.
//! * `--features sqlite` — a single file, for the self-hosted install
//!   that the Python tier served with `sqlite:///./sentinel.db`.

#[cfg(not(feature = "sqlite"))]
mod kind {
    pub type Db = sqlx::Postgres;
    pub type PoolOptions = sqlx::postgres::PgPoolOptions;
    pub type Row = sqlx::postgres::PgRow;
    pub const SQLITE: bool = false;
}

#[cfg(feature = "sqlite")]
mod kind {
    pub type Db = sqlx::Sqlite;
    pub type PoolOptions = sqlx::sqlite::SqlitePoolOptions;
    pub type Row = sqlx::sqlite::SqliteRow;
    pub const SQLITE: bool = true;
}

pub use kind::{Db, PoolOptions, Row, SQLITE};

pub type Pool = sqlx::Pool<Db>;
pub type Transaction<'c> = sqlx::Transaction<'c, Db>;

// ── Lists ────────────────────────────────────────────────────────────
//
// `col = ANY($1)` with a bound array is Postgres. SQLite has no array
// type, so the same list travels as one JSON text parameter and is
// unpacked by `json_each` — still ONE bind, so the placeholder numbering
// around it does not move and the two builds share every other
// character of the statement.

/// The membership test for placeholder `$n`: write
/// `format!("… WHERE id {}", db::any(1))`.
#[cfg(not(feature = "sqlite"))]
pub fn any(n: usize) -> String {
    format!("= ANY(${n})")
}

#[cfg(feature = "sqlite")]
pub fn any(n: usize) -> String {
    format!("IN (SELECT value FROM json_each(${n}))")
}

/// The value to `.bind()` for [`any`].
#[cfg(not(feature = "sqlite"))]
pub fn list<T: Clone + serde::Serialize>(items: &[T]) -> Vec<T> {
    items.to_vec()
}

#[cfg(feature = "sqlite")]
pub fn list<T: Clone + serde::Serialize>(items: &[T]) -> String {
    // A slice of strings or integers cannot fail to serialise.
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_is_one_parameter_in_either_dialect() {
        let sql = format!("id {}", any(3));
        assert_eq!(sql.matches("$3").count(), 1, "{sql}");
        #[cfg(feature = "sqlite")]
        assert_eq!(list(&["a", "b"]), r#"["a","b"]"#);
        #[cfg(not(feature = "sqlite"))]
        assert_eq!(list(&["a", "b"]), vec!["a", "b"]);
    }
}

// ── Connecting ───────────────────────────────────────────────────────

/// The schema, embedded at compile time — the one for this build.
#[cfg(not(feature = "sqlite"))]
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[cfg(feature = "sqlite")]
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations-sqlite");

/// What this build opens, for messages.
pub const BACKEND_NAME: &str = if SQLITE { "SQLite" } else { "PostgreSQL" };

/// Which backend a `DATABASE_URL` asks for, by scheme alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlKind {
    Postgres,
    Sqlite,
    Other,
}

pub fn url_kind(url: &str) -> UrlKind {
    let scheme = url
        .split(':')
        .next()
        .unwrap_or("")
        .split('+')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match scheme.as_str() {
        "postgres" | "postgresql" => UrlKind::Postgres,
        "sqlite" => UrlKind::Sqlite,
        _ => UrlKind::Other,
    }
}

/// The file a SQLite URL names.
///
/// Accepts SQLAlchemy's spelling as well as sqlx's, because the Python
/// tier's documented default was `sqlite:///./sentinel.db` and an
/// existing self-hosted `.env` still says that:
///
/// * `sqlite:///./sentinel.db`, `sqlite:///sentinel.db` — relative
/// * `sqlite:////data/sentinel.db` — absolute (four slashes)
/// * `sqlite://sentinel.db`, `sqlite:sentinel.db` — sqlx's own forms
/// * a trailing `?query` is dropped; the pragmas are set in code
pub fn sqlite_path(url: &str) -> String {
    let rest = url.split_once(':').map(|(_, rest)| rest).unwrap_or(url);
    let rest = rest.split('?').next().unwrap_or(rest);
    let path = if let Some(absolute) = rest.strip_prefix("////") {
        format!("/{absolute}")
    } else if let Some(relative) = rest.strip_prefix("///") {
        relative.to_string()
    } else if let Some(relative) = rest.strip_prefix("//") {
        relative.to_string()
    } else {
        rest.to_string()
    };
    if path.is_empty() {
        "sentinel.db".to_string()
    } else {
        path
    }
}

/// Open the pool for this build.
///
/// PostgreSQL: a real pool, because the database is across a network.
///
/// SQLite: the pragmas the Python tier set on every connection, for the
/// reasons it gave —
///
/// * `journal_mode=WAL` so readers are never blocked behind the writer.
///   The hot path, `push-segment`, does auth READS and no writes.
/// * `busy_timeout=30s`. The writers are the background loops, and
///   writer-vs-writer is exactly where waiting beats erroring: a short
///   timeout turns a heartbeat into "database is locked" and a lost row
///   while log cleanup holds the lock.
/// * `synchronous=NORMAL`, the standard WAL pairing: an fsync per
///   checkpoint rather than per commit. Power loss can cost the last few
///   commits and never corrupts.
/// * `foreign_keys=ON`, which SQLite leaves off unless asked — and
///   incident evidence is deleted by cascade.
pub async fn connect(url: &str, max_connections: u32) -> Result<Pool, sqlx::Error> {
    #[cfg(not(feature = "sqlite"))]
    {
        PoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(url)
            .await
    }
    #[cfg(feature = "sqlite")]
    {
        use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
        let path = sqlite_path(url);
        if let Some(parent) = std::path::Path::new(&path).parent() {
            if !parent.as_os_str().is_empty() {
                // A missing directory is the usual first-run failure, and
                // SQLite reports it as "unable to open database file".
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(30))
            .foreign_keys(true);
        PoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(std::time::Duration::from_secs(30))
            .connect_with(options)
            .await
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn schemes_are_read_through_a_sqlalchemy_driver_suffix() {
        assert_eq!(url_kind("postgresql+psycopg://u:p@h/db"), UrlKind::Postgres);
        assert_eq!(url_kind("postgres://h/db"), UrlKind::Postgres);
        assert_eq!(url_kind("sqlite:///./sentinel.db"), UrlKind::Sqlite);
        assert_eq!(url_kind("sqlite+pysqlite:///x.db"), UrlKind::Sqlite);
        assert_eq!(url_kind("SQLITE:x.db"), UrlKind::Sqlite);
        assert_eq!(url_kind("mysql://h/db"), UrlKind::Other);
        assert_eq!(url_kind(""), UrlKind::Other);
    }

    #[test]
    fn both_spellings_of_a_sqlite_url_name_the_same_file() {
        for (url, path) in [
            ("sqlite:///./sentinel.db", "./sentinel.db"),
            ("sqlite:///sentinel.db", "sentinel.db"),
            ("sqlite:////data/sentinel.db", "/data/sentinel.db"),
            ("sqlite://sentinel.db", "sentinel.db"),
            ("sqlite:sentinel.db", "sentinel.db"),
            ("sqlite:///data/x.db?mode=rwc", "data/x.db"),
            ("sqlite+pysqlite:////abs/x.db", "/abs/x.db"),
            ("sqlite://", "sentinel.db"),
        ] {
            assert_eq!(sqlite_path(url), path, "{url}");
        }
    }
}

// ── Case-insensitive match ───────────────────────────────────────────

/// `ILIKE` is Postgres. SQLite's `LIKE` is already case-insensitive —
/// for ASCII only, which is also all SQLAlchemy's `.ilike()` gave the
/// Python tier on SQLite (`lower(a) LIKE lower(b)`, with an ASCII
/// `lower`). Postgres folds by the database's collation.
///
/// SQLite has NO default escape character, where Postgres's is a
/// backslash, so every use must spell out `ESCAPE '\\'` or a filter
/// containing `%` or `_` means different things in the two builds.
pub const ILIKE: &str = if SQLITE { "LIKE" } else { "ILIKE" };

// ── Tests ────────────────────────────────────────────────────────────

/// The database the `*_db` integration tests run against, migrated.
///
/// * PostgreSQL build: `TEST_DATABASE_URL`. **Unset means skip** (`None`)
///   — `cargo test` needs no database. **Set and unreachable is a
///   failure**, not a skip: this used to end in `.ok()`, which turned a
///   wrong URL into every one of these tests returning early, green.
/// * SQLite build: a fresh file per call, so these tests ALWAYS run.
///   There is nothing to provision and so no reason to skip — which
///   makes the SQLite leg the one place they cannot silently not run.
pub async fn test_pool(max_connections: u32) -> Option<Pool> {
    #[cfg(not(feature = "sqlite"))]
    let pool = {
        let url = std::env::var("TEST_DATABASE_URL")
            .ok()
            .filter(|u| !u.is_empty())?;
        connect(&url, max_connections)
            .await
            .expect("TEST_DATABASE_URL is set but the database is unreachable")
    };
    #[cfg(feature = "sqlite")]
    let pool = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // Under target/, which is on real disk; the system temp directory
        // on this project's machines is a RAM-backed tmpfs.
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/target/test-dbs");
        let path = format!(
            "{dir}/{}-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        connect(&format!("sqlite:///{path}"), max_connections)
            .await
            .expect("a SQLite file under target/ must be creatable")
    };
    // Applied here, not assumed: CI hands the PostgreSQL leg an EMPTY
    // database. Idempotent, and sqlx serialises concurrent migrators.
    MIGRATOR
        .run(&pool)
        .await
        .expect("migrations must apply to the test database");
    Some(pool)
}

// ── Two binaries, one command ────────────────────────────────────────

/// The file name of the build that opens the OTHER database, given this
/// one's: `sentinel-command` ⇄ `sentinel-command-sqlite`, and the same
/// for the restore tool.
pub fn sibling_binary_name(own: &str) -> String {
    match own.strip_suffix("-sqlite") {
        Some(base) => base.to_string(),
        None => format!("{own}-sqlite"),
    }
}

/// This process's own file name, for messages. Falls back to the name
/// the service ships under.
pub fn own_binary_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "sentinel-command".to_string())
}

/// Replace this process with the build that can open `url`, if this one
/// cannot and the other is installed beside it.
///
/// The image ships both, so which database is in use stays a matter of
/// `DATABASE_URL` — as it was for the Python tier — and not of knowing
/// which binary to start. Returns if there is nothing to do or nowhere
/// to go; the caller then reports the mismatch with
/// `config::unsupported_database_url`.
pub fn dispatch_to_matching_build(url: &str) {
    let wrong_build = matches!(
        (url_kind(url), SQLITE),
        (UrlKind::Sqlite, false) | (UrlKind::Postgres, true)
    );
    if !wrong_build {
        return;
    }
    // Set on the way through, so two binaries that each believe the other
    // is the right one fail with a sentence instead of exec-ing each
    // other forever.
    const MARK: &str = "SENTINEL_DB_DISPATCHED";
    if std::env::var_os(MARK).is_some() {
        return;
    }
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    let other = me.with_file_name(sibling_binary_name(&own_binary_name()));
    if !other.is_file() || other == me {
        return;
    }
    tracing::info!(binary = %other.display(), "DATABASE_URL is for the other build — switching");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&other)
            .args(std::env::args_os().skip(1))
            .env(MARK, "1")
            .exec();
        tracing::error!(error = %err, "could not start {}", other.display());
    }
}

#[cfg(test)]
mod sibling_tests {
    use super::sibling_binary_name;

    #[test]
    fn the_sibling_is_the_same_tool_for_the_other_database() {
        assert_eq!(
            sibling_binary_name("sentinel-command"),
            "sentinel-command-sqlite"
        );
        assert_eq!(
            sibling_binary_name("sentinel-command-sqlite"),
            "sentinel-command"
        );
        assert_eq!(
            sibling_binary_name("sentinel-restore-from-cloud"),
            "sentinel-restore-from-cloud-sqlite"
        );
    }
}
