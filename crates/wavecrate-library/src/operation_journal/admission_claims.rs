//! Bounded observations for capacity admission and later settlement.
//!
//! These values bind an immutable [`super::full_capacity::FullCapacityPlan`] to the claim rows an
//! owner observed.
//! They do not reserve capacity, prove that bytes were allocated, or authorize a journal
//! transition.  A future V16 owner must add its private admission authority before using these
//! observations in a state transition.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use super::full_capacity::{
    FullCapacityAllocationClass, FullCapacityPlan, FullCapacityValidationError,
};
use crate::durable_capacity::VolumeIdentity;

/// Maximum number of capacity claims or settlement rows in one bounded observation.
pub const MAX_ADMISSION_CLAIMS: usize = 256;

/// Non-nil identity for one observed capacity claim.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AdmissionClaimId(Uuid);

impl AdmissionClaimId {
    /// Generate a new claim identity.  This creates an identifier only; it does not admit a claim.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap a UUID after rejecting the nil identity.
    pub fn from_uuid(value: Uuid) -> Result<Self, AdmissionClaimValidationError> {
        if value.is_nil() {
            return Err(AdmissionClaimValidationError::NilClaimId);
        }
        Ok(Self(value))
    }

    /// Return the UUID representation of this claim identity.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }

    /// Validate an identity that may have come from an untrusted decoder.
    pub const fn validate(self) -> Result<(), AdmissionClaimValidationError> {
        if self.0.is_nil() {
            Err(AdmissionClaimValidationError::NilClaimId)
        } else {
            Ok(())
        }
    }
}

impl Default for AdmissionClaimId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AdmissionClaimId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// One bounded observed allocation claim.
///
/// The row records what an owner reported for one claim.  It is not a reservation handle or
/// proof that the allocation exists on a live volume.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionClaim {
    claim_id: AdmissionClaimId,
    volume: VolumeIdentity,
    allocation_class: FullCapacityAllocationClass,
    allocated_bytes: u64,
}

impl AdmissionClaim {
    /// Construct one positive, non-nil claim observation.
    pub fn new(
        claim_id: AdmissionClaimId,
        volume: VolumeIdentity,
        allocation_class: FullCapacityAllocationClass,
        allocated_bytes: u64,
    ) -> Result<Self, AdmissionClaimValidationError> {
        claim_id.validate()?;
        if allocated_bytes == 0 {
            return Err(AdmissionClaimValidationError::ZeroClaimBytes { claim_id });
        }
        Ok(Self {
            claim_id,
            volume,
            allocation_class,
            allocated_bytes,
        })
    }

    /// Return the claim identity.
    pub const fn claim_id(&self) -> AdmissionClaimId {
        self.claim_id
    }

    /// Return the descriptor-derived volume identity observation.
    pub const fn volume(&self) -> &VolumeIdentity {
        &self.volume
    }

    /// Return the closed allocation class.
    pub const fn allocation_class(&self) -> FullCapacityAllocationClass {
        self.allocation_class
    }

    /// Return the observed allocated byte count.
    pub const fn allocated_bytes(&self) -> u64 {
        self.allocated_bytes
    }

    /// Validate one decoded claim observation without granting admission authority.
    pub fn validate(&self) -> Result<(), AdmissionClaimValidationError> {
        self.claim_id.validate()?;
        if self.allocated_bytes == 0 {
            return Err(AdmissionClaimValidationError::ZeroClaimBytes {
                claim_id: self.claim_id,
            });
        }
        Ok(())
    }
}

/// Immutable bounded inventory of observed admission claims.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionClaimInventory {
    claims: Vec<AdmissionClaim>,
}

impl AdmissionClaimInventory {
    /// Construct a nonempty immutable claim inventory after bounded shape validation.
    pub fn new(claims: Vec<AdmissionClaim>) -> Result<Self, AdmissionClaimValidationError> {
        validate_claim_rows(&claims)?;
        Ok(Self { claims })
    }

    /// Borrow the exact claim rows in their captured order.
    pub fn claims(&self) -> &[AdmissionClaim] {
        &self.claims
    }

    /// Validate decoded claim shape without treating it as a live reservation.
    pub fn validate(&self) -> Result<(), AdmissionClaimValidationError> {
        validate_claim_rows(&self.claims)
    }

