//! Shared native-audio test instrumentation.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static COUNT_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
    static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
    static DEALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
}

struct ThreadCountingAllocator;

unsafe impl GlobalAlloc for ThreadCountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count_deallocation();
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count_allocation();
        count_deallocation();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static TEST_ALLOCATOR: ThreadCountingAllocator = ThreadCountingAllocator;

fn count_allocation() {
    let _ = COUNT_ALLOCATIONS.try_with(|enabled| {
        if enabled.get() {
            let _ = ALLOCATION_COUNT.try_with(|count| count.set(count.get() + 1));
        }
    });
}

fn count_deallocation() {
    let _ = COUNT_ALLOCATIONS.try_with(|enabled| {
        if enabled.get() {
            let _ = DEALLOCATION_COUNT.try_with(|count| count.set(count.get() + 1));
        }
    });
}

/// Count allocations and deallocations made by the current thread while `run`
/// executes. The measured body should be prepared before calling this helper.
pub(crate) fn count_thread_allocations_and_deallocations(run: impl FnOnce()) -> (usize, usize) {
    ALLOCATION_COUNT.with(|count| count.set(0));
    DEALLOCATION_COUNT.with(|count| count.set(0));
    COUNT_ALLOCATIONS.with(|enabled| enabled.set(true));
    run();
    COUNT_ALLOCATIONS.with(|enabled| enabled.set(false));
    (
        ALLOCATION_COUNT.with(Cell::get),
        DEALLOCATION_COUNT.with(Cell::get),
    )
}

pub(crate) fn count_thread_allocations(run: impl FnOnce()) -> usize {
    count_thread_allocations_and_deallocations(run).0
}
