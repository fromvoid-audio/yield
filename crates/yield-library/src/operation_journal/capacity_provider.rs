//! Private provisional capacity accounting for the serialized journal owner.
//!
//! This module deliberately stops at an in-process ledger.  The production provider remains
//! unavailable until a retained-descriptor, non-sparse reserve primitive is integrated.  The
//! qualified fixture used by the tests supplies already-validated capacity observations; it does
//! not represent a physical reservation authority.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use super::admission_claims::{
    AdmissionClaim, AdmissionClaimId, AdmissionClaimInventory, MAX_ADMISSION_CLAIMS,
};
use super::full_capacity::{
    FullCapacityAllocationClass, FullCapacityPlan, MAX_FULL_CAPACITY_ALLOCATIONS,
};
use super::store::{
    ChunkClaimRecord, OrdinaryClaimRebuildSnapshot, OrdinaryClaimReleaseReceipt, RebuildClaim,
};
use crate::durable_capacity::{PROTECTED_FREE_SPACE_FLOOR, VolumeFacts, VolumeIdentity};
use crate::operation_journal::model::OperationId;
use uuid::Uuid;

mod charge_observer;
mod funded_admission;
#[cfg(any(test, feature = "test-support"))]
pub use funded_admission::AdmissionGrowthProbe;
mod funded_reconciliation;
mod funded_startup;
mod retained_backing;
pub(crate) use charge_observer::PublicationChargeBinding;
#[cfg(any(test, feature = "test-support"))]
pub use funded_reconciliation::ProviderReconciliationProbe;
#[cfg(any(test, feature = "test-support"))]
pub use funded_startup::ProviderStartupProbe;
use retained_backing::{RetainedArc, RetainedRc};
pub(crate) const PUBLICATION_MIRROR_BYTES: usize = 4096;
use charge_observer::{ChargeMirror, ChargeMutation, ChargeObserver};
const INITIAL_LEASE_TOKEN: u64 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderQualification {
    Unavailable,
    QualifiedFixture,
}

fn counts_against_incremental_free_space(class: FullCapacityAllocationClass) -> bool {
    !matches!(
        class,
        FullCapacityAllocationClass::ExistingDestination
            | FullCapacityAllocationClass::ExistingRecoveryPayload
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ChargeKey {
    volume: VolumeIdentity,
    allocation_class: FullCapacityAllocationClass,
}

impl Ord for ChargeKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.volume
            .cmp(&other.volume)
            .then_with(|| self.allocation_class.cmp(&other.allocation_class))
    }
}

