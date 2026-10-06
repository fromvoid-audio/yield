//! Normal-build real SQLite allocation diagnostic. No capacity/effect authority is installed.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    operation_journal::{OperationId, OperationJournalStore},
};

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
    let root = std::env::temp_dir().join(format!("yield-provider-probe-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let app = AppRootGuard::set(root.clone()).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let store = OperationJournalStore::open(&guard).unwrap();
    let id = OperationId::new();
    let wire = format!(
        r#"{{"schema_version":1,"operation_id":"{id}","intent":{{"actor":"User","kind":"FileHistory","label":"probe"}},"phase":"IntentDurable","disposition":"None","payload":null,"created_unix_ms":1,"updated_unix_ms":1}}"#
    );
    store
        .replace_historical_row_fixture_for_test(id, 1, wire.as_bytes())
        .unwrap();
    for rows in [0, 1, 64, 259] {
        for released in [false, true] {
            store
                .install_ordinary_collection_fixture_for_test(id, rows, released)
                .unwrap();
            let probe = store.provider_reconciliation_probe_for_test().unwrap();
            let mut old_result = None;
            let old = measure(|| {
                old_result = Some(probe.measure(false));
                true
            });
            let mut result = None;
            let funded = measure(|| {
                result = Some(probe.measure(true));
                true
            });
            let (matches, debit) = result.unwrap();
            assert!(matches && old_result.unwrap().0);
            assert!(
                funded.1 <= debit,
                "measured Rust peak {} exceeds debit {debit}",
                funded.1
            );
            println!(
                "rows={rows} released={released} old_peak={} funded_peak={} funded_largest={} debit={debit}",
                old.1, funded.1, funded.2
            );
        }
    }
    drop(store);
    drop(guard);
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
