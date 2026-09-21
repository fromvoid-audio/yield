//! Private descriptor-bound derivation of full-capacity observations.
//!
//! The planner only derives an observation. It does not reserve space, issue a claim, or make a
//! descriptor available to another owner. Every requirement borrows the descriptor whose volume
//! facts are used for that row; shared-existing deduplication also derives its object identity
//! from that same open descriptor.

use std::collections::BTreeMap;
use std::fs::File;

use thiserror::Error;

use super::full_capacity::{
    FullCapacityAllocation, FullCapacityAllocationClass, FullCapacityPlan,
    FullCapacityValidationError, MAX_FULL_CAPACITY_ALLOCATIONS,
};
use super::work_plan::MAX_WORK_PLAN_UNITS;
use crate::durable_capacity::{
    CapacityGateError, PROTECTED_FREE_SPACE_FLOOR, VolumeFacts, VolumeIdentity,
    descriptor_capacity_facts, round_up_allocation,
};
use crate::filesystem_identity::stable_filesystem_identity_from_open_file;

/// An estimate supplied by the owning operation component.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CapacityEstimate {
    /// A bounded logical byte estimate owned by the caller.
    Known(u64),
    /// The owner could not provide a safe estimate.
    Unknown,
}

/// Whether a requirement is a new allocation or retained physical data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CapacityAllocationNature {
    /// A new allocation that adds to the plan even when it uses the same descriptor as another
    /// requirement.
    NewAllocation,
    /// Existing retained data without a proven object identity.
    ExistingRetained,
    /// Existing retained data whose identity may be deduplicated when the open descriptor proves
    /// that two requirements refer to the same object.
    SharedExisting,
}

/// One private, non-serializable capacity requirement borrowed from a retained descriptor.
pub(super) struct CapacityRequirement<'a> {
    descriptor: &'a File,
    class: FullCapacityAllocationClass,
    estimate: CapacityEstimate,
    nature: CapacityAllocationNature,
    #[cfg(test)]
    test_facts: Option<VolumeFacts>,
}

impl<'a> CapacityRequirement<'a> {
    /// Construct a production requirement. Volume facts are read from `descriptor` during
    /// planning and no pathname is reopened.
    pub(super) fn new(
        descriptor: &'a File,
        class: FullCapacityAllocationClass,
        estimate: CapacityEstimate,
        nature: CapacityAllocationNature,
    ) -> Self {
        Self {
            descriptor,
            class,
            estimate,
            nature,
            #[cfg(test)]
            test_facts: None,
        }
    }

    #[cfg(test)]
    /// Construct deterministic test input while retaining the descriptor for object identity.
    pub(super) fn with_test_facts(
        descriptor: &'a File,
        facts: VolumeFacts,
        class: FullCapacityAllocationClass,
        estimate: CapacityEstimate,
        nature: CapacityAllocationNature,
    ) -> Self {
        Self {
            descriptor,
            class,
            estimate,
            nature,
            test_facts: Some(facts),
        }
    }

