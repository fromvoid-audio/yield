#![cfg(any(target_os = "linux", target_os = "macos", windows))]
use super::*;
use crate::app_dirs::{AppRootGuard, WritableProfileGuard};
use std::{fs, time::Duration};

struct Fixture {
    connection: OwnedConnection,
    root: BoundDatabaseRoot,
    _guard: WritableProfileGuard,
    _app_root: AppRootGuard,
    temp: tempfile::TempDir,
}
fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let app_root = AppRootGuard::set(temp.path().to_path_buf()).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let root = BoundDatabaseRoot::for_profile_guard(&guard).unwrap();
    let connection = root
        .open_owned_sqlite(Path::new("test.db"), rusqlite::OpenFlags::default())
        .unwrap();
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA mmap_size=0; PRAGMA wal_autocheckpoint=0; CREATE TABLE actual_data(value BLOB); INSERT INTO actual_data VALUES (zeroblob(32768)); PRAGMA wal_checkpoint(TRUNCATE); BEGIN IMMEDIATE;").unwrap();
    Fixture {
        connection,
        root,
        _guard: guard,
        _app_root: app_root,
        temp,
    }
}
fn capture(f: &Fixture) -> Option<WalObjects> {
    f.connection.retained_wal_objects(
        &f.root,
        Path::new("test.db"),
        Instant::now() + Duration::from_secs(2),
    )
}

#[test]
fn actual_native_main_wal_and_kernel_mapped_shm_are_distinct_read_only_facts() {
    let f = fixture();
    let before = f.connection.total_changes();
    let facts = capture(&f).expect("actual known native WAL and mapped SHM proof");
    assert_eq!(facts, capture(&f).unwrap());
    assert_eq!(
        facts.main.bytes,
        fs::metadata(f.temp.path().join("test.db")).unwrap().len()
    );
    assert_eq!(facts.wal.bytes, 0);
    assert!(facts.shm.bytes >= 32768);
    assert_ne!(facts.main.identity, facts.wal.identity);
    assert_ne!(facts.wal.identity, facts.shm.identity);
    assert_eq!(facts.main.device, facts.shm.device);
    assert!(facts.main.allocation_unit.is_power_of_two());
    assert_eq!(before, f.connection.total_changes());
    assert!(!f.connection.is_autocommit());
    f.connection.execute_batch("ROLLBACK").unwrap();
}

#[test]
#[cfg(unix)]
fn exact_size_replacement_of_main_or_wal_cannot_substitute_native_binding() {
    for name in ["test.db", "test.db-wal"] {
        let f = fixture();
        let facts = capture(&f).unwrap();
        let path = f.temp.path().join(name);
        let parked = path.with_extension("parked");
        fs::rename(&path, &parked).unwrap();
        fs::copy(&parked, &path).unwrap();
        assert!(capture(&f).is_none(), "same-size replacement {name}");
        assert_eq!(
            f.connection.observe_retained_wal_objects(
                &f.root,
                Path::new("test.db"),
                Instant::now() + Duration::from_secs(2),
            ),
            Err(WalObservationRefusal::Unavailable),
            "actual binding loss keeps its cause",
        );
        fs::remove_file(&path).unwrap();
        fs::rename(&parked, &path).unwrap();
        assert_eq!(capture(&f).unwrap(), facts);
        f.connection.execute_batch("ROLLBACK").unwrap();
    }
}

#[test]
#[cfg(unix)]
fn mapped_original_shm_refuses_identical_bytes_replacement_and_missing_path() {
    let f = fixture();
    let facts = capture(&f).unwrap();
    let path = f.temp.path().join("test.db-shm");
    let parked = f.temp.path().join("original-shm");
    fs::rename(&path, &parked).unwrap();
    assert!(capture(&f).is_none());
    fs::copy(&parked, &path).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), facts.shm.bytes);
    assert!(
        capture(&f).is_none(),
        "kernel mapping still names original SHM inode"
    );
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&parked, &path).unwrap();
    assert!(capture(&f).is_none(), "no-follow SHM binding rejects alias");
    fs::remove_file(&path).unwrap();
    fs::rename(&parked, &path).unwrap();
    assert_eq!(capture(&f).unwrap(), facts);
    f.connection.execute_batch("ROLLBACK").unwrap();
}

