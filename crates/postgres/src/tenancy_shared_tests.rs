//! Many tenants served through one process's [`SharedConnections`], against a real Postgres
//! (`REFHIR_TEST_POSTGRES_URL`): the shared pool, the shared lease session, the shared listener,
//! the registry the admin API changes, and upgrading a database that holds tenants.

use std::sync::Arc;
use std::time::Duration;

use refhir_persistence::{
    ConflictStrategy, DocumentLogEntry, Lease, Persistence, PersistenceReader,
};
use refhir_types::{DocumentId, TabletId, Timestamp};
use serde_json::json;

use crate::tenancy_tests::{
    app_url, check_twins, client, digests, operator_url, poison, shared, suite,
};
use crate::tests::{fresh_database, test_url};
use crate::{Created, PostgresPersistence, SharedConnections, SharedOptions};

fn connections(url: &str, pool_size: usize) -> Arc<SharedConnections> {
    SharedConnections::connect(
        url,
        &SharedOptions {
            pool_size,
            create_schema: true,
        },
    )
    .expect("shared connections")
}

fn doc(id: &str, ts: u64, text: &str) -> DocumentLogEntry {
    DocumentLogEntry {
        ts: Timestamp::from_u64(ts).unwrap(),
        id: DocumentId::new(TabletId::new("Patient"), id),
        value: Some(text.to_string()),
        prev_ts: None,
    }
}

fn read_all(p: &PostgresPersistence) -> Vec<String> {
    p.load_documents(
        refhir_persistence::TimestampRange {
            start_inclusive: Timestamp::ZERO,
            end_exclusive: Timestamp::from_u64(1 << 40).unwrap(),
        },
        refhir_types::Order::Asc,
    )
    .unwrap()
    .into_iter()
    .filter_map(|row| row.value)
    .collect()
}

/// One connection serves two tenants in turn, so every checkout switches the tenant it serves.
/// Under row-level security (the application's role), a switch that did not happen would show
/// the other tenant nothing, or refuse its write; with the superuser, the statements' own tenant
/// would have to be right. The store suite runs beside a tenant that twins every row, and the
/// twins come out untouched.
#[test]
fn one_pool_serves_tenants_in_turn_and_each_keeps_to_its_own_rows() {
    let Some(admin) = test_url("tenancy: shared pool") else {
        return;
    };
    let url = shared(&admin, "tshpool", &["a", "b", "c", "poison"]);
    poison(&url, "poison");
    let shared = connections(&app_url(&url), 1);
    let a = PostgresPersistence::open_tenant(&shared, "a", 1).unwrap();
    let b = PostgresPersistence::open_tenant(&shared, "b", 1).unwrap();
    // The store suite wants a tenant that holds nothing yet.
    let c = PostgresPersistence::open_tenant(&shared, "c", 1).unwrap();
    assert!(a.row_security_applies());

    for round in 0..3u64 {
        a.write(&[doc("p1", 10 + round, "a")], &[], ConflictStrategy::Error)
            .unwrap();
        b.write(&[doc("p1", 10 + round, "b")], &[], ConflictStrategy::Error)
            .unwrap();
    }
    assert_eq!(read_all(&a), ["a", "a", "a"]);
    assert_eq!(read_all(&b), ["b", "b", "b"]);
    let (before_a, before_b) = (digests(&url, "a"), digests(&url, "b"));
    suite(&c);
    assert_eq!(digests(&url, "a"), before_a, "the suite on c changed a");
    assert_eq!(digests(&url, "b"), before_b, "the suite on c changed b");
    assert_eq!(read_all(&a), ["a", "a", "a"]);
    assert!(
        check_twins(&url, "poison") > 0,
        "the poison tenant made no twins"
    );
    // The pool never opened a second connection to do it.
    assert_eq!(shared.stats().open, 1);
}

