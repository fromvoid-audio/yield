//! Original admission growth and exact loan lifetime through ledger/lease aliases.
use super::*;
use crate::operation_journal::owner::{InputPermit, StartupCredit};
use std::mem::size_of;

/// Prepared arithmetic-only allocation component. No provider, lease, witness or
/// physical permission can escape this diagnostic.
#[cfg(any(test, feature = "test-support"))]
pub struct AdmissionGrowthProbe {
    plan: FullCapacityPlan,
    claims: AdmissionClaimInventory,
    facts: BTreeMap<VolumeIdentity, VolumeFacts>,
}
#[cfg(any(test, feature = "test-support"))]
impl AdmissionGrowthProbe {
    /// Prepare an unqualified provider and bounded arithmetic inputs outside measurement.
    pub fn new(rows: usize) -> Self {
        assert!((1..=MAX_FULL_CAPACITY_ALLOCATIONS).contains(&rows));
        let mut facts = BTreeMap::new();
        let mut allocations = Vec::new();
        let mut claims = Vec::new();
        use FullCapacityAllocationClass as C;
        let classes = [
            C::DestinationStaging,
            C::JournalRecord,
            C::SourceDatabaseCommit,
            C::SourceWalShm,
            C::GlobalDatabaseCommit,
            C::GlobalWalShm,
            C::ExistingDestination,
            C::OperationRecoveryPayload,
        ];
        for row in 0..rows {
            let device = (row / classes.len() + 1) as u64;
            let class = classes[row % classes.len()];
            let volume = VolumeIdentity { device };
            facts.insert(
                volume.clone(),
                VolumeFacts {
                    identity: volume.clone(),
                    free_bytes: u64::MAX - 1,
                    allocation_unit: 4096,
                },
            );
            allocations.push(
                super::super::full_capacity::FullCapacityAllocation::new(
                    volume.clone(),
                    4096,
                    class,
                    4096,
                    4096,
                    PROTECTED_FREE_SPACE_FLOOR,
                )
                .unwrap(),
            );
            claims.push(AdmissionClaim::new(AdmissionClaimId::new(), volume, class, 4096).unwrap());
        }
        Self {
            plan: FullCapacityPlan::new(allocations).unwrap(),
            claims: AdmissionClaimInventory::new(claims).unwrap(),
            facts,
        }
    }

    /// Execute the original funded graph component and its disposition/drop paths.
    /// Mode 0 confirms rollback, 1 retains durable, 2 retains unknown, 3 refuses facts.
    /// Returns scalar success only; this cannot qualify native admission.
    pub fn measure(
        &self,
        funding: &super::super::owner::JournalNamespaceFundingProbe,
        mode: u8,
    ) -> bool {
        assert!(mode <= 3);
        let Ok(provider) = CapacityProvider::unavailable_for_input(&funding.input) else {
            return false;
        };
        let facts = if mode == 3 {
            &BTreeMap::new()
        } else {
            &self.facts
        };
        let result = admit(&provider, &self.plan, &self.claims, facts, &funding.input);
        let success = match result {
            Ok(mut lease) => {
                let operation = OperationId::new();
                let pinned = lease.pin_for_persistence(operation).is_ok();
                let settled = match mode {
                    0 => lease.release_after_confirmed_rollback(operation).is_ok(),
                    1 => lease.mark_durable(operation).is_ok(),
                    2 => lease.release() == Err(CapacityProviderError::LeaseRetained),
                    _ => false,
                };
                drop(lease);
                pinned && settled
            }
            Err(CapacityProviderError::FactsUnavailable { .. }) => mode == 3,
            Err(_) => false,
        };
        // Prepared input graphs are outside the measurement and outlive it.
        drop(provider);
        success
    }
}

#[derive(Debug)]
pub(super) struct GrowthCredit {
    allocation: Option<Box<Link>>,
    credit: Option<StartupCredit>,
}

#[cfg(test)]
mod tests {
    use super::super::tests::{facts, plan_and_claims};
    use super::*;
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;

