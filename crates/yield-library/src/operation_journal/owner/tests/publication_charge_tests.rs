//! Original owned SQLite/provider resolution only; fixture facts confer no native authority.
#[cfg(test)]
mod integration {
    use super::super::*;
    use crate::durable_capacity::DurableVolumeCapacity;
    use crate::operation_journal::admission_claims::{
        AdmissionClaim, AdmissionClaimId, AdmissionClaimInventory,
    };
    use crate::operation_journal::full_capacity::{
        FullCapacityAllocation, FullCapacityAllocationClass, FullCapacityPlan,
    };
    use crate::operation_journal::owner::publication_charge::{Selector, bind};

    fn funded(record: &StoredOperationRecord) -> (CapacityProvider, AdmissionClaimInventory) {
        let plan = FullCapacityPlan::from_legacy(record.capacity_plan.as_ref().unwrap()).unwrap();
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
        let facts = plan
            .allocations()
            .iter()
            .map(|row| {
                (
                    row.volume().clone(),
                    VolumeFacts {
                        identity: row.volume().clone(),
                        allocation_unit: row.allocation_unit(),
                        free_bytes: PROTECTED_FREE_SPACE_FLOOR + 1024,
                    },
                )
            })
            .collect();
        (CapacityProvider::qualified_fixture(facts), claims)
    }
    #[test]
    fn publication_charge_original_point_read_refuses_wrong_and_released_claims_without_writes() {
        let fixture = fixture("publication-charge-ordinary");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let operation = OperationId::new();
        let (record, _) = v5_copy_record(operation);
        let (provider, claims) = funded(&record);
        let settled = store
            .settle_initial_claims(&record, &provider, &claims)
            .unwrap();
        assert!(settled.outcome().receipt().is_some());
        let claim = &claims.claims()[0];
        let before = encode(&store.get(operation).unwrap().unwrap()).unwrap();
        let binding = bind(
            &store,
            &provider,
            Selector::Ordinary(claim.claim_id()),
            operation,
            claim.volume(),
            1,
        )
        .unwrap();
        assert!(binding.is_current());
        for (selector, op, volume, bytes) in [
            (
                Selector::Ordinary(AdmissionClaimId::new()),
                operation,
                claim.volume().clone(),
                1,
            ),
            (
                Selector::Ordinary(claim.claim_id()),
                OperationId::new(),
                claim.volume().clone(),
                1,
            ),
            (
                Selector::Ordinary(claim.claim_id()),
                operation,
                VolumeIdentity { device: 2 },
                1,
            ),
            (
                Selector::Ordinary(claim.claim_id()),
                operation,
                claim.volume().clone(),
                0,
            ),
            (
                Selector::Ordinary(claim.claim_id()),
                operation,
                claim.volume().clone(),
                2,
            ),
        ] {
            assert!(bind(&store, &provider, selector, op, &volume, bytes).is_err());
        }
        assert!(
            bind(
                &store,
                &CapacityProvider::unavailable(),
                Selector::Ordinary(claim.claim_id()),
                operation,
                claim.volume(),
                1
            )
            .is_err()
        );
        assert_eq!(
            encode(&store.get(operation).unwrap().unwrap()).unwrap(),
            before
        );
        let request = crate::operation_journal::store::OrdinaryClaimReleaseRequest::new(
            operation,
            claim.claim_id(),
            Uuid::new_v4(),
        )
        .unwrap();
        store.release_ordinary_claim(request, &provider).unwrap();
        assert!(
            store
                .read_active_ordinary_claim(claim.claim_id())
                .unwrap()
                .is_none()
        );
        assert!(!binding.is_current());
        assert!(
            bind(
                &store,
                &provider,
                Selector::Ordinary(claim.claim_id()),
                operation,
                claim.volume(),
                1
            )
            .is_err()
        );
        assert_eq!(
            encode(&store.get(operation).unwrap().unwrap()).unwrap(),
            before
        );
    }
    #[test]
    fn publication_charge_actual_chunk_must_be_issued_and_keep_original_durable_ordinary_row() {
        let fixture = fixture("publication-charge-chunk");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let operation = OperationId::new();
        let (record, _) = v5_copy_record(operation);
        let (provider, claims) = funded(&record);
        let settled = store
            .settle_initial_claims(&record, &provider, &claims)
            .unwrap();
        assert!(settled.outcome().receipt().is_some());
        let chunk = ChunkId::new();
        let volume = VolumeIdentity { device: 1 };
        let claim = AdmissionClaim::new(
            AdmissionClaimId::new(),
            volume.clone(),
            FullCapacityAllocationClass::DestinationStaging,
            1,
        )
        .unwrap();
        let plan = FullCapacityPlan::new(vec![
            FullCapacityAllocation::new(
                volume.clone(),
                1,
                FullCapacityAllocationClass::DestinationStaging,
                1,
                1,
                PROTECTED_FREE_SPACE_FLOOR,
            )
            .unwrap(),
        ])
        .unwrap();
        let inventory = AdmissionClaimInventory::new(vec![claim.clone()]).unwrap();
        let participant = record
            .universal_intent
            .as_ref()
            .unwrap()
            .participants()
            .descriptors()[1]
            .id();
        let request = ChunkClaimRequest::new(
            chunk,
            operation,
            claim,
            participant,
            "original-output".into(),
            0,
            1,
            1,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .unwrap();
        let lease = provider.admit(&plan, &inventory).unwrap();
        store
            .persist_chunk_claim_with_lease(&request, &provider, lease)
            .unwrap();
        assert!(
            bind(
                &store,
                &provider,
                Selector::Chunk(chunk),
                operation,
                &volume,
                1
            )
            .is_err()
        );
        store
            .issue_chunk_claim(chunk, operation, Uuid::new_v4())
            .unwrap();
        let binding = bind(
            &store,
            &provider,
            Selector::Chunk(chunk),
            operation,
            &volume,
            1,
        )
        .unwrap();
        assert!(binding.is_current());
        assert!(
            bind(
                &store,
                &provider,
                Selector::Chunk(chunk),
                OperationId::new(),
                &volume,
                1
            )
            .is_err()
        );
        drop(provider);
        assert!(!binding.is_current());
        assert_eq!(
            store.read_chunk_claim(chunk).unwrap().unwrap().state(),
            ChunkClaimState::Issued
        );
    }
    #[test]
    fn publication_charge_wrong_class_and_other_provisional_ledger_never_substitute() {
        let fixture = fixture("publication-charge-wrong-class");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let operation = OperationId::new();
        let (mut record, _) = v5_copy_record(operation);
        record
            .capacity_plan
            .as_mut()
            .unwrap()
            .volumes
            .push(DurableVolumeCapacity {
                identity: VolumeIdentity { device: 2 },
                allocation_unit: 1,
                allocation_class: CapacityAllocationClass::JournalRecord,
                logical_bytes: 1,
                allocated_bytes: 1,
                protected_free_bytes: PROTECTED_FREE_SPACE_FLOOR,
            });
        let (provider, claims) = funded(&record);
        let settled = store
            .settle_initial_claims(&record, &provider, &claims)
            .unwrap();
        assert!(settled.outcome().receipt().is_some());
        let journal = claims
            .claims()
            .iter()
            .find(|c| c.allocation_class() == FullCapacityAllocationClass::JournalRecord)
            .unwrap();
        assert!(
            bind(
                &store,
                &provider,
                Selector::Ordinary(journal.claim_id()),
                operation,
                journal.volume(),
                1
            )
            .is_err()
        );
        let staging = claims
            .claims()
            .iter()
            .find(|c| c.allocation_class() == FullCapacityAllocationClass::DestinationStaging)
            .unwrap();
        let (other, _) = funded(&record);
        let plan = FullCapacityPlan::from_legacy(record.capacity_plan.as_ref().unwrap()).unwrap();
        let mut provisional = other.admit(&plan, &claims).unwrap();
        assert!(
            bind(
                &store,
                &other,
                Selector::Ordinary(staging.claim_id()),
                operation,
                staging.volume(),
                1
            )
            .is_err()
        );
        provisional.pin_for_persistence(operation).unwrap();
        assert!(
            bind(
                &store,
                &other,
                Selector::Ordinary(staging.claim_id()),
                operation,
                staging.volume(),
                1
            )
            .is_err()
        );
        let binding = bind(
            &store,
            &provider,
            Selector::Ordinary(staging.claim_id()),
            operation,
            staging.volume(),
            1,
        )
        .unwrap();
        assert!(other.validate_publication_charge(&binding).is_err());
        provider.validate_publication_charge(&binding).unwrap();
    }
}