/// A tenant may hold its share of the pool and no more, so one busy tenant cannot keep the others
/// waiting for every connection.
#[test]
fn a_tenant_holds_no_more_than_its_share_of_the_pool() {
    let Some(admin) = test_url("tenancy: pool share") else {
        return;
    };
    let url = shared(&admin, "tshshare", &["busy", "other"]);
    let shared = connections(&operator_url(&url), 3);
    let busy = PostgresPersistence::open_tenant(&shared, "busy", 1).unwrap();
    let other = PostgresPersistence::open_tenant(&shared, "other", 2).unwrap();
    // `busy` holds its one connection until told to let go, from another thread.
    let (holding, held) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let holder = {
        let busy = busy.clone();
        std::thread::spawn(move || {
            let guard = busy.inner.lock().unwrap();
            holding.send(()).unwrap();
            released.recv().unwrap();
            drop(guard);
        })
    };
    held.recv().unwrap();
    // `other` is served while `busy` holds its share: from the connections `busy` may not take.
    other
        .write(&[doc("quick", 5, "y")], &[], ConflictStrategy::Error)
        .unwrap();
    // A second request of `busy` waits for its own first one, though the pool has room.
    let (done, finished) = std::sync::mpsc::channel();
    let waiter = {
        let busy = busy.clone();
        std::thread::spawn(move || {
            busy.write(&[doc("later", 6, "z")], &[], ConflictStrategy::Error)
                .unwrap();
            done.send(()).unwrap();
        })
    };
    assert!(
        finished.recv_timeout(Duration::from_millis(500)).is_err(),
        "busy took more than its share of the pool"
    );
    release.send(()).unwrap();
    finished
        .recv_timeout(Duration::from_secs(30))
        .expect("busy never got its connection back");
    waiter.join().unwrap();
    holder.join().unwrap();
    let stats = busy.pool_stats().unwrap();
    assert_eq!((stats.max, stats.peak_in_use), (1, 1));
    assert!(stats.waits >= 1);
}

/// Every tenant's lease is held on one session. Another process (another `SharedConnections`)
/// finds each busy, takes one once it is given up, and a tenant served by a process of its own
/// is never confused with them.
#[test]
fn one_session_holds_every_tenants_lease() {
    let Some(admin) = test_url("tenancy: lease session") else {
        return;
    };
    let url = shared(&admin, "tshlease", &["a", "b"]);
    let first = connections(&operator_url(&url), 2);
    let second = connections(&operator_url(&url), 2);
    let a1 = PostgresPersistence::open_tenant(&first, "a", 1).unwrap();
    let b1 = PostgresPersistence::open_tenant(&first, "b", 1).unwrap();
    assert!(matches!(
        a1.try_acquire_writer_lease().unwrap(),
        Lease::Held { .. }
    ));
    assert!(matches!(
        b1.try_acquire_writer_lease().unwrap(),
        Lease::Held { .. }
    ));
    assert_eq!(first.lease.held().len(), 2);

    let a2 = PostgresPersistence::open_tenant(&second, "a", 1).unwrap();
    let b2 = PostgresPersistence::open_tenant(&second, "b", 1).unwrap();
    assert!(matches!(
        a2.try_acquire_writer_lease().unwrap(),
        Lease::Busy
    ));
    assert!(matches!(
        b2.try_acquire_writer_lease().unwrap(),
        Lease::Busy
    ));
    // A process started for one tenant alone sees the lease taken too.
    let alone = crate::tenancy_tests::open(&url, "a");
    assert!(matches!(
        alone.try_acquire_writer_lease().unwrap(),
        Lease::Busy
    ));
    let listed = crate::list_tenants(&operator_url(&url)).unwrap();
    assert!(listed.iter().all(|tenant| tenant.primary_running));

    // Giving up `a` leaves `b` held, and lets the other process take `a`.
    a1.release_writer_lease();
    assert_eq!(first.lease.held().len(), 1);
    assert!(matches!(
        a2.try_acquire_writer_lease().unwrap(),
        Lease::Held { .. }
    ));
    assert!(matches!(
        b2.try_acquire_writer_lease().unwrap(),
        Lease::Busy
    ));
    // The writes of the process that gave `a` up are fenced.
    let fenced = a1.write(&[doc("late", 99, "x")], &[], ConflictStrategy::Error);
    assert!(
        matches!(fenced, Err(refhir_persistence::PersistError::Fenced(_))),
        "{fenced:?}"
    );
    b1.write(&[doc("still", 99, "x")], &[], ConflictStrategy::Error)
        .unwrap();
}

