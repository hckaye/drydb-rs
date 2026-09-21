//! That working out the codec profile costs nothing.
//!
//! It runs on the first read of a value, before a caller has had any chance to reserve
//! for it, so it has to be settled from what the types say rather than by archiving a
//! probe value and looking at the bytes. The allocator is global, so this file holds one
//! test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use drydb_rkyv::Profile;

static ARMED: AtomicBool = AtomicBool::new(false);
static ALLOCATED: AtomicI64 = AtomicI64::new(0);

struct Counting;

// SAFETY: every method forwards to the system allocator unchanged; the counter is
// bookkeeping on the side and never affects the pointers returned.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATED.fetch_add(layout.size() as i64, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATED.fetch_add(new_size as i64 - layout.size() as i64, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[test]
fn working_out_the_profile_allocates_nothing() {
    ARMED.store(true, Ordering::Relaxed);
    let first = Profile::current();
    let allocated = ALLOCATED.load(Ordering::Relaxed);
    let second = Profile::current();
    ARMED.store(false, Ordering::Relaxed);

    assert_eq!(
        allocated, 0,
        "working out the profile allocated {allocated} bytes"
    );
    // And it says the same thing every time, which is what files written earlier rely on.
    assert_eq!(first.id(), second.id());
    assert_eq!(first.describe(), second.describe());
}
