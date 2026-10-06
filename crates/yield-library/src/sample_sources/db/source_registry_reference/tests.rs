// Scalar codec fixtures only. They create no registration/capacity or source authority.
use super::*;
use crate::operation_journal::ParticipantId;

#[test]
fn source_audit_typed_decode_preserves_historical_tagged_representations() {
    // Exact original derive is a compatibility reference, never an owner capture.
    #[derive(Deserialize)]
    #[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
    enum Original {
        Unavailable {},
        Captured {
            revision: u64,
            directory_generation: u64,
        },
    }
    let cases = [
        r#"{"status":"unavailable"}"#,
        r#"{"status":"captured","revision":0,"directory_generation":1}"#,
        r#"{"directory_generation":1,"revision":0,"status":"captured"}"#,
        r#"{"\u0073tatus":"captured","\u0072evision":0,"directory_generation":1}"#,
        r#"["unavailable"]"#,
        r#"["captured",0,1]"#,
        r#"{"status":0}"#,
        r#"[1,0,1]"#,
        r#"{"status":"captured","revision":18446744073709551615,"directory_generation":1}"#,
        r#"{"status":"unavailable","revision":0}"#,
        r#"{"status":"captured","revision":0}"#,
        r#"{"status":"unavailable","status":"captured"}"#,
        r#"{"status":"captured","revision":0,"\u0072evision":1,"directory_generation":1}"#,
        r#"{"status":"captured","revision":0,"directory_generation":1,"directory_generation":2}"#,
        r#"{"status":"unavailable","unknown":[[[0]]]}"#,
        r#"{"status":"captured","revision":[],"directory_generation":1}"#,
        r#"{"status":"captured","revision":-1,"directory_generation":1}"#,
        r#"{"status":"captured","revision":0.5,"directory_generation":1}"#,
        r#"{"status":{"captured":null},"revision":0,"directory_generation":1}"#,
        r#"["captured",0,1,[[[0]]]]"#,
        r#"["unavailable",{}]"#,
        r#"[]"#,
        r#"null"#,
        r#"{"status":"unavailable"} false"#,
    ];
    for wire in cases {
        let original = serde_json::from_str::<Original>(wire).map(|value| match value {
            Original::Unavailable {} => SourceRegistryAuditObservation::Unavailable {},
            Original::Captured {
                revision,
                directory_generation,
            } => SourceRegistryAuditObservation::Captured {
                revision,
                directory_generation,
            },
        });
        let actual = serde_json::from_str::<SourceRegistryAuditObservation>(wire);
        assert_eq!(actual.is_ok(), original.is_ok(), "{wire}");
        if let Ok(original) = original {
            assert_eq!(actual.unwrap(), original, "{wire}");
        }
    }
    for wire in [
        format!(
            r#"{{"{}":0,"status":"unavailable"}}"#,
            "\\u2028".repeat(2600)
        ),
        format!(r#"{{"status":"{}"}}"#, "\\u2028".repeat(2600)),
    ] {
        assert!(serde_json::from_str::<Original>(&wire).is_err());
        assert!(serde_json::from_str::<SourceRegistryAuditObservation>(&wire).is_err());
    }
}

#[test]
fn source_audit_typed_decode_actual_public_proposal_keeps_identity_and_refusals() {
    let base = proposal();
    let mut value = serde_json::to_value(&base.stored).unwrap();
    for audit in [
        serde_json::json!({"status":"unavailable"}),
        serde_json::json!(["unavailable"]),
    ] {
        value["subject"]["audit"] = audit;
        assert_eq!(
            SourceRegistryReferenceProposal::decode(&serde_json::to_vec(&value).unwrap()).unwrap(),
            base
        );
    }
    let mut captured = base.stored.clone();
    captured.subject.audit = SourceRegistryAuditObservation::Captured {
        revision: 0,
        directory_generation: 1,
    };
    let expected = SourceRegistryReferenceProposal::from_stored(captured).unwrap();
    for audit in [
        serde_json::json!({"status":"captured","revision":0,"directory_generation":1}),
        serde_json::json!(["captured", 0, 1]),
    ] {
        value["subject"]["audit"] = audit;
        assert_eq!(
            SourceRegistryReferenceProposal::decode(&serde_json::to_vec(&value).unwrap()).unwrap(),
            expected
        );
    }
    for audit in [
        serde_json::json!({"status":0}),
        serde_json::json!([0]),
        serde_json::json!([1, 0, 1]),
        serde_json::json!({"status":"captured","revision":vec![0u8;6000],"directory_generation":1}),
        serde_json::json!({"status":"unavailable","unknown":vec![0u8;6000]}),
        serde_json::json!(["unavailable", vec![0u8; 6000]]),
        serde_json::json!(["captured", 0, 1, vec![0u8; 6000]]),
    ] {
        value["subject"]["audit"] = audit;
        let wire = serde_json::to_vec(&value).unwrap();
        assert!(wire.len() <= SOURCE_REGISTRY_REFERENCE_MAX_BYTES);
        assert!(SourceRegistryReferenceProposal::decode(&wire).is_err());
    }
    assert_eq!(
        SourceRegistryReferenceProposal::decode(base.canonical_bytes()).unwrap(),
        base
    );
}
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

#[test]
fn source_expected_typed_decode_preserves_exact_historical_representations() {
    #[derive(Deserialize)]
    #[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
    enum Original {
        Absent {},
        Present {
            state: SourceRegistryReferenceTransition,
            row_identity: [u8; 32],
        },
    }
    let row = serde_json::to_string(&[1u8; 32]).unwrap();
    let cases = vec![
        r#"{"status":"absent"}"#.into(),
        r#"["absent"]"#.into(),
        format!(r#"{{"status":"present","state":"enrolled","row_identity":{row}}}"#),
        format!(r#"{{"row_identity":{row},"state":"draining","status":"present"}}"#),
        format!(r#"{{"\u0073tatus":"present","\u0073tate":"retired","row_identity":{row}}}"#),
        format!(r#"["present","enrolled",{row}]"#),
        format!(r#"{{"status":"present","state":{{"draining":null}},"row_identity":{row}}}"#),
        format!(r#"["present",{{"retired":null}},{row}]"#),
        r#"{"status":0}"#.into(),
        r#"[0]"#.into(),
        r#"{"status":"absent","state":"enrolled"}"#.into(),
        format!(r#"{{"status":"absent","row_identity":{row}}}"#),
        r#"{"status":"absent","status":"absent"}"#.into(),
        format!(
            r#"{{"status":"present","state":"enrolled","\u0073tate":"draining","row_identity":{row}}}"#
        ),
        format!(
            r#"{{"status":"present","state":"enrolled","row_identity":{row},"row_identity":{row}}}"#
        ),
        r#"{"status":"present","state":"enrolled"}"#.into(),
        format!(r#"{{"state":"enrolled","row_identity":{row}}}"#),
        r#"{"status":"absent","unknown":[[[0]]]}"#.into(),
        r#"{"status":"present","state":"enrolled","row_identity":[]}"#.into(),
        r#"{"status":"present","state":[],"row_identity":[]}"#.into(),
        format!(r#"["present","enrolled",{row},[[[0]]]]"#),
        r#"["absent",{}]"#.into(),
        r#"{"status":"absent"} false"#.into(),
        r#"[]"#.into(),
        "null".into(),
        format!(
            r#"{{"status":"present","state":{{"enrolled":null,"enrolled":null}},"row_identity":{row}}}"#
        ),
        format!(r#"{{"status":"present","state":{{"enrolled":{{"x":0}}}},"row_identity":{row}}}"#),
        format!(
            r#"{{"status":"present","state":"enrolled","row_identity":{}}}"#,
            serde_json::to_string(&vec![1u8; 33]).unwrap()
        ),
        format!(r#"{{"{}":0,"status":"absent"}}"#, "\\u2028".repeat(2600)),
        format!(r#"{{"status":"{}"}}"#, "\\u2028".repeat(2600)),
    ];
    for wire in cases {
        let original = serde_json::from_str::<Original>(&wire).map(|v| match v {
            Original::Absent {} => SourceRegistryReferenceExpected::Absent {},
            Original::Present {
                state,
                row_identity,
            } => SourceRegistryReferenceExpected::Present {
                state,
                row_identity,
            },
        });
        let actual = serde_json::from_str::<SourceRegistryReferenceExpected>(&wire);
        assert_eq!(actual.is_ok(), original.is_ok(), "{wire}");
        if let Ok(original) = original {
            assert_eq!(actual.unwrap(), original, "{wire}");
        }
    }
}

#[test]
fn source_expected_typed_decode_actual_public_preserves_identity_and_refusals() {
    let base = proposal();
    let mut value = serde_json::to_value(&base.stored).unwrap();
    value["expected"] = serde_json::json!(["absent"]);
    assert_eq!(
        SourceRegistryReferenceProposal::decode(&serde_json::to_vec(&value).unwrap()).unwrap(),
        base
    );
    let mut stored = base.stored.clone();
    stored.transition = SourceRegistryReferenceTransition::Draining;
    stored.expected = SourceRegistryReferenceExpected::Present {
        state: SourceRegistryReferenceTransition::Enrolled,
        row_identity: [1; 32],
    };
    let expected = SourceRegistryReferenceProposal::from_stored(stored).unwrap();
    let mut value = serde_json::to_value(&expected.stored).unwrap();
    value["expected"] = serde_json::json!(["present",{"enrolled":null},vec![1;32]]);
    assert_eq!(
        SourceRegistryReferenceProposal::decode(&serde_json::to_vec(&value).unwrap()).unwrap(),
        expected
    );
    for field in ["row_identity", "unknown"] {
        value["expected"] =
            serde_json::json!({"status":"present","state":"enrolled",field:vec![0u8;6000]});
        let wire = serde_json::to_vec(&value).unwrap();
        assert!(wire.len() < SOURCE_REGISTRY_REFERENCE_MAX_BYTES);
        assert!(SourceRegistryReferenceProposal::decode(&wire).is_err());
    }
}
