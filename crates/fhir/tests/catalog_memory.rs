//! Loading the catalog must not hold every parsed document at once, and a catalog must not hold
//! every definition and code system either.
//!
//! A parsed StructureDefinition or code system is several times the size of its text. Loading used
//! to parse the whole conformance store and keep all of it until the catalog was built, so a start
//! needed several times the memory the running server did, and a server given a limit for the
//! second was killed in the first. Loading a page at a time brought the peak down to a small
//! multiple of what the catalog kept; the catalog now keeps an index of the definitions and
//! terminology and reads each when it is first used, so what it keeps is a small part of that.
//! This is a binary of its own, with one test, so that the allocator it counts is the only thing
//! running.

mod common;

use std::sync::{Arc, RwLock};

use common::{measure, mib, Counting};
use refhir_database::Database;
use refhir_fhir::{install_package_dir, r4_dir, Catalog};
use refhir_sqlite::SqlitePersistence;

#[global_allocator]
static COUNTING: Counting = Counting;

#[test]
fn a_catalog_keeps_an_index_and_loading_it_takes_a_fraction_of_holding_everything() {
    let store = SqlitePersistence::open_in_memory().unwrap();
    let db = Database::open(Arc::new(store.clone())).unwrap();
    // The whole R4 definition set, as a server stores it.
    let installed = RwLock::new(Catalog::empty());
    install_package_dir(&store, &db, &installed, r4_dir()).unwrap();
    drop(installed);

    // Reading and compiling every definition and code system, and holding all of it.
    let whole = measure(|| Catalog::open_whole(&store).unwrap());
    drop(whole.value);
    // Reading what a request needs when it needs it.
    let lazy = measure(|| Catalog::open(&store).unwrap());
    let read_when_used = lazy.value;

    // Loading everything as one piece took about seven times what the catalog keeps, and
    // parsing a table at a time about three. A page at a time takes about one and a third.
    assert!(
        whole.peak < whole.kept * 2,
        "loading the whole catalog peaked at {} MiB to keep {} MiB",
        mib(whole.peak),
        mib(whole.kept)
    );
    eprintln!(
        "whole: kept {} KiB, peak {} KiB; read when used: kept {} KiB, peak {} KiB",
        whole.kept >> 10,
        whole.peak >> 10,
        lazy.kept >> 10,
        lazy.peak >> 10
    );
    // What it keeps is an index and what every request needs: the base types, the value sets
    // their bindings check a code against, and the code systems behind those. For R4 core alone,
    // which is mostly that, it is well under what holding everything keeps; for the default
    // packages, which are mostly profiles and extensions, it is about a quarter.
    assert!(
        lazy.kept * 10 < whole.kept * 7,
        "the catalog that reads when used keeps {} MiB, the whole one {} MiB",
        mib(lazy.kept),
        mib(whole.kept)
    );
    assert!(
        lazy.kept < 24 << 20,
        "the catalog that reads when used keeps {} MiB",
        mib(lazy.kept)
    );
    // And loading it takes well under what loading the whole one does.
    assert!(
        lazy.peak * 10 < whole.peak * 7,
        "loading the catalog that reads when used peaked at {} MiB, the whole one at {} MiB",
        mib(lazy.peak),
        mib(whole.peak)
    );
    assert!(
        lazy.peak < 28 << 20,
        "loading the catalog that reads when used peaked at {} MiB",
        mib(lazy.peak)
    );
    drop(read_when_used);
}
