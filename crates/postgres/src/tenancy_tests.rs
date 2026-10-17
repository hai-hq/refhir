//! The shared tenant layout, against a real Postgres (`REFHIR_TEST_POSTGRES_URL`).
//!
//! The method: a statement that forgets its tenant does not fail, it reads or writes another
//! tenant's rows. So these tests never run a suite on a quiet database. They give a second tenant
//! the very same ids, keys and timestamps first, run the suite as the first tenant, and then
//! require both that the suite's own assertions hold and that the other tenant's rows are
//! byte-for-byte what they were.

use std::time::Duration;

use postgres::{Client, NoTls};
use refhir_persistence::{
    conformance, ConflictStrategy, DocumentLogEntry, Lease, PersistError, Persistence,
    PersistenceGlobalKey, PersistenceIndexEntry, PersistenceReader,
};
use refhir_types::{DocumentId, IndexId, TabletId, Timestamp};

use crate::tenant::TENANT_TABLES;
use crate::tests::{fresh_database, test_url};
use crate::{
    create_tenant, delete_tenant, list_tenants, ConnectOptions, PostgresPersistence, Scope,
};

pub(crate) fn client(url: &str) -> Client {
    Client::connect(url, NoTls).expect("connect")
}

pub(crate) fn open(url: &str, tenant: &str) -> PostgresPersistence {
    PostgresPersistence::connect_with(
        url,
        &ConnectOptions {
            pool_size: 2,
            tenant: Some(tenant.to_string()),
            ..ConnectOptions::default()
        },
    )
    .unwrap_or_else(|err| panic!("open tenant {tenant}: {err}"))
}

/// What a refused connection says. `PostgresPersistence` is not `Debug`, so the usual `unwrap_err`
/// is not available.
fn refusal(result: Result<PostgresPersistence, PersistError>) -> String {
    match result {
        Ok(_) => panic!("the connection was accepted"),
        Err(err) => err.to_string(),
    }
}

/// A new database in the shared layout holding these tenants.
pub(crate) fn shared(admin: &str, tag: &str, tenants: &[&str]) -> String {
    let url = fresh_database(admin, tag);
    for name in tenants {
        create_tenant(&url, name).unwrap_or_else(|err| panic!("create {name}: {err}"));
    }
    url
}

pub(crate) fn tenant_id(url: &str, name: &str) -> i32 {
    client(url)
        .query_one("SELECT id FROM refhir_tenants WHERE name = $1", &[&name])
        .expect("tenant")
        .get(0)
}

/// One digest per table of everything a tenant holds, with the tenant column left out so that two
/// tenants that did the same thing digest the same (for tables whose ids come from no sequence).
pub(crate) fn digests(url: &str, tenant: &str) -> Vec<(&'static str, String)> {
    let id = tenant_id(url, tenant);
    let mut client = client(url);
    TENANT_TABLES
        .iter()
        .map(|table| {
            let digest: String = client
                .query_one(
                    &format!(
                        "SELECT COALESCE(md5(string_agg(r, E'\\n' ORDER BY r)), '') FROM \
                         (SELECT (to_jsonb(t) - 'instance_id')::text AS r FROM {table} t \
                          WHERE instance_id = $1) x"
                    ),
                    &[&id],
                )
                .expect("digest")
                .get(0);
            (*table, digest)
        })
        .collect()
}

pub(crate) fn rows_of(url: &str, tenant: &str, table: &str) -> i64 {
    client(url)
        .query_one(
            &format!(
                "SELECT count(*) FROM {table} \
                 WHERE instance_id = (SELECT id FROM refhir_tenants WHERE name = $1)"
            ),
            &[&tenant],
        )
        .expect("count")
        .get(0)
}

pub(crate) fn suite(p: &PostgresPersistence) {
    conformance::exercise(p);
    conformance::exercise_index_scan_ids(p);
}

/// Makes `tenant` a poison tenant: from now on, every row any other tenant inserts into any
/// tenant table gets a twin in `tenant` at the moment it is inserted. The twin has the same keys
/// (the same ids, urls, kinds, timestamps) and a newer sequence id where the table has one.
///
/// A decoy twin (`exact = false`) has different content: a decoy payload, a far-future expiry, a
/// job waiting to be claimed. An exact twin has the very content of the row it copies, so a
/// statement that selects by content or state (`DELETE ... AND json = $3`, `WHERE lease_owner =
/// $3`, `WHERE exp <= $1`) matches it as it matches the original. Each kind exposes what the other
/// hides, so a test uses a poison tenant of each.
///
/// So a statement that forgot its tenant meets an ambiguous row: a read returns two, an update or
/// delete takes the twin too, a keyed lookup by an id the other tenant owns finds both. The suites
/// clean up after themselves, so a decoy made once, afterwards, would find nothing left to hit; a
/// twin made as each row appears does. [`check_twins`] then requires every twin to be exactly as it
/// was made.
pub(crate) fn poison(url: &str, tenant: &str) {
    poison_with(url, tenant, false);
}

