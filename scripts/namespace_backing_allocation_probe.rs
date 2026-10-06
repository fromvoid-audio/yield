//! Normal-build real SQLite allocation diagnostic. No capacity/effect authority is installed.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    operation_journal::OperationJournalStore,
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

fn observe(store: &OperationJournalStore, expected_success: bool, name: &str) {
    let before = LIVE.load(Relaxed);
    let measured = measure(
        || match store.retained_namespace_allocation_probe_for_test(false) {
            Ok(()) => true,
            Err(error) => {
                let diagnostic = error.to_string();
                assert!(!diagnostic.is_empty());
                drop(diagnostic);
                drop(error);
                false
            }
        },
    );
    assert_eq!(measured.0, expected_success);
    assert_eq!(
        LIVE.load(Relaxed),
        before,
        "all scoped backing must be destroyed"
    );
    println!(
        "case={name} success={} rust_peak={} largest={}",
        measured.0, measured.1, measured.2
    );
}
fn main() {
    let root = std::env::temp_dir().join(format!("yield-namespace-probe-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let app = AppRootGuard::set(root.clone()).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let store = OperationJournalStore::open(&guard).unwrap();
    let path = store.retained_namespace_path_for_test();
    let original = std::fs::read(&path).unwrap();
    observe(&store, true, "original-clean");
    store.retained_namespace_transaction_for_test(true).unwrap();
    observe(&store, false, "original-active-transaction");
    store
        .retained_namespace_transaction_for_test(false)
        .unwrap();
    let wal = path.with_file_name("journal.sqlite3-wal");
    std::fs::write(&wal, b"retained-sidecar-evidence").unwrap();
    observe(&store, false, "original-wal-sidecar");
    assert_eq!(std::fs::read(&wal).unwrap(), b"retained-sidecar-evidence");
    std::fs::remove_file(wal).unwrap();
    #[cfg(unix)]
    {
        let saved = path.with_file_name("namespace-original.saved");
        std::fs::rename(&path, &saved).unwrap();
        observe(&store, false, "original-missing-path");
        std::fs::rename(saved, &path).unwrap();
    }
    assert_eq!(std::fs::read(&path).unwrap(), original);
    observe(&store, true, "original-restored");
    drop(store);
    drop(guard);
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
