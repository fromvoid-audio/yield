// Real SQLite persistence with historical scalar inputs: no physical qualification or Start grant.
#[test]
fn registered_retirement_inventory_preserves_serial_separately_from_lease_epoch() {
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let (_, _, old) = forward_input(&store, &f.owner);
    let source = old
        .bindings()
        .iter()
        .find(|b| b.role == DatabaseRole::Source)
        .unwrap();
    let epoch = source.source.as_ref().unwrap().lease_epoch;
    // Deliberately historical scalars: production issuance/custody belongs to2383/2382.
    let serial = epoch.checked_add(31).unwrap();
    let inventory = DatabaseClaimInventory::new_registered_retirement_forward(
        old.header().clone(),
        old.bindings().to_vec(),
        old.slices().to_vec(),
        serial,
    )
    .unwrap();
    assert_eq!(
        &inventory.encode().unwrap()[..6],
        b"YWCB\x04\x00",
        "genuine serial needs a separate historical policy"
    );
    assert_eq!(inventory.source_registration_generation(), Some(serial));
    assert_eq!(inventory.bindings(), old.bindings());
    assert_eq!(
        DatabaseClaimInventory::decode(&inventory.encode().unwrap()).unwrap(),
        inventory
    );
    assert_eq!(old.source_registration_generation(), None);
    assert_eq!(&old.encode().unwrap()[..6], b"YWCB\x03\x00");
    for invalid in [0, i64::MAX as u64 + 1, u64::MAX] {
        assert!(
            DatabaseClaimInventory::new_registered_retirement_forward(
                old.header().clone(),
                old.bindings().to_vec(),
                old.slices().to_vec(),
                invalid
            )
            .is_err()
        );
    }
}

// Explicit historical scalar storage fixture; actual original producer remains OPT-2382.
fn registered_forward_input(
    store: &OperationJournalStore,
    owner: &WritableProfileGuard,
    serial: u64,
) -> (
    StoredOperationRecord,
    AdmissionClaimInventory,
    DatabaseClaimInventory,
) {
    let (record, claims, old) = forward_input(store, owner);
    let Some(UniversalCommandArgs::SourceRegistryRetirement(args)) =
        record.universal_command_args()
    else {
        unreachable!()
    };
    let digest = args.initial_forward_policy_digest().unwrap();
    let mut slices = old.slices().to_vec();
    for slice in &mut slices {
        let binding = old
            .bindings()
            .iter()
            .find(|b| b.id == slice.binding)
            .unwrap();
        let work = [
            args.source_work(),
            args.draining_work(),
            args.retired_work(),
        ]
        .into_iter()
        .find(|work| {
            crate::operation_journal::database_claims::retirement_forward_policy::slice_identity(
                old.header(),
                binding,
                slice.claim,
                &digest,
                *work,
                slice.direction,
                slice.class,
            )
            .unwrap()
                == slice.id
        })
        .unwrap();
        slice.id = crate::operation_journal::database_claims::retirement_forward_policy::registered_slice_identity(old.header(), binding, serial, slice.claim, &digest, work, slice.direction, slice.class).unwrap();
    }
    let inventory = DatabaseClaimInventory::new_registered_retirement_forward(
        old.header().clone(),
        old.bindings().to_vec(),
        slices,
        serial,
    )
    .unwrap();
    inventory.validate_record(&record).unwrap();
    inventory
        .validate_against(record.full_capacity_plan.as_ref().unwrap(), &claims)
        .unwrap();
    (record, claims, inventory)
}

