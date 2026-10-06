//! Normal-build original retained capture diagnostic; no allocation or migration grants.
use rusqlite::Connection;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    sample_sources::{DB_FILE_NAME, SourceDatabase},
};

struct Trace;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
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
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            added(layout.size());
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            added(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(ptr, old, new_size) };
        if !replacement.is_null() {
            LIVE.fetch_sub(old.size(), Relaxed);
            added(new_size);
        }
        replacement
    }
}
#[global_allocator]
static ALLOC: Trace = Trace;

fn main() {
    let encoding = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "UTF-8".to_owned());
    let root = std::env::temp_dir().join(format!("yield-capture-probe-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let profile = root.join("profile");
    let media = root.join("media");
    std::fs::create_dir(&profile).unwrap();
    std::fs::create_dir(&media).unwrap();
    std::fs::create_dir(profile.join("legacy")).unwrap();
    let app = AppRootGuard::set(profile.clone()).unwrap();
    let path = profile.join("legacy").join(DB_FILE_NAME);
    let mut connection = Connection::open(&path).unwrap();
    connection
        .pragma_update(None, "encoding", &encoding)
        .unwrap();
    connection
        .execute_batch(include_str!(
            "../crates/yield-library/tests/fixtures/profile-v11/source.sql"
        ))
        .unwrap();
    connection.execute("DELETE FROM wav_files", []).unwrap();
    let transaction = connection.transaction().unwrap();
    let header = std::mem::size_of::<(String, String)>();
    let remaining = 65536 - ("revision".len() + 1 + header) - 127 * (4 + header);
    for n in 0..127 {
        let length = remaining / 127 + if n == 0 { remaining % 127 } else { 0 };
        transaction
            .execute(
                "INSERT INTO metadata VALUES (?1,?2)",
                rusqlite::params![format!("m{n:03}"), "v".repeat(length)],
            )
            .unwrap();
    }
    let path_length = 4096 - std::mem::size_of::<(String, i64, i64)>();
    for n in (0..1024).rev() {
        let length = path_length + if n == 0 { 40 } else { 0 } - if n == 1 { 40 } else { 0 };
        let relative = format!(
            "{n:04}/{}{}",
            "\u{e000}".repeat((length - 5) / 3),
            "x".repeat((length - 5) % 3)
        );
        assert_eq!(relative.len(), length);
        transaction
            .execute(
                "INSERT INTO wav_files VALUES (?1,?2,?3)",
                rusqlite::params![relative, n as i64, -(n as i64)],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(connection);
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let owner = SourceDatabase::open_legacy_read_owner(&media, &guard, "legacy").unwrap();
    let original = owner.capture(&guard).unwrap();
    let before = std::fs::read(&path).unwrap();
    let baseline = LIVE.load(Relaxed);
    PEAK.store(baseline, Relaxed);
    MAX.store(0, Relaxed);
    let started = std::time::Instant::now();
    ACTIVE.store(true, Relaxed);
    let captured = owner.capture(&guard).unwrap();
    ACTIVE.store(false, Relaxed);
    let incremental_peak = PEAK.load(Relaxed).saturating_sub(baseline);
    let maximum_layout = MAX.load(Relaxed);
    assert_eq!(captured, original);
    assert_eq!(
        (captured.metadata_rows(), captured.audio_rows()),
        (128, 1024)
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    println!(
        "actual_retained_capture encoding={encoding} metadata=128 audio=1024 metadata_bytes=65536 audio_bytes=4194304 incremental_live_peak={incremental_peak} maximum_layout={maximum_layout} elapsed_us={} scratch_debit={}",
        started.elapsed().as_micros(),
        1024 * std::mem::size_of::<(i64, i64, i64)>()
            + 4096
            + std::mem::size_of::<Vec<(i64, i64, i64)>>()
            + 3 * std::mem::size_of::<rusqlite::Statement<'static>>()
    );
    // Existing frozen/observer metadata credit plus the derived 1024 scalar triples and
    // one 4096-byte path. Statement/Vec headers are conservative additions in the debit. This measures Rust allocation requests, not native SQLite/RSS.
    assert!(
        incremental_peak <= 64 * 1024 + 1024 * std::mem::size_of::<(i64, i64, i64)>() + 4096,
        "actual capture exceeds funded Rust metadata/scratch backing"
    );
    assert!(
        maximum_layout <= 1024 * std::mem::size_of::<(i64, i64, i64)>(),
        "actual capture materialized an oversized row collection"
    );
    drop(owner);
    drop(guard);
    drop(app);
    std::fs::remove_dir_all(&root).unwrap();
}
