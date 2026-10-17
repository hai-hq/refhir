//! A prepared package installed for two tenants of one Postgres database is stored once.
//!
//! Against a database of its own on the server named by `REFHIR_TEST_POSTGRES_URL`, which is
//! skipped when that is not set (and fails when `REFHIR_REQUIRE_POSTGRES` is). The package is the
//! small R4 fixture, given the source digest that a package downloaded from a registry carries,
//! because only a package whose source is known may be shared.

use std::path::Path;
use std::sync::{Arc, RwLock};

use postgres::{Client, NoTls};
use refhir_database::Database;
use refhir_fhir::{
    install_prepared, minir4_dir, write_prepared, Catalog, ConformanceStore, FhirError,
    PackageFetcher,
};
use refhir_postgres::{ConnectOptions, PostgresPersistence};

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

use refhir_postgres::test_support::with_database as database_url;

fn count(url: &str, sql: &str) -> i64 {
    Client::connect(url, NoTls)
        .unwrap()
        .query_one(sql, &[])
        .unwrap()
        .get(0)
}

struct Started {
    store: PostgresPersistence,
    db: Arc<Database>,
    catalog: RwLock<Catalog>,
}

fn start(url: &str, tenant: &str) -> Started {
    let store = PostgresPersistence::connect_with(
        url,
        &ConnectOptions {
            pool_size: 2,
            tenant: Some(tenant.to_string()),
            ..ConnectOptions::default()
        },
    )
    .unwrap();
    let db = Arc::new(Database::open(Arc::new(store.clone())).unwrap());
    Started {
        store,
        db,
        catalog: RwLock::new(Catalog::empty()),
    }
}

#[test]
fn a_second_tenant_is_given_the_packages_the_first_installed() {
    let admin_url = match std::env::var("REFHIR_TEST_POSTGRES_URL") {
        Ok(url) => url,
        Err(_) if std::env::var("REFHIR_REQUIRE_POSTGRES").is_ok() => {
            panic!("REFHIR_REQUIRE_POSTGRES is set but REFHIR_TEST_POSTGRES_URL is not")
        }
        Err(_) => {
            eprintln!("REFHIR_TEST_POSTGRES_URL unset, skipping the shared catalog test");
            return;
        }
    };
    let scratch =
        std::env::temp_dir().join(format!("refhir-shared-catalog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    // The fixture, marked as downloaded from a registry: it has a source digest.
    let package = scratch.join("package");
    copy_dir(minir4_dir(), &package);
    std::fs::write(package.join(".complete"), "ab".repeat(32)).unwrap();
    let bundle = scratch.join("packages.bundle");
    let names = write_prepared(
        &bundle,
        &[package.to_str().unwrap().to_string()],
        &NoNetwork,
        &scratch.join("cache"),
    )
    .unwrap();
    let specs: Vec<&str> = names.iter().map(String::as_str).collect();

    let database = format!("refhir_shared_catalog_{}", std::process::id());
    let mut admin = Client::connect(&admin_url, NoTls).expect("admin connection");
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {database}"))
        .unwrap();
    admin
        .batch_execute(&format!("CREATE DATABASE {database}"))
        .unwrap();
    let url = database_url(&admin_url, &database);
    for tenant in ["a", "b"] {
        refhir_postgres::create_tenant(&url, tenant).unwrap();
    }

    let a = start(&url, "a");
    a.catalog.write().unwrap().set_rebuild_in_place(true);
    let installed = install_prepared(&a.store, &a.db, &a.catalog, &bundle, &specs).unwrap();
    assert_eq!(installed, Some(names.len()));
    let shared_definitions = count(
        &url,
        "SELECT count(*) FROM structure_definitions WHERE instance_id = 0",
    );
    assert!(
        shared_definitions > 0,
        "the first tenant builds the shared copy"
    );
    assert_eq!(
        count(
            &url,
            "SELECT count(*) FROM structure_definitions WHERE instance_id <> 0"
        ),
        0
    );

    // The second tenant is given the packages: no definition or terminology is stored for it, and
    // what is stored is its own search parameters and expansions.
    let terminology_before = count(&url, "SELECT count(*) FROM terminology");
    let packages_before = count(&url, "SELECT count(*) FROM packages");
    let b = start(&url, "b");
    b.catalog.write().unwrap().set_rebuild_in_place(true);
    let installed = install_prepared(&b.store, &b.db, &b.catalog, &bundle, &specs).unwrap();
    assert_eq!(installed, Some(names.len()));
    assert_eq!(
        count(&url, "SELECT count(*) FROM structure_definitions"),
        shared_definitions,
        "the second tenant stored definitions of its own"
    );
    assert_eq!(
        count(&url, "SELECT count(*) FROM terminology"),
        terminology_before
    );
    assert_eq!(
        count(&url, "SELECT count(*) FROM packages"),
        packages_before,
        "the second tenant stored a package of its own"
    );
    assert_eq!(
        count(
            &url,
            "SELECT count(*) FROM tenant_packages WHERE instance_id > 0"
        ),
        2 * names.len() as i64
    );

    // Both tenants serve the same catalog, with their search indexes ready.
    for tenant in [&a, &b] {
        let packages = tenant.store.load_packages().unwrap();
        assert_eq!(packages.len(), names.len());
        let ready = tenant.store.load_search_parameters().unwrap();
        assert!(!ready.is_empty());
        assert!(
            ready
                .iter()
                .all(|row| row.status == refhir_persistence::ParamStatus::Ready),
            "a tenant's search parameters wait for an index nothing is building"
        );
        let whole = Catalog::open_whole(&tenant.store).unwrap();
        assert!(whole.get("Patient").is_ok());
    }
    let rows = |store: &PostgresPersistence| -> Vec<(String, String)> {
        store
            .load_structure_definitions()
            .unwrap()
            .into_iter()
            .map(|row| (row.url, row.json))
            .collect()
    };
    assert_eq!(rows(&a.store), rows(&b.store));

    drop((a, b));
    let _ = admin.batch_execute(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"));
    let _ = std::fs::remove_dir_all(scratch);
}