fn registered_retirement_codec_and_storage() {
    for fault in 0..3 {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, claims, inventory) = registered_forward_input(&store, &f.owner, 38);
        let bytes = inventory.encode().unwrap();
        assert_eq!(&bytes[..6], b"YWCB\x04\x00");
        assert_eq!(DatabaseClaimInventory::decode(&bytes).unwrap(), inventory);
        for length in 0..bytes.len() {
            assert!(DatabaseClaimInventory::decode(&bytes[..length]).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(DatabaseClaimInventory::decode(&trailing).is_err());
        let mut unknown = bytes.clone();
        unknown[4] = 5;
        assert!(DatabaseClaimInventory::decode(&unknown).is_err());
        let offset = 6
            + 2
            + inventory.header().profile.len()
            + 2
            + inventory.header().acquisition.len()
            + 8
            + 16;
        for serial in [0, i64::MAX as u64 + 1] {
            let mut invalid = bytes.clone();
            invalid[offset..offset + 8].copy_from_slice(&serial.to_le_bytes());
            assert!(DatabaseClaimInventory::decode(&invalid).is_err());
        }
        let changed = DatabaseClaimInventory::new_registered_retirement_forward(
            inventory.header().clone(),
            inventory.bindings().to_vec(),
            inventory.slices().to_vec(),
            39,
        )
        .unwrap();
        assert!(
            changed.validate_record(&record).is_err(),
            "every exact work slice binds the independent serial"
        );
        assert_ne!(changed.digest().unwrap(), inventory.digest().unwrap());
        let legacy = DatabaseClaimInventory::new_retirement_forward(
            inventory.header().clone(),
            inventory.bindings().to_vec(),
            inventory.slices().to_vec(),
        )
        .unwrap();
        assert!(
            legacy.validate_record(&record).is_err(),
            "tag4 slices cannot masquerade as tag3"
        );
        let source = inventory
            .bindings()
            .iter()
            .find(|b| b.role == DatabaseRole::Source)
            .unwrap();
        assert_eq!(source.owner_generation, 7);
        assert_eq!(inventory.source_registration_generation(), Some(38));
        let main = inventory
            .slices()
            .iter()
            .find(|s| {
                s.binding == source.id
                    && s.class == FullCapacityAllocationClass::SourceDatabaseCommit
            })
            .unwrap();
        let side = inventory
            .slices()
            .iter()
            .find(|s| {
                s.binding == source.id && s.class == FullCapacityAllocationClass::SourceWalShm
            })
            .unwrap();
        assert!(
            DatabaseAttemptStart::from_context(
                &record,
                &inventory,
                Uuid::new_v4(),
                Uuid::new_v4(),
                main.id,
                side.id,
                4096,
                4096,
                4096
            )
            .is_err()
        );
        if fault != 0 {
            store.set_insert_test_hook(move |at| {
                if fault == 1 && at == InsertTestBoundary::BeforePrecommitValidation {
                    InsertTestDirective::Fail(database_claims::error("registered rollback"))
                } else if fault == 2 && at == InsertTestBoundary::Commit {
                    InsertTestDirective::ExecuteThenFail(database_claims::error(
                        "registered lost reply",
                    ))
                } else {
                    InsertTestDirective::Continue
                }
            });
        }
        let provider = logical_provider(&record);
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
        let nonce = inventory.header().decision_nonce;
        let digest = inventory.digest().unwrap();
        assert_eq!(
            store
                .read_database_claim_inventory_for_nonce(nonce, digest)
                .unwrap(),
            Some((record.operation_id, inventory.clone()))
        );
        drop(store);
        let reopened = OperationJournalStore::open(&f.owner).unwrap();
        assert_eq!(
            reopened
                .read_database_claim_inventory_for_nonce(nonce, digest)
                .unwrap(),
            Some((record.operation_id, inventory.clone()))
        );
        assert_eq!(
            reopened
                .connection
                .pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
                .unwrap(),
            JOURNAL_SCHEMA_VERSION
        );
        let changed_digest = changed.digest().unwrap();
        reopened.connection.execute("UPDATE operation_journal_database_binding SET inventory_digest=?1,inventory=CASE WHEN ordinal=0 THEN ?2 ELSE NULL END WHERE operation_id=?3",params![changed_digest.as_slice(),changed.encode().unwrap(),record.operation_id.to_string()]).unwrap();
        assert!(
            reopened
                .read_database_claim_inventory_for_nonce(nonce, changed_digest)
                .is_err()
        );
    }
}

fn forward_input(
    store: &OperationJournalStore,
    owner: &WritableProfileGuard,
) -> (
    StoredOperationRecord,
    AdmissionClaimInventory,
    DatabaseClaimInventory,
) {
    let (mut record, old_claims, old) = input(store, owner);
    let allocations = record
        .full_capacity_plan
        .as_ref()
        .unwrap()
        .allocations()
        .iter()
        .map(|a| {
            let factor = if matches!(
                a.class(),
                FullCapacityAllocationClass::GlobalDatabaseCommit
                    | FullCapacityAllocationClass::GlobalWalShm
            ) {
                2
            } else {
                1
            };
            crate::operation_journal::full_capacity::FullCapacityAllocation::new(
                a.volume().clone(),
                a.allocation_unit(),
                a.class(),
                a.logical_bytes() * factor,
                a.allocated_bytes() * factor,
                a.protected_free_bytes(),
            )
            .unwrap()
        })
        .collect();
    record.full_capacity_plan = Some(FullCapacityPlan::new(allocations).unwrap());
    let claims = AdmissionClaimInventory::new(
        old_claims
            .claims()
            .iter()
            .map(|c| {
                let allocation = record
                    .full_capacity_plan
                    .as_ref()
                    .unwrap()
                    .allocations()
                    .iter()
                    .find(|a| a.class() == c.allocation_class())
                    .unwrap();
                AdmissionClaim::new(
                    c.claim_id(),
                    allocation.volume().clone(),
                    allocation.class(),
                    allocation.allocated_bytes(),
                )
                .unwrap()
            })
            .collect(),
    )
    .unwrap();
    let Some(UniversalCommandArgs::SourceRegistryRetirement(args)) =
        record.universal_command_args()
    else {
        unreachable!()
    };
    let digest = args.initial_forward_policy_digest().unwrap();
    let mut slices = Vec::new();
    for work in [
        args.source_work(),
        args.draining_work(),
        args.retired_work(),
    ] {
        let role = if work.role() == WorkUnitRole::SourceLeaseMaintenance {
            DatabaseRole::Source
        } else {
            DatabaseRole::Global
        };
        for template in old.slices().iter().filter(|s| {
            old.bindings()
                .iter()
                .any(|b| b.id == s.binding && b.role == role)
        }) {
            let binding = old
                .bindings()
                .iter()
                .find(|b| b.id == template.binding)
                .unwrap();
            let mut slice = template.clone();
            slice.id = crate::operation_journal::database_claims::retirement_forward_policy::slice_identity(old.header(), binding, slice.claim, &digest, work, slice.direction, slice.class).unwrap();
            slices.push(slice);
        }
    }
    let inventory = DatabaseClaimInventory::new_retirement_forward(
        old.header().clone(),
        old.bindings().to_vec(),
        slices,
    )
    .unwrap();
    inventory.validate_record(&record).unwrap();
    inventory
        .validate_against(record.full_capacity_plan.as_ref().unwrap(), &claims)
        .unwrap();
    (record, claims, inventory)
}
fn retirement_forward_codec_and_storage() {
    for fault in 0..3 {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (historical_record, _, historical) = input(&store, &f.owner);
        let historical_bytes = historical.encode().unwrap();
        assert_eq!(&historical_bytes[..6], b"YWCB\x02\x00");
        assert_eq!(
            DatabaseClaimInventory::decode(&historical_bytes).unwrap(),
            historical
        );
        historical.validate_record(&historical_record).unwrap();
        let (record, claims, inventory) = forward_input(&store, &f.owner);
        // Existing attempt formats deliberately refuse the new comparison policy. Issuer
        // integration must bind its exact work pair instead of widening old attempt tags.
        let source = inventory
            .bindings()
            .iter()
            .find(|b| b.role == DatabaseRole::Source)
            .unwrap();
        let main = inventory
            .slices()
            .iter()
            .find(|s| {
                s.binding == source.id
                    && s.class == FullCapacityAllocationClass::SourceDatabaseCommit
            })
            .unwrap();
        let side = inventory
            .slices()
            .iter()
            .find(|s| {
                s.binding == source.id && s.class == FullCapacityAllocationClass::SourceWalShm
            })
            .unwrap();
        assert!(
            DatabaseAttemptStart::from_context(
                &record,
                &inventory,
                Uuid::new_v4(),
                Uuid::new_v4(),
                main.id,
                side.id,
                4096,
                4096,
                4096
            )
            .is_err()
        );
        let image = inventory.encode().unwrap();
        assert_eq!(&image[..6], b"YWCB\x03\x00");
        assert_eq!(DatabaseClaimInventory::decode(&image).unwrap(), inventory);
        for length in 0..image.len() {
            assert!(
                DatabaseClaimInventory::decode(&image[..length]).is_err(),
                "truncation {length}"
            );
        }
        let mut trailing = image.clone();
        trailing.push(0);
        assert!(DatabaseClaimInventory::decode(&trailing).is_err());
        let mut unknown = image.clone();
        unknown[4] = 5;
        assert!(DatabaseClaimInventory::decode(&unknown).is_err());
        assert!(DatabaseClaimInventory::decode(&vec![0; 65537]).is_err());
        let mut missing = inventory.slices().to_vec();
        missing.pop();
        assert!(
            DatabaseClaimInventory::new_retirement_forward(
                inventory.header().clone(),
                inventory.bindings().to_vec(),
                missing
            )
            .is_err()
        );
        let mut surplus = inventory.slices().to_vec();
        let mut extra = surplus[0].clone();
        extra.id = Uuid::new_v4();
        surplus.push(extra);
        assert!(
            DatabaseClaimInventory::new_retirement_forward(
                inventory.header().clone(),
                inventory.bindings().to_vec(),
                surplus
            )
            .is_err()
        );
        let Some(UniversalCommandArgs::SourceRegistryRetirement(args)) =
            record.universal_command_args()
        else {
            unreachable!()
        };
        let family = args.initial_forward_policy_digest().unwrap();
        let global = inventory
            .bindings()
            .iter()
            .find(|b| b.role == DatabaseRole::Global)
            .unwrap();
        for wrong_family in [false, true] {
            let mut mixed = inventory.slices().to_vec();
            let slice = mixed.iter_mut().find(|s| s.binding == global.id).unwrap();
            slice.id = crate::operation_journal::database_claims::retirement_forward_policy::slice_identity(
                inventory.header(), global, slice.claim,
                if wrong_family { &[1;32] } else { &family },
                if wrong_family { args.draining_work() } else { WorkUnitKey::new(0,4,args.draining_work().participant_id(),WorkUnitRole::GlobalLibrary) },
                slice.direction, slice.class).unwrap();
            let bad = DatabaseClaimInventory::new_retirement_forward(
                inventory.header().clone(),
                inventory.bindings().to_vec(),
                mixed,
            )
            .unwrap();
            assert!(bad.validate_record(&record).is_err());
        }
        for mutate in 0..3 {
            let mut bindings = inventory.bindings().to_vec();
            let global = bindings
                .iter_mut()
                .find(|b| b.role == DatabaseRole::Global)
                .unwrap();
            match mutate {
                0 => global.owner_generation += 1,
                1 => global.database_root.push('x'),
                _ => global.root_binding.push('x'),
            }
            let bad = DatabaseClaimInventory::new_retirement_forward(
                inventory.header().clone(),
                bindings,
                inventory.slices().to_vec(),
            )
            .unwrap();
            assert!(bad.validate_record(&record).is_err());
        }
        let mut foreign_claim = inventory.slices().to_vec();
        foreign_claim[0].claim = Uuid::new_v4();
        let bad = DatabaseClaimInventory::new_retirement_forward(
            inventory.header().clone(),
            inventory.bindings().to_vec(),
            foreign_claim,
        )
        .unwrap();
        assert!(bad.validate_record(&record).is_err());
        let mut mixed = inventory.slices().to_vec();
        mixed[0].id = Uuid::new_v4();
        let bad = DatabaseClaimInventory::new_retirement_forward(
            inventory.header().clone(),
            inventory.bindings().to_vec(),
            mixed,
        )
        .unwrap();
        assert!(bad.validate_record(&record).is_err());
        let mut changed_nonce = inventory.header().clone();
        changed_nonce.decision_nonce = Uuid::new_v4();
        let bad = DatabaseClaimInventory::new_retirement_forward(
            changed_nonce,
            inventory.bindings().to_vec(),
            inventory.slices().to_vec(),
        )
        .unwrap();
        assert!(bad.validate_record(&record).is_err());
        let mut changed_binding = inventory.bindings().to_vec();
        changed_binding
            .iter_mut()
            .find(|b| b.role == DatabaseRole::Global)
            .unwrap()
            .registration = Uuid::new_v4();
        let bad = DatabaseClaimInventory::new_retirement_forward(
            inventory.header().clone(),
            changed_binding,
            inventory.slices().to_vec(),
        )
        .unwrap();
        assert!(bad.validate_record(&record).is_err());
        let mut unequal = inventory.slices().to_vec();
        unequal
            .iter_mut()
            .find(|s| s.class == FullCapacityAllocationClass::GlobalDatabaseCommit)
            .unwrap()
            .ceiling += 4096;
        let bad = DatabaseClaimInventory::new_retirement_forward(
            inventory.header().clone(),
            inventory.bindings().to_vec(),
            unequal,
        )
        .unwrap();
        assert!(bad.validate_record(&record).is_err());
        if fault != 0 {
            store.set_insert_test_hook(move |at| {
                if fault == 1 && at == InsertTestBoundary::BeforePrecommitValidation {
                    InsertTestDirective::Fail(database_claims::error("forward rollback"))
                } else if fault == 2 && at == InsertTestBoundary::Commit {
                    InsertTestDirective::ExecuteThenFail(database_claims::error(
                        "forward lost reply",
                    ))
                } else {
                    InsertTestDirective::Continue
                }
            });
        }
        let provider = logical_provider(&record);
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
                matches!(settled.outcome(), JournalInsertOutcome::Failed(f) if f.stage==JournalInsertWriteStage::CommitOutcomeUnknown)
            );
            assert!(settled.lease().is_some());
        } else {
            assert!(settled.outcome().receipt().is_some());
        }
        let nonce = inventory.header().decision_nonce;
        let digest = inventory.digest().unwrap();
        assert_eq!(
            store
                .read_database_claim_inventory_for_nonce(nonce, digest)
                .unwrap(),
            Some((record.operation_id, inventory.clone()))
        );
        assert!(
            store
                .read_database_claim_inventory_for_nonce(nonce, [0; 32])
                .is_err()
        );
        drop(store);
        let reopened = OperationJournalStore::open(&f.owner).unwrap();
        assert_eq!(
            reopened
                .read_database_claim_inventory_for_nonce(nonce, digest)
                .unwrap(),
            Some((record.operation_id, inventory.clone()))
        );
        assert_eq!(
            reopened
                .connection
                .pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
                .unwrap(),
            JOURNAL_SCHEMA_VERSION
        );
        // Correlating digest cannot repair unequal frozen transition component budgets.
        let bad_image = bad.encode().unwrap();
        let bad_digest = bad.digest().unwrap();
        reopened.connection.execute("UPDATE operation_journal_database_binding SET inventory_digest=?1,inventory=CASE WHEN ordinal=0 THEN ?2 ELSE NULL END WHERE operation_id=?3",params![bad_digest.as_slice(),bad_image,record.operation_id.to_string()]).unwrap();
        assert!(
            reopened
                .read_database_claim_inventory_for_nonce(nonce, bad_digest)
                .is_err()
        );
    }
}
