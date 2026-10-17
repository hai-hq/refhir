//! What the Postgres integration tests share: a database of its own per test.
//!
//! The writer lease is an advisory lock and those are per database, so every test that takes the
//! lease creates a database, and drops it when the test ends. These need `REFHIR_TEST_POSTGRES_URL`
//! (a role that may `CREATE DATABASE`); without it the test is skipped, unless
//! `REFHIR_REQUIRE_POSTGRES` is set, as it is in CI.
//!
//! It also holds what the tests that run the `refhir` binary share: a scratch directory, starting
//! the binary with a clean environment, and waiting for it to exit or to be ready.

// Each test crate includes this module and uses part of it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Not a connection string, so the server and `migrate` refuse it at once, with no network.
pub const UNUSABLE_POSTGRES: &str = "not a url";

/// A directory of its own, removed when the test ends.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("refhir-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Starts `refhir` in `dir`. Nothing is inherited from whoever runs the tests that could change
/// what it does (`REFHIR_*`, `RUST_LOG`), and no packages are downloaded.
pub fn start(
    dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    stdout: Stdio,
    stderr: Stdio,
) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_refhir"));
    for (name, _) in std::env::vars_os() {
        let text = name.to_string_lossy();
        if text.starts_with("REFHIR_") || text == "RUST_LOG" {
            command.env_remove(&name);
        }
    }
    command
        .current_dir(dir)
        .args(args)
        .env("REFHIR_SKIP_DEFAULT_PACKAGES", "1")
        .env("REFHIR_PACKAGE_CACHE", dir.join("packages"))
        .envs(env.iter().copied())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .expect("start refhir")
}

/// Runs `refhir` until it exits. A command line the server accepts never exits, so the wait is
/// bounded, and a server that outlives it fails the test.
pub fn exits(dir: &Path, args: &[&str]) -> Output {
    let mut child = start(dir, args, &[], Stdio::piped(), Stdio::piped());
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("`refhir {}` was still running", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().unwrap()
}

/// Runs the server until it answers `/readyz`, then stops it. Its log goes to a file that a
/// failure prints, so a chatty server cannot fill a pipe nobody is reading.
pub fn serves(dir: &Path, args: &[&str], env: &[(&str, &str)]) {
    let bind = format!("127.0.0.1:{}", free_port());
    let mut with_bind = args.to_vec();
    with_bind.extend(["--bind", &bind]);
    let log = dir.join("serve.log");
    let file = std::fs::File::create(&log).unwrap();
    let mut child = start(
        dir,
        &with_bind,
        env,
        Stdio::from(file.try_clone().unwrap()),
        Stdio::from(file),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let outcome = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Err(format!("exited ({status}) before it was ready"));
        }
        if ureq::get(&format!("http://{bind}/readyz"))
            .timeout(Duration::from_secs(2))
            .call()
            .is_ok()
        {
            break Ok(());
        }
        if Instant::now() > deadline {
            break Err("was not ready in 30s".to_string());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = child.kill();
    let _ = child.wait();
    if let Err(problem) = outcome {
        panic!(
            "`refhir {}` {problem}:\n{}",
            with_bind.join(" "),
            std::fs::read_to_string(&log).unwrap_or_default()
        );
    }
}

pub fn admin_url(what: &str) -> Option<String> {
    match std::env::var("REFHIR_TEST_POSTGRES_URL") {
        Ok(url) => Some(url),
        Err(_) if std::env::var("REFHIR_REQUIRE_POSTGRES").is_ok() => {
            panic!("REFHIR_REQUIRE_POSTGRES is set but REFHIR_TEST_POSTGRES_URL is not ({what})")
        }
        Err(_) => {
            eprintln!("REFHIR_TEST_POSTGRES_URL unset, skipping {what}");
            None
        }
    }
}

/// A database created for one test, dropped when the test ends (declare it first, so the
/// processes using it are stopped before it goes).
pub struct TestDatabase {
    admin: String,
    name: String,
    pub url: String,
}

impl std::ops::Deref for TestDatabase {
    type Target = str;
    fn deref(&self) -> &str {
        &self.url
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        if let Ok(mut client) = postgres::Client::connect(&self.admin, postgres::NoTls) {
            let _ = client.batch_execute(&format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                self.name
            ));
        }
    }
}

pub fn fresh_database(admin: &str, tag: &str) -> TestDatabase {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "refhir_t_{tag}_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let mut client = postgres::Client::connect(admin, postgres::NoTls).expect("admin connection");
    client
        .batch_execute(&format!("CREATE DATABASE {name}"))
        .expect("create database");
    TestDatabase {
        admin: admin.to_string(),
        url: with_database(admin, &name),
        name,
    }
}

/// `admin` with its database replaced by `name`. Handles `postgres://...` URLs and the
/// `host=... user=...` key/value form CI uses.
pub fn with_database(admin: &str, name: &str) -> String {
    if admin.contains("://") {
        let (head, tail) = match admin.split_once('?') {
            Some((head, query)) => (head, format!("?{query}")),
            None => (admin, String::new()),
        };
        let slash = head.rfind('/').expect("database name in the url");
        format!("{}/{name}{tail}", &head[..slash])
    } else {
        let kept: Vec<&str> = admin
            .split_whitespace()
            .filter(|part| !part.starts_with("dbname="))
            .collect();
        format!("{} dbname={name}", kept.join(" "))
    }
}

/// `url` with another user and password. Handles `postgres://...` URLs and the key/value form.
pub fn with_role(url: &str, user: &str, password: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, tail) = rest.split_once('/').unwrap_or((rest, ""));
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        format!("{scheme}://{user}:{password}@{host}/{tail}")
    } else {
        let kept: Vec<&str> = url
            .split_whitespace()
            .filter(|part| !part.starts_with("user=") && !part.starts_with("password="))
            .collect();
        format!("{} user={user} password={password}", kept.join(" "))
    }
}

/// A database owned by a role that is not a superuser, as the application's own role is of its
/// database, and the URL to connect to it as that role. Row-level security binds that role.
pub fn database_of_an_ordinary_role(admin: &str, tag: &str) -> (TestDatabase, String) {
    postgres::Client::connect(admin, postgres::NoTls)
        .expect("admin connection")
        .batch_execute(
            "DO $r$ BEGIN \
               CREATE ROLE refhir_rls_app LOGIN PASSWORD 'refhir-rls-app' NOSUPERUSER NOBYPASSRLS; \
             EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $r$",
        )
        .expect("create the application role (the test role needs CREATEROLE)");
    let database = fresh_database(admin, tag);
    postgres::Client::connect(&database.url, postgres::NoTls)
        .expect("connect")
        .batch_execute(&format!(
            "ALTER DATABASE {} OWNER TO refhir_rls_app",
            database.name
        ))
        .expect("give the database to the application's role");
    let url = with_role(&database.url, "refhir_rls_app", "refhir-rls-app");
    (database, url)
}
