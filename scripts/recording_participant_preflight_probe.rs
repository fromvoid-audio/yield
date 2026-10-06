//! Standalone allocation diagnostic linked to the actual library decoder. No grants.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use yield_library::operation_journal::recording_commands::RecordingProposal;

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

fn proposal() -> RecordingProposal {
    let ids: Vec<_> = (1..=6)
        .map(|n| format!("00000000-0000-4000-8000-{n:012}"))
        .collect();
    let fence = serde_json::json!({"source_id":"recording-source","root_identity":"root",
        "database_identity":"database","lifecycle_generation":3,"source_revision":7,"lease_epoch":1});
    let kinds = [
        "SourceDatabase",
        "Filesystem",
        "GlobalLibrary",
        "Harvest",
        "Projection",
        "Readiness",
    ];
    let descriptors: Vec<_> = kinds.iter().enumerate().map(|(i,kind)| serde_json::json!({
        "id":ids[i], "kind":kind, "requirement":"Required", "source_fence":
        if matches!(*kind,"SourceDatabase"|"Harvest"|"Projection"|"Readiness") {fence.clone()} else {serde_json::Value::Null},
        "artifact_key":null})).collect();
    let checkpoints: Vec<_> = ids
        .iter()
        .map(|id| {
            serde_json::json!({"participant_id":id,
        "state":"Pending","committed_revision":null,"diagnostic_code":null})
        })
        .collect();
    let root = serde_json::json!({"owner":ids[4],"registration":ids[5],"generation":1});
    let wire = serde_json::to_vec(&serde_json::json!({"version":1,"operation_id":ids[0],"output_id":ids[1],
        "work_key":{"action_ordinal":0,"step_ordinal":30,"participant_id":ids[1],"role":"Stage"},
        "participants":{"descriptors":descriptors,"checkpoints":checkpoints},
        "destination_root":root,"staging_root":root,
        "destination":{"parent":{"participant_id":ids[0],"source_id":"recording-source","locator":"Root",
            "expectation":{"source_revision":7,"filesystem_identity":"root","modified_unix_ns":null}},"leaf":"capture.wav"},
        "staging_path":"staging.wav","format":{"sample_rate":48000,"channels":2,"max_frames":2048,"header_bytes":68},
        "collision":"RequireAbsentNoReplace"})).unwrap();
    RecordingProposal::decode(&wire).unwrap()
}
fn measured(label: &str, wire: &[u8], maximum: usize) {
    MAX.store(0, Relaxed);
    REQUESTED.store(0, Relaxed);
    ACTIVE.store(true, Relaxed);
    let result = RecordingProposal::decode(wire);
    ACTIVE.store(false, Relaxed);
    assert!(result.is_err());
    println!(
        "{label} wire={} largest_request={} cumulative_requests={}",
        wire.len(),
        MAX.load(Relaxed),
        REQUESTED.load(Relaxed)
    );
    assert!(
        MAX.load(Relaxed) <= maximum,
        "exact-input request envelope exceeded"
    );
}
fn main() {
    let p = proposal();
    let baseline: serde_json::Value = serde_json::from_slice(&p.encode().unwrap()).unwrap();
    for field in ["descriptors", "checkpoints"] {
        let mut value = baseline.clone();
        value["participants"][field] = serde_json::json!(vec![serde_json::Value::Null; 20000]);
        let wire = serde_json::to_vec(&value).unwrap();
        assert!(wire.len() < 128 * 1024);
        measured(field, &wire, 1024);
    }
    let mut value = baseline;
    value["participants"]["descriptors"] = serde_json::json!(vec![serde_json::Value::Null; 65]);
    let original = serde_json::to_string(&value).unwrap();
    // Unknown escaped field identifier is intentionally consumed before excess refusal.
    // Its decoded scratch must be measured separately from the dense null array.
    for maximum_wire in [64 * 1024, 128 * 1024] {
        let repeat = maximum_wire - original.len() - 32;
        let wire = format!(
            "{{\"{}\\u0061\":null,{}",
            "x".repeat(repeat),
            &original[1..]
        );
        assert!(wire.len() <= maximum_wire);
        measured(
            "escaped_unknown_key_overcap",
            wire.as_bytes(),
            2 * maximum_wire,
        );
    }
}
