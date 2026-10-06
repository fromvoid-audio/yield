//! Actual public decoder allocation diagnostic. Historical JSON grants no authority.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use yield_library::sample_sources::SourceRegistryReferenceProposal;

struct Trace;
static ACTIVE: AtomicBool = AtomicBool::new(false);
static MAX: AtomicUsize = AtomicUsize::new(0);
static REQUESTED: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Trace {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ACTIVE.load(Relaxed) {
            MAX.fetch_max(layout.size(), Relaxed);
            REQUESTED.fetch_add(layout.size(), Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        if ACTIVE.load(Relaxed) {
            MAX.fetch_max(new_size, Relaxed);
            REQUESTED.fetch_add(new_size, Relaxed);
        }
        unsafe { System.realloc(ptr, old, new_size) }
    }
}
#[global_allocator]
static ALLOC: Trace = Trace;

fn base() -> serde_json::Value {
    let id = "00000000-0000-4000-8000-000000000001";
    serde_json::json!({"version":1,"operation_id":id,
        "work_key":{"action_ordinal":255,"step_ordinal":0,"participant_id":id,"role":"GlobalLibrary"},
        "transition":"enrolled","expected":{"status":"absent"},
        "subject":{"source_id":"codec-only","lifecycle_generation":1,
            "profile_acquisition":id,"lease_owner":id,"lease_epoch":1,
            "media_root_identity":[1,2],"database_root_identity":"db-root",
            "database_identity":"db-file","root_binding_identity":"binding",
            "scan_revision":0,"audit":{"status":"unavailable"},
            "registration":{"registration_id":id,"phase":"registered_phase_observed"}}})
}
fn measure(label: &str, wire: &[u8]) -> usize {
    MAX.store(0, Relaxed);
    REQUESTED.store(0, Relaxed);
    ACTIVE.store(true, Relaxed);
    let result = SourceRegistryReferenceProposal::decode(wire);
    ACTIVE.store(false, Relaxed);
    assert!(result.is_err());
    let largest = MAX.load(Relaxed);
    println!(
        "actual_expected_refusal case={label} wire={} maximum_layout={largest} cumulative_requested={}",
        wire.len(),
        REQUESTED.load(Relaxed)
    );
    largest
}
fn main() {
    // Validate all unrelated metadata outside the measured actual decode.
    let base = base();
    SourceRegistryReferenceProposal::decode(&serde_json::to_vec(&base).unwrap()).unwrap();
    let mut measurements = Vec::new();
    for field in ["row_identity", "unknown"] {
        let mut invalid = base.clone();
        invalid["expected"] = serde_json::json!({"status":"present",
            "state":"enrolled",field:vec![0u8;6000]});
        let wire = serde_json::to_vec(&invalid).unwrap();
        assert!(wire.len() <= 16 * 1024);
        measurements.push(measure(field, &wire));
    }
    assert!(
        measurements.iter().all(|largest| *largest <= 64 * 1024),
        "closed expected-row refusal must not materialize per-element Content"
    );
    // Measure escaped parser scratch and owned diagnostic formatting separately.
    // These cases do not assert that all decoder errors share the array threshold.
    let base_wire = serde_json::to_string(&base).unwrap();
    let original = r#"{"status":"absent"}"#;
    for (label, prefix, suffix) in [
        ("escaped_unknown_key", "{\"", "\":0,\"status\":\"absent\"}"),
        ("escaped_unknown_status", "{\"status\":\"", "\"}"),
    ] {
        let overhead = base_wire.len() - original.len() + prefix.len() + suffix.len();
        let repetitions = (16 * 1024 - overhead) / 6;
        let audit = format!("{prefix}{}{suffix}", "\\u2028".repeat(repetitions));
        let wire = base_wire.replacen(original, &audit, 1);
        assert_ne!(wire, base_wire);
        assert!((16 * 1024 - 6..=16 * 1024).contains(&wire.len()));
        measure(label, wire.as_bytes());
    }
}