    fn facts(&self) -> Result<VolumeFacts, FullCapacityPlannerError> {
        #[cfg(test)]
        if let Some(facts) = &self.test_facts {
            return Ok(facts.clone());
        }
        descriptor_capacity_facts(self.descriptor)
            .map_err(|error| FullCapacityPlannerError::DescriptorFacts(error.to_string()))
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct AllocationKey {
    volume: VolumeIdentity,
    class: FullCapacityAllocationClass,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Aggregate {
    volume: VolumeIdentity,
    allocation_unit: u64,
    class: FullCapacityAllocationClass,
    logical_bytes: u64,
    allocated_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SharedExistingKey {
    volume: VolumeIdentity,
    allocation_unit: u64,
    class: FullCapacityAllocationClass,
    logical_bytes: u64,
    allocated_bytes: u64,
}

/// Derive a bounded full-capacity observation from retained descriptor-borrowing requirements.
pub(super) fn derive_full_capacity_plan(
    requirements: &[CapacityRequirement<'_>],
) -> Result<FullCapacityPlan, FullCapacityPlannerError> {
    if requirements.is_empty() {
        return Err(FullCapacityPlannerError::EmptyRequirements);
    }
    if requirements.len() > MAX_WORK_PLAN_UNITS {
        return Err(FullCapacityPlannerError::TooManyRequirements {
            actual: requirements.len(),
            maximum: MAX_WORK_PLAN_UNITS,
        });
    }

    let mut aggregates = BTreeMap::<AllocationKey, Aggregate>::new();
    let mut allocation_units = BTreeMap::<VolumeIdentity, u64>::new();
    let mut shared_identities = BTreeMap::<String, SharedExistingKey>::new();

    for requirement in requirements {
        validate_nature(requirement.class, requirement.nature)?;
        let facts = requirement.facts()?;
        let allocation_unit = facts.allocation_unit;
        if allocation_unit == 0 {
            return Err(FullCapacityPlannerError::ZeroAllocationUnit {
                volume: facts.identity,
            });
        }
        if let Some(previous) = allocation_units.insert(facts.identity.clone(), allocation_unit) {
            if previous != allocation_unit {
                return Err(FullCapacityPlannerError::ConflictingAllocationUnit {
                    volume: facts.identity,
                    expected: previous,
                    actual: allocation_unit,
                });
            }
        }

        let logical_bytes = match requirement.estimate {
            CapacityEstimate::Known(bytes) if bytes > 0 => bytes,
            CapacityEstimate::Known(_) => return Err(FullCapacityPlannerError::ZeroEstimate),
            CapacityEstimate::Unknown => return Err(FullCapacityPlannerError::UnknownEstimate),
        };
        let allocated_bytes =
            round_up_allocation(logical_bytes, allocation_unit).map_err(map_capacity_error)?;

        if requirement.nature == CapacityAllocationNature::SharedExisting {
            let object_identity = stable_filesystem_identity_from_open_file(requirement.descriptor)
                .ok_or(FullCapacityPlannerError::SharedIdentityUnavailable)?;
            let shared_key = SharedExistingKey {
                volume: facts.identity.clone(),
                allocation_unit,
                class: requirement.class,
                logical_bytes,
                allocated_bytes,
            };
            match shared_identities.get(&object_identity) {
                None => {
                    shared_identities.insert(object_identity.clone(), shared_key);
                }
                Some(previous) if previous == &shared_key => continue,
                Some(_) => return Err(FullCapacityPlannerError::ConflictingSharedIdentity),
            }
        }

        let key = AllocationKey {
            volume: facts.identity.clone(),
            class: requirement.class,
        };
        let aggregate = aggregates.entry(key).or_insert_with(|| Aggregate {
            volume: facts.identity,
            allocation_unit,
            class: requirement.class,
            logical_bytes: 0,
            allocated_bytes: 0,
        });
        aggregate.logical_bytes = aggregate
            .logical_bytes
            .checked_add(logical_bytes)
            .ok_or(FullCapacityPlannerError::ArithmeticOverflow)?;
        aggregate.allocated_bytes = aggregate
            .allocated_bytes
            .checked_add(allocated_bytes)
            .ok_or(FullCapacityPlannerError::ArithmeticOverflow)?;
    }

    if aggregates.len() > MAX_FULL_CAPACITY_ALLOCATIONS {
        return Err(FullCapacityPlannerError::TooManyAllocations {
            actual: aggregates.len(),
            maximum: MAX_FULL_CAPACITY_ALLOCATIONS,
        });
    }

    let allocations = aggregates
        .into_values()
        .map(|aggregate| {
            FullCapacityAllocation::new(
                aggregate.volume,
                aggregate.allocation_unit,
                aggregate.class,
                aggregate.logical_bytes,
                aggregate.allocated_bytes,
                PROTECTED_FREE_SPACE_FLOOR,
            )
            .map_err(map_validation_error)
        })
        .collect::<Result<Vec<_>, _>>()?;
    FullCapacityPlan::new(allocations).map_err(map_validation_error)
}

fn validate_nature(
    class: FullCapacityAllocationClass,
    nature: CapacityAllocationNature,
) -> Result<(), FullCapacityPlannerError> {
    let retained_class = matches!(
        class,
        FullCapacityAllocationClass::ExistingDestination
            | FullCapacityAllocationClass::ExistingRecoveryPayload
    );
    match (retained_class, nature) {
        (true, CapacityAllocationNature::NewAllocation)
        | (false, CapacityAllocationNature::ExistingRetained)
        | (false, CapacityAllocationNature::SharedExisting) => {
            Err(FullCapacityPlannerError::InvalidNature { class, nature })
        }
        _ => Ok(()),
    }
}

fn map_capacity_error(error: CapacityGateError) -> FullCapacityPlannerError {
    match error {
        CapacityGateError::Overflow => FullCapacityPlannerError::ArithmeticOverflow,
        CapacityGateError::InvalidPlan => FullCapacityPlannerError::InvalidPlan,
        other => FullCapacityPlannerError::DescriptorFacts(other.to_string()),
    }
}

fn map_validation_error(error: FullCapacityValidationError) -> FullCapacityPlannerError {
    match error {
        FullCapacityValidationError::ArithmeticOverflow
        | FullCapacityValidationError::AggregateArithmeticOverflow { .. } => {
            FullCapacityPlannerError::ArithmeticOverflow
        }
        FullCapacityValidationError::TooManyAllocations { actual, maximum } => {
            FullCapacityPlannerError::TooManyAllocations { actual, maximum }
        }
        _ => FullCapacityPlannerError::InvalidPlan,
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum FullCapacityPlannerError {
    #[error("full capacity planner received no requirements")]
    EmptyRequirements,
    #[error("full capacity planner received {actual} requirements, maximum is {maximum}")]
    TooManyRequirements { actual: usize, maximum: usize },
    #[error("capacity estimate is unknown")]
    UnknownEstimate,
    #[error("capacity estimate is zero")]
    ZeroEstimate,
    #[error("descriptor-derived capacity facts are unavailable: {0}")]
    DescriptorFacts(String),
    #[error("allocation unit is zero for volume {volume:?}")]
    ZeroAllocationUnit { volume: VolumeIdentity },
    #[error("requirements for volume {volume:?} use units {expected} and {actual}")]
    ConflictingAllocationUnit {
        volume: VolumeIdentity,
        expected: u64,
        actual: u64,
    },
    #[error("allocation nature {nature:?} is invalid for class {class:?}")]
    InvalidNature {
        class: FullCapacityAllocationClass,
        nature: CapacityAllocationNature,
    },
    #[error("shared existing object identity is unavailable")]
    SharedIdentityUnavailable,
    #[error("shared existing object identity has conflicting requirements")]
    ConflictingSharedIdentity,
    #[error("full capacity planner arithmetic overflowed")]
    ArithmeticOverflow,
    #[error("full capacity planner output is invalid")]
    InvalidPlan,
    #[error("full capacity planner produced {actual} allocations, maximum is {maximum}")]
    TooManyAllocations { actual: usize, maximum: usize },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs::OpenOptions;

    fn facts(device: u64, allocation_unit: u64) -> VolumeFacts {
        VolumeFacts {
            identity: VolumeIdentity { device },
            free_bytes: PROTECTED_FREE_SPACE_FLOOR + 1024 * 1024,
            allocation_unit,
        }
    }

    fn file(temp: &tempfile::TempDir, name: &str) -> File {
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(temp.path().join(name))
            .expect("open descriptor fixture")
    }

    fn requirement<'a>(
        descriptor: &'a File,
        device: u64,
        allocation_unit: u64,
        class: FullCapacityAllocationClass,
        estimate: CapacityEstimate,
        nature: CapacityAllocationNature,
    ) -> CapacityRequirement<'a> {
        CapacityRequirement::with_test_facts(
            descriptor,
            facts(device, allocation_unit),
            class,
            estimate,
            nature,
        )
    }

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("create planner fixtures")
    }

    #[test]
    fn rounds_each_new_object_before_aggregation() {
        let temp = temp();
        let descriptor = file(&temp, "destination");
        let plan = derive_full_capacity_plan(&[
            requirement(
                &descriptor,
                1,
                4096,
                FullCapacityAllocationClass::DestinationStaging,
                CapacityEstimate::Known(1),
                CapacityAllocationNature::NewAllocation,
            ),
            requirement(
                &descriptor,
                1,
                4096,
                FullCapacityAllocationClass::DestinationStaging,
                CapacityEstimate::Known(1),
                CapacityAllocationNature::NewAllocation,
            ),
        ])
        .expect("rounded plan");
        let allocation = &plan.allocations()[0];
        assert_eq!(allocation.logical_bytes(), 2);
        assert_eq!(allocation.allocated_bytes(), 8192);
    }

    #[test]
    fn shared_existing_duplicate_open_handles_deduplicate_once() {
        let temp = temp();
        let descriptor = file(&temp, "existing");
        let duplicate = descriptor.try_clone().expect("clone descriptor");
        let plan = derive_full_capacity_plan(&[
            requirement(
                &descriptor,
                1,
                4096,
                FullCapacityAllocationClass::ExistingDestination,
                CapacityEstimate::Known(4097),
                CapacityAllocationNature::SharedExisting,
            ),
            requirement(
                &duplicate,
                1,
                4096,
                FullCapacityAllocationClass::ExistingDestination,
                CapacityEstimate::Known(4097),
                CapacityAllocationNature::SharedExisting,
            ),
        ])
        .expect("deduplicated plan");
        assert_eq!(plan.allocations()[0].logical_bytes(), 4097);
        assert_eq!(plan.allocations()[0].allocated_bytes(), 8192);
    }

    #[test]
    fn distinct_same_sized_existing_objects_are_not_deduplicated() {
        let temp = temp();
        let first = file(&temp, "first");
        let second = file(&temp, "second");
        let plan = derive_full_capacity_plan(&[
            requirement(
                &first,
                1,
                4096,
                FullCapacityAllocationClass::ExistingDestination,
                CapacityEstimate::Known(4097),
                CapacityAllocationNature::SharedExisting,
            ),
            requirement(
                &second,
                1,
                4096,
                FullCapacityAllocationClass::ExistingDestination,
                CapacityEstimate::Known(4097),
                CapacityAllocationNature::SharedExisting,
            ),
        ])
        .expect("separate existing objects");
        assert_eq!(plan.allocations()[0].logical_bytes(), 8194);
        assert_eq!(plan.allocations()[0].allocated_bytes(), 16384);
    }

    #[cfg(unix)]
    #[test]
    fn same_directory_descriptor_new_allocations_remain_additive() {
        let temp = temp();
        let descriptor = File::open(temp.path()).expect("open directory descriptor");
        let plan = derive_full_capacity_plan(&[
            CapacityRequirement::new(
                &descriptor,
                FullCapacityAllocationClass::DestinationStaging,
                CapacityEstimate::Known(1),
                CapacityAllocationNature::NewAllocation,
            ),
            CapacityRequirement::new(
                &descriptor,
                FullCapacityAllocationClass::DestinationStaging,
                CapacityEstimate::Known(1),
                CapacityAllocationNature::NewAllocation,
            ),
        ])
        .expect("directory-derived plan");
        assert_eq!(plan.allocations()[0].logical_bytes(), 2);
        assert_eq!(
            plan.allocations()[0].allocated_bytes(),
            2 * plan.allocations()[0].allocation_unit()
        );
    }

    #[test]
    fn all_classes_and_multiple_volumes_are_retained() {
        let temp = temp();
        let first = file(&temp, "first");
        let second = file(&temp, "second");
        let classes = [
            (
                FullCapacityAllocationClass::DestinationStaging,
                CapacityAllocationNature::NewAllocation,
                1,
            ),
            (
                FullCapacityAllocationClass::JournalRecord,
                CapacityAllocationNature::NewAllocation,
                1,
            ),
            (
                FullCapacityAllocationClass::SourceDatabaseCommit,
                CapacityAllocationNature::NewAllocation,
                1,
            ),
            (
                FullCapacityAllocationClass::SourceWalShm,
                CapacityAllocationNature::NewAllocation,
                1,
            ),
            (
                FullCapacityAllocationClass::GlobalDatabaseCommit,
                CapacityAllocationNature::NewAllocation,
                2,
            ),
            (
                FullCapacityAllocationClass::GlobalWalShm,
                CapacityAllocationNature::NewAllocation,
                2,
            ),
            (
                FullCapacityAllocationClass::ExistingDestination,
                CapacityAllocationNature::ExistingRetained,
                2,
            ),
            (
                FullCapacityAllocationClass::ExistingRecoveryPayload,
                CapacityAllocationNature::ExistingRetained,
                2,
            ),
            (
                FullCapacityAllocationClass::OperationRecoveryPayload,
                CapacityAllocationNature::NewAllocation,
                2,
            ),
        ];
        let requirements = classes
            .into_iter()
            .enumerate()
            .map(|(index, (class, nature, device))| {
                requirement(
                    if device == 1 { &first } else { &second },
                    device,
                    4096,
                    class,
                    CapacityEstimate::Known(index as u64 + 1),
                    nature,
                )
            })
            .collect::<Vec<_>>();
        let plan = derive_full_capacity_plan(&requirements).expect("all class plan");
        assert_eq!(plan.allocations().len(), classes.len());
        let classes = plan
            .allocations()
            .iter()
            .map(|allocation| allocation.class())
            .collect::<BTreeSet<_>>();
        assert_eq!(classes.len(), 9);
    }

    #[test]
    fn shared_identity_conflicts_fail_closed() {
        let temp = temp();
        let descriptor = file(&temp, "existing");
        let conflicting_estimate = derive_full_capacity_plan(&[
            requirement(
                &descriptor,
                1,
                4096,
                FullCapacityAllocationClass::ExistingDestination,
                CapacityEstimate::Known(1),
                CapacityAllocationNature::SharedExisting,
            ),
            requirement(
                &descriptor,
                1,
                4096,
                FullCapacityAllocationClass::ExistingDestination,
                CapacityEstimate::Known(2),
                CapacityAllocationNature::SharedExisting,
            ),
        ]);
        assert_eq!(
            conflicting_estimate,
            Err(FullCapacityPlannerError::ConflictingSharedIdentity)
        );

        let conflicting_class = derive_full_capacity_plan(&[
            requirement(
                &descriptor,
                1,
                4096,
                FullCapacityAllocationClass::ExistingDestination,
                CapacityEstimate::Known(1),
                CapacityAllocationNature::SharedExisting,
            ),
            requirement(
                &descriptor,
                1,
                4096,
                FullCapacityAllocationClass::ExistingRecoveryPayload,
                CapacityEstimate::Known(1),
                CapacityAllocationNature::SharedExisting,
            ),
        ]);
        assert_eq!(
            conflicting_class,
            Err(FullCapacityPlannerError::ConflictingSharedIdentity)
        );
    }

    #[test]
    fn unknown_zero_overflow_and_invalid_nature_reject() {
        let temp = temp();
        let descriptor = file(&temp, "destination");
        let unknown = derive_full_capacity_plan(&[requirement(
            &descriptor,
            1,
            4096,
            FullCapacityAllocationClass::DestinationStaging,
            CapacityEstimate::Unknown,
            CapacityAllocationNature::NewAllocation,
        )]);
        assert_eq!(unknown, Err(FullCapacityPlannerError::UnknownEstimate));
        let zero = derive_full_capacity_plan(&[requirement(
            &descriptor,
            1,
            4096,
            FullCapacityAllocationClass::DestinationStaging,
            CapacityEstimate::Known(0),
            CapacityAllocationNature::NewAllocation,
        )]);
        assert_eq!(zero, Err(FullCapacityPlannerError::ZeroEstimate));
        let overflow = derive_full_capacity_plan(&[requirement(
            &descriptor,
            1,
            4096,
            FullCapacityAllocationClass::DestinationStaging,
            CapacityEstimate::Known(u64::MAX),
            CapacityAllocationNature::NewAllocation,
        )]);
        assert_eq!(overflow, Err(FullCapacityPlannerError::ArithmeticOverflow));
        let invalid_nature = derive_full_capacity_plan(&[requirement(
            &descriptor,
            1,
            4096,
            FullCapacityAllocationClass::DestinationStaging,
            CapacityEstimate::Known(1),
            CapacityAllocationNature::ExistingRetained,
        )]);
        assert!(matches!(
            invalid_nature,
            Err(FullCapacityPlannerError::InvalidNature { .. })
        ));
    }

    #[test]
    fn unit_mismatch_and_input_limit_reject_but_large_aggregate_fits() {
        let temp = temp();
        let descriptor = file(&temp, "destination");
        let mismatch = derive_full_capacity_plan(&[
            requirement(
                &descriptor,
                1,
                4096,
                FullCapacityAllocationClass::DestinationStaging,
                CapacityEstimate::Known(1),
                CapacityAllocationNature::NewAllocation,
            ),
            requirement(
                &descriptor,
                1,
                8192,
                FullCapacityAllocationClass::JournalRecord,
                CapacityEstimate::Known(1),
                CapacityAllocationNature::NewAllocation,
            ),
        ]);
        assert!(matches!(
            mismatch,
            Err(FullCapacityPlannerError::ConflictingAllocationUnit { .. })
        ));

        let many = (0..MAX_WORK_PLAN_UNITS)
            .map(|_| {
                requirement(
                    &descriptor,
                    1,
                    4096,
                    FullCapacityAllocationClass::DestinationStaging,
                    CapacityEstimate::Known(1),
                    CapacityAllocationNature::NewAllocation,
                )
            })
            .collect::<Vec<_>>();
        let plan = derive_full_capacity_plan(&many).expect("bounded aggregation");
        assert_eq!(plan.allocations().len(), 1);
        assert_eq!(
            plan.allocations()[0].logical_bytes(),
            MAX_WORK_PLAN_UNITS as u64
        );

        let too_many = (0..=MAX_WORK_PLAN_UNITS)
            .map(|_| {
                requirement(
                    &descriptor,
                    1,
                    4096,
                    FullCapacityAllocationClass::DestinationStaging,
                    CapacityEstimate::Known(1),
                    CapacityAllocationNature::NewAllocation,
                )
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            derive_full_capacity_plan(&too_many),
            Err(FullCapacityPlannerError::TooManyRequirements { .. })
        ));
    }

    #[test]
    fn output_limit_and_aggregate_overflow_reject() {
        let temp = temp();
        let descriptor = file(&temp, "destination");
        let too_many = (0..=MAX_FULL_CAPACITY_ALLOCATIONS)
            .map(|device| {
                requirement(
                    &descriptor,
                    device as u64 + 1,
                    1,
                    FullCapacityAllocationClass::DestinationStaging,
                    CapacityEstimate::Known(1),
                    CapacityAllocationNature::NewAllocation,
                )
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            derive_full_capacity_plan(&too_many),
            Err(FullCapacityPlannerError::TooManyAllocations { .. })
        ));

        let first = requirement(
            &descriptor,
            1,
            1,
            FullCapacityAllocationClass::DestinationStaging,
            CapacityEstimate::Known(u64::MAX - PROTECTED_FREE_SPACE_FLOOR),
            CapacityAllocationNature::NewAllocation,
        );
        let second = requirement(
            &descriptor,
            1,
            1,
            FullCapacityAllocationClass::DestinationStaging,
            CapacityEstimate::Known(1),
            CapacityAllocationNature::NewAllocation,
        );
        assert_eq!(
            derive_full_capacity_plan(&[first, second]),
            Err(FullCapacityPlannerError::ArithmeticOverflow)
        );
    }

    #[test]
    fn production_requirement_uses_descriptor_facts() {
        let temp = temp();
        let descriptor = file(&temp, "destination");
        let plan = derive_full_capacity_plan(&[CapacityRequirement::new(
            &descriptor,
            FullCapacityAllocationClass::DestinationStaging,
            CapacityEstimate::Known(1),
            CapacityAllocationNature::NewAllocation,
        )])
        .expect("descriptor facts");
        assert!(plan.allocations()[0].volume().device > 0);
        assert!(plan.allocations()[0].allocation_unit() > 0);
    }
}
