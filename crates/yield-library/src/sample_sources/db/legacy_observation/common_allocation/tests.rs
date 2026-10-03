use super::*;
use crate::sample_sources::db::SourceDatabase;
fn fixture() -> (
    crate::test_runtime::TestRuntimeGuard,
    tempfile::TempDir,
    tempfile::TempDir,
    crate::app_dirs::AppRootGuard,
    WritableProfileGuard,
    LegacySourceReadOwner,
    OperationJournalStore,
) {
    let runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let profile = tempfile::tempdir().unwrap();
    let media = tempfile::tempdir().unwrap();
    let root = crate::app_dirs::AppRootGuard::set(profile.path().to_path_buf()).unwrap();
    let path = profile.path().join("legacy");
    std::fs::create_dir(&path).unwrap();
    rusqlite::Connection::open(path.join(super::super::super::DB_FILE_NAME))
        .unwrap()
        .execute_batch(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/profile-v11/source.sql"
        )))
        .unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let journal = OperationJournalStore::open(&guard).unwrap();
    let owner = SourceDatabase::open_legacy_read_owner(media.path(), &guard, "legacy").unwrap();
    (runtime, profile, media, root, guard, owner, journal)
}
#[test]
fn common_capture_has_one_original_deadline_and_refuses_changed_original_control() {
    let (_runtime, profile, _media, _root, guard, source, journal) = fixture();
    let bytes = std::fs::read(
        profile
            .path()
            .join("legacy")
            .join(super::super::super::DB_FILE_NAME),
    )
    .unwrap();
    assert!(
        source
            .capture_common_until(&guard, &journal, Instant::now() - Duration::from_nanos(1))
            .is_err()
    );
    let image = source
        .capture_common_allocation_observation(&guard, &journal)
        .unwrap();
    assert_eq!(image.objects().len(), 8);
    std::fs::write(
        profile.path().join("legacy/.yield-source-lease.lock"),
        vec![0; 1],
    )
    .unwrap();
    assert!(
        source
            .capture_common_allocation_observation(&guard, &journal)
            .is_err()
    );
    assert_eq!(
        std::fs::read(
            profile
                .path()
                .join("legacy")
                .join(super::super::super::DB_FILE_NAME)
        )
        .unwrap(),
        bytes
    );
}
#[test]
fn checked_object_volume_composition_preserves_all_eight_distinct_roles_and_aliases_once() {
    let (_runtime, _profile, _media, _root, guard, source, journal) = fixture();
    let mut image = source
        .capture_common_allocation_observation(&guard, &journal)
        .unwrap();
    assert_eq!(image.volumes().len(), 1);
    let expected = image.objects().iter().map(|o| o.allowance).sum::<u64>();
    assert_eq!(image.volumes()[0].logical_bytes, expected);
    // Pure composition boundary tests, never a physical/native multi-volume qualification.
    let other = VolumeIdentity {
        device: image.objects[0].volume.device.checked_add(1).unwrap(),
    };
    image.objects[0].volume = other.clone();
    let result = aggregate(&image.objects).unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(
        result
            .iter()
            .find(|v| v.volume == other)
            .unwrap()
            .logical_bytes,
        image.objects[0].allowance
    );
    image.objects[0].volume = image.objects[1].volume.clone();
    image.objects[0].unit *= 2;
    assert!(aggregate(&image.objects).is_err());
    image.objects[0].unit = image.objects[1].unit;
    image.objects[0].identity = image.objects[1].identity.clone();
    assert!(aggregate(&image.objects).is_err());
    image.objects[0].identity = "original main".into();
    image.objects[0].allowance = u64::MAX;
    assert!(aggregate(&image.objects).is_err());
    image.objects[0].allowance = 0;
    assert!(aggregate(&image.objects).is_err());
    image.objects[0].allowance = 4096;
    image.objects[0].observed_bytes = 4097;
    assert!(aggregate(&image.objects).is_err());
}
