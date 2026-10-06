// Real profile-owned SQLite tests. Capacity fixtures prove logical settlement only; no test
// invents a production reservation, sealed all-owner latch or physical allocation guarantee.
mod exact_database_claim_persistence {
    include!("database_attempt_metadata_tests.rs");
    include!("database_attempts_tests.rs");
    include!("source_lease_binding_tests.rs");
    include!("recording_prestart_database_claims_tests.rs");
    use super::*;
    use crate::operation_journal::database_claims::{
        ClaimDirection, DatabaseBinding, DatabaseClaimInventory, DatabaseClaimSlice,
        DatabaseParticipant, DatabaseRole, InventoryHeader, RevisionDomain, SourceBinding,
        command_digest,
    };

    fn recording_database_inputs(
        owner: &WritableProfileGuard,
    ) -> (
        crate::operation_journal::recording_commands::RecordingProposal,
        StoredOperationRecord,
        AdmissionClaimInventory,
        DatabaseClaimInventory,
    ) {
        use crate::operation_journal::recording_commands::recording_proposal_for_test;
        let proposal = recording_proposal_for_test(2, 100);
        let mut record = finite_recording_record(&proposal);
        let mut allocations = record
            .full_capacity_plan
            .as_ref()
            .unwrap()
            .allocations()
            .to_vec();
        for (class, bytes) in [
            (FullCapacityAllocationClass::SourceDatabaseCommit, 8192),
            (FullCapacityAllocationClass::SourceWalShm, 16384),
            (FullCapacityAllocationClass::GlobalDatabaseCommit, 8192),
            (FullCapacityAllocationClass::GlobalWalShm, 16384),
        ] {
            allocations.push(
                crate::operation_journal::full_capacity::FullCapacityAllocation::new(
                    VolumeIdentity { device: 1 },
                    1,
                    class,
                    bytes,
                    bytes,
                    PROTECTED_FREE_SPACE_FLOOR,
                )
                .unwrap(),
            );
        }
        record.full_capacity_plan = Some(FullCapacityPlan::new(allocations).unwrap());
        let claims = AdmissionClaimInventory::new(
            record
                .full_capacity_plan
                .as_ref()
                .unwrap()
                .allocations()
                .iter()
                .map(|a| {
                    AdmissionClaim::new(
                        AdmissionClaimId::new(),
                        a.volume().clone(),
                        a.class(),
                        a.allocated_bytes(),
                    )
                    .unwrap()
                })
                .collect(),
        )
        .unwrap();
        let digest = command_digest(&record, 1).unwrap();
        let descriptors = proposal.participants().descriptors();
        let source = descriptors
            .iter()
            .find(|d| d.kind() == ParticipantKind::SourceDatabase)
            .unwrap()
            .source_fence()
            .unwrap();
        let bindings = [DatabaseRole::Source, DatabaseRole::Global]
            .into_iter()
            .map(|role| DatabaseBinding {
                id: Uuid::new_v4(),
                role,
                volume: 1,
                allocation_unit: 1,
                main_file: if role == DatabaseRole::Source {
                    source.database_identity().into()
                } else {
                    "global-database".into()
                },
                registration: Uuid::new_v4(),
                owner_generation: 1,
                lifecycle: if role == DatabaseRole::Source {
                    source.lifecycle_generation()
                } else {
                    1
                },
                database_root: "database-root".into(),
                root_binding: "root-binding".into(),
                source: (role == DatabaseRole::Source).then(|| SourceBinding {
                    source_id: source.source_id().as_str().into(),
                    media_root: source.root_identity().into(),
                    lease_owner: "owner".into(),
                    lease_epoch: source.lease_epoch(),
                    lease_fence: None,
                }),
                current_revision: if role == DatabaseRole::Source {
                    source.source_revision()
                } else {
                    1
                },
                participants: descriptors
                    .iter()
                    .filter(|d| match role {
                        DatabaseRole::Source => matches!(
                            d.kind(),
                            ParticipantKind::SourceDatabase | ParticipantKind::Readiness
                        ),
                        DatabaseRole::Global => matches!(
                            d.kind(),
                            ParticipantKind::GlobalLibrary | ParticipantKind::Harvest
                        ),
                    })
                    .map(|d| DatabaseParticipant {
                        id: d.id().as_uuid(),
                        requirement: 1,
                        revision_domain: if role == DatabaseRole::Source {
                            RevisionDomain::SourceAuthoritative
                        } else {
                            RevisionDomain::GlobalMutationGeneration
                        },
                        command_digest: digest,
                        before: if role == DatabaseRole::Source {
                            source.source_revision()
                        } else {
                            1
                        },
                        after: if role == DatabaseRole::Source {
                            source.source_revision().checked_add(1).unwrap()
                        } else {
                            2
                        },
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        let mut slices = Vec::new();
        for b in &bindings {
            let classes = match b.role {
                DatabaseRole::Source => [
                    FullCapacityAllocationClass::SourceDatabaseCommit,
                    FullCapacityAllocationClass::SourceWalShm,
                ],
                DatabaseRole::Global => [
                    FullCapacityAllocationClass::GlobalDatabaseCommit,
                    FullCapacityAllocationClass::GlobalWalShm,
                ],
            };
            for p in &b.participants {
                for class in classes {
                    let sidecar = matches!(
                        class,
                        FullCapacityAllocationClass::SourceWalShm
                            | FullCapacityAllocationClass::GlobalWalShm
                    );
                    let claim = claims
                        .claims()
                        .iter()
                        .find(|c| c.allocation_class() == class)
                        .unwrap();
                    slices.push(DatabaseClaimSlice {
                        id: Uuid::new_v4(),
                        claim: claim.claim_id().as_uuid(),
                        binding: b.id,
                        participant: p.id,
                        direction: ClaimDirection::RequiredForward,
                        class,
                        ceiling: if sidecar { 8192 } else { 4096 },
                        wal_ceiling: if sidecar { 4096 } else { 0 },
                        shm_ceiling: if sidecar { 4096 } else { 0 },
                    });
                }
            }
        }
        let inventory = DatabaseClaimInventory::new(
            InventoryHeader {
                profile: owner.profile_root_identity().into(),
                acquisition: owner.owner_identity().into(),
                ownership_generation: owner.ownership_generation(),
                decision_nonce: Uuid::new_v4(),
            },
            bindings,
            slices,
        )
        .unwrap();
        (proposal, record, claims, inventory)
    }

    // Integration of both production transaction hooks; capacity/bindings remain explicit
    // synthetic observations, not live all-owner reservations or physical permission.
    #[test]
    fn recording_controls_and_database_inventory_share_one_atomic_initial_settlement() {
        use crate::operation_journal::recording_commands::controls::{
            ControlRole, RecordingControlReservation,
        };
        for fault in 0..5 {
            let f = fixture();
            let store = OperationJournalStore::open(&f.owner).unwrap();
            let (proposal, record, claims, inventory) = recording_database_inputs(&f.owner);
            let provider = logical_provider(&record);
            match fault {
                1 => store.set_insert_test_hook(|at| {
                    if at == InsertTestBoundary::BeforePrecommitValidation {
                        InsertTestDirective::Fail(database_claims::error("precommit fault"))
                    } else {
                        InsertTestDirective::Continue
                    }
                }),
                2 => store.set_insert_test_hook(|at| {
                    if at == InsertTestBoundary::Commit {
                        InsertTestDirective::ExecuteThenFail(database_claims::error(
                            "lost commit reply",
                        ))
                    } else {
                        InsertTestDirective::Continue
                    }
                }),
                3 => store.set_insert_test_hook(|at| {
                    if at == InsertTestBoundary::AfterCommit {
                        InsertTestDirective::Fail(database_claims::error("lost readback reply"))
                    } else {
                        InsertTestDirective::Continue
                    }
                }),
                4 => {
                    *store.recording_readback_corruption.lock().unwrap() = Some(
                        RecordingControlReservation::new(&proposal, ControlRole::Frozen)
                            .unwrap()
                            .id(),
                    )
                }
                _ => {}
            }
            let settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
            let committed = fault != 1;
            assert_eq!(
                rows(&store, database_claims::BINDING_TABLE),
                if committed { 2 } else { 0 }
            );
            assert_eq!(
                rows(&store, database_claims::SLICE_TABLE),
                if committed { 8 } else { 0 }
            );
            assert_eq!(
                rows(&store, "operation_journal_ordinary_claim"),
                if committed { 6 } else { 0 }
            );
            for claim in claims.claims() {
                assert_eq!(
                    provider.outstanding(claim.volume(), claim.allocation_class()),
                    if committed {
                        claim.allocated_bytes()
                    } else {
                        0
                    }
                );
            }
            assert_eq!(
                store.recording_retained_totals().unwrap(),
                if committed { (4, 16384) } else { (0, 0) }
            );
            assert_eq!(settled.lease().is_some(), committed);
            assert_eq!(settled.outcome().receipt().is_some(), fault == 0);
            if fault != 0 {
                assert_eq!(
                    settled.outcome().failure().unwrap().stage(),
                    match fault {
                        1 => JournalInsertWriteStage::RolledBackBeforeCommit,
                        2 => JournalInsertWriteStage::CommitOutcomeUnknown,
                        _ => JournalInsertWriteStage::CommittedButNotAcknowledged,
                    }
                );
            }
            if fault == 1 {
                assert!(store.get(record.operation_id).unwrap().is_none());
            } else if fault == 4 {
                assert!(
                    store
                        .read_database_claim_inventory_for_nonce(
                            inventory.header().decision_nonce,
                            inventory.digest().unwrap()
                        )
                        .is_err()
                );
            } else {
                assert_eq!(
                    store
                        .read_database_claim_inventory_for_nonce(
                            inventory.header().decision_nonce,
                            inventory.digest().unwrap()
                        )
                        .unwrap(),
                    Some((record.operation_id, inventory.clone()))
                );
            }
            drop(settled);
            drop(store);
            let store = OperationJournalStore::open(&f.owner).unwrap();
            assert_eq!(store.get(record.operation_id).unwrap().is_some(), committed);
            if fault == 4 {
                assert!(
                    store
                        .read_database_claim_inventory(record.operation_id)
                        .is_err()
                );
                assert!(store.prove_clean_handoff_durable().is_err());
            }
        }
    }

    #[test]
    fn nonce_readback_rejects_corrupted_original_aggregate_claims() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, claims, inventory) = input(&f.owner, 1);
        let provider = logical_provider(&record);
        let settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
        assert!(settled.outcome().receipt().is_some());
        store.connection.execute(
            "UPDATE operation_journal_ordinary_claim SET allocated_bytes=allocated_bytes+4096 WHERE operation_id=?1 AND allocation_class='SourceWalShm'",
            [record.operation_id.to_string()],
        ).unwrap();
        assert!(
            store
                .read_database_claim_inventory_for_nonce(
                    inventory.header().decision_nonce,
                    inventory.digest().unwrap(),
                )
                .is_err()
        );
        assert!(
            store
                .read_database_claim_inventory(record.operation_id)
                .is_err()
        );
        assert!(store.get(record.operation_id).unwrap().is_some());
        assert!(settled.lease().is_some());
        assert_eq!(
            rows(&store, "operation_journal_database_binding"),
            inventory.bindings().len() as i64
        );
    }

    #[test]
    fn lost_acceptance_nonce_names_one_original_operation_and_rejects_rebinding() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, claims, inventory) = input(&f.owner, 1);
        let provider = logical_provider(&record);
        let nonce = inventory.header().decision_nonce;
        let digest = inventory.digest().unwrap();
        assert!(
            store
                .read_database_claim_inventory_for_nonce(nonce, digest)
                .unwrap()
                .is_none()
        );
        let settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
        assert!(settled.outcome().receipt().is_some());
        assert_eq!(
            store
                .read_database_claim_inventory_for_nonce(nonce, digest)
                .unwrap(),
            Some((record.operation_id, inventory.clone()))
        );
        assert!(
            store
                .read_database_claim_inventory_for_nonce(nonce, [9; 32])
                .is_err()
        );
        let (other, other_claims, other_inventory) = input(&f.owner, 1);
        let mut header = other_inventory.header().clone();
        header.decision_nonce = nonce;
        let conflicting = DatabaseClaimInventory::new(
            header,
            other_inventory.bindings().to_vec(),
            other_inventory.slices().to_vec(),
        )
        .unwrap();
        let second = settle_fixture(&store, &other, &other_claims, &conflicting, &provider);
        assert_eq!(
            second.outcome().failure().unwrap().stage(),
            JournalInsertWriteStage::RolledBackBeforeCommit
        );
        assert!(second.lease().is_none());
        assert!(store.get(other.operation_id).unwrap().is_none());
        assert_eq!(
            store
                .read_database_claim_inventory_for_nonce(nonce, digest)
                .unwrap(),
            Some((record.operation_id, inventory))
        );
        drop(settled);
        drop(second);
        drop(store);
        let store = OperationJournalStore::open(&f.owner).unwrap();
        assert!(
            store
                .read_database_claim_inventory_for_nonce(nonce, digest)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn nonce_readback_refuses_missing_root_with_retained_original_binding_evidence() {
        for fault in 0..3 {
            let f = fixture();
            let store = OperationJournalStore::open(&f.owner).unwrap();
            let (record, claims, inventory) = input(&f.owner, 2);
            let provider = logical_provider(&record);
            let settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
            assert!(settled.outcome().receipt().is_some());
            let mut nonce = inventory.header().decision_nonce;
            let digest = inventory.digest().unwrap();
            let root = inventory.bindings()[0].id.to_string();
            match fault {
                0 => {
                    store.connection.execute("DELETE FROM operation_journal_database_claim_slice WHERE binding_id=?1", [&root]).unwrap();
                    store
                        .connection
                        .execute(
                            "DELETE FROM operation_journal_database_binding WHERE binding_id=?1",
                            [&root],
                        )
                        .unwrap();
                }
                1 => {
                    store.connection.execute("UPDATE operation_journal_database_binding SET ordinal=63,inventory=NULL WHERE binding_id=?1", [&root]).unwrap();
                }
                _ => {
                    nonce = Uuid::new_v4();
                    store.connection.execute("UPDATE operation_journal_database_binding SET decision_nonce=?1 WHERE ordinal=1", [nonce.to_string()]).unwrap();
                }
            }
            // Corrupt durable facts do not remove ordinary charges or complete settlement.
            let retained: u64 = store
                .connection
                .query_row(
                    "SELECT count(*) FROM operation_journal_ordinary_claim WHERE operation_id=?1",
                    [record.operation_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(retained as usize, claims.claims().len());
            assert!(
                store
                    .read_database_claim_inventory_for_nonce(nonce, digest)
                    .is_err(),
                "fault={fault}"
            );
            drop(settled);
            drop(store);
            let reopened = OperationJournalStore::open(&f.owner).unwrap();
            assert!(
                reopened
                    .read_database_claim_inventory_for_nonce(nonce, digest)
                    .is_err(),
                "reopen fault={fault}"
            );
        }
    }

    #[test]
    fn unresolved_rollback_cannot_supply_durable_inventory_readback_or_refund() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, claims, inventory) = input(&f.owner, 1);
        let provider = logical_provider(&record);
        store.set_insert_test_hook(|boundary| match boundary {
            InsertTestBoundary::BeforePrecommitValidation => {
                InsertTestDirective::FailAndLeaveForRollback(database_claims::error(
                    "precommit failure",
                ))
            }
            InsertTestBoundary::Rollback => {
                InsertTestDirective::Fail(database_claims::error("rollback unavailable"))
            }
            _ => InsertTestDirective::Continue,
        });
        let settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
        assert_eq!(
            settled.outcome().failure().unwrap().stage(),
            JournalInsertWriteStage::RollbackNotConfirmed
        );
        assert!(!store.connection.is_autocommit());
        assert!(settled.lease().is_some());
        assert!(
            store
                .read_database_claim_inventory(record.operation_id)
                .is_err()
        );
        store.connection.execute_batch("ROLLBACK").unwrap();
        assert!(
            store
                .read_database_claim_inventory(record.operation_id)
                .unwrap()
                .is_none()
        );
        drop(settled);
        for c in claims.claims() {
            assert_eq!(
                provider.outstanding(c.volume(), c.allocation_class()),
                c.allocated_bytes()
            );
        }
    }

    #[test]
    fn each_global_relation_ceiling_rolls_back_new_intent_without_eviction() {
        for table in [database_claims::BINDING_TABLE, database_claims::SLICE_TABLE] {
            let f = fixture();
            let store = OperationJournalStore::open(&f.owner).unwrap();
            let (record, claims, inventory) = input(&f.owner, 1);
            let provider = logical_provider(&record);
            // Direct corruption/exhaustion fixture: these preexisting rows are retained evidence,
            // not valid owner authority. The insert must reject the ceiling before extending it.
            store
                .connection
                .execute_batch("PRAGMA foreign_keys=OFF; BEGIN")
                .unwrap();
            let encoded = inventory.encode().unwrap();
            let digest = inventory.digest().unwrap();
            for i in 0..crate::operation_journal::database_claims::MAX_RELATION_ROWS {
                let id = Uuid::from_u128(i as u128 + 100_000).to_string();
                if table == database_claims::BINDING_TABLE {
                    store.connection.execute("INSERT INTO operation_journal_database_binding(binding_id,operation_id,ordinal,role,volume,main_file,decision_nonce,inventory_digest,inventory) VALUES(?1,?1,0,1,'1','retained',?2,?3,?4)",
                        params![id,id,digest.as_slice(),encoded.as_slice()]).unwrap();
                } else {
                    store.connection.execute("INSERT INTO operation_journal_database_claim_slice VALUES(?1,?1,?1,?1,?1,0,1,1,0,0)",[id]).unwrap();
                }
            }
            store
                .connection
                .execute_batch("COMMIT; PRAGMA foreign_keys=ON")
                .unwrap();
            let before = rows(&store, table);
            let settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
            assert_eq!(
                settled.outcome().failure().unwrap().stage(),
                JournalInsertWriteStage::RolledBackBeforeCommit
            );
            assert!(settled.lease().is_none());
            assert_eq!(rows(&store, table), before);
            assert!(store.get(record.operation_id).unwrap().is_none());
            for c in claims.claims() {
                assert_eq!(provider.outstanding(c.volume(), c.allocation_class()), 0);
            }
        }
    }

    #[test]
    fn live_persistence_rejects_uncovered_descriptor_peak_without_rows_or_pinned_lease() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, claims, inventory) = input(&f.owner, 1);
        let provider = logical_provider(&record);
        let plan = full_capacity_plan_for_record(&record).unwrap();
        let lease = provider.admit(&plan, &claims).unwrap();
        // Journal budget is bound to synthetic volume1, never the actual retained descriptor.
        // If process extension evidence is already tainted, the earlier runtime guard rejects.
        assert!(
            store
                .settle_initial_database_claims_with_lease(
                    &record, &claims, &inventory, &provider, lease
                )
                .is_err()
        );
        assert!(store.get(record.operation_id).unwrap().is_none());
        assert_eq!(rows(&store, database_claims::BINDING_TABLE), 0);
        for c in claims.claims() {
            assert_eq!(provider.outstanding(c.volume(), c.allocation_class()), 0);
        }
    }