/// A tenant whose epoch another process moved (it took the tenant over) is lost alone: the
/// process is told, and goes on serving its other tenants.
#[test]
fn losing_one_tenants_lease_leaves_the_others() {
    let Some(admin) = test_url("tenancy: lease lost") else {
        return;
    };
    let url = shared(&admin, "tshlost", &["a", "b"]);
    let first = connections(&operator_url(&url), 2);
    let a = PostgresPersistence::open_tenant(&first, "a", 1).unwrap();
    let b = PostgresPersistence::open_tenant(&first, "b", 1).unwrap();
    let (told, heard) = std::sync::mpsc::channel();
    a.on_writer_lease_lost(move |reason| {
        let _ = told.send(reason.to_string());
    });
    assert!(matches!(
        a.try_acquire_writer_lease().unwrap(),
        Lease::Held { .. }
    ));
    assert!(matches!(
        b.try_acquire_writer_lease().unwrap(),
        Lease::Held { .. }
    ));
    let id = crate::tenancy_tests::tenant_id(&url, "a");
    client(&url)
        .execute(
            "UPDATE persistence_globals SET json_value = '999' \
             WHERE instance_id = $1 AND key = 'writer_epoch'",
            &[&id],
        )
        .unwrap();
    let reason = heard.recv_timeout(Duration::from_secs(10)).expect("told");
    assert!(reason.contains("999"), "{reason}");
    assert!(a.writer_lease_lost() && !b.writer_lease_lost());
    b.write(&[doc("on", 5, "x")], &[], ConflictStrategy::Error)
        .unwrap();
    assert_eq!(first.lease.held().len(), 1);
}

/// A replica of each tenant listens on the one connection the process shares, and hears only
/// its own tenant's publishes.
#[test]
fn one_listener_wakes_only_the_tenant_that_published() {
    let Some(admin) = test_url("tenancy: shared listener") else {
        return;
    };
    let url = shared(&admin, "tshlisten", &["a", "b"]);
    let writers = connections(&operator_url(&url), 2);
    let readers = connections(&operator_url(&url), 2);
    let a = PostgresPersistence::open_tenant(&writers, "a", 1).unwrap();
    let a_reader = PostgresPersistence::open_tenant(&readers, "a", 1).unwrap();
    let b_reader = PostgresPersistence::open_tenant(&readers, "b", 1).unwrap();
    // Settle: the hub listens, and wakes each subscriber once when it starts listening.
    a_reader.wait_for_publish(Duration::from_secs(2));
    b_reader.wait_for_publish(Duration::from_secs(2));
    while a_reader.wait_for_publish(Duration::from_millis(300)) {}
    while b_reader.wait_for_publish(Duration::from_millis(300)) {}

    assert!(matches!(
        a.try_acquire_writer_lease().unwrap(),
        Lease::Held { .. }
    ));
    a.advance_published_ts(Timestamp::from_u64(7).unwrap())
        .unwrap();
    assert!(
        a_reader.wait_for_publish(Duration::from_secs(5)),
        "a's reader heard nothing"
    );
    assert!(
        !b_reader.wait_for_publish(Duration::from_millis(500)),
        "b's reader heard a"
    );
}

