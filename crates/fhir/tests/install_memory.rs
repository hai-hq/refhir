//! Installing a package from a prepared file must not hold the package.
//!
//! The largest of the default R4 packages is over a hundred megabytes of JSON, and a server that
//! read it whole before storing it needed that much memory to start. The file holds a package as
//! values of a few megabytes of rows and the store takes them as they are read, so what is held
//! at once is a few rows. This is a binary of its own, with one test, so that the allocator it
//! counts is the only thing running.

mod common;

use std::path::Path;
use std::sync::{Arc, RwLock};

use common::{measure, mib, Counting};
use refhir_database::Database;
use refhir_fhir::{
    install_prepared, r4_dir, write_prepared, Catalog, ConformanceStore, FhirError, PackageFetcher,
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
fn installing_a_prepared_package_takes_a_fraction_of_the_package() {
    let scratch =
        std::env::temp_dir().join(format!("refhir-install-memory-{}", std::process::id()));
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
    // Nothing serves while the packages are installed, as when a server starts: the catalog that
    // is rebuilt does not sit on top of the one it replaces.
    catalog.write().unwrap().set_rebuild_in_place(true);
    let installed = measure(|| install_prepared(&store, &db, &catalog, &file, &specs).unwrap());
    assert_eq!(installed.value, Some(names.len()));

    // What the package is, as text: what holding it whole would hold, and more.
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
    // Reading it whole took the text and more. Reading it a few rows at a time takes what the
    // rows and the catalog it ends in take, which is under the text: this package is the
    // smallest the server installs, and the catalog it ends in is a quarter of the text.
    assert!(
        installed.peak < text * 3 / 4,
        "installing {} MiB of package peaked at {} MiB (and kept {} MiB)",
        mib(text),
        mib(installed.peak),
        mib(installed.kept)
    );
    let _ = std::fs::remove_dir_all(scratch);
}
