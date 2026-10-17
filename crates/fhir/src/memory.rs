//! Handing memory the catalog loads freed back to the operating system.

/// Return freed heap memory to the operating system.
///
/// Loading or rebuilding the catalog parses far more JSON than the catalog keeps. On glibc the
/// memory freed afterwards stays with the process: the parsing runs on worker threads, each with
/// an arena of its own, and glibc only gives back the top of a heap on its own. The process then
/// sits at the peak of its first load for the rest of its life. This asks glibc to release every
/// free page it holds, and asks the same of macOS's allocator. It does nothing elsewhere.
pub fn release_unused_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: `malloc_trim` takes no pointers; it only asks glibc to release pages it holds free.
    unsafe {
        libc::malloc_trim(0);
    }
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: a null zone means every zone, and a goal of zero means as much as can be
        // released; the call only hands back pages the allocator holds free.
        unsafe {
            malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
        }
    }
}
