//! Normal-build genuine source+journal/profile observation, never an adoption/capacity grant.
#![cfg(all(
    not(feature = "test-support"),
    any(target_os = "linux", target_os = "macos")
))]
use std::path::PathBuf;
use tempfile::{TempDir, tempdir};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    operation_journal::store::{
        JOURNAL_DATABASE_FILE_NAME, JOURNAL_DATABASE_ROOT_NAME, OperationJournalStore,
    },
    sample_sources::{
        DB_FILE_NAME, SourceDatabase,
        db::{LegacyAllocationRole, LegacySourceReadOwner},
    },
};
struct Fixture {
    _root: AppRootGuard,
    profile: TempDir,
    _media: TempDir,
    guard: WritableProfileGuard,
    source: LegacySourceReadOwner,
    journal: OperationJournalStore,
}
impl Fixture {
    fn new() -> Self {
        let profile = tempdir().unwrap();
        let media = tempdir().unwrap();
        let root = AppRootGuard::set(profile.path().to_path_buf()).unwrap();
        std::fs::create_dir(profile.path().join("legacy")).unwrap();
        rusqlite::Connection::open(profile.path().join("legacy").join(DB_FILE_NAME))
            .unwrap()
            .execute_batch(include_str!("fixtures/profile-v11/source.sql"))
            .unwrap();
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let journal = OperationJournalStore::open(&guard).unwrap();
        let source =
            SourceDatabase::open_legacy_read_owner(media.path(), &guard, "legacy").unwrap();
        Self {
            _root: root,
            profile,
            _media: media,
            guard,
            source,
            journal,
        }
    }
    fn source_path(&self) -> PathBuf {
        self.profile.path().join("legacy").join(DB_FILE_NAME)
    }
    fn journal_path(&self) -> PathBuf {
        self.profile
            .path()
            .join(JOURNAL_DATABASE_ROOT_NAME)
            .join(JOURNAL_DATABASE_FILE_NAME)
    }
}
#[test]
fn actual_common_capture_preserves_original_bytes_and_accounts_all_objects_once_on_aliased_volume()
{
    let f = Fixture::new();
    let source_bytes = std::fs::read(f.source_path()).unwrap();
    let journal_bytes = std::fs::read(f.journal_path()).unwrap();
    let result = f
        .source
        .capture_common_allocation_observation(&f.guard, &f.journal)
        .unwrap();
    assert_eq!(
        result,
        f.source
            .capture_common_allocation_observation(&f.guard.try_clone().unwrap(), &f.journal)
            .unwrap()
    );
    assert_eq!(result.source().image().revision(), 7);
    assert_eq!(result.objects().len(), 8);
    assert_eq!(result.volumes().len(), 1);
    assert_eq!(
        result.volumes()[0].proposed_logical_bytes(),
        result.source().source_logical_peak_bytes()
            + result.journal().proposed_journal_logical_peak_bytes()
            + 8192
    );
    let roles: std::collections::BTreeSet<_> = result.objects().iter().map(|o| o.role()).collect();
    assert_eq!(roles.len(), 8);
    let identities: std::collections::BTreeSet<_> = result
        .objects()
        .iter()
        .map(|o| o.object_identity())
        .collect();
    assert_eq!(identities.len(), 8);
    for role in [
        LegacyAllocationRole::SourceRollback,
        LegacyAllocationRole::SourceFutureWal,
        LegacyAllocationRole::SourceFutureShm,
    ] {
        let o = result.objects().iter().find(|o| o.role() == role).unwrap();
        assert_eq!(o.observed_bytes(), 0);
        assert!(o.proposed_logical_bytes() > 0);
    }
    assert_eq!(std::fs::read(f.source_path()).unwrap(), source_bytes);
    assert_eq!(std::fs::read(f.journal_path()).unwrap(), journal_bytes);
    assert_eq!(
        f.journal.recovery_readiness().unwrap(),
        yield_library::operation_journal::OperationJournalRecoveryReadiness::Clear
    );
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(
            !f.source_path()
                .with_file_name(format!("{DB_FILE_NAME}{suffix}"))
                .exists()
        );
    }
}
#[test]
fn actual_common_capture_refuses_foreign_journal_acquisition_and_preserves_both_profiles() {
    let f = Fixture::new();
    let other = tempdir().unwrap();
    let _root = AppRootGuard::set(other.path().to_path_buf()).unwrap();
    let foreign = WritableProfileGuard::acquire_current().unwrap();
    let journal = OperationJournalStore::open(&foreign).unwrap();
    let original = std::fs::read(f.source_path()).unwrap();
    assert!(
        f.source
            .capture_common_allocation_observation(&f.guard, &journal)
            .is_err()
    );
    assert!(
        f.source
            .capture_common_allocation_observation(&foreign, &f.journal)
            .is_err()
    );
    assert_eq!(std::fs::read(f.source_path()).unwrap(), original);
    assert_eq!(
        journal.recovery_readiness().unwrap(),
        yield_library::operation_journal::OperationJournalRecoveryReadiness::Clear
    );
}
#[test]
fn actual_common_capture_refuses_changed_source_and_replaced_journal_or_source_control() {
    for which in ["source-rows", "journal", "source-control"] {
        let f = Fixture::new();
        assert!(
            f.source
                .capture_common_allocation_observation(&f.guard, &f.journal)
                .is_ok()
        );
        if which == "source-rows" {
            rusqlite::Connection::open(f.source_path())
                .unwrap()
                .execute("UPDATE metadata SET value='8' WHERE key='revision'", [])
                .unwrap();
        } else {
            let path = if which == "journal" {
                f.journal_path()
            } else {
                f.profile.path().join("legacy/.yield-source-lease.lock")
            };
            std::fs::rename(&path, path.with_extension("retained-original")).unwrap();
            std::fs::write(path, b"replacement evidence").unwrap();
        }
        let original = std::fs::read(f.source_path()).unwrap();
        assert!(
            f.source
                .capture_common_allocation_observation(&f.guard, &f.journal)
                .is_err(),
            "{which}"
        );
        assert_eq!(std::fs::read(f.source_path()).unwrap(), original);
    }
}

