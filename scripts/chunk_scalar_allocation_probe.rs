//! Normal-build original chunk-row decoder comparison. No actor/effect/capacity authority.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use yield_library::operation_journal::OperationJournalStore;

struct Trace;
static ACTIVE: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static MAX: AtomicUsize = AtomicUsize::new(0);
fn added(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Relaxed) + bytes;
    if ACTIVE.load(Relaxed) {
        PEAK.fetch_max(live, Relaxed);
        MAX.fetch_max(bytes, Relaxed);
    }
}
unsafe impl GlobalAlloc for Trace {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            added(layout.size());
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            added(layout.size());
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, old: Layout, bytes: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, old, bytes) };
        if !replacement.is_null() {
            LIVE.fetch_sub(old.size(), Relaxed);
            added(bytes);
        }
        replacement
    }
}
#[global_allocator]
static ALLOCATOR: Trace = Trace;

fn measure(mut action: impl FnMut() -> bool) -> (bool, usize, usize) {
    let baseline = LIVE.load(Relaxed);
    PEAK.store(baseline, Relaxed);
    MAX.store(0, Relaxed);
    ACTIVE.store(true, Relaxed);
    let valid = action();
    ACTIVE.store(false, Relaxed);
    (
        valid,
        PEAK.load(Relaxed).saturating_sub(baseline),
        MAX.load(Relaxed),
    )
}

fn main() {
    let root =
        std::env::temp_dir().join(format!("yield-chunk-scalar-probe-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    for field in [
        "output_key",
        "operation_id",
        "volume_device",
        "allocation_class",
        "state",
    ] {
        let database = root.join(format!("{field}.sqlite3"));
        let bytes = 1 << 20;
        let probe = OperationJournalStore::prepare_chunk_scalar_allocation_probe_for_test(
            &database, field, bytes,
        )
        .unwrap();
        let before = std::fs::read(&database).unwrap();
        let old = measure(|| probe.accepts_original_stored_row_for_test(true));
        let new = measure(|| probe.accepts_original_stored_row_for_test(false));
        assert!(
            !old.0 && !new.0,
            "both actual decoders must reject original corrupt {field}"
        );
        assert!(
            old.2 >= bytes,
            "before-reader must expose original large field allocation"
        );
        assert!(
            new.2 < 4096,
            "borrowed reader copied original corrupt {field}"
        );
        assert!(new.1 < old.1);
        assert_eq!(std::fs::read(&database).unwrap(), before);
        println!(
            "original_chunk_scalar field={field} stored_bytes={bytes} original_peak={} original_max={} borrowed_peak={} borrowed_max={}",
            old.1, old.2, new.1, new.2
        );
        drop(probe);
    }
    std::fs::remove_dir_all(root).unwrap();
}
