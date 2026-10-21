//! Applies the compiled-in schema migrations to a SQLite database.
//!
//! The rules, and what is recorded, are in `refhir_persistence::migrate`. Here: all pending
//! migrations run in one `BEGIN IMMEDIATE` transaction. That takes SQLite's write lock, so a
//! second process opening the same file waits for the first, and then finds nothing left to do.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use refhir_persistence::migrate::{self, Adoption, AppliedMigration, Existing, Migration, Report};
use refhir_persistence::PersistError;
use rusqlite::{params, Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::definite;

pub(crate) static MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "baseline",
        sql: include_str!("../migrations/0001_baseline.sql"),
    },
    Migration {
        version: 2,
        name: "governance",
        sql: include_str!("../migrations/0002_governance.sql"),
    },
    Migration {
        version: 3,
        name: "near_index",
        sql: include_str!("../migrations/0003_near_index.sql"),
    },
];

const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    checksum TEXT NOT NULL
)";

/// Columns that releases before version 1 added to tables with `ALTER TABLE`, so a table such a
/// release made may lack them. SQLite has no `ADD COLUMN IF NOT EXISTS`.
const LATER_COLUMNS: &[(&str, &str, &str)] = &[
    ("index_backfills", "claimed_epoch", "INTEGER"),
    ("jobs", "lease_owner", "TEXT"),
    ("jobs", "lease_until", "INTEGER"),
];

/// How long a process waits for another that is migrating the same file.
const MIGRATION_WAIT: Duration = Duration::from_secs(60);
const DEFAULT_WAIT: Duration = Duration::from_millis(5000);

/// Brings the database up to `known`, or fails and leaves it exactly as it was.
pub(crate) fn run(
    connection: &mut Connection,
    known: &'static [Migration],
) -> Result<(), PersistError> {
    // A database that is current is decided from a plain read, so starting against one takes
    // no write lock.
    let applied = read_applied(connection).map_err(definite)?;
    if !applied.is_empty() && migrate::pending(known, &applied)?.is_empty() {
        return Ok(());
    }
    connection.busy_timeout(MIGRATION_WAIT).map_err(definite)?;
    let result = apply(connection, known);
    connection.busy_timeout(DEFAULT_WAIT).map_err(definite)?;
    result
}

fn apply(connection: &mut Connection, known: &'static [Migration]) -> Result<(), PersistError> {
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(definite)?;
    tx.execute_batch(CREATE_TABLE).map_err(definite)?;
    // Decided again under the lock: another process may have just done it.
    let applied = read_applied(&tx).map_err(definite)?;
    let held = if applied.is_empty() {
        existing(&tx).map_err(definite)?
    } else {
        Existing::Complete // not consulted once versions are recorded
    };
    let plan = migrate::plan(known, &applied, held)?;
    match plan.adoption {
        Adoption::No => {}
        Adoption::Record => record(&tx, &known[0])?,
        Adoption::CompleteThenRecord => {
            complete_older_schema(&tx, &known[0])?;
            record(&tx, &known[0])?;
        }
    }
    for migration in plan.run {
        tx.execute_batch(migration.sql).map_err(|err| {
            definite(format!(
                "schema migration {} ({}) failed: {err}; the database is unchanged",
                migration.version, migration.name
            ))
        })?;
        record(&tx, migration)?;
    }
    tx.commit().map_err(definite)
}

fn record(tx: &Transaction<'_>, migration: &Migration) -> Result<(), PersistError> {
    tx.execute(
        "INSERT INTO schema_migrations (version, checksum) VALUES (?1, ?2)",
        params![migration.version, migration.checksum()],
    )
    .map_err(definite)?;
    Ok(())
}

/// What every earlier release did at every start: create what is missing, add the columns it
/// grew later. Only ever adds.
fn complete_older_schema(tx: &Transaction<'_>, baseline: &Migration) -> Result<(), PersistError> {
    tx.execute_batch(baseline.sql).map_err(definite)?;
    for (table, column, definition) in LATER_COLUMNS {
        if !has_column(tx, table, column).map_err(definite)? {
            tx.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition}"
            ))
            .map_err(definite)?;
        }
    }
    Ok(())
}