    fn fixture() -> (
        CapacityProvider,
        FullCapacityPlan,
        AdmissionClaimInventory,
        BTreeMap<VolumeIdentity, VolumeFacts>,
    ) {
        // Arithmetic-only facts: this seam never constructs a live admission witness.
        let volume = VolumeIdentity { device: 812 };
        let (plan, claims) = plan_and_claims(&[(
            volume.clone(),
            FullCapacityAllocationClass::SourceDatabaseCommit,
            4096,
            4096,
        )]);
        let facts =
            BTreeMap::from([(volume, facts(812, PROTECTED_FREE_SPACE_FLOOR + 16384, 4096))]);
        (
            CapacityProvider::qualified_fixture(facts.clone()),
            plan,
            claims,
            facts,
        )
    }

    #[test]
    fn original_pool_pressure_precedes_token_and_charge_mutation() {
        let (provider, plan, claims, facts) = fixture();
        let probe = JournalNamespaceFundingProbe::new();
        let available = probe.remaining_bytes();
        let [ledger, lease, scratch] = sizes(&plan, &claims).unwrap();
        for left in [1, ledger, ledger + lease, ledger + lease + scratch - 1] {
            let held = probe
                .input
                .reserve_historical_scratch(available - left)
                .unwrap();
            let token = provider.next_token();
            assert!(matches!(
                admit(&provider, &plan, &claims, &facts, &probe.input),
                Err(CapacityProviderError::RebuildResourceBound)
            ));
            assert_eq!(provider.next_token(), token);
            assert_eq!(provider.claim_count(), 0);
            assert!(provider.ledger.borrow()._growth_credit.is_none());
            assert_eq!(probe.remaining_bytes(), left);
            drop(held);
            assert_eq!(probe.remaining_bytes(), available);
        }
    }

    #[test]
    fn refused_facts_return_every_growth_loan_without_touching_ledger() {
        let (provider, plan, claims, _) = fixture();
        let probe = JournalNamespaceFundingProbe::new();
        let available = probe.remaining_bytes();
        let token = provider.next_token();
        assert!(matches!(
            admit(&provider, &plan, &claims, &BTreeMap::new(), &probe.input),
            Err(CapacityProviderError::FactsUnavailable { .. })
        ));
        assert_eq!(provider.next_token(), token);
        assert_eq!(provider.claim_count(), 0);
        assert!(provider.ledger.borrow()._growth_credit.is_none());
        assert_eq!(probe.remaining_bytes(), available);
    }

    #[test]
    fn pinned_original_lease_keeps_growth_charged_after_input_and_provider_drop() {
        let (provider, plan, claims, facts) = fixture();
        let probe = JournalNamespaceFundingProbe::new();
        let observe = probe.pool_bytes_observer_for_test();
        let [ledger_bytes, lease_bytes, _] = sizes(&plan, &claims).unwrap();
        let available = probe.remaining_bytes();
        let mut lease = admit(&provider, &plan, &claims, &facts, &probe.input).unwrap();
        assert_eq!(
            probe.remaining_bytes(),
            available - ledger_bytes - lease_bytes
        );
        let operation = OperationId::new();
        lease.pin_for_persistence(operation).unwrap();
        drop(provider);
        drop(probe);
        let (remaining, baseline) = observe();
        assert_eq!(remaining, baseline - ledger_bytes - lease_bytes);
        assert_eq!(lease.release(), Err(CapacityProviderError::LeaseRetained));
        lease.release_after_confirmed_rollback(operation).unwrap();
        assert_eq!(observe().0, baseline - lease_bytes);
        drop(lease);
        assert_eq!(observe().0, baseline);
    }

    #[test]
    fn original_provider_alias_retains_durable_ledger_growth_after_lease_drop() {
        let (provider, plan, claims, facts) = fixture();
        let probe = JournalNamespaceFundingProbe::new();
        let available = probe.remaining_bytes();
        let [ledger_bytes, _, _] = sizes(&plan, &claims).unwrap();
        let mut lease = admit(&provider, &plan, &claims, &facts, &probe.input).unwrap();
        let operation = OperationId::new();
        lease.pin_for_persistence(operation).unwrap();
        lease.mark_durable(operation).unwrap();
        let alias = provider.ledger.clone();
        drop(lease);
        assert_eq!(provider.claim_count(), 1);
        assert_eq!(probe.remaining_bytes(), available - ledger_bytes);
        drop(provider);
        assert_eq!(probe.remaining_bytes(), available - ledger_bytes);
        drop(alias);
        assert_eq!(probe.remaining_bytes(), available);
    }

