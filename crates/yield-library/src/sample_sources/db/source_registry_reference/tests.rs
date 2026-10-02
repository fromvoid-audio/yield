// Scalar codec fixtures only. They create no registration/capacity or source authority.
use super::*;
use crate::operation_journal::ParticipantId;
fn observation() -> SourceRegistryReferenceObservation {
    SourceRegistryReferenceObservation {
        subject: SourceRegistryReferenceSubject {
            source_id: "codec-only".into(),
            lifecycle_generation: 1,
            profile_acquisition: Uuid::from_u128(1),
            lease_owner: Uuid::from_u128(2),
            lease_epoch: 1,
            media_root_identity: vec![1, 2],
            database_root_identity: "db-root".into(),
            database_identity: "db-file".into(),
            root_binding_identity: "binding".into(),
            scan_revision: 0,
            audit: SourceRegistryAuditObservation::Unavailable {},
            registration: Some(SourceRegistryRegistrationObservation {
                registration_id: Uuid::from_u128(3),
                phase: SourceRegistryRegistrationPhase::RegisteredPhaseObserved,
            }),
        },
    }
}
fn work(ordinal: u16) -> WorkUnitKey {
    WorkUnitKey::new(
        255,
        ordinal,
        ParticipantId::from_uuid(Uuid::from_u128(2192)),
        WorkUnitRole::GlobalLibrary,
    )
}
fn proposal() -> SourceRegistryReferenceProposal {
    SourceRegistryReferenceProposal::new(
        &observation(),
        OperationId::from_uuid(Uuid::from_u128(2192)),
        work(0),
        SourceRegistryReferenceTransition::Enrolled,
        SourceRegistryReferenceExpected::Absent {},
    )
    .unwrap()
}
#[test]
fn codec_scalar_fixture_roundtrip_is_canonical_and_all_context_changes_change_identity() {
    let p = proposal();
    assert_eq!(
        SourceRegistryReferenceProposal::decode(p.canonical_bytes()).unwrap(),
        p
    );
    let pretty = serde_json::to_vec_pretty(&p.stored).unwrap();
    assert_eq!(SourceRegistryReferenceProposal::decode(&pretty).unwrap(), p);
    let base = p.payload_identity();
    let mutations: Vec<Box<dyn Fn(&mut StoredProposal)>> = vec![
        Box::new(|s| s.operation_id = OperationId::from_uuid(Uuid::from_u128(9))),
        Box::new(|s| s.work_key = work(1)),
        Box::new(|s| {
            s.work_key = WorkUnitKey::new(
                1,
                0,
                ParticipantId::from_uuid(Uuid::from_u128(2192)),
                WorkUnitRole::GlobalLibrary,
            )
        }),
        Box::new(|s| s.subject.lease_epoch += 1),
        Box::new(|s| s.subject.lifecycle_generation += 1),
        Box::new(|s| s.subject.profile_acquisition = Uuid::from_u128(10)),
        Box::new(|s| s.subject.lease_owner = Uuid::from_u128(11)),
        Box::new(|s| s.subject.database_identity.push('x')),
        Box::new(|s| s.subject.database_root_identity.push('x')),
        Box::new(|s| s.subject.root_binding_identity.push('x')),
        Box::new(|s| s.subject.media_root_identity.push(4)),
        Box::new(|s| s.subject.source_id.push('x')),
        Box::new(|s| s.subject.scan_revision += 1),
        Box::new(|s| {
            s.subject.registration.as_mut().unwrap().registration_id = Uuid::from_u128(12)
        }),
        Box::new(|s| {
            s.transition = SourceRegistryReferenceTransition::Draining;
            s.expected = SourceRegistryReferenceExpected::Present {
                state: SourceRegistryReferenceTransition::Enrolled,
                row_identity: [1; 32],
            };
        }),
    ];
    for mutate in mutations {
        let mut stored = p.stored.clone();
        mutate(&mut stored);
        assert_ne!(
            SourceRegistryReferenceProposal::from_stored(stored)
                .unwrap()
                .payload_identity(),
            base
        );
    }
    let mut stored = p.stored.clone();
    stored.subject.audit = SourceRegistryAuditObservation::Captured {
        revision: 0,
        directory_generation: 1,
    };
    assert_ne!(
        SourceRegistryReferenceProposal::from_stored(stored)
            .unwrap()
            .payload_identity(),
        base
    );
    let mut stored = p.stored.clone();
    stored.transition = SourceRegistryReferenceTransition::Draining;
    stored.expected = SourceRegistryReferenceExpected::Present {
        state: SourceRegistryReferenceTransition::Enrolled,
        row_identity: [1; 32],
    };
    let first = SourceRegistryReferenceProposal::from_stored(stored.clone()).unwrap();
    stored.expected = SourceRegistryReferenceExpected::Present {
        state: SourceRegistryReferenceTransition::Enrolled,
        row_identity: [2; 32],
    };
    assert_ne!(
        first.payload_identity(),
        SourceRegistryReferenceProposal::from_stored(stored)
            .unwrap()
            .payload_identity()
    );
}
#[test]
fn codec_scalar_fixture_rejects_duplicates_unknown_nested_fields_and_versions() {
    let bytes = String::from_utf8(proposal().canonical).unwrap();
    for invalid in [
        bytes.replacen("{", "{\"version\":1,", 1),
        bytes.replacen("{", "{\"unknown\":0,", 1),
        bytes.replace("\"step_ordinal\":0", "\"step_ordinal\":0,\"unknown\":0"),
        bytes.replace("\"source_id\":", "\"unknown\":0,\"source_id\":"),
        bytes.replace("\"phase\":", "\"unknown\":0,\"phase\":"),
        bytes.replace(
            "\"status\":\"unavailable\"",
            "\"status\":\"unavailable\",\"unknown\":0",
        ),
        bytes.replace(
            "\"status\":\"absent\"",
            "\"status\":\"absent\",\"unknown\":0",
        ),
        bytes.replace(
            "\"status\":\"unavailable\"",
            "\"status\":\"unavailable\",\"status\":\"unavailable\"",
        ),
        bytes.replace("\"version\":1", "\"version\":2"),
    ] {
        assert!(
            SourceRegistryReferenceProposal::decode(invalid.as_bytes()).is_err(),
            "accepted {invalid}"
        );
    }
    let mut trailing = proposal().canonical;
    trailing.extend_from_slice(b"{}");
    assert!(SourceRegistryReferenceProposal::decode(&trailing).is_err());
    assert!(
        SourceRegistryReferenceProposal::decode(&vec![
            b' ';
            SOURCE_REGISTRY_REFERENCE_MAX_BYTES + 1
        ])
        .is_err()
    );
}
#[test]
fn codec_scalar_fixture_bounds_missing_registration_and_invalid_states_fail_before_encoding() {
    let p = proposal();
    for mutate in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11] {
        let mut s = p.stored.clone();
        match mutate {
            0 => s.subject.registration = None,
            1 => {
                s.subject.registration.as_mut().unwrap().phase =
                    SourceRegistryRegistrationPhase::Prepared
            }
            2 => {
                s.subject.registration.as_mut().unwrap().phase =
                    SourceRegistryRegistrationPhase::Abandoned
            }
            3 => {
                s.subject.registration.as_mut().unwrap().phase =
                    SourceRegistryRegistrationPhase::Draining
            }
            4 => s.subject.source_id = "x".repeat(257),
            5 => s.subject.media_root_identity = vec![1; ROOT_BYTES + 1],
            6 => s.subject.database_identity = "x".repeat(IDENTITY_BYTES + 1),
            7 => s.subject.lease_epoch = 0,
            8 => s.subject.lifecycle_generation = i64::MAX as u64 + 1,
            9 => {
                s.subject.audit = SourceRegistryAuditObservation::Captured {
                    revision: 1,
                    directory_generation: 1,
                }
            }
            10 => s.transition = SourceRegistryReferenceTransition::Retired,
            _ => {
                s.work_key = WorkUnitKey::new(
                    0,
                    0,
                    ParticipantId::from_uuid(Uuid::from_u128(1)),
                    WorkUnitRole::SourceRead,
                )
            }
        }
        assert!(SourceRegistryReferenceProposal::from_stored(s).is_err());
    }
    let mut s = p.stored.clone();
    s.subject.media_root_identity = vec![255; ROOT_BYTES];
    assert!(matches!(
        SourceRegistryReferenceProposal::from_stored(s),
        Err(SourceRegistryReferenceError::TooLarge)
    ));
    // Ceiling applies after escaping too; large but individually legal identities cannot bypass it.
    let mut s = p.stored;
    s.subject.database_identity = "\\".repeat(IDENTITY_BYTES);
    s.subject.database_root_identity = "\\".repeat(IDENTITY_BYTES);
    s.subject.root_binding_identity = "\\".repeat(IDENTITY_BYTES);
    s.subject.media_root_identity = vec![255; 3000];
    assert!(matches!(
        SourceRegistryReferenceProposal::from_stored(s),
        Err(SourceRegistryReferenceError::TooLarge)
    ));
}