impl PartialOrd for ChargeKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug)]
struct ProvisionalLedger {
    mirror: RetainedArc<ChargeMirror>,
    next_token: u64,
    charges_by_token: BTreeMap<u64, LedgerCharge>,
    aggregate_charges: BTreeMap<ChargeKey, u64>,
    released_claims: BTreeMap<AdmissionClaimId, ReleasedClaim>,
    #[cfg(test)]
    fail_release: bool,
    #[cfg(test)]
    fail_disposition: bool,
    _rebuild_credit: Option<super::owner::StartupCredit>,
    // Last: original dynamic loans outlive every retained ledger graph.
    _growth_credit: Option<funded_admission::GrowthCredit>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LedgerCharge {
    charges: BTreeMap<ChargeKey, u64>,
    claims: BTreeMap<AdmissionClaimId, LedgerClaim>,
    disposition: ProvisionalLeaseDisposition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LedgerClaim {
    volume: VolumeIdentity,
    allocation_class: FullCapacityAllocationClass,
    allocated_bytes: u64,
    release_id: Option<Uuid>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReleasedClaim {
    operation_id: OperationId,
    volume: VolumeIdentity,
    allocation_class: FullCapacityAllocationClass,
    allocated_bytes: u64,
    release_id: Uuid,
}

/// Private readiness produced while reconstructing the owner-thread capacity ledger.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CapacityRebuildReadiness {
    /// Every durable claim and release observation was internally consistent.
    Ready,
    /// Known claims can be retained, but an actionable ambiguity keeps the provider recovery-only.
    RecoveryOnly { diagnostics: Vec<String> },
    /// The durable snapshot is unusable as a complete capacity image; no optimistic install is
    /// permitted until the retained evidence is explicitly reconciled.
    ReconciliationRequired { diagnostics: Vec<String> },
}

impl CapacityRebuildReadiness {
    pub(crate) fn ready() -> Self {
        Self::Ready
    }

    pub(crate) fn recovery_only(diagnostics: Vec<String>) -> Self {
        Self::RecoveryOnly { diagnostics }
    }

    pub(crate) fn reconciliation_required(diagnostics: Vec<String>) -> Self {
        Self::ReconciliationRequired { diagnostics }
    }

    pub(crate) fn can_install(&self) -> bool {
        !matches!(self, Self::ReconciliationRequired { .. })
    }

    pub(crate) fn diagnostics(&self) -> &[String] {
        match self {
            Self::Ready => &[],
            Self::RecoveryOnly { diagnostics } | Self::ReconciliationRequired { diagnostics } => {
                diagnostics
            }
        }
    }
}

impl Default for ProvisionalLedger {
    fn default() -> Self {
        Self::default_without_mirror(RetainedArc::new(ChargeMirror::default(), None))
    }
}
impl ProvisionalLedger {
    fn default_without_mirror(mirror: RetainedArc<ChargeMirror>) -> Self {
        Self {
            mirror,
            next_token: INITIAL_LEASE_TOKEN,
            charges_by_token: BTreeMap::new(),
            aggregate_charges: BTreeMap::new(),
            released_claims: BTreeMap::new(),
            #[cfg(test)]
            fail_release: false,
            #[cfg(test)]
            fail_disposition: false,
            _rebuild_credit: None,
            _growth_credit: None,
        }
    }
}

#[derive(Clone, Debug)]
struct CapacityFacts {
    by_volume: BTreeMap<VolumeIdentity, VolumeFacts>,
}

impl CapacityFacts {
    fn new(by_volume: BTreeMap<VolumeIdentity, VolumeFacts>) -> Self {
        Self { by_volume }
    }
}

/// Private in-process provider state.  `Rc<RefCell<_>>` keeps this authority synchronous and
/// non-`Send`; it must remain owned by the operation-journal owner thread.
#[allow(dead_code)]
pub(crate) struct CapacityProvider {
    qualification: ProviderQualification,
    facts: Option<CapacityFacts>,
    ledger: RetainedRc<RefCell<ProvisionalLedger>>,
    mirror: RetainedArc<ChargeMirror>,
}

impl CapacityProvider {
    fn with_qualification(
        qualification: ProviderQualification,
        facts: Option<CapacityFacts>,
    ) -> Self {
        let ledger = RetainedRc::new(RefCell::new(ProvisionalLedger::default()), None);
        let mirror = ledger.borrow().mirror.clone();
        Self {
            qualification,
            facts,
            ledger,
            mirror,
        }
    }
    pub(crate) fn unavailable() -> Self {
        Self::with_qualification(ProviderQualification::Unavailable, None)
    }
    #[cfg(test)]
    pub(crate) fn outstanding_claim_count_for_test(&self) -> usize {
        self.ledger
            .borrow()
            .charges_by_token
            .values()
            .map(|entry| {
                entry
                    .claims
                    .values()
                    .filter(|claim| claim.release_id.is_none())
                    .count()
            })
            .sum()
    }

    /// Observe only an actual currently charged original-ledger volume; never native authority.
    pub(crate) fn publication_charge_observer(
        &self,
        volume: &VolumeIdentity,
    ) -> Result<RetainedArc<ChargeObserver>, CapacityProviderError> {
        let ledger = self
            .ledger
            .try_borrow()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        ChargeMirror::bind(&ledger.mirror, &ledger, volume)
    }

    /// Bind ordinary or chunk claim data to its exact confirmed durable original charge.
    /// Decoded claim data alone is insufficient; this never admits bytes or issues a permit.
    pub(crate) fn bind_publication_charge(
        &self,
        operation: OperationId,
        claim: &AdmissionClaim,
    ) -> Result<PublicationChargeBinding, CapacityProviderError> {
        let ledger = self
            .ledger
            .try_borrow()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        validate_publication_claim(&ledger, operation, claim)?;
        let observer = ChargeMirror::bind(&ledger.mirror, &ledger, claim.volume())?;
        let observation = observer
            .observation()
            .ok_or(CapacityProviderError::Unavailable)?;
        Ok(PublicationChargeBinding::new(
            operation,
            claim.clone(),
            observer,
            observation,
        ))
    }

    /// Actor-side revalidation additionally rejects a foreign provider incarnation.
    pub(crate) fn validate_publication_charge(
        &self,
        binding: &PublicationChargeBinding,
    ) -> Result<(), CapacityProviderError> {
        if !binding.belongs_to(&self.mirror) || !binding.is_current() {
            return Err(CapacityProviderError::Unavailable);
        }
        let ledger = self
            .ledger
            .try_borrow()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        validate_publication_claim(&ledger, binding.operation(), binding.claim())
    }

    /// Prove that this owner-thread ledger has no live charge that could be lost by a clean
    /// volume handoff.  Released claim identities are historical evidence and are deliberately
    /// allowed to remain; a token entry itself remains blocking even when its byte totals happen
    /// to be zero.
    pub(crate) fn prove_no_live_charges(&self) -> Result<(), CapacityProviderError> {
        let ledger = self
            .ledger
            .try_borrow()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        if !ledger.charges_by_token.is_empty()
            || ledger.aggregate_charges.values().any(|amount| *amount != 0)
        {
            return Err(CapacityProviderError::RebuildLedgerConflict);
        }
        Ok(())
    }

    /// Install a validated restart image without changing provider qualification.
    ///
    /// The staged image is grouped deterministically by operation identity.  It is committed to
    /// the owner-thread ledger only after every claim, release identity, aggregate, and token
    /// allocation has passed checked validation.  An exact image already present in the live
    /// ledger is an idempotent replay; any other live image is left untouched and rejected.
    pub(crate) fn install_rebuilt_claims(
        &self,
        snapshot: &OrdinaryClaimRebuildSnapshot,
    ) -> Result<(), CapacityProviderError> {
        if !snapshot.readiness().can_install() {
            return Err(CapacityProviderError::RebuildNotReady);
        }
        let mut ledger = self
            .ledger
            .try_borrow_mut()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        let canonical = stage_rebuilt_ledger(snapshot.claims(), INITIAL_LEASE_TOKEN)?;
        if ledger_matches_rebuild(&ledger, &canonical) {
            return Ok(());
        }
        if !ledger.charges_by_token.is_empty() || !ledger.released_claims.is_empty() {
            return Err(CapacityProviderError::RebuildLedgerConflict);
        }
        let staged = stage_rebuilt_ledger(snapshot.claims(), ledger.next_token)?;
        #[cfg(test)]
        let fail_release = ledger.fail_release;
        #[cfg(test)]
        let fail_disposition = ledger.fail_disposition;
        let mutation = ChargeMutation::begin(&ledger);
        ledger.next_token = staged.next_token;
        ledger.charges_by_token = staged.charges_by_token;
        ledger.aggregate_charges = staged.aggregate_charges;
        ledger.released_claims = staged.released_claims;
        #[cfg(test)]
        {
            ledger.fail_release = fail_release;
            ledger.fail_disposition = fail_disposition;
        }
        mutation.finish(&ledger);
        Ok(())
    }

    /// Verify that the owner-thread ledger already represents the current durable image without
    /// installing or replacing anything. A changed image is a recovery result for the caller;
    /// admission rejection must never mutate live accounting as a side effect.
    pub(crate) fn validate_rebuilt_claims(
        &self,
        snapshot: &OrdinaryClaimRebuildSnapshot,
    ) -> Result<(), CapacityProviderError> {
        if !snapshot.readiness().can_install() {
            return Err(CapacityProviderError::RebuildNotReady);
        }
        let ledger = self
            .ledger
            .try_borrow()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        let canonical = stage_rebuilt_ledger(snapshot.claims(), INITIAL_LEASE_TOKEN)?;
        if ledger_matches_rebuild(&ledger, &canonical) {
            Ok(())
        } else {
            Err(CapacityProviderError::RebuildLedgerConflict)
        }
    }

    pub(super) fn validate_rebuilt_claims_for_input(
        &self,
        snapshot: &OrdinaryClaimRebuildSnapshot,
        input: &super::owner::InputPermit,
    ) -> Result<(), CapacityProviderError> {
        if !snapshot.readiness().can_install() {
            return Err(CapacityProviderError::RebuildNotReady);
        }
        let ledger = self
            .ledger
            .try_borrow()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        // The borrowed live image cannot change between inventory, debit and comparison.
        // Declaration order keeps credit until canonical and both normalized images drop.
        let _credit = funded_reconciliation::reserve(snapshot.claims(), &ledger, input)?;
        let canonical = stage_rebuilt_ledger(snapshot.claims(), INITIAL_LEASE_TOKEN)?;
        #[cfg(test)]
        funded_reconciliation::after_stage(input);
        if ledger_matches_rebuild(&ledger, &canonical) {
            Ok(())
        } else {
            Err(CapacityProviderError::RebuildLedgerConflict)
        }
    }

    #[cfg(test)]
    pub(crate) fn reconciliation_debit_for_test(snapshot: &OrdinaryClaimRebuildSnapshot) -> usize {
        let provider = Self::unavailable();
        provider.install_rebuilt_claims(snapshot).unwrap();
        funded_reconciliation::bytes(snapshot.claims(), &provider.ledger.borrow()).unwrap()
    }

    #[cfg(test)]
    pub(crate) fn qualified_fixture(facts: BTreeMap<VolumeIdentity, VolumeFacts>) -> Self {
        Self::with_qualification(
            ProviderQualification::QualifiedFixture,
            Some(CapacityFacts::new(facts)),
        )
    }

    pub(crate) fn admit(
        &self,
        plan: &FullCapacityPlan,
        claims: &AdmissionClaimInventory,
    ) -> Result<ProvisionalCapacityLease, CapacityProviderError> {
        super::capacity_admission_telemetry::observe(|| {
            if self.qualification == ProviderQualification::Unavailable {
                return Err(CapacityProviderError::Unavailable);
            }
            let facts = self
                .facts
                .as_ref()
                .ok_or(CapacityProviderError::UnsupportedFacts)?;
            self.admit_with_facts(plan, claims, &facts.by_volume)
        })
    }

    /// Admit once against a fresh owner-created descriptor witness. The witness borrows the
    /// retained owner/profile/reserve authorities and is consumed synchronously; it cannot be
    /// queued, cloned, or reused as an admission token.
    pub(crate) fn admit_with_live_witness(
        &self,
        plan: &FullCapacityPlan,
        claims: &AdmissionClaimInventory,
        witness: super::owner::LiveAdmissionWitness<'_>,
    ) -> Result<ProvisionalCapacityLease, CapacityProviderError> {
        super::capacity_admission_telemetry::observe(|| {
            self.admit_with_facts(plan, claims, witness.facts())
        })
    }

    pub(super) fn admit_with_live_witness_for_input(
        &self,
        plan: &FullCapacityPlan,
        claims: &AdmissionClaimInventory,
        witness: super::owner::LiveAdmissionWitness<'_>,
        input: &super::owner::InputPermit,
    ) -> Result<ProvisionalCapacityLease, CapacityProviderError> {
        super::capacity_admission_telemetry::observe(|| {
            funded_admission::admit(self, plan, claims, witness.facts(), input)
        })
    }

    /// Test-only arithmetic seam. Production callers must provide an owner-created witness.
    #[cfg(test)]
    fn admit_with_fresh_facts(
        &self,
        plan: &FullCapacityPlan,
        claims: &AdmissionClaimInventory,
        facts: &BTreeMap<VolumeIdentity, VolumeFacts>,
    ) -> Result<ProvisionalCapacityLease, CapacityProviderError> {
        super::capacity_admission_telemetry::observe(|| self.admit_with_facts(plan, claims, facts))
    }

    /// Check stable claim identities before any lease token or ledger mutation is created.
    pub(crate) fn validate_claim_identities_available(
        &self,
        claims: &AdmissionClaimInventory,
    ) -> Result<(), CapacityProviderError> {
        let ledger = self
            .ledger
            .try_borrow()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        for claim in claims.claims() {
            if ledger.released_claims.contains_key(&claim.claim_id())
                || ledger
                    .charges_by_token
                    .values()
                    .any(|charge| charge.claims.contains_key(&claim.claim_id()))
            {
                return Err(CapacityProviderError::DuplicateClaim {
                    claim_id: claim.claim_id(),
                });
            }
        }
        Ok(())
    }

    /// Verify that a durable post-intent claim still owns exactly one retained provider token.
    /// Issuance is a state transition on the journal relation; it must never call admission or
    /// add a second charge for the already-held ordinary claim.
    pub(crate) fn validate_durable_chunk_claim(
        &self,
        record: &ChunkClaimRecord,
    ) -> Result<(), CapacityProviderError> {
        let ledger = self
            .ledger
            .try_borrow()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        let mut matching_tokens = Vec::new();
        for (token, entry) in &ledger.charges_by_token {
            if !matches!(
                entry.disposition,
                ProvisionalLeaseDisposition::Durable { .. }
                    | ProvisionalLeaseDisposition::AuditPending { .. }
            ) {
                continue;
            }
            let Some(claim) = entry.claims.get(&record.claim_id()) else {
                continue;
            };
            if disposition_operation(&entry.disposition) != Some(record.operation_id())
                || claim.volume != *record.volume()
                || claim.allocation_class != record.allocation_class()
                || claim.allocated_bytes != record.allocated_bytes()
                || claim.release_id.is_some()
            {
                return Err(CapacityProviderError::ClaimReleaseConflict);
            }
            matching_tokens.push(*token);
        }
        match matching_tokens.as_slice() {
            [] => Err(CapacityProviderError::ClaimReleaseNotFound),
            [_] => Ok(()),
            _ => Err(CapacityProviderError::ClaimReleaseAmbiguous),
        }
    }

    /// Revalidate an already-held chunk charge against the current descriptor observation before
    /// issuing or entering it. This deliberately performs no admission and adds no ledger bytes;
    /// it only proves that the retained charge still fits above the full protected floor.
    pub(crate) fn validate_durable_chunk_issuance(
        &self,
        record: &ChunkClaimRecord,
        witness: &super::owner::LiveAdmissionWitness<'_>,
    ) -> Result<(), CapacityProviderError> {
        self.validate_durable_chunk_claim(record)?;
        let claim = AdmissionClaim::new(
            record.claim_id(),
            record.volume().clone(),
            record.allocation_class(),
            record.allocated_bytes(),
        )
        .map_err(|_| CapacityProviderError::InvalidClaims)?;
        let binding = self.bind_publication_charge(record.operation_id(), &claim)?;
        self.validate_publication_charge(&binding)?;
        if record.protected_free_bytes() != PROTECTED_FREE_SPACE_FLOOR {
            return Err(CapacityProviderError::UnsupportedFacts);
        }
        let observed = witness.facts().get(record.volume()).ok_or_else(|| {
            CapacityProviderError::FactsUnavailable {
                volume: record.volume().clone(),
            }
        })?;
        if observed.identity != *record.volume() {
            return Err(CapacityProviderError::FactsUnavailable {
                volume: record.volume().clone(),
            });
        }
        if observed.allocation_unit == 0 {
            return Err(CapacityProviderError::UnsupportedAllocationUnit {
                volume: record.volume().clone(),
            });
        }
        if observed.allocation_unit != record.allocation_unit() {
            return Err(CapacityProviderError::UnsupportedAllocationUnit {
                volume: record.volume().clone(),
            });
        }
        if observed.free_bytes <= PROTECTED_FREE_SPACE_FLOOR {
            return Err(CapacityProviderError::RecoveryReserveLow {
                volume: record.volume().clone(),
                free_bytes: observed.free_bytes,
            });
        }
        let available = observed.free_bytes - PROTECTED_FREE_SPACE_FLOOR;
        let ledger = self
            .ledger
            .try_borrow()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        let retained = ledger
            .aggregate_charges
            .iter()
            .filter(|(key, _)| {
                key.volume == *record.volume()
                    && counts_against_incremental_free_space(key.allocation_class)
            })
            .try_fold(0u64, |total, (_, bytes)| total.checked_add(*bytes))
            .ok_or(CapacityProviderError::ArithmeticOverflow)?;
        if retained > available {
            return Err(CapacityProviderError::DiskPressureRecoveryOnly {
                volume: record.volume().clone(),
                required_bytes: retained,
                available_bytes: available,
            });
        }
        Ok(())
    }

    /// Reject admission while a previous persistence outcome is still audit-pending. Durable
    /// ordinary charges may coexist with a new request; an uncertain write cannot.
    pub(crate) fn validate_recovery_state(&self) -> Result<(), CapacityProviderError> {
        let ledger = self
            .ledger
            .try_borrow()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        if ledger.charges_by_token.values().any(|charge| {
            matches!(
                charge.disposition,
                ProvisionalLeaseDisposition::AuditPending { .. }
            )
        }) {
            return Err(CapacityProviderError::RecoveryRequired);
        }
        Ok(())
    }

    fn admit_with_facts(
        &self,
        plan: &FullCapacityPlan,
        claims: &AdmissionClaimInventory,
        facts: &BTreeMap<VolumeIdentity, VolumeFacts>,
    ) -> Result<ProvisionalCapacityLease, CapacityProviderError> {
        if plan.allocations().len() > MAX_FULL_CAPACITY_ALLOCATIONS {
            return Err(CapacityProviderError::InvalidPlan);
        }
        if plan
            .allocations()
            .iter()
            .any(|allocation| allocation.protected_free_bytes() != PROTECTED_FREE_SPACE_FLOOR)
        {
            return Err(CapacityProviderError::UnsupportedFacts);
        }
        plan.validate()
            .map_err(|_| CapacityProviderError::InvalidPlan)?;
        claims
            .validate_against(plan)
            .map_err(|_| CapacityProviderError::InvalidClaims)?;

        let mut charge_by_key = BTreeMap::new();
        let mut planned_by_volume = BTreeMap::new();
        let mut seen_volumes = BTreeSet::new();
        for allocation in plan.allocations() {
            let volume = allocation.volume().clone();
            let observed =
                facts
                    .get(&volume)
                    .ok_or_else(|| CapacityProviderError::FactsUnavailable {
                        volume: volume.clone(),
                    })?;
            if observed.identity != volume {
                return Err(CapacityProviderError::UnsupportedFacts);
            }
            if observed.allocation_unit == 0
                || observed.allocation_unit != allocation.allocation_unit()
            {
                return Err(CapacityProviderError::UnsupportedAllocationUnit { volume });
            }
            seen_volumes.insert(volume.clone());
            let key = ChargeKey {
                volume: volume.clone(),
                allocation_class: allocation.class(),
            };
            if charge_by_key
                .insert(key, allocation.allocated_bytes())
                .is_some()
            {
                return Err(CapacityProviderError::InvalidPlan);
            }
            if counts_against_incremental_free_space(allocation.class()) {
                let planned = planned_by_volume.entry(volume).or_insert(0_u64);
                *planned = planned
                    .checked_add(allocation.allocated_bytes())
                    .ok_or(CapacityProviderError::ArithmeticOverflow)?;
            }
        }

        let mut ledger = self
            .ledger
            .try_borrow_mut()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        if ledger.charges_by_token.len() >= MAX_ADMISSION_CLAIMS {
            return Err(CapacityProviderError::ClaimInventoryExhausted);
        }
        let next_token = ledger
            .next_token
            .checked_add(1)
            .ok_or(CapacityProviderError::LeaseTokenExhausted)?;

        for volume in seen_volumes {
            let observed = facts
                .get(&volume)
                .ok_or(CapacityProviderError::UnsupportedFacts)?;
            if observed.free_bytes <= PROTECTED_FREE_SPACE_FLOOR {
                return Err(CapacityProviderError::RecoveryReserveLow {
                    volume,
                    free_bytes: observed.free_bytes,
                });
            }
            let existing = ledger
                .aggregate_charges
                .iter()
                .filter(|(key, _)| {
                    key.volume == volume
                        && counts_against_incremental_free_space(key.allocation_class)
                })
                .try_fold(0_u64, |sum, (_, bytes)| {
                    sum.checked_add(*bytes)
                        .ok_or(CapacityProviderError::ArithmeticOverflow)
                })?;
            let required = existing
                .checked_add(planned_by_volume.get(&volume).copied().unwrap_or(0))
                .ok_or(CapacityProviderError::ArithmeticOverflow)?;
            let available = observed
                .free_bytes
                .checked_sub(PROTECTED_FREE_SPACE_FLOOR)
                .ok_or(CapacityProviderError::RecoveryReserveLow {
                    volume: volume.clone(),
                    free_bytes: observed.free_bytes,
                })?;
            if required > available {
                return Err(CapacityProviderError::DiskPressureRecoveryOnly {
                    volume,
                    required_bytes: required,
                    available_bytes: available,
                });
            }
        }

        for (key, bytes) in &charge_by_key {
            let existing = ledger.aggregate_charges.get(key).copied().unwrap_or(0);
            existing
                .checked_add(*bytes)
                .ok_or(CapacityProviderError::ArithmeticOverflow)?;
        }
        let mutation = ChargeMutation::begin(&ledger);
        ledger.next_token = next_token;
        for (key, bytes) in &charge_by_key {
            let aggregate = ledger.aggregate_charges.entry(key.clone()).or_insert(0);
            *aggregate = aggregate
                .checked_add(*bytes)
                .expect("provisional aggregate was checked before mutation");
        }
        let claim_by_id = claims
            .claims()
            .iter()
            .map(|claim| {
                (
                    claim.claim_id(),
                    LedgerClaim {
                        volume: claim.volume().clone(),
                        allocation_class: claim.allocation_class(),
                        allocated_bytes: claim.allocated_bytes(),
                        release_id: None,
                    },
                )
            })
            .collect();
        ledger.charges_by_token.insert(
            next_token - 1,
            LedgerCharge {
                charges: charge_by_key.clone(),
                claims: claim_by_id,
                disposition: ProvisionalLeaseDisposition::Provisional,
            },
        );

        let claim_ids = claims
            .claims()
            .iter()
            .map(AdmissionClaim::claim_id)
            .collect();

        mutation.finish(&ledger);
        Ok(ProvisionalCapacityLease {
            ledger: self.ledger.clone(),
            token: next_token - 1,
            charges: charge_by_key,
            claim_ids,
            disposition: ProvisionalLeaseDisposition::Provisional,
            _input_credit: None,
        })
    }

    /// Apply one already-acknowledged ordinary-claim release to the retained owner-thread ledger.
    ///
    /// A claim identity must map to exactly one retained token, and all metadata must match the
    /// durable receipt before any aggregate byte is decremented. Successful releases remain in a
    /// private history so replay is idempotent even after the final token is removed.
    pub(crate) fn release_claim(
        &self,
        receipt: &OrdinaryClaimReleaseReceipt,
    ) -> Result<(), CapacityProviderError> {
        use super::capacity_retention_telemetry::{CapacityLedgerCall as K, observe};
        observe(K::OrdinaryClaimRelease, || {
            self.release_claim_original(receipt)
        })
    }

    fn release_claim_original(
        &self,
        receipt: &OrdinaryClaimReleaseReceipt,
    ) -> Result<(), CapacityProviderError> {
        let mut ledger = self
            .ledger
            .try_borrow_mut()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        if receipt.allocated_bytes() == 0 || receipt.release_id().is_nil() {
            return Err(CapacityProviderError::ClaimReleaseConflict);
        }
        let released_history = ledger.released_claims.get(&receipt.claim_id()).cloned();
        let release_id_in_use = ledger
            .released_claims
            .values()
            .any(|released| released.release_id == receipt.release_id());
        #[cfg(test)]
        if ledger.fail_release {
            return Err(CapacityProviderError::ReleaseFailed);
        }

        let mut matching_tokens = Vec::new();
        let mut matching_metadata = None;
        let mut unreleased_tokens = Vec::new();
        for (token, entry) in &ledger.charges_by_token {
            let retained = matches!(
                entry.disposition,
                ProvisionalLeaseDisposition::Durable { .. }
                    | ProvisionalLeaseDisposition::AuditPending { .. }
            );
            if !retained {
                continue;
            }
            let Some(claim) = entry.claims.get(&receipt.claim_id()) else {
                continue;
            };
            matching_tokens.push(*token);
            if claim.release_id.is_none() {
                unreleased_tokens.push(*token);
            }
            if claim.volume == *receipt.volume()
                && claim.allocation_class == receipt.allocation_class()
                && claim.allocated_bytes == receipt.allocated_bytes()
                && claim.release_id.is_none()
                && disposition_operation(&entry.disposition) == Some(receipt.operation_id())
            {
                matching_metadata = Some((
                    *token,
                    ChargeKey {
                        volume: claim.volume.clone(),
                        allocation_class: claim.allocation_class,
                    },
                    claim.allocated_bytes,
                ));
            }
        }
        if let Some(released) = released_history {
            if released.operation_id != receipt.operation_id()
                || released.volume != *receipt.volume()
                || released.allocation_class != receipt.allocation_class()
                || released.allocated_bytes != receipt.allocated_bytes()
                || released.release_id != receipt.release_id()
            {
                return Err(CapacityProviderError::ClaimReleaseConflict);
            }
            if matching_tokens.len() > 1 || !unreleased_tokens.is_empty() {
                return Err(if matching_tokens.len() > 1 {
                    CapacityProviderError::ClaimReleaseAmbiguous
                } else {
                    CapacityProviderError::ClaimReleaseConflict
                });
            }
            return Ok(());
        }
        if release_id_in_use {
            return Err(CapacityProviderError::ClaimReleaseConflict);
        }
        if matching_tokens.is_empty() {
            return Err(CapacityProviderError::ClaimReleaseNotFound);
        }
        if matching_tokens.len() != 1 {
            return Err(CapacityProviderError::ClaimReleaseAmbiguous);
        }
        let Some((token, key, bytes)) = matching_metadata else {
            return Err(CapacityProviderError::ClaimReleaseConflict);
        };

        {
            let entry = ledger
                .charges_by_token
                .get(&token)
                .ok_or(CapacityProviderError::ClaimReleaseNotFound)?;
            let claim = entry
                .claims
                .get(&receipt.claim_id())
                .ok_or(CapacityProviderError::ClaimReleaseNotFound)?;
            if claim.release_id.is_some()
                || claim.volume != *receipt.volume()
                || claim.allocation_class != receipt.allocation_class()
                || claim.allocated_bytes != bytes
                || disposition_operation(&entry.disposition) != Some(receipt.operation_id())
            {
                return Err(CapacityProviderError::ClaimReleaseConflict);
            }
            let current = entry
                .charges
                .get(&key)
                .copied()
                .ok_or(CapacityProviderError::ReleaseFailed)?;
            if current < bytes {
                return Err(CapacityProviderError::ReleaseFailed);
            }
        }
        let aggregate = ledger
            .aggregate_charges
            .get(&key)
            .copied()
            .ok_or(CapacityProviderError::ReleaseFailed)?;
        if aggregate < bytes {
            return Err(CapacityProviderError::ReleaseFailed);
        }

        let mutation = ChargeMutation::begin(&ledger);
        {
            let current = ledger
                .aggregate_charges
                .get_mut(&key)
                .expect("validated retained aggregate");
            *current -= bytes;
            if *current == 0 {
                ledger.aggregate_charges.remove(&key);
            }
        }
        let all_released = {
            let entry = ledger
                .charges_by_token
                .get_mut(&token)
                .expect("validated retained token");
            let current = entry
                .charges
                .get_mut(&key)
                .expect("validated retained token charge");
            *current -= bytes;
            if *current == 0 {
                entry.charges.remove(&key);
            }
            let claim = entry
                .claims
                .get_mut(&receipt.claim_id())
                .expect("validated retained claim");
            claim.release_id = Some(receipt.release_id());
            let all_released = entry
                .claims
                .values()
                .all(|claim| claim.release_id.is_some());
            if !all_released {
                let mut remaining_claim_ids = entry
                    .claims
                    .iter()
                    .filter_map(|(claim_id, claim)| claim.release_id.is_none().then_some(*claim_id))
                    .collect::<Vec<_>>();
                remaining_claim_ids.sort();
                entry.disposition = match &entry.disposition {
                    ProvisionalLeaseDisposition::Durable { operation_id, .. } => {
                        ProvisionalLeaseDisposition::Durable {
                            operation_id: *operation_id,
                            claim_ids: remaining_claim_ids,
                        }
                    }
                    ProvisionalLeaseDisposition::AuditPending { operation_id, .. } => {
                        ProvisionalLeaseDisposition::AuditPending {
                            operation_id: *operation_id,
                            claim_ids: remaining_claim_ids,
                        }
                    }
                    disposition => disposition.clone(),
                };
            }
            all_released
        };
        ledger.released_claims.insert(
            receipt.claim_id(),
            ReleasedClaim {
                operation_id: receipt.operation_id(),
                volume: receipt.volume().clone(),
                allocation_class: receipt.allocation_class(),
                allocated_bytes: receipt.allocated_bytes(),
                release_id: receipt.release_id(),
            },
        );
        if all_released {
            ledger.charges_by_token.remove(&token);
        }
        mutation.finish(&ledger);
        Ok(())
    }

    fn admission_gate(&self) -> Result<(), ()> {
        match self.qualification {
            ProviderQualification::Unavailable => Err(()),
            ProviderQualification::QualifiedFixture => Ok(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn outstanding(
        &self,
        volume: &VolumeIdentity,
        class: FullCapacityAllocationClass,
    ) -> u64 {
        self.ledger
            .borrow()
            .aggregate_charges
            .get(&ChargeKey {
                volume: volume.clone(),
                allocation_class: class,
            })
            .copied()
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn claim_count(&self) -> usize {
        self.ledger.borrow().charges_by_token.len()
    }

    #[cfg(test)]
    pub(crate) fn ledger_claims(
        &self,
    ) -> Vec<(
        AdmissionClaimId,
        VolumeIdentity,
        FullCapacityAllocationClass,
        u64,
    )> {
        let mut claims: Vec<_> = self
            .ledger
            .borrow()
            .charges_by_token
            .values()
            .flat_map(|charge| {
                charge.claims.iter().map(|(claim_id, claim)| {
                    (
                        *claim_id,
                        claim.volume.clone(),
                        claim.allocation_class,
                        claim.allocated_bytes,
                    )
                })
            })
            .collect();
        claims.sort_by_key(|row| row.0);
        claims
    }

    #[cfg(test)]
    pub(crate) fn ledger_dispositions(&self) -> Vec<ProvisionalLeaseDisposition> {
        self.ledger
            .borrow()
            .charges_by_token
            .values()
            .map(|entry| entry.disposition.clone())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn next_token(&self) -> u64 {
        self.ledger.borrow().next_token
    }

    #[cfg(test)]
    pub(crate) fn fail_release_for_test(&self) {
        self.ledger.borrow_mut().fail_release = true;
    }

    #[cfg(test)]
    pub(crate) fn allow_release_for_test(&self) {
        self.ledger.borrow_mut().fail_release = false;
    }

    #[cfg(test)]
    pub(crate) fn fail_disposition_for_test(&self) {
        self.ledger.borrow_mut().fail_disposition = true;
    }

    #[cfg(test)]
    pub(crate) fn allow_disposition_for_test(&self) {
        self.ledger.borrow_mut().fail_disposition = false;
    }
}

#[cfg(test)]
pub(super) fn funded_startup_header_for_test() -> usize {
    funded_startup::header_bytes()
}

impl Drop for CapacityProvider {
    fn drop(&mut self) {
        // Observer custody cannot outlive the original actor/provider incarnation.
        self.mirror.close();
    }
}

fn validate_publication_claim(
    ledger: &ProvisionalLedger,
    operation: OperationId,
    expected: &AdmissionClaim,
) -> Result<(), CapacityProviderError> {
    let mut matches = ledger
        .charges_by_token
        .values()
        .filter(|entry| entry.claims.contains_key(&expected.claim_id()));
    let entry = matches
        .next()
        .ok_or(CapacityProviderError::ClaimReleaseNotFound)?;
    if matches.next().is_some() {
        return Err(CapacityProviderError::ClaimReleaseAmbiguous);
    }
    let claim = &entry.claims[&expected.claim_id()];
    if !matches!(&entry.disposition, ProvisionalLeaseDisposition::Durable {
        operation_id, claim_ids
    } if *operation_id == operation && claim_ids.contains(&expected.claim_id()))
        || claim.volume != *expected.volume()
        || claim.allocation_class != expected.allocation_class()
        || claim.allocated_bytes != expected.allocated_bytes()
        || claim.release_id.is_some()
        || ledger.released_claims.contains_key(&expected.claim_id())
    {
        return Err(CapacityProviderError::ClaimReleaseConflict);
    }
    Ok(())
}

fn stage_rebuilt_ledger(
    claims: &[RebuildClaim],
    initial_next_token: u64,
) -> Result<ProvisionalLedger, CapacityProviderError> {
    if initial_next_token == 0 {
        return Err(CapacityProviderError::LeaseTokenExhausted);
    }
    let mut by_operation: BTreeMap<OperationId, Vec<&RebuildClaim>> = BTreeMap::new();
    let mut seen_claims = BTreeSet::new();
    let mut seen_release_ids = BTreeSet::new();
    for claim in claims {
        if claim.operation_id().as_uuid().is_nil()
            || claim.claim_id().validate().is_err()
            || claim.allocated_bytes() == 0
        {
            return Err(CapacityProviderError::RebuildInvalidClaim);
        }
        if !seen_claims.insert(claim.claim_id()) {
            return Err(CapacityProviderError::RebuildIdentityConflict);
        }
        if let Some(release_id) = claim.release_id() {
            if release_id.is_nil() || !seen_release_ids.insert(release_id) {
                return Err(CapacityProviderError::RebuildIdentityConflict);
            }
        }
        by_operation
            .entry(claim.operation_id())
            .or_default()
            .push(claim);
    }

    let mut staged = ProvisionalLedger {
        mirror: RetainedArc::new(ChargeMirror::default(), None),
        next_token: initial_next_token,
        charges_by_token: BTreeMap::new(),
        aggregate_charges: BTreeMap::new(),
        released_claims: BTreeMap::new(),
        #[cfg(test)]
        fail_release: false,
        #[cfg(test)]
        fail_disposition: false,
        _rebuild_credit: None,
        _growth_credit: None,
    };

    for (operation_id, operation_claims) in by_operation {
        let mut charges = BTreeMap::new();
        let mut retained_claims = BTreeMap::new();
        let mut claim_ids = Vec::new();
        let mut audit_pending = false;
        let mut has_unreleased_claim = false;
        for claim in operation_claims {
            if let Some(release_id) = claim.release_id() {
                staged.released_claims.insert(
                    claim.claim_id(),
                    ReleasedClaim {
                        operation_id,
                        volume: claim.volume().clone(),
                        allocation_class: claim.allocation_class(),
                        allocated_bytes: claim.allocated_bytes(),
                        release_id,
                    },
                );
                retained_claims.insert(
                    claim.claim_id(),
                    LedgerClaim {
                        volume: claim.volume().clone(),
                        allocation_class: claim.allocation_class(),
                        allocated_bytes: claim.allocated_bytes(),
                        release_id: Some(release_id),
                    },
                );
                continue;
            }
            has_unreleased_claim = true;
            let key = ChargeKey {
                volume: claim.volume().clone(),
                allocation_class: claim.allocation_class(),
            };
            let aggregate = charges.entry(key.clone()).or_insert(0_u64);
            *aggregate = (*aggregate)
                .checked_add(claim.allocated_bytes())
                .ok_or(CapacityProviderError::ArithmeticOverflow)?;
            let provider_aggregate = staged.aggregate_charges.entry(key.clone()).or_insert(0);
            *provider_aggregate = provider_aggregate
                .checked_add(claim.allocated_bytes())
                .ok_or(CapacityProviderError::ArithmeticOverflow)?;
            retained_claims.insert(
                claim.claim_id(),
                LedgerClaim {
                    volume: claim.volume().clone(),
                    allocation_class: claim.allocation_class(),
                    allocated_bytes: claim.allocated_bytes(),
                    release_id: None,
                },
            );
            claim_ids.push(claim.claim_id());
            audit_pending |= claim.audit_pending();
        }
        if !has_unreleased_claim {
            continue;
        }
        claim_ids.sort_unstable();
        let token = staged.next_token;
        staged.next_token = staged
            .next_token
            .checked_add(1)
            .ok_or(CapacityProviderError::LeaseTokenExhausted)?;
        staged.charges_by_token.insert(
            token,
            LedgerCharge {
                charges,
                claims: retained_claims,
                disposition: if audit_pending {
                    ProvisionalLeaseDisposition::AuditPending {
                        operation_id,
                        claim_ids,
                    }
                } else {
                    ProvisionalLeaseDisposition::Durable {
                        operation_id,
                        claim_ids,
                    }
                },
            },
        );
    }
    Ok(staged)
}

fn ledger_matches_rebuild(current: &ProvisionalLedger, staged: &ProvisionalLedger) -> bool {
    match (
        normalized_ledger_image(current),
        normalized_ledger_image(staged),
    ) {
        (Some(current), Some(staged)) => current == staged,
        _ => false,
    }
}

/// Compare durable accounting by operation and claim identity rather than by provisional token.
/// Post-intent chunk claims may be admitted as separate owner-thread tokens, while the bounded
/// restart image intentionally groups every retained ordinary claim for one operation together.
/// A provisional or otherwise non-durable token is never normalized and therefore cannot be
/// mistaken for a durable restart image.
fn normalized_ledger_image(
    ledger: &ProvisionalLedger,
) -> Option<(
    BTreeMap<OperationId, LedgerCharge>,
    BTreeMap<ChargeKey, u64>,
    BTreeMap<AdmissionClaimId, ReleasedClaim>,
)> {
    let mut by_operation: BTreeMap<OperationId, LedgerCharge> = BTreeMap::new();
    for entry in ledger.charges_by_token.values() {
        let operation_id = match entry.disposition {
            ProvisionalLeaseDisposition::Durable { operation_id, .. }
            | ProvisionalLeaseDisposition::AuditPending { operation_id, .. } => operation_id,
            ProvisionalLeaseDisposition::Provisional | ProvisionalLeaseDisposition::Released => {
                return None;
            }
        };
        let target = by_operation
            .entry(operation_id)
            .or_insert_with(|| LedgerCharge {
                charges: BTreeMap::new(),
                claims: BTreeMap::new(),
                disposition: ProvisionalLeaseDisposition::Durable {
                    operation_id,
                    claim_ids: Vec::new(),
                },
            });
        let pending = matches!(
            entry.disposition,
            ProvisionalLeaseDisposition::AuditPending { .. }
        );
        if pending {
            target.disposition = ProvisionalLeaseDisposition::AuditPending {
                operation_id,
                claim_ids: Vec::new(),
            };
        }
        for (key, bytes) in &entry.charges {
            let aggregate = target.charges.entry(key.clone()).or_insert(0);
            *aggregate = aggregate.checked_add(*bytes)?;
        }
        for (claim_id, claim) in &entry.claims {
            if target.claims.insert(*claim_id, claim.clone()).is_some() {
                return None;
            }
        }
    }
    for (claim_id, released) in &ledger.released_claims {
        let Some(target) = by_operation.get_mut(&released.operation_id) else {
            continue;
        };
        let released_claim = LedgerClaim {
            volume: released.volume.clone(),
            allocation_class: released.allocation_class,
            allocated_bytes: released.allocated_bytes,
            release_id: Some(released.release_id),
        };
        if let Some(previous) = target.claims.insert(*claim_id, released_claim.clone()) {
            if previous != released_claim {
                return None;
            }
        }
    }
    for charge in by_operation.values_mut() {
        let operation_id = disposition_operation(&charge.disposition)?;
        let mut claim_ids = charge
            .claims
            .iter()
            .filter_map(|(claim_id, claim)| claim.release_id.is_none().then_some(*claim_id))
            .collect::<Vec<_>>();
        claim_ids.sort_unstable();
        charge.disposition = if matches!(
            charge.disposition,
            ProvisionalLeaseDisposition::AuditPending { .. }
        ) {
            ProvisionalLeaseDisposition::AuditPending {
                operation_id,
                claim_ids,
            }
        } else {
            ProvisionalLeaseDisposition::Durable {
                operation_id,
                claim_ids,
            }
        };
    }
    Some((
        by_operation,
        ledger.aggregate_charges.clone(),
        ledger.released_claims.clone(),
    ))
}

pub(crate) fn production_admission_gate() -> Result<(), ()> {
    CapacityProvider::unavailable().admission_gate()
}

/// A synchronous, non-serializable owner-thread charge. It can be bound privately to the exact
/// persisted operation and claim identities by the settlement helper; generic transfer remains
/// unavailable in this milestone.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct ProvisionalCapacityLease {
    ledger: RetainedRc<RefCell<ProvisionalLedger>>,
    token: u64,
    charges: BTreeMap<ChargeKey, u64>,
    claim_ids: Vec<AdmissionClaimId>,
    disposition: ProvisionalLeaseDisposition,
    // Last: maps and all original/replacement claim-id vectors drop first.
    _input_credit: Option<super::owner::StartupCredit>,
}

/// The owner-thread disposition of one provisional ledger token.
///
/// Durable and audit-pending entries intentionally remain in the provisional ledger until the
/// later ordinary-claim release or restart-rebuild milestone.  Dropping their handle therefore
/// cannot make capacity appear available prematurely.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProvisionalLeaseDisposition {
    /// The token may be released only after a confirmed pre-intent rollback.
    Provisional,
    /// The exact claim identities and operation now own this retained charge.
    Durable {
        operation_id: OperationId,
        claim_ids: Vec<AdmissionClaimId>,
    },
    /// Persistence may have committed; retain the charge for later audit/reconciliation.
    AuditPending {
        operation_id: OperationId,
        claim_ids: Vec<AdmissionClaimId>,
    },
    /// The provisional token was successfully released.
    Released,
}

impl ProvisionalCapacityLease {
    /// Verify that this exact owner-thread lease still binds the supplied immutable plan and
    /// claim inventory before a caller consumes it for durable settlement.
    pub(crate) fn validate_against(
        &self,
        provider: &CapacityProvider,
        plan: &FullCapacityPlan,
        claims: &AdmissionClaimInventory,
    ) -> Result<(), CapacityProviderError> {
        if !RetainedRc::ptr_eq(&self.ledger, &provider.ledger) {
            return Err(CapacityProviderError::LeaseDispositionConflict);
        }
        if !matches!(self.disposition, ProvisionalLeaseDisposition::Provisional) {
            return Err(CapacityProviderError::LeaseDispositionConflict);
        }
        plan.validate()
            .map_err(|_| CapacityProviderError::InvalidPlan)?;
        claims
            .validate_against(plan)
            .map_err(|_| CapacityProviderError::InvalidClaims)?;
        let expected = plan
            .allocations()
            .iter()
            .map(|allocation| {
                (
                    ChargeKey {
                        volume: allocation.volume().clone(),
                        allocation_class: allocation.class(),
                    },
                    allocation.allocated_bytes(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        if expected != self.charges {
            return Err(CapacityProviderError::LeaseDispositionConflict);
        }
        let mut expected_claim_ids = claims
            .claims()
            .iter()
            .map(AdmissionClaim::claim_id)
            .collect::<Vec<_>>();
        expected_claim_ids.sort();
        let mut actual_claim_ids = self.claim_ids.clone();
        actual_claim_ids.sort();
        if expected_claim_ids != actual_claim_ids {
            return Err(CapacityProviderError::LeaseDispositionConflict);
        }
        let ledger = self
            .ledger
            .try_borrow()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        let Some(entry) = ledger.charges_by_token.get(&self.token) else {
            return Err(CapacityProviderError::LeaseDispositionConflict);
        };
        if entry.charges != self.charges
            || entry.claims.len() != claims.claims().len()
            || claims.claims().iter().any(|claim| {
                entry
                    .claims
                    .get(&claim.claim_id())
                    .map_or(true, |retained| {
                        retained.volume != *claim.volume()
                            || retained.allocation_class != claim.allocation_class()
                            || retained.allocated_bytes != claim.allocated_bytes()
                            || retained.release_id.is_some()
                    })
            })
        {
            return Err(CapacityProviderError::LeaseDispositionConflict);
        }
        Ok(())
    }

    pub(crate) fn release(&mut self) -> Result<(), CapacityProviderError> {
        use super::capacity_retention_telemetry::{CapacityLedgerCall as K, observe};
        observe(K::ProvisionalRelease, || self.release_original())
    }

    fn release_original(&mut self) -> Result<(), CapacityProviderError> {
        match &self.disposition {
            ProvisionalLeaseDisposition::Released => return Ok(()),
            ProvisionalLeaseDisposition::Provisional => {}
            ProvisionalLeaseDisposition::Durable { .. }
            | ProvisionalLeaseDisposition::AuditPending { .. } => {
                return Err(CapacityProviderError::LeaseRetained);
            }
        }
        let mut ledger = self
            .ledger
            .try_borrow_mut()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        #[cfg(test)]
        if ledger.fail_release {
            return Err(CapacityProviderError::ReleaseFailed);
        }
        let Some(entry) = ledger.charges_by_token.get(&self.token) else {
            return Err(CapacityProviderError::ReleaseFailed);
        };
        match &entry.disposition {
            ProvisionalLeaseDisposition::Provisional => {}
            ProvisionalLeaseDisposition::Durable { .. }
            | ProvisionalLeaseDisposition::AuditPending { .. } => {
                return Err(CapacityProviderError::LeaseRetained);
            }
            ProvisionalLeaseDisposition::Released => {
                return Err(CapacityProviderError::ReleaseFailed);
            }
        }
        if entry.charges != self.charges {
            return Err(CapacityProviderError::ReleaseFailed);
        }
        for (key, bytes) in &self.charges {
            let Some(current) = ledger.aggregate_charges.get(key).copied() else {
                return Err(CapacityProviderError::ReleaseFailed);
            };
            if current < *bytes {
                return Err(CapacityProviderError::ReleaseFailed);
            }
        }
        let mutation = ChargeMutation::begin(&ledger);
        for (key, bytes) in &self.charges {
            let current = ledger
                .aggregate_charges
                .get_mut(key)
                .expect("validated provisional aggregate");
            *current -= *bytes;
            if *current == 0 {
                ledger.aggregate_charges.remove(key);
            }
        }
        ledger.charges_by_token.remove(&self.token);
        self.disposition = ProvisionalLeaseDisposition::Released;
        mutation.finish(&ledger);
        Ok(())
    }

    pub(crate) fn mark_durable(
        &mut self,
        operation_id: OperationId,
    ) -> Result<(), CapacityProviderError> {
        self.bind_disposition(operation_id, false)
    }

    pub(crate) fn mark_audit_pending(
        &mut self,
        operation_id: OperationId,
    ) -> Result<(), CapacityProviderError> {
        self.bind_disposition(operation_id, true)
    }

    /// Pin this lease before entering a transaction whose durable outcome is not yet known.
    pub(crate) fn pin_for_persistence(
        &mut self,
        operation_id: OperationId,
    ) -> Result<(), CapacityProviderError> {
        self.mark_audit_pending(operation_id)
    }

    /// Atomically release a pinned lease after a confirmed rollback.
    pub(crate) fn release_after_confirmed_rollback(
        &mut self,
        operation_id: OperationId,
    ) -> Result<(), CapacityProviderError> {
        use super::capacity_retention_telemetry::{CapacityLedgerCall as K, observe};
        observe(K::ConfirmedRollbackRelease, || {
            self.release_after_confirmed_rollback_original(operation_id)
        })
    }

    fn release_after_confirmed_rollback_original(
        &mut self,
        operation_id: OperationId,
    ) -> Result<(), CapacityProviderError> {
        if operation_id.as_uuid().is_nil() {
            return Err(CapacityProviderError::LeaseDispositionConflict);
        }
        match &self.disposition {
            ProvisionalLeaseDisposition::AuditPending {
                operation_id: current,
                ..
            } if *current == operation_id => {
                self.release_pinned_after_confirmed_rollback(operation_id)
            }
            ProvisionalLeaseDisposition::Provisional => self.release(),
            _ => Err(CapacityProviderError::LeaseDispositionConflict),
        }
    }

    pub(crate) fn disposition(&self) -> &ProvisionalLeaseDisposition {
        &self.disposition
    }

    fn bind_disposition(
        &mut self,
        operation_id: OperationId,
        audit_pending: bool,
    ) -> Result<(), CapacityProviderError> {
        use super::capacity_retention_telemetry::{CapacityLedgerCall as K, observe};
        observe(
            if audit_pending {
                K::AuditPendingBinding
            } else {
                K::DurableBinding
            },
            || self.bind_disposition_original(operation_id, audit_pending),
        )
    }

    fn bind_disposition_original(
        &mut self,
        operation_id: OperationId,
        audit_pending: bool,
    ) -> Result<(), CapacityProviderError> {
        if operation_id.as_uuid().is_nil() {
            return Err(CapacityProviderError::LeaseDispositionConflict);
        }
        let mut ledger = self
            .ledger
            .try_borrow_mut()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        #[cfg(test)]
        if ledger.fail_disposition && !audit_pending {
            return Err(CapacityProviderError::LedgerBusy);
        }
        let Some(entry) = ledger.charges_by_token.get_mut(&self.token) else {
            return Err(CapacityProviderError::LeaseDispositionConflict);
        };
        if entry.charges != self.charges {
            return Err(CapacityProviderError::LeaseDispositionConflict);
        }
        let claim_ids = self.claim_ids.clone();
        let next = match &entry.disposition {
            ProvisionalLeaseDisposition::Provisional => {
                if audit_pending {
                    ProvisionalLeaseDisposition::AuditPending {
                        operation_id,
                        claim_ids,
                    }
                } else {
                    ProvisionalLeaseDisposition::Durable {
                        operation_id,
                        claim_ids,
                    }
                }
            }
            ProvisionalLeaseDisposition::AuditPending {
                operation_id: current,
                ..
            } if !audit_pending && *current == operation_id => {
                ProvisionalLeaseDisposition::Durable {
                    operation_id,
                    claim_ids,
                }
            }
            ProvisionalLeaseDisposition::Durable {
                operation_id: current,
                ..
            } if !audit_pending && *current == operation_id => return Ok(()),
            ProvisionalLeaseDisposition::AuditPending {
                operation_id: current,
                ..
            } if audit_pending && *current == operation_id => return Ok(()),
            _ => return Err(CapacityProviderError::LeaseDispositionConflict),
        };
        let mutation = ChargeMutation::begin(&ledger);
        ledger
            .charges_by_token
            .get_mut(&self.token)
            .expect("validated disposition token")
            .disposition = next.clone();
        self.disposition = next;
        mutation.finish(&ledger);
        Ok(())
    }

    fn release_pinned_after_confirmed_rollback(
        &mut self,
        operation_id: OperationId,
    ) -> Result<(), CapacityProviderError> {
        let mut ledger = self
            .ledger
            .try_borrow_mut()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        #[cfg(test)]
        if ledger.fail_release {
            return Err(CapacityProviderError::ReleaseFailed);
        }
        let Some(entry) = ledger.charges_by_token.get_mut(&self.token) else {
            return Err(CapacityProviderError::ReleaseFailed);
        };
        match &entry.disposition {
            ProvisionalLeaseDisposition::AuditPending {
                operation_id: current,
                claim_ids,
            } if *current == operation_id && claim_ids == &self.claim_ids => {}
            _ => return Err(CapacityProviderError::LeaseDispositionConflict),
        }
        if entry.charges != self.charges {
            return Err(CapacityProviderError::ReleaseFailed);
        }
        let owned_charges = entry.charges.clone();
        for (key, bytes) in &owned_charges {
            let Some(current) = ledger.aggregate_charges.get(key).copied() else {
                return Err(CapacityProviderError::ReleaseFailed);
            };
            if current < *bytes {
                return Err(CapacityProviderError::ReleaseFailed);
            }
        }
        let mutation = ChargeMutation::begin(&ledger);
        for (key, bytes) in &owned_charges {
            let current = ledger
                .aggregate_charges
                .get_mut(key)
                .expect("validated provisional aggregate");
            *current -= *bytes;
            if *current == 0 {
                ledger.aggregate_charges.remove(key);
            }
        }
        ledger.charges_by_token.remove(&self.token);
        self.disposition = ProvisionalLeaseDisposition::Released;
        mutation.finish(&ledger);
        Ok(())
    }

    pub(crate) fn transfer_to_intent(&self) -> Result<(), CapacityProviderError> {
        Err(CapacityProviderError::IntentTransferUnavailable)
    }
}

impl Drop for ProvisionalCapacityLease {
    fn drop(&mut self) {
        if matches!(&self.disposition, ProvisionalLeaseDisposition::Provisional) {
            let _ = self.release();
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum CapacityProviderError {
    #[error("capacity provider is unavailable")]
    Unavailable,
    #[error("capacity plan validation failed")]
    InvalidPlan,
    #[error("admission claim identity is already retained: {claim_id}")]
    DuplicateClaim { claim_id: AdmissionClaimId },
    #[error("admission claim validation failed")]
    InvalidClaims,
    #[error("capacity facts are unavailable for volume {volume:?}")]
    FactsUnavailable { volume: VolumeIdentity },
    #[error("capacity facts are unsupported")]
    UnsupportedFacts,
    #[error("capacity allocation unit is unsupported for volume {volume:?}")]
    UnsupportedAllocationUnit { volume: VolumeIdentity },
    #[error("capacity arithmetic overflow")]
    ArithmeticOverflow,
    #[error("protected recovery reserve is low on volume {volume:?}: {free_bytes} bytes free")]
    RecoveryReserveLow {
        volume: VolumeIdentity,
        free_bytes: u64,
    },
    #[error(
        "ordinary capacity admission requires {required_bytes} bytes on volume {volume:?}, only {available_bytes} remain above the recovery reserve"
    )]
    DiskPressureRecoveryOnly {
        volume: VolumeIdentity,
        required_bytes: u64,
        available_bytes: u64,
    },
    #[error("provisional capacity claim inventory is exhausted")]
    ClaimInventoryExhausted,
    #[error("provisional lease token space is exhausted")]
    LeaseTokenExhausted,
    #[error("provisional capacity ledger is busy")]
    LedgerBusy,
    #[error("provisional capacity release failed; accounting remains held")]
    ReleaseFailed,
    #[error("ordinary claim release has no unique retained capacity token")]
    ClaimReleaseNotFound,
    #[error("ordinary claim release matches multiple retained capacity tokens")]
    ClaimReleaseAmbiguous,
    #[error("ordinary claim release identity or durable metadata conflicts")]
    ClaimReleaseConflict,
    #[error("provisional capacity is retained by a durable or audit-pending intent")]
    LeaseRetained,
    #[error("capacity admission is blocked by an unresolved persistence outcome")]
    RecoveryRequired,
    #[error("provisional lease disposition does not match the requested operation")]
    LeaseDispositionConflict,
    #[error("provisional capacity cannot transfer to a durable intent yet")]
    IntentTransferUnavailable,
    #[error("restart rebuild contains an invalid ordinary claim")]
    RebuildInvalidClaim,
    #[error("restart rebuild evidence is not ready for provider installation")]
    RebuildNotReady,
    #[error("restart rebuild contains conflicting claim or release identity")]
    RebuildIdentityConflict,
    #[error("restart rebuild conflicts with the live owner-thread ledger")]
    RebuildLedgerConflict,
    #[error("capacity reconciliation resource envelope exceeded")]
    RebuildResourceBound,
}

fn disposition_operation(disposition: &ProvisionalLeaseDisposition) -> Option<OperationId> {
    match disposition {
        ProvisionalLeaseDisposition::Durable { operation_id, .. }
        | ProvisionalLeaseDisposition::AuditPending { operation_id, .. } => Some(*operation_id),
        ProvisionalLeaseDisposition::Provisional | ProvisionalLeaseDisposition::Released => None,
    }
}

#[cfg(test)]
mod admission_metrics_tests;

#[cfg(test)]
mod retention_metrics_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_journal::full_capacity_planner::{
        CapacityAllocationNature, CapacityEstimate, CapacityRequirement, derive_full_capacity_plan,
    };
    use crate::operation_journal::store::{OrdinaryClaimRebuildSnapshot, RebuildClaim};
    use crate::operation_journal::{
        AdmissionClaim, AdmissionClaimId, FullCapacityAllocation, FullCapacityPlan,
    };

    pub(super) fn facts(device: u64, free_bytes: u64, allocation_unit: u64) -> VolumeFacts {
        VolumeFacts {
            identity: VolumeIdentity { device },
            free_bytes,
            allocation_unit,
        }
    }

    pub(super) fn plan_and_claims(
        rows: &[(VolumeIdentity, FullCapacityAllocationClass, u64, u64)],
    ) -> (FullCapacityPlan, AdmissionClaimInventory) {
        let allocations = rows
            .iter()
            .map(|(volume, class, logical_bytes, allocation_unit)| {
                FullCapacityAllocation::new(
                    volume.clone(),
                    *allocation_unit,
                    *class,
                    *logical_bytes,
                    round_up(*logical_bytes, *allocation_unit),
                    PROTECTED_FREE_SPACE_FLOOR,
                )
                .expect("test allocation")
            })
            .collect::<Vec<_>>();
        let plan = FullCapacityPlan::new(allocations).expect("test plan");
        let claims = claims_for_plan(&plan);
        (plan, claims)
    }

    fn claims_for_plan(plan: &FullCapacityPlan) -> AdmissionClaimInventory {
        let claims = AdmissionClaimInventory::new(
            plan.allocations()
                .iter()
                .map(|allocation| {
                    AdmissionClaim::new(
                        AdmissionClaimId::new(),
                        allocation.volume().clone(),
                        allocation.class(),
                        allocation.allocated_bytes(),
                    )
                    .expect("test claim")
                })
                .collect(),
        )
        .expect("test claims");
        claims
    }

    fn round_up(bytes: u64, allocation_unit: u64) -> u64 {
        crate::durable_capacity::round_up_allocation(bytes, allocation_unit).expect("test round")
    }

    pub(super) fn rebuild_claim(
        operation_id: OperationId,
        claim_id: AdmissionClaimId,
        volume: VolumeIdentity,
        allocation_class: FullCapacityAllocationClass,
        allocated_bytes: u64,
        release_id: Option<Uuid>,
        audit_pending: bool,
    ) -> RebuildClaim {
        RebuildClaim::for_test(
            operation_id,
            claim_id,
            volume,
            allocation_class,
            allocated_bytes,
            release_id,
            audit_pending,
        )
    }

    pub(super) fn rebuild_snapshot(claims: Vec<RebuildClaim>) -> OrdinaryClaimRebuildSnapshot {
        OrdinaryClaimRebuildSnapshot::for_test(claims, CapacityRebuildReadiness::ready())
    }

    #[test]
    fn restart_rebuild_groups_claims_replays_and_never_resurrects_released_claims() {
        let first_volume = VolumeIdentity { device: 101 };
        let second_volume = VolumeIdentity { device: 102 };
        let first_class = FullCapacityAllocationClass::DestinationStaging;
        let second_class = FullCapacityAllocationClass::SourceWalShm;
        let first_operation = OperationId::new();
        let second_operation = OperationId::new();
        let first_claim = AdmissionClaimId::new();
        let second_claim = AdmissionClaimId::new();
        let third_claim = AdmissionClaimId::new();
        let initial = rebuild_snapshot(vec![
            rebuild_claim(
                first_operation,
                first_claim,
                first_volume.clone(),
                first_class,
                4096,
                None,
                false,
            ),
            rebuild_claim(
                first_operation,
                second_claim,
                first_volume.clone(),
                first_class,
                8192,
                None,
                false,
            ),
            rebuild_claim(
                second_operation,
                third_claim,
                second_volume.clone(),
                second_class,
                2048,
                None,
                false,
            ),
        ]);
        let provider = CapacityProvider::unavailable();
        provider
            .install_rebuilt_claims(&initial)
            .expect("initial rebuild");
        assert_eq!(provider.claim_count(), 2);
        assert_eq!(provider.outstanding(&first_volume, first_class), 12288);
        assert_eq!(provider.outstanding(&second_volume, second_class), 2048);
        provider
            .install_rebuilt_claims(&initial)
            .expect("exact rebuild replay");
        assert_eq!(provider.claim_count(), 2);

        let release_id = Uuid::new_v4();
        let receipt = OrdinaryClaimReleaseReceipt::for_test(
            first_operation,
            first_claim,
            release_id,
            first_volume.clone(),
            first_class,
            4096,
        );
        provider
            .release_claim(&receipt)
            .expect("release one rebuilt claim");
        let after_release = rebuild_snapshot(vec![
            rebuild_claim(
                first_operation,
                first_claim,
                first_volume.clone(),
                first_class,
                4096,
                Some(release_id),
                false,
            ),
            rebuild_claim(
                first_operation,
                second_claim,
                first_volume.clone(),
                first_class,
                8192,
                None,
                false,
            ),
            rebuild_claim(
                second_operation,
                third_claim,
                second_volume.clone(),
                second_class,
                2048,
                None,
                false,
            ),
        ]);
        provider
            .install_rebuilt_claims(&after_release)
            .expect("replay after durable release");
        assert_eq!(provider.outstanding(&first_volume, first_class), 8192);
        assert_eq!(provider.outstanding(&second_volume, second_class), 2048);
        assert_eq!(provider.claim_count(), 2);
    }

    #[test]
    fn clean_handoff_requires_empty_live_token_map() {
        let provider = CapacityProvider::unavailable();
        provider
            .prove_no_live_charges()
            .expect("empty provider ledger is zero");
        provider.ledger.borrow_mut().charges_by_token.insert(
            1,
            LedgerCharge {
                charges: BTreeMap::new(),
                claims: BTreeMap::new(),
                disposition: ProvisionalLeaseDisposition::Provisional,
            },
        );
        assert!(matches!(
            provider.prove_no_live_charges(),
            Err(CapacityProviderError::RebuildLedgerConflict)
        ));
    }

    #[test]
    fn fresh_descriptor_facts_can_admit_unqualified_production_provider_once() {
        let volume = VolumeIdentity { device: 106 };
        let class = FullCapacityAllocationClass::DestinationStaging;
        let (plan, claims) = plan_and_claims(&[(volume.clone(), class, 4096, 4096)]);
        let provider = CapacityProvider::unavailable();
        let mut observations = BTreeMap::new();
        observations.insert(
            volume.clone(),
            facts(106, PROTECTED_FREE_SPACE_FLOOR + 8192, 4096),
        );
        let lease = provider
            .admit_with_fresh_facts(&plan, &claims, &observations)
            .expect("fresh retained facts admit one provisional lease");
        assert_eq!(provider.outstanding(&volume, class), 4096);
        drop(lease);
        assert_eq!(provider.outstanding(&volume, class), 0);
        assert!(matches!(
            provider.admit(&plan, &claims),
            Err(CapacityProviderError::Unavailable)
        ));
    }

    #[test]
    fn fresh_fact_rejections_leave_provider_ledger_unchanged() {
        let volume = VolumeIdentity { device: 107 };
        let class = FullCapacityAllocationClass::DestinationStaging;
        let (plan, claims) = plan_and_claims(&[(volume.clone(), class, 4096, 4096)]);
        let provider = CapacityProvider::unavailable();

        let mut low = BTreeMap::new();
        low.insert(volume.clone(), facts(107, PROTECTED_FREE_SPACE_FLOOR, 4096));
        assert!(matches!(
            provider.admit_with_fresh_facts(&plan, &claims, &low),
            Err(CapacityProviderError::RecoveryReserveLow { .. })
        ));

        let mut wrong_unit = BTreeMap::new();
        wrong_unit.insert(
            volume.clone(),
            facts(107, PROTECTED_FREE_SPACE_FLOOR + 8192, 8192),
        );
        assert!(matches!(
            provider.admit_with_fresh_facts(&plan, &claims, &wrong_unit),
            Err(CapacityProviderError::UnsupportedAllocationUnit { .. })
        ));
        assert_eq!(provider.claim_count(), 0);
        assert_eq!(provider.outstanding(&volume, class), 0);
    }

    #[test]
    fn provisional_lease_rejects_settlement_through_a_foreign_provider() {
        let volume = VolumeIdentity { device: 109 };
        let class = FullCapacityAllocationClass::DestinationStaging;
        let (plan, claims) = plan_and_claims(&[(volume.clone(), class, 4096, 4096)]);
        let mut observations = BTreeMap::new();
        observations.insert(
            volume.clone(),
            facts(109, PROTECTED_FREE_SPACE_FLOOR + 8192, 4096),
        );
        let provider = CapacityProvider::unavailable();
        let foreign = CapacityProvider::unavailable();
        let lease = provider
            .admit_with_fresh_facts(&plan, &claims, &observations)
            .expect("test lease");
        assert_eq!(
            lease.validate_against(&foreign, &plan, &claims),
            Err(CapacityProviderError::LeaseDispositionConflict)
        );
        drop(lease);
        assert_eq!(provider.claim_count(), 0);
        assert_eq!(foreign.claim_count(), 0);
    }

    #[test]
    fn audit_pending_ledger_state_blocks_new_prepared_admission() {
        let provider = CapacityProvider::unavailable();
        let snapshot = rebuild_snapshot(vec![rebuild_claim(
            OperationId::new(),
            AdmissionClaimId::new(),
            VolumeIdentity { device: 108 },
            FullCapacityAllocationClass::DestinationStaging,
            4096,
            None,
            true,
        )]);
        provider
            .install_rebuilt_claims(&snapshot)
            .expect("audit-pending image installs for retention");
        assert_eq!(
            provider.validate_recovery_state(),
            Err(CapacityProviderError::RecoveryRequired)
        );
    }

    #[test]
    fn restart_rebuild_rejects_conflicting_live_image_and_near_max_replay_is_idempotent() {
        let volume = VolumeIdentity { device: 103 };
        let class = FullCapacityAllocationClass::DestinationStaging;
        let operation_id = OperationId::new();
        let first_claim = AdmissionClaimId::new();
        let second_claim = AdmissionClaimId::new();
        let first = rebuild_snapshot(vec![rebuild_claim(
            operation_id,
            first_claim,
            volume.clone(),
            class,
            4096,
            None,
            false,
        )]);
        let conflicting = rebuild_snapshot(vec![rebuild_claim(
            operation_id,
            second_claim,
            volume.clone(),
            class,
            8192,
            None,
            false,
        )]);
        let provider = CapacityProvider::unavailable();
        provider
            .install_rebuilt_claims(&first)
            .expect("initial rebuild");
        provider.ledger.borrow_mut().next_token = u64::MAX;
        provider
            .install_rebuilt_claims(&first)
            .expect("near-max exact replay");
        assert_eq!(provider.next_token(), u64::MAX);
        assert_eq!(provider.outstanding(&volume, class), 4096);
        assert!(matches!(
            provider.install_rebuilt_claims(&conflicting),
            Err(CapacityProviderError::RebuildLedgerConflict)
        ));
        assert_eq!(provider.outstanding(&volume, class), 4096);
        assert_eq!(provider.claim_count(), 1);
    }

    #[test]
    fn restart_rebuild_rejects_unready_snapshot_without_installing_claims() {
        let snapshot = OrdinaryClaimRebuildSnapshot::for_test(
            vec![rebuild_claim(
                OperationId::new(),
                AdmissionClaimId::new(),
                VolumeIdentity { device: 104 },
                FullCapacityAllocationClass::DestinationStaging,
                4096,
                None,
                true,
            )],
            CapacityRebuildReadiness::reconciliation_required(vec![String::from(
                "malformed durable row",
            )]),
        );
        let provider = CapacityProvider::unavailable();
        assert_eq!(
            provider.install_rebuilt_claims(&snapshot),
            Err(CapacityProviderError::RebuildNotReady)
        );
        assert_eq!(provider.claim_count(), 0);
    }

    #[test]
    fn restart_rebuild_rejects_checked_aggregate_overflow_before_install() {
        let operation_id = OperationId::new();
        let volume = VolumeIdentity { device: 105 };
        let class = FullCapacityAllocationClass::DestinationStaging;
        let snapshot = rebuild_snapshot(vec![
            rebuild_claim(
                operation_id,
                AdmissionClaimId::new(),
                volume.clone(),
                class,
                u64::MAX,
                None,
                false,
            ),
            rebuild_claim(
                operation_id,
                AdmissionClaimId::new(),
                volume.clone(),
                class,
                1,
                None,
                false,
            ),
        ]);
        let provider = CapacityProvider::unavailable();
        assert_eq!(
            provider.install_rebuilt_claims(&snapshot),
            Err(CapacityProviderError::ArithmeticOverflow)
        );
        assert_eq!(provider.claim_count(), 0);
        assert_eq!(provider.outstanding(&volume, class), 0);
    }

    #[test]
    fn two_coexisting_claims_same_volume_are_accounted_independently() {
        let volume = VolumeIdentity { device: 1 };
        let (plan, claims) = plan_and_claims(&[(
            volume.clone(),
            FullCapacityAllocationClass::DestinationStaging,
            4096,
            4096,
        )]);
        let mut fixture = BTreeMap::new();
        fixture.insert(
            volume.clone(),
            facts(1, PROTECTED_FREE_SPACE_FLOOR + 8192, 4096),
        );
        let provider = CapacityProvider::qualified_fixture(fixture);
        let first = provider.admit(&plan, &claims).expect("first claim");
        let second = provider.admit(&plan, &claims).expect("second claim");
        assert_eq!(
            provider.outstanding(&volume, FullCapacityAllocationClass::DestinationStaging),
            8192
        );
        drop(first);
        assert_eq!(
            provider.outstanding(&volume, FullCapacityAllocationClass::DestinationStaging),
            4096
        );
        drop(second);
        assert_eq!(provider.claim_count(), 0);
    }

    #[test]
    fn ordinary_release_fails_closed_for_ambiguous_or_reused_claim_identity() {
        let volume = VolumeIdentity { device: 11 };
        let class = FullCapacityAllocationClass::DestinationStaging;
        let (plan, claims) = plan_and_claims(&[(volume.clone(), class, 4096, 4096)]);
        let mut fixture = BTreeMap::new();
        fixture.insert(
            volume.clone(),
            facts(11, PROTECTED_FREE_SPACE_FLOOR + 8192, 4096),
        );
        let provider = CapacityProvider::qualified_fixture(fixture);
        let operation_one = OperationId::new();
        let operation_two = OperationId::new();
        let mut first = provider.admit(&plan, &claims).expect("first lease");
        first.mark_durable(operation_one).expect("first durable");
        drop(first);
        let mut second = provider.admit(&plan, &claims).expect("second lease");
        second.mark_durable(operation_two).expect("second durable");
        drop(second);
        let claim = &claims.claims()[0];
        let ambiguous = OrdinaryClaimReleaseReceipt::for_test(
            operation_one,
            claim.claim_id(),
            Uuid::new_v4(),
            volume.clone(),
            class,
            claim.allocated_bytes(),
        );
        assert!(matches!(
            provider.release_claim(&ambiguous),
            Err(CapacityProviderError::ClaimReleaseAmbiguous)
        ));
        assert_eq!(provider.outstanding(&volume, class), 8192);

        let unique = CapacityProvider::qualified_fixture(BTreeMap::from([(
            volume.clone(),
            facts(11, PROTECTED_FREE_SPACE_FLOOR + 4096, 4096),
        )]));
        let mut lease = unique.admit(&plan, &claims).expect("unique lease");
        lease.mark_durable(operation_one).expect("unique durable");
        drop(lease);
        let released = OrdinaryClaimReleaseReceipt::for_test(
            operation_one,
            claim.claim_id(),
            Uuid::new_v4(),
            volume.clone(),
            class,
            claim.allocated_bytes(),
        );
        unique
            .release_claim(&released)
            .expect("release unique claim");
        let mut reused = unique.admit(&plan, &claims).expect("reused claim lease");
        reused.mark_durable(operation_two).expect("reused durable");
        drop(reused);
        assert!(matches!(
            unique.release_claim(&released),
            Err(CapacityProviderError::ClaimReleaseConflict)
        ));
        assert_eq!(unique.outstanding(&volume, class), 4096);
    }

    #[test]
    fn multiple_volumes_keep_independent_class_charges() {
        let first_volume = VolumeIdentity { device: 1 };
        let second_volume = VolumeIdentity { device: 2 };
        let (plan, claims) = plan_and_claims(&[
            (
                first_volume.clone(),
                FullCapacityAllocationClass::DestinationStaging,
                4096,
                4096,
            ),
            (
                second_volume.clone(),
                FullCapacityAllocationClass::SourceWalShm,
                8192,
                4096,
            ),
        ]);
        let mut fixture = BTreeMap::new();
        fixture.insert(
            first_volume.clone(),
            facts(1, PROTECTED_FREE_SPACE_FLOOR + 4096, 4096),
        );
        fixture.insert(
            second_volume.clone(),
            facts(2, PROTECTED_FREE_SPACE_FLOOR + 8192, 4096),
        );
        let provider = CapacityProvider::qualified_fixture(fixture);
        let lease = provider.admit(&plan, &claims).expect("multi-volume claim");
        assert_eq!(
            provider.outstanding(
                &first_volume,
                FullCapacityAllocationClass::DestinationStaging
            ),
            4096
        );
        assert_eq!(
            provider.outstanding(&second_volume, FullCapacityAllocationClass::SourceWalShm),
            8192
        );
        drop(lease);
        assert_eq!(provider.claim_count(), 0);
    }

    #[test]
    fn seeded_two_volume_pressure_is_atomic_and_order_independent() {
        const SEEDS: [(u64, bool); 2] = [(0x1567, false), (0x1568, true)];
        let first_volume = VolumeIdentity { device: 1567 };
        let second_volume = VolumeIdentity { device: 1568 };
        let first_class = FullCapacityAllocationClass::DestinationStaging;
        let first_recovery_class = FullCapacityAllocationClass::OperationRecoveryPayload;
        let second_class = FullCapacityAllocationClass::DestinationStaging;

        for (seed, reverse_order) in SEEDS {
            let mut rows = vec![
                (first_volume.clone(), first_class, 4096, 4096),
                (first_volume.clone(), first_recovery_class, 4096, 4096),
                (second_volume.clone(), second_class, 4096, 4096),
            ];
            if reverse_order {
                rows.reverse();
            }
            let (plan, claims) = plan_and_claims(&rows);

            let mut exact_fixture = BTreeMap::new();
            exact_fixture.insert(
                first_volume.clone(),
                facts(1567, PROTECTED_FREE_SPACE_FLOOR + 8192, 4096),
            );
            exact_fixture.insert(
                second_volume.clone(),
                facts(1568, PROTECTED_FREE_SPACE_FLOOR + 4096, 4096),
            );
            let exact_provider = CapacityProvider::qualified_fixture(exact_fixture);
            let exact_lease = exact_provider
                .admit(&plan, &claims)
                .unwrap_or_else(|error| panic!("seed {seed:#x} exact boundary: {error}"));
            assert_eq!(
                exact_provider.outstanding(&first_volume, first_class),
                4096,
                "seed {seed:#x} first-volume staging charge"
            );
            assert_eq!(
                exact_provider.outstanding(&first_volume, first_recovery_class),
                4096,
                "seed {seed:#x} first-volume recovery charge"
            );
            assert_eq!(
                exact_provider.outstanding(&second_volume, second_class),
                4096,
                "seed {seed:#x} second-volume charge"
            );
            assert_eq!(exact_provider.claim_count(), 1);
            drop(exact_lease);
            assert_eq!(exact_provider.claim_count(), 0);

            for pressured_volume in [first_volume.clone(), second_volume.clone()] {
                let mut pressure_fixture = BTreeMap::new();
                let first_free = if pressured_volume == first_volume {
                    PROTECTED_FREE_SPACE_FLOOR + 16383
                } else {
                    PROTECTED_FREE_SPACE_FLOOR + 16384
                };
                let second_free = if pressured_volume == second_volume {
                    PROTECTED_FREE_SPACE_FLOOR + 8191
                } else {
                    PROTECTED_FREE_SPACE_FLOOR + 8192
                };
                pressure_fixture.insert(first_volume.clone(), facts(1567, first_free, 4096));
                pressure_fixture.insert(second_volume.clone(), facts(1568, second_free, 4096));
                let provider = CapacityProvider::qualified_fixture(pressure_fixture);
                let baseline_claims = claims_for_plan(&plan);
                let mut baseline_lease = provider
                    .admit(&plan, &baseline_claims)
                    .unwrap_or_else(|error| panic!("seed {seed:#x} baseline admission: {error}"));
                baseline_lease
                    .mark_durable(OperationId::new())
                    .unwrap_or_else(|error| panic!("seed {seed:#x} baseline durable: {error}"));
                let before_ledger = {
                    let ledger = provider.ledger.borrow();
                    (
                        ledger.next_token,
                        ledger.charges_by_token.clone(),
                        ledger.aggregate_charges.clone(),
                        ledger.released_claims.clone(),
                    )
                };
                let before_totals = (
                    provider.outstanding(&first_volume, first_class),
                    provider.outstanding(&first_volume, first_recovery_class),
                    provider.outstanding(&second_volume, second_class),
                );
                let result = provider.admit(&plan, &claims);
                match result {
                    Err(CapacityProviderError::DiskPressureRecoveryOnly { volume, .. }) => {
                        assert_eq!(
                            volume, pressured_volume,
                            "seed {seed:#x} pressure must identify the specifically pressured volume"
                        );
                    }
                    other => panic!(
                        "seed {seed:#x} pressure on {pressured_volume:?} must reject with typed pressure, got {other:?}"
                    ),
                }
                let after_ledger = {
                    let ledger = provider.ledger.borrow();
                    (
                        ledger.next_token,
                        ledger.charges_by_token.clone(),
                        ledger.aggregate_charges.clone(),
                        ledger.released_claims.clone(),
                    )
                };
                assert_eq!(
                    after_ledger, before_ledger,
                    "seed {seed:#x} pressure rejection must preserve the exact pre-existing claims, token, dispositions, and release history"
                );
                assert_eq!(
                    (
                        provider.outstanding(&first_volume, first_class),
                        provider.outstanding(&first_volume, first_recovery_class),
                        provider.outstanding(&second_volume, second_class),
                    ),
                    before_totals,
                    "seed {seed:#x} pressure rejection must leave both volume totals unchanged"
                );
            }
        }
    }

    #[test]
    fn ordinary_admission_preserves_the_recovery_floor() {
        let volume = VolumeIdentity { device: 3 };
        let (plan, claims) = plan_and_claims(&[(
            volume.clone(),
            FullCapacityAllocationClass::DestinationStaging,
            8192,
            4096,
        )]);
        let mut fixture = BTreeMap::new();
        fixture.insert(
            volume.clone(),
            facts(3, PROTECTED_FREE_SPACE_FLOOR + 4096, 4096),
        );
        let provider = CapacityProvider::qualified_fixture(fixture);
        assert!(matches!(
            provider.admit(&plan, &claims),
            Err(CapacityProviderError::DiskPressureRecoveryOnly { .. })
        ));
        assert_eq!(provider.claim_count(), 0);
    }

    #[test]
    fn nonexact_plan_floor_is_rejected_without_ledger_or_token_mutation() {
        let volume = VolumeIdentity { device: 9 };
        let allocation = FullCapacityAllocation::new(
            volume.clone(),
            4096,
            FullCapacityAllocationClass::DestinationStaging,
            4096,
            4096,
            PROTECTED_FREE_SPACE_FLOOR + 4096,
        )
        .expect("higher observed floor is valid as an observation");
        let plan = FullCapacityPlan::new(vec![allocation]).expect("higher floor plan");
        let claims = claims_for_plan(&plan);
        let mut fixture = BTreeMap::new();
        fixture.insert(volume, facts(9, PROTECTED_FREE_SPACE_FLOOR + 4096, 4096));
        let provider = CapacityProvider::qualified_fixture(fixture);
        let token_before = provider.next_token();
        assert!(matches!(
            provider.admit(&plan, &claims),
            Err(CapacityProviderError::UnsupportedFacts)
        ));
        assert_eq!(provider.claim_count(), 0);
        assert_eq!(provider.next_token(), token_before);
    }

    #[test]
    fn all_classes_track_retained_rows_but_charge_only_new_headroom() {
        let volume = VolumeIdentity { device: 10 };
        let retained_bytes = 1 << 30;
        let per_class_bytes = 4096;
        let rows = [
            (
                volume.clone(),
                FullCapacityAllocationClass::DestinationStaging,
                per_class_bytes,
                4096,
            ),
            (
                volume.clone(),
                FullCapacityAllocationClass::JournalRecord,
                per_class_bytes,
                4096,
            ),
            (
                volume.clone(),
                FullCapacityAllocationClass::SourceDatabaseCommit,
                per_class_bytes,
                4096,
            ),
            (
                volume.clone(),
                FullCapacityAllocationClass::SourceWalShm,
                per_class_bytes,
                4096,
            ),
            (
                volume.clone(),
                FullCapacityAllocationClass::GlobalDatabaseCommit,
                per_class_bytes,
                4096,
            ),
            (
                volume.clone(),
                FullCapacityAllocationClass::GlobalWalShm,
                per_class_bytes,
                4096,
            ),
            (
                volume.clone(),
                FullCapacityAllocationClass::ExistingDestination,
                retained_bytes,
                4096,
            ),
            (
                volume.clone(),
                FullCapacityAllocationClass::ExistingRecoveryPayload,
                retained_bytes,
                4096,
            ),
            (
                volume.clone(),
                FullCapacityAllocationClass::OperationRecoveryPayload,
                per_class_bytes,
                4096,
            ),
        ];
        let (plan, claims) = plan_and_claims(&rows);
        let new_headroom = 7 * per_class_bytes;
        let mut fixture = BTreeMap::new();
        fixture.insert(
            volume.clone(),
            facts(10, PROTECTED_FREE_SPACE_FLOOR + new_headroom, 4096),
        );
        let provider = CapacityProvider::qualified_fixture(fixture);
        let lease = provider.admit(&plan, &claims).expect("all-class claim");
        assert_eq!(
            provider.outstanding(&volume, FullCapacityAllocationClass::ExistingDestination),
            retained_bytes
        );
        assert_eq!(
            provider.outstanding(
                &volume,
                FullCapacityAllocationClass::ExistingRecoveryPayload
            ),
            retained_bytes
        );
        assert_eq!(
            provider.outstanding(&volume, FullCapacityAllocationClass::DestinationStaging),
            per_class_bytes
        );
        assert_eq!(
            provider.outstanding(
                &volume,
                FullCapacityAllocationClass::OperationRecoveryPayload
            ),
            per_class_bytes
        );
        drop(lease);
        assert_eq!(provider.claim_count(), 0);
    }

    #[test]
    fn descriptor_planner_output_keeps_retained_rows_and_charges_independent_new_classes() {
        let temp = tempfile::tempdir().expect("create planner fixtures");
        let path = temp.path().join("destination");
        let descriptor = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)
            .expect("open planner descriptor");
        let volume = VolumeIdentity { device: 12 };
        let facts = |free_bytes| VolumeFacts {
            identity: volume.clone(),
            free_bytes,
            allocation_unit: 4096,
        };
        let requirements = [
            CapacityRequirement::with_test_facts(
                &descriptor,
                facts(PROTECTED_FREE_SPACE_FLOOR + 8192),
                FullCapacityAllocationClass::ExistingDestination,
                CapacityEstimate::Known(1 << 30),
                CapacityAllocationNature::ExistingRetained,
            ),
            CapacityRequirement::with_test_facts(
                &descriptor,
                facts(PROTECTED_FREE_SPACE_FLOOR + 8192),
                FullCapacityAllocationClass::DestinationStaging,
                CapacityEstimate::Known(4096),
                CapacityAllocationNature::NewAllocation,
            ),
            CapacityRequirement::with_test_facts(
                &descriptor,
                facts(PROTECTED_FREE_SPACE_FLOOR + 8192),
                FullCapacityAllocationClass::OperationRecoveryPayload,
                CapacityEstimate::Known(4096),
                CapacityAllocationNature::NewAllocation,
            ),
        ];
        let plan = derive_full_capacity_plan(&requirements).expect("descriptor-derived plan");
        let claims = claims_for_plan(&plan);
        let mut fixture = BTreeMap::new();
        fixture.insert(volume.clone(), facts(PROTECTED_FREE_SPACE_FLOOR + 8192));
        let provider = CapacityProvider::qualified_fixture(fixture);
        let lease = provider.admit(&plan, &claims).expect("fixture admission");
        assert_eq!(
            provider.outstanding(&volume, FullCapacityAllocationClass::ExistingDestination),
            1 << 30
        );
        assert_eq!(
            provider.outstanding(&volume, FullCapacityAllocationClass::DestinationStaging),
            4096
        );
        assert_eq!(
            provider.outstanding(
                &volume,
                FullCapacityAllocationClass::OperationRecoveryPayload
            ),
            4096
        );
        drop(lease);
        assert_eq!(provider.claim_count(), 0);
    }

    #[test]
    fn overflow_is_rejected_before_ledger_mutation() {
        let volume = VolumeIdentity { device: 4 };
        let class = FullCapacityAllocationClass::ExistingDestination;
        let (plan, claims) = plan_and_claims(&[(volume.clone(), class, 4096, 4096)]);
        let mut fixture = BTreeMap::new();
        fixture.insert(
            volume.clone(),
            facts(4, PROTECTED_FREE_SPACE_FLOOR + 4096, 4096),
        );
        let provider = CapacityProvider::qualified_fixture(fixture);
        provider.ledger.borrow_mut().aggregate_charges.insert(
            ChargeKey {
                volume: volume.clone(),
                allocation_class: class,
            },
            u64::MAX,
        );
        let token_before = provider.next_token();
        assert!(matches!(
            provider.admit(&plan, &claims),
            Err(CapacityProviderError::ArithmeticOverflow)
        ));
        assert_eq!(provider.next_token(), token_before);
        assert_eq!(provider.claim_count(), 0);
    }

    #[test]
    fn explicit_release_is_idempotent_and_drop_does_not_double_subtract() {
        let volume = VolumeIdentity { device: 5 };
        let class = FullCapacityAllocationClass::DestinationStaging;
        let (plan, claims) = plan_and_claims(&[(volume.clone(), class, 4096, 4096)]);
        let mut fixture = BTreeMap::new();
        fixture.insert(
            volume.clone(),
            facts(5, PROTECTED_FREE_SPACE_FLOOR + 4096, 4096),
        );
        let provider = CapacityProvider::qualified_fixture(fixture);
        let mut lease = provider.admit(&plan, &claims).expect("claim");
        lease.release().expect("release");
        lease.release().expect("idempotent release");
        assert_eq!(provider.outstanding(&volume, class), 0);
        drop(lease);
        assert_eq!(provider.claim_count(), 0);
    }

    #[test]
    fn failed_release_retains_accounting_until_a_retry_succeeds() {
        let volume = VolumeIdentity { device: 6 };
        let class = FullCapacityAllocationClass::DestinationStaging;
        let (plan, claims) = plan_and_claims(&[(volume.clone(), class, 4096, 4096)]);
        let mut fixture = BTreeMap::new();
        fixture.insert(
            volume.clone(),
            facts(6, PROTECTED_FREE_SPACE_FLOOR + 4096, 4096),
        );
        let provider = CapacityProvider::qualified_fixture(fixture);
        let mut lease = provider.admit(&plan, &claims).expect("claim");
        provider.fail_release_for_test();
        assert_eq!(lease.release(), Err(CapacityProviderError::ReleaseFailed));
        assert_eq!(provider.outstanding(&volume, class), 4096);
        provider.allow_release_for_test();
        lease.release().expect("retry release");
        assert_eq!(provider.outstanding(&volume, class), 0);
    }

    #[test]
    fn failed_drop_retains_accounting() {
        let volume = VolumeIdentity { device: 11 };
        let class = FullCapacityAllocationClass::DestinationStaging;
        let (plan, claims) = plan_and_claims(&[(volume.clone(), class, 4096, 4096)]);
        let mut fixture = BTreeMap::new();
        fixture.insert(
            volume.clone(),
            facts(11, PROTECTED_FREE_SPACE_FLOOR + 4096, 4096),
        );
        let provider = CapacityProvider::qualified_fixture(fixture);
        let lease = provider.admit(&plan, &claims).expect("claim");
        provider.fail_release_for_test();
        drop(lease);
        assert_eq!(provider.outstanding(&volume, class), 4096);
        assert_eq!(provider.claim_count(), 1);
    }

    #[test]
    fn production_provider_stays_unavailable_and_cannot_issue_a_lease() {
        let volume = VolumeIdentity { device: 7 };
        let (plan, claims) = plan_and_claims(&[(
            volume,
            FullCapacityAllocationClass::JournalRecord,
            4096,
            4096,
        )]);
        let provider = CapacityProvider::unavailable();
        assert!(matches!(
            provider.admit(&plan, &claims),
            Err(CapacityProviderError::Unavailable)
        ));
        assert_eq!(provider.claim_count(), 0);
    }

    #[test]
    fn production_admission_gate_is_unavailable() {
        assert_eq!(production_admission_gate(), Err(()));
    }

    #[test]
    fn durable_transfer_is_not_available_for_a_provisional_lease() {
        let volume = VolumeIdentity { device: 8 };
        let class = FullCapacityAllocationClass::DestinationStaging;
        let (plan, claims) = plan_and_claims(&[(volume.clone(), class, 4096, 4096)]);
        let mut fixture = BTreeMap::new();
        fixture.insert(
            volume.clone(),
            facts(8, PROTECTED_FREE_SPACE_FLOOR + 4096, 4096),
        );
        let provider = CapacityProvider::qualified_fixture(fixture);
        let lease = provider.admit(&plan, &claims).expect("claim");
        assert_eq!(
            lease.transfer_to_intent(),
            Err(CapacityProviderError::IntentTransferUnavailable)
        );
        drop(lease);
        assert_eq!(provider.claim_count(), 0);
    }

    #[test]
    fn durable_and_audit_pending_drop_retain_accounting() {
        let volume = VolumeIdentity { device: 13 };
        let class = FullCapacityAllocationClass::DestinationStaging;
        let (plan, claims) = plan_and_claims(&[(volume.clone(), class, 4096, 4096)]);
        let mut fixture = BTreeMap::new();
        fixture.insert(
            volume.clone(),
            facts(13, PROTECTED_FREE_SPACE_FLOOR + 8192, 4096),
        );
        let provider = CapacityProvider::qualified_fixture(fixture);

        let operation_id = OperationId::new();
        let expected_durable_claim_ids = claims
            .claims()
            .iter()
            .map(AdmissionClaim::claim_id)
            .collect::<Vec<_>>();
        let mut durable = provider.admit(&plan, &claims).expect("durable claim");
        durable
            .pin_for_persistence(operation_id)
            .expect("pin durable claim");
        durable
            .mark_durable(operation_id)
            .expect("settle durable claim");
        assert_eq!(durable.release(), Err(CapacityProviderError::LeaseRetained));
        drop(durable);
        assert_eq!(provider.outstanding(&volume, class), 4096);
        assert_eq!(provider.claim_count(), 1);
        assert_eq!(
            provider.ledger_dispositions(),
            vec![ProvisionalLeaseDisposition::Durable {
                operation_id,
                claim_ids: expected_durable_claim_ids.clone(),
            }]
        );

        let audit_claim_ids = claims
            .claims()
            .iter()
            .map(AdmissionClaim::claim_id)
            .collect::<Vec<_>>();
        let mut audit_pending = provider.admit(&plan, &claims).expect("audit claim");
        audit_pending
            .pin_for_persistence(operation_id)
            .expect("pin audit claim");
        assert_eq!(
            audit_pending.release(),
            Err(CapacityProviderError::LeaseRetained)
        );
        drop(audit_pending);
        assert_eq!(provider.outstanding(&volume, class), 8192);
        assert_eq!(provider.claim_count(), 2);
        assert_eq!(
            provider.ledger_dispositions(),
            vec![
                ProvisionalLeaseDisposition::Durable {
                    operation_id,
                    claim_ids: expected_durable_claim_ids,
                },
                ProvisionalLeaseDisposition::AuditPending {
                    operation_id,
                    claim_ids: audit_claim_ids,
                },
            ]
        );
    }

    #[test]
    fn audit_pending_drop_during_unwind_retains_accounting() {
        let volume = VolumeIdentity { device: 14 };
        let class = FullCapacityAllocationClass::DestinationStaging;
        let (plan, claims) = plan_and_claims(&[(volume.clone(), class, 4096, 4096)]);
        let mut fixture = BTreeMap::new();
        fixture.insert(
            volume.clone(),
            facts(14, PROTECTED_FREE_SPACE_FLOOR + 4096, 4096),
        );
        let provider = CapacityProvider::qualified_fixture(fixture);
        let operation_id = OperationId::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut lease = provider.admit(&plan, &claims).expect("claim");
            lease
                .pin_for_persistence(operation_id)
                .expect("pin before persistence");
            panic!("simulate unwind after persistence started");
        }));
        assert!(result.is_err());
        assert_eq!(provider.outstanding(&volume, class), 4096);
        assert_eq!(provider.claim_count(), 1);
    }
}
