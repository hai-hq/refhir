//! Tenants of one process that hold the same definitions hold one compiled copy of them.
//!
//! Against a database of its own on the server named by `REFHIR_TEST_POSTGRES_URL`, skipped when
//! that is not set (and failing when `REFHIR_REQUIRE_POSTGRES` is). The tenants are served through
//! one `SharedConnections`, as `refhir --all-tenants` serves them. This is a binary of its own,
//! with one test, so that the allocator it counts is the only thing running.

mod common;

use std::path::Path;
use std::sync::{Arc, RwLock};

use common::{measure, Counting};
use postgres::{Client, NoTls};
use refhir_database::Database;
use refhir_fhir::{
    install_builtin_terminology, install_package_dir, install_prepared, minir4_dir, share_catalog,
    write_prepared, Catalog, ConformanceStore, FhirError, PackageFetcher,
};
use refhir_postgres::test_support::with_database as database_url;
use refhir_postgres::{PostgresPersistence, SharedConnections, SharedOptions};

#[global_allocator]
static COUNTING: Counting = Counting;

struct NoNetwork;

impl PackageFetcher for NoNetwork {
    fn fetch(&self, name: &str, version: &str, _dest: &Path) -> Result<(), FhirError> {
        panic!("{name}@{version} was fetched, and nothing here may fetch");
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

struct Tenant {
    store: PostgresPersistence,
    db: Arc<Database>,
    catalog: RwLock<Catalog>,
}

fn tenant(shared: &Arc<SharedConnections>, name: &str) -> Tenant {
    let store = PostgresPersistence::open_tenant(shared, name, 2).unwrap();
    let db = Arc::new(Database::open(Arc::new(store.clone())).unwrap());
    Tenant {
        store,
        db,
        catalog: RwLock::new(Catalog::empty()),
    }
}

fn parameter_count(catalog: &Catalog) -> usize {
    catalog
        .resource_models()
        .map(|model| model.params.len())
        .sum()
}

fn admin_query<T: for<'a> postgres::types::FromSql<'a>>(url: &str, sql: &str) -> T {
    Client::connect(url, NoTls)
        .unwrap()
        .query_one(sql, &[])
        .unwrap()
        .get(0)
}

#[test]
fn tenants_with_the_same_definitions_hold_one_catalog_and_a_change_is_a_tenants_own() {
    let admin_url = match std::env::var("REFHIR_TEST_POSTGRES_URL") {
        Ok(url) => url,
        Err(_) if std::env::var("REFHIR_REQUIRE_POSTGRES").is_ok() => {
            panic!("REFHIR_REQUIRE_POSTGRES is set but REFHIR_TEST_POSTGRES_URL is not")
        }
        Err(_) => {
            eprintln!("REFHIR_TEST_POSTGRES_URL unset, skipping the shared definitions test");
            return;
        }
    };
    let scratch = std::env::temp_dir().join(format!("refhir-shared-core-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    // The fixture, marked as downloaded from a registry: it has a source digest, so it is shared.
    let package = scratch.join("package");
    copy_dir(minir4_dir(), &package);
    std::fs::write(package.join(".complete"), "ab".repeat(32)).unwrap();
    // A tenant that writes terminology of its own needs the type to write it as.
    std::fs::write(
        package.join("StructureDefinition-CodeSystem.json"),
        serde_json::json!({
            "resourceType": "StructureDefinition",
            "url": "http://hl7.org/fhir/StructureDefinition/CodeSystem",
            "name": "CodeSystem", "status": "active", "kind": "resource", "abstract": false,
            "type": "CodeSystem", "fhirVersion": "4.0.1", "derivation": "specialization",
            "snapshot": {"element": [
                {"path": "CodeSystem", "min": 0, "max": "*"},
                {"path": "CodeSystem.url", "min": 0, "max": "1", "type": [{"code": "uri"}]},
                {"path": "CodeSystem.status", "min": 1, "max": "1", "type": [{"code": "code"}]},
                {"path": "CodeSystem.content", "min": 1, "max": "1", "type": [{"code": "code"}]},
                {"path": "CodeSystem.concept", "min": 0, "max": "*",
                 "type": [{"code": "BackboneElement"}]},
                {"path": "CodeSystem.concept.code", "min": 1, "max": "1",
                 "type": [{"code": "code"}]}
            ]}
        })
        .to_string(),
    )
    .unwrap();
    let bundle = scratch.join("packages.bundle");
    let names = write_prepared(
        &bundle,
        &[package.to_str().unwrap().to_string()],
        &NoNetwork,
        &scratch.join("cache"),
    )
    .unwrap();
    let specs: Vec<&str> = names.iter().map(String::as_str).collect();

    let database = format!("refhir_shared_core_{}", std::process::id());
    let mut admin = Client::connect(&admin_url, NoTls).expect("admin connection");
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {database}"))
        .unwrap();
    admin
        .batch_execute(&format!("CREATE DATABASE {database}"))
        .unwrap();
    let url = database_url(&admin_url, &database);
    for name in ["a", "b", "c", "d"] {
        refhir_postgres::create_tenant(&url, name).unwrap();
    }
    let shared = SharedConnections::connect(
        &url,
        &SharedOptions {
            pool_size: 4,
            create_schema: false,
        },
    )
    .unwrap();

    // Two tenants given the shared packages, and one that installed a package of its own.
    let a = tenant(&shared, "a");
    let b = tenant(&shared, "b");
    let c = tenant(&shared, "c");
    for t in [&a, &b] {
        t.catalog.write().unwrap().set_rebuild_in_place(true);
        let installed = install_prepared(&t.store, &t.db, &t.catalog, &bundle, &specs).unwrap();
        assert_eq!(installed, Some(names.len()));
    }
    install_package_dir(&c.store, &c.db, &c.catalog, minir4_dir()).unwrap();
    // What a server does once it has finished installing: the built-in terminology, which is a
    // package every tenant is given the one copy of, and then the catalog is put with the others'.
    for t in [&a, &b] {
        install_builtin_terminology(&t.store, &t.db, &t.catalog).unwrap();
        assert!(share_catalog(&t.store, &t.catalog).unwrap());
    }
    assert!(
        !share_catalog(&c.store, &c.catalog).unwrap(),
        "c has a package of its own"
    );
    assert_eq!(
        admin_query::<i64>(
            &url,
            "SELECT count(*) FROM packages WHERE name = 'refhir.builtin.terminology'"
        ),
        1,
        "the built-in terminology is stored once for the tenants"
    );
    // The catalogs the installs left in place are the same one, for the two that were given the
    // same packages.
    assert!(a
        .catalog
        .read()
        .unwrap()
        .shares_definitions_with(&b.catalog.read().unwrap()));
    assert!(!a
        .catalog
        .read()
        .unwrap()
        .shares_definitions_with(&c.catalog.read().unwrap()));

    // The store says the same of both, and says nothing of the tenant with a package of its own.
    let key_a = a.store.shared_catalog().unwrap().expect("a shares").key;
    let key_b = b.store.shared_catalog().unwrap().expect("b shares").key;
    assert_eq!(key_a, key_b);
    assert!(c.store.shared_catalog().unwrap().is_none());

    // The first catalog is compiled, and the second is the same one: it keeps next to nothing.
    // (What the installs left in place is let go first, so that nothing holds one yet.)
    for t in [&a, &b, &c] {
        *t.catalog.write().unwrap() = Catalog::empty();
    }
    let first = measure(|| Catalog::open(&a.store).unwrap());
    let second = measure(|| Catalog::open(&b.store).unwrap());
    let (a_catalog, b_catalog) = (first.value, second.value);
    assert!(a_catalog.shares_definitions_with(&b_catalog));
    // The second took nothing and compiled nothing: what the first allocated to read and compile
    // the definitions, the second did not (the fixture is small, so what the first keeps is within
    // what other threads allocate and free meanwhile, and the most it held at once is what shows).
    assert!(
        second.peak * 20 < first.peak,
        "the second tenant's catalog peaked at {} KiB, the first at {} KiB",
        second.peak >> 10,
        first.peak >> 10
    );
    eprintln!(
        "first catalog peaked at {} KiB and kept {} B; the second peaked at {} KiB and kept {} B",
        first.peak >> 10,
        first.kept,
        second.peak >> 10,
        second.kept
    );
    // A tenant with a package of its own has a catalog of its own, with the same types.
    let c_catalog = Catalog::open(&c.store).unwrap();
    assert!(!c_catalog.shares_definitions_with(&a_catalog));
    assert!(c_catalog.get("Patient").is_ok());

    // What a primary does at its start, which makes the tenant a package of its own that holds
    // nothing, leaves its catalog the same one.
    a.store.ensure_local_package("4.0.1").unwrap();
    let a_again = Catalog::open(&a.store).unwrap();
    assert!(a_again.shares_definitions_with(&b_catalog));
    assert_eq!(a.store.shared_catalog().unwrap().unwrap().key, key_b);

    // A change one tenant makes to its catalog is its own: the other tenant's has every
    // search parameter it had.
    let parameters = parameter_count(&b_catalog);
    assert!(parameters > 0);
    let mut changed = a_catalog.clone();
    assert!(changed.shares_definitions_with(&b_catalog));
    changed.recompile_parameters(&[]);
    assert_eq!(parameter_count(&changed), 0);
    assert!(!changed.shares_definitions_with(&b_catalog));
    assert_eq!(parameter_count(&b_catalog), parameters);
    assert_eq!(parameter_count(&a_catalog), parameters);
    assert!(a_catalog.shares_definitions_with(&b_catalog));

    // A tenant whose own rows differ from what the others hold does not share their catalog: one
    // of its search parameters waits for an index, so its key is another, and it has a catalog of
    // its own (that other tenants in the same state would share).
    let tenant_id: i32 = admin_query(&url, "SELECT id FROM refhir_tenants WHERE name = 'b'");
    let mut db = Client::connect(&url, NoTls).unwrap();
    db.execute(
        "UPDATE search_parameters SET status = 'pending' WHERE instance_id = $1 \
         AND ctid = (SELECT ctid FROM search_parameters WHERE instance_id = $1 LIMIT 1)",
        &[&tenant_id],
    )
    .unwrap();
    let pending = b
        .store
        .shared_catalog()
        .unwrap()
        .expect("still made of what is shared");
    assert_ne!(pending.key, key_b);
    let b_alone = Catalog::open(&b.store).unwrap();
    assert!(!b_alone.shares_definitions_with(&a_catalog));
    drop(b_alone);
    db.execute(
        "UPDATE search_parameters SET status = 'ready' WHERE instance_id = $1",
        &[&tenant_id],
    )
    .unwrap();
    assert_eq!(b.store.shared_catalog().unwrap().unwrap().key, key_b);

    // One that holds an expansion the shared copy does not, as when it expanded a value set that a
    // package it was given brought something to, shares with the tenants that hold the same: the
    // catalog carries it. One that lacks an expansion the shared copy holds is not made of what is
    // shared at all.
    let package_id: i64 = admin_query(
        &url,
        "SELECT id FROM packages WHERE instance_id = 0 ORDER BY id LIMIT 1",
    );
    db.execute(
        "INSERT INTO value_set_expansion (instance_id, package_id, url, json) \
         VALUES ($1, $2, 'urn:test:only-b', '{}')",
        &[&tenant_id, &package_id],
    )
    .unwrap();
    let with_extra = b
        .store
        .shared_catalog()
        .unwrap()
        .expect("shares, with one more");
    assert_ne!(with_extra.key, key_b);
    assert_eq!(
        with_extra
            .source
            .load_expansion_json(package_id, "urn:test:only-b")
            .unwrap()
            .as_deref(),
        Some("{}")
    );
    db.execute(
        "DELETE FROM value_set_expansion WHERE instance_id = $1 AND url = 'urn:test:only-b'",
        &[&tenant_id],
    )
    .unwrap();
    // The shared copy is written only by whoever builds a shared package, which says so.
    db.batch_execute("SET refhir.shared_writer = 'on'").unwrap();
    db.execute(
        "INSERT INTO base_expansions (package_id, url, json) VALUES ($1, 'urn:test:base-only', '{}')",
        &[&package_id],
    )
    .unwrap();
    assert!(b.store.shared_catalog().unwrap().is_none());
    db.execute(
        "DELETE FROM base_expansions WHERE url = 'urn:test:base-only'",
        &[],
    )
    .unwrap();
    assert_eq!(b.store.shared_catalog().unwrap().unwrap().key, key_b);

    // Whether a catalog can be shared is only a saving: a store that cannot say (here the shared
    // copy cannot be read) gives the tenant a catalog of its own and no error.
    db.batch_execute("ALTER TABLE base_expansions RENAME TO base_expansions_away")
        .unwrap();
    assert!(b.store.shared_catalog().is_err());
    let alone = Catalog::open(&b.store);
    db.batch_execute("ALTER TABLE base_expansions_away RENAME TO base_expansions")
        .unwrap();
    let alone = alone.expect("the catalog was not opened because the store could not say");
    assert!(!alone.shares_definitions_with(&b_catalog));
    drop(alone);

    // A tenant that writes terminology of its own through the API leaves the shared catalog, and
    // the catalog it has afterwards reads the tenant's own rows and not the shared ones: it finds
    // what it wrote after what it had read was let go, which the shared source could not answer.
    let d = tenant(&shared, "d");
    d.catalog.write().unwrap().set_rebuild_in_place(true);
    install_prepared(&d.store, &d.db, &d.catalog, &bundle, &specs).unwrap();
    install_builtin_terminology(&d.store, &d.db, &d.catalog).unwrap();
    assert!(share_catalog(&d.store, &d.catalog).unwrap());
    assert!(d.catalog.read().unwrap().reads_shared_rows());
    assert!(d
        .catalog
        .read()
        .unwrap()
        .shares_definitions_with(&b_catalog));
    let written = refhir_fhir::commit_operation(
        &d.store,
        &d.db,
        &d.catalog,
        "CodeSystem/create",
        &serde_json::json!({"resource": {
            "resourceType": "CodeSystem", "url": "http://example.org/d-own", "status": "active",
            "content": "complete", "concept": [{"code": "one"}]
        }}),
    );
    written.unwrap();
    assert!(!d
        .catalog
        .read()
        .unwrap()
        .shares_definitions_with(&b_catalog));
    assert!(
        !d.catalog.read().unwrap().reads_shared_rows(),
        "the catalog still reads the rows tenants share"
    );
    assert!(b_catalog.reads_shared_rows());
    assert!(d.store.shared_catalog().unwrap().is_none());
    assert!(b_catalog.get("Patient").is_ok());
    drop(d);

    // The catalog reads through a source that belongs to no tenant, so it goes on working when
    // the tenant it was first made from is deleted.
    let shared_source = a.store.shared_catalog().unwrap().unwrap().source;
    drop((a_catalog, a_again, changed));
    drop(a);
    refhir_postgres::delete_tenant(&url, "a").unwrap();
    let definition = shared_source
        .load_structure_definition_json(
            package_id,
            "http://hl7.org/fhir/StructureDefinition/Patient",
        )
        .unwrap();
    assert!(
        definition.is_some(),
        "the shared source depended on the deleted tenant"
    );
    assert!(b_catalog.get("Patient").is_ok());

    // A tenant that opens a catalog now is given the one the tenants still serving hold.
    let rebuilt = Catalog::open(&b.store).unwrap();
    assert!(rebuilt.shares_definitions_with(&b_catalog));
    drop((b_catalog, c_catalog, rebuilt));

    drop((b, c, shared));
    let _ = admin.batch_execute(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"));
    let _ = std::fs::remove_dir_all(scratch);
}
