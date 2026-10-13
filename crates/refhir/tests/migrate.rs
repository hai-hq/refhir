//! `refhir migrate`, and the server refusing a database it cannot use, through the real binary.
//!
//! The SQLite cases need nothing. The Postgres one needs `HAIFHIR_POSTGRES_URL` (a role that
//! may `CREATE DATABASE`) and is skipped without it, unless `REFHIR_REQUIRE_POSTGRES` is set.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

mod common;

fn temp_db(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("refhir-migrate-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("refhir.sqlite")
}

fn migrate(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_refhir"))
        .arg("migrate")
        .args(args)
        // Never pick up a config file or database from the machine running the tests.
        .env_remove("REFHIR_CONFIG")
        .env_remove("REFHIR_POSTGRES_URL")
        .current_dir(std::env::temp_dir())
        .output()
        .expect("run refhir migrate")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn sqlite(path: &Path, sql: &str) {
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

#[test]
fn status_of_a_database_that_does_not_exist_creates_nothing() {
    let path = temp_db("missing");
    let output = migrate(&["--status", "--sqlite", path.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("new database"),
        "{}",
        stdout(&output)
    );
    assert!(!path.exists(), "a status check created the database");
}

#[test]
fn status_and_dry_run_of_a_current_database() {
    let path = temp_db("current");
    drop(refhir_sqlite::SqlitePersistence::open(&path).unwrap());
    let file = path.to_str().unwrap();

    let status = migrate(&["--status", "--sqlite", file]);
    assert!(status.status.success(), "{}", stderr(&status));
    let text = stdout(&status);
    assert!(text.contains("up to date at version 1"), "{text}");
    assert!(text.contains("baseline"), "{text}");
    assert!(
        !text.contains("would run"),
        "--status does not print SQL: {text}"
    );

    let dry_run = migrate(&["--dry-run", "--sqlite", file]);
    assert!(dry_run.status.success(), "{}", stderr(&dry_run));
    assert!(
        stdout(&dry_run).contains("pending: none"),
        "{}",
        stdout(&dry_run)
    );
}

#[test]
fn an_unversioned_database_is_reported_as_one_that_would_be_adopted() {
    let path = temp_db("unversioned");
    drop(refhir_sqlite::SqlitePersistence::open(&path).unwrap());
    sqlite(&path, "DROP TABLE schema_migrations");

    let output = migrate(&["--dry-run", "--sqlite", path.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("without running it"), "{text}");
    assert!(text.contains("adopt (recorded, not re-run)"), "{text}");
    // And looking did not adopt it.
    let still_unversioned = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'schema_migrations'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(still_unversioned, 0);
}

#[test]
fn a_database_from_a_newer_version_is_reported_and_exits_nonzero() {
    let path = temp_db("newer-report");
    drop(refhir_sqlite::SqlitePersistence::open(&path).unwrap());
    sqlite(
        &path,
        "INSERT INTO schema_migrations (version, checksum) VALUES (2, 'future')",
    );
    let output = migrate(&["--status", "--sqlite", path.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stdout(&output).contains("cannot start"),
        "{}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("newer ReFHIR"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn it_needs_to_be_told_what_to_do() {
    let output = migrate(&[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("--status or --dry-run"));
    let both = migrate(&["--status", "--dry-run"]);
    assert_eq!(both.status.code(), Some(1));
}

/// The server, not just the report, refuses: it exits without serving.
#[test]
fn the_server_refuses_to_start_on_a_database_from_a_newer_version() {
    let path = temp_db("newer-serve");
    drop(refhir_sqlite::SqlitePersistence::open(&path).unwrap());
    sqlite(
        &path,
        "INSERT INTO schema_migrations (version, checksum) VALUES (2, 'future')",
    );
    let dir = path.parent().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_refhir"))
        .current_dir(dir)
        .args(["--sqlite", path.to_str().unwrap(), "--bind", "127.0.0.1:0"])
        .env("REFHIR_SKIP_DEFAULT_PACKAGES", "1")
        .env("REFHIR_PACKAGE_CACHE", dir.join("packages"))
        .env_remove("REFHIR_CONFIG")
        .env_remove("REFHIR_POSTGRES_URL")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start refhir");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the server was still running on a database from a newer version");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let output = child.wait_with_output().unwrap();
    assert_eq!(status.code(), Some(1));
    assert!(
        stderr(&output).contains("newer ReFHIR"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn postgres_status_reports_the_recorded_versions() {
    let Some(admin) = common::admin_url("refhir migrate on postgres") else {
        return;
    };
    let database = common::fresh_database(&admin, "migratecli");
    let before = migrate(&["--status", "--postgres", &database.url]);
    assert!(before.status.success(), "{}", stderr(&before));
    assert!(
        stdout(&before).contains("new database"),
        "{}",
        stdout(&before)
    );
    // The password in the URL is not echoed.
    assert!(!stdout(&before).contains(&database.url));

    drop(refhir_postgres::PostgresPersistence::connect(&database.url).unwrap());
    let after = migrate(&["--status", "--postgres", &database.url]);
    assert!(after.status.success(), "{}", stderr(&after));
    assert!(
        stdout(&after).contains("up to date at version 1"),
        "{}",
        stdout(&after)
    );
}
