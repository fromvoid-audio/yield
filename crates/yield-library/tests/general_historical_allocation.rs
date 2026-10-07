//! Real Store/SQLite and System allocations; diagnostic feature installs no effect authority.
#![cfg(feature = "test-support")]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    operation_journal::{OperationId, OperationJournalStore, owner::JournalNamespaceFundingProbe},
};

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

fn wire(payload: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "schema_version":3, "operation_id":"01234567-89ab-cdef-0123-456789abcdef",
        "intent":{"actor":"User","kind":"FileMutation","label":"historical allocation"},
        "universal_intent": {
            "parent_operation_id":null, "command_kind":"CreateFile", "user_status_key":"operation.status.create-file",
            "participants": {
                "descriptors":[{"id":"fedcba98-7654-3210-fedc-ba9876543210","kind":"GlobalLibrary",
                    "requirement":"Required","source_fence":null,"artifact_key":null}],
                "checkpoints":[{"participant_id":"fedcba98-7654-3210-fedc-ba9876543210",
                    "state":"Pending","committed_revision":null,"diagnostic_code":null}]
            }
        },
        "phase":"IntentDurable", "disposition":"None", "payload":payload,
        "capacity_plan":{"volumes":[{"identity":{"device":1},"allocation_unit":1,
            "allocation_class":"DestinationStaging","logical_bytes":1,"allocated_bytes":1,
            "protected_free_bytes":268435456}]},
        "created_unix_ms":31, "updated_unix_ms":32
    })
}

#[test]
fn system_general_historical_rust_allocations_stay_prepaid_through_success_and_error_destruction() {
    let base = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(base.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let store = OperationJournalStore::open(&profile).unwrap();
    let operation = OperationId::parse("01234567-89ab-cdef-0123-456789abcdef").unwrap();
    let mut invalid = wire(serde_json::json!({"opaque":"é😀\\\"".repeat(128)}));
    invalid["universal_intent"]["command_kind"] = serde_json::json!("\u{85}".repeat(1024));
    let mut unknown = wire(serde_json::json!({"opaque":[null, false, "é😀"]}));
    unknown["schema_version"] = serde_json::json!(999);
    for (value, succeeds) in [
        (
            wire(serde_json::json!({"opaque":[null, 0, "é😀\\\"".repeat(256)]})),
            true,
        ),
        (wire(serde_json::json!({"opaque":vec![0; 256]})), true),
        (invalid, false),
        (unknown, false),
    ] {
        let bytes = serde_json::to_vec(&value).unwrap();
        store
            .replace_historical_row_fixture_for_test(operation, 1, &bytes)
            .unwrap();
        let funding = JournalNamespaceFundingProbe::new();
        // The original consistent read transaction is established outside measurement.
        // No worker/provider/native backend is installed by this diagnostic fixture.
        store.retained_namespace_transaction_for_test(true).unwrap();
        let mut actual_success = false;
        let totals = measured(&funding, || {
            let result = store.observe_historical_claim_with_funding_for_test(operation, &funding);
            actual_success = result.is_ok();
            if let Err(error) = result {
                drop(funding.retain_command_error_for_test(error));
            }
        });
        store
            .retained_namespace_transaction_for_test(false)
            .unwrap();
        assert_eq!(actual_success, succeeds);
        assert!(totals.peak > 0);
        assert_eq!(
            totals.current, 0,
            "all owned reader/error storage must be destroyed"
        );
        assert_eq!(
            totals.violations, 0,
            "every allocation/free must retain original debit; peak={}",
            totals.peak
        );
    }
}
