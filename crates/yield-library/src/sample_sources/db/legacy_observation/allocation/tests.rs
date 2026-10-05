use super::*;
use crate::sample_sources::db::SourceDatabase;

#[test]
fn original_delete_sector_callback_observation_never_uses_ordinary_sql_or_writes() {
    with_owner(|owner, _profile, path| {
        let bytes = std::fs::read(path).unwrap();
        let before = owner.connection.known_delete_sector_size().unwrap();
        assert!(before.is_power_of_two() && (512..=65536).contains(&before));
        #[cfg(windows)]
        assert_eq!(before, 4096, "actual bundled winSectorSize callback");
        owner
            .connection
            .authorizer(Some(|_: rusqlite::hooks::AuthContext<'_>| {
                rusqlite::hooks::Authorization::Deny
            }));
        for _ in 0..8 {
            assert_eq!(owner.connection.known_delete_sector_size(), Some(before));
        }
        assert!(owner.connection.is_autocommit());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        owner.connection.authorizer(
            None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>,
        );
        assert_eq!(owner.connection.known_delete_sector_size(), Some(before));
        assert!(
            !path
                .with_file_name(format!("{}-wal", crate::sample_sources::DB_FILE_NAME))
                .exists()
        );
        assert!(
            !path
                .with_file_name(format!("{}-shm", crate::sample_sources::DB_FILE_NAME))
                .exists()
        );
        assert!(
            !path
                .with_file_name(format!("{}-journal", crate::sample_sources::DB_FILE_NAME))
                .exists()
        );
    });
}

fn with_owner(run: impl FnOnce(&LegacySourceReadOwner, &WritableProfileGuard, &std::path::Path)) {
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let profile = tempfile::tempdir().unwrap();
    let media = tempfile::tempdir().unwrap();
    let _root = crate::app_dirs::AppRootGuard::set(profile.path().to_path_buf()).unwrap();
    let child = profile.path().join("legacy");
    std::fs::create_dir(&child).unwrap();
    let path = child.join(crate::sample_sources::DB_FILE_NAME);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/profile-v11/source.sql"
        )))
        .unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let owner = SourceDatabase::open_legacy_read_owner(media.path(), &profile, "legacy").unwrap();
    run(&owner, &profile, &path);
}
#[test]
fn legacy_pager_real_smaller_connection_cap_is_never_raised_or_written_to_source() {
    with_owner(|owner, profile, path| {
        let bytes = std::fs::read(path).unwrap();
        let before = owner.capture_pager_observation(profile).unwrap();
        let smaller = before.page_count() + 1;
        owner
            .connection
            .pragma_update(None, "max_page_count", smaller)
            .unwrap();
        let after = owner.capture_pager_observation(profile).unwrap();
        assert_eq!(after.existing_max_pages(), smaller);
        assert_eq!(after.selected_max_pages(), smaller);
        assert_eq!(after.image(), before.image());
        assert!(after.source_logical_peak_bytes() < before.source_logical_peak_bytes());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    });
}
#[test]
fn legacy_pager_real_policy_change_and_expired_deadline_refuse_without_reset_or_write() {
    with_owner(|owner, profile, path| {
        let bytes = std::fs::read(path).unwrap();
        assert!(owner.capture_pager_until(profile, Instant::now()).is_err());
        owner
            .connection
            .pragma_update(None, "cache_spill", true)
            .unwrap();
        assert!(owner.capture_pager_observation(profile).is_err());
        let spill: u64 = owner
            .connection
            .pragma_query_value(None, "cache_spill", |r| r.get(0))
            .unwrap();
        assert_ne!(
            spill, 0,
            "failed capture must not silently reinstall policy"
        );
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    });
}
#[test]
fn legacy_pager_checked_formulas_caps_and_overflow_reject_unknown_instead_of_zero() {
    for p in [512u64, 1024, 4096, 8192, 65536] {
        let m = OUTPUT_CAP / p;
        let b = peak(p, m).unwrap();
        assert_eq!(b.database, OUTPUT_CAP);
        assert_eq!(b.rollback, (m + 1) * 65536 + m * (p + 8));
        assert_eq!(b.wal, 32 + m * (p + 24));
        assert_eq!(b.shm, 32768 + 16 * m);
        assert_eq!(b.total, b.database + b.rollback + b.wal + b.shm);
        assert_eq!(select_pages(p, u64::MAX, 1, p), Some(m));
        assert_eq!(select_pages(p, 2, 1, p), Some(2));
        assert!(select_pages(p, 2, 3, 3 * p).is_none());
    }
    for (p, m) in [
        (0, 1),
        (1, 0),
        (u64::MAX, 1),
        (4096, u64::MAX),
        (u64::MAX, u64::MAX),
    ] {
        assert!(peak(p, m).is_none());
    }
    for (p, m, c, main) in [
        (0, 1, 1, 1),
        (513, 1, 1, 513),
        (4096, 0, 1, 4096),
        (4096, 1, 0, 4096),
        (4096, 1, 1, 4095),
        (4096, 1, 1, 8192),
    ] {
        assert!(select_pages(p, m, c, main).is_none());
    }
}
#[test]
fn legacy_pager_real_connection_without_retained_known_vfs_never_qualifies() {
    let raw: crate::sqlite_budget::OwnedConnection =
        rusqlite::Connection::open_in_memory().unwrap().into();
    assert!(raw.known_delete_sector_size().is_none());
}

#[test]
fn legacy_pager_tainted_extension_runtime_child() {
    if std::env::var_os("YIELD_LEGACY_PAGER_TAINT_CHILD").is_none() {
        return;
    }
    with_owner(|owner, profile, path| {
        let bytes = std::fs::read(path).unwrap();
        assert!(owner.capture_pager_observation(profile).is_ok());
        assert!(
            crate::sqlite_ext::load_extension_for_test(
                &owner.connection,
                std::path::Path::new("/nonexistent/yield-legacy-extension.so")
            )
            .is_err()
        );
        assert!(owner.capture_pager_observation(profile).is_err());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    });
}
#[test]
fn legacy_pager_tainted_extension_runtime_is_refused_in_real_isolated_process() {
    let output=std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact","sample_sources::db::legacy_observation::allocation::tests::legacy_pager_tainted_extension_runtime_child","--test-threads=1"])
        .env("YIELD_LEGACY_PAGER_TAINT_CHILD","1").output().unwrap();
    assert!(
        output.status.success(),
        "child failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}