    #[test]
    fn repeated_confirmed_rollback_returns_growth_only_after_all_original_graphs_die() {
        let (provider, plan, claims, facts) = fixture();
        let probe = JournalNamespaceFundingProbe::new();
        let available = probe.remaining_bytes();
        let [_, lease_bytes, _] = sizes(&plan, &claims).unwrap();
        for _ in 0..512 {
            let mut lease = admit(&provider, &plan, &claims, &facts, &probe.input).unwrap();
            let operation = OperationId::new();
            lease.pin_for_persistence(operation).unwrap();
            lease.release_after_confirmed_rollback(operation).unwrap();
            assert_eq!(probe.remaining_bytes(), available - lease_bytes);
            assert!(provider.ledger.borrow()._growth_credit.is_none());
            drop(lease);
            assert_eq!(probe.remaining_bytes(), available);
        }
    }

    #[test]
    fn released_history_keeps_original_ledger_growth_charged_until_final_alias_drop() {
        let (provider, plan, claims, facts) = fixture();
        let probe = JournalNamespaceFundingProbe::new();
        let available = probe.remaining_bytes();
        let [ledger_bytes, _, _] = sizes(&plan, &claims).unwrap();
        let mut lease = admit(&provider, &plan, &claims, &facts, &probe.input).unwrap();
        let operation = OperationId::new();
        lease.pin_for_persistence(operation).unwrap();
        lease.mark_durable(operation).unwrap();
        let claim = &claims.claims()[0];
        let receipt = OrdinaryClaimReleaseReceipt::for_test(
            operation,
            claim.claim_id(),
            Uuid::new_v4(),
            claim.volume().clone(),
            claim.allocation_class(),
            claim.allocated_bytes(),
        );
        provider.release_claim(&receipt).unwrap();
        assert_eq!(provider.claim_count(), 0);
        assert!(!provider.ledger.borrow().released_claims.is_empty());
        drop(lease);
        let alias = provider.ledger.clone();
        drop(provider);
        assert_eq!(probe.remaining_bytes(), available - ledger_bytes);
        drop(alias);
        assert_eq!(probe.remaining_bytes(), available);
    }
}
#[derive(Debug)]
struct Link {
    next: Option<GrowthCredit>,
}
impl GrowthCredit {
    fn new(credit: StartupCredit, next: Option<Self>) -> Self {
        Self {
            allocation: Some(Box::new(Link { next })),
            credit: Some(credit),
        }
    }
    pub(super) fn take_next(&mut self) -> Option<Self> {
        self.allocation
            .as_mut()
            .expect("live original growth node")
            .next
            .take()
    }
}
impl Drop for GrowthCredit {
    fn drop(&mut self) {
        // Iterative teardown avoids a history-sized recursive Drop stack. Every
        // original link allocation is freed before its own loan returns.
        let mut allocation = self.allocation.take();
        let mut credit = self.credit.take();
        while let Some(mut link) = allocation.take() {
            let next = link.next.take();
            drop(link);
            drop(credit.take());
            if let Some(mut next) = next {
                allocation = next.allocation.take();
                credit = next.credit.take();
            }
        }
        drop(credit);
    }
}

