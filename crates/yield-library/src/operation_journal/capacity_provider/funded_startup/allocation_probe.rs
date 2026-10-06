//! Normal-build original Store/provider component. No bootstrap qualification.
use super::*;
use std::hash::{Hash, Hasher};

/// Only a borrowed original Store and private original input remain outside measurement.
pub struct ProviderStartupProbe<'a> {
    store: &'a super::super::super::store::OperationJournalStore,
    input: InputPermit,
}
impl ProviderStartupProbe<'_> {
    /// Return scalar comparison evidence and a checked bound only. No ledger, snapshot,
    /// backing clone, permit, physical proof or native authority can escape.
    pub fn measure(&self, funded: bool) -> Result<(usize, bool, u64, usize), String> {
        let provider = if funded {
            CapacityProvider::unavailable_for_input(&self.input)
        } else {
            Ok(CapacityProvider::unavailable())
        }
        .map_err(|e| e.to_string())?;
        let snapshot = if funded {
            self.store
                .rebuild_ordinary_startup_snapshot_for_input(&self.input)
        } else {
            self.store
                .rebuild_ordinary_claim_snapshot()
                .map(super::super::super::store::FundedSnapshot::unfunded)
        }
        .map_err(|e| e.to_string())?;
        let debit = if funded {
            header_bytes()
                .checked_add(snapshot.collection_debit_for_test())
                .and_then(|n| {
                    n.checked_add(scratch_bytes(snapshot.claims(), &provider.ledger.borrow())?)
                })
                .and_then(|n| n.checked_add(retained_bytes(snapshot.claims())?))
                .ok_or_else(|| String::from("original startup probe bound overflow"))?
        } else {
            0
        };
        if funded {
            snapshot.install_provider_for_input(&provider, &self.input)
        } else {
            snapshot.install_provider_component_compatibility(&provider)
        }
        .map_err(|e| e.to_string())?;
        let ledger = provider.ledger.borrow();
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        ledger.next_token.hash(&mut hash);
        for (token, entry) in &ledger.charges_by_token {
            token.hash(&mut hash);
            for (key, bytes) in &entry.charges {
                key.volume.device.hash(&mut hash);
                (key.allocation_class as usize).hash(&mut hash);
                bytes.hash(&mut hash);
            }
            for (id, claim) in &entry.claims {
                id.as_uuid().hash(&mut hash);
                claim.volume.device.hash(&mut hash);
                (claim.allocation_class as usize).hash(&mut hash);
                claim.allocated_bytes.hash(&mut hash);
                claim.release_id.hash(&mut hash);
            }
            match &entry.disposition {
                ProvisionalLeaseDisposition::Durable {
                    operation_id,
                    claim_ids,
                }
                | ProvisionalLeaseDisposition::AuditPending {
                    operation_id,
                    claim_ids,
                } => {
                    matches!(
                        entry.disposition,
                        ProvisionalLeaseDisposition::AuditPending { .. }
                    )
                    .hash(&mut hash);
                    operation_id.as_uuid().hash(&mut hash);
                    for id in claim_ids {
                        id.as_uuid().hash(&mut hash);
                    }
                }
                _ => {
                    return Err(String::from(
                        "original startup probe non-reconstructed disposition",
                    ));
                }
            }
        }
        for (key, bytes) in &ledger.aggregate_charges {
            key.volume.device.hash(&mut hash);
            (key.allocation_class as usize).hash(&mut hash);
            bytes.hash(&mut hash);
        }
        for (id, claim) in &ledger.released_claims {
            id.as_uuid().hash(&mut hash);
            claim.operation_id.as_uuid().hash(&mut hash);
            claim.volume.device.hash(&mut hash);
            (claim.allocation_class as usize).hash(&mut hash);
            claim.allocated_bytes.hash(&mut hash);
            claim.release_id.hash(&mut hash);
        }
        Ok((
            snapshot.claims().len(),
            snapshot.proves_durable_zero(),
            hash.finish(),
            debit,
        ))
    }
}
impl super::super::super::store::OperationJournalStore {
    pub fn provider_startup_probe_for_test(&self) -> ProviderStartupProbe<'_> {
        ProviderStartupProbe {
            store: self,
            input: InputPermit::historical_test_input(1),
        }
    }
}