/// [`poison`] with exact twins.
pub(crate) fn poison_exact(url: &str, tenant: &str) {
    poison_with(url, tenant, true);
}

fn poison_with(url: &str, tenant: &str, exact: bool) {
    let id = tenant_id(url, tenant);
    let mut c = client(url);
    c.batch_execute(
        r#"
CREATE TABLE IF NOT EXISTS twin_log (tbl text NOT NULL, row jsonb NOT NULL);
CREATE OR REPLACE FUNCTION make_twin() RETURNS trigger LANGUAGE plpgsql AS $f$
DECLARE
    poisoned int := TG_ARGV[0]::int;
    exact boolean := TG_ARGV[1] = 'exact';
    made jsonb;
    seq text;
    inserted int;
BEGIN
    -- Only for the row a test wrote, not for the twins the poison tenants make of it.
    IF pg_trigger_depth() > 1 OR NEW.instance_id = poisoned THEN
        RETURN NULL;
    END IF;
    made := to_jsonb(NEW) || jsonb_build_object('instance_id', poisoned);
    IF made ? 'id' THEN
        seq := pg_get_serial_sequence(TG_TABLE_NAME, 'id');
    END IF;
    IF seq IS NOT NULL THEN
        made := made || jsonb_build_object('id', nextval(seq));
    END IF;
    IF NOT exact THEN
        IF made ? 'json' AND jsonb_typeof(made -> 'json') = 'string' THEN
            made := made || jsonb_build_object('json', '{"decoy":true}');
        END IF;
        IF made ? 'json_value' AND jsonb_typeof(made -> 'json_value') = 'string'
           AND TG_TABLE_NAME <> 'persistence_globals' THEN
            made := made || jsonb_build_object('json_value', '{"decoy":true}');
        END IF;
        IF TG_TABLE_NAME = 'smart_oauth' THEN
            made := made || jsonb_build_object('exp', 9000000000000000000);
        END IF;
        IF TG_TABLE_NAME = 'jobs' THEN
            made := made || jsonb_build_object(
                'status', 'accepted', 'lease_owner', NULL::text, 'lease_until', NULL::bigint,
                'result', NULL::text, 'error', NULL::text, 'completed_at', NULL::bigint);
        END IF;
    END IF;
    EXECUTE format(
        'INSERT INTO %I SELECT * FROM jsonb_populate_record(NULL::%I, $1) ON CONFLICT DO NOTHING',
        TG_TABLE_NAME, TG_TABLE_NAME) USING made;
    GET DIAGNOSTICS inserted = ROW_COUNT;
    IF inserted = 1 THEN
        INSERT INTO twin_log VALUES (TG_TABLE_NAME, made);
    END IF;
    RETURN NULL;
END
$f$;
"#,
    )
    .expect("twin function");
    for table in TENANT_TABLES {
        c.batch_execute(&format!(
            "CREATE TRIGGER twin_{id} AFTER INSERT ON {table} \
             FOR EACH ROW EXECUTE FUNCTION make_twin({id}, '{}')",
            if exact { "exact" } else { "decoy" }
        ))
        .expect("twin trigger");
    }
}

