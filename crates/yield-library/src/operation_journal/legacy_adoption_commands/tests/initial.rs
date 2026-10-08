//! Actual original observations; altered device/unit DTOs below prove arithmetic only.
use super::*;
use crate::operation_journal::full_capacity::{
    FullCapacityAllocationClass as Class, FullCapacityPlan,
};

#[test]
fn legacy_initial_source_checkpoint_survives_original_journal_change_and_preserves_deadline() {
    let mut fixture = Fixture::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    assert!(
        fixture
            .source
            .validate_admission_checkpoint_until(&fixture.guard, &fixture.prepare(), deadline)
            .is_err(),
        "plain readonly inspection is not no-effect RW custody"
    );
    fixture
        .source
        .prepare_no_effect_writable_until(&fixture.guard, deadline)
        .unwrap();
    let args = fixture.prepare();
    fixture
        .source
        .validate_admission_checkpoint_until(&fixture.guard, &args, deadline)
        .unwrap();
    // Actual original Store mutation changes only its own frozen journal checkpoint.
    // This V1 metadata fixture supplies no physical capacity or source effect grant.
    let operation = OperationId::new();
    let wire = serde_json::json!({
        "schema_version":1, "operation_id":operation,
        "intent":{"actor":"User","kind":"FileHistory","label":"journal checkpoint change"},
        "phase":"IntentDurable","disposition":"None","payload":null,
        "capacity_plan":{"volumes":[{"identity":{"device":1},"allocation_unit":4096,
            "allocation_class":"JournalRecord","logical_bytes":4096,"allocated_bytes":4096,
            "protected_free_bytes":268435456}]}, "created_unix_ms":1,"updated_unix_ms":1
    });
    let record = decode(&serde_json::to_vec(&wire).unwrap()).unwrap();
    assert!(matches!(
        fixture.store.insert_intent_with_outcome(&record),
        JournalInsertOutcome::Committed(_)
    ));
    assert_ne!(
        fixture
            .source
            .prepare_legacy_adoption_input(
                &fixture.guard,
                &fixture.store,
                args.participant_id(),
                args.source_command_id(),
                &AtomicBool::new(false)
            )
            .unwrap(),
        args
    );
    fixture
        .source
        .validate_admission_checkpoint_until(&fixture.guard, &args, deadline)
        .unwrap();
    let expired = Instant::now() - Duration::from_millis(1);
    assert!(
        fixture
            .source
            .validate_admission_checkpoint_until(&fixture.guard, &args, expired)
            .is_err()
    );
    let mut changed = args.clone();
    changed.data.objects[4].identity.push_str("-foreign");
    assert!(
        fixture
            .source
            .validate_admission_checkpoint_until(&fixture.guard, &changed, deadline)
            .is_err()
    );
}

#[test]
fn legacy_initial_independently_rounds_all_eight_objects_before_same_volume_class_sums() {
    let fixture = Fixture::new();
    let mut args = fixture.prepare();
    // Passive wire arithmetic, never fed to a live owner or capacity provider.
    for object in &mut args.data.objects {
        object.device = 42;
        object.unit = 1024 * 1024;
    }
    let plan = args.initial_capacity_plan().unwrap();
    assert_eq!(plan.allocations().len(), 3);
    for (class, indices) in [
        (Class::SourceDatabaseCommit, &[0, 1, 4][..]),
        (Class::SourceWalShm, &[2, 3][..]),
        (Class::JournalRecord, &[5, 6, 7][..]),
    ] {
        let allocation = plan
            .allocations()
            .iter()
            .find(|a| a.class() == class)
            .unwrap();
        let logical = indices
            .iter()
            .map(|&i| args.data.objects[i].conditional_logical_bytes)
            .sum::<u64>();
        let rounded = indices
            .iter()
            .map(|&i| {
                let object = &args.data.objects[i];
                object.conditional_logical_bytes.div_ceil(object.unit) * object.unit
            })
            .sum::<u64>();
        assert_eq!(allocation.logical_bytes(), logical);
        assert_eq!(allocation.allocated_bytes(), rounded);
        assert_eq!(allocation.volume().device, 42);
        assert_eq!(allocation.allocation_unit(), 1024 * 1024);
    }
    let source = plan
        .allocations()
        .iter()
        .find(|a| a.class() == Class::SourceDatabaseCommit)
        .unwrap();
    assert!(
        source.allocated_bytes()
            > source.logical_bytes().div_ceil(source.allocation_unit()) * source.allocation_unit(),
        "rounding a combined bucket must not undercharge distinct original objects"
    );
}