    /// Require exact aggregate coverage of every `(volume, allocation class)` plan row.
    ///
    /// Validation first checks all bounds and duplicate plan keys before any plan identity is
    /// cloned.  Every claimed pair must exist in the plan, and every expected row must have the
    /// exact checked sum of positive claim bytes.  This method remains an observation check; it
    /// does not reserve or release capacity.
    pub fn validate_against(
        &self,
        plan: &FullCapacityPlan,
    ) -> Result<(), AdmissionClaimValidationError> {
        validate_claim_rows(&self.claims)?;
        validate_plan_rows(plan)?;

        for claim in &self.claims {
            if !plan.allocations().iter().any(|expected| {
                expected.volume() == claim.volume() && expected.class() == claim.allocation_class()
            }) {
                return Err(AdmissionClaimValidationError::UnknownAllocation {
                    claim_id: claim.claim_id(),
                    volume: claim.volume().clone(),
                    allocation_class: claim.allocation_class(),
                });
            }
        }

        for expected in plan.allocations() {
            let mut observed = 0_u64;
            for claim in &self.claims {
                if claim.volume() == expected.volume()
                    && claim.allocation_class() == expected.class()
                {
                    observed = observed
                        .checked_add(claim.allocated_bytes())
                        .ok_or(AdmissionClaimValidationError::ArithmeticOverflow)?;
                }
            }
            if observed == 0 {
                return Err(AdmissionClaimValidationError::MissingAllocation {
                    volume: expected.volume().clone(),
                    allocation_class: expected.class(),
                    expected_bytes: expected.allocated_bytes(),
                });
            }
            if observed < expected.allocated_bytes() {
                return Err(AdmissionClaimValidationError::UnderAllocated {
                    volume: expected.volume().clone(),
                    allocation_class: expected.class(),
                    expected_bytes: expected.allocated_bytes(),
                    observed_bytes: observed,
                });
            }
            if observed > expected.allocated_bytes() {
                return Err(AdmissionClaimValidationError::OverAllocated {
                    volume: expected.volume().clone(),
                    allocation_class: expected.class(),
                    expected_bytes: expected.allocated_bytes(),
                    observed_bytes: observed,
                });
            }
        }
        Ok(())
    }
}

/// Closed outcome observed for one admitted claim after owner settlement work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AdmissionClaimSettlementOutcome {
    /// The operation's logical reservation or charge was closed by its owner.
    /// This does not establish filesystem absence or database page reclamation.
    Released,
    /// The allocation remains accounted by the durable-result or existing-data owner.
    ConsumedOrTransferred,
    /// The claim remains held and cannot satisfy a release-safe terminal predicate.
    Held,
}

/// One bounded observed settlement outcome for an admission claim.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionClaimSettlement {
    claim_id: AdmissionClaimId,
    outcome: AdmissionClaimSettlementOutcome,
}

impl AdmissionClaimSettlement {
    /// Construct one settlement observation; it does not authorize the settlement.
    pub fn new(
        claim_id: AdmissionClaimId,
        outcome: AdmissionClaimSettlementOutcome,
    ) -> Result<Self, AdmissionClaimValidationError> {
        claim_id.validate()?;
        Ok(Self { claim_id, outcome })
    }

    /// Return the settled claim identity.
    pub const fn claim_id(self) -> AdmissionClaimId {
        self.claim_id
    }

    /// Return the closed observed settlement outcome.
    pub const fn outcome(self) -> AdmissionClaimSettlementOutcome {
        self.outcome
    }
}

/// Immutable bounded settlement observations for one admission inventory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionClaimSettlementInventory {
    settlements: Vec<AdmissionClaimSettlement>,
}

impl AdmissionClaimSettlementInventory {
    /// Construct a nonempty immutable settlement observation.
    pub fn new(
        settlements: Vec<AdmissionClaimSettlement>,
    ) -> Result<Self, AdmissionClaimValidationError> {
        validate_settlement_rows(&settlements)?;
        Ok(Self { settlements })
    }

    /// Borrow settlement observations in their captured order.
    pub fn settlements(&self) -> &[AdmissionClaimSettlement] {
        &self.settlements
    }

    /// Validate decoded settlement shape without granting transition authority.
    pub fn validate(&self) -> Result<(), AdmissionClaimValidationError> {
        validate_settlement_rows(&self.settlements)
    }

    /// Require exactly one settlement outcome for every admitted claim.
    pub fn validate_against(
        &self,
        claims: &AdmissionClaimInventory,
    ) -> Result<(), AdmissionClaimValidationError> {
        claims.validate()?;
        validate_settlement_rows(&self.settlements)?;
        if self.settlements.len() != claims.claims.len() {
            return Err(AdmissionClaimValidationError::SettlementCountMismatch {
                expected: claims.claims.len(),
                actual: self.settlements.len(),
            });
        }
        for settlement in &self.settlements {
            if !claims
                .claims
                .iter()
                .any(|claim| claim.claim_id() == settlement.claim_id())
            {
                return Err(AdmissionClaimValidationError::UnknownSettlementClaim {
                    claim_id: settlement.claim_id(),
                });
            }
        }
        for claim in &claims.claims {
            if !self
                .settlements
                .iter()
                .any(|settlement| settlement.claim_id() == claim.claim_id())
            {
                return Err(AdmissionClaimValidationError::MissingSettlement {
                    claim_id: claim.claim_id(),
                });
            }
        }
        Ok(())
    }

