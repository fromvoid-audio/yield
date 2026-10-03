//! Normal-build actual journal/profile capture; no fixture capacity or migration permission.
#![cfg(not(feature = "test-support"))]
use tempfile::{TempDir, tempdir};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    operation_journal::store::{
        JOURNAL_DATABASE_FILE_NAME, JOURNAL_DATABASE_ROOT_NAME, OperationJournalStore,
    },
};
struct Fixture {
    _root: AppRootGuard,
    directory: TempDir,
    guard: WritableProfileGuard,
    store: OperationJournalStore,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempdir().unwrap();
        let root = AppRootGuard::set(directory.path().to_path_buf()).unwrap();
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let store = OperationJournalStore::open(&guard).unwrap();
        Self {
            _root: root,
            directory,
            guard,
            store,
        }
    }
    fn path(&self) -> std::path::PathBuf {
        self.directory
            .path()
            .join(JOURNAL_DATABASE_ROOT_NAME)
            .join(JOURNAL_DATABASE_FILE_NAME)
    }
}
#[test]
fn actual_retained_journal_observation_preserves_bytes_and_carries_only_historical_facts() {
    let f = Fixture::new();
    let bytes = std::fs::read(f.path()).unwrap();
    let lock = std::fs::read(f.directory.path().join("profile-owner.lock")).unwrap();
    let observation = f
        .store
        .capture_legacy_journal_observation(&f.guard)
        .unwrap();
    assert_eq!(
        observation,
        f.store
            .capture_legacy_journal_observation(&f.guard.try_clone().unwrap())
            .unwrap()
    );
    assert_eq!(observation.main_bytes(), bytes.len() as u64);
    assert_eq!(observation.profile_lock_bytes(), lock.len() as u64);
    assert_eq!(observation.owner_identity(), f.guard.owner_identity());
    assert_eq!(
        observation.selected_max_pages(),
        observation
            .current_max_pages()
            .min(512 * 1024 * 1024 / observation.page_size())
    );
    assert_eq!(
        observation.proposed_database_bytes(),
        observation.page_size() * observation.selected_max_pages()
    );
    let m = observation.selected_max_pages();
    assert_eq!(
        observation.proposed_rollback_bytes(),
        (m + 1) * 65536 + m * (observation.page_size() + 8)
    );
    assert_eq!(
        observation.proposed_journal_logical_peak_bytes(),
        observation.proposed_database_bytes() + observation.proposed_rollback_bytes()
    );
    assert_eq!(observation.volume(), observation.profile_lock_volume());
    assert!(observation.allocation_unit() > 0);
    assert!((512..=65536).contains(&observation.sector_size()));
    assert_eq!(std::fs::read(f.path()).unwrap(), bytes);
    assert_eq!(
        std::fs::read(f.directory.path().join("profile-owner.lock")).unwrap(),
        lock
    );
    assert!(
        !f.path()
            .with_file_name(format!("{JOURNAL_DATABASE_FILE_NAME}-wal"))
            .exists()
    );
    assert!(
        !f.path()
            .with_file_name(format!("{JOURNAL_DATABASE_FILE_NAME}-shm"))
            .exists()
    );
    assert_eq!(
        f.store.recovery_readiness().unwrap(),
        yield_library::operation_journal::OperationJournalRecoveryReadiness::Clear
    );
}
#[test]
fn actual_observer_refuses_foreign_profile_without_creating_an_intent() {
    let f = Fixture::new();
    let other = tempdir().unwrap();
    let _other_root = AppRootGuard::set(other.path().to_path_buf()).unwrap();
    let foreign = WritableProfileGuard::acquire_current().unwrap();
    let bytes = std::fs::read(f.path()).unwrap();
    assert!(
        f.store
            .capture_legacy_journal_observation(&foreign)
            .is_err()
    );
    assert_eq!(std::fs::read(f.path()).unwrap(), bytes);
}
#[test]
fn actual_observer_refuses_database_root_and_profile_lock_substitution() {
    for which in ["database", "root", "profile-lock"] {
        let f = Fixture::new();
        let original = match which {
            "database" => f.path(),
            "root" => f.path().parent().unwrap().to_path_buf(),
            _ => f.directory.path().join("profile-owner.lock"),
        };
        let moved = original.with_extension("original");
        std::fs::rename(&original, &moved).unwrap();
        if which == "root" {
            std::fs::create_dir(&original).unwrap();
        } else {
            std::fs::write(&original, b"replacement evidence").unwrap();
        }
        assert!(
            f.store
                .capture_legacy_journal_observation(&f.guard)
                .is_err(),
            "{which}"
        );
        if which != "root" {
            assert_eq!(std::fs::read(&original).unwrap(), b"replacement evidence");
        }
    }
}
#[test]
fn actual_observer_refuses_new_wal_shm_evidence_without_removing_it() {
    for extension in ["wal", "shm"] {
        let f = Fixture::new();
        let path = f
            .path()
            .with_file_name(format!("{JOURNAL_DATABASE_FILE_NAME}-{extension}"));
        std::fs::write(&path, b"unexpected evidence").unwrap();
        let bytes = std::fs::read(f.path()).unwrap();
        assert!(
            f.store
                .capture_legacy_journal_observation(&f.guard)
                .is_err()
        );
        assert_eq!(std::fs::read(path).unwrap(), b"unexpected evidence");
        assert_eq!(std::fs::read(f.path()).unwrap(), bytes);
    }
}
