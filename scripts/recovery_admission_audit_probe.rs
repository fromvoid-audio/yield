//! Normal-build real SQLite allocation diagnostic. No capacity/effect authority is installed.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    operation_journal::{MAX_RECORD_BYTES, OperationId, OperationJournalStore},
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
    let root = std::env::temp_dir().join(format!("yield-recovery-probe-{}", std::process::id()));
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
    let database = guard
        .profile_root()
        .join("operation_journal/journal.sqlite3");
    let mut setup_peak = 0;
    for (spends, corrupt) in [(0, 0), (1, 0), (259, 0), (768, 0), (1, MAX_RECORD_BYTES)] {
        store
            .install_recovery_audit_fixture_for_test(id, spends, corrupt)
            .unwrap();
        let before = std::fs::read(&database).unwrap();
        let old = measure(|| {
            store
                .recovery_audit_allocation_probe_for_test(false)
                .unwrap()
                .0
        });
        let mut debit = 0;
        let new = measure(|| {
            let result = store
                .recovery_audit_allocation_probe_for_test(true)
                .unwrap();
            debit = result.1;
            result.0
        });
        assert_eq!(old.0, new.0);
        assert_eq!(new.0, corrupt == 0);
        if spends == 0 {
            setup_peak = new.1;
        }
        assert!(
            new.1 <= setup_peak + debit,
            "original row scratch exceeds debit after isolated pool setup"
        );
        if corrupt != 0 {
            assert!(old.2 >= corrupt);
            assert!(new.2 < corrupt);
            assert!(new.1 < old.1);
        }
        assert_eq!(std::fs::read(&database).unwrap(), before);
        println!(
            "original_recovery_audit spends={spends} corrupt={corrupt} old_peak={} old_max={} funded_peak={} funded_max={} debit={debit} isolated_pool_peak={setup_peak}",
            old.1, old.2, new.1, new.2
        );
    }
    drop(store);
    drop(guard);
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