    fn input(
        owner: &WritableProfileGuard,
        count: usize,
    ) -> (
        StoredOperationRecord,
        AdmissionClaimInventory,
        DatabaseClaimInventory,
    ) {
        let (id, intent, universal, args, mut plan) = v5_copy_inputs();
        let mut descriptors = universal.participants().descriptors().to_vec();
        let mut checkpoints = universal.participants().checkpoints().to_vec();
        let source = descriptors
            .iter()
            .find(|d| d.kind() == ParticipantKind::SourceDatabase)
            .unwrap()
            .id();
        let args = if count == 2 {
            let dest = crate::operation_journal::ParticipantId::new();
            descriptors.push(
                ParticipantDescriptor::new(
                    dest,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(
                        StoredSourceFence::new(
                            SourceId::from_string("source-b"),
                            "root-b",
                            "database-b",
                            1,
                            3,
                            1,
                        )
                        .unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            );
            checkpoints.push(
                ParticipantCheckpoint::new(
                    dest,
                    crate::operation_journal::ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .unwrap(),
            );
            CopyIntentArgs::new(
                args.source().clone(),
                CopyEndpoint::new(
                    dest,
                    SourceId::from_string("source-b"),
                    StoredRelativePath::new("out.wav").unwrap(),
                )
                .unwrap(),
                args.expected_source().clone(),
                CopyCollisionPolicy::NoReplace,
                Some("metadata/entry".into()),
            )
            .unwrap()
        } else {
            args
        };
        let participants = ParticipantSet::new(descriptors, checkpoints).unwrap();
        let universal = UniversalIntent::new(
            None,
            UniversalCommandKind::Copy,
            "operation.status.copy",
            participants,
        )
        .unwrap();
        for (class, bytes) in [
            (
                CapacityAllocationClass::SourceDatabaseCommit,
                4096 * count as u64,
            ),
            (CapacityAllocationClass::SourceWalShm, 8192 * count as u64),
            (
                CapacityAllocationClass::JournalRecord,
                crate::operation_journal::recording_commands::RECORDING_JOURNAL_PEAK_BYTES,
            ),
        ] {
            plan.volumes.push(DurableVolumeCapacity {
                identity: VolumeIdentity { device: 1 },
                allocation_unit: 1,
                allocation_class: class,
                logical_bytes: bytes,
                allocated_bytes: bytes,
                protected_free_bytes: PROTECTED_FREE_SPACE_FLOOR,
            });
        }
        let plan = FullCapacityPlan::from_legacy(&plan).unwrap();
        let record = StoredOperationRecord::new_v16_command(
            id,
            intent,
            universal,
            UniversalCommandArgs::Copy(args),
            plan,
            1,
        );
        let claims = AdmissionClaimInventory::new(
            record
                .full_capacity_plan
                .as_ref()
                .unwrap()
                .allocations()
                .iter()
                .map(|a| {
                    AdmissionClaim::new(
                        AdmissionClaimId::new(),
                        a.volume().clone(),
                        a.class(),
                        a.allocated_bytes(),
                    )
                    .unwrap()
                })
                .collect(),
        )
        .unwrap();
        let digest = command_digest(&record, 1).unwrap();
        let bindings = record
            .universal_intent
            .as_ref()
            .unwrap()
            .participants()
            .descriptors()
            .iter()
            .filter(|d| d.kind() == ParticipantKind::SourceDatabase)
            .map(|d| {
                let f = d.source_fence().unwrap();
                DatabaseBinding {
                    id: Uuid::new_v4(),
                    role: DatabaseRole::Source,
                    volume: 1,
                    allocation_unit: 1,
                    main_file: f.database_identity().into(),
                    registration: Uuid::new_v4(),
                    owner_generation: 1,
                    lifecycle: f.lifecycle_generation(),
                    database_root: "database-root".into(),
                    root_binding: "root-binding".into(),
                    source: Some(SourceBinding {
                        source_id: f.source_id().as_str().into(),
                        media_root: f.root_identity().into(),
                        lease_owner: "owner".into(),
                        lease_epoch: f.lease_epoch(),
                        lease_fence: None,
                    }),
                    current_revision: f.source_revision(),
                    participants: vec![DatabaseParticipant {
                        id: d.id().as_uuid(),
                        requirement: 1,
                        revision_domain: RevisionDomain::SourceAuthoritative,
                        command_digest: digest,
                        before: f.source_revision(),
                        after: f.source_revision() + 1,
                    }],
                }
            })
            .collect::<Vec<_>>();
        assert!(
            bindings
                .iter()
                .any(|b| b.participants[0].id == source.as_uuid())
        );
        let mut slices = Vec::new();
        for b in &bindings {
            for c in claims.claims().iter().filter(|c| {
                matches!(
                    c.allocation_class(),
                    FullCapacityAllocationClass::SourceDatabaseCommit
                        | FullCapacityAllocationClass::SourceWalShm
                )
            }) {
                let sidecar = c.allocation_class() == FullCapacityAllocationClass::SourceWalShm;
                slices.push(DatabaseClaimSlice {
                    id: Uuid::new_v4(),
                    claim: c.claim_id().as_uuid(),
                    binding: b.id,
                    participant: b.participants[0].id,
                    direction: ClaimDirection::RequiredForward,
                    class: c.allocation_class(),
                    ceiling: if sidecar { 8192 } else { 4096 },
                    wal_ceiling: if sidecar { 4096 } else { 0 },
                    shm_ceiling: if sidecar { 4096 } else { 0 },
                });
            }
        }
        let inventory = DatabaseClaimInventory::new(
            InventoryHeader {
                profile: owner.profile_root_identity().into(),
                acquisition: owner.owner_identity().into(),
                ownership_generation: owner.ownership_generation(),
                decision_nonce: Uuid::new_v4(),
            },
            bindings,
            slices,
        )
        .unwrap();
        (record, claims, inventory)
    }
    fn logical_provider(record: &StoredOperationRecord) -> CapacityProvider {
        let plan = full_capacity_plan_for_record(record).unwrap();
        let mut facts = BTreeMap::new();
        for a in plan.allocations() {
            let row = facts.entry(a.volume().clone()).or_insert(VolumeFacts {
                identity: a.volume().clone(),
                allocation_unit: a.allocation_unit(),
                free_bytes: PROTECTED_FREE_SPACE_FLOOR,
            });
            row.free_bytes += a.allocated_bytes() * 8;
        }
        CapacityProvider::qualified_fixture(facts)
    }
    fn settle_fixture(
        store: &OperationJournalStore,
        record: &StoredOperationRecord,
        claims: &AdmissionClaimInventory,
        inventory: &DatabaseClaimInventory,
        provider: &CapacityProvider,
    ) -> InitialClaimSettlement {
        let plan = full_capacity_plan_for_record(record).unwrap();
        let lease = provider.admit(&plan, claims).unwrap();
        // Same actual atomic store/rollback/readback code; skips only native descriptor peak
        // qualification which a synthetic provider cannot supply.
        store
            .settle_initial_claims_with_inventory(record, claims, provider, lease, Some(inventory))
            .unwrap()
    }
    fn rows(store: &OperationJournalStore, table: &str) -> i64 {
        store
            .connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
    #[test]
    fn original_profile_generation_mismatch_refuses_before_pin_or_sql() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, claims, inventory) = input(&f.owner, 2);
        let mut header = inventory.header().clone();
        header.ownership_generation = if f.owner.ownership_generation() == i64::MAX as u64 {
            1
        } else {
            f.owner.ownership_generation() + 1
        };
        let bad = DatabaseClaimInventory::new(
            header,
            inventory.bindings().to_vec(),
            inventory.slices().to_vec(),
        )
        .unwrap();
        let provider = logical_provider(&record);
        let plan = full_capacity_plan_for_record(&record).unwrap();
        let lease = provider.admit(&plan, &claims).unwrap();
        let sql_entries = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_entries = std::sync::Arc::clone(&sql_entries);
        store.set_insert_test_hook(move |_| {
            observed_entries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            InsertTestDirective::Continue
        });
        assert!(matches!(
            store.settle_initial_claims_with_inventory(
                &record, &claims, &provider, lease, Some(&bad)
            ),
            Err(InitialClaimSettlementError::Store(
                OperationJournalStoreError::InconsistentRecord { reason }
            )) if reason.contains("current profile binding mismatch")
        ));
        assert_eq!(sql_entries.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(store.get(record.operation_id).unwrap().is_none());
        for table in [
            database_claims::BINDING_TABLE,
            database_claims::SLICE_TABLE,
            ordinary_claims::TABLE_NAME,
        ] {
            assert_eq!(rows(&store, table), 0);
        }
        assert!(
            store
                .read_database_claim_inventory_for_nonce(
                    bad.header().decision_nonce,
                    bad.digest().unwrap()
                )
                .unwrap()
                .is_none()
        );
        for claim in claims.claims() {
            assert_eq!(
                provider.outstanding(claim.volume(), claim.allocation_class()),
                0
            );
        }
    }
    #[test]
    fn same_volume_split_is_atomic_and_reopens_with_retained_charges() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, claims, inventory) = input(&f.owner, 2);
        let provider = logical_provider(&record);
        let settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
        assert!(settled.outcome().receipt().is_some());
        assert_eq!(rows(&store, database_claims::BINDING_TABLE), 2);
        assert_eq!(rows(&store, database_claims::SLICE_TABLE), 4);
        assert_eq!(
            store
                .read_database_claim_inventory(record.operation_id)
                .unwrap(),
            Some(inventory.clone())
        );
        store.validate_database_claims_inside_transaction().unwrap();
        assert!(matches!(
            settled.lease().unwrap().disposition(),
            ProvisionalLeaseDisposition::Durable { .. }
        ));
        drop(settled);
        drop(store);
        drop(f.owner);
        let owner = WritableProfileGuard::acquire_current().unwrap();
        assert_ne!(owner.owner_identity(), inventory.header().acquisition);
        assert!(owner.ownership_generation() > inventory.header().ownership_generation);
        let store = OperationJournalStore::open(&owner).unwrap();
        assert_eq!(
            store
                .read_database_claim_inventory_for_nonce(
                    inventory.header().decision_nonce,
                    inventory.digest().unwrap(),
                )
                .unwrap(),
            Some((record.operation_id, inventory.clone()))
        );
        assert_eq!(
            store.get(record.operation_id).unwrap(),
            Some(record.clone())
        );
        assert_eq!(
            store
                .read_database_claim_inventory(record.operation_id)
                .unwrap(),
            Some(inventory)
        );
        store.rebuild_ordinary_claim_snapshot().unwrap();
        for c in claims.claims() {
            assert_eq!(
                provider.outstanding(c.volume(), c.allocation_class()),
                c.allocated_bytes()
            );
        }
    }
    #[test]
    fn confirmed_precommit_rollback_removes_all_rows_and_releases_only_provisional_charge() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, claims, inventory) = input(&f.owner, 2);
        let provider = logical_provider(&record);
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                InsertTestDirective::Fail(database_claims::error("injected precommit failure"))
            } else {
                InsertTestDirective::Continue
            }
        });
        let settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
        assert_eq!(
            settled.outcome().failure().unwrap().stage(),
            JournalInsertWriteStage::RolledBackBeforeCommit
        );
        assert!(settled.lease().is_none());
        assert!(store.get(record.operation_id).unwrap().is_none());
        for table in [
            database_claims::BINDING_TABLE,
            database_claims::SLICE_TABLE,
            ordinary_claims::TABLE_NAME,
        ] {
            assert_eq!(rows(&store, table), 0);
        }
        for c in claims.claims() {
            assert_eq!(provider.outstanding(c.volume(), c.allocation_class()), 0);
        }
    }
    #[test]
    fn uncertain_commit_and_lost_reply_keep_the_exact_inventory_and_capacity_pinned() {
        for boundary in [InsertTestBoundary::Commit, InsertTestBoundary::AfterCommit] {
            let f = fixture();
            let store = OperationJournalStore::open(&f.owner).unwrap();
            let (record, claims, inventory) = input(&f.owner, 2);
            let provider = logical_provider(&record);
            store.set_insert_test_hook(move |actual| {
                if actual == boundary {
                    if boundary == InsertTestBoundary::Commit {
                        InsertTestDirective::ExecuteThenFail(database_claims::error(
                            "lost commit acknowledgement",
                        ))
                    } else {
                        InsertTestDirective::Fail(database_claims::error("lost delivery"))
                    }
                } else {
                    InsertTestDirective::Continue
                }
            });
            let settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
            assert!(settled.outcome().failure().is_some());
            assert!(settled.lease().is_some());
            assert_eq!(
                store.get(record.operation_id).unwrap(),
                Some(record.clone())
            );
            assert_eq!(
                store
                    .read_database_claim_inventory(record.operation_id)
                    .unwrap(),
                Some(inventory.clone())
            );
            drop(settled);
            drop(store);
            let store = OperationJournalStore::open(&f.owner).unwrap();
            assert_eq!(
                store
                    .read_database_claim_inventory(record.operation_id)
                    .unwrap(),
                Some(inventory)
            );
            store.rebuild_ordinary_claim_snapshot().unwrap();
            for c in claims.claims() {
                assert_eq!(
                    provider.outstanding(c.volume(), c.allocation_class()),
                    c.allocated_bytes()
                );
            }
        }
    }
    #[test]
    fn bound_claim_release_and_duplicate_intent_do_not_erase_evidence() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, claims, inventory) = input(&f.owner, 1);
        let provider = logical_provider(&record);
        let settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
        assert!(settled.outcome().receipt().is_some());
        let bound = claims
            .claims()
            .iter()
            .find(|c| c.allocation_class() == FullCapacityAllocationClass::SourceWalShm)
            .unwrap();
        let request =
            OrdinaryClaimReleaseRequest::new(record.operation_id, bound.claim_id(), Uuid::new_v4())
                .unwrap();
        assert!(store.persist_ordinary_claim_release(request).is_err());
        assert_eq!(
            provider.outstanding(bound.volume(), bound.allocation_class()),
            bound.allocated_bytes()
        );
        let outcome = store.persist_record_with_encoded_outcome_and_database_claims(
            &record,
            &encode(&record).unwrap(),
            Some(&claims),
            Some(&inventory),
        );
        assert!(matches!(
            outcome.failure().unwrap().error(),
            OperationJournalStoreError::DuplicateOperation { .. }
        ));
        assert_eq!(rows(&store, database_claims::BINDING_TABLE), 1);
        assert_eq!(rows(&store, database_claims::SLICE_TABLE), 2);
        assert_eq!(
            store
                .read_database_claim_inventory(record.operation_id)
                .unwrap(),
            Some(inventory)
        );
    }
    #[test]
    fn source_revision_command_and_omitted_participant_fail_before_any_rows() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, claims, inventory) = input(&f.owner, 2);
        for kind in 0..3 {
            let mut bindings = inventory.bindings().to_vec();
            let mut slices = inventory.slices().to_vec();
            match kind {
                0 => {
                    bindings[0].current_revision += 1;
                    bindings[0].participants[0].before += 1;
                }
                1 => bindings[0].participants[0].command_digest = [9; 32],
                _ => {
                    let id = bindings.pop().unwrap().id;
                    slices.retain(|s| s.binding != id);
                }
            }
            let bad =
                DatabaseClaimInventory::new(inventory.header().clone(), bindings, slices).unwrap();
            let provider = logical_provider(&record);
            let plan = full_capacity_plan_for_record(&record).unwrap();
            let lease = provider.admit(&plan, &claims).unwrap();
            assert!(
                store
                    .settle_initial_claims_with_inventory(
                        &record,
                        &claims,
                        &provider,
                        lease,
                        Some(&bad)
                    )
                    .is_err()
            );
            assert_eq!(rows(&store, database_claims::BINDING_TABLE), 0);
            assert!(store.get(record.operation_id).unwrap().is_none());
            for c in claims.claims() {
                assert_eq!(provider.outstanding(c.volume(), c.allocation_class()), 0);
            }
        }
    }
    #[test]
    fn corrupt_binding_slice_and_orphan_rows_block_bounded_restart_without_repair() {
        for sql in [
            "UPDATE operation_journal_database_binding SET inventory_digest=zeroblob(32)",
            "UPDATE operation_journal_database_claim_slice SET direction=2",
            "DELETE FROM operation_journal_database_claim_slice",
            "DELETE FROM operation_journal WHERE operation_id IN(SELECT operation_id FROM operation_journal_database_binding)",
        ] {
            let f = fixture();
            let store = OperationJournalStore::open(&f.owner).unwrap();
            let (record, claims, inventory) = input(&f.owner, 1);
            let provider = logical_provider(&record);
            let settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
            assert!(settled.outcome().receipt().is_some());
            store
                .connection
                .execute_batch("PRAGMA foreign_keys=OFF")
                .unwrap();
            store.connection.execute_batch(sql).unwrap();
            let before = store.connection.total_changes();
            assert!(store.validate_database_claims_inside_transaction().is_err());
            assert_eq!(store.connection.total_changes(), before);
            drop(store);
            let store = OperationJournalStore::open(&f.owner).unwrap();
            assert!(store.rebuild_ordinary_claim_snapshot().is_err());
            for c in claims.claims() {
                assert_eq!(
                    provider.outstanding(c.volume(), c.allocation_class()),
                    c.allocated_bytes()
                );
            }
        }
    }
    #[test]
    fn v10_migration_preserves_legacy_claims_without_fabricated_database_permission() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let record = intent_record(OperationId::new());
        let claims = claims_for_intent(&record);
        let provider = logical_provider(&record);
        assert!(
            store
                .settle_initial_claims(&record, &provider, &claims)
                .unwrap()
                .outcome()
                .receipt()
                .is_some()
        );
        store.connection.execute_batch("DROP TABLE operation_journal_database_attempt_outcome; DROP TABLE operation_journal_database_attempt; DROP TABLE operation_journal_database_claim_slice; DROP TABLE operation_journal_database_binding; PRAGMA user_version=10").unwrap();
        drop(store);
        let store = OperationJournalStore::open(&f.owner).unwrap();
        assert_eq!(
            store
                .connection
                .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            JOURNAL_SCHEMA_VERSION as i64
        );
        assert_eq!(
            store.get(record.operation_id).unwrap(),
            Some(record.clone())
        );
        assert!(
            store
                .read_database_claim_inventory(record.operation_id)
                .unwrap()
                .is_none()
        );
        store.rebuild_ordinary_claim_snapshot().unwrap();
        assert_eq!(ordinary_claim_count(&store), 1);
        assert_eq!(
            provider.outstanding(
                claims.claims()[0].volume(),
                claims.claims()[0].allocation_class()
            ),
            1
        );
    }
}
