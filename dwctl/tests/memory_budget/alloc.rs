//! Global allocator that counts live heap bytes for the whole process, except
//! for threads that opt out with [`exclude_current_thread`].
//!
//! The load generator and the mock upstream run on excluded threads, so the
//! count covers the application under test. Memory is never handed between
//! the two sides except through loopback sockets, which keeps the count exact.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicIsize, Ordering};

pub struct CountingAllocator;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    static EXCLUDED: Cell<bool> = const { Cell::new(false) };
}

/// Stops counting allocations made on the calling thread.
pub fn exclude_current_thread() {
    EXCLUDED.with(|excluded| excluded.set(true));
}

/// Bytes currently allocated by counted threads.
pub fn live_bytes() -> isize {
    LIVE.load(Ordering::Relaxed)
}

/// Highest value of [`live_bytes`] since the last [`reset_peak`].
pub fn peak_bytes() -> isize {
    PEAK.load(Ordering::Relaxed)
}

pub fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

fn counted() -> bool {
    // `try_with` fails only while the thread's locals are being destroyed.
    EXCLUDED.try_with(|excluded| !excluded.get()).unwrap_or(true)
}

fn add(bytes: isize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() && counted() {
            add(layout.size() as isize);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() && counted() {
            add(layout.size() as isize);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if counted() {
            LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        }
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() && counted() {
            add(new_size as isize - layout.size() as isize);
        }
        new_ptr
    }
}
