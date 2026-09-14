//! Bounded observations for capacity admission and later settlement.
//!
//! These values bind an immutable [`DurableCapacityPlan`] to the claim rows an owner observed.
//! They do not reserve capacity, prove that bytes were allocated, or authorize a journal
//! transition.  A future V16 owner must add its private admission authority before using these
//! observations in a state transition.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::durable_capacity::{CapacityAllocationClass, DurableCapacityPlan, VolumeIdentity};

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
    allocation_class: CapacityAllocationClass,
    allocated_bytes: u64,
}

impl AdmissionClaim {
    /// Construct one positive, non-nil claim observation.
    pub fn new(
        claim_id: AdmissionClaimId,
        volume: VolumeIdentity,
        allocation_class: CapacityAllocationClass,
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
    pub const fn allocation_class(&self) -> CapacityAllocationClass {
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
        plan: &DurableCapacityPlan,
    ) -> Result<(), AdmissionClaimValidationError> {
        validate_claim_rows(&self.claims)?;
        validate_plan_rows(plan)?;

        for claim in &self.claims {
            if !plan.volumes.iter().any(|expected| {
                expected.identity == *claim.volume()
                    && expected.allocation_class == claim.allocation_class()
            }) {
                return Err(AdmissionClaimValidationError::UnknownAllocation {
                    claim_id: claim.claim_id(),
                    volume: claim.volume().clone(),
                    allocation_class: claim.allocation_class(),
                });
            }
        }

        for expected in &plan.volumes {
            let mut observed = 0_u64;
            for claim in &self.claims {
                if claim.volume() == &expected.identity
                    && claim.allocation_class() == expected.allocation_class
                {
                    observed = observed
                        .checked_add(claim.allocated_bytes())
                        .ok_or(AdmissionClaimValidationError::ArithmeticOverflow)?;
                }
            }
            if observed == 0 {
                return Err(AdmissionClaimValidationError::MissingAllocation {
                    volume: expected.identity.clone(),
                    allocation_class: expected.allocation_class,
                    expected_bytes: expected.allocated_bytes,
                });
            }
            if observed < expected.allocated_bytes {
                return Err(AdmissionClaimValidationError::UnderAllocated {
                    volume: expected.identity.clone(),
                    allocation_class: expected.allocation_class,
                    expected_bytes: expected.allocated_bytes,
                    observed_bytes: observed,
                });
            }
            if observed > expected.allocated_bytes {
                return Err(AdmissionClaimValidationError::OverAllocated {
                    volume: expected.identity.clone(),
                    allocation_class: expected.allocation_class,
                    expected_bytes: expected.allocated_bytes,
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
    /// The claim's allocation was released.
    Released,
    /// The claim's allocation was consumed by or transferred to the durable result.
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

fn validate_plan_rows(plan: &DurableCapacityPlan) -> Result<(), AdmissionClaimValidationError> {
    if plan.volumes.is_empty() {
        return Err(AdmissionClaimValidationError::EmptyCapacityPlan);
    }
    if plan.volumes.len() > MAX_ADMISSION_CLAIMS {
        return Err(AdmissionClaimValidationError::TooManyExpectedRows {
            actual: plan.volumes.len(),
            maximum: MAX_ADMISSION_CLAIMS,
        });
    }
    for (index, expected) in plan.volumes.iter().enumerate() {
        if expected.allocated_bytes == 0 {
            return Err(AdmissionClaimValidationError::ZeroExpectedBytes {
                volume: expected.identity.clone(),
                allocation_class: expected.allocation_class,
            });
        }
        if plan.volumes[..index].iter().any(|prior| {
            prior.identity == expected.identity
                && prior.allocation_class == expected.allocation_class
        }) {
            return Err(AdmissionClaimValidationError::DuplicateExpectedAllocation {
                volume: expected.identity.clone(),
                allocation_class: expected.allocation_class,
            });
        }
    }
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
        allocation_class: CapacityAllocationClass,
    },
    /// Zero-sized expected rows are rejected explicitly by this contract.
    #[error(
        "admission capacity plan has zero bytes for volume {volume:?} and class {allocation_class:?}"
    )]
    ZeroExpectedBytes {
        /// Zero-sized expected volume.
        volume: VolumeIdentity,
        /// Zero-sized expected class.
        allocation_class: CapacityAllocationClass,
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
        allocation_class: CapacityAllocationClass,
    },
    /// No positive claim covered an expected plan row.
    #[error(
        "admission plan allocation is missing for volume {volume:?} and class {allocation_class:?}: expected {expected_bytes} bytes"
    )]
    MissingAllocation {
        /// Missing expected volume identity.
        volume: VolumeIdentity,
        /// Missing expected class.
        allocation_class: CapacityAllocationClass,
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
        allocation_class: CapacityAllocationClass,
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
        allocation_class: CapacityAllocationClass,
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
    use crate::durable_capacity::{DurableVolumeCapacity, PROTECTED_FREE_SPACE_FLOOR};

    fn claim_id(value: u128) -> AdmissionClaimId {
        AdmissionClaimId::from_uuid(Uuid::from_u128(value)).expect("claim id")
    }

    fn plan_row(
        device: u64,
        allocation_class: CapacityAllocationClass,
        allocated_bytes: u64,
    ) -> DurableVolumeCapacity {
        DurableVolumeCapacity {
            identity: VolumeIdentity { device },
            allocation_unit: 1,
            allocation_class,
            logical_bytes: allocated_bytes,
            allocated_bytes,
            protected_free_bytes: PROTECTED_FREE_SPACE_FLOOR,
        }
    }

    fn plan(rows: &[(u64, CapacityAllocationClass, u64)]) -> DurableCapacityPlan {
        DurableCapacityPlan {
            volumes: rows
                .iter()
                .map(|(device, allocation_class, bytes)| {
                    plan_row(*device, *allocation_class, *bytes)
                })
                .collect(),
        }
    }

    fn claim(
        id: u128,
        device: u64,
        allocation_class: CapacityAllocationClass,
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
                CapacityAllocationClass::DestinationStaging,
                0,
            ),
            Err(AdmissionClaimValidationError::ZeroClaimBytes { .. })
        ));
        assert!(matches!(
            AdmissionClaimInventory::new(vec![
                claim(1, 1, CapacityAllocationClass::DestinationStaging, 1),
                claim(1, 1, CapacityAllocationClass::DestinationStaging, 1),
            ]),
            Err(AdmissionClaimValidationError::DuplicateClaimId { .. })
        ));
    }