fn has_column(connection: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        params![table, column],
        |row| row.get(0),
    )
}

fn read_applied(connection: &Connection) -> rusqlite::Result<Vec<AppliedMigration>> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' \
         AND name = 'schema_migrations')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(Vec::new());
    }
    connection
        .prepare("SELECT version, applied_at, checksum FROM schema_migrations ORDER BY version")?
        .query_map([], |row| {
            Ok(AppliedMigration {
                version: row.get(0)?,
                applied_at: row.get(1)?,
                checksum: row.get(2)?,
            })
        })?
        .collect()
}

/// What an unversioned database holds, judged against version 1.
fn existing(connection: &Connection) -> rusqlite::Result<Existing> {
    let present: HashSet<String> = connection
        .prepare("SELECT name FROM sqlite_master WHERE type IN ('table', 'index')")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let found = migrate::BASELINE_OBJECTS
        .iter()
        .filter(|name| present.contains(**name))
        .count();
    if found == 0 {
        return Ok(Existing::Empty);
    }
    let mut complete = found == migrate::BASELINE_OBJECTS.len();
    for (table, column, _) in LATER_COLUMNS {
        complete = complete && has_column(connection, table, column)?;
    }
    Ok(if complete {
        Existing::Complete
    } else {
        Existing::Partial
    })
}

