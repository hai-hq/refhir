//! What it takes to give a tenant the packages that the tenants of a database share.
//!
//! Against a database of its own on the server named by `REFHIR_TEST_POSTGRES_URL`, skipped when
//! that is not set (and failing when `REFHIR_REQUIRE_POSTGRES` is). A tenant made after the first
//! is given the release's packages from the copy the first one stored, without reading them from
//! the prepared file and without compiling the catalog again: this is what makes a tenant of a
//! process that serves many of them available in moments, and what these keep.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use postgres::{Client, NoTls};
use refhir_database::Database;
use refhir_fhir::{
    install_builtin_terminology, install_prepared, minir4_dir, retain_shared_catalogs,
    share_catalog, shared_catalogs_compiled, write_prepared, Catalog, FhirError, PackageFetcher,
};
use refhir_postgres::test_support::with_database as database_url;
use refhir_postgres::{PostgresPersistence, SharedConnections, SharedOptions};

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

/// A tenant as the host opens one: its catalog is rebuilt in place and nothing is read of its own
/// rows until it settles which catalog it holds.
fn starting(shared: &Arc<SharedConnections>, name: &str) -> Tenant {
    let store = PostgresPersistence::open_tenant(shared, name, 2).unwrap();
    let db = Arc::new(Database::open(Arc::new(store.clone())).unwrap());
    let catalog = RwLock::new(Catalog::empty());
    {
        let mut starting = catalog.write().unwrap();
        starting.set_rebuild_in_place(true);
        starting.set_detach_deferred(true);
    }
    Tenant { store, db, catalog }
}

/// `bundle` with its packages' rows replaced by something that is not a value of the format: the
/// header, which lists the packages and what each was made from, is all that can be read of it.
fn header_only(bundle: &Path, out: &Path) {
    let mut text = Vec::new();
    GzDecoder::new(std::fs::File::open(bundle).unwrap())
        .read_to_end(&mut text)
        .unwrap();
    let header_end = text.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    let mut encoder = GzEncoder::new(std::fs::File::create(out).unwrap(), Compression::fast());
    encoder.write_all(&text[..header_end]).unwrap();
    encoder.write_all(b"{ this is not a package").unwrap();
    encoder.finish().unwrap();
}

#[test]
fn a_tenant_made_after_the_first_reads_no_package_and_compiles_no_catalog() {
    let admin_url = match std::env::var("REFHIR_TEST_POSTGRES_URL") {
        Ok(url) => url,
        Err(_) if std::env::var("REFHIR_REQUIRE_POSTGRES").is_ok() => {
            panic!("REFHIR_REQUIRE_POSTGRES is set but REFHIR_TEST_POSTGRES_URL is not")
        }
        Err(_) => {
            eprintln!("REFHIR_TEST_POSTGRES_URL unset, skipping the shared install test");
            return;
        }
    };
    let scratch =
        std::env::temp_dir().join(format!("refhir-shared-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    // The fixture, marked as downloaded from a registry: it has a source digest, so it is shared.
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
    let unreadable = scratch.join("unreadable.bundle");
    header_only(&bundle, &unreadable);

    let database = format!("refhir_shared_install_{}", std::process::id());
    let mut admin = Client::connect(&admin_url, NoTls).expect("admin connection");
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {database}"))
        .unwrap();
    admin
        .batch_execute(&format!("CREATE DATABASE {database}"))
        .unwrap();
    let url = database_url(&admin_url, &database);
    let tenants = ["a", "b", "c", "d"];
    for name in tenants {
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
    // What a host that serves many tenants does.
    retain_shared_catalogs(Duration::from_secs(300));

    // What each tenant does to open, as the host does it.
    let open = |t: &Tenant, from: &Path| {
        let installed = install_prepared(&t.store, &t.db, &t.catalog, from, &specs).unwrap();
        assert_eq!(installed, Some(names.len()));
        install_builtin_terminology(&t.store, &t.db, &t.catalog).unwrap();
        assert!(share_catalog(&t.store, &t.catalog).unwrap());
    };

    // The first tenant reads the file, stores the packages for everyone and compiles the catalog.
    let a = starting(&shared, "a");
    open(&a, &bundle);
    let after_first = shared_catalogs_compiled();
    assert!(after_first > 0, "the first tenant compiled the catalog");

    // The tenants after it are given the packages from that copy: a file that cannot be read past
    // its header is as good as the whole one, and nothing is compiled that is compiled already.
    for name in &tenants[1..] {
        let t = starting(&shared, name);
        open(&t, &unreadable);
        assert!(t
            .catalog
            .read()
            .unwrap()
            .shares_definitions_with(&a.catalog.read().unwrap()));
        assert_eq!(
            shared_catalogs_compiled(),
            after_first,
            "tenant {name} compiled a catalog that the first had compiled"
        );
    }

    // A file that does hold what a tenant needs and cannot be read is still refused: nothing of
    // it is given away unseen.
    let e = {
        refhir_postgres::create_tenant(&url, "e").unwrap();
        starting(&shared, "e")
    };
    let other = scratch.join("other");
    copy_dir(minir4_dir(), &other);
    std::fs::write(
        other.join("package.json"),
        r#"{"name":"example.other","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    std::fs::write(other.join(".complete"), "cd".repeat(32)).unwrap();
    let different = scratch.join("different.bundle");
    let names = write_prepared(
        &different,
        &[other.to_str().unwrap().to_string()],
        &NoNetwork,
        &scratch.join("cache"),
    )
    .unwrap();
    let unreadable_other = scratch.join("unreadable-other.bundle");
    header_only(&different, &unreadable_other);
    let specs: Vec<&str> = names.iter().map(String::as_str).collect();
    let refused = install_prepared(&e.store, &e.db, &e.catalog, &unreadable_other, &specs);
    assert!(
        refused.is_err(),
        "a package that is not stored yet is read from the file, which here cannot be read"
    );
}