#[test]
fn legacy_initial_preserves_separate_role_volumes_and_rejects_inconsistent_units() {
    let fixture = Fixture::new();
    let mut args = fixture.prepare();
    for (object, device) in args.data.objects.iter_mut().zip([1, 2, 2, 2, 3, 4, 5, 6]) {
        object.device = device;
    }
    let plan = args.initial_capacity_plan().unwrap();
    assert_eq!(plan.allocations().len(), 7);
    assert_eq!(
        plan.allocations()
            .iter()
            .filter(|a| a.class() == Class::SourceDatabaseCommit)
            .count(),
        3
    );
    assert_eq!(
        plan.allocations()
            .iter()
            .filter(|a| a.class() == Class::JournalRecord)
            .count(),
        3
    );
    args.data.objects[4].device = args.data.objects[0].device;
    args.data.objects[4].unit = args.data.objects[0].unit * 2;
    assert!(args.initial_capacity_plan().is_err());
}

#[test]
fn legacy_initial_v19_roundtrips_exact_original_plan_and_refuses_family_or_plan_coercion() {
    let fixture = Fixture::new();
    let args = fixture.prepare();
    let record =
        StoredOperationRecord::new_v19_legacy_adoption(OperationId::new(), args, 31).unwrap();
    let bytes = encode(&record).unwrap();
    let refused = fixture.store.insert_intent_with_outcome(&record);
    let failure = refused
        .failure()
        .expect("passive bytes cannot bypass original claims consumer");
    assert_eq!(
        failure.stage(),
        crate::operation_journal::store::JournalInsertWriteStage::NoMutationAttempted
    );
    assert!(fixture.store.get(record.operation_id).unwrap().is_none());

    assert_eq!(decode(&bytes).unwrap(), record);
    assert_eq!(
        validation::classify_record(&record),
        validation::RecordConsistency::Consistent
    );
    for version in 1..=18 {
        let mut old = record.clone();
        old.schema_version = version;
        assert!(
            encode(&old).is_err(),
            "historical schema {version} must not admit legacy adoption"
        );
        let mut old: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        old["schema_version"] = version.into();
        assert!(decode(&serde_json::to_vec(&old).unwrap()).is_err());
    }
    let mut missing = record.clone();
    missing.full_capacity_plan = Some(
        FullCapacityPlan::new(
            record
                .full_capacity_plan
                .as_ref()
                .unwrap()
                .allocations()
                .iter()
                .filter(|a| a.class() != Class::SourceWalShm)
                .cloned()
                .collect(),
        )
        .unwrap(),
    );
    assert!(encode(&missing).is_err());
    let mut changed = record.clone();
    changed.intent.kind = OperationKind::FileMutation;
    assert!(encode(&changed).is_err());
    let mut parent = record.clone();
    parent.disposition = OperationDisposition::RetryPending;
    assert!(encode(&parent).is_err());
    let mut unknown: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    unknown["schema_version"] = 20.into();
    assert!(matches!(
        decode(&serde_json::to_vec(&unknown).unwrap()),
        Err(DecodeError::UnsupportedVersion { version: 20 })
    ));
}

// Logical provider arithmetic only: no live witness, physical qualification or
// source mutation authority is fabricated by these Store settlement fixtures.
fn settlement_fixture(
    fixture: &Fixture,
) -> (
    StoredOperationRecord,
    AdmissionClaimInventory,
    crate::operation_journal::capacity_provider::CapacityProvider,
) {
    use crate::durable_capacity::{PROTECTED_FREE_SPACE_FLOOR, VolumeFacts};
    use crate::operation_journal::{
        admission_claims::{AdmissionClaim, AdmissionClaimId},
        capacity_provider::CapacityProvider,
    };
    let record =
        StoredOperationRecord::new_v19_legacy_adoption(OperationId::new(), fixture.prepare(), 40)
            .unwrap();
    let plan = record.full_capacity_plan.as_ref().unwrap();
    let claims = AdmissionClaimInventory::new(
        plan.allocations()
            .iter()
            .map(|row| {
                AdmissionClaim::new(
                    AdmissionClaimId::new(),
                    row.volume().clone(),
                    row.class(),
                    row.allocated_bytes(),
                )
                .unwrap()
            })
            .collect(),
    )
    .unwrap();
    let mut facts = BTreeMap::new();
    for row in plan.allocations() {
        let entry = facts.entry(row.volume().clone()).or_insert(VolumeFacts {
            identity: row.volume().clone(),
            free_bytes: PROTECTED_FREE_SPACE_FLOOR,
            allocation_unit: row.allocation_unit(),
        });
        entry.free_bytes += row.allocated_bytes() * 4;
    }
    (record, claims, CapacityProvider::qualified_fixture(facts))
}

