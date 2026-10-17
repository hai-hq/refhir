//! Reads that a source could not make while a request was being worked on.
//!
//! A source of definitions or terminology answers `None` for a row it could not read, because a
//! question about a profile or a code has no way to say that the store failed. Left at that, a
//! request that hit a database error would be answered as if the profile or the value set did not
//! exist: a write refused as invalid, or one accepted because nothing was left to check it by.
//! The source says so here, on the thread that asked, and whoever runs the request asks, before
//! it commits anything, whether a read failed while it ran ([`watching_reads`]).

use std::cell::{Cell, RefCell};

thread_local! {
    static DEPTH: Cell<usize> = const { Cell::new(0) };
    static FAILED: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Say that a read the work on this thread needed could not be made. Only the first failure of
/// the work being watched is kept.
pub fn note_read_failure(message: String) {
    FAILED.with(|failed| {
        let mut failed = failed.borrow_mut();
        if failed.is_none() {
            *failed = Some(message);
        }
    });
}

/// Leaves the region when it is dropped, even if the work in it panicked.
struct Region {
    outermost: bool,
}

impl Region {
    fn enter() -> Self {
        let outermost = DEPTH.with(|depth| {
            let outer = depth.get();
            depth.set(outer + 1);
            outer == 0
        });
        if outermost {
            // What an earlier piece of work on this thread (the building of a catalog, say) noted
            // is not about this one.
            FAILED.with(|failed| failed.borrow_mut().take());
        }
        Self { outermost }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

/// Run `work`, and say whether a read it needed failed: `Err` has what the source said, and
/// then whatever `work` returned is not to be trusted. A call made while another is running on the
/// thread leaves the failure to the outer one, which sees all of it.
pub fn watching_reads<T>(work: impl FnOnce() -> T) -> Result<T, String> {
    let region = Region::enter();
    let value = work();
    if region.outermost {
        if let Some(message) = FAILED.with(|failed| failed.borrow_mut().take()) {
            return Err(message);
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_that_read_everything_is_returned() {
        assert_eq!(watching_reads(|| 7), Ok(7));
    }

    #[test]
    fn a_failed_read_is_reported_and_the_first_one_is_kept() {
        let result = watching_reads(|| {
            note_read_failure("first".into());
            note_read_failure("second".into());
            7
        });
        assert_eq!(result, Err("first".to_string()));
        // It is reported once, not to the work that follows.
        assert_eq!(watching_reads(|| 8), Ok(8));
    }

    #[test]
    fn what_was_noted_before_the_work_began_is_not_about_it() {
        note_read_failure("from building a catalog".into());
        assert_eq!(watching_reads(|| 9), Ok(9));
    }

    #[test]
    fn a_failure_inside_nested_work_reaches_the_outer_work() {
        let result = watching_reads(|| {
            let inner = watching_reads(|| {
                note_read_failure("inner".into());
                1
            });
            // The inner call does not take it: it is the outer call's to report.
            assert_eq!(inner, Ok(1));
            2
        });
        assert_eq!(result, Err("inner".to_string()));
    }

    #[test]
    fn a_thread_reports_only_its_own_failures() {
        let other = std::thread::spawn(|| {
            watching_reads(|| {
                note_read_failure("on another thread".into());
            })
        });
        assert!(watching_reads(|| ()).is_ok());
        assert!(other.join().unwrap().is_err());
    }

    #[test]
    fn work_that_panics_leaves_the_thread_ready_for_the_next() {
        let panicked = std::panic::catch_unwind(|| {
            let _ = watching_reads(|| -> u8 { panic!("boom") });
        });
        assert!(panicked.is_err());
        assert_eq!(watching_reads(|| 3), Ok(3));
    }
}