#[cfg(windows)]
#[test]
fn windows_original_native_handles_refuse_rename_and_wrong_region_without_sql_or_close() {
    let f = fixture();
    let facts = capture(&f).unwrap();
    let observation = f.root.readonly_observation().unwrap();
    observation.validate().unwrap();
    for name in ["test.db", "test.db-wal", "test.db-shm"] {
        // Bundled win32 opens these native handles without FILE_SHARE_DELETE.
        // Verify the OS refusal rather than pretend a Unix rename can succeed.
        let path = f.temp.path().join(name);
        assert!(fs::rename(&path, path.with_extension("parked")).is_err());
        assert_eq!(capture(&f).unwrap(), facts);
    }
    let context = &f.connection.registration.as_ref().unwrap().context;
    let original = context
        .state
        .lock()
        .unwrap()
        .mapped_shm_region_zero
        .unwrap();
    context.state.lock().unwrap().mapped_shm_region_zero = Some(original + 32768);
    assert!(capture(&f).is_none());
    context.state.lock().unwrap().mapped_shm_region_zero = Some(original);
    let main = context.main.load(Ordering::SeqCst);
    // SAFETY: real live private wrapper; ask for the wrong role and require refusal.
    assert!(unsafe { io::known_windows_file(main, context, 1) }.is_none());
    f.connection
        .authorizer(Some(|_: rusqlite::hooks::AuthContext<'_>| {
            rusqlite::hooks::Authorization::Deny
        }));
    assert_eq!(capture(&f).unwrap(), facts);
    assert!(!f.connection.is_autocommit());
    f.connection
        .authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>);
    f.connection.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn unknown_unowned_delete_expired_or_wrong_root_has_no_retained_wal_observation() {
    let f = fixture();
    assert!(
        f.connection
            .retained_wal_objects(&f.root, Path::new("test.db"), Instant::now())
            .is_none()
    );
    assert!(
        f.connection
            .retained_wal_objects(
                &f.root,
                Path::new("../test.db"),
                Instant::now() + Duration::from_secs(2)
            )
            .is_none()
    );
    assert!(
        f.connection
            .retained_wal_objects(
                &f.root,
                Path::new("another.db"),
                Instant::now() + Duration::from_secs(2)
            )
            .is_none()
    );
    let raw: OwnedConnection = Connection::open_in_memory().unwrap().into();
    assert!(
        raw.retained_wal_objects(
            &f.root,
            Path::new("test.db"),
            Instant::now() + Duration::from_secs(2)
        )
        .is_none()
    );
    f.connection
        .execute_batch("ROLLBACK; PRAGMA journal_mode=DELETE")
        .unwrap();
    assert!(
        capture(&f).is_none(),
        "closed WAL mapping cannot survive journal-mode change"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn kernel_map_stream_checks_complete_input_with_fixed_backing() {
    use std::{
        io::Cursor,
        time::{Duration, Instant},
    };
    let device = libc::makedev(8, 1) as u64;
    let scan = |bytes: &[u8]| {
        super::mapped_identity_stream(
            Cursor::new(bytes),
            0x10000,
            device,
            42,
            Instant::now() + Duration::from_secs(2),
        )
    };
    let valid = b"10000-18000 rw-s 00000000 08:01 42 /ignored-path\n";
    assert_eq!(scan(valid), Some(true));
    let mut duplicate = valid.to_vec();
    duplicate.extend(valid);
    assert_eq!(scan(&duplicate), None);
    assert_eq!(scan(b"10000-18000 rw-p 00000000 08:01 42\n"), None);
    assert_eq!(scan(b"10000-18000 rw-s 00000001 08:01 42\n"), None);
    assert_eq!(scan(b"10000-18000 rw-s 00000000 08:02 42\n"), None);
    assert_eq!(scan(b"10000-18000 rw-s 00000000 08:01 43\n"), None);
    assert_eq!(scan(b"20000-28000 rw-s 00000000 08:01 42\n"), Some(false));
    assert_eq!(scan(b"10000-18000 rw-s 00000000 08:01"), None);
    let mut long = valid[..valid.len() - 1].to_vec();
    long.extend(std::iter::repeat_n(b'x', 100_000));
    long.push(b'\n');
    assert_eq!(scan(&long), Some(true));
    let mut oversized = long.clone();
    oversized.extend(std::iter::repeat_n(b'x', 2 * 1024 * 1024));
    assert_eq!(scan(&oversized), None);
    let expired =
        super::mapped_identity_stream(Cursor::new(valid), 0x10000, device, 42, Instant::now());
    assert_eq!(expired, None);
    // A fifth token split at the prefix ceiling cannot silently become an accepted inode.
    let mut partial = b"10000-18000 rw-s 00000000 08:01 ".to_vec();
    partial.extend(std::iter::repeat_n(b'0', 250));
    partial.extend(b"42\n");
    assert_eq!(scan(&partial), None);
}

#[test]
fn original_native_expired_observation_reports_deadline_without_mutation() {
    let f = fixture();
    let changes = f.connection.total_changes();
    assert_eq!(
        f.connection
            .observe_retained_wal_objects(&f.root, Path::new("test.db"), Instant::now()),
        Err(WalObservationRefusal::Deadline),
    );
    assert_eq!(changes, f.connection.total_changes());
    assert!(!f.connection.is_autocommit());
    assert!(capture(&f).is_some());
    f.connection.execute_batch("ROLLBACK").unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn original_map_stream_io_refusal_is_not_relabelled_after_deadline() {
    // Stream-parser contract only; the genuine Global/native tests cover producer behavior.
    struct FailingRead {
        deadline: Instant,
        entered: bool,
    }
    impl std::io::Read for FailingRead {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            self.entered = true;
            while Instant::now() < self.deadline {
                std::thread::park_timeout(self.deadline.saturating_duration_since(Instant::now()));
            }
            Err(std::io::Error::other("original read refusal"))
        }
    }
    let original_deadline = Instant::now() + Duration::from_secs(1);
    let mut reader = FailingRead {
        deadline: original_deadline,
        entered: false,
    };
    let mut tracker = ObservationDeadline::new(original_deadline);
    let result =
        super::mapped_identity_stream_with_deadline(&mut reader, 0x10000, 7, 42, &mut tracker);
    assert!(reader.entered);
    assert_eq!(
        tracker.outcome(result),
        Err(WalObservationRefusal::Unavailable)
    );
    assert!(Instant::now() >= original_deadline);

    // An actual original stream clock check, instead, records Deadline.
    let mut expired = ObservationDeadline::new(Instant::now());
    let result =
        super::mapped_identity_stream_with_deadline(std::io::empty(), 0x10000, 7, 42, &mut expired);
    assert_eq!(
        expired.outcome(result),
        Err(WalObservationRefusal::Deadline)
    );
}
