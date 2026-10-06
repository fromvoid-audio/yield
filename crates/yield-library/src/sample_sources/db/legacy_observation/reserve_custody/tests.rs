use super::*;
#[test]
fn original_prelease_bootstrap_proof_requires_fence_live_source_and_same_deadline() {
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let media = tempfile::tempdir().unwrap();
    let _root = crate::app_dirs::AppRootGuard::set(temp.path().to_owned()).unwrap();
    let child = temp.path().join("legacy");
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
    let mut owner =
        SourceDatabase::open_legacy_read_owner(media.path(), &profile, "legacy").unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let image = owner.capture(&profile).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    assert!(owner.reserve_use_proof(&profile, deadline).is_err());
    owner
        .prepare_no_effect_writable_until(&profile, deadline)
        .unwrap();
    let proof = owner.reserve_use_proof(&profile, deadline).unwrap();
    proof.validate_for_bootstrap().unwrap();
    let targets = owner.reserve_targets(&profile, deadline).unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].root.identity(), owner.binding.root().identity());
    proof
        .validate_root_for_bootstrap(targets[0].root.as_ref())
        .unwrap();
    let unrelated = BoundDatabaseRoot::for_external_source_root(&profile, media.path()).unwrap();
    assert!(proof.validate_root_for_bootstrap(&unrelated).is_err());
    assert!(owner.reserve_use_proof(&profile, Instant::now()).is_err());
    let sidecar = child.join(format!("{}-journal", crate::sample_sources::DB_FILE_NAME));
    std::fs::write(&sidecar, b"changed evidence").unwrap();
    assert!(proof.validate_for_bootstrap().is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(std::fs::read(&sidecar).unwrap(), b"changed evidence");
    std::fs::remove_file(sidecar).unwrap(); // Only the fixture removes its injected sidecar.
    assert_eq!(owner.capture(&profile).unwrap(), image);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE metadata SET value='8' WHERE key='revision'", [])
        .unwrap();
    // Still-valid v11 rows cannot silently become the original proof's fresh authority.
    assert!(proof.validate_for_bootstrap().is_err());
}
#[test]
fn explicitly_retained_main_reserve_root_refuses_replacement_without_mutating_source() {
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let media = tempfile::tempdir().unwrap();
    let _root = crate::app_dirs::AppRootGuard::set(temp.path().to_owned()).unwrap();
    let child = temp.path().join("legacy");
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
    let mut owner =
        SourceDatabase::open_legacy_read_owner(media.path(), &profile, "legacy").unwrap();
    let root = temp.path().join("explicit-reserve");
    std::fs::create_dir(&root).unwrap();
    owner.bind_legacy_main_reserve_root(&root).unwrap();
    assert!(owner.bind_legacy_main_reserve_root(&root).is_err());
    let bytes = std::fs::read(&path).unwrap();
    std::fs::rename(&root, temp.path().join("original-reserve")).unwrap();
    std::fs::create_dir(&root).unwrap();
    assert!(
        owner
            .reserve_targets(&profile, Instant::now() + Duration::from_secs(2))
            .is_err()
    );
    assert!(std::fs::read_dir(&root).unwrap().next().is_none());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn original_prelease_proof_refuses_expiry_after_actual_targets_and_root_validation_return() {
    // Test-only holds delay return from genuine Native observation; no fake authority or floor.
    fn hold(deadline: Instant) {
        while Instant::now() < deadline {
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        }
    }
    for after_root in [false, true] {
        let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
        let temp = tempfile::tempdir().unwrap();
        let media = tempfile::tempdir().unwrap();
        let _app = crate::app_dirs::AppRootGuard::set(temp.path().to_owned()).unwrap();
        let child = temp.path().join("legacy");
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
        let mut source =
            SourceDatabase::open_legacy_read_owner(media.path(), &profile, "legacy").unwrap();
        let original = std::fs::read(&path).unwrap();
        source
            .prepare_no_effect_writable_until(&profile, Instant::now() + Duration::from_secs(2))
            .unwrap();
        let targets = source
            .reserve_targets(&profile, Instant::now() + Duration::from_secs(2))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut proof = source.reserve_use_proof(&profile, deadline).unwrap();
        if after_root {
            proof.before_final_root = Some(hold);
            assert!(
                proof
                    .validate_root_for_bootstrap(targets[0].root.as_ref())
                    .is_err()
            );
        } else {
            proof.before_final_targets = Some(hold);
            assert!(proof.validate_for_bootstrap().is_err());
        }
        assert!(Instant::now() >= deadline);
        assert!(proof.validate_for_bootstrap().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(!child.join("recovery-reserve.bin").exists());
        assert!(source.connection.has_no_effect_fence());
    }
}
