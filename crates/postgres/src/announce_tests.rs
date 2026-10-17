//! Announcing publishes at most once per interval, against a real Postgres
//! (`REFHIR_TEST_POSTGRES_URL`).
//!
//! What is checked is what a replica sees: a listener on the channel counts the announcements and
//! when they arrive, while the primary publishes. The watermark must be exact whatever is
//! announced, the first announcement after a quiet spell must not wait, and a burst must end with
//! one that comes after its last publish, or a replica would sit on a stale watermark until its
//! timer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use postgres::fallible_iterator::FallibleIterator;
use refhir_persistence::{Persistence, PersistenceGlobalKey};
use refhir_types::Timestamp;

use crate::tenancy_tests::{client, shared};
use crate::tests::{fresh_database, test_url};
use crate::{ConnectOptions, PostgresPersistence};

/// A connection that listens on `channel` and records when each announcement arrives.
struct Listener {
    heard: Arc<Mutex<Vec<Instant>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Listener {
    fn start(url: &str, channel: &str) -> Self {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let mut conn = client(url);
        conn.batch_execute(&format!("LISTEN {channel}"))
            .expect("listen");
        let (seen, halt) = (heard.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !halt.load(Ordering::SeqCst) {
                let mut notifications = conn.notifications();
                let mut iter = notifications.timeout_iter(Duration::from_millis(20));
                match iter.next() {
                    Ok(Some(_)) => seen.lock().unwrap().push(Instant::now()),
                    Ok(None) => {}
                    Err(_) => return,
                }
            }
        });
        Self {
            heard,
            stop,
            thread: Some(thread),
        }
    }