/// The registry the admin API changes: creating is idempotent, a deleted tenant is not served and
/// can be restored until its grace period is over, and then its rows are removed, but never
/// while a process holds its lease.
#[test]
fn the_registry_creates_deletes_restores_and_purges() {
    let Some(admin) = test_url("tenancy: registry") else {
        return;
    };
    let url = shared(&admin, "tshreg", &[]);
    let operator = operator_url(&url);
    let shared = connections(&operator, 2);
    let watch = shared.watch_registry();
    let settings = json!({"smart": false, "packages": ["x@1"]});
    let created = shared.create_tenant("acme", &settings).unwrap();
    let Created::New(tenant) = created else {
        panic!("not new: {created:?}")
    };
    assert_eq!(tenant.settings, settings);
    assert!(
        watch.wait(Duration::from_secs(5)),
        "the create was not announced"
    );
    let signature = shared.registry_signature().unwrap();
    // Asking again finds it, with what it was created with.
    match shared.create_tenant("acme", &json!({"other": 1})).unwrap() {
        Created::Existing(found) => assert_eq!(found.settings, settings),
        other => panic!("{other:?}"),
    }
    assert_eq!(shared.registry_signature().unwrap(), signature);

    let acme = PostgresPersistence::open_tenant(&shared, "acme", 1).unwrap();
    assert!(matches!(
        acme.try_acquire_writer_lease().unwrap(),
        Lease::Held { .. }
    ));
    acme.write(&[doc("p", 3, "x")], &[], ConflictStrategy::Error)
        .unwrap();

    // Deleted: not opened again, and announced.
    let deleted = shared.delete_tenant("acme", 3600).unwrap().unwrap();
    assert!(deleted.is_deleted() && deleted.purge_after.is_some());
    assert!(watch.wait(Duration::from_secs(5)));
    assert_ne!(shared.registry_signature().unwrap(), signature);
    let refused = PostgresPersistence::open_tenant(&shared, "acme", 1);
    assert!(refused.is_err());
    // Deleting again keeps the time it was given.
    let again = shared.delete_tenant("acme", 1).unwrap().unwrap();
    assert_eq!(again.purge_after, deleted.purge_after);
    assert!(shared.tenants_due_for_purge().unwrap().is_empty());
    assert!(
        shared.purge_tenant("acme").is_err(),
        "purged inside its grace period"
    );

    // Restored, it is served again with what it held.
    let restored = shared.restore_tenant("acme").unwrap().unwrap();
    assert!(!restored.is_deleted());
    assert!(PostgresPersistence::open_tenant(&shared, "acme", 1).is_ok());

    // Past its grace period it is removed, but not while its lease is held.
    shared.delete_tenant("acme", 0).unwrap();
    assert_eq!(shared.tenants_due_for_purge().unwrap(), ["acme"]);
    let held = shared.purge_tenant("acme").unwrap_err().to_string();
    assert!(held.contains("primary running"), "{held}");
    acme.release_writer_lease();
    let removed = shared.purge_tenant("acme").unwrap();
    assert!(removed > 0);
    assert!(shared.get_tenant("acme").unwrap().is_none());
    assert_eq!(crate::tenancy_tests::rows_of(&url, "acme", "documents"), 0);
    // The name is free again.
    assert!(matches!(
        shared.create_tenant("acme", &json!({})).unwrap(),
        Created::New(_)
    ));
}