    /// Classify whether these exact settlement observations satisfy a prepublication
    /// cancellation's allocation policy.
    ///
    /// This first validates one settlement row for every admitted claim.  It is an observation
    /// predicate only: it does not prove filesystem absence, database reclamation, or release
    /// authority.  A `Held` row always makes the predicate false.
    pub fn satisfies_prepublication_cancellation(
        &self,
        claims: &AdmissionClaimInventory,
    ) -> Result<bool, AdmissionClaimValidationError> {
        self.validate_against(claims)?;
        for claim in claims.claims() {
            let settlement = self
                .settlements
                .iter()
                .find(|settlement| settlement.claim_id() == claim.claim_id())
                .expect("validated settlement coverage");
            let acceptable = match claim.allocation_class() {
                FullCapacityAllocationClass::DestinationStaging
                | FullCapacityAllocationClass::OperationRecoveryPayload => {
                    settlement.outcome() == AdmissionClaimSettlementOutcome::Released
                }
                FullCapacityAllocationClass::JournalRecord
                | FullCapacityAllocationClass::ExistingDestination
                | FullCapacityAllocationClass::ExistingRecoveryPayload => {
                    settlement.outcome() == AdmissionClaimSettlementOutcome::ConsumedOrTransferred
                }
                FullCapacityAllocationClass::SourceDatabaseCommit
                | FullCapacityAllocationClass::SourceWalShm
                | FullCapacityAllocationClass::GlobalDatabaseCommit
                | FullCapacityAllocationClass::GlobalWalShm => matches!(
                    settlement.outcome(),
                    AdmissionClaimSettlementOutcome::Released
                        | AdmissionClaimSettlementOutcome::ConsumedOrTransferred
                ),
            };
            if !acceptable {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Validate exact settlement coverage for normal success.
    ///
    /// Retained journal and existing-data charges may be consumed or transferred, but a normal
    /// success settlement may not report them as released. Other allocation classes retain their
    /// existing released or consumed settlement allowances. `Held` remains a valid nonterminal
    /// observation for every class.
    pub(crate) fn validate_normal_success_settlement(
        &self,
        claims: &AdmissionClaimInventory,
    ) -> Result<(), AdmissionClaimValidationError> {
        self.validate_against(claims)?;
        for claim in claims.claims() {
            let settlement = self
                .settlements
                .iter()
                .find(|settlement| settlement.claim_id() == claim.claim_id())
                .expect("validated settlement coverage");
            let retained_charge = matches!(
                claim.allocation_class(),
                FullCapacityAllocationClass::JournalRecord
                    | FullCapacityAllocationClass::ExistingDestination
                    | FullCapacityAllocationClass::ExistingRecoveryPayload
            );
            if retained_charge && settlement.outcome() == AdmissionClaimSettlementOutcome::Released
            {
                return Err(
                    AdmissionClaimValidationError::NormalSuccessRetainedChargeReleased {
                        claim_id: claim.claim_id(),
                        allocation_class: claim.allocation_class(),
                    },
                );
            }
        }
        Ok(())
    }

    /// Return whether every retained settlement observation is `Released`.
    ///
    /// This is a classification predicate over observations, not a release proof.  Callers must
    /// first validate exact coverage against the admitted claim inventory.
    pub fn all_released(&self) -> bool {
        !self.settlements.is_empty()
            && self
                .settlements
                .iter()
                .all(|settlement| settlement.outcome() == AdmissionClaimSettlementOutcome::Released)
    }

    /// Return whether every retained settlement is settled without a `Held` outcome.
    ///
    /// This is a classification predicate over observations, not a cancellation authority.
    pub fn all_settled_without_held(&self) -> bool {
        !self.settlements.is_empty()
            && self.settlements.iter().all(|settlement| {
                matches!(
                    settlement.outcome(),
                    AdmissionClaimSettlementOutcome::Released
                        | AdmissionClaimSettlementOutcome::ConsumedOrTransferred
                )
            })
    }
}

fn validate_claim_rows(claims: &[AdmissionClaim]) -> Result<(), AdmissionClaimValidationError> {
    if claims.is_empty() {
        return Err(AdmissionClaimValidationError::EmptyClaimInventory);
    }
    if claims.len() > MAX_ADMISSION_CLAIMS {
        return Err(AdmissionClaimValidationError::TooManyClaims {
            actual: claims.len(),
            maximum: MAX_ADMISSION_CLAIMS,
        });
    }
    for (index, claim) in claims.iter().enumerate() {
        claim.validate()?;
        if claims[..index]
            .iter()
            .any(|prior| prior.claim_id() == claim.claim_id())
        {
            return Err(AdmissionClaimValidationError::DuplicateClaimId {
                claim_id: claim.claim_id(),
            });
        }
    }
    Ok(())
}

fn validate_plan_rows(plan: &FullCapacityPlan) -> Result<(), AdmissionClaimValidationError> {
    if plan.allocations().is_empty() {
        return Err(AdmissionClaimValidationError::EmptyCapacityPlan);
    }
    if plan.allocations().len() > MAX_ADMISSION_CLAIMS {
        return Err(AdmissionClaimValidationError::TooManyExpectedRows {
            actual: plan.allocations().len(),
            maximum: MAX_ADMISSION_CLAIMS,
        });
    }
    for (index, expected) in plan.allocations().iter().enumerate() {
        if expected.allocated_bytes() == 0 {
            return Err(AdmissionClaimValidationError::ZeroExpectedBytes {
                volume: expected.volume().clone(),
                allocation_class: expected.class(),
            });
        }
        if plan.allocations()[..index]
            .iter()
            .any(|prior| prior.volume() == expected.volume() && prior.class() == expected.class())
        {
            return Err(AdmissionClaimValidationError::DuplicateExpectedAllocation {
                volume: expected.volume().clone(),
                allocation_class: expected.class(),
            });
        }
    }
    plan.validate()?;
    Ok(())
}

/// Failure while validating bounded admission claims or settlement observations.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum AdmissionClaimValidationError {
    /// A claim identity was nil.
    #[error("admission claim identity must be non-nil")]
    NilClaimId,
    /// No claim rows were supplied.
    #[error("admission claim inventory must not be empty")]
    EmptyClaimInventory,
    /// The claim inventory exceeded the bounded row count.
    #[error("admission claim inventory contains {actual} rows, maximum is {maximum}")]
    TooManyClaims {
        /// Number of supplied claim rows.
        actual: usize,
        /// Maximum allowed claim rows.
        maximum: usize,
    },
    /// A claim identity occurred more than once.
    #[error("admission claim identity is duplicated: {claim_id}")]
    DuplicateClaimId {
        /// Duplicated claim identity.
        claim_id: AdmissionClaimId,
    },
    /// A claim reported no allocated bytes.
    #[error("admission claim {claim_id} has zero allocated bytes")]
    ZeroClaimBytes {
        /// Claim with the zero-sized observation.
        claim_id: AdmissionClaimId,
    },
    /// The supplied capacity plan had no expected rows.
    #[error("admission capacity plan must not be empty")]
    EmptyCapacityPlan,
    /// The capacity plan exceeded the bounded expected-row count.
    #[error("admission capacity plan contains {actual} rows, maximum is {maximum}")]
    TooManyExpectedRows {
        /// Number of expected plan rows.
        actual: usize,
        /// Maximum expected rows accepted by this contract.
        maximum: usize,
    },
    /// The plan repeated one `(volume, allocation class)` row.
    #[error("admission capacity plan repeats volume {volume:?} and class {allocation_class:?}")]
    DuplicateExpectedAllocation {
        /// Repeated volume identity.
        volume: VolumeIdentity,
        /// Repeated allocation class.
        allocation_class: FullCapacityAllocationClass,
    },
    /// Zero-sized expected rows are rejected explicitly by this contract.
    #[error(
        "admission capacity plan has zero bytes for volume {volume:?} and class {allocation_class:?}"
    )]
    ZeroExpectedBytes {
        /// Zero-sized expected volume.
        volume: VolumeIdentity,
        /// Zero-sized expected class.
        allocation_class: FullCapacityAllocationClass,
    },
    /// A claim named a `(volume, allocation class)` absent from the plan.
    #[error(
        "admission claim {claim_id} names unknown volume {volume:?} and class {allocation_class:?}"
    )]
    UnknownAllocation {
        /// Claim naming the unknown allocation pair.
        claim_id: AdmissionClaimId,
        /// Unknown volume identity.
        volume: VolumeIdentity,
        /// Unknown allocation class.
        allocation_class: FullCapacityAllocationClass,
    },
    /// No positive claim covered an expected plan row.
    #[error(
        "admission plan allocation is missing for volume {volume:?} and class {allocation_class:?}: expected {expected_bytes} bytes"
    )]
    MissingAllocation {
        /// Missing expected volume identity.
        volume: VolumeIdentity,
        /// Missing expected class.
        allocation_class: FullCapacityAllocationClass,
        /// Expected positive bytes.
        expected_bytes: u64,
    },
    /// Claims covered fewer bytes than expected.
    #[error(
        "admission plan allocation is under-covered for volume {volume:?} and class {allocation_class:?}: expected {expected_bytes}, observed {observed_bytes}"
    )]
    UnderAllocated {
        /// Under-covered volume identity.
        volume: VolumeIdentity,
        /// Under-covered allocation class.
        allocation_class: FullCapacityAllocationClass,
        /// Expected bytes.
        expected_bytes: u64,
        /// Observed bytes.
        observed_bytes: u64,
    },
    /// Claims covered more bytes than expected.
    #[error(
        "admission plan allocation is over-covered for volume {volume:?} and class {allocation_class:?}: expected {expected_bytes}, observed {observed_bytes}"
    )]
    OverAllocated {
        /// Over-covered volume identity.
        volume: VolumeIdentity,
        /// Over-covered allocation class.
        allocation_class: FullCapacityAllocationClass,
        /// Expected bytes.
        expected_bytes: u64,
        /// Observed bytes.
        observed_bytes: u64,
    },
    /// Checked aggregate claim arithmetic overflowed.
    #[error("admission claim byte arithmetic overflowed")]
    ArithmeticOverflow,
    /// No settlement rows were supplied.
    #[error("admission settlement inventory must not be empty")]
    EmptySettlementInventory,
    /// The settlement inventory exceeded the bounded row count.
    #[error("admission settlement inventory contains {actual} rows, maximum is {maximum}")]
    TooManySettlements {
        /// Number of supplied settlement rows.
        actual: usize,
        /// Maximum allowed settlement rows.
        maximum: usize,
    },
    /// A settlement identity occurred more than once.
    #[error("admission settlement identity is duplicated: {claim_id}")]
    DuplicateSettlementClaimId {
        /// Duplicated settlement identity.
        claim_id: AdmissionClaimId,
    },
    /// Settlement rows did not have one row for each claim.
    #[error("admission settlement count {actual} does not match claim count {expected}")]
    SettlementCountMismatch {
        /// Number of admitted claims.
        expected: usize,
        /// Number of settlement rows.
        actual: usize,
    },
    /// A settlement named a claim absent from the admission inventory.
    #[error("admission settlement names unknown claim {claim_id}")]
    UnknownSettlementClaim {
        /// Unknown claim identity.
        claim_id: AdmissionClaimId,
    },
    /// An admitted claim had no settlement row.
    #[error("admitted claim has no settlement row: {claim_id}")]
    MissingSettlement {
        /// Claim identity without a settlement.
        claim_id: AdmissionClaimId,
    },
    /// Normal success attempted to release a charge retained by the durable-result or existing-data
    /// owner.
    #[error(
        "normal-success settlement cannot release retained claim {claim_id} class {allocation_class:?}"
    )]
    NormalSuccessRetainedChargeReleased {
        /// Claim carrying the retained charge.
        claim_id: AdmissionClaimId,
        /// Retained allocation class whose charge cannot be released on normal success.
        allocation_class: FullCapacityAllocationClass,
    },
    /// A full-capacity plan failed validation before claim coverage could be checked.
    #[error("invalid full capacity plan: {0}")]
    FullCapacity(#[from] FullCapacityValidationError),
}

