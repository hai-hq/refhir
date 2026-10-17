//! Loading the catalog must not hold every parsed document at once.
//!
//! A parsed StructureDefinition or code system is several times the size of its text, and the
//! catalog keeps only a fraction of it. Loading used to parse the whole conformance store and
//! keep all of it until the catalog was built, so a start needed several times the memory the
//! running server did, and a server given a limit for the second was killed in the first. This is
//! a binary of its own, with one test, so that the allocator it counts is the only thing running.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, RwLock};

use refhir_database::Database;
use refhir_fhir::{install_package_dir, r4_dir, Catalog};
use refhir_sqlite::SqlitePersistence;

/// Bytes the program has allocated now, and the most it has had at once since the last reset.
struct Counting;
static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = System.alloc(layout);
        if !pointer.is_null() {
            let now = NOW.fetch_add(layout.size(), Relaxed) + layout.size();
            PEAK.fetch_max(now, Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        System.dealloc(pointer, layout);
        NOW.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let moved = System.realloc(pointer, layout, size);
        if !moved.is_null() {
            if size >= layout.size() {
                let now = NOW.fetch_add(size - layout.size(), Relaxed) + (size - layout.size());
                PEAK.fetch_max(now, Relaxed);
            } else {
                NOW.fetch_sub(layout.size() - size, Relaxed);
            }
        }
        moved
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

#[test]
fn loading_the_catalog_takes_a_small_multiple_of_what_it_keeps() {
    let store = SqlitePersistence::open_in_memory().unwrap();
    let db = Database::open(Arc::new(store.clone())).unwrap();
    // The whole R4 definition set, as a server stores it.
    let installed = RwLock::new(Catalog::empty());
    install_package_dir(&store, &db, &installed, r4_dir()).unwrap();
    drop(installed);

    let before = NOW.load(Relaxed);
    PEAK.store(before, Relaxed);
    let loaded = Catalog::open(&store).unwrap();
    let kept = NOW.load(Relaxed) - before;
    let peak = PEAK.load(Relaxed) - before;
    drop(loaded);

    // Loading it as one piece took about seven times what the catalog keeps, and parsing a
    // table at a time about three. A page at a time takes about one and a third.
    assert!(
        peak < kept * 2,
        "loading the catalog peaked at {} MiB to keep {} MiB",
        peak >> 20,
        kept >> 20
    );
}
