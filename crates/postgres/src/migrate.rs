//! Applies the compiled-in schema migrations to a Postgres database.
//!
//! The rules, and what is recorded, are in `refhir_persistence::migrate`. Here: all pending
//! migrations run in one transaction, behind a transaction-scoped advisory lock, so two
//! processes starting together cannot both migrate: the second waits for the first, then finds
//! nothing left to do.

use postgres::{Client, GenericClient};
use refhir_persistence::migrate::{self, Adoption, AppliedMigration, Existing, Migration, Report};
use refhir_persistence::PersistError;

use crate::{connect_client, definite, off_runtime, ErrorText};

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
];

/// The same two steps for a database in the shared tenant layout. A database has one stream or the
/// other for good; see `crate::tenant::Layout`.
pub(crate) static SHARED_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "baseline",
        sql: include_str!("../migrations-shared/0001_baseline.sql"),
    },
    Migration {
        version: 2,
        name: "governance",
        sql: include_str!("../migrations-shared/0002_governance.sql"),
    },
    Migration {
        version: 3,
        name: "row_security",
        sql: include_str!("../migrations-shared/0003_row_security.sql"),
    },
];

const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS schema_migrations (
    version BIGINT PRIMARY KEY,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    checksum TEXT NOT NULL
)";

/// Held for the length of a migration transaction. (The key schema setup has always used.)
const MIGRATION_LOCK: i64 = 7428310;

/// `index_backfills` has the column a claim is recorded in. Tables made by an earlier release
/// lack it.
const HAS_CLAIM_COLUMN: &str = "EXISTS (SELECT 1 FROM information_schema.columns \
     WHERE table_name = 'index_backfills' AND column_name = 'claimed_epoch')";

/// `jobs` has the columns a lease is recorded in. A table made before jobs were leased lacks
/// them.
const HAS_JOB_LEASE_COLUMNS: &str = "(SELECT count(*) FROM information_schema.columns \
     WHERE table_name = 'jobs' AND column_name IN ('lease_owner', 'lease_until')) = 2";

/// An index that a failed concurrent build left behind exists but is not usable.
const HAS_INVALID_INDEX: &str = "EXISTS (SELECT 1 FROM pg_index i \
     JOIN pg_class c ON c.oid = i.indexrelid \
     WHERE c.relname = 'indexes_by_ts' AND NOT i.indisvalid)";

/// Longest a migration waits for a lock: the advisory lock another migrating process holds, or a
/// table lock held by an open transaction. Past it startup fails with an error instead of hanging.
const LOCK_TIMEOUT: &str = "120s";

/// How long a reader waits for the primary to migrate.
const WAIT_FOR_PRIMARY: std::time::Duration = std::time::Duration::from_secs(120);

/// Brings the database up to `known`, or fails and leaves it exactly as it was.
pub(crate) fn run(client: &mut Client, known: &'static [Migration]) -> Result<(), PersistError> {
    // A database that is current is decided from a plain read. Taking the lock, or running any
    // DDL (even `... IF NOT EXISTS` locks its table first), would stall a running primary's
    // writes whenever a standby, a replica or a restart connects.
    let applied = read_applied(client)?;
    if !applied.is_empty() && migrate::pending(known, &applied)?.is_empty() {
        return Ok(());
    }
    let mut tx = client.transaction().map_err(definite)?;
    // Before anything is created: concurrent `CREATE TABLE IF NOT EXISTS` can still collide.
    tx.batch_execute(&format!(
        "SET LOCAL lock_timeout = '{LOCK_TIMEOUT}'; SELECT pg_advisory_xact_lock({MIGRATION_LOCK})"
    ))
    .map_err(|err| {
        definite(format!(
            "could not take the schema migration lock within {LOCK_TIMEOUT}: {}",
            err.error_text()
        ))
    })?;
    tx.batch_execute(CREATE_TABLE).map_err(definite)?;
    // Decided again under the lock: another process may have just done it.
    let applied = read_applied(&mut tx)?;
    let held = if applied.is_empty() {
        existing(&mut tx)?
    } else {
        Existing::Complete // not consulted once versions are recorded
    };
    let plan = migrate::plan(known, &applied, held)?;
    match plan.adoption {
        Adoption::No => {}
        Adoption::Record => record(&mut tx, &known[0])?,
        Adoption::CompleteThenRecord => {
            complete_older_schema(&mut tx, &known[0])?;
            record(&mut tx, &known[0])?;
        }
    }
    for migration in plan.run {
        tx.batch_execute(migration.sql).map_err(|err| {
            definite(format!(
                "schema migration {} ({}) failed: {}; the database is unchanged",
                migration.version,
                migration.name,
                err.error_text()
            ))
        })?;
        record(&mut tx, migration)?;
    }
    tx.commit().map_err(definite)
}