#[test]
fn real_retained_capture_does_not_renew_the_source_lease() {
    use crate::app_dirs::{AppRootGuard, WritableProfileGuard};
    use crate::sample_sources::{SourceId, SourceWriter};
    use std::{fs, path::Path};
    let temp = tempfile::tempdir().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        &media,
        &profile,
        Path::new("registry-no-renew"),
    )
    .unwrap();
    let owner = SourceWriter::new_for_scan(database, SourceId::from_string("registry-no-renew"), 1)
        .unwrap();
    let lease = owner.catalog_database().source_lease.as_ref().unwrap();
    let before = lease.validate_current_owner().unwrap();
    for _ in 0..8 {
        owner.capture_registry_reference_observation(None).unwrap();
    }
    assert_eq!(lease.validate_current_owner().unwrap(), before);
    owner.close().unwrap();
}

#[test]
fn real_retained_capture_never_reads_directory_entry_payloads() {
    use crate::app_dirs::{AppRootGuard, WritableProfileGuard};
    use crate::sample_sources::{SourceId, SourceWriter};
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::{fs, path::Path};
    let temp = tempfile::tempdir().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        &media,
        &profile,
        Path::new("registry-no-paths"),
    )
    .unwrap();
    let owner = SourceWriter::new_for_scan(database, SourceId::from_string("registry-no-paths"), 1)
        .unwrap();
    let generation = owner.begin_scan_directory_truth_generation(0).unwrap();
    let fence = owner.current_scan_fence().unwrap();
    let batch = owner.catalog_database().write_batch().unwrap();
    owner
        .commit_scan_audit_completion(
            &fence,
            batch,
            generation,
            1,
            fence.root_identity().clone(),
            None,
        )
        .unwrap();
    owner
        .catalog_database()
        .connection
        .authorizer(Some(|context: AuthContext<'_>| match context.action {
            AuthAction::Read {
                table_name: "source_directory_entries",
                ..
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }));
    let observation = owner.capture_registry_reference_observation(None).unwrap();
    assert!(
        matches!(observation.subject().audit(), SourceRegistryAuditObservation::Captured { directory_generation, .. } if *directory_generation == generation)
    );
    owner
        .catalog_database()
        .connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    owner.close().unwrap();
}
