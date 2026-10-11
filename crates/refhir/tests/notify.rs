//! A replica wakes when the writer publishes, instead of waiting for its timer.
//!
//! Needs `HAIFHIR_POSTGRES_URL` (a role that may `CREATE DATABASE`); skipped without it unless
//! `REFHIR_REQUIRE_POSTGRES` is set, as in CI.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use refhir_database::{Database, Put};
use refhir_persistence::{Lease, Persistence};
use refhir_postgres::{ConnectOptions, PostgresPersistence};
use refhir_types::{exact_key, IndexId, TabletId};
use serde_json::json;

fn admin_url() -> Option<String> {
    match std::env::var("HAIFHIR_POSTGRES_URL") {
        Ok(url) => Some(url),
        Err(_) if std::env::var("REFHIR_REQUIRE_POSTGRES").is_ok() => {
            panic!("REFHIR_REQUIRE_POSTGRES is set but HAIFHIR_POSTGRES_URL is not")
        }
        Err(_) => {
            eprintln!("HAIFHIR_POSTGRES_URL unset, skipping the notification test");
            None
        }
    }
}

struct TestDatabase {
    admin: String,
    name: String,
    url: String,
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

fn fresh_database(admin: &str) -> TestDatabase {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "refhir_n_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let mut client = postgres::Client::connect(admin, postgres::NoTls).expect("admin connection");
    client
        .batch_execute(&format!("CREATE DATABASE {name}"))
        .expect("create database");
    let url = if admin.contains("://") {
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
    };
    TestDatabase {
        admin: admin.to_string(),
        name,
        url,
    }
}

fn put(db: &Database, id: &str) -> refhir_types::Timestamp {
    let tablet = TabletId::new("t");
    let key = (IndexId::new("t/by_id"), exact_key(id));
    let mut tx = db.begin();
    tx.stage_put(Put {
        tablet,
        id: id.into(),
        json: json!({"id": id}),
        new_keys: vec![key],
        old_keys: vec![],
        prev_ts: None,
    })
    .unwrap();
    db.commit(tx).unwrap()
}

#[test]
fn a_reader_wakes_when_the_writer_publishes_not_when_its_timer_fires() {
    let Some(admin) = admin_url() else {
        return;
    };
    let database = fresh_database(&admin);
    let writer_store = Arc::new(PostgresPersistence::connect(&database.url).unwrap());
    assert!(matches!(
        writer_store.try_acquire_writer_lease().unwrap(),
        Lease::Held { .. }
    ));
    let writer = Database::open(writer_store).unwrap();

    let options = ConnectOptions {
        pool_size: 2,
        create_schema: false,
    };
    let reader_store =
        Arc::new(PostgresPersistence::connect_with(&database.url, &options).unwrap());
    assert!(reader_store.publishes_notifications());
    let reader = Arc::new(Database::open_read_only(reader_store).unwrap());
    // A timer far longer than the assertion: only a notification can explain catching up in time.
    reader.start_refresher(Duration::from_secs(30));
    std::thread::sleep(Duration::from_millis(800)); // the listener is registered by now

    let ts = put(&writer, "a");
    let started = Instant::now();
    assert!(
        reader.wait_for_ts(ts, Duration::from_secs(5)),
        "the reader never caught up"
    );
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_secs(3),
        "caught up after {waited:?}: that is the timer, not the notification"
    );
    println!("reader caught up {waited:?} after the commit, with a 30 s timer");

    // And again, to show it is not a one-off: each publish wakes it.
    let second = put(&writer, "b");
    let started = Instant::now();
    assert!(reader.wait_for_ts(second, Duration::from_secs(5)));
    assert!(started.elapsed() < Duration::from_secs(3));
}
