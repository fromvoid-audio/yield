//! Real retained Store/SQLite; all inputs are passive historical observations.
use super::*;
use crate::operation_journal::owner::JournalNamespaceFundingProbe;
use crate::operation_journal::{ParticipantCheckpointState, ParticipantId};

fn install(store: &OperationJournalStore, bytes: &[u8]) -> OperationId {
    let value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    let operation: OperationId = serde_json::from_value(value["operation_id"].clone()).unwrap();
    store
        .connection
        .execute("DELETE FROM operation_journal", [])
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO operation_journal VALUES (?1, 1, ?2)",
            params![operation.to_string(), bytes],
        )
        .unwrap();
    operation
}

fn enter_count() -> usize {
    historical_claim_observation::GENERAL_DECODE_ENTRIES.with(|n| n.get())
}

fn retirement_record() -> StoredOperationRecord {
    use crate::operation_journal::{
        source_lease_binding::StoredSourceLeaseFence,
        source_registry_retirement::SourceRegistryRetirementIntentArgs,
        work_plan::{WorkUnitKey, WorkUnitRole},
    };
    use crate::sample_sources::db::{
        SourceRegistryReferenceProposal, SourceRegistryReferenceTransition,
    };
    // Passive registered-phase storage observations; no live source registration or witness.
    let fence: StoredSourceLeaseFence = serde_json::from_value(serde_json::json!({
        "version":1, "subject": {
            "source_id":Uuid::from_u128(1), "lifecycle_generation":3,
            "profile_acquisition":Uuid::from_u128(5), "lease_owner":Uuid::from_u128(2),
            "lease_epoch":7, "media_root_identity":[255,0,1], "database_root_identity":"db-root",
            "database_identity":"main", "root_binding_identity":"root-binding", "scan_revision":0,
            "audit":{"status":"unavailable"},
            "registration":{"registration_id":Uuid::from_u128(3),"phase":"registered_phase_observed"}
        },
        "preparation_nonce":Uuid::from_u128(4), "page_size":4096, "maximum_pages":16384,
        "objects":[
            {"identity":"main","device":1,"unit":4096,"bytes":4096,"ceiling":272646144},
            {"identity":"wal","device":1,"unit":4096,"bytes":0,"ceiling":274206720},
            {"identity":"shm","device":1,"unit":4096,"bytes":32768,"ceiling":5505024}
        ]
    })).unwrap();
    let source = ParticipantId::new();
    let global = ParticipantId::new();
    let operation = OperationId::new();
    let proposal = |ordinal, transition, state: &str| {
        SourceRegistryReferenceProposal::decode(
            &serde_json::to_vec(&serde_json::json!({
                "version":1, "operation_id":operation,
                "work_key":WorkUnitKey::new(0, ordinal, global, WorkUnitRole::GlobalLibrary),
                "transition":transition,
                "expected":{"status":"present","state":state,"row_identity":vec![1;32]},
                "subject":fence.subject()
            }))
            .unwrap(),
        )
        .unwrap()
    };
    let draining = proposal(0, SourceRegistryReferenceTransition::Draining, "enrolled");
    let retired = proposal(2, SourceRegistryReferenceTransition::Retired, "draining");
    let args = SourceRegistryRetirementIntentArgs::new(source, global, fence, &draining, &retired)
        .unwrap();
    let descriptors = vec![
        args.source_descriptor().unwrap(),
        ParticipantDescriptor::new(
            global,
            ParticipantKind::GlobalLibrary,
            ParticipantRequirement::Required,
            None,
            None,
        )
        .unwrap(),
    ];
    let checkpoints = descriptors
        .iter()
        .map(|d| {
            ParticipantCheckpoint::new(d.id(), ParticipantCheckpointState::Pending, None, None)
                .unwrap()
        })
        .collect();
    let participants = ParticipantSet::new(descriptors, checkpoints).unwrap();
    let plan = FullCapacityPlan::new(
        [
            FullCapacityAllocationClass::JournalRecord,
            FullCapacityAllocationClass::SourceDatabaseCommit,
            FullCapacityAllocationClass::SourceWalShm,
            FullCapacityAllocationClass::GlobalDatabaseCommit,
            FullCapacityAllocationClass::GlobalWalShm,
        ]
        .into_iter()
        .map(|class| {
            crate::operation_journal::full_capacity::FullCapacityAllocation::new(
                VolumeIdentity { device: 1 },
                4096,
                class,
                4096,
                4096,
                PROTECTED_FREE_SPACE_FLOOR,
            )
            .unwrap()
        })
        .collect(),
    )
    .unwrap();
    StoredOperationRecord::new_v18_source_retirement(
        operation,
        OperationIntent {
            actor: OperationActor::User,
            kind: OperationKind::SourceRegistryRetirement,
            label: "historical retirement".into(),
        },
        UniversalIntent::new(
            None,
            UniversalCommandKind::SourceRegistryRetirement,
            "operation.status.source_registry_retirement",
            participants,
        )
        .unwrap(),
        UniversalCommandArgs::SourceRegistryRetirement(args),
        plan,
        1,
    )
}

