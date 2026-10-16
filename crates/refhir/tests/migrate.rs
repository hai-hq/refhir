//! `refhir migrate`, and the server refusing a database it cannot use, through the real binary.
//!
//! The SQLite cases need nothing. The Postgres one needs `REFHIR_TEST_POSTGRES_URL` (a role that
//! may `CREATE DATABASE`) and is skipped without it, unless `REFHIR_REQUIRE_POSTGRES` is set.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
    assert!(text.contains("up to date at version 2"), "{text}");
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
        "INSERT INTO schema_migrations (version, checksum) VALUES (3, 'future')",
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
    // Not the exit code that means "this build would refuse to start".
    let output = migrate(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("--status or --dry-run"));
    let both = migrate(&["--status", "--dry-run"]);
    assert_eq!(both.status.code(), Some(2));
}

/// The flags used to be applied in a fixed order (`--sqlite`, then `--postgres`), each clearing the
/// other, so `--postgres` won whatever order the command line gave.
#[test]
fn naming_more_than_one_database_is_refused_in_any_order() {
    let dir = common::Scratch::new("migrate-both");
    let file = dir.join("refhir.sqlite");
    let file = file.to_str().unwrap();
    let url = common::UNUSABLE_POSTGRES;
    for args in [
        ["--status", "--sqlite", file, "--postgres", url],
        ["--status", "--postgres", url, "--sqlite", file],
        ["--status", "--sqlite", file, "--sqlite", file],
        ["--status", "--postgres", url, "--postgres", url],
    ] {
        let output = migrate(&args);
        // The exit code for arguments that were wrong.
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr(&output)
        );
        assert_eq!(
            stderr(&output),
            "refhir: pass only one of --sqlite or --postgres\n",
            "{args:?}"
        );
        assert!(stdout(&output).is_empty(), "{args:?}: {}", stdout(&output));
    }
}

/// A database flag followed by another flag has no value, and was read as the value instead.
#[test]
fn a_database_flag_does_not_take_the_next_flag_as_its_value() {
    for (args, said) in [
        (
            ["--status", "--sqlite", "--postgres"],
            "refhir: --sqlite needs a value, not the flag --postgres\n",
        ),
        (
            ["--status", "--postgres", "--sqlite"],
            "refhir: --postgres needs a value, not the flag --sqlite\n",
        ),
    ] {
        let output = migrate(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(stderr(&output), said, "{args:?}");
    }
}

/// "CLI flags override the file", and the environment: one flag replaces the database the
/// settings name, and is not a conflict with it.
#[test]
fn one_database_flag_still_overrides_the_settings() {
    let dir = common::Scratch::new("migrate-override");
    let from_file = dir.join("from-file.sqlite");
    let from_flag = dir.join("from-flag.sqlite");
    let config = dir.join("settings.toml");
    std::fs::write(
        &config,
        format!(
            "[database]\nkind = \"sqlite\"\nsqlite_path = '{}'\n",
            from_file.display()
        ),
    )
    .unwrap();
    let config = config.to_str().unwrap();

    let output = migrate(&[
        "--status",
        "--config",
        config,
        "--sqlite",
        from_flag.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with(&format!("database: sqlite {}\n", from_flag.display())),
        "{}",
        stdout(&output)
    );

    // The file's SQLite database is replaced by Postgres, which here is refused at once.
    let output = migrate(&[
        "--status",
        "--config",
        config,
        "--postgres",
        common::UNUSABLE_POSTGRES,
    ]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        !stderr(&output).contains("pass only one"),
        "{}",
        stderr(&output)
    );
    assert!(
        !stdout(&output).contains("database: sqlite"),
        "{}",
        stdout(&output)
    );

    // The environment's Postgres is replaced by SQLite.
    let output = Command::new(env!("CARGO_BIN_EXE_refhir"))
        .args([
            "migrate",
            "--status",
            "--sqlite",
            from_flag.to_str().unwrap(),
        ])
        .env("REFHIR_POSTGRES_URL", common::UNUSABLE_POSTGRES)
        .env_remove("REFHIR_CONFIG")
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with(&format!("database: sqlite {}\n", from_flag.display())),
        "{}",
        stdout(&output)
    );
}

/// The server, not just the report, refuses: it exits without serving.
#[test]
fn the_server_refuses_to_start_on_a_database_from_a_newer_version() {
    let path = temp_db("newer-serve");
    drop(refhir_sqlite::SqlitePersistence::open(&path).unwrap());
    sqlite(
        &path,
        "INSERT INTO schema_migrations (version, checksum) VALUES (3, 'future')",
    );
    let dir = path.parent().unwrap();
    let output = common::exits(
        dir,
        &["--sqlite", path.to_str().unwrap(), "--bind", "127.0.0.1:0"],
    );
    assert_eq!(output.status.code(), Some(1));
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
        stdout(&after).contains("up to date at version 2"),
        "{}",
        stdout(&after)
    );
}

/// Settings `migrate` does not use must not stop it from looking at a database.
#[test]
fn unrelated_bad_settings_do_not_stop_a_status_check() {
    let path = temp_db("unrelated");
    drop(refhir_sqlite::SqlitePersistence::open(&path).unwrap());
    let output = Command::new(env!("CARGO_BIN_EXE_refhir"))
        .args(["migrate", "--status", "--sqlite", path.to_str().unwrap()])
        .env("REFHIR_DB_POOL", "0")
        .env("REFHIR_ROLE", "nonsense")
        .env_remove("REFHIR_CONFIG")
        .env_remove("REFHIR_POSTGRES_URL")
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("up to date"),
        "{}",
        stdout(&output)
    );
}
