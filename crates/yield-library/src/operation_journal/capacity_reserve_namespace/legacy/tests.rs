use super::*;
use crate::{
    app_dirs::AppRootGuard,
    sample_sources::{DB_FILE_NAME, SourceDatabase},
    test_runtime::TestRuntimeGuard,
};
use std::{
    io::Write,
    time::{Duration, Instant},
};

#[test]
fn legacy_reserve_original_child_create_and_directory_sync_refuse_lost_proof_with_exact_backing() {
    for before_create in [true, false] {
        let _test = TestRuntimeGuard::acquire();
        let temp = tempfile::tempdir().unwrap();
        let media = tempfile::tempdir().unwrap();
        let _app = AppRootGuard::set(temp.path().to_owned()).unwrap();
        let child = temp.path().join("legacy");
        std::fs::create_dir(&child).unwrap();
        let source_path = child.join(DB_FILE_NAME);
        rusqlite::Connection::open(&source_path)
            .unwrap()
            .execute_batch(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/profile-v11/source.sql"
            )))
            .unwrap();
        let profile = WritableProfileGuard::acquire_current().unwrap();
        let mut source =
            SourceDatabase::open_legacy_read_owner(media.path(), &profile, "legacy").unwrap();
        let original = std::fs::read(&source_path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        source
            .prepare_no_effect_writable_until(&profile, deadline)
            .unwrap();
        let proof = source.reserve_use_proof(&profile, deadline).unwrap();
        let targets = source.reserve_targets(&profile, deadline).unwrap();
        let root = targets[0].root.as_ref();
        let path = child.join(RESERVE_FILE_NAME);
        let sidecar = child.join(format!("{DB_FILE_NAME}-journal"));
        if before_create {
            std::fs::write(&sidecar, b"retained source evidence").unwrap();
            let failure = open_legacy_new(root, &profile, &proof).unwrap_err();
            assert_eq!(failure.stage(), ReserveNamespaceFailureStage::Open);
            assert!(failure.owned_file.is_none());
            assert!(!path.exists());
        } else {
            let mut file = open_legacy_new(root, &profile, &proof).unwrap();
            file.write_all(&[0x6b; 4096]).unwrap();
            let identity =
                crate::filesystem_identity::stable_filesystem_identity_from_open_file(&file)
                    .unwrap();
            // Logical ProtectedReserve fixture only; refusal cannot issue a qualified reserve.
            let reserve = super::super::super::capacity_reserve::test_fixture_reserve(file);
            std::fs::write(&sidecar, b"retained source evidence").unwrap();
            let failure = match finish_legacy_created_reserve(root, &profile, reserve, &proof) {
                Err(failure) => failure,
                Ok(_) => panic!("lost source proof cannot complete reserve qualification"),
            };
            assert_eq!(failure.stage(), ReserveNamespaceFailureStage::Sync);
            assert!(matches!(
                failure.owned_file.as_ref(),
                Some(OwnedFailureFile::Protected(_))
            ));
            let backing = failure.into_legacy_backing();
            let held = backing.file.as_ref().unwrap();
            assert_eq!(
                crate::filesystem_identity::stable_filesystem_identity_from_open_file(held)
                    .unwrap(),
                identity
            );
            assert_eq!(held.metadata().unwrap().len(), 4096);
            assert_eq!(std::fs::read(&path).unwrap(), vec![0x6b; 4096]);
            drop(backing);
            assert_eq!(std::fs::read(&path).unwrap(), vec![0x6b; 4096]);
        }
        assert_eq!(std::fs::read(&source_path).unwrap(), original);
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            b"retained source evidence"
        );
    }
}

#[cfg(not(windows))]
#[test]
fn legacy_namespace_second_native_sync_refuses_original_deadline_after_first_sync() {
    thread_local! {
        static HOLD: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
        static CALLS: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
    }
    fn after_sync(call: u8) {
        CALLS.set(call);
        let deadline = HOLD.get().unwrap();
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    }
    let _test = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let media = tempfile::tempdir().unwrap();
    let _app = AppRootGuard::set(temp.path().to_owned()).unwrap();
    let child = temp.path().join("legacy");
    std::fs::create_dir(&child).unwrap();
    let source_path = child.join(DB_FILE_NAME);
    rusqlite::Connection::open(&source_path)
        .unwrap()
        .execute_batch(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/profile-v11/source.sql"
        )))
        .unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let mut source =
        SourceDatabase::open_legacy_read_owner(media.path(), &profile, "legacy").unwrap();
    let original = std::fs::read(&source_path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    source
        .prepare_no_effect_writable_until(&profile, deadline)
        .unwrap();
    let proof = source.reserve_use_proof(&profile, deadline).unwrap();
    let targets = source.reserve_targets(&profile, deadline).unwrap();
    let root = targets[0].root.as_ref();
    let mut file = open_legacy_new(root, &profile, &proof).unwrap();
    file.write_all(&[0x7c; 4096]).unwrap();
    let identity =
        crate::filesystem_identity::stable_filesystem_identity_from_open_file(&file).unwrap();
    // This logical protected fixture supplies no production physical qualification.
    let reserve = super::super::super::capacity_reserve::test_fixture_reserve(file);
    HOLD.set(Some(deadline));
    let failure = match finish_legacy_created_reserve_inner(
        root,
        &profile,
        reserve,
        &proof,
        Some(after_sync),
    ) {
        Err(failure) => failure,
        Ok(_) => panic!("expired original proof must refuse the second native sync"),
    };
    assert_eq!(
        CALLS.get(),
        1,
        "only the first actual native sync completed"
    );
    HOLD.set(None);
    assert_eq!(failure.stage(), ReserveNamespaceFailureStage::Sync);
    assert!(matches!(
        failure.owned_file.as_ref(),
        Some(OwnedFailureFile::Protected(_))
    ));
    let backing = failure.into_legacy_backing();
    let held = backing.file.as_ref().unwrap();
    assert_eq!(
        crate::filesystem_identity::stable_filesystem_identity_from_open_file(held).unwrap(),
        identity
    );
    assert_eq!(held.metadata().unwrap().len(), 4096);
    drop(backing);
    assert_eq!(
        std::fs::read(child.join(RESERVE_FILE_NAME)).unwrap(),
        vec![0x7c; 4096]
    );
    assert_eq!(std::fs::read(&source_path).unwrap(), original);
    assert!(!child.join(format!("{DB_FILE_NAME}-journal")).exists());
}
