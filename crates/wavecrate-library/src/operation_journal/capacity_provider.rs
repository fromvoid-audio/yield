//! Private provisional capacity accounting for the serialized journal owner.
//!
//! This module deliberately stops at an in-process ledger.  The production provider remains
//! unavailable until a retained-descriptor, non-sparse reserve primitive is integrated.  The
//! qualified fixture used by the tests supplies already-validated capacity observations; it does
//! not represent a physical reservation authority.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use thiserror::Error;

use super::admission_claims::{AdmissionClaimInventory, MAX_ADMISSION_CLAIMS};
use super::full_capacity::{
    FullCapacityAllocationClass, FullCapacityPlan, MAX_FULL_CAPACITY_ALLOCATIONS,
};
use crate::durable_capacity::{PROTECTED_FREE_SPACE_FLOOR, VolumeFacts, VolumeIdentity};

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
    next_token: u64,
    charges_by_token: BTreeMap<u64, BTreeMap<ChargeKey, u64>>,
    aggregate_charges: BTreeMap<ChargeKey, u64>,
    #[cfg(test)]
    fail_release: bool,
}

impl Default for ProvisionalLedger {
    fn default() -> Self {
        Self {
            next_token: INITIAL_LEASE_TOKEN,
            charges_by_token: BTreeMap::new(),
            aggregate_charges: BTreeMap::new(),
            #[cfg(test)]
            fail_release: false,
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
struct CapacityProvider {
    qualification: ProviderQualification,
    facts: Option<CapacityFacts>,
    ledger: Rc<RefCell<ProvisionalLedger>>,
}

impl CapacityProvider {
    fn unavailable() -> Self {
        Self {
            qualification: ProviderQualification::Unavailable,
            facts: None,
            ledger: Rc::new(RefCell::new(ProvisionalLedger::default())),
        }
    }

    #[cfg(test)]
    fn qualified_fixture(facts: BTreeMap<VolumeIdentity, VolumeFacts>) -> Self {
        Self {
            qualification: ProviderQualification::QualifiedFixture,
            facts: Some(CapacityFacts::new(facts)),
            ledger: Rc::new(RefCell::new(ProvisionalLedger::default())),
        }
    }

    fn admit(
        &self,
        plan: &FullCapacityPlan,
        claims: &AdmissionClaimInventory,
    ) -> Result<ProvisionalCapacityLease, CapacityProviderError> {
        if self.qualification == ProviderQualification::Unavailable {
            return Err(CapacityProviderError::Unavailable);
        }
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

        let facts = self
            .facts
            .as_ref()
            .ok_or(CapacityProviderError::UnsupportedFacts)?;
        let mut charge_by_key = BTreeMap::new();
        let mut planned_by_volume = BTreeMap::new();
        let mut seen_volumes = BTreeSet::new();
        for allocation in plan.allocations() {
            let volume = allocation.volume().clone();
            let observed = facts.by_volume.get(&volume).ok_or_else(|| {
                CapacityProviderError::FactsUnavailable {
                    volume: volume.clone(),
                }
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
                .by_volume
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
        ledger.next_token = next_token;
        for (key, bytes) in &charge_by_key {
            let aggregate = ledger.aggregate_charges.entry(key.clone()).or_insert(0);
            *aggregate = aggregate
                .checked_add(*bytes)
                .expect("provisional aggregate was checked before mutation");
        }
        ledger
            .charges_by_token
            .insert(next_token - 1, charge_by_key.clone());

        Ok(ProvisionalCapacityLease {
            ledger: Rc::clone(&self.ledger),
            token: next_token - 1,
            charges: charge_by_key,
            released: false,
        })
    }

    fn admission_gate(&self) -> Result<(), ()> {
        match self.qualification {
            ProviderQualification::Unavailable => Err(()),
            ProviderQualification::QualifiedFixture => Ok(()),
        }
    }

    #[cfg(test)]
    fn outstanding(&self, volume: &VolumeIdentity, class: FullCapacityAllocationClass) -> u64 {
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
    fn claim_count(&self) -> usize {
        self.ledger.borrow().charges_by_token.len()
    }

    #[cfg(test)]
    fn next_token(&self) -> u64 {
        self.ledger.borrow().next_token
    }

    #[cfg(test)]
    fn fail_release_for_test(&self) {
        self.ledger.borrow_mut().fail_release = true;
    }

    #[cfg(test)]
    fn allow_release_for_test(&self) {
        self.ledger.borrow_mut().fail_release = false;
    }
}

pub(crate) fn production_admission_gate() -> Result<(), ()> {
    CapacityProvider::unavailable().admission_gate()
}

/// A synchronous, non-serializable provisional charge.  It has no durable identity and cannot
/// be transferred to an operation intent in this milestone.
#[allow(dead_code)]
struct ProvisionalCapacityLease {
    ledger: Rc<RefCell<ProvisionalLedger>>,
    token: u64,
    charges: BTreeMap<ChargeKey, u64>,
    released: bool,
}

impl ProvisionalCapacityLease {
    fn release(&mut self) -> Result<(), CapacityProviderError> {
        if self.released {
            return Ok(());
        }
        let mut ledger = self
            .ledger
            .try_borrow_mut()
            .map_err(|_| CapacityProviderError::LedgerBusy)?;
        #[cfg(test)]
        if ledger.fail_release {
            return Err(CapacityProviderError::ReleaseFailed);
        }
        let Some(owned_charges) = ledger.charges_by_token.get(&self.token) else {
            return Err(CapacityProviderError::ReleaseFailed);
        };
        if owned_charges != &self.charges {
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
        self.released = true;
        Ok(())
    }

    fn transfer_to_intent(&self) -> Result<(), CapacityProviderError> {
        Err(CapacityProviderError::IntentTransferUnavailable)
    }
}

impl Drop for ProvisionalCapacityLease {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
enum CapacityProviderError {
    #[error("capacity provider is unavailable")]
    Unavailable,
    #[error("capacity plan validation failed")]
    InvalidPlan,
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
    #[error("provisional capacity cannot transfer to a durable intent yet")]
    IntentTransferUnavailable,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_journal::full_capacity_planner::{
        CapacityAllocationNature, CapacityEstimate, CapacityRequirement, derive_full_capacity_plan,
    };
    use crate::operation_journal::{
        AdmissionClaim, AdmissionClaimId, FullCapacityAllocation, FullCapacityPlan,
    };

    fn facts(device: u64, free_bytes: u64, allocation_unit: u64) -> VolumeFacts {
        VolumeFacts {
            identity: VolumeIdentity { device },
            free_bytes,
            allocation_unit,
        }
    }

    fn plan_and_claims(
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
}
