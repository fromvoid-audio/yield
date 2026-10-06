//! Normal System allocator: original same-connection controls and native-copy fixtures.
//! Pool/C-string/namespace setup and native C/RSS are excluded. Dynamic text cases
//! exercise the actual copy producer, but are not real SQLite control failures.
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

fn main() {
    use std::ffi::CString;
    use std::hash::{Hash, Hasher};
    let root = std::env::temp_dir().join(format!("yield-control-probe-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let app = AppRootGuard::set(root.clone()).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let store = OperationJournalStore::open(&guard).unwrap();
    let path = store.retained_namespace_path_for_test();
    let original_bytes = std::fs::read(&path).unwrap();
    let probe = store.control_funding_probe_for_test();
    for begin in [false, true] {
        for active in [false, true] {
            if active {
                store.retained_namespace_transaction_for_test(true).unwrap();
            }
            let mut original = None;
            let old = measure(|| {
                original = Some(probe.measure(false, begin).unwrap());
                true
            });
            if active && begin {
                store
                    .retained_namespace_transaction_for_test(false)
                    .unwrap();
            }
            if active {
                store.retained_namespace_transaction_for_test(true).unwrap();
            }
            let mut actual = None;
            let funded = measure(|| {
                actual = Some(probe.measure(true, begin).unwrap());
                true
            });
            if active && begin {
                store
                    .retained_namespace_transaction_for_test(false)
                    .unwrap();
            }
            let original = original.unwrap();
            let actual = actual.unwrap();
            assert_eq!(
                (original.0, original.1, original.2),
                (actual.0, actual.1, actual.2)
            );
            assert!(
                funded.1 <= actual.3,
                "original control peak {} exceeds debit {}",
                funded.1,
                actual.3
            );
            assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
            println!(
                "control={} active={active} native_code={} old_peak={} funded_peak={} largest={} debit={}",
                if begin { "BEGIN" } else { "ROLLBACK" },
                actual.0,
                old.1,
                funded.1,
                funded.2,
                actual.3
            );
        }
    }
    for (name, bytes) in [
        ("empty", Vec::new()),
        ("small", b"native error".to_vec()),
        ("long-valid", vec![b'x'; 65_536]),
        ("invalid-utf8", vec![0xff; 16_384]),
        ("long-invalid-utf8", vec![0xff; 131_072]),
    ] {
        let fixture = CString::new(bytes).unwrap();
        let text = fixture.to_string_lossy();
        let queued = format!("operation journal SQLite error: {text}");
        let shared = format!("ordinary-claim capacity rebuild bootstrap failed: {queued}");
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        queued.hash(&mut hash);
        queued.hash(&mut hash);
        shared.hash(&mut hash);
        let expected = hash.finish();
        let mut actual = None;
        let funded = measure(|| {
            actual = Some(probe.native_text_fixture(&fixture).unwrap());
            true
        });
        let actual = actual.unwrap();
        assert_eq!(actual.0, expected);
        assert!(
            funded.1 <= actual.1,
            "fixture {name} peak {} exceeds debit {}",
            funded.1,
            actual.1
        );
        println!(
            "native-copy-fixture={name} bytes={} peak={} largest={} debit={}",
            fixture.as_bytes().len(),
            funded.1,
            funded.2,
            actual.1
        );
    }
    let refused = CString::new(vec![b'x'; 1_048_576]).unwrap();
    let mut result = None;
    let measured = measure(|| {
        result = Some(probe.native_text_fixture(&refused));
        true
    });
    assert!(result.unwrap().is_err());
    assert_eq!(
        measured.1, 0,
        "over-budget conversion must refuse before owned native text"
    );
    println!("native-copy-fixture=over-budget peak=0");
    assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
    drop(probe);
    drop(store);
    drop(guard);
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