#[test]
fn legacy_initial_borrowed_store_commit_keeps_original_lease_and_exact_readback() {
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;
    let f = Fixture::new();
    let (record, claims, provider) = settlement_fixture(&f);
    let wire = encode(&record).unwrap();
    let mut lease = provider
        .admit(record.full_capacity_plan.as_ref().unwrap(), &claims)
        .unwrap();
    let funding = JournalNamespaceFundingProbe::new();
    let baseline = funding.remaining_bytes();
    let (outcome, retained) = f
        .store
        .settle_legacy_initial_frozen(
            &record,
            &wire,
            &claims,
            &provider,
            &mut lease,
            &funding.input,
        )
        .unwrap();
    assert!(matches!(outcome, JournalInsertOutcome::Committed(_)));
    assert!(retained);
    f.store
        .verify_legacy_initial_frozen(&record, &claims, &funding.input)
        .unwrap();
    let snapshot = f
        .store
        .rebuild_admission_snapshot_for_input(&funding.input)
        .unwrap();
    assert!(matches!(
        snapshot.readiness(),
        crate::operation_journal::capacity_provider::CapacityRebuildReadiness::Ready
    ));
    assert_eq!(snapshot.claims().len(), claims.claims().len());
    assert!(
        snapshot
            .claims()
            .iter()
            .all(|row| !row.audit_pending() && row.release_id().is_none())
    );
    snapshot
        .validate_provider(&provider, Some(&funding.input))
        .unwrap();
    drop(snapshot);
    assert_eq!(funding.remaining_bytes(), baseline);
    assert_eq!(f.store.get(record.operation_id).unwrap(), Some(record));
    assert_eq!(provider.claim_count(), 1);
    drop(lease);
    assert_eq!(provider.claim_count(), 1);
}

#[test]
fn legacy_initial_borrowed_store_refuses_changed_frozen_bytes_before_pin() {
    use crate::operation_journal::{
        owner::JournalNamespaceFundingProbe, store::BorrowedInitialClaimSettlementError,
    };
    let f = Fixture::new();
    let (record, claims, provider) = settlement_fixture(&f);
    let mut wire = encode(&record).unwrap();
    wire.push(b' ');
    let mut lease = provider
        .admit(record.full_capacity_plan.as_ref().unwrap(), &claims)
        .unwrap();
    let funding = JournalNamespaceFundingProbe::new();
    assert!(matches!(
        f.store.settle_legacy_initial_frozen(
            &record,
            &wire,
            &claims,
            &provider,
            &mut lease,
            &funding.input
        ),
        Err(BorrowedInitialClaimSettlementError::Validation(_))
    ));
    assert!(f.store.get(record.operation_id).unwrap().is_none());
    lease.release().unwrap();
    assert_eq!(provider.claim_count(), 0);
}

#[test]
fn legacy_initial_borrowed_store_rollback_unknown_commit_and_ledger_failure_retain_exact_rules() {
    use crate::operation_journal::{
        owner::JournalNamespaceFundingProbe,
        store::{
            BorrowedInitialClaimSettlementError, InsertTestBoundary, InsertTestDirective,
            OperationJournalStoreError,
        },
    };
    for case in 0..4 {
        let f = Fixture::new();
        let (record, claims, provider) = settlement_fixture(&f);
        let wire = encode(&record).unwrap();
        let mut lease = provider
            .admit(record.full_capacity_plan.as_ref().unwrap(), &claims)
            .unwrap();
        if case == 3 {
            provider.fail_disposition_for_test();
        }
        f.store.set_insert_test_hook(move |boundary| {
            let selected = match case {
                0 => boundary == InsertTestBoundary::BeforePrecommitValidation,
                1 => boundary == InsertTestBoundary::Commit,
                2 => boundary == InsertTestBoundary::AfterCommit,
                _ => false,
            };
            if !selected {
                return InsertTestDirective::Continue;
            }
            let error = OperationJournalStoreError::IdentityCheckUnavailable {
                reason: "original legacy settlement boundary".into(),
            };
            if case == 1 {
                InsertTestDirective::ExecuteThenFail(error)
            } else {
                InsertTestDirective::Fail(error)
            }
        });
        let funding = JournalNamespaceFundingProbe::new();
        let result = f.store.settle_legacy_initial_frozen(
            &record,
            &wire,
            &claims,
            &provider,
            &mut lease,
            &funding.input,
        );
        if case == 3 {
            assert!(matches!(
                result,
                Err(BorrowedInitialClaimSettlementError::OutcomeRetained {
                    outcome: JournalInsertOutcome::Committed(_),
                    ..
                })
            ));
        } else {
            let (outcome, retained) = result.unwrap();
            assert_eq!(retained, case != 0);
            assert_eq!(
                outcome.failure().unwrap().stage(),
                match case {
                    0 => JournalInsertWriteStage::RolledBackBeforeCommit,
                    1 => JournalInsertWriteStage::CommitOutcomeUnknown,
                    _ => JournalInsertWriteStage::CommittedButNotAcknowledged,
                }
            );
        }
        assert_eq!(provider.claim_count(), usize::from(case != 0));
        assert_eq!(
            f.store.get(record.operation_id).unwrap().is_some(),
            case != 0
        );
        drop(lease);
        assert_eq!(provider.claim_count(), usize::from(case != 0));
    }
}