fn validate_settlement_rows(
    settlements: &[AdmissionClaimSettlement],
) -> Result<(), AdmissionClaimValidationError> {
    if settlements.is_empty() {
        return Err(AdmissionClaimValidationError::EmptySettlementInventory);
    }
    if settlements.len() > MAX_ADMISSION_CLAIMS {
        return Err(AdmissionClaimValidationError::TooManySettlements {
            actual: settlements.len(),
            maximum: MAX_ADMISSION_CLAIMS,
        });
    }
    for (index, settlement) in settlements.iter().enumerate() {
        settlement.claim_id().validate()?;
        if settlements[..index]
            .iter()
            .any(|prior| prior.claim_id() == settlement.claim_id())
        {
            return Err(AdmissionClaimValidationError::DuplicateSettlementClaimId {
                claim_id: settlement.claim_id(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR;
    use crate::operation_journal::full_capacity::{
        FULL_CAPACITY_PLAN_VERSION, FullCapacityAllocation,
    };

    fn claim_id(value: u128) -> AdmissionClaimId {
        AdmissionClaimId::from_uuid(Uuid::from_u128(value)).expect("claim id")
    }

    fn plan_row(
        device: u64,
        allocation_class: FullCapacityAllocationClass,
        allocated_bytes: u64,
    ) -> FullCapacityAllocation {
        FullCapacityAllocation::new(
            VolumeIdentity { device },
            1,
            allocation_class,
            allocated_bytes,
            allocated_bytes,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("plan row")
    }

    fn plan(rows: &[(u64, FullCapacityAllocationClass, u64)]) -> FullCapacityPlan {
        FullCapacityPlan::new(
            rows.iter()
                .map(|(device, allocation_class, bytes)| {
                    plan_row(*device, *allocation_class, *bytes)
                })
                .collect(),
        )
        .expect("capacity plan")
    }

    fn unchecked_plan(allocations: Vec<FullCapacityAllocation>) -> FullCapacityPlan {
        serde_json::from_value(serde_json::json!({
            "version": FULL_CAPACITY_PLAN_VERSION,
            "allocations": allocations,
        }))
        .expect("unchecked test plan shape")
    }

    fn claim(
        id: u128,
        device: u64,
        allocation_class: FullCapacityAllocationClass,
        allocated_bytes: u64,
    ) -> AdmissionClaim {
        AdmissionClaim::new(
            claim_id(id),
            VolumeIdentity { device },
            allocation_class,
            allocated_bytes,
        )
        .expect("claim")
    }

    fn settlement(id: u128, outcome: AdmissionClaimSettlementOutcome) -> AdmissionClaimSettlement {
        AdmissionClaimSettlement::new(claim_id(id), outcome).expect("settlement")
    }

    #[test]
    fn claim_ids_and_empty_or_zero_observations_fail_closed() {
        assert_eq!(
            AdmissionClaimId::from_uuid(Uuid::nil()),
            Err(AdmissionClaimValidationError::NilClaimId)
        );
        assert_eq!(
            AdmissionClaimInventory::new(Vec::new()),
            Err(AdmissionClaimValidationError::EmptyClaimInventory)
        );
        assert_eq!(
            AdmissionClaimSettlementInventory::new(Vec::new()),
            Err(AdmissionClaimValidationError::EmptySettlementInventory)
        );
        assert!(matches!(
            AdmissionClaim::new(
                claim_id(1),
                VolumeIdentity { device: 1 },
                FullCapacityAllocationClass::DestinationStaging,
                0,
            ),
            Err(AdmissionClaimValidationError::ZeroClaimBytes { .. })
        ));
        assert!(matches!(
            AdmissionClaimInventory::new(vec![
                claim(1, 1, FullCapacityAllocationClass::DestinationStaging, 1),
                claim(1, 1, FullCapacityAllocationClass::DestinationStaging, 1),
            ]),
            Err(AdmissionClaimValidationError::DuplicateClaimId { .. })
        ));
    }

    #[test]
    fn exact_aggregate_coverage_accepts_split_claims_and_rejects_plan_ambiguity() {
        let capacity_plan = plan(&[
            (1, FullCapacityAllocationClass::DestinationStaging, 10),
            (1, FullCapacityAllocationClass::JournalRecord, 5),
            (2, FullCapacityAllocationClass::ExistingRecoveryPayload, 3),
        ]);
        let inventory = AdmissionClaimInventory::new(vec![
            claim(1, 1, FullCapacityAllocationClass::DestinationStaging, 4),
            claim(2, 1, FullCapacityAllocationClass::DestinationStaging, 6),
            claim(3, 1, FullCapacityAllocationClass::JournalRecord, 5),
            claim(
                4,
                2,
                FullCapacityAllocationClass::ExistingRecoveryPayload,
                3,
            ),
        ])
        .expect("claims");
        inventory
            .validate_against(&capacity_plan)
            .expect("exact coverage");

        let duplicate_plan = unchecked_plan(vec![
            plan_row(1, FullCapacityAllocationClass::DestinationStaging, 4),
            plan_row(1, FullCapacityAllocationClass::DestinationStaging, 6),
        ]);
        assert!(matches!(
            inventory.validate_against(&duplicate_plan),
            Err(AdmissionClaimValidationError::DuplicateExpectedAllocation { .. })
        ));
        let zero_plan: FullCapacityPlan = serde_json::from_value(serde_json::json!({
            "version": FULL_CAPACITY_PLAN_VERSION,
            "allocations": [{
                "volume": {"device": 1},
                "allocation_unit": 1,
                "class": "DestinationStaging",
                "logical_bytes": 0,
                "allocated_bytes": 0,
                "protected_free_bytes": PROTECTED_FREE_SPACE_FLOOR,
            }]
        }))
        .expect("zero test plan shape");
        assert_eq!(
            inventory.validate_against(&zero_plan),
            Err(AdmissionClaimValidationError::ZeroExpectedBytes {
                volume: VolumeIdentity { device: 1 },
                allocation_class: FullCapacityAllocationClass::DestinationStaging,
            })
        );
        assert_eq!(
            inventory.validate_against(&unchecked_plan(Vec::new())),
            Err(AdmissionClaimValidationError::EmptyCapacityPlan)
        );
    }

    #[test]
    fn coverage_reports_missing_under_over_unknown_and_overflow() {
        let two_rows = plan(&[
            (1, FullCapacityAllocationClass::DestinationStaging, 10),
            (1, FullCapacityAllocationClass::JournalRecord, 4),
        ]);
        let missing = AdmissionClaimInventory::new(vec![claim(
            1,
            1,
            FullCapacityAllocationClass::DestinationStaging,
            10,
        )])
        .expect("missing inventory");
        assert!(matches!(
            missing.validate_against(&two_rows),
            Err(AdmissionClaimValidationError::MissingAllocation {
                allocation_class: FullCapacityAllocationClass::JournalRecord,
                ..
            })
        ));

        let under = AdmissionClaimInventory::new(vec![claim(
            1,
            1,
            FullCapacityAllocationClass::DestinationStaging,
            9,
        )])
        .expect("under inventory");
        assert!(matches!(
            under.validate_against(&plan(&[(
                1,
                FullCapacityAllocationClass::DestinationStaging,
                10
            )])),
            Err(AdmissionClaimValidationError::UnderAllocated {
                expected_bytes: 10,
                observed_bytes: 9,
                ..
            })
        ));

        let over = AdmissionClaimInventory::new(vec![claim(
            1,
            1,
            FullCapacityAllocationClass::DestinationStaging,
            11,
        )])
        .expect("over inventory");
        assert!(matches!(
            over.validate_against(&plan(&[(
                1,
                FullCapacityAllocationClass::DestinationStaging,
                10
            )])),
            Err(AdmissionClaimValidationError::OverAllocated {
                expected_bytes: 10,
                observed_bytes: 11,
                ..
            })
        ));

        let unknown = AdmissionClaimInventory::new(vec![claim(
            1,
            1,
            FullCapacityAllocationClass::JournalRecord,
            1,
        )])
        .expect("unknown inventory");
        assert!(matches!(
            unknown.validate_against(&plan(&[(
                1,
                FullCapacityAllocationClass::DestinationStaging,
                1
            )])),
            Err(AdmissionClaimValidationError::UnknownAllocation { .. })
        ));

        let overflow = AdmissionClaimInventory::new(vec![
            claim(
                1,
                1,
                FullCapacityAllocationClass::DestinationStaging,
                u64::MAX,
            ),
            claim(2, 1, FullCapacityAllocationClass::DestinationStaging, 1),
        ])
        .expect("overflow inventory");
        assert_eq!(
            overflow.validate_against(&plan(&[(
                1,
                FullCapacityAllocationClass::DestinationStaging,
                1
            )])),
            Err(AdmissionClaimValidationError::ArithmeticOverflow)
        );
    }

    #[test]
    fn claim_and_settlement_bounds_are_checked_before_retention() {
        let too_many_claims = (0..=MAX_ADMISSION_CLAIMS)
            .map(|index| {
                claim(
                    (index + 1) as u128,
                    1,
                    FullCapacityAllocationClass::DestinationStaging,
                    1,
                )
            })
            .collect();
        assert!(matches!(
            AdmissionClaimInventory::new(too_many_claims),
            Err(AdmissionClaimValidationError::TooManyClaims { .. })
        ));

        let too_many_settlements = (0..=MAX_ADMISSION_CLAIMS)
            .map(|index| settlement((index + 1) as u128, AdmissionClaimSettlementOutcome::Held))
            .collect();
        assert!(matches!(
            AdmissionClaimSettlementInventory::new(too_many_settlements),
            Err(AdmissionClaimValidationError::TooManySettlements { .. })
        ));
    }

    #[test]
    fn settlement_inventory_requires_each_claim_and_exposes_only_observation_predicates() {
        let claims = AdmissionClaimInventory::new(vec![
            claim(1, 1, FullCapacityAllocationClass::DestinationStaging, 1),
            claim(2, 1, FullCapacityAllocationClass::JournalRecord, 2),
            claim(3, 2, FullCapacityAllocationClass::ExistingDestination, 3),
        ])
        .expect("claims");
        let all_released = AdmissionClaimSettlementInventory::new(vec![
            settlement(1, AdmissionClaimSettlementOutcome::Released),
            settlement(2, AdmissionClaimSettlementOutcome::Released),
            settlement(3, AdmissionClaimSettlementOutcome::Released),
        ])
        .expect("settlements");
        all_released
            .validate_against(&claims)
            .expect("exact settlements");
        assert!(all_released.all_released());
        assert!(all_released.all_settled_without_held());

        let consumed = AdmissionClaimSettlementInventory::new(vec![
            settlement(1, AdmissionClaimSettlementOutcome::Released),
            settlement(2, AdmissionClaimSettlementOutcome::ConsumedOrTransferred),
            settlement(3, AdmissionClaimSettlementOutcome::Released),
        ])
        .expect("consumed settlements");
        consumed
            .validate_against(&claims)
            .expect("exact settlements");
        assert!(!consumed.all_released());
        assert!(consumed.all_settled_without_held());

        let held = AdmissionClaimSettlementInventory::new(vec![
            settlement(1, AdmissionClaimSettlementOutcome::Released),
            settlement(2, AdmissionClaimSettlementOutcome::Held),
            settlement(3, AdmissionClaimSettlementOutcome::Released),
        ])
        .expect("held settlements");
        held.validate_against(&claims).expect("exact settlements");
        assert!(!held.all_released());
        assert!(!held.all_settled_without_held());

        let duplicate = AdmissionClaimSettlementInventory::new(vec![
            settlement(1, AdmissionClaimSettlementOutcome::Released),
            settlement(1, AdmissionClaimSettlementOutcome::Held),
            settlement(2, AdmissionClaimSettlementOutcome::Released),
        ])
        .expect_err("duplicate settlement");
        assert!(matches!(
            duplicate,
            AdmissionClaimValidationError::DuplicateSettlementClaimId { .. }
        ));

        let unknown = AdmissionClaimSettlementInventory::new(vec![
            settlement(1, AdmissionClaimSettlementOutcome::Released),
            settlement(2, AdmissionClaimSettlementOutcome::Released),
            settlement(4, AdmissionClaimSettlementOutcome::Released),
        ])
        .expect("unknown settlement shape");
        assert!(matches!(
            unknown.validate_against(&claims),
            Err(AdmissionClaimValidationError::UnknownSettlementClaim { .. })
        ));
    }

    #[test]
    fn prepublication_settlement_policy_is_class_sensitive_and_exact() {
        let classes = [
            FullCapacityAllocationClass::DestinationStaging,
            FullCapacityAllocationClass::OperationRecoveryPayload,
            FullCapacityAllocationClass::JournalRecord,
            FullCapacityAllocationClass::ExistingDestination,
            FullCapacityAllocationClass::ExistingRecoveryPayload,
            FullCapacityAllocationClass::SourceDatabaseCommit,
            FullCapacityAllocationClass::SourceWalShm,
            FullCapacityAllocationClass::GlobalDatabaseCommit,
            FullCapacityAllocationClass::GlobalWalShm,
        ];
        let claims = AdmissionClaimInventory::new(
            classes
                .iter()
                .enumerate()
                .map(|(index, class)| claim((index + 1) as u128, 1, *class, 1))
                .collect(),
        )
        .expect("class-complete claims");

        let all_released = AdmissionClaimSettlementInventory::new(
            classes
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    settlement(
                        (index + 1) as u128,
                        AdmissionClaimSettlementOutcome::Released,
                    )
                })
                .collect(),
        )
        .expect("released settlements");
        assert!(
            !all_released
                .satisfies_prepublication_cancellation(&claims)
                .expect("exact released coverage")
        );

        let policy_valid = AdmissionClaimSettlementInventory::new(
            classes
                .iter()
                .enumerate()
                .map(|(index, class)| {
                    let outcome = match class {
                        FullCapacityAllocationClass::DestinationStaging
                        | FullCapacityAllocationClass::OperationRecoveryPayload
                        | FullCapacityAllocationClass::SourceDatabaseCommit
                        | FullCapacityAllocationClass::SourceWalShm
                        | FullCapacityAllocationClass::GlobalDatabaseCommit
                        | FullCapacityAllocationClass::GlobalWalShm => {
                            AdmissionClaimSettlementOutcome::Released
                        }
                        FullCapacityAllocationClass::JournalRecord
                        | FullCapacityAllocationClass::ExistingDestination
                        | FullCapacityAllocationClass::ExistingRecoveryPayload => {
                            AdmissionClaimSettlementOutcome::ConsumedOrTransferred
                        }
                    };
                    settlement((index + 1) as u128, outcome)
                })
                .collect(),
        )
        .expect("policy-valid settlements");
        assert!(
            policy_valid
                .satisfies_prepublication_cancellation(&claims)
                .expect("exact policy coverage")
        );

        let held = AdmissionClaimSettlementInventory::new(
            classes
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    settlement(
                        (index + 1) as u128,
                        if index == 0 {
                            AdmissionClaimSettlementOutcome::Held
                        } else {
                            AdmissionClaimSettlementOutcome::Released
                        },
                    )
                })
                .collect(),
        )
        .expect("held settlements");
        assert!(
            !held
                .satisfies_prepublication_cancellation(&claims)
                .expect("held is a valid observation")
        );

        let incomplete = AdmissionClaimSettlementInventory::new(
            classes
                .iter()
                .take(classes.len() - 1)
                .enumerate()
                .map(|(index, _)| {
                    settlement(
                        (index + 1) as u128,
                        AdmissionClaimSettlementOutcome::Released,
                    )
                })
                .collect(),
        )
        .expect("incomplete shape");
        assert!(matches!(
            incomplete.satisfies_prepublication_cancellation(&claims),
            Err(AdmissionClaimValidationError::SettlementCountMismatch { .. })
        ));
    }

    #[test]
    fn normal_success_settlement_preserves_retained_charge_classes() {
        let retained_classes = [
            FullCapacityAllocationClass::JournalRecord,
            FullCapacityAllocationClass::ExistingDestination,
            FullCapacityAllocationClass::ExistingRecoveryPayload,
        ];
        for (index, allocation_class) in retained_classes.iter().enumerate() {
            let id = (100 + index) as u128;
            let claims = AdmissionClaimInventory::new(vec![claim(id, 1, *allocation_class, 1)])
                .expect("retained claim");
            let released = AdmissionClaimSettlementInventory::new(vec![settlement(
                id,
                AdmissionClaimSettlementOutcome::Released,
            )])
            .expect("released settlement");
            assert_eq!(
                released.validate_normal_success_settlement(&claims),
                Err(
                    AdmissionClaimValidationError::NormalSuccessRetainedChargeReleased {
                        claim_id: claim_id(id),
                        allocation_class: *allocation_class,
                    }
                )
            );

            let held = AdmissionClaimSettlementInventory::new(vec![settlement(
                id,
                AdmissionClaimSettlementOutcome::Held,
            )])
            .expect("held settlement");
            held.validate_normal_success_settlement(&claims)
                .expect("held is a valid nonterminal observation");

            let consumed = AdmissionClaimSettlementInventory::new(vec![settlement(
                id,
                AdmissionClaimSettlementOutcome::ConsumedOrTransferred,
            )])
            .expect("consumed settlement");
            consumed
                .validate_normal_success_settlement(&claims)
                .expect("consumed retained charge is valid");
        }

        let unrelated_classes = [
            FullCapacityAllocationClass::DestinationStaging,
            FullCapacityAllocationClass::OperationRecoveryPayload,
            FullCapacityAllocationClass::SourceDatabaseCommit,
            FullCapacityAllocationClass::SourceWalShm,
            FullCapacityAllocationClass::GlobalDatabaseCommit,
            FullCapacityAllocationClass::GlobalWalShm,
        ];
        let claims = AdmissionClaimInventory::new(
            unrelated_classes
                .iter()
                .enumerate()
                .map(|(index, allocation_class)| {
                    claim((200 + index) as u128, 1, *allocation_class, 1)
                })
                .collect(),
        )
        .expect("unrelated claims");
        let released = AdmissionClaimSettlementInventory::new(
            unrelated_classes
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    settlement(
                        (200 + index) as u128,
                        AdmissionClaimSettlementOutcome::Released,
                    )
                })
                .collect(),
        )
        .expect("unrelated released settlements");
        released
            .validate_normal_success_settlement(&claims)
            .expect("unrelated released classes remain allowed");
    }
}
