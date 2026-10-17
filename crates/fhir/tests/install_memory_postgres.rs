//! Installing a prepared package into Postgres must not hold the package either.
//!
//! The same as `install_memory`, against a database of its own on the server named by
//! `REFHIR_TEST_POSTGRES_URL`, which is skipped when that is not set (and fails when
//! `REFHIR_REQUIRE_POSTGRES` is). The packages are the R4 fixture, or those of the prepared file
//! `REFHIR_MEASURE_BUNDLE` names, which is how the default packages are measured: the largest of
//! them is over a hundred megabytes of JSON. This is a binary of its own, with one test, so that
//! the allocator it counts is the only thing running.

mod common;

use std::path::Path;
use std::sync::{Arc, RwLock};

use common::{measure, mib, Counting};
use postgres::{Client, NoTls};
use refhir_database::Database;
use refhir_fhir::{
    install_prepared, r4_dir, write_prepared, Catalog, ConformanceStore, FhirError, PackageFetcher,
    PreparedBundle,
};
use refhir_postgres::PostgresPersistence;

#[global_allocator]
static COUNTING: Counting = Counting;

struct NoNetwork;

impl PackageFetcher for NoNetwork {
    fn fetch(&self, name: &str, version: &str, _dest: &Path) -> Result<(), FhirError> {
        panic!("{name}@{version} was fetched, and nothing here may fetch");
    }
}

/// `admin_url` with its database replaced by `name`.
fn database_url(admin_url: &str, name: &str) -> String {
    if admin_url.contains("://") {
        let (head, tail) = match admin_url.split_once('?') {
            Some((head, query)) => (head, format!("?{query}")),
            None => (admin_url, String::new()),
        };
        let slash = head.rfind('/').expect("database name in the url");
        format!("{}/{name}{tail}", &head[..slash])
    } else {
        let kept: Vec<&str> = admin_url
            .split_whitespace()
            .filter(|part| !part.starts_with("dbname="))
            .collect();
        format!("{} dbname={name}", kept.join(" "))
    }
}

#[test]
fn installing_a_prepared_package_into_postgres_takes_a_fraction_of_the_package() {
    let admin_url = match std::env::var("REFHIR_TEST_POSTGRES_URL") {
        Ok(url) => url,
        Err(_) if std::env::var("REFHIR_REQUIRE_POSTGRES").is_ok() => {
            panic!("REFHIR_REQUIRE_POSTGRES is set but REFHIR_TEST_POSTGRES_URL is not")
        }
        Err(_) => {
            eprintln!("REFHIR_TEST_POSTGRES_URL unset, skipping the Postgres install memory test");
            return;
        }
    };
    let scratch =
        std::env::temp_dir().join(format!("refhir-install-memory-pg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let (file, names, real) = match std::env::var("REFHIR_MEASURE_BUNDLE") {
        Ok(path) => {
            let file = Path::new(&path).to_path_buf();
            let names = PreparedBundle::open(&file).unwrap().names().to_vec();
            (file, names, true)
        }
        Err(_) => {
            let file = scratch.join("packages.bundle");
            let names = write_prepared(
                &file,
                &[r4_dir().to_str().unwrap().to_string()],
                &NoNetwork,
                &scratch.join("cache"),
            )
            .unwrap();
            (file, names, false)
        }
    };

    // A database that has no packages, as a first start finds it.
    let database = format!("refhir_install_memory_{}", std::process::id());
    let mut admin = Client::connect(&admin_url, NoTls).expect("admin connection");
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {database}"))
        .unwrap();
    admin
        .batch_execute(&format!("CREATE DATABASE {database}"))
        .unwrap();
    let store = PostgresPersistence::connect(&database_url(&admin_url, &database)).unwrap();
    let db = Database::open(Arc::new(store.clone())).unwrap();
    let catalog = RwLock::new(Catalog::empty());
    let specs: Vec<&str> = names.iter().map(String::as_str).collect();
    // Nothing serves while the packages are installed, as when a server starts: the catalog that
    // is rebuilt does not sit on top of the one it replaces.
    catalog.write().unwrap().set_rebuild_in_place(true);
    let installed = measure(|| install_prepared(&store, &db, &catalog, &file, &specs).unwrap());
    assert_eq!(installed.value, Some(names.len()));
    eprintln!(
        "installed {} package(s) into Postgres: kept {} MiB, peak {} MiB",
        names.len(),
        mib(installed.kept),
        mib(installed.peak)
    );

    if real {
        // What the default packages take to install is what the largest of them used to take
        // to hold: over a hundred megabytes of JSON, read whole by a store that could not take
        // it a few rows at a time.
        assert!(
            installed.peak < 80 << 20,
            "installing the packages of {} peaked at {} MiB (and kept {} MiB)",
            file.display(),
            mib(installed.peak),
            mib(installed.kept)
        );
    } else {
        // What the package is, as text: what holding it whole would hold, and more. The catalog
        // this ends in holds the base types and what they bind (17 MiB here, and about as much
        // for a package ten times this size), so the bound is the text itself and not a fraction
        // of it; the real packages above are the strict check.
        let text: usize = store
            .load_structure_definitions()
            .unwrap()
            .iter()
            .map(|row| row.json.len())
            .chain(
                store
                    .load_terminology()
                    .unwrap()
                    .iter()
                    .map(|row| row.json.len()),
            )
            .chain(
                store
                    .load_expansions()
                    .unwrap()
                    .iter()
                    .map(|row| row.json.len()),
            )
            .chain(
                store
                    .load_search_parameters()
                    .unwrap()
                    .iter()
                    .map(|row| row.json.len()),
            )
            .sum();
        assert!(text > 20 << 20, "the package is only {} MiB", mib(text));
        assert!(
            installed.peak < text,
            "installing {} MiB of package peaked at {} MiB (and kept {} MiB)",
            mib(text),
            mib(installed.peak),
            mib(installed.kept)
        );
    }

    drop((db, store, catalog));
    let _ = admin.batch_execute(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"));
    let _ = std::fs::remove_dir_all(scratch);
}