#[test]
fn general_historical_original_sqlite_golden_families_match_without_blob_copy() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let mut fixtures = super::super::super::codec::general_historical_fixture_bytes_for_test();
    let proposal =
        crate::operation_journal::recording_commands::recording_proposal_for_test(2, 10_000);
    fixtures.push(encode(&finite_recording_record(&proposal)).unwrap());
    fixtures.push(encode(&retirement_record()).unwrap());
    for bytes in fixtures {
        let operation = install(&store, &bytes);
        let original = store.get(operation).map(|v| v.is_some());
        let funding = JournalNamespaceFundingProbe::new();
        let before = funding.remaining_bytes();
        POINT_ROW_MATERIALIZED_BYTES.with(|bytes| bytes.set(0));
        let entered = enter_count();
        let actual = store.observe_claim_operation(operation, &funding.input);
        assert_eq!(
            actual.is_ok(),
            original.is_ok(),
            "schema {}: {actual:?}",
            decode(&bytes).unwrap().schema_version
        );
        if let (Ok(actual), Ok(original)) = (actual, original) {
            assert_eq!(actual, original);
            assert_eq!(funding.remaining_bytes(), before);
        }
        assert_eq!(enter_count(), entered + 1);
        POINT_ROW_MATERIALIZED_BYTES.with(|bytes| assert_eq!(bytes.get(), 0));
        assert_eq!(
            store
                .connection
                .query_row("SELECT record FROM operation_journal", [], |r| r
                    .get::<_, Vec<u8>>(0))
                .unwrap(),
            bytes
        );
    }
}

#[test]
fn general_historical_pressure_refuses_before_original_decode_and_preserves_record() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let bytes = super::super::super::codec::general_historical_fixture_bytes_for_test()
        .into_iter()
        .find(|b| decode(b).unwrap().schema_version == SCHEMA_V16)
        .unwrap();
    let operation = install(&store, &bytes);
    let funding = JournalNamespaceFundingProbe::new();
    // Leave syntax/SQL backing but not the derived owned graph/typed/semantic loan.
    let pressure = funding
        .input
        .reserve_historical_scratch(funding.remaining_bytes() - 80 * 1024)
        .unwrap();
    let before = funding.remaining_bytes();
    let entered = enter_count();
    let attempts = historical_claim_observation::GENERAL_FUND_ATTEMPTS.with(|n| n.get());
    assert!(matches!(
        store.observe_claim_operation(operation, &funding.input),
        Err(OperationJournalStoreError::HistoricalClaimResourceBound)
    ));
    assert_eq!(enter_count(), entered);
    historical_claim_observation::GENERAL_FUND_ATTEMPTS.with(|n| assert_eq!(n.get(), attempts + 1));
    // Selected refusal backing is retained until its original diagnostic consumer drops.
    assert!(funding.remaining_bytes() < before);
    drop(pressure);
    drop(funding);
    assert!(store.get(operation).unwrap().is_some());
    assert_eq!(
        store
            .connection
            .query_row("SELECT record FROM operation_journal", [], |r| r
                .get::<_, Vec<u8>>(0))
            .unwrap(),
        bytes
    );
}

#[test]
fn general_historical_selected_decode_failure_keeps_original_reply_credit_after_input_drop() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(
        &super::super::super::codec::general_historical_fixture_bytes_for_test()[0],
    )
    .unwrap();
    value["schema_version"] = serde_json::json!(999);
    let bytes = serde_json::to_vec(&value).unwrap();
    let operation = install(&store, &bytes);
    let original = store.get(operation).unwrap_err().to_string();
    let funding = JournalNamespaceFundingProbe::new();
    let observer = funding.pool_bytes_observer_for_test();
    let error = store
        .observe_claim_operation(operation, &funding.input)
        .unwrap_err();
    assert_eq!(error.to_string(), original);
    let error = funding.retain_command_error_for_test(error);
    assert!(matches!(
        error,
        super::super::super::owner::OperationJournalOwnerError::RetainedBacking(_)
    ));
    drop(funding);
    assert!(observer().0 < observer().1);
    drop(error);
    assert_eq!(observer().0, observer().1);
}

#[test]
fn general_historical_recording_prestart_gate_remains_the_original_gate() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let bytes = super::super::super::codec::general_historical_fixture_bytes_for_test()
        .into_iter()
        .find(|b| decode(b).unwrap().schema_version == SCHEMA_V16)
        .unwrap();
    let operation = install(&store, &bytes);
    let funding = JournalNamespaceFundingProbe::new();
    store.recording_prestart_scope.set(true);
    assert!(matches!(
        store.observe_claim_operation(operation, &funding.input),
        Err(OperationJournalStoreError::RecordingPrestartResourceBound)
    ));
    assert!(store.recording_prestart_resource_refused.get());
    store.recording_prestart_scope.set(false);
    assert!(store.get(operation).unwrap().is_some());
}
