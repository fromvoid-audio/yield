//! Actual funded provider graph allocations, with no native admission authority.
#![cfg(feature = "test-support")]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
use yield_library::operation_journal::{AdmissionGrowthProbe, owner::JournalNamespaceFundingProbe};

#[derive(Clone, Copy)]
struct Tracking {
    active: bool,
    funding: *const JournalNamespaceFundingProbe,
    baseline: usize,
    current: usize,
    peak: usize,
    violations: usize,
}
thread_local! {
    static TRACKING: Cell<Tracking> = const { Cell::new(Tracking {
        active:false, funding:std::ptr::null(), baseline:0, current:0, peak:0, violations:0,
    }) };
}

struct CheckedSystem;
fn allocation_change(allocated: usize, freed: usize) {
    let _ = TRACKING.try_with(|cell| {
        let mut state = cell.get();
        if !state.active {
            return;
        }
        // SAFETY: this thread's measurement keeps the exact borrowed funding owner alive;
        // tracking is disabled before returning or dropping it. The getter allocates nothing.
        let charged = state
            .baseline
            .saturating_sub(unsafe { &*state.funding }.remaining_bytes());
        if state.current > charged {
            state.violations += 1;
        }
        if freed > state.current {
            state.violations += 1;
        }
        state.current = state
            .current
            .saturating_sub(freed)
            .saturating_add(allocated);
        state.peak = state.peak.max(state.current);
        if state.current > charged {
            state.violations += 1;
        }
        cell.set(state);
    });
}

// SAFETY: every allocation is delegated unchanged to System. Accounting observes only
// the calling thread and never changes pointers, layouts, allocator decisions or lifetimes.
unsafe impl GlobalAlloc for CheckedSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocation_change(layout.size(), 0);
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            allocation_change(layout.size(), 0);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        allocation_change(0, layout.size());
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = unsafe { System.realloc(pointer, layout, size) };
        if !result.is_null() {
            allocation_change(size, layout.size());
        }
        result
    }
}
#[global_allocator]
static ALLOCATOR: CheckedSystem = CheckedSystem;

fn measured(funding: &JournalNamespaceFundingProbe, work: impl FnOnce()) -> Tracking {
    TRACKING.with(|cell| {
        cell.set(Tracking {
            active: true,
            funding,
            baseline: funding.remaining_bytes(),
            current: 0,
            peak: 0,
            violations: 0,
        })
    });
    work();
    TRACKING.with(|cell| {
        let state = cell.get();
        cell.set(Tracking {
            active: false,
            ..state
        });
        state
    })
}

#[test]
fn actual_growth_is_charged_before_allocation_and_until_original_graphs_are_freed() {
    for rows in [1, 8, 32, 256] {
        for mode in 0..=3 {
            let funding = JournalNamespaceFundingProbe::new();
            let probe = AdmissionGrowthProbe::new(rows);
            let available = funding.remaining_bytes();
            let stats = measured(&funding, || {
                assert!(probe.measure(&funding, mode));
            });
            assert_eq!(
                stats.violations, 0,
                "rows={rows}, mode={mode}, peak={}",
                stats.peak
            );
            assert_eq!(
                stats.current, 0,
                "all original Rust graphs freed before measurement ends"
            );
            assert_eq!(funding.remaining_bytes(), available);
            assert!(stats.peak > 0);
        }
    }
}
