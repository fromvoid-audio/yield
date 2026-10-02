//! Real normal-build retained source metadata observations, not physical qualification.
#![cfg(not(feature = "test-support"))]
use std::{fs, path::Path};
use uuid::Uuid;
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    operation_journal::{
        OperationId, ParticipantId,
        work_plan::{WorkUnitKey, WorkUnitRole},
    },
    sample_sources::{
        SOURCE_REGISTRY_REFERENCE_MAX_BYTES, SourceDatabase, SourceId,
        SourceRegistryAuditObservation, SourceRegistryReferenceError,
        SourceRegistryReferenceExpected, SourceRegistryReferenceProposal,
        SourceRegistryReferenceTransition, SourceRegistryRegistrationPhase, SourceWriter,
    },
};

fn writer(media: &Path, profile: &WritableProfileGuard, id: &str, lifecycle: u64) -> SourceWriter {
    SourceWriter::new_for_scan(
        SourceDatabase::open_for_catalog_with_profile_guard(media, profile, Path::new(id)).unwrap(),
        SourceId::from_string(id),
        lifecycle,
    )
    .unwrap()
}
#[test]
fn retained_owner_captures_prepared_and_audited_observations_without_mutation_or_enrollment() {
    let temp = tempfile::tempdir().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let owner = writer(&media, &profile, "registry-observation", 1);
    let before = owner.current_scan_fence().unwrap();
    let observation = owner.capture_registry_reference_observation(None).unwrap();
    let s = observation.subject();
    assert_eq!(s.source_id(), before.source_id().as_str());
    assert_eq!(s.lifecycle_generation(), before.lifecycle_generation());
    assert_eq!(s.lease_epoch(), before.lease_epoch());
    assert_eq!(s.scan_revision(), before.revision());
    assert_eq!(s.media_root_identity(), before.root_identity().as_bytes());
    assert_eq!(
        s.profile_acquisition().to_string(),
        profile.owner_identity()
    );
    assert!(!s.database_identity().is_empty());
    assert!(!s.database_root_identity().is_empty());
    assert!(!s.root_binding_identity().is_empty());
    assert!(s.registration().is_none());
    assert_eq!(s.audit(), &SourceRegistryAuditObservation::Unavailable {});
    let key = WorkUnitKey::new(
        0,
        0,
        ParticipantId::from_uuid(Uuid::from_u128(2192)),
        WorkUnitRole::GlobalLibrary,
    );
    assert!(matches!(
        SourceRegistryReferenceProposal::new(
            &observation,
            OperationId::from_uuid(Uuid::from_u128(2192)),
            key,
            SourceRegistryReferenceTransition::Enrolled,
            SourceRegistryReferenceExpected::Absent {}
        ),
        Err(SourceRegistryReferenceError::RegistrationUnavailable)
    ));
    assert_eq!(owner.current_scan_fence().unwrap(), before);
    // Genuine local preparation clones real roots; no journal/reserve installation is invented.
    let registration = owner.catalog_database().prepare_source_authority().unwrap();
    let prepared = owner
        .capture_registry_reference_observation(Some(&registration))
        .unwrap();
    let r = prepared.subject().registration().unwrap();
    assert!(!r.registration_id().is_nil());
    assert_eq!(r.phase(), SourceRegistryRegistrationPhase::Prepared);
    assert!(matches!(
        SourceRegistryReferenceProposal::new(
            &prepared,
            OperationId::from_uuid(Uuid::from_u128(2192)),
            key,
            SourceRegistryReferenceTransition::Enrolled,
            SourceRegistryReferenceExpected::Absent {}
        ),
        Err(SourceRegistryReferenceError::RegistrationUnavailable)
    ));
    let other_media = temp.path().join("other-media");
    fs::create_dir(&other_media).unwrap();
    let other = writer(&other_media, &profile, "other-registry-observation", 1);
    assert!(matches!(
        other.capture_registry_reference_observation(Some(&registration)),
        Err(SourceRegistryReferenceError::RegistrationMismatch)
    ));
    let fence = owner.current_scan_fence().unwrap();
    let mut batch = owner.catalog_database().write_batch().unwrap();
    batch
        .upsert_file_with_hash(Path::new("fixed.wav"), 44, 1000, "fixed-hash")
        .unwrap();
    owner.commit_scan_manifest_batch(&fence, batch).unwrap();
    let generation = owner.begin_scan_directory_truth_generation(0).unwrap();
    let fence = owner.current_scan_fence().unwrap();
    let batch = owner.catalog_database().write_batch().unwrap();
    owner
        .commit_scan_audit_completion(
            &fence,
            batch,
            generation,
            2000,
            fence.root_identity().clone(),
            None,
        )
        .unwrap();
    let current = owner.current_revision().unwrap();
    let audited = owner
        .capture_registry_reference_observation(Some(&registration))
        .unwrap();
    let s = audited.subject();
    assert_eq!(s.database_identity(), current.database_identity());
    assert_eq!(s.root_binding_identity(), current.root_binding_identity());
    assert_eq!(s.scan_revision(), current.revision());
    assert_eq!(
        s.audit(),
        &SourceRegistryAuditObservation::Captured {
            revision: current.revision(),
            directory_generation: generation
        }
    );
    assert_eq!(owner.current_revision().unwrap(), current);
    assert!(serde_json::to_vec(&audited).unwrap().len() <= SOURCE_REGISTRY_REFERENCE_MAX_BYTES);
    let old_registration = r.registration_id();
    let old_epoch = s.lease_epoch();
    let old_db = s.database_identity().to_owned();
    drop(registration);
    other.close().unwrap();
    owner.close().unwrap();
    let reopened = writer(&media, &profile, "registry-observation", 2);
    let new_registration = reopened
        .catalog_database()
        .prepare_source_authority()
        .unwrap();
    let fresh = reopened
        .capture_registry_reference_observation(Some(&new_registration))
        .unwrap();
    assert_eq!(fresh.subject().database_identity(), old_db);
    assert_ne!(fresh.subject().lease_epoch(), old_epoch);
    assert_eq!(fresh.subject().lifecycle_generation(), 2);
    assert_ne!(
        fresh.subject().registration().unwrap().registration_id(),
        old_registration
    );
    assert_eq!(
        audited.subject().lease_epoch(),
        old_epoch,
        "historical observations never rebind"
    );
    drop(new_registration);
    reopened.close().unwrap();
}
#[test]
fn retained_owner_observation_rejects_replaced_profile_without_changing_source_revision() {
    let temp = tempfile::tempdir().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let profile_path = temp.path().join("profile");
    let _root = AppRootGuard::set(profile_path.clone()).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let owner = writer(&media, &profile, "registry-owner-loss", 1);
    let before = owner.current_scan_fence().unwrap();
    fs::rename(&profile_path, temp.path().join("old-profile")).unwrap();
    fs::create_dir(&profile_path).unwrap();
    assert!(owner.capture_registry_reference_observation(None).is_err());
    assert_eq!(
        owner.catalog_database().get_revision().unwrap(),
        before.revision()
    );
}
