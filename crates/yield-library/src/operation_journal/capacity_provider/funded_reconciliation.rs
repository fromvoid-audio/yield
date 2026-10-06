//! Temporary original provider reconciliation graphs, not retained startup ledger backing.
use super::super::owner::InputPermit;
use super::*;
use std::mem::size_of;

pub(super) fn reserve(
    claims: &[RebuildClaim],
    ledger: &ProvisionalLedger,
    input: &InputPermit,
) -> Result<InputPermit, CapacityProviderError> {
    let bytes = bytes(claims, ledger).ok_or(CapacityProviderError::RebuildResourceBound)?;
    input
        .reserve_historical_scratch(bytes)
        .map_err(|_| CapacityProviderError::RebuildResourceBound)
}

// A full pinned internal node per entry covers sparse/root/split occupancy. The
// key/value pair stride is at least the separate arrays' combined stride.
pub(super) fn node<K, V>() -> usize {
    12 * size_of::<(K, V)>() + 12 * size_of::<usize>() + 4 * size_of::<usize>()
}
fn add(total: &mut usize, count: usize, each: usize) -> Option<()> {
    *total = total.checked_add(count.checked_mul(each)?)?;
    Some(())
}

pub(super) fn normalized(
    operations: usize,
    claims: usize,
    charges: usize,
    aggregates: usize,
    releases: usize,
) -> Option<usize> {
    let mut total = size_of::<(
        BTreeMap<OperationId, LedgerCharge>,
        BTreeMap<ChargeKey, u64>,
        BTreeMap<AdmissionClaimId, ReleasedClaim>,
    )>();
    add(&mut total, operations, node::<OperationId, LedgerCharge>())?;
    add(&mut total, claims, node::<AdmissionClaimId, LedgerClaim>())?;
    add(&mut total, charges, node::<ChargeKey, u64>())?;
    add(&mut total, aggregates, node::<ChargeKey, u64>())?;
    add(
        &mut total,
        releases,
        node::<AdmissionClaimId, ReleasedClaim>(),
    )?;
    // Original filter_map Vec growth: initial capacity four, subsequent doubling.
    // Four elements per input covers every group, including singleton groups.
    add(&mut total, claims, 4 * size_of::<AdmissionClaimId>())?;
    Some(total)
}

pub(super) fn bytes(claims: &[RebuildClaim], ledger: &ProvisionalLedger) -> Option<usize> {
    let count = claims.len();
    let releases = claims
        .iter()
        .filter(|claim| claim.release_id().is_some())
        .count();
    let mut live_claims = 0usize;
    let mut live_charges = 0usize;
    for entry in ledger.charges_by_token.values() {
        live_claims = live_claims.checked_add(entry.claims.len())?;
        live_charges = live_charges.checked_add(entry.charges.len())?;
    }
    let mut total = size_of::<ProvisionalLedger>()
        + size_of::<ChargeMirror>()
        + 2 * size_of::<usize>()
        + size_of::<InputPermit>()
        + size_of::<BTreeMap<OperationId, Vec<&RebuildClaim>>>()
        + 2 * size_of::<BTreeSet<AdmissionClaimId>>();
    // Every operation count is bounded by its actual input rows, not a new ceiling.
    add(&mut total, count, node::<OperationId, Vec<&RebuildClaim>>())?;
    add(&mut total, count, 4 * size_of::<&RebuildClaim>())?;
    add(&mut total, count, node::<AdmissionClaimId, ()>())?;
    add(&mut total, releases, node::<Uuid, ()>())?;
    add(&mut total, count, node::<u64, LedgerCharge>())?;
    add(&mut total, count, node::<ChargeKey, u64>())?;
    add(&mut total, count, node::<AdmissionClaimId, LedgerClaim>())?;
    add(&mut total, count, node::<ChargeKey, u64>())?;
    add(
        &mut total,
        releases,
        node::<AdmissionClaimId, ReleasedClaim>(),
    )?;
    add(&mut total, count, 4 * size_of::<AdmissionClaimId>())?;
    let staged_normalized =
        normalized(count, count.checked_add(releases)?, count, count, releases)?;
    let live_normalized = normalized(
        ledger.charges_by_token.len(),
        live_claims.checked_add(ledger.released_claims.len())?,
        live_charges,
        ledger.aggregate_charges.len(),
        ledger.released_claims.len(),
    )?;
    total
        .checked_add(staged_normalized)?
        .checked_add(live_normalized)
}

/// Original-provider allocation component, prepared before measurement. No
/// qualification, witness, ledger mutation or admission authority is exposed.
#[cfg(any(test, feature = "test-support"))]
pub struct ProviderReconciliationProbe {
    snapshot: OrdinaryClaimRebuildSnapshot,
    provider: CapacityProvider,
    input: InputPermit,
}
#[cfg(any(test, feature = "test-support"))]
impl ProviderReconciliationProbe {
    /// Compare the exact original prepared snapshot/ledger; return scalar results only.
    pub fn measure(&self, funded: bool) -> (bool, usize) {
        let debit = bytes(self.snapshot.claims(), &self.provider.ledger.borrow()).unwrap();
        let result = if funded {
            self.provider
                .validate_rebuilt_claims_for_input(&self.snapshot, &self.input)
        } else {
            self.provider.validate_rebuilt_claims(&self.snapshot)
        };
        (result.is_ok(), if funded { debit } else { 0 })
    }
}
#[cfg(any(test, feature = "test-support"))]
impl super::super::store::OperationJournalStore {
    /// Prepare identical original SQLite snapshot/provider bytes outside the probe scope.
    pub fn provider_reconciliation_probe_for_test(
        &self,
    ) -> Result<ProviderReconciliationProbe, String> {
        let snapshot = self
            .rebuild_ordinary_claim_snapshot()
            .map_err(|error| error.to_string())?;
        let provider = CapacityProvider::unavailable();
        provider
            .install_rebuilt_claims(&snapshot)
            .map_err(|error| error.to_string())?;
        Ok(ProviderReconciliationProbe {
            snapshot,
            provider,
            input: InputPermit::historical_test_input(1),
        })
    }
}

#[cfg(test)]
thread_local! {
    static STAGE_PROBE: std::cell::Cell<(bool, usize)> = const { std::cell::Cell::new((false, 0)) };
}
#[cfg(test)]
pub(super) fn after_stage(input: &InputPermit) {
    STAGE_PROBE.with(|probe| {
        let (panic, _) = probe.get();
        probe.set((false, input.historical_test_remaining()));
        assert!(!panic, "original provider staging unwind fixture");
    });
}

#[cfg(test)]
mod tests;
