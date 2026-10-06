mod publication_issuance {
    use super::super::publication_bindings::PublicationBindingDraft;
    use super::*;
    use crate::operation_journal::ParticipantId;

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
            serde_json::to_value(retained).unwrap()["command"]["effect"]["PublishNoReplace"]["target"]
                ["binding"]["locator"],
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
            assert!(
                PublicationBindingDraft::capture(
                    &canonical,
                    &staging,
                    &claim,
                    (2, 1),
                    None,
                    (Uuid::new_v4(), 1)
                )
                .is_err()
            );
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
        assert!(
            store
                .reserve_publication_binding_fixture(&staging, sequence, binding)
                .is_err()
        );
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
        assert!(
            reopened
                .reserve_publication_binding_fixture(&staging, sequence, prepared_binding(&staging))
                .is_err()
        );
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
        assert!(
            reopened
                .read_publication_binding(staging.output_id())
                .unwrap()
                .is_some()
        );
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
            assert!(
                store
                    .read_publication_binding(staging.output_id())
                    .unwrap()
                    .is_none()
            );
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
        assert!(
            reopened
                .read_publication_binding(staging.output_id())
                .unwrap()
                .is_none()
        );
        let version: i64 = reopened
            .connection
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 13);
        assert!(
            reopened
                .reserve_publication_binding_fixture(&staging, sequence, prepared_binding(&staging))
                .is_err()
        );
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
        assert!(
            store
                .reserve_publication_binding_fixture(&staging, sequence, selected.clone())
                .is_err()
        );
        // This is a durable row observation, not a Native grant or allocation fixture.
        store.connection.execute("INSERT INTO operation_journal_chunk_claim
            (chunk_id,operation_id,claim_id,participant_id,output_key,logical_offset,logical_length,
             volume_device,allocation_class,allocated_bytes,allocation_unit,protected_free_bytes,state,issue_id,release_id)
            VALUES (?1,?2,?3,?4,'staged.wav',0,1,'1','DestinationStaging',1,1,1,'Claimed',NULL,NULL)",
            params![chunk.to_string(), staging.operation_id().to_string(),
                value["charge"]["claim_id"].as_str().unwrap(), staging.participant_id().to_string()]).unwrap();
        assert!(
            store
                .reserve_publication_binding_fixture(&staging, sequence, selected.clone())
                .is_err()
        );
        store
            .connection
            .execute(
                "UPDATE operation_journal_chunk_claim SET state='Issued',issue_id=?1",
                params![Uuid::new_v4().to_string()],
            )
            .unwrap();
        assert!(
            store
                .reserve_publication_binding_fixture(&staging, sequence, selected.clone())
                .is_err()
        );
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
        assert!(
            store
                .read_publication_binding(staging.output_id())
                .unwrap()
                .is_some()
        );
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
        assert!(
            PublicationBindingDraft::capture(
                &vec![b' '; 12 * 1024 + 1],
                &staging,
                &claim,
                (1, 1),
                None,
                (Uuid::new_v4(), 1)
            )
            .is_err()
        );
        let bytes = binding.encode().unwrap();
        store.connection.execute("WITH RECURSIVE rows(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM rows WHERE n<8192)
            INSERT INTO operation_journal_publication_issuance(output_id,publication_effect_id,record)
            SELECT printf('00000000-0000-4000-8000-%012d',n),printf('10000000-0000-4000-8000-%012d',n),?1 FROM rows",
            params![bytes]).unwrap();
        assert!(
            store
                .reserve_publication_binding_fixture(&staging, sequence, binding)
                .is_err()
        );
        assert!(
            !store
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .2
        );
        assert!(
            store
                .read_publication_binding(staging.output_id())
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn publication_issuance_effect_collision_rolls_back_original_boolean() {
        let (_fixture, store, staging, binding, sequence) = prepared();
        store.connection.execute("INSERT INTO operation_journal_publication_issuance(output_id,publication_effect_id,record) VALUES (?1,?2,?3)",
            params![Uuid::new_v4().to_string(),binding.publication_effect().to_string(),binding.encode().unwrap()]).unwrap();
        assert!(
            store
                .reserve_publication_binding_fixture(&staging, sequence, binding)
                .is_err()
        );
        assert!(
            !store
                .read_staging_checkpoint_row(staging.output_id())
                .unwrap()
                .unwrap()
                .2
        );
        assert!(
            store
                .read_publication_binding(staging.output_id())
                .unwrap()
                .is_none()
        );
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
            assert!(
                store
                    .read_staging_checkpoint_row(staging.output_id())
                    .is_err()
            );
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
        assert!(
            store
                .read_publication_binding(staging.output_id())
                .unwrap()
                .is_some()
        );
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
