// Actual profile-owned journal/SQLite; all registered-phase/capacity inputs below are explicitly
// historical scalar storage fixtures. They establish no live registration or physical grant.
#[cfg(test)]
mod source_lease_binding_storage {
    use super::*;
    use crate::operation_journal::{
        OperationActor, OperationKind, ParticipantCheckpointState, ParticipantId,
        database_attempts::DatabaseAttemptStart,
        source_lease_binding::StoredSourceLeaseFence,
        source_registry_retirement::SourceRegistryRetirementIntentArgs,
        work_plan::{WorkUnitKey, WorkUnitRole},
    };
    use crate::sample_sources::db::{
        SourceRegistryReferenceProposal, SourceRegistryReferenceTransition,
    };
    use serde_json::{Value, json};

    fn scope(acquisition: &str) -> StoredSourceLeaseFence {
        // A stored registered-phase observation, never SourceAuthorityRegistration or receipt.
        serde_json::from_value(json!({
            "version":1,
            "subject":{
                "source_id":"pre-audit-storage-fixture", "lifecycle_generation":3,
                "profile_acquisition":acquisition,
                "lease_owner":Uuid::from_u128(2), "lease_epoch":7,
                "media_root_identity":[255,0,1], "database_root_identity":"db-root",
                "database_identity":"main", "root_binding_identity":"root-binding",
                "scan_revision":0, "audit":{"status":"unavailable"},
                "registration":{"registration_id":Uuid::from_u128(3),"phase":"registered_phase_observed"}
            },
            "preparation_nonce":Uuid::from_u128(4), "page_size":4096,"maximum_pages":16384,
            "objects":[
                {"identity":"main","device":1,"unit":4096,"bytes":4096,"ceiling":272646144},
                {"identity":"wal","device":1,"unit":4096,"bytes":0,"ceiling":274206720},
                {"identity":"shm","device":1,"unit":4096,"bytes":32768,"ceiling":5505024}
            ]
        })).unwrap()
    }
    fn proposal(
        fence: &StoredSourceLeaseFence,
        operation: OperationId,
        key: WorkUnitKey,
        transition: SourceRegistryReferenceTransition,
    ) -> SourceRegistryReferenceProposal {
        let prior = match transition {
            SourceRegistryReferenceTransition::Draining => "enrolled",
            SourceRegistryReferenceTransition::Retired => "draining",
            _ => unreachable!(),
        };
        SourceRegistryReferenceProposal::decode(
            &serde_json::to_vec(&json!({
                "version":1,"operation_id":operation,"work_key":key,"transition":transition,
                "expected":{"status":"present","state":prior,"row_identity":vec![1;32]},
                "subject":fence.subject()
            }))
            .unwrap(),
        )
        .unwrap()
    }
    fn input(
        store: &OperationJournalStore,
        owner: &WritableProfileGuard,
    ) -> (
        StoredOperationRecord,
        AdmissionClaimInventory,
        DatabaseClaimInventory,
    ) {
        let (base, _, _) = integration::actual_journal_input(store, owner);
        let source = ParticipantId::new();
        let global = ParticipantId::new();
        let operation = OperationId::new();
        let fence = scope(owner.owner_identity());
        fence.validate().unwrap();
        let draining = proposal(
            &fence,
            operation,
            WorkUnitKey::new(0, 0, global, WorkUnitRole::GlobalLibrary),
            SourceRegistryReferenceTransition::Draining,
        );
        let retired = proposal(
            &fence,
            operation,
            WorkUnitKey::new(0, 2, global, WorkUnitRole::GlobalLibrary),
            SourceRegistryReferenceTransition::Retired,
        );
        let args = SourceRegistryRetirementIntentArgs::new(
            source,
            global,
            fence.clone(),
            &draining,
            &retired,
        )
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
        let universal = UniversalIntent::new(
            None,
            UniversalCommandKind::SourceRegistryRetirement,
            "operation.status.source_registry_retirement",
            participants,
        )
        .unwrap();
        let mut allocations = base
            .full_capacity_plan
            .unwrap()
            .allocations()
            .iter()
            .filter(|a| a.class() == FullCapacityAllocationClass::JournalRecord)
            .cloned()
            .collect::<Vec<_>>();
        for (class, bytes) in [
            (FullCapacityAllocationClass::SourceDatabaseCommit, 4096),
            (FullCapacityAllocationClass::SourceWalShm, 8192),
            (FullCapacityAllocationClass::GlobalDatabaseCommit, 4096),
            (FullCapacityAllocationClass::GlobalWalShm, 8192),
        ] {
            allocations.push(
                crate::operation_journal::full_capacity::FullCapacityAllocation::new(
                    VolumeIdentity { device: 1 },
                    4096,
                    class,
                    bytes,
                    bytes,
                    PROTECTED_FREE_SPACE_FLOOR,
                )
                .unwrap(),
            );
        }
        let record = StoredOperationRecord::new_v18_source_retirement(
            operation,
            OperationIntent {
                actor: OperationActor::User,
                kind: OperationKind::SourceRegistryRetirement,
                label: "Historical retirement storage fixture".into(),
            },
            universal,
            UniversalCommandArgs::SourceRegistryRetirement(args),
            FullCapacityPlan::new(allocations).unwrap(),
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
        let source_binding = DatabaseBinding {
            id: Uuid::new_v4(),
            role: DatabaseRole::Source,
            volume: 1,
            allocation_unit: 4096,
            main_file: "main".into(),
            registration: Uuid::from_u128(3),
            owner_generation: 7,
            lifecycle: 3,
            database_root: "db-root".into(),
            root_binding: "root-binding".into(),
            source: Some(SourceBinding::from_lease_fence(&fence).unwrap()),
            current_revision: 0,
            participants: vec![DatabaseParticipant {
                id: source.as_uuid(),
                requirement: 1,
                revision_domain: RevisionDomain::SourceLeaseLifecycle,
                command_digest: digest,
                before: 7,
                after: 7,
            }],
        };
        let global_binding = DatabaseBinding {
            id: Uuid::new_v4(),
            role: DatabaseRole::Global,
            volume: 1,
            allocation_unit: 4096,
            main_file: "global".into(),
            registration: Uuid::new_v4(),
            owner_generation: 1,
            lifecycle: 1,
            database_root: "global-root".into(),
            root_binding: "global-binding".into(),
            source: None,
            current_revision: 1,
            participants: vec![DatabaseParticipant {
                id: global.as_uuid(),
                requirement: 1,
                revision_domain: RevisionDomain::GlobalMutationGeneration,
                command_digest: digest,
                before: 1,
                after: 2,
            }],
        };
        let bindings = vec![source_binding, global_binding];
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
                    participant: b.participants[0].id,
                    direction: if b.role == DatabaseRole::Source {
                        ClaimDirection::LeaseMaintenance
                    } else {
                        ClaimDirection::RequiredForward
                    },
                    class,
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
        inventory.validate_record(&record).unwrap();
        inventory
            .validate_against(record.full_capacity_plan.as_ref().unwrap(), &claims)
            .unwrap();
        (record, claims, inventory)
    }
    #[test]
    fn actual_versioned_binding_storage_child_suite() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","operation_journal::store::tests::exact_database_claim_persistence::source_lease_binding_storage::binding_storage_child","--nocapture"])
            .env("YIELD_BINDING_STORAGE_CHILD","1").output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
    }
    #[test]
    fn binding_storage_child() {
        if std::env::var("YIELD_BINDING_STORAGE_CHILD").as_deref() != Ok("1") {
            return;
        }
        codec_and_domain_refusals();
        actual_atomic_storage_attempts_and_legacy_reopen();
        retirement_forward_codec_and_storage();
        registered_retirement_codec_and_storage();
        expected_decoder_preserves_retained_retirement_refusals();
    }
    fn codec_and_domain_refusals() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, claims, inventory) = input(&store, &f.owner);
        let image = crate::operation_journal::encode(&record).unwrap();
        let decoded = crate::operation_journal::decode(&image).unwrap();
        assert_eq!(
            crate::operation_journal::codec::decode_with_limits(&image, stored_json_limits())
                .unwrap(),
            decoded
        );
        assert_eq!(crate::operation_journal::encode(&decoded).unwrap(), image);
        assert_eq!(
            crate::operation_journal::classify_record(&decoded),
            crate::operation_journal::RecordConsistency::Consistent
        );
        let bytes = inventory.encode().unwrap();
        assert!(bytes.starts_with(b"YWCB\x02\x00"));
        assert_eq!(DatabaseClaimInventory::decode(&bytes).unwrap(), inventory);
        for size in 0..bytes.len() {
            assert!(DatabaseClaimInventory::decode(&bytes[..size]).is_err());
        }
        let mut wrong = bytes.clone();
        wrong[4] = 1;
        assert!(DatabaseClaimInventory::decode(&wrong).is_err());
        let mut trailing = bytes;
        trailing.push(0);
        assert!(DatabaseClaimInventory::decode(&trailing).is_err());
        for version in 1..=17 {
            let mut r = record.clone();
            r.schema_version = version;
            assert!(crate::operation_journal::encode(&r).is_err());
            let mut raw: Value = serde_json::from_slice(&image).unwrap();
            raw["schema_version"] = version.into();
            assert!(crate::operation_journal::decode(&serde_json::to_vec(&raw).unwrap()).is_err());
        }
        let (old, _, legacy) = integration::actual_journal_input(&store, &f.owner);
        let old_image = crate::operation_journal::encode(&old).unwrap();
        assert!(
            !String::from_utf8(old_image.clone())
                .unwrap()
                .contains("lease_fence")
        );
        assert_eq!(
            crate::operation_journal::encode(
                &crate::operation_journal::decode(&old_image).unwrap()
            )
            .unwrap(),
            old_image
        );
        assert!(legacy.encode().unwrap().starts_with(b"YWCB\x01\x00"));
        let mut old_raw: Value = serde_json::from_slice(&old_image).unwrap();
        old_raw["universal_intent"]["participants"]["descriptors"][0]["lease_fence"] = Value::Null;
        assert!(crate::operation_journal::decode(&serde_json::to_vec(&old_raw).unwrap()).is_err());
        let mut old_as_new = old.clone();
        old_as_new.schema_version = 18;
        assert!(crate::operation_journal::encode(&old_as_new).is_err());
        let mut wrong = inventory.bindings().to_vec();
        let source_index = wrong
            .iter()
            .position(|b| b.role == DatabaseRole::Source)
            .unwrap();
        for fault in 0..12 {
            let mut bindings = wrong.clone();
            let b = &mut bindings[source_index];
            match fault {
                0 => b.participants[0].revision_domain = RevisionDomain::SourceAuthoritative,
                1 => b.participants[0].before = 0,
                2 => b.participants[0].after = 8,
                3 => b.registration = Uuid::new_v4(),
                4 => b.owner_generation += 1,
                5 => b.current_revision = 1,
                6 => b.source.as_mut().unwrap().lease_owner = Uuid::new_v4().to_string(),
                7 => b.root_binding.push('x'),
                8 => b.main_file.push('x'),
                9 => b.source.as_mut().unwrap().lease_fence = None,
                10 => b.participants[0].requirement = 2,
                _ => b.volume += 1,
            }
            assert!(
                DatabaseClaimInventory::new(
                    inventory.header().clone(),
                    bindings,
                    inventory.slices().to_vec()
                )
                .is_err(),
                "binding fault {fault}"
            );
        }
        for direction in [
            ClaimDirection::RequiredForward,
            ClaimDirection::RequiredCompensation,
            ClaimDirection::Routine,
        ] {
            let mut slices = inventory.slices().to_vec();
            slices
                .iter_mut()
                .filter(|s| s.binding == wrong[source_index].id)
                .for_each(|s| s.direction = direction);
            assert!(
                DatabaseClaimInventory::new(inventory.header().clone(), wrong.clone(), slices)
                    .is_err()
            );
        }
        let mut header = inventory.header().clone();
        header.acquisition = Uuid::new_v4().to_string();
        assert!(
            DatabaseClaimInventory::new(header, wrong.clone(), inventory.slices().to_vec())
                .is_err()
        );
        let mut changed = wrong.clone();
        changed[source_index].participants[0].command_digest = [9; 32];
        let changed = DatabaseClaimInventory::new(
            inventory.header().clone(),
            changed,
            inventory.slices().to_vec(),
        )
        .unwrap();
        assert!(changed.validate_record(&record).is_err());
        wrong.retain(|b| b.role != DatabaseRole::Source);
        let omitted = DatabaseClaimInventory::new(
            inventory.header().clone(),
            wrong.clone(),
            inventory
                .slices()
                .iter()
                .filter(|s| s.binding == wrong[0].id)
                .cloned()
                .collect(),
        )
        .unwrap();
        assert!(omitted.validate_record(&record).is_err());
        let mut slices = inventory.slices().to_vec();
        slices[0].ceiling += 4096;
        assert!(
            DatabaseClaimInventory::new(
                inventory.header().clone(),
                inventory.bindings().to_vec(),
                slices
            )
            .and_then(|i| i.validate_against(record.full_capacity_plan.as_ref().unwrap(), &claims))
            .is_err()
        );
        let fence = inventory.bindings()[source_index]
            .source
            .as_ref()
            .unwrap()
            .lease_fence
            .as_ref()
            .unwrap();
        let canonical = fence.canonical_bytes().unwrap();
        let duplicate = String::from_utf8(canonical.clone())
            .unwrap()
            .replace("\"version\":1", "\"version\":1,\"version\":1");
        assert!(StoredSourceLeaseFence::decode(duplicate.as_bytes()).is_err());
        let mut unknown = serde_json::to_value(fence).unwrap();
        unknown["unknown"] = json!(1);
        assert!(serde_json::from_value::<StoredSourceLeaseFence>(unknown).is_err());
        let mut incomplete = inventory.slices().to_vec();
        incomplete.retain(|s| s.class != FullCapacityAllocationClass::SourceDatabaseCommit);
        assert!(
            DatabaseClaimInventory::new(
                inventory.header().clone(),
                inventory.bindings().to_vec(),
                incomplete
            )
            .is_err()
        );

        assert!(
            StoredSourceLeaseFence::decode(&serde_json::to_vec_pretty(fence).unwrap()).is_err()
        );
        let mut oversized = canonical.clone();
        oversized.resize(16385, b' ');
        assert!(StoredSourceLeaseFence::decode(&oversized).is_err());
        for (path, value) in [
            ("/version", json!(2)),
            ("/preparation_nonce", json!(Uuid::nil())),
            ("/subject/registration/phase", json!("retired")),
            ("/subject/registration", Value::Null),
            ("/subject/media_root_identity", json!(vec![0; 177])),
            ("/objects/1/identity", json!("main")),
            ("/maximum_pages", json!(16385)),
            ("/objects/2/unit", json!(3)),
        ] {
            let mut raw = serde_json::to_value(fence).unwrap();
            *raw.pointer_mut(path).unwrap() = value;
            let invalid: StoredSourceLeaseFence = serde_json::from_value(raw).unwrap();
            assert!(invalid.validate().is_err(), "scope {path}");
        }
        let mut raw = serde_json::to_value(fence).unwrap();
        raw["subject"]["registration"]["phase"] = json!("prepared");
        let prepared: StoredSourceLeaseFence = serde_json::from_value(raw).unwrap();
        prepared.validate().unwrap();
        assert!(SourceBinding::from_lease_fence(&prepared).is_err());
        // Every immutable nonce/root/registration change affects bytes and command digest.
        let digest = command_digest(&record, 1).unwrap();
        let mut raw: Value = serde_json::from_slice(&image).unwrap();
        raw["universal_command_args"]["SourceRegistryRetirement"]["lease_fence"]["preparation_nonce"] =
            json!(Uuid::new_v4());
        assert!(crate::operation_journal::decode(&serde_json::to_vec(&raw).unwrap()).is_err());
        assert_ne!(digest, command_digest(&old, 1).unwrap());
        // V2 scope frames participate in the original aggregate byte bound, before persistence.
        let template = inventory
            .bindings()
            .iter()
            .find(|b| b.role == DatabaseRole::Source)
            .unwrap();
        let mut bindings = Vec::new();
        let mut slices = Vec::new();
        let mut aggregate_refused = false;
        for index in 0..64 {
            let mut raw = serde_json::to_value(
                template
                    .source
                    .as_ref()
                    .unwrap()
                    .lease_fence
                    .as_ref()
                    .unwrap(),
            )
            .unwrap();
            let main = format!("bounded-main-{index}");
            raw["subject"]["database_identity"] = json!(main);
            raw["objects"][0]["identity"] = json!(main);
            raw["subject"]["source_id"] = json!(format!("{index:03}{}", "s".repeat(253)));
            let fence: StoredSourceLeaseFence = serde_json::from_value(raw).unwrap();
            let mut b = template.clone();
            b.id = Uuid::new_v4();
            b.main_file = main;
            b.source = Some(SourceBinding::from_lease_fence(&fence).unwrap());
            b.participants[0].id = Uuid::new_v4();
            for original in inventory
                .slices()
                .iter()
                .filter(|s| s.binding == template.id)
            {
                let mut slice = original.clone();
                slice.id = Uuid::new_v4();
                slice.binding = b.id;
                slice.participant = b.participants[0].id;
                slices.push(slice);
            }
            bindings.push(b);
            match DatabaseClaimInventory::new(
                inventory.header().clone(),
                bindings.clone(),
                slices.clone(),
            ) {
                Ok(i) => assert!(i.encode().unwrap().len() <= 65536),
                Err(e) => {
                    assert_eq!(e.0, "aggregate encoding bound");
                    aggregate_refused = true;
                    break;
                }
            }
        }
        assert!(aggregate_refused);
        let too_many = vec![template.clone(); 65];
        assert_eq!(
            DatabaseClaimInventory::new(
                inventory.header().clone(),
                too_many,
                inventory.slices().to_vec()
            )
            .unwrap_err()
            .0,
            "inventory count"
        );
        let too_many = vec![inventory.slices()[0].clone(); 257];
        assert_eq!(
            DatabaseClaimInventory::new(
                inventory.header().clone(),
                inventory.bindings().to_vec(),
                too_many
            )
            .unwrap_err()
            .0,
            "inventory count"
        );
    }
    fn actual_atomic_storage_attempts_and_legacy_reopen() {
        for fault in 0..3 {
            let f = fixture();
            let store = OperationJournalStore::open(&f.owner).unwrap();
            let (old, old_claims, old_inventory) =
                integration::actual_journal_input(&store, &f.owner);
            let old_provider = logical_provider(&old);
            assert!(
                settle_fixture(&store, &old, &old_claims, &old_inventory, &old_provider)
                    .outcome()
                    .receipt()
                    .is_some()
            );
            let old_bytes = crate::operation_journal::encode(&old).unwrap();
            let (record, claims, inventory) = input(&store, &f.owner);
            let provider = logical_provider(&record);
            if fault != 0 {
                store.set_insert_test_hook(move |at| {
                    if fault == 1 && at == InsertTestBoundary::BeforePrecommitValidation {
                        InsertTestDirective::Fail(database_claims::error("binding rollback"))
                    } else if fault == 2 && at == InsertTestBoundary::Commit {
                        InsertTestDirective::ExecuteThenFail(database_claims::error(
                            "binding lost commit reply",
                        ))
                    } else {
                        InsertTestDirective::Continue
                    }
                });
            }
            let settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
            if fault == 1 {
                assert!(settled.outcome().receipt().is_none());
                assert!(
                    store
                        .read_database_claim_inventory(record.operation_id)
                        .unwrap()
                        .is_none()
                );
                continue;
            }
            if fault == 2 {
                assert!(
                    matches!(settled.outcome(), JournalInsertOutcome::Failed(f) if f.stage == JournalInsertWriteStage::CommitOutcomeUnknown)
                );
                assert!(settled.lease().is_some());
            } else {
                assert!(settled.outcome().receipt().is_some());
            }
            assert_eq!(
                store
                    .read_database_claim_inventory(record.operation_id)
                    .unwrap()
                    .unwrap(),
                inventory
            );
            assert_eq!(
                store
                    .read_database_claim_inventory(old.operation_id)
                    .unwrap()
                    .unwrap(),
                old_inventory
            );
            let b = inventory
                .bindings()
                .iter()
                .find(|b| b.role == DatabaseRole::Source)
                .unwrap();
            let db = inventory
                .slices()
                .iter()
                .find(|s| {
                    s.binding == b.id
                        && s.class == FullCapacityAllocationClass::SourceDatabaseCommit
                })
                .unwrap();
            let wal = inventory
                .slices()
                .iter()
                .find(|s| s.binding == b.id && s.class == FullCapacityAllocationClass::SourceWalShm)
                .unwrap();
            let start = DatabaseAttemptStart::from_context(
                &record,
                &inventory,
                Uuid::new_v4(),
                Uuid::new_v4(),
                db.id,
                wal.id,
                4096,
                4096,
                4096,
            )
            .unwrap();
            assert_eq!(start.version, 2);
            assert_eq!((start.before, start.after), (7, 7));
            assert_eq!(
                DatabaseAttemptStart::decode(&start.encode().unwrap()).unwrap(),
                start
            );
            let mut old_start = start.clone();
            old_start.version = 1;
            assert!(old_start.encode().is_err());
            assert!(!store.persist_database_attempt_start(&start).unwrap().replay);
            assert!(store.persist_database_attempt_start(&start).unwrap().replay);
            let before = integration::durable_image(&store);
            let mut conflicting = start.clone();
            conflicting.nonce = Uuid::new_v4();
            assert!(store.persist_database_attempt_start(&conflicting).is_err());
            assert_eq!(integration::durable_image(&store), before);
            drop(store);
            let reopened = OperationJournalStore::open(&f.owner).unwrap();
            assert_eq!(
                reopened
                    .read_database_claim_inventory(record.operation_id)
                    .unwrap()
                    .unwrap(),
                inventory
            );
            assert_eq!(
                reopened
                    .read_database_claim_inventory(old.operation_id)
                    .unwrap()
                    .unwrap(),
                old_inventory
            );
            assert_eq!(
                reopened
                    .read_database_attempt(start.attempt)
                    .unwrap()
                    .unwrap()
                    .0,
                start
            );
            let old_read = reopened.get(old.operation_id).unwrap().unwrap();
            assert_eq!(
                crate::operation_journal::encode(&old_read).unwrap(),
                old_bytes
            );
            assert_eq!(
                reopened
                    .connection
                    .pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
                    .unwrap(),
                12
            );
            // Corruption retains the evidence and fails actual restart readback; no owner restored.
            let mut bad = start.clone();
            bad.schema = 17;
            reopened
                .connection
                .execute(
                    "UPDATE operation_journal_database_attempt SET image=?1 WHERE attempt_id=?2",
                    params![bad.encode().unwrap(), start.attempt.to_string()],
                )
                .unwrap();
            assert!(reopened.read_database_attempt(start.attempt).is_err());
        }
    }
    include!("retirement_forward_storage_tests.rs");
    fn expected_decoder_preserves_retained_retirement_refusals() {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        for field in ["row_identity", "unknown"] {
            let f = fixture();
            let store = OperationJournalStore::open(&f.owner).unwrap();
            let (record, claims, inventory) = input(&store, &f.owner);
            let provider = logical_provider(&record);
            assert!(
                settle_fixture(&store, &record, &claims, &inventory, &provider)
                    .outcome()
                    .receipt()
                    .is_some()
            );
            let mut raw: Value =
                serde_json::from_slice(&crate::operation_journal::encode(&record).unwrap())
                    .unwrap();
            let encoded =
                &mut raw["universal_command_args"]["SourceRegistryRetirement"]["draining"];
            let bytes = URL_SAFE_NO_PAD.decode(encoded.as_str().unwrap()).unwrap();
            let mut proposal: Value = serde_json::from_slice(&bytes).unwrap();
            proposal["expected"] =
                json!({"status":"present", "state":"enrolled", field:vec![0u8;6000]});
            let proposal = serde_json::to_vec(&proposal).unwrap();
            assert!(
                proposal.len() < crate::sample_sources::db::SOURCE_REGISTRY_REFERENCE_MAX_BYTES
            );
            *encoded = json!(URL_SAFE_NO_PAD.encode(&proposal));
            let bytes = serde_json::to_vec(&raw).unwrap();
            store
                .connection
                .execute(
                    "UPDATE operation_journal SET record=?1 WHERE operation_id=?2",
                    params![bytes, record.operation_id.to_string()],
                )
                .unwrap();
            let retained: (String,i64,Vec<u8>) = store.connection.query_row("SELECT operation_id,sequence,record FROM operation_journal WHERE operation_id=?1", [record.operation_id.to_string()], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
            let durable = integration::durable_image(&store);
            assert!(store.get(record.operation_id).is_err(), "{field}");
            assert!(store.connection.is_autocommit());
            assert_eq!(integration::durable_image(&store), durable);
            // Typed claim readback validates its parent record and must also refuse.
            // The full raw durable image above proves inventory/charge rows remain exact.
            assert!(
                store
                    .read_database_claim_inventory(record.operation_id)
                    .is_err()
            );
            drop(store);
            let reopened = OperationJournalStore::open(&f.owner).unwrap();
            let after: (String,i64,Vec<u8>) = reopened.connection.query_row("SELECT operation_id,sequence,record FROM operation_journal WHERE operation_id=?1", [record.operation_id.to_string()], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
            assert_eq!(after, retained);
            assert!(reopened.get(record.operation_id).is_err());
            assert!(reopened.connection.is_autocommit());
            assert_eq!(integration::durable_image(&reopened), durable);
        }
    }
}