/// What `path` holds and what startup would do to it, without changing it: a file that does
/// not exist is reported as new, and is not created.
pub fn migration_report(path: &Path) -> Result<Report, PersistError> {
    if !path.exists() {
        return Ok(Report::new(MIGRATIONS, Vec::new(), Existing::Empty));
    }
    let connection =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(definite)?;
    connection.busy_timeout(DEFAULT_WAIT).map_err(definite)?;
    Ok(Report::new(
        MIGRATIONS,
        read_applied(&connection).map_err(definite)?,
        existing(&connection).map_err(definite)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqlitePersistence;
    use std::path::PathBuf;

    fn temp_db(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "refhir-sqlite-migrate-{tag}-{}-{}.sqlite",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ))
    }

    fn remove_db(path: &Path) {
        for suffix in ["", "-wal", "-shm", ".lock"] {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            let _ = std::fs::remove_file(name);
        }
    }

    fn versions(connection: &Connection) -> Vec<i64> {
        read_applied(connection)
            .unwrap()
            .iter()
            .map(|row| row.version)
            .collect()
    }

    fn table_exists(connection: &Connection, name: &str) -> bool {
        connection
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = ?1)",
                [name],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn schema_version(connection: &Connection) -> i64 {
        connection
            .query_row("PRAGMA schema_version", [], |row| row.get(0))
            .unwrap()
    }

    /// Migration 1 is applied to databases that cannot be changed afterwards, and every one of
    /// them records this checksum. If this fails you edited the baseline file: revert it and
    /// add a new migration instead.
    #[test]
    fn the_baseline_is_frozen() {
        migrate::assert_well_formed(MIGRATIONS);
        assert_eq!(
            MIGRATIONS[0].checksum(),
            "c338d189db0388d40b3c36d432b4cd99c7dc848a601ccc917b875b521bc4cc66",
            "migration 1 was edited"
        );
    }

    /// Migration 2 is pinned like the baseline, from the first release that carries it.
    #[test]
    fn the_governance_migration_is_frozen() {
        assert_eq!(MIGRATIONS[1].name, "governance");
        assert_eq!(
            MIGRATIONS[1].checksum(),
            "b3da78316099b4a8e0a4d8d8df6e04e8341dd8b9dd91e01ebe21ab4185ea4333",
            "migration 2 was edited"
        );
    }

    /// How many times running `sql` changes the schema, which SQLite counts in `schema_version`.
    fn schema_changes_made_by(sql: &str) -> i64 {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(MIGRATIONS[0].sql).unwrap();
        let before = schema_version(&connection);
        connection.execute_batch(sql).unwrap();
        schema_version(&connection) - before
    }

    #[test]
    fn a_database_at_the_baseline_gets_the_governance_tables_and_keeps_what_it_held() {
        let path = temp_db("baseline-to-governance");
        {
            let mut connection = Connection::open(&path).unwrap();
            run(&mut connection, &MIGRATIONS[..1]).unwrap();
            connection
                .execute_batch(
                    "INSERT INTO persistence_globals (key, json_value) VALUES ('kept', '1');",
                )
                .unwrap();
            assert_eq!(versions(&connection), [1]);
            assert!(!table_exists(&connection, "audit_log"));
        }
        SqlitePersistence::open(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        assert_eq!(versions(&connection), [1, 2, 3]);
        for table in [
            "usage_daily",
            "request_log",
            "governance_members",
            "audit_log",
        ] {
            assert!(table_exists(&connection, table), "{table}");
        }
        let kept: String = connection
            .query_row(
                "SELECT json_value FROM persistence_globals WHERE key = 'kept'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept, "1");
        remove_db(&path);
    }

    #[test]
    fn a_new_database_gets_the_baseline_and_a_record_of_it() {
        let path = temp_db("fresh");
        {
            SqlitePersistence::open(&path).unwrap();
        }
        let connection = Connection::open(&path).unwrap();
        let applied = read_applied(&connection).unwrap();
        assert_eq!(applied.len(), 3);
        assert_eq!(applied[0].version, 1);
        assert_eq!(applied[0].checksum, MIGRATIONS[0].checksum());
        assert_eq!(applied[1].checksum, MIGRATIONS[1].checksum());
        assert!(
            applied[0].applied_at.ends_with('Z'),
            "{}",
            applied[0].applied_at
        );
        // The list of objects used to recognise an old database is what the SQL creates.
        assert_eq!(existing(&connection).unwrap(), Existing::Complete);
        // Opening again is a read and changes nothing.
        let before = schema_version(&connection);
        SqlitePersistence::open(&path).unwrap();
        assert_eq!(versions(&connection), [1, 2, 3]);
        assert_eq!(schema_version(&connection), before);
        remove_db(&path);
    }

    #[test]
    fn an_unversioned_database_with_the_current_tables_is_adopted_without_rerunning() {
        let path = temp_db("adopt");
        {
            // What the last unversioned release left behind, with data in it.
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch(MIGRATIONS[0].sql).unwrap();
            connection
                .execute_batch(
                    "INSERT INTO persistence_globals (key, json_value) VALUES ('kept', '1'); \
                     INSERT INTO jobs (id, kind, status, request, created_at) \
                     VALUES ('j', 'export', 'running', '{}', 5);",
                )
                .unwrap();
            assert!(!table_exists(&connection, "schema_migrations"));
        }
        let before = {
            let connection = Connection::open(&path).unwrap();
            schema_version(&connection)
        };
        let report = migration_report(&path).unwrap();
        assert_eq!(report.existing, Existing::Complete);
        assert!(report.render(false).contains("without running it"));

        SqlitePersistence::open(&path).unwrap();

        let connection = Connection::open(&path).unwrap();
        assert_eq!(versions(&connection), [1, 2, 3]);
        assert_eq!(
            read_applied(&connection).unwrap()[0].checksum,
            MIGRATIONS[0].checksum()
        );
        // The baseline is not re-run: the only schema changes are the new `schema_migrations`
        // table and what the governance migration creates.
        assert_eq!(
            schema_version(&connection),
            before + 1 + schema_changes_made_by(MIGRATIONS[1].sql)
        );
        let kept: String = connection
            .query_row(
                "SELECT json_value FROM persistence_globals WHERE key = 'kept'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept, "1");
        let jobs: i64 = connection
            .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(jobs, 1);
        remove_db(&path);
    }

    #[test]
    fn an_unversioned_database_from_an_older_release_is_completed_then_recorded() {
        let path = temp_db("older");
        {
            // `jobs` before leases, `index_backfills` before claims, and nothing else.
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE jobs (id TEXT NOT NULL PRIMARY KEY, kind TEXT NOT NULL, \
                     status TEXT NOT NULL, request TEXT NOT NULL, result TEXT, error TEXT, \
                     attempts INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, \
                     completed_at INTEGER); \
                     INSERT INTO jobs (id, kind, status, request, attempts, created_at) \
                     VALUES ('old', 'export', 'running', '{}', 1, 5); \
                     CREATE TABLE index_backfills (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                     package_id INTEGER NOT NULL, watermark_ts INTEGER NOT NULL, \
                     tablet TEXT NOT NULL, cursor_id TEXT, done INTEGER NOT NULL); \
                     INSERT INTO index_backfills (package_id, watermark_ts, tablet, cursor_id, \
                     done) VALUES (1, 7, 'Patient', 'p3', 0);",
                )
                .unwrap();
        }
        assert_eq!(migration_report(&path).unwrap().existing, Existing::Partial);
        SqlitePersistence::open(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        assert_eq!(versions(&connection), [1, 2, 3]);
        assert_eq!(existing(&connection).unwrap(), Existing::Complete);
        let kept: (String, i64) = connection
            .query_row(
                "SELECT cursor_id, (SELECT COUNT(*) FROM jobs WHERE id = 'old') \
                 FROM index_backfills",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(kept, ("p3".to_string(), 1));
        remove_db(&path);
    }

    #[test]
    fn a_database_from_a_newer_version_refuses_to_start_and_is_left_alone() {
        let path = temp_db("newer");
        {
            SqlitePersistence::open(&path).unwrap();
            let connection = Connection::open(&path).unwrap();
            connection
                .execute(
                    "INSERT INTO schema_migrations (version, checksum) VALUES (4, 'from-the-future')",
                    [],
                )
                .unwrap();
        }
        let err = match SqlitePersistence::open(&path) {
            Ok(_) => panic!("a database from a newer version was opened"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("schema version 4"), "{err}");
        assert!(err.contains("newer ReFHIR"), "{err}");
        let report = migration_report(&path).unwrap();
        assert!(!report.can_start());
        // Nothing was written to it.
        let connection = Connection::open(&path).unwrap();
        assert_eq!(versions(&connection), [1, 2, 3, 4]);
        remove_db(&path);
    }

    #[test]
    fn a_database_whose_applied_migration_was_edited_refuses_to_start() {
        let path = temp_db("edited");
        {
            SqlitePersistence::open(&path).unwrap();
            let connection = Connection::open(&path).unwrap();
            connection
                .execute("UPDATE schema_migrations SET checksum = 'other'", [])
                .unwrap();
        }
        let err = match SqlitePersistence::open(&path) {
            Ok(_) => panic!("opened with a different checksum"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("checksum"), "{err}");
        remove_db(&path);
    }

    static FAILING: [Migration; 4] = [
        Migration {
            version: 1,
            name: "baseline",
            sql: include_str!("../migrations/0001_baseline.sql"),
        },
        Migration {
            version: 2,
            name: "governance",
            sql: include_str!("../migrations/0002_governance.sql"),
        },
        Migration {
            version: 3,
            name: "near_index",
            sql: include_str!("../migrations/0003_near_index.sql"),
        },
        Migration {
            version: 4,
            name: "half_done",
            // The table is created, then the second statement fails.
            sql: "CREATE TABLE half_done (x INTEGER); INSERT INTO no_such_table VALUES (1);",
        },
    ];

    #[test]
    fn a_failed_migration_rolls_everything_back() {
        // On a new database the baseline goes with it: all pending migrations are one
        // transaction, so there is no state in between.
        let mut fresh = Connection::open_in_memory().unwrap();
        let err = run(&mut fresh, &FAILING).unwrap_err().to_string();
        assert!(err.contains("migration 4 (half_done) failed"), "{err}");
        assert!(err.contains("no_such_table"), "{err}");
        assert!(!table_exists(&fresh, "half_done"));
        assert!(!table_exists(&fresh, "documents"));
        assert!(!table_exists(&fresh, "schema_migrations"));

        // On a database already at the last version this build knows, the earlier work is kept
        // and the failed one is not.
        let mut at_one = Connection::open_in_memory().unwrap();
        run(&mut at_one, MIGRATIONS).unwrap();
        assert!(run(&mut at_one, &FAILING).is_err());
        assert_eq!(versions(&at_one), [1, 2, 3]);
        assert!(!table_exists(&at_one, "half_done"));
        assert!(table_exists(&at_one, "documents"));
    }

    static WITH_SECOND: [Migration; 4] = [
        Migration {
            version: 1,
            name: "baseline",
            sql: include_str!("../migrations/0001_baseline.sql"),
        },
        Migration {
            version: 2,
            name: "governance",
            sql: include_str!("../migrations/0002_governance.sql"),
        },
        Migration {
            version: 3,
            name: "near_index",
            sql: include_str!("../migrations/0003_near_index.sql"),
        },
        Migration {
            version: 4,
            name: "probe",
            // Not idempotent: a second run is an error.
            sql: "CREATE TABLE probe (n INTEGER); INSERT INTO probe VALUES (1);",
        },
    ];

    #[test]
    fn processes_opening_one_file_at_once_migrate_it_once() {
        let path = temp_db("together");
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut connection = Connection::open(&path).unwrap();
                    // As `from_connection` does: switching to WAL while another connection does
                    // the same fails at once with BUSY (the busy handler is not consulted), so
                    // the timeout is set on its own and the switch is allowed to lose the race.
                    connection
                        .execute_batch("PRAGMA busy_timeout=5000;")
                        .unwrap();
                    let _ = connection.execute_batch("PRAGMA journal_mode=WAL;");
                    run(&mut connection, &WITH_SECOND)
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().expect("every opener gets through");
        }
        let connection = Connection::open(&path).unwrap();
        assert_eq!(versions(&connection), [1, 2, 3, 4]);
        let rows: i64 = connection
            .query_row("SELECT COUNT(*) FROM probe", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1, "the last migration ran once");
        remove_db(&path);
    }

    #[test]
    fn a_pending_migration_is_applied_to_a_database_at_the_last_version_this_build_knows() {
        let mut connection = Connection::open_in_memory().unwrap();
        run(&mut connection, MIGRATIONS).unwrap();
        run(&mut connection, &WITH_SECOND).unwrap();
        assert_eq!(versions(&connection), [1, 2, 3, 4]);
        assert!(table_exists(&connection, "probe"));
    }

    /// A `search_parameters` row, for the near-index tests.
    fn parameter(
        connection: &Connection,
        package: i64,
        url: &str,
        base: &str,
        code: &str,
        status: &str,
    ) {
        connection
            .execute(
                "INSERT INTO search_parameters \
                 (package_id, url, base, code, param_type, expression, status, json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, '{}')",
                rusqlite::params![
                    package,
                    url,
                    base,
                    code,
                    if code == "near" { "special" } else { "string" },
                    status
                ],
            )
            .unwrap();
    }

    /// (status, url) of every parameter row, in a fixed order.
    fn statuses(connection: &Connection) -> Vec<(String, String)> {
        let mut statement = connection
            .prepare("SELECT status, package_id || ' ' || url FROM search_parameters ORDER BY 2, 1")
            .unwrap();
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    }

    /// (id, package, watermark, tablet, cursor, done, claimed epoch, its parameter urls).
    #[allow(clippy::type_complexity)]
    fn backfills(
        connection: &Connection,
    ) -> Vec<(
        i64,
        i64,
        i64,
        String,
        Option<String>,
        i64,
        Option<i64>,
        Vec<String>,
    )> {
        let mut statement = connection
            .prepare(
                "SELECT id, package_id, watermark_ts, tablet, cursor_id, done, claimed_epoch \
                 FROM index_backfills ORDER BY id",
            )
            .unwrap();
        let rows: Vec<_> = statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        rows.into_iter()
            .map(|(id, package, watermark, tablet, cursor, done, claimed): (i64, i64, i64, String, Option<String>, i64, Option<i64>)| {
                let mut urls = connection
                    .prepare("SELECT url FROM backfill_parameter WHERE backfill_id = ?1 ORDER BY url")
                    .unwrap();
                let urls = urls
                    .query_map([id], |row| row.get(0))
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
                (id, package, watermark, tablet, cursor, done, claimed, urls)
            })
            .collect()
    }

    #[test]
    fn a_ready_near_parameter_goes_back_to_pending_with_a_backfill_for_its_type() {
        let mut connection = Connection::open_in_memory().unwrap();
        run(&mut connection, &MIGRATIONS[..2]).unwrap();
        // Two packages that each carry `near` for Location, a `near` that was already pending
        // (an install that had not finished: it has a job of its own), a parameter of another name,
        // and a job that is half way through.
        parameter(&connection, 1, "u/near", "Location", "near", "ready");
        parameter(&connection, 1, "u/name", "Location", "name", "ready");
        parameter(&connection, 2, "u/near", "Location", "near", "ready");
        parameter(&connection, 3, "u/near", "Location", "near", "pending");
        parameter(&connection, 3, "u/other", "Patient", "name", "pending");
        connection
            .execute_batch(
                "INSERT INTO index_backfills (package_id, watermark_ts, tablet, cursor_id, done, claimed_epoch) \
                 VALUES (3, 4, 'Location', 'l2', 0, 7); \
                 INSERT INTO backfill_parameter (backfill_id, url) VALUES (1, 'u/near'); \
                 INSERT INTO documents (id, ts, table_id, json_value, deleted, prev_ts) \
                 VALUES (x'6c31', 9, x'4c', '{}', 0, NULL), (x'6c32', 12, x'4c', '{}', 0, NULL);",
            )
            .unwrap();

        run(&mut connection, MIGRATIONS).unwrap();

        // What was ready and is `near` waits for its index; nothing else changed, and nothing the
        // migration borrowed is left.
        let status = |status: &str, url: &str| (status.to_string(), url.to_string());
        assert_eq!(
            statuses(&connection),
            [
                status("ready", "1 u/name"),
                status("pending", "1 u/near"),
                status("pending", "2 u/near"),
                status("pending", "3 u/near"),
                status("pending", "3 u/other"),
            ]
        );
        let jobs = backfills(&connection);
        // The job that was there is as it was. One new job for each package whose `near` was
        // ready, with the newest document as its watermark, nothing done yet, and only `near`.
        assert_eq!(
            jobs,
            [
                (
                    1,
                    3,
                    4,
                    "Location".into(),
                    Some("l2".into()),
                    0,
                    Some(7),
                    vec!["u/near".into()]
                ),
                (
                    2,
                    1,
                    12,
                    "Location".into(),
                    None,
                    0,
                    None,
                    vec!["u/near".into()]
                ),
                (
                    3,
                    2,
                    12,
                    "Location".into(),
                    None,
                    0,
                    None,
                    vec!["u/near".into()]
                ),
            ]
        );
    }

    #[test]
    fn a_database_with_no_near_parameter_is_not_touched_by_the_near_index_migration() {
        let mut connection = Connection::open_in_memory().unwrap();
        run(&mut connection, &MIGRATIONS[..2]).unwrap();
        parameter(&connection, 1, "u/name", "Location", "name", "ready");
        run(&mut connection, MIGRATIONS).unwrap();
        assert_eq!(
            statuses(&connection),
            [("ready".to_string(), "1 u/name".to_string())]
        );
        assert!(backfills(&connection).is_empty());
        // And a new database, which has no parameters yet, gets nothing either.
        let mut fresh = Connection::open_in_memory().unwrap();
        run(&mut fresh, MIGRATIONS).unwrap();
        assert!(statuses(&fresh).is_empty() && backfills(&fresh).is_empty());
    }

    #[test]
    fn a_report_does_not_create_the_file_it_is_asked_about() {
        let path = temp_db("report");
        let report = migration_report(&path).unwrap();
        assert!(report.can_start());
        assert!(report.render(true).contains("new database"));
        assert!(!path.exists(), "a read-only report created the database");
    }
}