/// Requires that the poison tenant `tenant` holds exactly the twins [`poison`] made, each as made:
/// nothing another tenant did changed, removed or added a row of it. Returns how many twins there
/// were, so a test can check it saw some.
pub(crate) fn check_twins(url: &str, tenant: &str) -> i64 {
    let id = tenant_id(url, tenant);
    let mut c = client(url);
    let mut twins = 0;
    for table in TENANT_TABLES {
        let made: i64 = c
            .query_one(
                "SELECT count(*) FROM twin_log WHERE tbl = $1 AND (row ->> 'instance_id')::int = $2",
                &[table, &id],
            )
            .unwrap()
            .get(0);
        let altered: i64 = c
            .query_one(
                &format!(
                    "SELECT count(*) FROM twin_log l WHERE l.tbl = '{table}' \
                     AND (l.row ->> 'instance_id')::int = {id} AND NOT EXISTS \
                     (SELECT 1 FROM {table} t WHERE t.instance_id = {id} AND to_jsonb(t) = l.row)"
                ),
                &[],
            )
            .unwrap()
            .get(0);
        let held: i64 = c
            .query_one(
                &format!("SELECT count(*) FROM {table} WHERE instance_id = {id}"),
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(
            altered, 0,
            "{altered} twins in {table} were changed or removed"
        );
        assert_eq!(
            held, made,
            "{table} holds rows the poison tenant was never given"
        );
        twins += made;
    }
    twins
}

/// Gives `tenant` a decoy revision of every document and index row it has, at every timestamp from
/// 1 to 100 that it does not already use. Same ids, same keys, different content, and (unlike the
/// suite's own data, which a statement that forgot its tenant would mostly still get right by luck)
/// a newer row wherever another tenant looks. Any read that reaches across tenants returns a decoy,
/// or the wrong newest timestamp, and a suite that asserts exact results fails.
pub(crate) fn decoy(url: &str, tenant: &str) {
    let id = tenant_id(url, tenant);
    client(url)
        .batch_execute(&format!(
            "INSERT INTO documents (instance_id, id, ts, table_id, json_value, deleted, prev_ts) \
             SELECT d.instance_id, d.id, g, d.table_id, '{{\"decoy\":true}}', 0, NULL \
             FROM (SELECT DISTINCT instance_id, id, table_id FROM documents \
                   WHERE instance_id = {id}) d, generate_series(1, 100) g \
             ON CONFLICT DO NOTHING; \
             INSERT INTO indexes (instance_id, index_id, ts, key, deleted, table_id, document_id) \
             SELECT i.instance_id, i.index_id, g, i.key, 0, i.table_id, i.document_id \
             FROM (SELECT DISTINCT instance_id, index_id, key, table_id, document_id FROM indexes \
                   WHERE instance_id = {id} AND document_id IS NOT NULL) i, \
                  generate_series(1, 100) g \
             ON CONFLICT DO NOTHING;"
        ))
        .expect("decoy rows");
}

fn write_doc(p: &PostgresPersistence, ts: u64, name: &str) -> Result<(), PersistError> {
    let id = DocumentId::new(TabletId::new("lease"), name.as_bytes());
    let ts = Timestamp::from_u64(ts).unwrap();
    p.write(
        &[DocumentLogEntry {
            ts,
            id: id.clone(),
            value: Some("{}".into()),
            prev_ts: None,
        }],
        &[PersistenceIndexEntry {
            ts,
            index_id: IndexId::new("lease/by_id"),
            key: name.as_bytes().to_vec(),
            value: Some(id),
        }],
        ConflictStrategy::Error,
    )
}

fn acquire_within(p: &PostgresPersistence, seconds: u64) -> Lease {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        match p.try_acquire_writer_lease().expect("lease attempt") {
            Lease::Busy if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            other => return other,
        }
    }
}

#[test]
fn the_first_tenant_makes_the_shared_layout() {
    let Some(admin) = test_url("tenant registry") else {
        return;
    };
    let url = fresh_database(&admin, "registry");
    assert!(list_tenants(&url).unwrap().is_empty());

    let acme = create_tenant(&url, "acme").unwrap();
    let beta = create_tenant(&url, "beta").unwrap();
    assert_eq!((acme.id, acme.name.as_str()), (1, "acme"));
    assert_eq!((beta.id, beta.name.as_str()), (2, "beta"));

    let error = create_tenant(&url, "acme").unwrap_err().to_string();
    assert!(error.contains("already exists"), "{error}");
    for bad in [
        "",
        "Acme",
        "-x",
        "has space",
        "_base",
        "x".repeat(64).as_str(),
    ] {
        assert!(create_tenant(&url, bad).is_err(), "{bad:?} was accepted");
    }

    let listed = list_tenants(&url).unwrap();
    let names: Vec<_> = listed.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        ["acme", "beta"],
        "the reserved base row is not a tenant"
    );
    assert!(listed.iter().all(|tenant| !tenant.primary_running));
    // The reserved row exists, so a later shared catalog can be owned by id 0, and the name check
    // cannot be used to create it.
    let base: String = client(&url)
        .query_one("SELECT name FROM refhir_tenants WHERE id = 0", &[])
        .unwrap()
        .get(0);
    assert_eq!(base, "_base");
}

#[test]
fn a_database_keeps_its_layout() {
    let Some(admin) = test_url("tenant layouts") else {
        return;
    };

    // A database made without tenants is not shared, and says so.
    let classic = fresh_database(&admin, "classic");
    PostgresPersistence::connect(&classic).expect("classic connect");
    let error = create_tenant(&classic, "acme").unwrap_err().to_string();
    assert!(error.contains("without tenants"), "{error}");
    assert!(list_tenants(&classic).is_err());
    let error = refusal(PostgresPersistence::connect_with(
        &classic,
        &ConnectOptions {
            tenant: Some("acme".into()),
            ..ConnectOptions::default()
        },
    ));
    assert!(error.contains("created without tenants"), "{error}");

    // A shared database refuses a process that does not name a tenant, and one that names a tenant
    // it does not have.
    let url = shared(&admin, "layouts", &["acme"]);
    let error = refusal(PostgresPersistence::connect(&url));
    assert!(error.contains("--tenant"), "{error}");
    let error = refusal(PostgresPersistence::connect_with(
        &url,
        &ConnectOptions {
            tenant: Some("ghost".into()),
            ..ConnectOptions::default()
        },
    ));
    assert!(error.contains("does not exist"), "{error}");
    let _ = open(&url, "acme");

    // An empty database has nothing to be a tenant of.
    let empty = fresh_database(&admin, "emptylayout");
    let error = refusal(PostgresPersistence::connect_with(
        &empty,
        &ConnectOptions {
            tenant: Some("acme".into()),
            ..ConnectOptions::default()
        },
    ));
    assert!(error.contains("is empty"), "{error}");

    // The reserved id is not a tenant a process can serve.
    let error = refusal(PostgresPersistence::connect_with(
        &url,
        &ConnectOptions {
            tenant: Some("_base".into()),
            ..ConnectOptions::default()
        },
    ));
    assert!(error.contains("not a tenant name"), "{error}");
}

