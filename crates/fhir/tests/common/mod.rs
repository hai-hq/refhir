//! A counting allocator for the tests that measure memory.
//!
//! A test binary that measures memory names [`Counting`] as its global allocator, and has one
//! test, so that the bytes it counts are that test's and no other's.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// Bytes the program has allocated now, and the most it has had at once since the last reset.
pub struct Counting;
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

/// What `work` held on to when it was done, and the most it held at once: both in bytes above
/// what the program held before it started. What `work` returns is counted in the first, so a
/// caller that wants only what is kept after dropping it can drop it before reading `NOW` again.
pub struct Measured<T> {
    pub value: T,
    /// Bytes held when `work` returned, above what was held before.
    pub kept: usize,
    /// The most bytes held at any moment during `work`, above what was held before.
    pub peak: usize,
}

pub fn measure<T>(work: impl FnOnce() -> T) -> Measured<T> {
    let before = NOW.load(Relaxed);
    PEAK.store(before, Relaxed);
    let value = work();
    Measured {
        value,
        kept: NOW.load(Relaxed).saturating_sub(before),
        peak: PEAK.load(Relaxed).saturating_sub(before),
    }
}

pub fn mib(bytes: usize) -> usize {
    bytes >> 20
}