    fn times(&self) -> Vec<Instant> {
        self.heard.lock().unwrap().clone()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn ts(n: u64) -> Timestamp {
    Timestamp::from_u64(n).expect("timestamp")
}

fn open_classic(url: &str, interval: Duration) -> PostgresPersistence {
    PostgresPersistence::connect_with(
        url,
        &ConnectOptions {
            pool_size: 2,
            announce_interval: Some(interval),
            ..ConnectOptions::default()
        },
    )
    .expect("open")
}

fn open_tenant(url: &str, tenant: &str, interval: Option<Duration>) -> PostgresPersistence {
    PostgresPersistence::connect_with(
        url,
        &ConnectOptions {
            pool_size: 2,
            tenant: Some(tenant.to_string()),
            announce_interval: interval,
            ..ConnectOptions::default()
        },
    )
    .unwrap_or_else(|err| panic!("open tenant {tenant}: {err}"))
}

/// The recorded watermark of the classic layout.
fn watermark(url: &str) -> u64 {
    let text: String = client(url)
        .query_one(
            "SELECT json_value FROM persistence_globals WHERE key = $1",
            &[&PersistenceGlobalKey::PublishedTimestamp.as_str()],
        )
        .expect("watermark")
        .get(0);
    text.trim().parse().expect("number")
}

fn settle(how_long: Duration) {
    std::thread::sleep(how_long);
}

/// Waits until `listener` has heard `count` announcements, for at most `within`.
fn hear(listener: &Listener, count: usize, within: Duration) {
    let deadline = Instant::now() + within;
    while listener.times().len() < count && Instant::now() < deadline {
        settle(Duration::from_millis(20));
    }
}

#[test]
fn the_first_publish_after_a_quiet_spell_is_announced_at_once() {
    let Some(admin) = test_url("announcements") else {
        return;
    };
    let url = fresh_database(&admin, "announce_first");
    let store = open_classic(&url, Duration::from_millis(500));
    let listener = Listener::start(&url, "refhir_published");
    settle(Duration::from_millis(100));

    let published = Instant::now();
    store.advance_published_ts(ts(1)).unwrap();
    settle(Duration::from_millis(150));
    let heard = listener.times();
    assert_eq!(heard.len(), 1, "one announcement for one publish");
    assert!(
        heard[0].duration_since(published) < Duration::from_millis(150),
        "the first announcement waited for the interval"
    );
    // Nothing more is owed, so nothing more comes.
    settle(Duration::from_millis(900));
    assert_eq!(
        listener.times().len(),
        1,
        "an announcement came from nowhere"
    );
    assert_eq!(watermark(&url), 1);
}

#[test]
fn a_burst_is_announced_a_few_times_and_ends_with_an_announcement_after_its_last_publish() {
    let Some(admin) = test_url("announcements") else {
        return;
    };
    let url = fresh_database(&admin, "announce_burst");
    let interval = Duration::from_millis(100);
    let store = open_classic(&url, interval);
    let listener = Listener::start(&url, "refhir_published");
    settle(Duration::from_millis(100));

    let started = Instant::now();
    for n in 1..=300 {
        store.advance_published_ts(ts(n)).unwrap();
    }
    let finished = Instant::now();
    settle(Duration::from_millis(600));

    let heard = listener.times();
    let burst = finished.duration_since(started);
    let most = (burst.as_millis() / interval.as_millis()) as usize + 2;
    assert!(
        heard.len() <= most,
        "{} announcements in a burst of {burst:?}: more than one per interval ({most} allowed)",
        heard.len()
    );
    assert!(heard.len() >= 2, "a leading and a trailing announcement");
    assert!(
        *heard.last().unwrap() > finished,
        "the burst ended without an announcement after its last publish: a replica would not \
         learn of it until its timer"
    );
    assert_eq!(
        watermark(&url),
        300,
        "the watermark is exact whatever is announced"
    );
    // Announcements are about an interval apart: a listener that looks every 20 ms sees each one
    // up to that much late, so this allows half an interval of slack and still fails on a storm.
    for pair in heard.windows(2) {
        assert!(
            pair[1].duration_since(pair[0]) >= interval / 2,
            "two announcements {:?} apart",
            pair[1].duration_since(pair[0])
        );
    }
    // And then it is quiet.
    let count = heard.len();
    settle(Duration::from_millis(500));
    assert_eq!(listener.times().len(), count, "announcements kept coming");
}

#[test]
fn a_zero_interval_announces_every_publish() {
    let Some(admin) = test_url("announcements") else {
        return;
    };
    let url = fresh_database(&admin, "announce_every");
    let store = open_classic(&url, Duration::ZERO);
    let listener = Listener::start(&url, "refhir_published");
    settle(Duration::from_millis(100));
    for n in 1..=25 {
        store.advance_published_ts(ts(n)).unwrap();
    }
    settle(Duration::from_millis(300));
    assert_eq!(listener.times().len(), 25);
}

#[test]
fn a_classic_database_announces_every_publish_unless_told_otherwise() {
    let Some(admin) = test_url("announcements") else {
        return;
    };
    let url = fresh_database(&admin, "announce_classic");
    let store = PostgresPersistence::connect(&url).unwrap();
    let listener = Listener::start(&url, "refhir_published");
    settle(Duration::from_millis(100));
    for n in 1..=25 {
        store.advance_published_ts(ts(n)).unwrap();
    }
    settle(Duration::from_millis(300));
    assert_eq!(listener.times().len(), 25);
}

#[test]
fn a_tenants_burst_wakes_only_its_own_replicas_and_is_rationed_by_default() {
    let Some(admin) = test_url("announcements") else {
        return;
    };
    let url = shared(&admin, "announce_tenants", &["a", "b"]);
    let (a, b) = (
        crate::tenancy_tests::tenant_id(&url, "a"),
        crate::tenancy_tests::tenant_id(&url, "b"),
    );
    // The default interval for a tenant: no interval given.
    let store = open_tenant(&url, "a", None);
    let mine = Listener::start(&url, &format!("refhir_published_{a}"));
    let theirs = Listener::start(&url, &format!("refhir_published_{b}"));
    settle(Duration::from_millis(100));

    let started = Instant::now();
    for n in 1..=400 {
        store.advance_published_ts(ts(n)).unwrap();
    }
    let finished = Instant::now();
    settle(Duration::from_millis(300));

    let heard = mine.times();
    let burst = finished.duration_since(started);
    assert!(
        heard.len() as u128 <= burst.as_millis() / crate::TENANT_ANNOUNCE_INTERVAL.as_millis() + 2,
        "{} announcements in {burst:?}",
        heard.len()
    );
    assert!(heard.len() >= 2);
    assert!(*heard.last().unwrap() > finished);
    assert!(
        theirs.times().is_empty(),
        "another tenant's replicas were woken"
    );
}

#[test]
fn the_announcement_thread_ends_with_the_persistence() {
    let Some(admin) = test_url("announcements") else {
        return;
    };
    let url = fresh_database(&admin, "announce_drop");
    let store = open_classic(&url, Duration::from_millis(50));
    for n in 1..=50 {
        store.advance_published_ts(ts(n)).unwrap();
    }
    let weak = Arc::downgrade(&store.announcer);
    drop(store);
    let deadline = Instant::now() + Duration::from_secs(3);
    while weak.upgrade().is_some() {
        assert!(
            Instant::now() < deadline,
            "the announcement thread still holds the announcer"
        );
        settle(Duration::from_millis(50));
    }
}

#[test]
fn an_announcement_that_could_not_be_sent_is_sent_when_the_connection_is_back() {
    let Some(admin) = test_url("announcements") else {
        return;
    };
    let url = fresh_database(&admin, "announce_reconnect");
    let store = open_classic(&url, Duration::from_millis(50));
    let listener = Listener::start(&url, "refhir_published");
    settle(Duration::from_millis(100));

    // A burst, so that the announcer's thread has connected and used its connection.
    store.advance_published_ts(ts(1)).unwrap();
    store.advance_published_ts(ts(2)).unwrap();
    hear(&listener, 2, Duration::from_secs(5));
    settle(Duration::from_millis(100));
    assert_eq!(
        listener.times().len(),
        2,
        "a leading and a trailing announcement"
    );

    // The connection goes away without the thread being told, as a link a NAT forgot would.
    let ended: i64 = client(&url)
        .query_one(
            "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity \
             WHERE datname = current_database() AND pid <> pg_backend_pid() \
               AND query = 'NOTIFY refhir_published'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(ended, 1, "the announcer's connection");
    settle(Duration::from_millis(200));

    // The next burst's tail is still announced: the failed send is tried again on a new
    // connection and not dropped.
    store.advance_published_ts(ts(3)).unwrap();
    store.advance_published_ts(ts(4)).unwrap();
    let published = Instant::now();
    hear(&listener, 4, Duration::from_secs(5));
    let heard = listener.times();
    assert_eq!(
        heard.len(),
        4,
        "the announcement was lost with the connection"
    );
    assert!(*heard.last().unwrap() > published);
    assert_eq!(watermark(&url), 4);
}

#[test]
fn an_announcer_that_cannot_connect_keeps_the_announcement_owed_and_stops_with_its_owner() {
    // Nothing listens on port 1: the thread cannot connect, and must neither lose what is owed
    // nor outlive the announcer.
    let announcer = crate::announce::Announcer::new(
        Duration::from_millis(20),
        "refhir_published".into(),
        Arc::from("host=127.0.0.1 port=1 user=nobody dbname=nothing connect_timeout=1"),
    );
    announcer.owe();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut owed = false;
    while !owed && Instant::now() < deadline {
        owed = announcer.is_owed();
        settle(Duration::from_millis(5));
    }
    assert!(owed, "an announcement that cannot be sent was forgotten");
    let weak = Arc::downgrade(&announcer);
    drop(announcer);
    let deadline = Instant::now() + Duration::from_secs(8);
    while weak.upgrade().is_some() {
        assert!(
            Instant::now() < deadline,
            "the thread still holds the announcer"
        );
        settle(Duration::from_millis(50));
    }
}

/// Not run with the suite: `cargo test --release -p refhir-postgres announcing_less -- --ignored
/// --nocapture`.
///
/// Eight tenants of one database publish as fast as they can for a few seconds while each has
/// replicas listening, once announcing every publish and once announcing at most every interval.
/// The publishes are the tiny transactions a primary makes, so what is measured is the cost of the
/// announcements: the cluster-wide lock the committing transaction takes, and the wake-up of every
/// listening connection of the database, whichever channel it listens on.
#[test]
#[ignore = "benchmark"]
fn announcing_less_often_lets_many_tenants_publish_faster() {
    let Some(admin) = test_url("announcement benchmark") else {
        return;
    };
    let replicas: usize = std::env::var("BENCH_REPLICAS")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(3);
    let names: Vec<String> = (0..8).map(|n| format!("t{n}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let url = shared(&admin, "announce_bench", &refs);
    for (label, interval) in [
        ("every publish", Duration::ZERO),
        ("at most every 20 ms", crate::TENANT_ANNOUNCE_INTERVAL),
        ("at most every 100 ms", Duration::from_millis(100)),
        ("every publish", Duration::ZERO),
        ("at most every 20 ms", crate::TENANT_ANNOUNCE_INTERVAL),
        ("at most every 100 ms", Duration::from_millis(100)),
    ] {
        let stores: Vec<_> = names
            .iter()
            .map(|name| open_tenant(&url, name, Some(interval)))
            .collect();
        let listeners: Vec<Listener> = names
            .iter()
            .flat_map(|name| {
                let id = crate::tenancy_tests::tenant_id(&url, name);
                (0..replicas).map(move |_| id)
            })
            .map(|id| Listener::start(&url, &format!("refhir_published_{id}")))
            .collect();
        let stop = Arc::new(AtomicBool::new(false));
        let handles: Vec<_> = stores
            .into_iter()
            .map(|store| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut n = 0u64;
                    while !stop.load(Ordering::SeqCst) {
                        n += 1;
                        store.advance_published_ts(ts(n)).unwrap();
                    }
                    n
                })
            })
            .collect();
        let started = Instant::now();
        std::thread::sleep(Duration::from_secs(5));
        stop.store(true, Ordering::SeqCst);
        let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        let seconds = started.elapsed().as_secs_f64();
        let heard: usize = listeners.iter().map(|l| l.times().len()).sum();
        println!(
            "8 tenants x {replicas} replicas, announcing {label}: {:.0} publishes/s, \
             {:.0} announcements/s heard by each replica",
            total as f64 / seconds,
            heard as f64 / listeners.len() as f64 / seconds
        );
    }
}