#[cfg(target_os = "linux")]
mod cross_volume {
    use super::*;
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    struct MountGuard(PathBuf);
    impl Drop for MountGuard {
        fn drop(&mut self) {
            let path = CString::new(self.0.as_os_str().as_bytes()).unwrap();
            // SAFETY: owned test mount path, borrowed only during this syscall. Namespace dies
            // with the child even on assertion failure; no parent/global mount is changed.
            assert_eq!(unsafe { libc::umount2(path.as_ptr(), 0) }, 0);
        }
    }
    #[test]
    fn actual_mounted_source_main_child() {
        if std::env::var_os("YIELD_LEGACY_COMMON_MOUNT_CHILD").is_none() {
            return;
        }
        let profile = tempdir().unwrap();
        let media = tempdir().unwrap();
        let backing = tempfile::tempdir_in("/dev/shm").unwrap();
        let root = AppRootGuard::set(profile.path().to_path_buf()).unwrap();
        let child = profile.path().join("legacy");
        std::fs::create_dir(&child).unwrap();
        let source_main = child.join(DB_FILE_NAME);
        rusqlite::Connection::open(&source_main)
            .unwrap()
            .execute_batch(include_str!("fixtures/profile-v11/source.sql"))
            .unwrap();
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let initial_journal = OperationJournalStore::open(&guard).unwrap();
        drop(initial_journal); // Genuine store bootstrap/reopen, never concurrent journal owners.
        let journal_mount = std::env::var("YIELD_LEGACY_COMMON_MOUNT_KIND").unwrap() == "journal";
        let main = if journal_mount {
            profile
                .path()
                .join(JOURNAL_DATABASE_ROOT_NAME)
                .join(JOURNAL_DATABASE_FILE_NAME)
        } else {
            source_main
        };
        let other_main = backing.path().join("original.sqlite");
        std::fs::copy(&main, &other_main).unwrap();
        let from = CString::new(other_main.as_os_str().as_bytes()).unwrap();
        let to = CString::new(main.as_os_str().as_bytes()).unwrap();
        // SAFETY: current child has private user/mount namespaces; direct test file paths remain
        // alive. MS_BIND reads neither filesystem-type nor data pointers and retains no pointers.
        assert_eq!(
            unsafe {
                libc::mount(
                    from.as_ptr(),
                    to.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                )
            },
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        let mount = MountGuard(main.clone());
        let bytes = std::fs::read(&main).unwrap();
        let journal = OperationJournalStore::open(&guard).unwrap();
        let source =
            SourceDatabase::open_legacy_read_owner(media.path(), &guard, "legacy").unwrap();
        let result = source
            .capture_common_allocation_observation(&guard, &journal)
            .unwrap();
        let main_file = std::fs::File::open(&main).unwrap();
        let directory = std::fs::File::open(&child).unwrap();
        let main_facts =
            yield_library::durable_capacity::descriptor_capacity_facts(&main_file).unwrap();
        let directory_facts =
            yield_library::durable_capacity::descriptor_capacity_facts(&directory).unwrap();
        assert_ne!(main_facts.identity, directory_facts.identity);
        assert_eq!(result.volumes().len(), 2);
        let mounted_role = if journal_mount {
            LegacyAllocationRole::JournalMain
        } else {
            LegacyAllocationRole::SourceMain
        };
        let mounted_main = result
            .objects()
            .iter()
            .find(|o| o.role() == mounted_role)
            .unwrap();
        assert_eq!(mounted_main.volume(), &main_facts.identity);
        assert_eq!(
            result
                .volumes()
                .iter()
                .find(|v| v.volume() == &main_facts.identity)
                .unwrap()
                .proposed_logical_bytes(),
            mounted_main.proposed_logical_bytes()
        );
        for role in [
            LegacyAllocationRole::SourceRollback,
            LegacyAllocationRole::SourceFutureWal,
            LegacyAllocationRole::SourceFutureShm,
            LegacyAllocationRole::SourceReservationControl,
            LegacyAllocationRole::JournalRollback,
            LegacyAllocationRole::ProfileOwnerControl,
        ] {
            assert_eq!(
                result
                    .objects()
                    .iter()
                    .find(|o| o.role() == role)
                    .unwrap()
                    .volume(),
                &directory_facts.identity
            );
        }
        assert_eq!(std::fs::read(&main).unwrap(), bytes);
        assert!(!child.join(format!("{DB_FILE_NAME}-journal")).exists());
        assert_eq!(
            result,
            source
                .capture_common_allocation_observation(&guard, &journal)
                .unwrap()
        );
        drop(source);
        drop(main_file);
        drop(directory);
        drop(journal);
        drop(mount);
        drop(guard);
        drop(root);
        assert_eq!(std::fs::read(other_main).unwrap(), bytes);
        // tmpfs/overlay observations prove descriptor placement only, not power-loss semantics.
    }
    #[test]
    fn actual_mounted_main_and_sidecar_directory_volumes_are_observed_in_real_process() {
        for kind in ["source", "journal"] {
            let output = std::process::Command::new("unshare")
                .args(["--user", "--map-root-user", "--mount"])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cross_volume::actual_mounted_source_main_child",
                    "--test-threads=1",
                ])
                .env("YIELD_LEGACY_COMMON_MOUNT_CHILD", "1")
                .env("YIELD_LEGACY_COMMON_MOUNT_KIND", kind)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child failed: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        }
    }
}