/// A database made by the release before the registry kept settings, holding tenants with rows,
/// is brought up to date by the first process that serves every tenant, and every tenant is
/// served as it was.
#[test]
fn upgrading_a_shared_database_keeps_every_tenant() {
    let Some(admin) = test_url("tenancy: upgrade") else {
        return;
    };
    crate::test_support::ensure_ordinary_role(&admin);
    let url = fresh_database(&admin, "tshupgrade");
    client(&url)
        .batch_execute(&format!(
            "ALTER DATABASE {} OWNER TO {}",
            crate::test_support::database_name(&url),
            crate::test_support::ORDINARY_ROLE
        ))
        .unwrap();
    let app = app_url(&url);
    // The layout of the release before: four migrations, no settings in the registry.
    let mut old = client(&app);
    crate::migrate::run(&mut old, &crate::migrate::SHARED_MIGRATIONS[..4]).unwrap();
    const N: usize = 5;
    let names: Vec<String> = (0..N).map(|n| format!("t{n}")).collect();
    for name in &names {
        old.execute("INSERT INTO refhir_tenants (name) VALUES ($1)", &[name])
            .unwrap();
        let tenant = crate::tenancy_tests::open_restricted(&url, name);
        tenant
            .write(&[doc("p", 1, name)], &[], ConflictStrategy::Error)
            .unwrap();
    }

    let shared = connections(&app, 2);
    let applied: i64 = client(&url)
        .query_one("SELECT max(version) FROM schema_migrations", &[])
        .unwrap()
        .get(0);
    assert_eq!(applied, crate::migrate::SHARED_MIGRATIONS.len() as i64);
    let tenants = shared.list_tenants().unwrap();
    assert_eq!(
        tenants
            .iter()
            .map(|tenant| tenant.name.clone())
            .collect::<Vec<_>>(),
        names
    );
    for tenant in &tenants {
        assert_eq!(tenant.settings, json!({}));
        assert!(!tenant.is_deleted());
        let served = PostgresPersistence::open_tenant(&shared, &tenant.name, 1).unwrap();
        assert_eq!(read_all(&served), [tenant.name.clone()]);
    }
    // A second process finds nothing left to do.
    let _again = connections(&app, 1);
}

/// A process that serves every tenant may be pointed at an empty database: it gives it the shared
/// layout, with no tenants yet. One made without tenants it refuses.
#[test]
fn shared_connections_take_an_empty_database_and_refuse_a_classic_one() {
    let Some(admin) = test_url("tenancy: shared on empty") else {
        return;
    };
    let empty = fresh_database(&admin, "tshempty");
    let shared = connections(&empty, 1);
    assert!(shared.list_tenants().unwrap().is_empty());
    assert!(crate::holds_tenants(&empty).unwrap());

    let classic = fresh_database(&admin, "tshclassic");
    PostgresPersistence::connect(&classic).unwrap();
    let refused = SharedConnections::connect(&classic, &SharedOptions::default())
        .unwrap_err()
        .to_string();
    assert!(refused.contains("without tenants"), "{refused}");
}

/// What a tenant stores is measured from its own rows.
#[test]
fn storage_is_measured_per_tenant() {
    let Some(admin) = test_url("tenancy: storage") else {
        return;
    };
    let url = shared(&admin, "tshsize", &["big", "small"]);
    let shared = connections(&operator_url(&url), 2);
    let big = PostgresPersistence::open_tenant(&shared, "big", 1).unwrap();
    let small = PostgresPersistence::open_tenant(&shared, "small", 1).unwrap();
    // Text that does not compress, as Postgres would compress a repeated letter to nothing.
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let text: String = (0..10_000)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            char::from(b'a' + (seed % 26) as u8)
        })
        .collect();
    for ts in 1..=20u64 {
        big.write(
            &[doc(&format!("p{ts}"), ts, &text)],
            &[],
            ConflictStrategy::Error,
        )
        .unwrap();
    }
    small
        .write(&[doc("p", 1, "y")], &[], ConflictStrategy::Error)
        .unwrap();
    let big = big.storage_usage().unwrap();
    let small = small.storage_usage().unwrap();
    assert_eq!((big.documents, small.documents), (20, 1));
    assert!(big.bytes() > 20 * 1_000, "{big:?}");
    assert!(small.bytes() < 1_000, "{small:?}");
}

