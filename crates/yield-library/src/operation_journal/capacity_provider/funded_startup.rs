//! Original reconstructed baseline only; future admission/release growth is separate.
use super::super::owner::InputPermit;
use super::retained_backing::Credit;
use super::*;
use std::mem::size_of;

pub(super) fn header_bytes() -> usize {
    size_of::<RefCell<ProvisionalLedger>>()
        + 2 * size_of::<usize>()
        + size_of::<ChargeMirror>()
        + 2 * size_of::<usize>()
        // A new observer is allocated before replacing one dead Weak registry slot.
        + (charge_observer::MAX_VOLUMES + 1) * (size_of::<ChargeObserver>() + 2 * size_of::<usize>())
        + size_of::<super::super::owner::StartupCredit>()
        + 2 * size_of::<usize>()
}
pub(super) fn credit(input: &InputPermit, bytes: usize) -> Result<Credit, CapacityProviderError> {
    let permit = input
        .reserve_historical_scratch(bytes)
        .map_err(|_| CapacityProviderError::RebuildResourceBound)?;
    Ok(Credit::new(permit.into_startup_credit()))
}
fn node<K, V>() -> usize {
    12 * size_of::<(K, V)>() + 16 * size_of::<usize>()
}
fn operations(claims: &[RebuildClaim]) -> usize {
    // Scalar inventory before any allocation. No identity-set scratch or new row cap.
    claims
        .iter()
        .enumerate()
        .filter(|(i, claim)| {
            !claims[..*i]
                .iter()
                .any(|prior| prior.operation_id() == claim.operation_id())
        })
        .count()
}
pub(super) fn retained_bytes(claims: &[RebuildClaim]) -> Option<usize> {
    let count = claims.len();
    let releases = claims.iter().filter(|c| c.release_id().is_some()).count();
    operations(claims)
        .checked_mul(node::<u64, LedgerCharge>())?
        .checked_add(count.checked_mul(
            node::<ChargeKey, u64>()
                + node::<AdmissionClaimId, LedgerClaim>()
                + node::<ChargeKey, u64>()
                + 4 * size_of::<AdmissionClaimId>(),
        )?)?
        .checked_add(releases.checked_mul(node::<AdmissionClaimId, ReleasedClaim>())?)?
        .checked_add(size_of::<super::super::owner::StartupCredit>())
}
fn scratch_bytes(claims: &[RebuildClaim], ledger: &ProvisionalLedger) -> Option<usize> {
    let count = claims.len();
    let releases = claims.iter().filter(|c| c.release_id().is_some()).count();
    let groups = operations(claims);
    let retained = retained_bytes(claims)?;
    let stage = retained
        .checked_add(
            size_of::<ProvisionalLedger>() + size_of::<ChargeMirror>() + 2 * size_of::<usize>(),
        )?
        .checked_add(groups.checked_mul(node::<OperationId, Vec<&RebuildClaim>>())?)?
        .checked_add(
            count.checked_mul(4 * size_of::<&RebuildClaim>() + node::<AdmissionClaimId, ()>())?,
        )?
        .checked_add(releases.checked_mul(node::<Uuid, ()>())?)?;
    let mut live_claims = 0usize;
    let mut live_charges = 0usize;
    for entry in ledger.charges_by_token.values() {
        live_claims = live_claims.checked_add(entry.claims.len())?;
        live_charges = live_charges.checked_add(entry.charges.len())?;
    }
    let normalization = funded_reconciliation::normalized(
        groups,
        count.checked_add(releases)?,
        count,
        count,
        releases,
    )?
    .checked_add(funded_reconciliation::normalized(
        ledger.charges_by_token.len(),
        live_claims.checked_add(ledger.released_claims.len())?,
        live_charges,
        ledger.aggregate_charges.len(),
        ledger.released_claims.len(),
    )?)?;
    // The first canonical ledger survives comparison and the second stage. Normalized
    // images die before the second stage starts; max names their actual overlap.
    retained
        .checked_add(stage.max(normalization))?
        .checked_add(size_of::<InputPermit>())
}
impl CapacityProvider {
    pub(in crate::operation_journal) fn unavailable_for_input(
        input: &InputPermit,
    ) -> Result<Self, CapacityProviderError> {
        let credit = credit(input, header_bytes())?;
        let mut value = ProvisionalLedger::default_without_mirror(RetainedArc::new(
            ChargeMirror::default(),
            Some(credit.clone()),
        ));
        value.next_token = INITIAL_LEASE_TOKEN;
        let ledger = RetainedRc::new(RefCell::new(value), Some(credit));
        let mirror = ledger.borrow().mirror.clone();
        Ok(Self {
            qualification: ProviderQualification::Unavailable,
            facts: None,
            ledger,
            mirror,
        })
    }
    pub(in crate::operation_journal) fn install_rebuilt_claims_for_input(
        &self,
        snapshot: &OrdinaryClaimRebuildSnapshot,
        input: &InputPermit,
    ) -> Result<(), CapacityProviderError> {
        if !snapshot.readiness().can_install() {
            return Err(CapacityProviderError::RebuildNotReady);
        }
        let mut ledger = self
            .ledger
            .try_borrow_mut()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        let scratch_bytes = scratch_bytes(snapshot.claims(), &ledger)
            .ok_or(CapacityProviderError::RebuildResourceBound)?;
        let _scratch = input
            .reserve_historical_scratch(scratch_bytes)
            .map_err(|_| CapacityProviderError::RebuildResourceBound)?;
        let canonical = stage_rebuilt_ledger(snapshot.claims(), INITIAL_LEASE_TOKEN)?;
        if ledger_matches_rebuild(&ledger, &canonical) {
            return Ok(());
        }
        if !ledger.charges_by_token.is_empty() || !ledger.released_claims.is_empty() {
            return Err(CapacityProviderError::RebuildLedgerConflict);
        }
        let bytes =
            retained_bytes(snapshot.claims()).ok_or(CapacityProviderError::RebuildResourceBound)?;
        let retained = input
            .reserve_historical_scratch(bytes)
            .map_err(|_| CapacityProviderError::RebuildResourceBound)?
            .into_startup_credit();
        let staged = stage_rebuilt_ledger(snapshot.claims(), ledger.next_token)?;
        #[cfg(test)]
        SECOND_STAGE_UNWIND.with(|probe| {
            let (panic, _) = probe.get();
            probe.set((false, input.historical_test_remaining()));
            assert!(
                !panic,
                "original funded startup second-stage unwind fixture"
            );
        });
        let mutation = ChargeMutation::begin(&ledger);
        ledger.next_token = staged.next_token;
        ledger.charges_by_token = staged.charges_by_token;
        ledger.aggregate_charges = staged.aggregate_charges;
        ledger.released_claims = staged.released_claims;
        ledger._rebuild_credit = Some(retained);
        mutation.finish(&ledger);
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static SECOND_STAGE_UNWIND: std::cell::Cell<(bool, usize)> = const { std::cell::Cell::new((false, usize::MAX)) };
}

#[cfg(test)]
mod tests;

#[cfg(any(test, feature = "test-support"))]
mod allocation_probe;
#[cfg(any(test, feature = "test-support"))]
pub use allocation_probe::ProviderStartupProbe;