#[test]
fn legacy_initial_streaming_transport_matches_original_canonical_bytes_and_cap() {
    use base64::Engine;
    let f = Fixture::new();
    let original = f.prepare();
    for label_bytes in [0, 1, 2, 3, 100, 500] {
        let mut args = original.clone();
        args.data.profile_owner.push_str(&"q".repeat(label_bytes));
        let expected =
            base64::engine::general_purpose::STANDARD.encode(args.canonical_bytes().unwrap());
        assert_eq!(
            serde_json::to_string(&args).unwrap(),
            serde_json::to_string(&expected).unwrap()
        );
    }
    let mut escaped = original.clone();
    escaped.data.profile_owner.push_str("é\\\"");
    let expected =
        base64::engine::general_purpose::STANDARD.encode(escaped.canonical_bytes().unwrap());
    assert_eq!(
        serde_json::to_string(&escaped).unwrap(),
        serde_json::to_string(&expected).unwrap()
    );
    let mut invalid = original;
    invalid.data.objects[0].identity = "q".repeat(1025);
    assert!(serde_json::to_string(&invalid).is_err());
}

#[test]
fn legacy_initial_typed_estimator_covers_exact_three_role_classes_without_relabeling() {
    use crate::operation_journal::capacity_estimator::{
        CapacityEstimateDisposition as D, estimate_capacity,
    };
    let f = Fixture::new();
    let args = f.prepare();
    let plan = args.initial_capacity_plan().unwrap();
    let matrix = estimate_capacity(&UniversalCommandArgs::SourceLegacyAdoption(args)).unwrap();
    for row in matrix.rows() {
        let expected: u64 = plan
            .allocations()
            .iter()
            .filter(|a| a.class() == row.class())
            .map(|a| a.logical_bytes())
            .sum();
        if expected == 0 {
            assert!(matches!(row.disposition(), D::NotApplicable(_)));
        } else {
            assert_eq!(row.disposition(), D::Known(expected));
        }
    }
}

#[test]
fn legacy_adoption_source_receipt_is_bounded_canonical_and_matches_only_original_request() {
    use super::super::receipt::{
        ADOPTION_METADATA_KEY, AdoptionReceiptEnvelope, MAX_ADOPTION_RECEIPT_BYTES,
    };
    let fixture = Fixture::new();
    let args = fixture.prepare();
    let operation = OperationId::new();
    // Passive codec observations only: these digest fixtures issue no source mutation grant.
    let receipt =
        AdoptionReceiptEnvelope::capture(&args, operation, [1; 32], [2; 32], [3; 32], 3, 0)
            .unwrap();
    let encoded = receipt.encode().unwrap();
    assert_eq!(ADOPTION_METADATA_KEY, "source.legacy_adoption.v1");
    assert!(encoded.len() <= MAX_ADOPTION_RECEIPT_BYTES);
    let decoded = AdoptionReceiptEnvelope::decode(&encoded).unwrap();
    assert_eq!(decoded, receipt);
    assert!(decoded.matches_original(&args, operation));
    assert!(!decoded.matches_original(&args, OperationId::new()));
    let mut changed = args.clone();
    changed.data.command = SourceCommandId::new();
    assert!(!decoded.matches_original(&changed, operation));
    let wire = std::str::from_utf8(&encoded).unwrap();
    for malformed in [
        wire.replacen("{", "{\"version\":1,", 1),
        wire.replacen("{", "{\"unknown\":null,", 1),
        wire.replace("\"after_version\":13", "\"after_version\":14"),
        format!(" {wire}"),
        format!("{wire} false"),
        "x".repeat(MAX_ADOPTION_RECEIPT_BYTES + 1),
    ] {
        assert!(AdoptionReceiptEnvelope::decode(malformed.as_bytes()).is_err());
    }
}
