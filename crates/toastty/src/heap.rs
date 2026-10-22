//! Returning freed heap memory to the OS.
//!
//! Startup allocates and frees a lot: the grid is built at a placeholder
//! size and rebuilt once the cell size is known, and the font scan and
//! driver initialisation churn through temporaries on their threads.
//! glibc keeps the freed pages (only the very top of the main heap is
//! released automatically), so an idle terminal held ~11 MB of free heap.
//! [`release_free_memory`] hands them back.

/// Give free heap pages back to the OS (`malloc_trim(0)`: every arena,
/// including pages in the middle of a heap). Takes ~1 ms with a 40 MB
/// heap, so call it at idle points, not per frame. A no-op on allocators
/// other than glibc's.
pub fn release_free_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: `malloc_trim` only inspects and releases allocator-owned
    // free memory; it is thread-safe and has no preconditions.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn release_keeps_live_allocations_intact() {
        let live: Vec<Vec<u8>> = (0..64u8).map(|i| vec![i; 64 * 1024]).collect();
        drop((0..256).map(|_| vec![0u8; 64 * 1024]).collect::<Vec<_>>());
        super::release_free_memory();
        for (i, v) in (0..64u8).zip(&live) {
            assert!(v.iter().all(|&b| b == i));
        }
    }
}