fn record(tx: &mut impl GenericClient, migration: &Migration) -> Result<(), PersistError> {
    tx.execute(
        "INSERT INTO schema_migrations (version, checksum) VALUES ($1, $2)",
        &[&migration.version, &migration.checksum()],
    )
    .map_err(definite)?;
    Ok(())
}

/// What every earlier release did at every start: create what is missing, add the columns it
/// grew later (the baseline's `ADD COLUMN IF NOT EXISTS`). Only ever adds. Only reached for a
/// database that lacks something, because these statements lock the tables they name.
fn complete_older_schema(
    tx: &mut impl GenericClient,
    baseline: &Migration,
) -> Result<(), PersistError> {
    // A build that died may have left `indexes_by_ts` behind unusable, and `IF NOT EXISTS`
    // would then keep it.
    let invalid: bool = tx
        .query_one(&format!("SELECT {HAS_INVALID_INDEX}"), &[])
        .map_err(definite)?
        .get(0);
    if invalid {
        tx.batch_execute("DROP INDEX IF EXISTS indexes_by_ts")
            .map_err(definite)?;
    }
    tx.batch_execute(baseline.sql).map_err(definite)
}

fn read_applied(client: &mut impl GenericClient) -> Result<Vec<AppliedMigration>, PersistError> {
    let exists: bool = client
        .query_one("SELECT to_regclass('schema_migrations') IS NOT NULL", &[])
        .map_err(definite)?
        .get(0);
    if !exists {
        return Ok(Vec::new());
    }
    let rows = client
        .query(
            r#"SELECT version,
                      to_char(applied_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
                      checksum
               FROM schema_migrations ORDER BY version"#,
            &[],
        )
        .map_err(definite)?;
    Ok(rows
        .iter()
        .map(|row| AppliedMigration {
            version: row.get(0),
            applied_at: row.get(1),
            checksum: row.get(2),
        })
        .collect())
}

/// What an unversioned database holds, judged against version 1.
fn existing(client: &mut impl GenericClient) -> Result<Existing, PersistError> {
    let objects: Vec<&str> = migrate::BASELINE_OBJECTS.to_vec();
    let any: bool = client
        .query_one(
            "SELECT COALESCE(bool_or(to_regclass(object) IS NOT NULL), false) \
             FROM unnest($1::text[]) AS object",
            &[&objects],
        )
        .map_err(definite)?
        .get(0);
    if !any {
        return Ok(Existing::Empty);
    }
    let complete: bool = client
        .query_one(
            &format!(
                "SELECT COALESCE((SELECT bool_and(to_regclass(object) IS NOT NULL) \
                                  FROM unnest($1::text[]) AS object), false) \
                        AND {HAS_CLAIM_COLUMN} AND {HAS_JOB_LEASE_COLUMNS} \
                        AND NOT {HAS_INVALID_INDEX}"
            ),
            &[&objects],
        )
        .map_err(definite)?
        .get(0);
    Ok(if complete {
        Existing::Complete
    } else {
        Existing::Partial
    })
}

/// For a process that never runs DDL (a replica): waits until the primary has migrated the
/// database to what this build knows, or the database already has that schema unversioned. A database ahead of this build is refused at once, not
/// waited on.
pub(crate) fn wait_until_current(
    client: &mut Client,
    known: &'static [Migration],
) -> Result<(), PersistError> {
    let deadline = std::time::Instant::now() + WAIT_FOR_PRIMARY;
    loop {
        let applied = read_applied(client)?;
        // A database that has the whole current schema but was never versioned (its primary
        // still runs the previous release) is usable as it is; only a missing or older one waits.
        let held = if applied.is_empty() {
            existing(client)?
        } else {
            Existing::Complete
        };
        let plan = migrate::plan(known, &applied, held)?;
        if plan.adoption != Adoption::CompleteThenRecord && plan.run.is_empty() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(PersistError::Definite(format!(
                "the database is at schema version {}, but this build needs {}; start the \
                 primary first, which applies the migrations",
                applied.len(),
                known.len()
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// What the database at `url` holds and what startup would do to it, without changing it. A
/// database in the shared tenant layout is judged against the shared migrations.
pub fn migration_report(url: &str) -> Result<Report, PersistError> {
    off_runtime(|| {
        let mut client = connect_client(url)?;
        if crate::tenant::detect_layout(&mut client)? == crate::tenant::Layout::Shared {
            return Ok(Report::new(
                SHARED_MIGRATIONS,
                read_applied(&mut client)?,
                Existing::Complete,
            ));
        }
        Ok(Report::new(
            MIGRATIONS,
            read_applied(&mut client)?,
            existing(&mut client)?,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{fresh_database, test_url};
    use crate::{ConnectOptions, PostgresPersistence};
    use postgres::NoTls;

    fn connect(url: &str) -> Client {
        Client::connect(url, NoTls).expect("connect")
    }

    fn versions(client: &mut Client) -> Vec<i64> {
        read_applied(client)
            .unwrap()
            .iter()
            .map(|row| row.version)
            .collect()
    }

    fn exists(client: &mut Client, name: &str) -> bool {
        client
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[&name])
            .unwrap()
            .get(0)
    }

    fn index_is_valid(client: &mut Client) -> bool {
        client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
                 WHERE c.relname = 'indexes_by_ts' AND i.indisvalid)",
                &[],
            )
            .unwrap()
            .get(0)
    }

    /// Migration 1 is applied to databases that cannot be changed afterwards, and every one of
    /// them records this checksum. If this fails you edited the baseline file: revert it and
    /// add a new migration instead.
    #[test]
    fn the_baseline_is_frozen() {
        migrate::assert_well_formed(MIGRATIONS);
        assert_eq!(
            MIGRATIONS[0].checksum(),
            "0340e5ffad094dc9569da5aed0a1a90c9d61b4e8a69bf51af11ec930bc0850e3",
            "migration 1 was edited"
        );
    }

    /// Migration 2 is pinned like the baseline, from the first release that carries it.
    #[test]
    fn the_governance_migration_is_frozen() {
        assert_eq!(MIGRATIONS[1].name, "governance");
        assert_eq!(
            MIGRATIONS[1].checksum(),
            "97715d421cbf6d3fe408d5ae921b2735b36b30c56ac7d1f70c8ddce2c7a6ca44",
            "migration 2 was edited"
        );
    }

    #[test]
    fn a_database_at_the_baseline_gets_the_governance_tables_and_keeps_what_it_held() {
        let Some(admin) = test_url("postgres migrations: baseline to governance") else {
            return;
        };
        let url = fresh_database(&admin, "mgovernance");
        let mut client = connect(&url);
        run(&mut client, &MIGRATIONS[..1]).unwrap();
        client
            .batch_execute("INSERT INTO persistence_globals (key, json_value) VALUES ('kept', '1')")
            .unwrap();
        assert_eq!(versions(&mut client), [1]);
        assert!(!exists(&mut client, "audit_log"));

        PostgresPersistence::connect(&url).unwrap();
        assert_eq!(versions(&mut client), [1, 2]);
        for table in [
            "usage_daily",
            "request_log",
            "governance_members",
            "audit_log",
        ] {
            assert!(exists(&mut client, table), "{table}");
        }
        let kept: String = client
            .query_one(
                "SELECT json_value FROM persistence_globals WHERE key = 'kept'",
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(kept, "1");
    }

    #[test]
    fn a_new_database_gets_the_baseline_and_a_record_of_it() {
        let Some(admin) = test_url("postgres migrations: new database") else {
            return;
        };
        let url = fresh_database(&admin, "mnew");
        PostgresPersistence::connect(&url).unwrap();
        let mut client = connect(&url);
        let applied = read_applied(&mut client).unwrap();
        assert_eq!(applied.len(), 2);
        assert_eq!(applied[0].version, 1);
        assert_eq!(applied[0].checksum, MIGRATIONS[0].checksum());
        assert!(
            applied[0].applied_at.ends_with('Z'),
            "{}",
            applied[0].applied_at
        );
        // The list of objects used to recognise an old database is what the SQL creates, and
        // the timestamp index is built, valid, in the same transaction.
        assert_eq!(existing(&mut client).unwrap(), Existing::Complete);
        assert!(index_is_valid(&mut client));
        // Connecting again changes nothing.
        PostgresPersistence::connect(&url).unwrap();
        assert_eq!(versions(&mut client), [1, 2]);
    }

    #[test]
    fn an_unversioned_database_with_the_current_tables_is_adopted_without_rerunning() {
        let Some(admin) = test_url("postgres migrations: adoption") else {
            return;
        };
        let url = fresh_database(&admin, "madopt");
        let mut client = connect(&url);
        // What the last unversioned release left behind, with data in it.
        client.batch_execute(MIGRATIONS[0].sql).unwrap();
        client
            .batch_execute("INSERT INTO persistence_globals (key, json_value) VALUES ('kept', '1')")
            .unwrap();
        assert!(!exists(&mut client, "schema_migrations"));
        let report = migration_report(&url).unwrap();
        assert_eq!(report.existing, Existing::Complete);
        assert!(report.render(false).contains("without running it"));
        assert!(
            !exists(&mut client, "schema_migrations"),
            "a report created the table"
        );

        // Re-running the baseline would take an exclusive lock on `jobs` (ADD COLUMN IF NOT
        // EXISTS does, even when the column is there) and wait for this open write. Adoption
        // must not touch the existing tables at all.
        client
            .batch_execute(
                "BEGIN; INSERT INTO jobs (id, kind, status, request, created_at) \
                 VALUES ('j', 'export', 'running', '{}', 5)",
            )
            .unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        let other = url.clone();
        std::thread::spawn(move || {
            let _ = done.send(PostgresPersistence::connect(&other).map(|_| ()));
        });
        finished
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("adoption waited for a lock: it re-ran the baseline")
            .unwrap();
        client.batch_execute("COMMIT").unwrap();

        assert_eq!(versions(&mut client), [1, 2]);
        assert_eq!(
            read_applied(&mut client).unwrap()[0].checksum,
            MIGRATIONS[0].checksum()
        );
        let kept: String = client
            .query_one(
                "SELECT json_value FROM persistence_globals WHERE key = 'kept'",
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(kept, "1");
    }

    #[test]
    fn an_unversioned_database_from_an_older_release_is_completed_then_recorded() {
        let Some(admin) = test_url("postgres migrations: older release") else {
            return;
        };
        let url = fresh_database(&admin, "molder");
        let mut client = connect(&url);
        // `index_backfills` before claims, with a job in it, and nothing else.
        client
            .batch_execute(
                "CREATE TABLE index_backfills (
                    id BIGSERIAL PRIMARY KEY,
                    package_id BIGINT NOT NULL,
                    watermark_ts BIGINT NOT NULL,
                    tablet TEXT NOT NULL,
                    cursor_id TEXT,
                    done INTEGER NOT NULL
                );
                INSERT INTO index_backfills (package_id, watermark_ts, tablet, cursor_id, done)
                VALUES (1, 7, 'Patient', 'p3', 0);",
            )
            .unwrap();
        assert_eq!(migration_report(&url).unwrap().existing, Existing::Partial);
        PostgresPersistence::connect(&url).unwrap();
        assert_eq!(versions(&mut client), [1, 2]);
        assert_eq!(existing(&mut client).unwrap(), Existing::Complete);
        let cursor: String = client
            .query_one("SELECT cursor_id FROM index_backfills", &[])
            .unwrap()
            .get(0);
        assert_eq!(cursor, "p3");
    }

    #[test]
    fn an_unusable_timestamp_index_from_an_older_release_is_replaced() {
        let Some(admin) = test_url("postgres migrations: invalid index") else {
            return;
        };
        let url = fresh_database(&admin, "minvalid");
        let mut client = connect(&url);
        client.batch_execute(MIGRATIONS[0].sql).unwrap();
        // What a concurrent build that died leaves behind. Needs a role that may edit the
        // catalog; without one there is nothing to test here.
        if client
            .batch_execute(
                "UPDATE pg_index SET indisvalid = false \
                 WHERE indexrelid = 'indexes_by_ts'::regclass",
            )
            .is_err()
        {
            eprintln!("role cannot edit pg_index, skipping");
            return;
        }
        assert!(!index_is_valid(&mut client));
        assert_eq!(existing(&mut client).unwrap(), Existing::Partial);
        PostgresPersistence::connect(&url).unwrap();
        assert!(index_is_valid(&mut client));
        assert_eq!(versions(&mut client), [1, 2]);
    }

    #[test]
    fn a_database_from_a_newer_version_refuses_to_start_and_is_left_alone() {
        let Some(admin) = test_url("postgres migrations: newer database") else {
            return;
        };
        let url = fresh_database(&admin, "mnewer");
        PostgresPersistence::connect(&url).unwrap();
        let mut client = connect(&url);
        client
            .execute(
                "INSERT INTO schema_migrations (version, checksum) VALUES (3, 'from-the-future')",
                &[],
            )
            .unwrap();

        let err = match PostgresPersistence::connect(&url) {
            Ok(_) => panic!("a database from a newer version was opened"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("schema version 3"), "{err}");
        assert!(err.contains("newer ReFHIR"), "{err}");

        // A replica refuses at once too, instead of waiting for a primary that cannot help.
        let started = std::time::Instant::now();
        let options = ConnectOptions {
            pool_size: 1,
            create_schema: false,
            ..ConnectOptions::default()
        };
        assert!(PostgresPersistence::connect_with(&url, &options).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));

        assert!(!migration_report(&url).unwrap().can_start());
        assert_eq!(versions(&mut client), [1, 2, 3], "nothing was written");
    }

    #[test]
    fn a_database_whose_applied_migration_was_edited_refuses_to_start() {
        let Some(admin) = test_url("postgres migrations: edited migration") else {
            return;
        };
        let url = fresh_database(&admin, "medited");
        PostgresPersistence::connect(&url).unwrap();
        connect(&url)
            .batch_execute("UPDATE schema_migrations SET checksum = 'other'")
            .unwrap();
        let err = match PostgresPersistence::connect(&url) {
            Ok(_) => panic!("opened with a different checksum"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("checksum"), "{err}");
    }

    static FAILING: [Migration; 3] = [
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
            name: "half_done",
            // The table is created, then the second statement fails.
            sql: "CREATE TABLE half_done (x INTEGER); INSERT INTO no_such_table VALUES (1);",
        },
    ];

    #[test]
    fn a_failed_migration_rolls_everything_back() {
        let Some(admin) = test_url("postgres migrations: rollback") else {
            return;
        };
        // On a new database the baseline goes with it: all pending migrations are one
        // transaction, so there is no state in between.
        let url = fresh_database(&admin, "mfail");
        let mut client = connect(&url);
        let err = run(&mut client, &FAILING).unwrap_err().to_string();
        assert!(err.contains("migration 3 (half_done) failed"), "{err}");
        assert!(err.contains("no_such_table"), "{err}");
        assert!(!exists(&mut client, "half_done"));
        assert!(!exists(&mut client, "documents"));
        assert!(!exists(&mut client, "schema_migrations"));

        // On a database already at version 1 the earlier work is kept and the failed one is not.
        run(&mut client, MIGRATIONS).unwrap();
        assert!(run(&mut client, &FAILING).is_err());
        assert_eq!(versions(&mut client), [1, 2]);
        assert!(!exists(&mut client, "half_done"));
        assert!(exists(&mut client, "documents"));
        // And the connection is usable afterwards: the failed transaction did not wedge it.
        assert_eq!(versions(&mut client), [1, 2]);
    }

    static WITH_SECOND: [Migration; 3] = [
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
            name: "probe",
            // Not idempotent: a second run is an error.
            sql: "CREATE TABLE probe (n INTEGER); INSERT INTO probe VALUES (1);",
        },
    ];

    /// Runs `run` on `threads` connections at the same instant.
    fn run_together(url: &str, threads: usize) -> Vec<Result<(), PersistError>> {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let url = url.to_string();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut client = Client::connect(&url, NoTls).unwrap();
                    barrier.wait();
                    run(&mut client, &WITH_SECOND)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    }

    #[test]
    fn processes_starting_at_once_on_a_new_database_migrate_it_once() {
        let Some(admin) = test_url("postgres migrations: concurrent start") else {
            return;
        };
        let url = fresh_database(&admin, "mtogether");
        for result in run_together(&url, 8) {
            result.expect("every process gets through");
        }
        let mut client = connect(&url);
        assert_eq!(versions(&mut client), [1, 2, 3]);
        let rows: i64 = client
            .query_one("SELECT COUNT(*) FROM probe", &[])
            .unwrap()
            .get(0);
        assert_eq!(rows, 1, "the second migration ran once");
        assert!(index_is_valid(&mut client));
    }

    #[test]
    fn processes_starting_at_once_on_an_unversioned_database_adopt_it_once() {
        let Some(admin) = test_url("postgres migrations: concurrent adoption") else {
            return;
        };
        let url = fresh_database(&admin, "mtogetheradopt");
        let mut client = connect(&url);
        client.batch_execute(MIGRATIONS[0].sql).unwrap();
        for result in run_together(&url, 8) {
            result.expect("every process gets through");
        }
        assert_eq!(versions(&mut client), [1, 2, 3]);
        let rows: i64 = client
            .query_one("SELECT COUNT(*) FROM probe", &[])
            .unwrap()
            .get(0);
        assert_eq!(rows, 1);
    }

    #[test]
    fn a_process_waiting_on_the_lock_does_not_block_the_migration_it_waits_for() {
        let Some(admin) = test_url("postgres migrations: lock holder") else {
            return;
        };
        // A process holding the advisory lock (as another migrating process would) makes a
        // starter wait; it proceeds once the lock is released, and does not fail.
        let url = fresh_database(&admin, "mlock");
        let mut holder = connect(&url);
        holder
            .batch_execute(&format!(
                "BEGIN; SELECT pg_advisory_xact_lock({MIGRATION_LOCK})"
            ))
            .unwrap();
        let waiter_url = url.clone();
        let waiter = std::thread::spawn(move || {
            let mut client = Client::connect(&waiter_url, NoTls).unwrap();
            run(&mut client, MIGRATIONS)
        });
        std::thread::sleep(std::time::Duration::from_millis(800));
        assert!(
            !waiter.is_finished(),
            "the starter did not wait for the lock"
        );
        holder.batch_execute("COMMIT").unwrap();
        waiter.join().unwrap().unwrap();
        assert_eq!(versions(&mut holder), [1, 2]);
    }

    #[test]
    fn a_replica_starts_on_a_complete_unversioned_database_but_waits_for_a_partial_one() {
        let Some(admin) = test_url("postgres migrations: replica on unversioned") else {
            return;
        };
        let options = ConnectOptions {
            pool_size: 1,
            create_schema: false,
            ..ConnectOptions::default()
        };
        // Its primary still runs the previous release: every table, no schema_migrations.
        let complete = fresh_database(&admin, "mreplicaok");
        connect(&complete).batch_execute(MIGRATIONS[0].sql).unwrap();
        // A build that knows only the baseline finds that usable as it is.
        wait_until_current(&mut connect(&complete), &MIGRATIONS[..1]).expect("usable as it is");
        assert!(
            !exists(&mut connect(&complete), "schema_migrations"),
            "a replica ran DDL"
        );
        // This build knows a later migration too, which only a primary applies: the replica
        // waits for it, and does not run it itself.
        let url = complete.clone();
        let replica_options = options.clone();
        let behind = std::thread::spawn(move || {
            PostgresPersistence::connect_with(&url, &replica_options).map(|_| ())
        });
        std::thread::sleep(std::time::Duration::from_millis(1200));
        assert!(!behind.is_finished(), "a replica started without version 2");
        assert!(!exists(&mut connect(&complete), "audit_log"));
        PostgresPersistence::connect(&complete).unwrap();
        behind
            .join()
            .unwrap()
            .expect("starts once the primary has migrated");

        // An older schema is not usable yet: it waits for the primary to complete it.
        let partial = fresh_database(&admin, "mreplicawait");
        connect(&partial)
            .batch_execute(
                "CREATE TABLE index_backfills (id BIGSERIAL PRIMARY KEY, package_id BIGINT NOT NULL, \
                 watermark_ts BIGINT NOT NULL, tablet TEXT NOT NULL, cursor_id TEXT, \
                 done INTEGER NOT NULL)",
            )
            .unwrap();
        let url = partial.clone();
        let waiting = std::thread::spawn(move || {
            PostgresPersistence::connect_with(&url, &options).map(|_| ())
        });
        std::thread::sleep(std::time::Duration::from_millis(1200));
        assert!(
            !waiting.is_finished(),
            "a replica started on a partial schema"
        );
        PostgresPersistence::connect(&partial).unwrap();
        waiting
            .join()
            .unwrap()
            .expect("starts once the primary has migrated");
    }

    #[test]
    fn a_report_does_not_change_the_database() {
        let Some(admin) = test_url("postgres migrations: report") else {
            return;
        };
        let url = fresh_database(&admin, "mreport");
        let report = migration_report(&url).unwrap();
        assert!(report.can_start());
        assert!(report.render(true).contains("new database"));
        let mut client = connect(&url);
        assert!(!exists(&mut client, "schema_migrations"));
        assert!(!exists(&mut client, "documents"));
    }
}
