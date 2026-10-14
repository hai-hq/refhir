//! What the Postgres integration tests share: a database of its own per test.
//!
//! The writer lease is an advisory lock and those are per database, so every test that takes the
//! lease creates a database, and drops it when the test ends. These need `REFHIR_TEST_POSTGRES_URL`
//! (a role that may `CREATE DATABASE`); without it the test is skipped, unless
//! `REFHIR_REQUIRE_POSTGRES` is set, as it is in CI.

// Each test crate includes this module and uses part of it.
#![allow(dead_code)]

use std::sync::atomic::{AtomicU32, Ordering};

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