/// What a tenant of a shared pool says about the catalog it can share with others (the SQL behind
/// it reads instance 0 and the tenant's own rows, as the application's role): tenants holding the
/// same definitions say the same key and read the same rows, a tenant's own terminology or a
/// package of its own ends it, an expansion it changed is carried beside the shared copy, and a
/// database of another name says another key.
#[test]
fn tenants_of_one_pool_with_the_same_definitions_say_the_same_catalog() {
    use crate::tenancy_catalog_tests::{demo, install};
    use refhir_persistence::{ConformanceStore, ExpansionChange, NewTerminology};

    let Some(admin) = test_url("tenancy: shared catalog key") else {
        return;
    };
    let url = shared(&admin, "tshcat", &["a", "b", "c", "d"]);
    let pool = connections(&app_url(&url), 2);
    let open = |name: &str| PostgresPersistence::open_tenant(&pool, name, 1).unwrap();
    let (a, b, c, d) = (open("a"), open("b"), open("c"), open("d"));
    let package = demo("shared", "4.0.1");
    let (installed, _) = install(&a, &package, "sha-1").unwrap();
    install(&b, &package, "sha-1").unwrap();

    // Nothing shared yet for a tenant that has no package.
    assert!(c.shared_catalog().unwrap().is_none());
    let (ka, kb) = (
        a.shared_catalog().unwrap().expect("a shares"),
        b.shared_catalog().unwrap().expect("b shares"),
    );
    assert_eq!(ka.key, kb.key);
    let database = crate::test_support::database_name(&url);
    assert!(
        ka.key.contains(&database),
        "{} does not name the database {database}",
        ka.key
    );

    // What every tenant reads from it is the shared copy, whichever tenant it was made from.
    let url_0 = package.expansions[0].url.clone();
    let from_base = ka.source.load_expansion_json(installed.package_id, &url_0);
    assert_eq!(from_base.unwrap().as_deref(), Some(r#"{"expansion":0}"#));
    assert!(ka
        .source
        .load_expansion_json(installed.package_id, "http://example.org/none")
        .unwrap()
        .is_none());

    // The same package in another database says another key: the ids are each database's own.
    let other = shared(&admin, "tshcat2", &["a"]);
    let other_pool = connections(&app_url(&other), 2);
    let other_a = PostgresPersistence::open_tenant(&other_pool, "a", 1).unwrap();
    install(&other_a, &package, "sha-1").unwrap();
    let other_key = other_a.shared_catalog().unwrap().expect("shares").key;
    assert_ne!(other_key, ka.key);

    // A tenant that changed an expansion of the shared package says another key, and its source
    // answers with what it changed; the others' still answer with the shared copy.
    a.apply_expansions(&[ExpansionChange {
        package_id: installed.package_id,
        url: url_0.clone(),
        json: Some(r#"{"expansion":"changed by a"}"#.into()),
    }])
    .unwrap();
    let changed = a.shared_catalog().unwrap().expect("a still shares");
    assert_ne!(changed.key, kb.key);
    assert_eq!(
        changed
            .source
            .load_expansion_json(installed.package_id, &url_0)
            .unwrap()
            .as_deref(),
        Some(r#"{"expansion":"changed by a"}"#)
    );
    assert_eq!(
        b.shared_catalog().unwrap().unwrap().key,
        kb.key,
        "b's key changed with a's"
    );
    assert_eq!(
        b.shared_catalog()
            .unwrap()
            .unwrap()
            .source
            .load_expansion_json(installed.package_id, &url_0)
            .unwrap()
            .as_deref(),
        Some(r#"{"expansion":0}"#)
    );

    // A tenant's own terminology is not in the shared rows, so it shares nothing from then on.
    install(&d, &package, "sha-1").unwrap();
    assert!(d.shared_catalog().unwrap().is_some());
    let local = d.ensure_local_package("4.0.1").unwrap();
    d.upsert_terminology(
        local,
        &NewTerminology {
            resource_type: "CodeSystem".into(),
            url: "http://example.org/d-own".into(),
            json: r#"{"resourceType":"CodeSystem"}"#.into(),
        },
    )
    .unwrap();
    assert!(d.shared_catalog().unwrap().is_none());
}
