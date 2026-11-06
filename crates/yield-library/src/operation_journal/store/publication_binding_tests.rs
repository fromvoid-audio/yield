mod publication_issuance {
    use super::super::publication_bindings::PublicationBindingDraft;
    use super::*;
    use crate::operation_journal::ParticipantId;
    use crate::operation_journal::writer_acquisition_telemetry::{
        JournalWriterCall, JournalWriterReturn, tests::Capture,
    };

    // Authority-free durable row observations only. This fixture creates no Native provider,
    // source pin, live file-owner target, publication grant or physical allocation proof.
    fn prepared() -> (
        Fixture,
        OperationJournalStore,
        staging_checkpoints::StagingCheckpointRecord,
        PublicationBindingDraft,
        i64,
    ) {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("original store");
        let (operation, intent, universal, args, legacy) = v5_copy_inputs();
        let participant = universal
            .participants()
            .descriptors()
            .iter()
            .find(|p| p.kind() == ParticipantKind::Filesystem)
            .expect("filesystem participant")
            .id();
        let plan = FullCapacityPlan::from_legacy(&legacy).expect("capacity observations");
        let volume = plan.allocations()[0].volume().clone();
        let record = StoredOperationRecord::new_v16_command(
            operation,
            intent,
            universal,
            UniversalCommandArgs::Copy(args),
            plan,
            50,
        );
        store
            .insert_intent(&record)
            .expect("actual original intent transaction");
        let claim_id = AdmissionClaimId::new();
        store.connection.execute(
            "INSERT INTO operation_journal_ordinary_claim(claim_id, operation_id, volume_device, allocation_class, allocated_bytes) VALUES (?1, ?2, '1', 'DestinationStaging', 1)",
            params![claim_id.to_string(), operation.to_string()]).expect("claim observations");
        let claim = AdmissionClaim::new(
            claim_id,
            volume,
            FullCapacityAllocationClass::DestinationStaging,
            1,
        )
        .expect("charge observations");
        let owner = Uuid::new_v4();
        let root =
            serde_json::json!({"owner":owner,"registration_id":Uuid::new_v4(),"generation":1});
        let object = Uuid::new_v4();
        let staging = staging_checkpoints::StagingCheckpointRecord::decode(
            &serde_json::to_vec(&serde_json::json!({
                "schema_version":1,"operation_id":operation,"participant_id":participant,
                "effect_id":Uuid::new_v4(),"output_id":Uuid::new_v4(),"staging_root":root,
                "source_root":null,"source_fence":null,"locator":"staged.wav",
                "logical_object_id":object,"physical_object_id":"row-observation-only",
                "bytes":1,"content_hash":vec![2;32],"request_fingerprint":vec![3;32]
            }))
            .expect("staging bytes"),
        )
        .expect("staging observation");
        let canonical = command_bytes(&staging, owner, Uuid::new_v4(), "out.wav");
        let binding = PublicationBindingDraft::capture(
            &canonical,
            &staging,
            &claim,
            (2, 1),
            None,
            (Uuid::new_v4(), 1),
        )
        .expect("capture data-only binding");
        let sequence = match store
            .checkpoint_staging(&staging)
            .expect("actual checkpoint transaction")
        {
            staging_checkpoints::StagingCheckpointReceipt::Committed { sequence } => sequence,
            _ => panic!("fresh checkpoint cannot replay"),
        };
        (fixture, store, staging, binding, sequence)
    }

    #[test]
    fn staging_writer_acquisition_reports_actual_sqlite_contention_and_retry() {
        let telemetry = Capture::new();
        let (_fixture, store, staging, binding, sequence) = prepared();
        // Release the fixture's persistent exclusive lock before installing a
        // competing writer. The original Store entry points still issue BEGIN EXCLUSIVE.
        store
            .connection
            .execute_batch("PRAGMA locking_mode = NORMAL; BEGIN EXCLUSIVE; COMMIT;")
            .unwrap();
        store.connection.busy_timeout(std::time::Duration::ZERO).unwrap();
        let blocker = rusqlite::Connection::open(&store.database_path).unwrap();
        blocker.busy_timeout(std::time::Duration::ZERO).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let checkpoint_error = store.checkpoint_staging(&staging).unwrap_err();
        let reservation_error = store
            .reserve_publication_binding_fixture(&staging, sequence, binding.clone())
            .unwrap_err();
        for error in [checkpoint_error, reservation_error] {
            assert!(matches!(
                error,
                OperationJournalStoreError::Sql(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::DatabaseBusy
            ));
        }
        assert!(store.connection.is_autocommit());
        assert!(!store.read_staging_checkpoint_row(staging.output_id()).unwrap().unwrap().2);
        assert!(store.read_publication_binding(staging.output_id()).unwrap().is_none());

        blocker.execute_batch("ROLLBACK").unwrap();
        assert!(matches!(
            store.checkpoint_staging(&staging).unwrap(),
            staging_checkpoints::StagingCheckpointReceipt::Replay { .. }
        ));
        store
            .reserve_publication_binding_fixture(&staging, sequence, binding)
            .unwrap();
        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.count(JournalWriterCall::StagingCheckpoint, JournalWriterReturn::Busy), 1);
        assert_eq!(snapshot.count(JournalWriterCall::PublicationReservation, JournalWriterReturn::Busy), 1);
        assert_eq!(snapshot.count(JournalWriterCall::StagingCheckpoint, JournalWriterReturn::Acquired), 2);
        assert_eq!(snapshot.count(JournalWriterCall::PublicationReservation, JournalWriterReturn::Acquired), 1);
    }

    // Real original SQLite persistence under a test-only scalar slot; no physical grant.
    fn no_effect_prepared() -> (
        Fixture,
        OperationJournalStore,
        staging_checkpoints::StagingCheckpointRecord,
        super::super::super::owner::JournalNamespaceFundingProbe,
        super::super::super::owner::NoEffectSlotFixture,
    ) {
        let (fixture, store, staging, binding, sequence) = prepared();
        let funding = super::super::super::owner::JournalNamespaceFundingProbe::new_publication();
        let slot = funding.original_no_effect_slot(staging.operation_id(), staging.output_id());
        let issuer = slot.issuer();
        let mut observed = serde_json::to_value(binding).unwrap();
        observed["issuer_incarnation"] = serde_json::json!(issuer.0);
        observed["slot_generation"] = serde_json::json!(issuer.1);
        let binding = serde_json::from_value(observed.clone()).unwrap();
        store
            .reserve_publication_binding_funded_fixture(&staging, sequence, binding, &funding.input)
            .unwrap();
        let fingerprint: [u8; 32] =
            serde_json::from_value(observed["command_fingerprint"].clone()).unwrap();
        slot.complete(
            fingerprint,
            "unsupported original no-effect storage fixture",
        );
        (fixture, store, staging, funding, slot)
    }

    #[test]
    fn original_no_effect_completion_exact_readback_preserves_issuance_staging_and_charge() {
        let (_fixture, store, staging, funding, slot) = no_effect_prepared();
        let before = store.publication_binding_observation_for_test(staging.output_id());
        let pool = funding.publication_observer();
        let baseline = pool();
        slot.settle(|completion| {
            store
                .settle_original_publication_no_effect(completion, &funding.input)
                .is_ok()
        });
        assert!(!slot.unresolved());
        assert_eq!(pool(), baseline);
        let after = store.publication_binding_observation_for_test(staging.output_id());
        assert_eq!(after["schema_version"], serde_json::json!(2));
        assert_eq!(after["no_effect"]["kind"], serde_json::json!(1));
        assert_eq!(
            after["no_effect"]["reason"],
            serde_json::json!("unsupported original no-effect storage fixture")
        );
        for key in before
            .as_object()
            .unwrap()
            .keys()
            .filter(|key| key.as_str() != "schema_version")
        {
            assert_eq!(after[key], before[key]);
        }
        assert!(
            store
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .2
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM operation_journal_ordinary_claim_release",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert!(store.prove_clean_handoff_durable().is_err());
    }

    #[test]
    fn original_no_effect_duplicate_readback_and_restart_preserve_exact_completion() {
        let (fixture, store, staging, funding, slot) = no_effect_prepared();
        let before = funding.publication_observer()();
        slot.settle(|completion| {
            let first = store
                .settle_original_publication_no_effect(completion, &funding.input)
                .unwrap();
            assert_eq!(format!("{first:?}"), "Committed");
            let original = store.publication_binding_observation_for_test(staging.output_id());
            let replay = store
                .settle_original_publication_no_effect(completion, &funding.input)
                .unwrap();
            assert_eq!(format!("{replay:?}"), "Replay");
            assert_eq!(
                store.publication_binding_observation_for_test(staging.output_id()),
                original
            );
            true
        });
        assert!(!slot.unresolved());
        assert_eq!(funding.publication_observer()(), before);
        let original = store.publication_binding_observation_for_test(staging.output_id());
        drop(store);
        let reopened =
            OperationJournalStore::open(&fixture.owner).expect("actual no-effect restart");
        assert_eq!(
            reopened.publication_binding_observation_for_test(staging.output_id()),
            original
        );
        assert!(
            reopened
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .2
        );
        assert_eq!(
            reopened
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM operation_journal_ordinary_claim",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            reopened
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM operation_journal_ordinary_claim_release",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert!(reopened.prove_clean_handoff_durable().is_err());
    }

    #[test]
    fn original_no_effect_corrupt_or_oversized_evidence_cannot_close_original_slot() {
        for malformed in ["digest", "reason", "kind"] {
            let (_fixture, store, staging, funding, slot) = no_effect_prepared();
            let mut original = store.publication_binding_observation_for_test(staging.output_id());
            original["schema_version"] = serde_json::json!(2);
            original["no_effect"] =
                serde_json::json!({"kind":1,"reason":"bounded diagnostic","digest":vec![7;32]});
            match malformed {
                "reason" => original["no_effect"]["reason"] = serde_json::json!("x".repeat(513)),
                "kind" => original["no_effect"]["kind"] = serde_json::json!(0),
                _ => {}
            }
            let typed: PublicationBindingDraft = serde_json::from_value(original.clone()).unwrap();
            let encoded = typed.encode().unwrap();
            store.connection.execute("UPDATE operation_journal_publication_issuance SET record = ?1 WHERE output_id = ?2", params![encoded, staging.output_id().to_string()]).unwrap();
            slot.settle(|completion| {
                store
                    .settle_original_publication_no_effect(completion, &funding.input)
                    .is_ok()
            });
            assert!(slot.unresolved());
            assert!(store.connection.is_autocommit());
            let retained: Vec<u8> = store.connection.query_row(
                "SELECT record FROM operation_journal_publication_issuance WHERE output_id = ?1",
                [staging.output_id().to_string()], |row| row.get(0)).unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&retained).unwrap(),
                original
            );
            assert!(store.read_publication_binding(staging.output_id()).is_err());
            assert_eq!(
                store
                    .connection
                    .query_row(
                        "SELECT COUNT(*) FROM operation_journal_ordinary_claim_release",
                        [],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn original_no_effect_actor_poll_persists_or_quarantines_without_queue_admission() {
        for lose_ack in [false, true] {
            let (_fixture, store, staging, funding, mut slot) = no_effect_prepared();
            if lose_ack {
                store.set_insert_test_hook(|boundary| {
                    if boundary == InsertTestBoundary::AfterCommit {
                        InsertTestDirective::Fail(OperationJournalStoreError::SequenceExhausted)
                    } else {
                        InsertTestDirective::Continue
                    }
                });
            }
            let before = funding.publication_observer()();
            let store = slot.poll_original_actor(store);
            assert_eq!(slot.unresolved(), lose_ack);
            let retained = store.publication_binding_observation_for_test(staging.output_id());
            assert_eq!(
                retained["no_effect"]["reason"],
                serde_json::json!("unsupported original no-effect storage fixture")
            );
            assert_eq!(
                store
                    .connection
                    .query_row(
                        "SELECT COUNT(*) FROM operation_journal_ordinary_claim_release",
                        [],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
            if !lose_ack {
                assert_eq!(funding.publication_observer()(), before);
            }
            assert!(store.prove_clean_handoff_durable().is_err());
        }
    }

    #[test]
    fn original_no_effect_failure_before_commit_rolls_back_and_keeps_original_latch_uncertain() {
        let (_fixture, store, staging, funding, slot) = no_effect_prepared();
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                InsertTestDirective::Fail(OperationJournalStoreError::SequenceExhausted)
            } else {
                InsertTestDirective::Continue
            }
        });
        slot.settle(|completion| {
            store
                .settle_original_publication_no_effect(completion, &funding.input)
                .is_ok()
        });
        assert!(slot.unresolved());
        assert!(store.connection.is_autocommit());
        let record = store.publication_binding_observation_for_test(staging.output_id());
        assert_eq!(record["schema_version"], serde_json::json!(1));
        assert!(record.get("no_effect").is_none());
        assert!(
            store
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .2
        );
    }

    #[test]
    fn original_no_effect_lost_commit_ack_retains_durable_record_and_uncertain_live_slot() {
        for boundary in [InsertTestBoundary::Commit, InsertTestBoundary::AfterCommit] {
            let (_fixture, store, staging, funding, slot) = no_effect_prepared();
            store.set_insert_test_hook(move |at| {
                if at == boundary {
                    if at == InsertTestBoundary::Commit {
                        InsertTestDirective::ExecuteThenFail(
                            OperationJournalStoreError::SequenceExhausted,
                        )
                    } else {
                        InsertTestDirective::Fail(OperationJournalStoreError::SequenceExhausted)
                    }
                } else {
                    InsertTestDirective::Continue
                }
            });
            slot.settle(|completion| {
                store
                    .settle_original_publication_no_effect(completion, &funding.input)
                    .is_ok()
            });
            assert!(slot.unresolved());
            assert!(store.connection.is_autocommit());
            let record = store.publication_binding_observation_for_test(staging.output_id());
            assert_eq!(record["schema_version"], serde_json::json!(2));
            assert_eq!(record["no_effect"]["kind"], serde_json::json!(1));
            assert_eq!(
                record["no_effect"]["reason"],
                serde_json::json!("unsupported original no-effect storage fixture")
            );
            assert!(store.prove_clean_handoff_durable().is_err());
        }
    }

    #[test]
    fn original_no_effect_foreign_issuance_and_changed_digest_refuse_without_overwriting() {
        for field in ["issuer_incarnation", "slot_generation", "no_effect"] {
            let (_fixture, store, staging, funding, slot) = no_effect_prepared();
            let mut original = store.publication_binding_observation_for_test(staging.output_id());
            match field {
                "issuer_incarnation" => original[field] = serde_json::json!(Uuid::new_v4()),
                "slot_generation" => original[field] = serde_json::json!(2),
                _ => {
                    original["schema_version"] = serde_json::json!(2);
                    let fingerprint: [u8; 32] =
                        serde_json::from_value(original["command_fingerprint"].clone()).unwrap();
                    let reason = "conflicting exact no-effect diagnostic";
                    original[field] = serde_json::json!({"kind":1,"reason":reason,"digest":crate::file_operation_owner::publication_no_effect_digest(1, fingerprint, reason)});
                }
            }
            let record = serde_json::to_vec(&original).unwrap();
            // Re-encode the typed draft to maintain exact canonical field ordering.
            let typed: PublicationBindingDraft = serde_json::from_slice(&record).unwrap();
            let bytes = typed.encode().unwrap();
            store.connection.execute("UPDATE operation_journal_publication_issuance SET record = ?1 WHERE output_id = ?2", params![bytes, staging.output_id().to_string()]).unwrap();
            slot.settle(|completion| {
                store
                    .settle_original_publication_no_effect(completion, &funding.input)
                    .is_ok()
            });
            assert!(slot.unresolved());
            assert_eq!(
                store.publication_binding_observation_for_test(staging.output_id()),
                original
            );
        }
    }

    #[test]
    fn original_publication_input_funds_real_namespace_checkpoint_and_reserved_binding() {
        let (_fixture, store, staging, binding, _) = prepared();
        store
            .connection
            .execute("DELETE FROM operation_journal_staging_checkpoints", [])
            .unwrap();
        let funding = super::super::super::owner::JournalNamespaceFundingProbe::new_publication();
        let observer = funding.publication_observer();
        let before = observer();
        let sequence = match store
            .checkpoint_staging_with_input(&staging, Some(&funding.input))
            .unwrap()
        {
            staging_checkpoints::StagingCheckpointReceipt::Committed { sequence } => sequence,
            _ => panic!("fresh original checkpoint"),
        };
        assert_eq!(observer(), before);
        store
            .reserve_publication_binding_funded_fixture(&staging, sequence, binding, &funding.input)
            .unwrap();
        assert_eq!(observer(), before);
        assert!(
            store
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .2
        );
        assert!(store
            .read_publication_binding(staging.output_id())
            .unwrap()
            .is_some());
    }

    #[test]
    fn original_publication_namespace_failure_keeps_both_debits_through_selected_error() {
        let (_fixture, store, staging, binding, sequence) = prepared();
        let funding = super::super::super::owner::JournalNamespaceFundingProbe::new_publication();
        let observer = funding.publication_observer();
        let ordinary = funding.pool_bytes_observer_for_test();
        let before = observer();
        let sidecar = store
            .database_path
            .with_file_name(format!("{JOURNAL_DATABASE_FILE_NAME}{SQLITE_WAL_SUFFIX}"));
        std::fs::write(sidecar, b"unexpected retained namespace evidence").unwrap();
        let error = store
            .reserve_publication_binding_funded_fixture(&staging, sequence, binding, &funding.input)
            .unwrap_err();
        assert!(observer().0 < before.0 && observer().1 < before.1);
        let error = funding.retain_command_error_for_test(error);
        drop(funding);
        assert!(observer().0 < ordinary().1);
        assert!(observer().1 < 2 * 1024 * 1024);
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT grant_reserved FROM operation_journal_staging_checkpoints",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert!(matches!(
            error,
            super::super::super::owner::OperationJournalOwnerError::RetainedBacking(_)
        ));
        drop(error);
        assert_eq!(observer(), (ordinary().1, 2 * 1024 * 1024));
    }

    #[test]
    fn original_publication_subset_exhaustion_refuses_before_sql_with_ordinary_credit_remaining() {
        let (_fixture, store, staging, binding, sequence) = prepared();
        let funding = super::super::super::owner::JournalNamespaceFundingProbe::new_publication();
        let observer = funding.publication_observer();
        let before = observer();
        let pressure = funding.input.reserve_historical_scratch(before.1).unwrap();
        assert_eq!(observer().1, 0);
        assert!(observer().0 > 1024 * 1024);
        assert!(matches!(
            store.reserve_publication_binding_funded_fixture(
                &staging,
                sequence,
                binding,
                &funding.input
            ),
            Err(OperationJournalStoreError::OrdinaryRebuildResourceBound)
        ));
        assert!(
            !store
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .2
        );
        assert!(store
            .read_publication_binding(staging.output_id())
            .unwrap()
            .is_none());
        drop(pressure);
        assert_eq!(observer(), before);
    }

    #[test]
    fn original_publication_postcommit_failure_keeps_reserved_evidence_and_both_error_debits() {
        let (_fixture, store, staging, binding, sequence) = prepared();
        let funding = super::super::super::owner::JournalNamespaceFundingProbe::new_publication();
        let observer = funding.publication_observer();
        let ordinary = funding.pool_bytes_observer_for_test();
        let path = store.database_path.clone();
        store.set_insert_test_hook(move |boundary| {
            if boundary == InsertTestBoundary::AfterCommit {
                let sidecar =
                    path.with_file_name(format!("{JOURNAL_DATABASE_FILE_NAME}{SQLITE_WAL_SUFFIX}"));
                std::fs::write(sidecar, b"unexpected postcommit evidence").unwrap();
            }
            InsertTestDirective::Continue
        });
        let error = store
            .reserve_publication_binding_funded_fixture(&staging, sequence, binding, &funding.input)
            .unwrap_err();
        assert!(matches!(
            error,
            OperationJournalStoreError::StagingOutcomeUncertain { .. }
        ));
        let error = funding.retain_command_error_for_test(error);
        drop(funding);
        assert!(observer().0 < ordinary().1 && observer().1 < 2 * 1024 * 1024);
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT grant_reserved FROM operation_journal_staging_checkpoints",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM operation_journal_publication_issuance",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        drop(error);
        assert_eq!(observer(), (ordinary().1, 2 * 1024 * 1024));
    }

    #[test]
    fn publication_namespace_pressure_refuses_checkpoint_and_grant_before_writes() {
        let (_fixture, store, staging, binding, sequence) = prepared();
        let funding = super::super::super::owner::JournalNamespaceFundingProbe::new();
        let pressure = funding
            .input
            .reserve_historical_scratch(funding.remaining_bytes())
            .unwrap();
        assert!(matches!(
            store.reserve_publication_binding_funded_fixture(
                &staging,
                sequence,
                binding.clone(),
                &funding.input
            ),
            Err(OperationJournalStoreError::OrdinaryRebuildResourceBound)
        ));
        assert!(
            !store
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .2
        );
        assert!(store
            .read_publication_binding(staging.output_id())
            .unwrap()
            .is_none());
        store
            .connection
            .execute("DELETE FROM operation_journal_staging_checkpoints", [])
            .unwrap();
        assert!(matches!(
            store.checkpoint_staging_with_input(&staging, Some(&funding.input)),
            Err(OperationJournalStoreError::OrdinaryRebuildResourceBound)
        ));
        assert!(store
            .read_staging_checkpoint_row(staging.output_id())
            .unwrap()
            .is_none());
        drop(pressure);
        assert!(matches!(
            store
                .checkpoint_staging_with_input(&staging, Some(&funding.input))
                .unwrap(),
            staging_checkpoints::StagingCheckpointReceipt::Committed { .. }
        ));
    }

    #[test]
    fn publication_namespace_success_and_replay_return_temporary_debits() {
        let (_fixture, store, staging, binding, _) = prepared();
        store
            .connection
            .execute("DELETE FROM operation_journal_staging_checkpoints", [])
            .unwrap();
        let funding = super::super::super::owner::JournalNamespaceFundingProbe::new();
        let before = funding.remaining_bytes();
        let sequence = match store
            .checkpoint_staging_with_input(&staging, Some(&funding.input))
            .unwrap()
        {
            staging_checkpoints::StagingCheckpointReceipt::Committed { sequence } => sequence,
            _ => panic!("fresh checkpoint"),
        };
        assert_eq!(funding.remaining_bytes(), before);
        assert_eq!(
            store
                .checkpoint_staging_with_input(&staging, Some(&funding.input))
                .unwrap(),
            staging_checkpoints::StagingCheckpointReceipt::Replay { sequence }
        );
        assert_eq!(funding.remaining_bytes(), before);
        store
            .reserve_publication_binding_funded_fixture(&staging, sequence, binding, &funding.input)
            .unwrap();
        assert_eq!(funding.remaining_bytes(), before);
        assert!(
            store
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .2
        );
        assert!(store
            .read_publication_binding(staging.output_id())
            .unwrap()
            .is_some());
    }

    #[test]
    fn publication_namespace_selected_failure_keeps_original_debit_after_input_drop() {
        let (_fixture, store, staging, binding, sequence) = prepared();
        let funding = super::super::super::owner::JournalNamespaceFundingProbe::new();
        let observer = funding.pool_bytes_observer_for_test();
        let sidecar = store
            .database_path
            .with_file_name(format!("{JOURNAL_DATABASE_FILE_NAME}{SQLITE_WAL_SUFFIX}"));
        std::fs::write(sidecar, b"unexpected namespace evidence").unwrap();
        let before = funding.remaining_bytes();
        let error = store
            .reserve_publication_binding_funded_fixture(&staging, sequence, binding, &funding.input)
            .unwrap_err();
        assert!(funding.remaining_bytes() < before);
        let error = funding.retain_command_error_for_test(error);
        assert!(matches!(
            error,
            super::super::super::owner::OperationJournalOwnerError::RetainedBacking(_)
        ));
        drop(funding);
        assert!(observer().0 < observer().1);
        drop(error);
        assert_eq!(observer().0, observer().1);
        // The actual open SQLite connection remains unchanged; no reservation was written.
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT grant_reserved FROM operation_journal_staging_checkpoints",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn publication_namespace_postcommit_failure_retains_reserved_binding_and_error_debit() {
        let (_fixture, store, staging, binding, sequence) = prepared();
        let funding = super::super::super::owner::JournalNamespaceFundingProbe::new();
        let observer = funding.pool_bytes_observer_for_test();
        let path = store.database_path.clone();
        store.set_insert_test_hook(move |boundary| {
            if boundary == InsertTestBoundary::AfterCommit {
                let sidecar =
                    path.with_file_name(format!("{JOURNAL_DATABASE_FILE_NAME}{SQLITE_WAL_SUFFIX}"));
                std::fs::write(sidecar, b"unexpected namespace evidence").unwrap();
            }
            InsertTestDirective::Continue
        });
        let error = store
            .reserve_publication_binding_funded_fixture(&staging, sequence, binding, &funding.input)
            .unwrap_err();
        assert!(matches!(
            error,
            OperationJournalStoreError::StagingOutcomeUncertain { .. }
        ));
        let error = funding.retain_command_error_for_test(error);
        drop(funding);
        assert!(observer().0 < observer().1);
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT grant_reserved FROM operation_journal_staging_checkpoints",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM operation_journal_publication_issuance",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        drop(error);
        assert_eq!(observer().0, observer().1);
    }

    #[test]
    fn publication_namespace_precommit_pressure_rolls_back_both_original_transactions() {
        for grant in [false, true] {
            let (_fixture, store, staging, binding, sequence) = prepared();
            if !grant {
                store
                    .connection
                    .execute("DELETE FROM operation_journal_staging_checkpoints", [])
                    .unwrap();
            }
            let funding = std::sync::Arc::new(
                super::super::super::owner::JournalNamespaceFundingProbe::new(),
            );
            let pressure = std::sync::Arc::new(std::sync::Mutex::new(None));
            let held = pressure.clone();
            let original = funding.clone();
            store.set_insert_test_hook(move |boundary| {
                if boundary == InsertTestBoundary::BeforePrecommitValidation {
                    *held.lock().unwrap() = Some(
                        original
                            .input
                            .reserve_historical_scratch(original.remaining_bytes())
                            .unwrap(),
                    );
                }
                InsertTestDirective::Continue
            });
            let result = if grant {
                store.reserve_publication_binding_funded_fixture(
                    &staging,
                    sequence,
                    binding,
                    &funding.input,
                )
            } else {
                store
                    .checkpoint_staging_with_input(&staging, Some(&funding.input))
                    .map(|_| ())
            };
            assert!(matches!(
                result,
                Err(OperationJournalStoreError::OrdinaryRebuildResourceBound)
            ));
            assert!(store.connection.is_autocommit());
            assert!(store
                .read_publication_binding(staging.output_id())
                .unwrap()
                .is_none());
            let row = store
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap();
            assert_eq!(row.is_some(), grant);
            assert!(row.is_none_or(|(_, _, reserved)| !reserved));
            drop(pressure.lock().unwrap().take());
        }
    }

    #[test]
    fn publication_namespace_postcommit_pressure_preserves_commit_with_fixed_uncertainty() {
        for grant in [false, true] {
            let (_fixture, store, staging, binding, sequence) = prepared();
            if !grant {
                store
                    .connection
                    .execute("DELETE FROM operation_journal_staging_checkpoints", [])
                    .unwrap();
            }
            let funding = std::sync::Arc::new(
                super::super::super::owner::JournalNamespaceFundingProbe::new(),
            );
            let pressure = std::sync::Arc::new(std::sync::Mutex::new(None));
            let held = pressure.clone();
            let original = funding.clone();
            store.set_insert_test_hook(move |boundary| {
                if boundary == InsertTestBoundary::AfterCommit {
                    *held.lock().unwrap() = Some(
                        original
                            .input
                            .reserve_historical_scratch(original.remaining_bytes())
                            .unwrap(),
                    );
                }
                InsertTestDirective::Continue
            });
            let result = if grant {
                store.reserve_publication_binding_funded_fixture(
                    &staging,
                    sequence,
                    binding,
                    &funding.input,
                )
            } else {
                store
                    .checkpoint_staging_with_input(&staging, Some(&funding.input))
                    .map(|_| ())
            };
            assert!(matches!(
                result,
                Err(OperationJournalStoreError::StagingNamespaceOutcomeUncertain)
            ));
            assert!(store.connection.is_autocommit());
            assert_eq!(
                store
                    .read_staging_checkpoint_row(staging.output_id())
                    .unwrap()
                    .unwrap()
                    .2,
                grant
            );
            assert_eq!(
                store
                    .read_publication_binding(staging.output_id())
                    .unwrap()
                    .is_some(),
                grant
            );
            assert_eq!(funding.remaining_bytes(), 0);
            drop(pressure.lock().unwrap().take());
        }
    }

    fn command_bytes(
        staging: &staging_checkpoints::StagingCheckpointRecord,
        owner: Uuid,
        effect: Uuid,
        leaf: &str,
    ) -> Vec<u8> {
        let stage = serde_json::to_value(staging).expect("stage projection");
        let target =
            serde_json::json!({"owner":owner,"registration_id":Uuid::new_v4(),"generation":1});
        let authority =
            serde_json::json!({"Profile":{"owner":owner,"generation":1,"lease_epoch":1}});
        PublicationBindingDraft::canonical_command_fixture(serde_json::json!({
            "operation_id":staging.operation_id(),"participant_id":staging.participant_id(),
            "effect_id":effect,"source_fences":[],"target_registration":target,"expected":"Absent",
            "effect":{"PublishNoReplace":{
                "staging":{"binding":{"registration":stage["staging_root"],"authority":authority,
                    "locator":"staged.wav","expected":{"Exact":stage["logical_object_id"]}}},
                "target":{"binding":{"registration":target,"authority":authority,"locator":leaf,"expected":"Absent"}},
                "staged":{"object":stage["logical_object_id"],"bytes":1},"expected_absent":{"marker":1}
            }}
        }))
    }

    #[test]
    fn publication_issuance_current_namespace_requires_exact_binding_shape() {
        for malformed in [
            "CREATE TABLE operation_journal_publication_issuance(output_id TEXT NOT NULL PRIMARY KEY, publication_effect_id TEXT NOT NULL UNIQUE, record TEXT NOT NULL)",
            "CREATE TABLE operation_journal_publication_issuance(output_id TEXT NOT NULL PRIMARY KEY, publication_effect_id TEXT NOT NULL, record BLOB NOT NULL)",
        ] {
            let (fixture, store, _staging, _binding, _sequence) = prepared();
            store
                .connection
                .execute_batch("DROP TABLE operation_journal_publication_issuance")
                .unwrap();
            store.connection.execute_batch(malformed).unwrap();
            // Exercise the exact layout validator used by actual open/migration.
            assert!(
                validate_existing_schema(
                    &store.connection,
                    &store.database_path,
                    JOURNAL_SCHEMA_VERSION
                )
                .is_err()
            );
            drop(store);
            assert!(OperationJournalStore::open(&fixture.owner).is_err());
        }
    }

    #[test]
    fn publication_issuance_preserves_original_nested_target_locator_contract() {
        let (_fixture, store, staging, binding, sequence) = prepared();
        let value = serde_json::to_value(&binding).unwrap();
        let owner: Uuid =
            serde_json::from_value(value["command"]["target_registration"]["owner"].clone())
                .unwrap();
        let claim: AdmissionClaim = serde_json::from_value(value["charge"].clone()).unwrap();
        let leaf = "nested/target/out.wav";
        crate::file_operation_owner::BoundedRelativeLocator::new(leaf).unwrap();
        let canonical = command_bytes(&staging, owner, Uuid::new_v4(), leaf);
        let nested = PublicationBindingDraft::capture(
            &canonical,
            &staging,
            &claim,
            (2, 1),
            None,
            (Uuid::new_v4(), 1),
        )
        .unwrap();
        store
            .reserve_publication_binding_fixture(&staging, sequence, nested)
            .unwrap();
        let retained = store
            .read_publication_binding(staging.output_id())
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(retained).unwrap()["command"]["effect"]["PublishNoReplace"]
                ["target"]["binding"]["locator"],
            leaf
        );
        for bad in [
            "../out.wav",
            "/out.wav",
            "nested//out.wav",
            "nested/./out.wav",
            "nested/",
        ] {
            assert!(crate::file_operation_owner::BoundedRelativeLocator::new(bad).is_err());
            let canonical = command_bytes(&staging, owner, Uuid::new_v4(), bad);
            assert!(PublicationBindingDraft::capture(
                &canonical,
                &staging,
                &claim,
                (2, 1),
                None,
                (Uuid::new_v4(), 1)
            )
            .is_err());
        }
    }

    #[test]
    fn publication_issuance_actual_atomic_readback_restart_and_duplicate_never_reissue() {
        let (fixture, store, staging, binding, sequence) = prepared();
        let before = store.get(staging.operation_id()).expect("operation before");
        store
            .reserve_publication_binding_fixture(&staging, sequence, binding.clone())
            .expect("actual reservation");
        let original = store
            .read_publication_binding(staging.output_id())
            .expect("readback")
            .expect("binding");
        assert_eq!(
            store.get(staging.operation_id()).expect("operation after"),
            before
        );
        assert!(store
            .reserve_publication_binding_fixture(&staging, sequence, binding)
            .is_err());
        assert_eq!(
            store
                .read_publication_binding(staging.output_id())
                .expect("same original"),
            Some(original.clone())
        );
        assert!(store.prove_clean_handoff_durable().is_err());
        drop(store);
        let reopened = OperationJournalStore::open(&fixture.owner).expect("actual restart");
        assert_eq!(
            reopened
                .read_publication_binding(staging.output_id())
                .expect("persisted binding"),
            Some(original)
        );
        assert!(matches!(
            reopened.checkpoint_staging(&staging),
            Err(OperationJournalStoreError::StagingCheckpoint { reason })
                if reason == "publication grant was already reserved"
        ));
        assert_eq!(
            reopened
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap(),
            Some((sequence, staging.clone(), true))
        );
        assert!(reopened
            .reserve_publication_binding_fixture(&staging, sequence, prepared_binding(&staging))
            .is_err());
    }
    fn prepared_binding(
        staging: &staging_checkpoints::StagingCheckpointRecord,
    ) -> PublicationBindingDraft {
        let value = serde_json::to_value(staging).expect("stage value");
        let owner =
            serde_json::from_value(value["staging_root"]["owner"].clone()).expect("owner UUID");
        let claim = AdmissionClaim::new(
            AdmissionClaimId::new(),
            VolumeIdentity { device: 1 },
            FullCapacityAllocationClass::DestinationStaging,
            1,
        )
        .expect("unqualified data claim");
        PublicationBindingDraft::capture(
            &command_bytes(staging, owner, Uuid::new_v4(), "out.wav"),
            staging,
            &claim,
            (1, 1),
            None,
            (Uuid::new_v4(), 1),
        )
        .expect("data-only draft")
    }

    #[test]
    fn publication_issuance_real_fault_boundaries_keep_binding_and_reservation_atomic() {
        for boundary in [
            InsertTestBoundary::BeforePrecommitValidation,
            InsertTestBoundary::Commit,
            InsertTestBoundary::AfterCommit,
        ] {
            let (_fixture, store, staging, binding, sequence) = prepared();
            store.set_insert_test_hook(move |at| {
                if at == boundary {
                    InsertTestDirective::Fail(
                        OperationJournalStoreError::IdentityCheckUnavailable {
                            reason: "issuance boundary".into(),
                        },
                    )
                } else {
                    InsertTestDirective::Continue
                }
            });
            let result = store.reserve_publication_binding_fixture(&staging, sequence, binding);
            assert!(result.is_err());
            let reserved = store
                .read_staging_checkpoint_row(staging.output_id())
                .expect("checkpoint")
                .unwrap()
                .2;
            let retained = store
                .read_publication_binding(staging.output_id())
                .expect("binding");
            assert_eq!(reserved, boundary == InsertTestBoundary::AfterCommit);
            assert_eq!(retained.is_some(), reserved);
            if reserved {
                assert!(matches!(
                    result,
                    Err(OperationJournalStoreError::StagingOutcomeUncertain { .. })
                ));
            }
        }
        let (fixture, store, staging, binding, sequence) = prepared();
        store.set_insert_test_hook(|at| {
            if at == InsertTestBoundary::Commit {
                InsertTestDirective::ExecuteThenFail(
                    OperationJournalStoreError::IdentityCheckUnavailable {
                        reason: "lost genuine commit acknowledgement".into(),
                    },
                )
            } else {
                InsertTestDirective::Continue
            }
        });
        assert!(matches!(
            store.reserve_publication_binding_fixture(&staging, sequence, binding),
            Err(OperationJournalStoreError::StagingOutcomeUncertain { .. })
        ));
        drop(store);
        let reopened =
            OperationJournalStore::open(&fixture.owner).expect("restart uncertain commit");
        assert!(
            reopened
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .2
        );
        assert!(reopened
            .read_publication_binding(staging.output_id())
            .unwrap()
            .is_some());
    }

    #[test]
    fn publication_issuance_foreign_charge_target_and_fingerprint_roll_back_without_reserving() {
        let (_fixture, store, staging, binding, sequence) = prepared();
        let base = serde_json::to_value(&binding).expect("binding fields");
        let changes = [
            (
                "/command/operation_id",
                serde_json::json!(OperationId::new()),
            ),
            (
                "/command/participant_id",
                serde_json::json!(ParticipantId::new()),
            ),
            ("/command/effect_id", serde_json::json!(staging.effect_id())),
            ("/issuer_incarnation", serde_json::json!(Uuid::nil())),
            ("/slot_generation", serde_json::json!(0)),
            ("/staging_fingerprint", serde_json::json!(vec![0; 32])),
            ("/command_fingerprint", serde_json::json!(vec![0; 32])),
            (
                "/charge/claim_id",
                serde_json::json!(AdmissionClaimId::new()),
            ),
            ("/charge/allocated_bytes", serde_json::json!(2)),
            (
                "/command/effect/PublishNoReplace/target/binding/locator",
                serde_json::json!("foreign.wav"),
            ),
            (
                "/command/effect/PublishNoReplace/staged/bytes",
                serde_json::json!(2),
            ),
            (
                "/command/effect/PublishNoReplace/expected_absent/marker",
                serde_json::json!(0),
            ),
        ];
        for (path, value) in changes {
            let mut changed = base.clone();
            *changed.pointer_mut(path).expect("existing field") = value;
            let changed: PublicationBindingDraft =
                serde_json::from_value(changed).expect("data-only altered row");
            assert!(
                store
                    .reserve_publication_binding_fixture(&staging, sequence, changed)
                    .is_err(),
                "{path}"
            );
            assert!(
                !store
                    .read_staging_checkpoint_row(staging.output_id())
                    .unwrap()
                    .unwrap()
                    .2
            );
            assert!(store
                .read_publication_binding(staging.output_id())
                .unwrap()
                .is_none());
        }
        store
            .reserve_publication_binding_fixture(&staging, sequence, binding)
            .expect("unchanged original succeeds");
    }

    #[test]
    fn publication_issuance_schema12_migration_preserves_reserved_unknown_history_without_binding()
    {
        let (fixture, store, staging, _binding, sequence) = prepared();
        store
            .reserve_staging_grant_record(&staging, sequence)
            .expect("actual legacy reservation fixture");
        let before = store
            .read_staging_checkpoint_row(staging.output_id())
            .unwrap()
            .unwrap();
        store
            .connection
            .execute_batch(
                "DROP TABLE operation_journal_publication_issuance; PRAGMA user_version=12",
            )
            .expect("exact previous schema fixture");
        drop(store);
        let reopened =
            OperationJournalStore::open(&fixture.owner).expect("guarded previous-schema migration");
        assert_eq!(
            reopened
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap(),
            Some(before)
        );
        assert!(reopened
            .read_publication_binding(staging.output_id())
            .unwrap()
            .is_none());
        let version: i64 = reopened
            .connection
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 13);
        assert!(reopened
            .reserve_publication_binding_fixture(&staging, sequence, prepared_binding(&staging))
            .is_err());
        assert!(reopened.prove_clean_handoff_durable().is_err());
    }

    #[test]
    fn publication_issuance_strict_readback_corruption_retains_original_reservation() {
        let (_fixture, store, staging, binding, sequence) = prepared();
        store
            .reserve_publication_binding_fixture(&staging, sequence, binding)
            .unwrap();
        let retained = store
            .read_publication_binding(staging.output_id())
            .unwrap()
            .unwrap();
        let original = retained.encode().unwrap();
        let mut duplicate = b"{\"schema_version\":1,".to_vec();
        duplicate.extend_from_slice(&original[1..]);
        let mut unknown = serde_json::to_value(&retained).unwrap();
        unknown["authority"] = serde_json::json!("never reconstructed");
        let corruptions = [
            Vec::new(),
            b"{}".to_vec(),
            duplicate,
            serde_json::to_vec(&unknown).unwrap(),
            serde_json::to_vec_pretty(&retained).unwrap(),
            vec![b' '; super::super::publication_bindings::MAX_RECORD_BYTES + 1],
        ];
        store
            .connection
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        for bytes in corruptions {
            store.connection.execute("UPDATE operation_journal_publication_issuance SET record=?1 WHERE output_id=?2",
                params![bytes, staging.output_id().to_string()]).unwrap();
            assert!(store.read_publication_binding(staging.output_id()).is_err());
            assert!(
                store
                    .read_staging_checkpoint_row(staging.output_id())
                    .unwrap()
                    .unwrap()
                    .2
            );
        }
        store
            .connection
            .execute(
                "UPDATE operation_journal_publication_issuance SET record=?1",
                params![original],
            )
            .unwrap();
        assert_eq!(
            store.read_publication_binding(staging.output_id()).unwrap(),
            Some(retained)
        );
        store
            .connection
            .execute(
                "UPDATE operation_journal_publication_issuance SET publication_effect_id=?1",
                params![Uuid::new_v4().to_string()],
            )
            .unwrap();
        assert!(store.read_publication_binding(staging.output_id()).is_err());
        assert!(
            store
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .2
        );
    }

    #[test]
    fn publication_issuance_selected_chunk_requires_exact_issued_row() {
        let (_fixture, store, staging, binding, sequence) = prepared();
        let mut value = serde_json::to_value(&binding).unwrap();
        let chunk = Uuid::new_v4();
        let issue = Uuid::new_v4();
        value["chunk"] = serde_json::json!({"chunk_id":chunk,"issue_id":issue});
        let selected: PublicationBindingDraft = serde_json::from_value(value.clone()).unwrap();
        assert!(store
            .reserve_publication_binding_fixture(&staging, sequence, selected.clone())
            .is_err());
        // This is a durable row observation, not a Native grant or allocation fixture.
        store.connection.execute("INSERT INTO operation_journal_chunk_claim
            (chunk_id,operation_id,claim_id,participant_id,output_key,logical_offset,logical_length,
             volume_device,allocation_class,allocated_bytes,allocation_unit,protected_free_bytes,state,issue_id,release_id)
            VALUES (?1,?2,?3,?4,'staged.wav',0,1,'1','DestinationStaging',1,1,1,'Claimed',NULL,NULL)",
            params![chunk.to_string(), staging.operation_id().to_string(),
                value["charge"]["claim_id"].as_str().unwrap(), staging.participant_id().to_string()]).unwrap();
        assert!(store
            .reserve_publication_binding_fixture(&staging, sequence, selected.clone())
            .is_err());
        store
            .connection
            .execute(
                "UPDATE operation_journal_chunk_claim SET state='Issued',issue_id=?1",
                params![Uuid::new_v4().to_string()],
            )
            .unwrap();
        assert!(store
            .reserve_publication_binding_fixture(&staging, sequence, selected.clone())
            .is_err());
        assert!(
            !store
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .2
        );
        store
            .connection
            .execute(
                "UPDATE operation_journal_chunk_claim SET issue_id=?1",
                params![issue.to_string()],
            )
            .unwrap();
        store
            .reserve_publication_binding_fixture(&staging, sequence, selected)
            .unwrap();
        assert!(store
            .read_publication_binding(staging.output_id())
            .unwrap()
            .is_some());
        store
            .connection
            .execute(
                "UPDATE operation_journal_chunk_claim SET participant_id=?1",
                params![ParticipantId::new().to_string()],
            )
            .unwrap();
        assert!(store.read_publication_binding(staging.output_id()).is_err());
        assert!(
            store
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .2
        );
    }

    #[test]
    fn publication_issuance_bounded_history_and_command_refuse_without_consuming_slot() {
        let (_fixture, store, staging, binding, sequence) = prepared();
        let value = serde_json::to_value(&binding).unwrap();
        let claim: AdmissionClaim = serde_json::from_value(value["charge"].clone()).unwrap();
        assert!(PublicationBindingDraft::capture(
            &vec![b' '; 12 * 1024 + 1],
            &staging,
            &claim,
            (1, 1),
            None,
            (Uuid::new_v4(), 1)
        )
        .is_err());
        let bytes = binding.encode().unwrap();
        store.connection.execute("WITH RECURSIVE rows(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM rows WHERE n<8192)
            INSERT INTO operation_journal_publication_issuance(output_id,publication_effect_id,record)
            SELECT printf('00000000-0000-4000-8000-%012d',n),printf('10000000-0000-4000-8000-%012d',n),?1 FROM rows",
            params![bytes]).unwrap();
        assert!(store
            .reserve_publication_binding_fixture(&staging, sequence, binding)
            .is_err());
        assert!(
            !store
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .2
        );
        assert!(store
            .read_publication_binding(staging.output_id())
            .unwrap()
            .is_none());
    }
    #[test]
    fn publication_issuance_effect_collision_rolls_back_original_boolean() {
        let (_fixture, store, staging, binding, sequence) = prepared();
        store.connection.execute("INSERT INTO operation_journal_publication_issuance(output_id,publication_effect_id,record) VALUES (?1,?2,?3)",
            params![Uuid::new_v4().to_string(),binding.publication_effect().to_string(),binding.encode().unwrap()]).unwrap();
        assert!(store
            .reserve_publication_binding_fixture(&staging, sequence, binding)
            .is_err());
        assert!(
            !store
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .2
        );
        assert!(store
            .read_publication_binding(staging.output_id())
            .unwrap()
            .is_none());
        let count: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM operation_journal_publication_issuance",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
    #[test]
    fn publication_issuance_schema12_unexpected_namespace_refuses_without_partial_migration() {
        let (fixture, store, staging, _binding, _sequence) = prepared();
        store.connection.execute_batch("DROP TABLE operation_journal_publication_issuance; PRAGMA user_version=12; CREATE TABLE unknown_namespace(x INTEGER)").unwrap();
        drop(store);
        assert!(OperationJournalStore::open(&fixture.owner).is_err());
        let raw = Connection::open(database_path(&fixture.owner)).unwrap();
        let version: i64 = raw
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 12);
        let created:i64 = raw.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name='operation_journal_publication_issuance'",[],|row|row.get(0)).unwrap();
        assert_eq!(created, 0);
        let reserved:bool = raw.query_row("SELECT grant_reserved FROM operation_journal_staging_checkpoints WHERE output_id=?1",params![staging.output_id().to_string()],|row|row.get(0)).unwrap();
        assert!(!reserved);
    }
    #[test]
    fn publication_issuance_staging_readback_borrows_before_bounded_typed_decode() {
        let (_fixture, store, staging, binding, sequence) = prepared();
        store
            .reserve_publication_binding_fixture(&staging, sequence, binding)
            .unwrap();
        let original = staging.encode().unwrap();
        let mut duplicate = b"{\"schema_version\":1,".to_vec();
        duplicate.extend_from_slice(&original[1..]);
        store
            .connection
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        for corrupt in [
            vec![b' '; 64 * 1024 + 1],
            duplicate,
            b"{\"unknown\":[]}".to_vec(),
        ] {
            store
                .connection
                .execute(
                    "UPDATE operation_journal_staging_checkpoints SET record=?1 WHERE output_id=?2",
                    params![corrupt, staging.output_id().to_string()],
                )
                .unwrap();
            assert!(store
                .read_staging_checkpoint_row(staging.output_id())
                .is_err());
            assert!(store.read_publication_binding(staging.output_id()).is_err());
            let reserved:bool=store.connection.query_row("SELECT grant_reserved FROM operation_journal_staging_checkpoints WHERE output_id=?1",
                params![staging.output_id().to_string()],|row|row.get(0)).unwrap();
            assert!(reserved);
        }
        store
            .connection
            .execute(
                "UPDATE operation_journal_staging_checkpoints SET record=?1 WHERE output_id=?2",
                params![original, staging.output_id().to_string()],
            )
            .unwrap();
        assert!(store
            .read_publication_binding(staging.output_id())
            .unwrap()
            .is_some());
    }

    #[test]
    fn publication_issuance_staging_maximum_valid_text_fits_canonical_temporary_bound() {
        let (_fixture, _store, staging, _binding, _sequence) = prepared();
        let mut value = serde_json::to_value(&staging).unwrap();
        value["locator"] = serde_json::json!("\u{1}".repeat(1024));
        value["physical_object_id"] = serde_json::json!("\u{1}".repeat(256));
        value["source_root"] = value["staging_root"].clone();
        value["source_fence"] = serde_json::json!({"source_id":"\u{1}".repeat(256),"root_identity":"\u{1}".repeat(256),
            "database_identity":"\u{1}".repeat(256),"lifecycle_generation":u64::MAX,"source_revision":u64::MAX,"lease_epoch":u64::MAX});
        let record = staging_checkpoints::StagingCheckpointRecord::decode(
            &serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        let encoded = record.encode().unwrap();
        assert!(encoded.len() <= 16 * 1024);
        assert_eq!(
            staging_checkpoints::StagingCheckpointRecord::decode(&encoded).unwrap(),
            record
        );
    }
    #[test]
    fn publication_issuance_staging_preserves_historical_whitespace_and_field_order() {
        let (_fixture, store, staging, _binding, _sequence) = prepared();
        let reordered = serde_json::to_vec(&serde_json::to_value(&staging).unwrap()).unwrap();
        let mut historical = vec![b' '; 64 * 1024 - reordered.len()];
        historical.extend_from_slice(&reordered);
        assert_eq!(historical.len(), 64 * 1024);
        assert_eq!(
            staging_checkpoints::StagingCheckpointRecord::decode(&historical).unwrap(),
            staging
        );
        store
            .connection
            .execute(
                "UPDATE operation_journal_staging_checkpoints SET record=?1 WHERE output_id=?2",
                params![historical, staging.output_id().to_string()],
            )
            .unwrap();
        assert_eq!(
            store
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .1,
            staging
        );
    }
    #[test]
    fn publication_issuance_string_scan_preserves_valid_unicode_and_refuses_oversize_diagnostics() {
        let (_fixture, _store, staging, _binding, _sequence) = prepared();
        let encoded = String::from_utf8(staging.encode().unwrap()).unwrap();
        let surrogate = encoded.replace("staged.wav", r"\ud83d\ude00");
        let parsed =
            staging_checkpoints::StagingCheckpointRecord::decode(surrogate.as_bytes()).unwrap();
        assert_eq!(parsed.locator(), "😀");
        for replacement in [r"\ud83d", r"\ude00", r"\uXXXX"] {
            let bad = encoded.replace("staged.wav", replacement);
            assert!(staging_checkpoints::StagingCheckpointRecord::decode(bad.as_bytes()).is_err());
        }
        let oversized = serde_json::to_vec(&"\u{200d}".repeat(1025)).unwrap();
        assert!(staging_checkpoints::bounded_string_tokens(&oversized).is_err());
        let escaped = serde_json::to_vec(&"\u{1}".repeat(1024)).unwrap();
        staging_checkpoints::bounded_string_tokens(&escaped).unwrap();
    }
}