#[test]
fn a_tenant_passes_the_persistence_suite_beside_a_tenant_holding_the_same_data() {
    let Some(admin) = test_url("tenant persistence suite") else {
        return;
    };
    let url = shared(&admin, "suite", &["a", "b", "twins", "exact"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));

    // Tenant b first: it writes the very ids, keys and timestamps the suite uses, and then a decoy
    // revision of each of them at every timestamp.
    suite(&b);
    decoy(&url, "b");
    // From here, `twins` and `exact` get a twin of every row tenant `a` writes, as it is written:
    // a decoy one and an exact copy. (After the decoys, which are thousands of rows.)
    poison(&url, "twins");
    poison_exact(&url, "exact");
    let before = digests(&url, "b");
    assert!(rows_of(&url, "b", "documents") > 100);
    assert_eq!(rows_of(&url, "a", "documents"), 0);

    // Tenant a then finds an empty database (`is_fresh` is per tenant) and passes the same suite,
    // which asserts exact results, so a read that crossed over would fail it.
    suite(&a);
    assert_eq!(
        digests(&url, "b"),
        before,
        "tenant a's suite changed tenant b's rows"
    );
    for poisoned in ["twins", "exact"] {
        assert!(
            check_twins(&url, poisoned) > 0,
            "the poison tenant {poisoned} was given no twins"
        );
    }
}