    #[test]
    fn exact_aggregate_coverage_accepts_split_claims_and_rejects_plan_ambiguity() {
        let capacity_plan = plan(&[
            (1, CapacityAllocationClass::DestinationStaging, 10),
            (1, CapacityAllocationClass::JournalRecord, 5),
            (2, CapacityAllocationClass::ExistingRecoveryPayload, 3),
        ]);
        let inventory = AdmissionClaimInventory::new(vec![
            claim(1, 1, CapacityAllocationClass::DestinationStaging, 4),
            claim(2, 1, CapacityAllocationClass::DestinationStaging, 6),
            claim(3, 1, CapacityAllocationClass::JournalRecord, 5),
            claim(4, 2, CapacityAllocationClass::ExistingRecoveryPayload, 3),
        ])
        .expect("claims");
        inventory
            .validate_against(&capacity_plan)
            .expect("exact coverage");

        let duplicate_plan = plan(&[
            (1, CapacityAllocationClass::DestinationStaging, 4),
            (1, CapacityAllocationClass::DestinationStaging, 6),
        ]);
        assert!(matches!(
            inventory.validate_against(&duplicate_plan),
            Err(AdmissionClaimValidationError::DuplicateExpectedAllocation { .. })
        ));
        let zero_plan = plan(&[(1, CapacityAllocationClass::DestinationStaging, 0)]);
        assert_eq!(
            inventory.validate_against(&zero_plan),
            Err(AdmissionClaimValidationError::ZeroExpectedBytes {
                volume: VolumeIdentity { device: 1 },
                allocation_class: CapacityAllocationClass::DestinationStaging,
            })
        );
        assert_eq!(
            inventory.validate_against(&DurableCapacityPlan::default()),
            Err(AdmissionClaimValidationError::EmptyCapacityPlan)
        );
    }

    #[test]
    fn coverage_reports_missing_under_over_unknown_and_overflow() {
        let two_rows = plan(&[
            (1, CapacityAllocationClass::DestinationStaging, 10),
            (1, CapacityAllocationClass::JournalRecord, 4),
        ]);
        let missing = AdmissionClaimInventory::new(vec![claim(
            1,
            1,
            CapacityAllocationClass::DestinationStaging,
            10,
        )])
        .expect("missing inventory");
        assert!(matches!(
            missing.validate_against(&two_rows),
            Err(AdmissionClaimValidationError::MissingAllocation {
                allocation_class: CapacityAllocationClass::JournalRecord,
                ..
            })
        ));

        let under = AdmissionClaimInventory::new(vec![claim(
            1,
            1,
            CapacityAllocationClass::DestinationStaging,
            9,
        )])
        .expect("under inventory");
        assert!(matches!(
            under.validate_against(&plan(&[(
                1,
                CapacityAllocationClass::DestinationStaging,
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
            CapacityAllocationClass::DestinationStaging,
            11,
        )])
        .expect("over inventory");
        assert!(matches!(
            over.validate_against(&plan(&[(
                1,
                CapacityAllocationClass::DestinationStaging,
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
            CapacityAllocationClass::JournalRecord,
            1,
        )])
        .expect("unknown inventory");
        assert!(matches!(
            unknown.validate_against(&plan(&[(
                1,
                CapacityAllocationClass::DestinationStaging,
                1
            )])),
            Err(AdmissionClaimValidationError::UnknownAllocation { .. })
        ));

        let overflow = AdmissionClaimInventory::new(vec![
            claim(1, 1, CapacityAllocationClass::DestinationStaging, u64::MAX),
            claim(2, 1, CapacityAllocationClass::DestinationStaging, 1),
        ])
        .expect("overflow inventory");
        assert_eq!(
            overflow.validate_against(&plan(&[(
                1,
                CapacityAllocationClass::DestinationStaging,
                u64::MAX
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
                    CapacityAllocationClass::DestinationStaging,
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
            claim(1, 1, CapacityAllocationClass::DestinationStaging, 1),
            claim(2, 1, CapacityAllocationClass::JournalRecord, 2),
            claim(3, 2, CapacityAllocationClass::ExistingDestination, 3),
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
}
