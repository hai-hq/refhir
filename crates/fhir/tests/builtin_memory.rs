//! The code systems a server stores itself (ISO 4217) must not raise what a start needs.
//!
//! A database that has the release's packages is given them once, by a publish that rebuilds the
//! catalog. The peak of that is the catalog's own, and the catalog is the size it was, so the most
//! a start holds is where it was; and a start that finds them stored does nothing at all. (A first
//! start stores them before the packages, with no publish of their own, and so rebuilds nothing
//! more than it did.) This is a binary of its own, with one test, so that the allocator it counts
//! is the only thing running.

mod common;

use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use common::{measure, mib, Counting};
use refhir_database::Database;
use refhir_fhir::{
    install_builtin_terminology, install_prepared, r4_dir, write_prepared, Catalog, FhirError,
    PackageFetcher,
};
use refhir_sqlite::SqlitePersistence;

#[global_allocator]
static COUNTING: Counting = Counting;

struct NoNetwork;

impl PackageFetcher for NoNetwork {
    fn fetch(&self, name: &str, version: &str, _dest: &Path) -> Result<(), FhirError> {
        panic!("{name}@{version} was fetched, and nothing here may fetch");
    }
}

#[test]
fn storing_the_builtin_code_systems_takes_less_than_installing_the_packages() {
    let scratch =
        std::env::temp_dir().join(format!("refhir-builtin-memory-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let file = scratch.join("packages.bundle");
    let names = write_prepared(
        &file,
        &[r4_dir().to_str().unwrap().to_string()],
        &NoNetwork,
        &scratch.join("cache"),
    )
    .unwrap();

    let store = SqlitePersistence::open_in_memory().unwrap();
    let db = Database::open(Arc::new(store.clone())).unwrap();
    let catalog = RwLock::new(Catalog::empty());
    let specs: Vec<&str> = names.iter().map(String::as_str).collect();
    // As when a server starts: nothing serves, so a catalog is rebuilt in place.
    catalog.write().unwrap().set_rebuild_in_place(true);
    let started = Instant::now();
    let installed = measure(|| install_prepared(&store, &db, &catalog, &file, &specs).unwrap());
    let install_time = started.elapsed();
    assert_eq!(installed.value, Some(names.len()));

    let started = Instant::now();
    let added = measure(|| install_builtin_terminology(&store, &db, &catalog).unwrap());
    let add_time = started.elapsed();
    assert!(added.value, "they were stored");
    println!(
        "packages: peak {} MiB, kept {} MiB, {install_time:.1?}; built-in code systems: peak {} MiB, kept {} MiB, {add_time:.1?}",
        mib(installed.peak),
        mib(installed.kept),
        mib(added.peak),
        mib(added.kept)
    );
    // Publishing again rebuilds a catalog that is the size of the one before, so it peaks no
    // higher than installing the packages did, and what it keeps is what a catalog is.
    assert!(
        added.peak <= installed.peak,
        "storing the built-in code systems peaked at {} MiB, above the {} MiB of installing the packages",
        mib(added.peak),
        mib(installed.peak)
    );
    // A start that finds them stored does nothing.
    let again = measure(|| install_builtin_terminology(&store, &db, &catalog).unwrap());
    assert!(!again.value);
    assert!(again.peak < 1 << 20, "{} bytes", again.peak);
    let _ = std::fs::remove_dir_all(scratch);
}