fn add(total: &mut usize, count: usize, each: usize) -> Option<()> {
    *total = total.checked_add(count.checked_mul(each)?)?;
    Some(())
}
fn sizes(plan: &FullCapacityPlan, claims: &AdmissionClaimInventory) -> Option<[usize; 3]> {
    let rows = plan.allocations().len();
    let count = claims.claims().len();
    let node = funded_reconciliation::node::<ChargeKey, u64>();
    // Live token, its own charges/claims and aggregate/release history. A complete
    // pinned internal node per entry includes sparse roots and split occupancy.
    let mut ledger = size_of::<Link>() + funded_reconciliation::node::<u64, LedgerCharge>();
    add(&mut ledger, rows, 2 * node)?;
    add(
        &mut ledger,
        count,
        funded_reconciliation::node::<AdmissionClaimId, LedgerClaim>()
            + funded_reconciliation::node::<AdmissionClaimId, ReleasedClaim>(),
    )?;
    // Pinned/durable dispositions own original claim-id vectors; charge retention
    // can coexist with both the original and replacement disposition during update.
    add(
        &mut ledger,
        count.checked_mul(4)?.checked_add(8)?,
        size_of::<AdmissionClaimId>(),
    )?;
    let mut lease = size_of::<ProvisionalCapacityLease>();
    add(&mut lease, rows, node)?;
    add(
        &mut lease,
        count.checked_mul(6)?.checked_add(12)?,
        size_of::<AdmissionClaimId>(),
    )?;
    // Before the token is installed, plan validation, facts/volume staging and
    // FromIterator sorting can coexist with the permanent original graphs.
    let mut scratch = size_of::<FullCapacityPlan>()
        + size_of::<AdmissionClaimInventory>()
        + 4 * size_of::<BTreeMap<ChargeKey, u64>>()
        + size_of::<BTreeSet<VolumeIdentity>>();
    add(
        &mut scratch,
        rows,
        2 * node
            + funded_reconciliation::node::<VolumeIdentity, u64>()
            + funded_reconciliation::node::<VolumeIdentity, ()>(),
    )?;
    add(
        &mut scratch,
        count,
        2 * funded_reconciliation::node::<AdmissionClaimId, ()>()
            + 4 * size_of::<(AdmissionClaimId, LedgerClaim)>()
            + 4 * size_of::<AdmissionClaimId>(),
    )?;
    Some([ledger, lease, scratch])
}

pub(super) fn admit(
    provider: &CapacityProvider,
    plan: &FullCapacityPlan,
    claims: &AdmissionClaimInventory,
    facts: &BTreeMap<VolumeIdentity, VolumeFacts>,
    input: &InputPermit,
) -> Result<ProvisionalCapacityLease, CapacityProviderError> {
    if plan.allocations().len() > MAX_FULL_CAPACITY_ALLOCATIONS
        || claims.claims().len() > MAX_ADMISSION_CLAIMS
    {
        return Err(CapacityProviderError::RebuildResourceBound);
    }
    let [ledger_bytes, lease_bytes, scratch_bytes] =
        sizes(plan, claims).ok_or(CapacityProviderError::RebuildResourceBound)?;
    // Reserve every simultaneous loan before any graph or link construction.
    let ledger_credit = input
        .reserve_historical_scratch(ledger_bytes)
        .map_err(|_| CapacityProviderError::RebuildResourceBound)?;
    let lease_credit = input
        .reserve_historical_scratch(lease_bytes)
        .map_err(|_| CapacityProviderError::RebuildResourceBound)?;
    let scratch = input
        .reserve_historical_scratch(scratch_bytes)
        .map_err(|_| CapacityProviderError::RebuildResourceBound)?;
    plan.validate()
        .map_err(|_| CapacityProviderError::InvalidPlan)?;
    claims
        .validate_against(plan)
        .map_err(|_| CapacityProviderError::InvalidClaims)?;
    {
        let mut ledger = provider
            .ledger
            .try_borrow_mut()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        let previous = ledger._growth_credit.take();
        ledger._growth_credit = Some(GrowthCredit::new(
            ledger_credit.into_startup_credit(),
            previous,
        ));
    }
    // Installing the loan first also covers unwind after a ledger write. This
    // metadata-only keeper changes no token, charge, mirror or provider authority.
    let result = provider.admit_with_facts(plan, claims, facts);
    match result {
        Ok(mut lease) => {
            lease._input_credit = Some(lease_credit.into_startup_credit());
            drop(scratch);
            Ok(lease)
        }
        Err(error) => {
            let removed = {
                let mut ledger = provider
                    .ledger
                    .try_borrow_mut()
                    .map_err(|_| CapacityProviderError::LedgerBusy)?;
                let mut removed = ledger
                    ._growth_credit
                    .take()
                    .expect("original admission growth loan");
                ledger._growth_credit = removed.take_next();
                removed
            };
            drop(removed);
            drop(scratch);
            drop(lease_credit);
            Err(error)
        }
    }
}
