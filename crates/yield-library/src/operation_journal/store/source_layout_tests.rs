//! Real journal persistence fed by the actual retained SourceWriter fixture producer.
#[cfg(test)]
mod integration {
    use crate::app_dirs::WritableProfileGuard;
    use crate::durable_capacity::{PROTECTED_FREE_SPACE_FLOOR, VolumeIdentity};
    use crate::operation_journal::full_progress_codec::{self, StoredFullProgressRecord};
    use crate::operation_journal::source_layout_commands::SourceLayoutIntentArgs;
    use crate::operation_journal::store::{
        OperationJournalStore,
        recovery::{CoordinatorRecoveryRecord, CoordinatorRecoveryRowOutcome},
    };
    use crate::operation_journal::*;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    pub(crate) fn run(source: SourceLayoutIntentArgs, profile: &WritableProfileGuard) {
        let participants = source.participant_observation().unwrap();
        source.validate_against_participants(&participants).unwrap();
        let args = UniversalCommandArgs::SourceDerivedLayout(source.clone());
        let plan = derive_work_plan(&args, &participants).unwrap();
        assert_eq!(plan.plan_version(), WORK_PLAN_VERSION_SOURCE_LAYOUT);
        assert_eq!(plan.units().len(), 1);
        assert_eq!(
            plan.units()[0].key().role(),
            WorkUnitRole::SourceDerivedLayout
        );
        assert!(plan.edges().is_empty());
        assert!(derive_work_plan_v1(&args, &participants).is_err());
        assert_eq!(
            args.command_kind(),
            UniversalCommandKind::SourceDerivedLayout
        );
        assert!(serde_json::from_str::<SourceLayoutIntentArgs>("[1,2,3]").is_err());
        let wire = serde_json::to_string(&source).unwrap();
        assert_eq!(
            serde_json::from_str::<SourceLayoutIntentArgs>(&wire).unwrap(),
            source
        );
        assert!(
            serde_json::from_str::<SourceLayoutIntentArgs>(&format!(
                "\"{}\"",
                "A".repeat(4 * (512usize * 1024 + 8 * 1024).div_ceil(3) + 1)
            ))
            .is_err()
        );
        let mut stale = serde_json::to_value(&participants).unwrap();
        stale["descriptors"][0]["source_fence"]["source_revision"] =
            serde_json::json!(source.scope().revision() + 1);
        let stale: ParticipantSet = serde_json::from_value(stale).unwrap();
        assert!(source.validate_against_participants(&stale).is_err());
        let db = source.allocation().page_size() * source.allocation().maximum_pages();
        let sidecars = source.allocation().required_metadata_bytes() - db;
        let capacity = FullCapacityPlan::new(vec![
            allocation(
                FullCapacityAllocationClass::JournalRecord,
                MAX_RECORD_BYTES as u64,
            ),
            allocation(FullCapacityAllocationClass::SourceDatabaseCommit, db),
            allocation(FullCapacityAllocationClass::SourceWalShm, sidecars),
        ])
        .unwrap();
        let claims = AdmissionClaimInventory::new(
            capacity
                .allocations()
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    AdmissionClaim::new(
                        AdmissionClaimId::from_uuid(Uuid::from_u128(800 + i as u128)).unwrap(),
                        a.volume().clone(),
                        a.class(),
                        a.allocated_bytes(),
                    )
                    .unwrap()
                })
                .collect(),
        )
        .unwrap();
        // Fixed identity and claims are historical fixture observations, never admission.
        let id = OperationId::from_uuid(Uuid::from_u128(700));
        let intent = FullProgressIntentObservation::new(
            id,
            args.clone(),
            participants.clone(),
            plan.clone(),
            capacity.clone(),
            claims.clone(),
        )
        .unwrap();
        let mut bad_capacity = capacity.allocations().to_vec();
        bad_capacity[1] = allocation(FullCapacityAllocationClass::SourceDatabaseCommit, db - 1);
        assert!(
            FullProgressIntentObservation::new(
                id,
                args.clone(),
                participants.clone(),
                plan.clone(),
                FullCapacityPlan::new(bad_capacity).unwrap(),
                claims.clone()
            )
            .is_err()
        );
        let progress = WorkProgress::from_plan_with_admission_observations(
            id,
            &plan,
            &args,
            &participants,
            &capacity,
            claims,
        )
        .unwrap();
        assert_eq!(progress.phase(), OperationPhase::IntentDurable);
        let snapshot = progress.capture_full_progress_snapshot(&intent).unwrap();
        let record = StoredFullProgressRecord::new(snapshot).unwrap();
        assert_eq!(record.schema_version(), 2);
        let encoded = full_progress_codec::encode(&record).unwrap();
        assert!(encoded.len() < MAX_RECORD_BYTES);
        assert!(
            full_progress_codec::encode_for_test_with_limit(&record, encoded.len() - 1).is_err()
        );
        assert_eq!(full_progress_codec::decode(&encoded).unwrap(), record);
        let fence = intent.participants().descriptors()[0]
            .source_fence()
            .unwrap();
        let ordinary_source = SourceRevisionObservation::new(
            fence.source_id().clone(),
            fence.root_identity(),
            fence.database_identity(),
            fence.lifecycle_generation(),
            fence.lease_epoch(),
            fence.source_revision(),
        )
        .unwrap();
        let mut forged = serde_json::from_slice::<serde_json::Value>(&encoded).unwrap();
        forged["snapshot"]["steps"][0]["status"] = serde_json::json!("Completed");
        forged["snapshot"]["steps"][0]["evidence"] =
            serde_json::to_value(StoredStepEvidence::SourceCommitted {
                before_revision: fence.source_revision(),
                after: ordinary_source,
            })
            .unwrap();
        forged["snapshot"]["steps"][0]["completion_context"] = serde_json::to_value(
            CompletionContext::new(1, record.snapshot().scopes()[0].clone()),
        )
        .unwrap();
        assert!(full_progress_codec::decode(&serde_json::to_vec(&forged).unwrap()).is_err());
        let mut wrong_schema = serde_json::from_slice::<serde_json::Value>(&encoded).unwrap();
        wrong_schema["schema_version"] = serde_json::json!(1);
        assert!(full_progress_codec::decode(&serde_json::to_vec(&wrong_schema).unwrap()).is_err());

        let store = OperationJournalStore::open(profile).unwrap();
        assert_eq!(store.persist_full(&record, None).unwrap().sequence(), 1);
        assert!(store.persist_full(&record, None).unwrap().is_replay());
        drop(store);
        let store = OperationJournalStore::open(profile).unwrap();
        assert_eq!(store.get_full(id).unwrap().unwrap().record(), &record);
        let page = store.scan_coordinator_recovery_page(None).unwrap();
        assert!(
            matches!(page.entries()[0].outcome(), CoordinatorRecoveryRowOutcome::Consistent(CoordinatorRecoveryRecord::FullProgress(r)) if r == &record)
        );
        // Unknown/corrupt/stale supported command evidence is retained, never rewritten.
        for mut raw in [
            {
                let mut v = serde_json::from_slice::<serde_json::Value>(&encoded).unwrap();
                v["schema_version"] = serde_json::json!(999);
                serde_json::to_vec(&v).unwrap()
            },
            b"{\"record_kind\":\"full_progress\",\"schema_version\":2,\"broken\":true}".to_vec(),
            {
                let mut v = serde_json::from_slice::<serde_json::Value>(&encoded).unwrap();
                v["snapshot"]["intent"]["participants"]["descriptors"][0]["source_fence"]["source_revision"] =
                    serde_json::json!(source.scope().revision() + 1);
                serde_json::to_vec(&v).unwrap()
            },
        ] {
            store
                .connection
                .execute(
                    "UPDATE operation_journal_full_progress SET record=?1 WHERE operation_id=?2",
                    rusqlite::params![&raw, id.to_string()],
                )
                .unwrap();
            let page = store.scan_coordinator_recovery_page(None).unwrap();
            assert!(matches!(
                page.entries()[0].outcome(),
                CoordinatorRecoveryRowOutcome::Retained(_)
            ));
            let retained: Vec<u8> = store
                .connection
                .query_row(
                    "SELECT record FROM operation_journal_full_progress WHERE operation_id=?1",
                    [id.to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(retained, raw);
            raw.clear();
        }
        store
            .connection
            .execute(
                "UPDATE operation_journal_full_progress SET record=?1 WHERE operation_id=?2",
                rusqlite::params![&encoded, id.to_string()],
            )
            .unwrap();
        let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while owner.status() == OperationJournalOwnerStatus::Initializing {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(owner.status(), OperationJournalOwnerStatus::Available);
        let client = owner.client();
        let observed = client
            .get_full(id)
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(observed, record);
        let page = client
            .scan_coordinator_recovery_page(None)
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(page.entries().len(), 1);
        let denied = client
            .admit_full(args, participants)
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap_err();
        assert!(denied.to_string().contains("pre-intent admission latch"));
        assert!(
            client
                .persist_full(record.clone(), Some(1))
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .is_replay()
        );
        assert!(owner.shutdown());
        let store = OperationJournalStore::open(profile).unwrap();
        assert_eq!(
            store
                .scan_coordinator_recovery_page(None)
                .unwrap()
                .entries()
                .len(),
            1
        );
        assert_eq!(store.get_full(id).unwrap().unwrap().record(), &record);
    }
    fn allocation(class: FullCapacityAllocationClass, n: u64) -> FullCapacityAllocation {
        FullCapacityAllocation::new(
            VolumeIdentity { device: 1 },
            4096,
            class,
            n,
            n.div_ceil(4096) * 4096,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .unwrap()
    }
}
pub(crate) use integration::run;