#[test]
fn deleting_a_tenant_removes_its_rows_and_only_its_rows() {
    let Some(admin) = test_url("tenant delete") else {
        return;
    };
    let url = shared(&admin, "tdelete", &["a", "b", "big"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    suite(&a);
    suite(&b);
    // Tenant `big` holds more rows than one batch of the deletion removes.
    let big = tenant_id(&url, "big");
    client(&url)
        .batch_execute(&format!(
            "INSERT INTO documents (instance_id, id, ts, table_id, json_value, deleted) \
             SELECT {big}, convert_to('doc-' || g, 'UTF8'), g, convert_to('t', 'UTF8'), '{{}}', 0 \
             FROM generate_series(1, 25000) g"
        ))
        .unwrap();
    let kept = digests(&url, "b");
    let a_id = tenant_id(&url, "a");

    // While a primary of the tenant runs, it is refused and nothing is removed.
    a.try_acquire_writer_lease().unwrap();
    let error = delete_tenant(&url, "a").unwrap_err().to_string();
    assert!(error.contains("primary running"), "{error}");
    assert!(rows_of(&url, "a", "documents") > 0);
    a.release_writer_lease();
    let removed = (0..100)
        .find_map(|_| match delete_tenant(&url, "a") {
            Ok(removed) => Some(removed),
            Err(_) => {
                std::thread::sleep(Duration::from_millis(100));
                None
            }
        })
        .expect("the tenant could be deleted once its primary was gone");
    assert!(removed > 0);

    let mut c = client(&url);
    for table in TENANT_TABLES {
        let left: i64 = c
            .query_one(
                &format!("SELECT count(*) FROM {table} WHERE instance_id = {a_id}"),
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(left, 0, "{table} still holds rows of the deleted tenant");
    }
    assert_eq!(
        digests(&url, "b"),
        kept,
        "deleting tenant a changed tenant b"
    );
    let names: Vec<_> = list_tenants(&url)
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, ["b", "big"]);
    let error = refusal(PostgresPersistence::connect_with(
        &url,
        &ConnectOptions {
            tenant: Some("a".into()),
            ..ConnectOptions::default()
        },
    ));
    assert!(error.contains("does not exist"), "{error}");

    // More than one batch, and the registry row goes with it; the name can be used again.
    assert!(delete_tenant(&url, "big").unwrap() >= 25_000);
    assert_eq!(rows_of_id(&url, big, "documents"), 0);
    create_tenant(&url, "big").expect("a deleted tenant's name is free again");

    assert!(delete_tenant(&url, "ghost").is_err());
    let classic = fresh_database(&admin, "tdeletec");
    PostgresPersistence::connect(&classic).unwrap();
    assert!(delete_tenant(&classic, "a").is_err());
}

fn rows_of_id(url: &str, id: i32, table: &str) -> i64 {
    client(url)
        .query_one(
            &format!("SELECT count(*) FROM {table} WHERE instance_id = {id}"),
            &[],
        )
        .unwrap()
        .get(0)
}

#[test]
fn point_reads_never_return_another_tenants_revision() {
    let Some(admin) = test_url("tenant point reads") else {
        return;
    };
    let url = shared(&admin, "tpoint", &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    // Tenant b holds this very document at every timestamp up to 100, with different content.
    suite(&b);
    decoy(&url, "b");

    let id = DocumentId::new(TabletId::new("fixture"), b"doc-1".as_slice());
    let at = |n: u64| Timestamp::from_u64(n).unwrap();
    let mine = r#"{"owner":"a"}"#;
    a.write(
        &[DocumentLogEntry {
            ts: at(3),
            id: id.clone(),
            value: Some(mine.into()),
            prev_ts: None,
        }],
        &[],
        ConflictStrategy::Error,
    )
    .unwrap();

    let latest = a
        .latest_document(&id, at(50))
        .unwrap()
        .expect("a's revision");
    assert_eq!((latest.ts, latest.value.as_deref()), (at(3), Some(mine)));
    let before = a
        .previous_revision(&id, at(50))
        .unwrap()
        .expect("a's revision");
    assert_eq!((before.ts, before.value.as_deref()), (at(3), Some(mine)));
    assert!(
        a.exact_revision(&id, at(7)).unwrap().is_none(),
        "a has no revision at 7; b's decoy is not one"
    );
    let history = a.document_history(&id, at(50)).unwrap();
    assert_eq!(history.len(), 1, "{history:?}");
    assert_eq!(history[0].value.as_deref(), Some(mine));
}

#[test]
fn index_only_commits_overwrites_and_globals_stay_inside_the_tenant() {
    let Some(admin) = test_url("tenant write paths") else {
        return;
    };
    let url = shared(&admin, "twrites", &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    let at = |n: u64| Timestamp::from_u64(n).unwrap();
    let id = DocumentId::new(TabletId::new("t"), b"d".as_slice());

    // A commit that only moves index rows (a backfill) is recorded in a global, not by a
    // document: it must be this tenant's global.
    a.write(
        &[],
        &[PersistenceIndexEntry {
            ts: at(9),
            index_id: IndexId::new("t/by_x"),
            key: b"k".to_vec(),
            value: Some(id.clone()),
        }],
        ConflictStrategy::Error,
    )
    .unwrap();
    assert_eq!(a.max_ts().unwrap(), Some(at(9)));
    assert_eq!(
        b.max_ts().unwrap(),
        None,
        "tenant b sees tenant a's index-only commit"
    );

    // Overwriting a row replaces it, by the tenant's keys: a conflict target without the tenant
    // column would be refused by Postgres, and one that ignored the tenant would replace b's row.
    let write = |p: &PostgresPersistence, ts: u64, json: &str| {
        p.write(
            &[DocumentLogEntry {
                ts: at(ts),
                id: id.clone(),
                value: Some(json.into()),
                prev_ts: None,
            }],
            &[PersistenceIndexEntry {
                ts: at(ts),
                index_id: IndexId::new("t/by_id"),
                key: b"k".to_vec(),
                value: Some(id.clone()),
            }],
            ConflictStrategy::Overwrite,
        )
        .unwrap();
    };
    write(&b, 5, r#"{"v":"b"}"#);
    write(&a, 5, r#"{"v":"a1"}"#);
    write(&a, 5, r#"{"v":"a2"}"#);
    let read = |p: &PostgresPersistence| {
        p.latest_document(&id, at(5))
            .unwrap()
            .expect("revision")
            .value
            .unwrap()
    };
    assert_eq!(read(&a), r#"{"v":"a2"}"#);
    assert_eq!(
        read(&b),
        r#"{"v":"b"}"#,
        "an overwrite in tenant a replaced tenant b's row"
    );
    assert_eq!(rows_of(&url, "a", "documents"), 1);
    assert_eq!(rows_of(&url, "b", "documents"), 1);

    // A global written for one tenant is not read by another.
    a.write_persistence_global(PersistenceGlobalKey::CatalogVersion, &serde_json::json!(7))
        .unwrap();
    assert_eq!(
        a.get_persistence_global(PersistenceGlobalKey::CatalogVersion)
            .unwrap(),
        Some(serde_json::json!(7))
    );
    assert_eq!(
        b.get_persistence_global(PersistenceGlobalKey::CatalogVersion)
            .unwrap(),
        None
    );
}

#[test]
fn tenants_run_the_persistence_suite_at_the_same_time() {
    let Some(admin) = test_url("tenants at once") else {
        return;
    };
    let names = ["t1", "t2", "t3", "t4"];
    let url = shared(&admin, "atonce", &["poison", "t1", "t2", "t3", "t4"]);
    suite(&open(&url, "poison"));
    decoy(&url, "poison");
    let poisoned = digests(&url, "poison");
    let handles: Vec<_> = names
        .iter()
        .map(|name| {
            let (url, name) = (url.clone(), name.to_string());
            std::thread::spawn(move || {
                suite(&open(&url, &name));
                name
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("a tenant's suite failed");
    }
    assert_eq!(
        digests(&url, "poison"),
        poisoned,
        "a tenant changed the decoy tenant's rows"
    );
    let first = digests(&url, "t1");
    for name in &names[1..] {
        let other = digests(&url, name);
        for table in ["documents", "indexes", "persistence_globals"] {
            let pick = |all: &[(&'static str, String)]| {
                all.iter().find(|(n, _)| *n == table).unwrap().1.clone()
            };
            assert_eq!(pick(&first), pick(&other), "{table}: t1 and {name} differ");
        }
    }
}

#[test]
fn every_tenant_has_its_own_writer_lease_and_epoch() {
    let Some(admin) = test_url("tenant leases") else {
        return;
    };
    let url = shared(&admin, "tlease", &["a", "b"]);
    let (a1, a2, b) = (open(&url, "a"), open(&url, "a"), open(&url, "b"));

    assert_eq!(
        a1.try_acquire_writer_lease().unwrap(),
        Lease::Held { epoch: 1 }
    );
    // Tenant a holding its lease does not keep tenant b out of its own.
    assert_eq!(
        b.try_acquire_writer_lease().unwrap(),
        Lease::Held { epoch: 1 }
    );
    // A second process of the same tenant still cannot write.
    assert_eq!(a2.try_acquire_writer_lease().unwrap(), Lease::Busy);
    assert!(list_tenants(&url)
        .unwrap()
        .iter()
        .all(|t| t.primary_running));

    // When tenant a's holder goes, a successor takes over at a higher epoch and b is not touched.
    a1.release_writer_lease();
    assert_eq!(acquire_within(&a2, 10), Lease::Held { epoch: 2 });
    let epochs: Vec<(String, String)> = client(&url)
        .query(
            "SELECT t.name, g.json_value FROM persistence_globals g \
             JOIN refhir_tenants t ON t.id = g.instance_id \
             WHERE g.key = 'writer_epoch' ORDER BY t.name",
            &[],
        )
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    assert_eq!(
        epochs,
        [
            ("a".to_string(), "2".to_string()),
            ("b".to_string(), "1".to_string())
        ]
    );
    write_doc(&b, 1, "b still writes").expect("tenant b is not fenced by a's takeover");
}

#[test]
fn a_tenant_is_fenced_by_its_own_epoch_only() {
    let Some(admin) = test_url("tenant fencing") else {
        return;
    };
    let url = shared(&admin, "tfence", &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    a.try_acquire_writer_lease().unwrap();
    b.try_acquire_writer_lease().unwrap();
    write_doc(&a, 1, "one").unwrap();
    write_doc(&b, 1, "one").unwrap();

    // A successor takes over tenant a alone.
    client(&url)
        .execute(
            "UPDATE persistence_globals SET json_value = '2' WHERE key = 'writer_epoch' \
             AND instance_id = (SELECT id FROM refhir_tenants WHERE name = 'a')",
            &[],
        )
        .unwrap();
    let stale = write_doc(&a, 2, "stale").unwrap_err();
    assert!(matches!(stale, PersistError::Fenced(_)), "{stale}");
    write_doc(&b, 2, "fine").expect("tenant b's epoch was not touched");
    // Tenant a's keepalive would see the new epoch and end this process; let it go first.
    a.release_writer_lease();
}

#[test]
fn the_published_watermark_and_its_announcements_are_per_tenant() {
    let Some(admin) = test_url("tenant watermark") else {
        return;
    };
    let url = shared(&admin, "twatermark", &["a", "b"]);
    let (a, b) = (open(&url, "a"), open(&url, "b"));
    let at = |n: u64| Timestamp::from_u64(n).unwrap();
    let published = |p: &PostgresPersistence| {
        p.get_persistence_global(PersistenceGlobalKey::PublishedTimestamp)
            .unwrap()
            .and_then(|value| value.as_u64())
    };

    a.advance_published_ts(at(5)).unwrap();
    assert_eq!(published(&a), Some(5));
    assert_eq!(
        published(&b),
        None,
        "tenant b followed tenant a's watermark"
    );
    b.advance_published_ts(at(3)).unwrap();
    assert_eq!((published(&a), published(&b)), (Some(5), Some(3)));

    // Each tenant listens on a channel of its own, so one tenant's commits do not wake another's
    // replicas.
    let _ = a.wait_for_publish(Duration::from_millis(50)); // opens the listener
    b.advance_published_ts(at(4)).unwrap();
    assert!(
        !a.wait_for_publish(Duration::from_millis(500)),
        "tenant b's publish woke tenant a"
    );
    a.advance_published_ts(at(6)).unwrap();
    assert!(
        a.wait_for_publish(Duration::from_secs(5)),
        "tenant a's own publish did not arrive"
    );
}

#[test]
fn the_hot_queries_read_an_index_range_not_the_table_with_many_tenants() {
    let Some(admin) = test_url("tenant query plans") else {
        return;
    };
    let names: Vec<String> = (0..20).map(|n| format!("t{n:02}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let url = shared(&admin, "tplans", &refs);
    let mut c = client(&url);
    // 20 tenants with the same 4,000 ids, keys and timestamps each: only the tenant column keeps
    // them apart, so a plan that does not lead with it reads other tenants' rows.
    c.batch_execute(
        "INSERT INTO documents (instance_id, id, ts, table_id, json_value, deleted) \
         SELECT t.id, convert_to('doc-' || g, 'UTF8'), g, convert_to('tbl', 'UTF8'), '{}', 0 \
         FROM refhir_tenants t, generate_series(1, 4000) g WHERE t.id > 0; \
         INSERT INTO indexes (instance_id, index_id, ts, key, deleted, table_id, document_id) \
         SELECT t.id, convert_to('tbl/by_id', 'UTF8'), g, convert_to('doc-' || g, 'UTF8'), 0, \
                convert_to('tbl', 'UTF8'), convert_to('doc-' || g, 'UTF8') \
         FROM refhir_tenants t, generate_series(1, 4000) g WHERE t.id > 0; \
         ANALYZE documents; ANALYZE indexes; ANALYZE persistence_globals;",
    )
    .unwrap();

    let scope = Scope::Tenant(tenant_id(&url, "t07"));
    let plan = |c: &mut Client, sql: &str, types: &str, args: &str| -> String {
        c.batch_execute("SET plan_cache_mode = force_custom_plan; DEALLOCATE ALL")
            .unwrap();
        c.batch_execute(&format!("PREPARE q({types}) AS {sql}"))
            .unwrap();
        let rows = c.query(&format!("EXPLAIN EXECUTE q({args})"), &[]).unwrap();
        rows.iter()
            .map(|row| row.get::<_, String>(0))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let (tbl, doc1) = (r"'\x74626c'", r"'\x646f632d31'");
    let index_id = r"'\x74626c2f62795f6964'";
    let cases = [
        (
            "latest_document",
            crate::sql_latest_document(scope),
            plan_args("bytea, bytea, bigint", &format!("{tbl}, {doc1}, 100")),
        ),
        (
            "index_scan_ids",
            crate::sql_index_scan_ids(scope, "", "ASC", " LIMIT 50"),
            plan_args("bytea, bigint, bytea", &format!("{index_id}, 100, '\\x'")),
        ),
        (
            "load_index_writes",
            crate::sql_load_index_writes(scope),
            plan_args("bigint, bigint", "10, 50"),
        ),
        (
            "load_documents",
            crate::sql_load_documents(scope, refhir_types::Order::Asc),
            plan_args("bigint, bigint", "10, 50"),
        ),
        (
            "max_ts",
            crate::sql_max_ts(scope),
            plan_args("text", "'max_repeatable_ts'"),
        ),
    ];
    for (name, sql, (types, args)) in cases {
        let text = plan(&mut c, &sql, types, &args);
        assert!(
            !text.contains("Seq Scan on documents") && !text.contains("Seq Scan on indexes"),
            "{name} scans a whole table:\n{text}"
        );
        assert!(text.contains("Index"), "{name} uses no index:\n{text}");
    }
}

/// What the shared layout has that the classic one does not: the tenant registry, and what lets
/// tenants share the packages they have in common. Nothing else is allowed to differ but the tenant
/// column and the few indexes listed beside the check.
const ADDED_TABLES: &[&str] = &[
    "refhir_tenants",
    "tenant_packages",
    "base_search_parameters",
    "base_expansions",
];

/// A column the shared layout adds to a table the classic one has, besides the tenant column.
const ADDED_COLUMNS: &[(&str, &str)] = &[("packages", "digest")];

/// (table, column) -> type, and index name -> its columns in order.
type Columns = std::collections::BTreeMap<(String, String), String>;
type Indexes = std::collections::BTreeMap<String, Vec<String>>;

fn plan_args<'a>(types: &'a str, args: &str) -> (&'a str, String) {
    (types, args.to_string())
}

#[test]
fn the_shared_layout_is_the_classic_one_plus_the_tenant_column() {
    let Some(admin) = test_url("shared layout drift") else {
        return;
    };
    let classic = fresh_database(&admin, "driftc");
    PostgresPersistence::connect(&classic).expect("classic");
    let shared_url = shared(&admin, "drifts", &["acme"]);

    let shape = |url: &str| -> (Columns, Indexes) {
        let mut c = client(url);
        let columns = c
            .query(
                "SELECT table_name, column_name, data_type || CASE WHEN is_nullable = 'NO' \
                 THEN ' not null' ELSE '' END FROM information_schema.columns \
                 WHERE table_schema = 'public'",
                &[],
            )
            .unwrap()
            .iter()
            .map(|row| ((row.get(0), row.get(1)), row.get(2)))
            .collect();
        // Index name -> its columns, in order (the key columns of every index, the primary keys
        // and unique constraints included).
        let mut indexes = std::collections::BTreeMap::new();
        for row in c
            .query(
                "SELECT i.relname, array_agg(a.attname::text ORDER BY k.ord) \
                 FROM pg_index x \
                 JOIN pg_class i ON i.oid = x.indexrelid \
                 JOIN pg_class t ON t.oid = x.indrelid \
                 JOIN pg_namespace n ON n.oid = t.relnamespace AND n.nspname = 'public' \
                 CROSS JOIN LATERAL unnest(x.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
                 JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
                 WHERE k.attnum > 0 GROUP BY i.relname",
                &[],
            )
            .unwrap()
        {
            indexes.insert(row.get::<_, String>(0), row.get::<_, Vec<String>>(1));
        }
        (columns, indexes)
    };
    let (classic_columns, classic_indexes) = shape(&classic);
    let (shared_columns, shared_indexes) = shape(&shared_url);

    // Tables and columns: every classic column is in the shared table, with the same type, and
    // the shared tables have `instance_id` in addition (and the registry, and nothing else).
    let tenant_tables: std::collections::BTreeSet<&str> = TENANT_TABLES.iter().copied().collect();
    for ((table, column), kind) in &classic_columns {
        if table == "schema_migrations" {
            continue;
        }
        assert_eq!(
            shared_columns.get(&(table.clone(), column.clone())),
            Some(kind),
            "{table}.{column} differs in the shared layout"
        );
    }
    for (table, column) in shared_columns.keys() {
        if table == "schema_migrations" || ADDED_TABLES.contains(&table.as_str()) {
            continue;
        }
        assert!(
            classic_columns.contains_key(&(table.clone(), column.clone()))
                || column == "instance_id"
                || ADDED_COLUMNS.contains(&(table.as_str(), column.as_str())),
            "the shared layout has {table}.{column}, which the classic layout lacks"
        );
    }
    for table in &tenant_tables {
        assert!(
            shared_columns.contains_key(&(table.to_string(), "instance_id".to_string())),
            "{table} has no tenant column"
        );
    }

    // Keys and indexes: every classic one is in the shared layout with `instance_id` in front;
    // the shared layout may add the few listed here, and nothing else.
    const ADDED: &[&str] = &[
        "index_backfills_by_instance",
        "terminology_by_url",
        "terminology_by_bare_url",
        "request_log_by_instance",
        "audit_log_by_instance",
    ];
    for (name, columns) in &classic_indexes {
        if name.starts_with("schema_migrations") {
            continue;
        }
        let mut want = vec!["instance_id".to_string()];
        want.extend(columns.iter().cloned());
        // A table keyed by a sequence (`id BIGSERIAL PRIMARY KEY`) keeps that key; the tenant is
        // then in its other indexes and in the row. (`jobs.id` is a text key, not one of these.)
        let keyed_by_sequence = [
            "packages_pkey",
            "index_backfills_pkey",
            "request_log_pkey",
            "audit_log_pkey",
        ]
        .contains(&name.as_str());
        let found = shared_indexes.get(name);
        if keyed_by_sequence {
            assert_eq!(found, Some(columns), "{name}");
        } else if name.ends_with("_key") {
            // A `UNIQUE (a, b)` constraint is named by Postgres from its columns.
            assert!(
                shared_indexes.values().any(|cols| *cols == want),
                "no shared index has the columns {want:?} that {name} has in the classic layout"
            );
        } else {
            assert_eq!(found, Some(&want), "{name} in the shared layout");
        }
    }
    for name in shared_indexes.keys() {
        if name.starts_with("schema_migrations")
            || ADDED_TABLES.iter().any(|table| name.starts_with(table))
        {
            continue;
        }
        let known = classic_indexes.contains_key(name)
            || ADDED.contains(&name.as_str())
            || name.ends_with("_key");
        assert!(
            known,
            "the shared layout has an index {name} the classic one lacks"
        );
    }
}
