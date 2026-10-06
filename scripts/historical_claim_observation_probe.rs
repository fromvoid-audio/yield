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
    let root = std::env::temp_dir().join(format!("yield-history-probe-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let app = AppRootGuard::set(root.clone()).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let store = OperationJournalStore::open(&guard).unwrap();
    let database = guard
        .profile_root()
        .join("operation_journal/journal.sqlite3");
    let id = OperationId::new();
    let envelope = format!(
        r#"{{"schema_version":1,"operation_id":"{id}","intent":{{"actor":"User","kind":"FileHistory","label":"probe"}},"phase":"IntentDurable","disposition":"None","payload":PAYLOAD,"created_unix_ms":1,"updated_unix_ms":1}}"#
    );
    for case in ["dense_array", "unique_object", "escaped_string"] {
        let budget = MAX_RECORD_BYTES - (envelope.len() - "PAYLOAD".len());
        let payload = match case {
            "dense_array" => format!("[0{}]", ",0".repeat((budget - 3) / 2)),
            "unique_object" => {
                let mut object = String::from("{");
                for n in 0.. {
                    let key = format!("{}\"{n}\":0", if n == 0 { "" } else { "," });
                    if object.len() + key.len() + 1 > budget {
                        break;
                    }
                    object.push_str(&key);
                }
                object.push('}');
                object
            }
            _ => format!("\"{}\"", "\\u2028".repeat((budget - 2) / 6)),
        };
        let wire = envelope.replace("PAYLOAD", &payload);
        assert!(wire.len() <= MAX_RECORD_BYTES);
        store
            .replace_historical_row_fixture_for_test(id, 1, wire.as_bytes())
            .unwrap();
        let before = std::fs::read(&database).unwrap();
        // Public get retains the payload and shares the unchanged old claim read_row path.
        let old = measure(|| store.get(id).unwrap().is_some());
        // Test-support component fixture includes isolated original-pool setup. The
        // separate queued-owner regression verifies genuine production command funding.
        let new = measure(|| store.observe_historical_claim_for_test(id).unwrap());
        assert_eq!(old.0, new.0);
        assert!(
            new.1 < old.1,
            "opaque graph ownership did not shrink for {case}"
        );
        assert!(
            new.1 <= 3 * MAX_RECORD_BYTES,
            "component Rust peak exceeds conservative parser/key funding"
        );
        assert_eq!(std::fs::read(&database).unwrap(), before);
        println!(
            "actual_sqlite_historical_bool case={case} wire={} old_peak={} old_maximum_layout={} observed_peak={} observed_maximum_layout={}",
            wire.len(),
            old.1,
            old.2,
            new.1,
            new.2
        );
    }
    // Borrowed errmsg conversion fixtures exercise the SAME normal-build funded
    // routine used by actual sqlite3_errmsg. These synthetic messages establish
    // Rust allocation coefficients only, not native SQLite or physical qualification.
    for (case, bytes) in [
        ("small_ascii", b"fixture SQLite error".to_vec()),
        ("small_invalid_utf8", vec![0xff; 16]),
        ("large_invalid_utf8", vec![0xff; 256 * 1024]),
    ] {
        let expected = String::from_utf8_lossy(&bytes).len();
        let mut terminated = bytes.clone();
        terminated.push(0);
        let mut diagnostic = (0, 0);
        let observed = measure(|| {
            diagnostic = store
                .historical_error_conversion_for_test(&terminated)
                .unwrap();
            diagnostic.0 == expected
        });
        assert!(observed.0);
        println!(
            "borrowed_errmsg_conversion_fixture case={case} raw={} copied={} dynamic_credit={} observed_peak={} observed_maximum_layout={} fixture_setup_included=true",
            bytes.len(),
            diagnostic.0,
            diagnostic.1,
            observed.1,
            observed.2
        );
    }
    drop(store);
    drop(guard);
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
