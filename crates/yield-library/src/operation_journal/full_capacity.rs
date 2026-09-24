//! V16-only capacity observations for full-progress admission.
//!
//! The full-capacity types keep every allocation class explicit, including operation-owned
//! recovery payloads.  They are bounded observations and do not reserve capacity, prove a live
//! allocation, or authorize cleanup or cancellation.  The legacy
//! [`crate::durable_capacity::DurableCapacityPlan`] remains unchanged for schemas V1 through
//! V15; conversion into this model copies its facts and fails validation rather than filling in
//! missing recovery or protected-floor information.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

use crate::durable_capacity::{
    CapacityAllocationClass, DurableCapacityPlan, PROTECTED_FREE_SPACE_FLOOR, VolumeIdentity,
};

/// Version of the bounded full-capacity plan shape.
pub const FULL_CAPACITY_PLAN_VERSION: u16 = 1;

/// Maximum number of distinct `(volume, class)` allocations in a full-capacity plan.
pub const MAX_FULL_CAPACITY_ALLOCATIONS: usize = 256;

/// Allocation classes understood by the full-progress contract.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum FullCapacityAllocationClass {
    /// Destination-side staging bytes.
    DestinationStaging,
    /// Bytes for a future operation-journal record.
    JournalRecord,
    /// Bytes for a future source-database commit.
    SourceDatabaseCommit,
    /// Bytes for a future source-database WAL/SHM pair.
    SourceWalShm,
    /// Bytes for a future global-database commit.
    GlobalDatabaseCommit,
    /// Bytes for a future global-database WAL/SHM pair.
    GlobalWalShm,
    /// Existing destination allocation retained by the operation.
    ExistingDestination,
    /// Existing recovery payload retained by the operation.
    ExistingRecoveryPayload,
    /// Operation-created recovery payload retained by the operation.
    OperationRecoveryPayload,
}

impl FullCapacityAllocationClass {
    /// Return the stable documentation identifier for this allocation class.
    pub const fn stable_id(self) -> &'static str {
        match self {
            Self::DestinationStaging => "capacity.destination_staging",
            Self::JournalRecord => "capacity.journal_record",
            Self::SourceDatabaseCommit => "capacity.source_database_commit",
            Self::SourceWalShm => "capacity.source_wal_shm",
            Self::GlobalDatabaseCommit => "capacity.global_database_commit",
            Self::GlobalWalShm => "capacity.global_wal_shm",
            Self::ExistingDestination => "capacity.existing_destination",
            Self::ExistingRecoveryPayload => "capacity.existing_recovery_payload",
            Self::OperationRecoveryPayload => "capacity.operation_recovery_payload",
        }
    }
}

impl From<CapacityAllocationClass> for FullCapacityAllocationClass {
    fn from(value: CapacityAllocationClass) -> Self {
        match value {
            CapacityAllocationClass::DestinationStaging => Self::DestinationStaging,
            CapacityAllocationClass::JournalRecord => Self::JournalRecord,
            CapacityAllocationClass::SourceDatabaseCommit => Self::SourceDatabaseCommit,
            CapacityAllocationClass::SourceWalShm => Self::SourceWalShm,
            CapacityAllocationClass::GlobalDatabaseCommit => Self::GlobalDatabaseCommit,
            CapacityAllocationClass::GlobalWalShm => Self::GlobalWalShm,
            CapacityAllocationClass::ExistingDestination => Self::ExistingDestination,
            CapacityAllocationClass::ExistingRecoveryPayload => Self::ExistingRecoveryPayload,
        }
    }
}

/// One bounded volume/class allocation in a full-capacity plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FullCapacityAllocation {
    volume: VolumeIdentity,
    allocation_unit: u64,
    class: FullCapacityAllocationClass,
    logical_bytes: u64,
    allocated_bytes: u64,
    protected_free_bytes: u64,
}

impl FullCapacityAllocation {
    /// Construct one allocation after validating its arithmetic and protected-floor facts.
    pub fn new(
        volume: VolumeIdentity,
        allocation_unit: u64,
        class: FullCapacityAllocationClass,
        logical_bytes: u64,
        allocated_bytes: u64,
        protected_free_bytes: u64,
    ) -> Result<Self, FullCapacityValidationError> {
        let allocation = Self {
            volume,
            allocation_unit,
            class,
            logical_bytes,
            allocated_bytes,
            protected_free_bytes,
        };
        allocation.validate()?;
        Ok(allocation)
    }

    /// Return the descriptor-derived volume identity.
    pub const fn volume(&self) -> &VolumeIdentity {
        &self.volume
    }

    /// Return the physical allocation unit in bytes.
    pub const fn allocation_unit(&self) -> u64 {
        self.allocation_unit
    }

    /// Return the explicit allocation class.
    pub const fn class(&self) -> FullCapacityAllocationClass {
        self.class
    }

    /// Return the logical payload bytes before allocation rounding.
    pub const fn logical_bytes(&self) -> u64 {
        self.logical_bytes
    }

    /// Return the physical bytes charged by this allocation.
    pub const fn allocated_bytes(&self) -> u64 {
        self.allocated_bytes
    }

    /// Return the protected free-space floor observed for this volume.
    pub const fn protected_free_bytes(&self) -> u64 {
        self.protected_free_bytes
    }

    /// Validate one decoded allocation without granting capacity authority.
    pub fn validate(&self) -> Result<(), FullCapacityValidationError> {
        if self.allocation_unit == 0 {
            return Err(FullCapacityValidationError::ZeroAllocationUnit {
                volume: self.volume.clone(),
                class: self.class,
            });
        }
        if self.allocated_bytes == 0 {
            return Err(FullCapacityValidationError::ZeroAllocatedBytes {
                volume: self.volume.clone(),
                class: self.class,
            });
        }
        if self.protected_free_bytes < PROTECTED_FREE_SPACE_FLOOR {
            return Err(FullCapacityValidationError::ProtectedFloorTooLow {
                volume: self.volume.clone(),
                class: self.class,
                observed: self.protected_free_bytes,
                required: PROTECTED_FREE_SPACE_FLOOR,
            });
        }
        if self.allocated_bytes < self.logical_bytes {
            return Err(FullCapacityValidationError::AllocatedBelowLogical {
                volume: self.volume.clone(),
                class: self.class,
                logical_bytes: self.logical_bytes,
                allocated_bytes: self.allocated_bytes,
            });
        }
        if self.allocated_bytes % self.allocation_unit != 0 {
            return Err(FullCapacityValidationError::AllocationMisaligned {
                volume: self.volume.clone(),
                class: self.class,
                allocated_bytes: self.allocated_bytes,
                allocation_unit: self.allocation_unit,
            });
        }
        let rounded_input = self
            .logical_bytes
            .checked_add(self.allocation_unit - 1)
            .ok_or(FullCapacityValidationError::ArithmeticOverflow)?;
        let minimum_allocated = (rounded_input / self.allocation_unit)
            .checked_mul(self.allocation_unit)
            .ok_or(FullCapacityValidationError::ArithmeticOverflow)?;
        if self.allocated_bytes < minimum_allocated {
            return Err(FullCapacityValidationError::AllocatedBelowRoundedLogical {
                volume: self.volume.clone(),
                class: self.class,
                minimum_allocated,
                allocated_bytes: self.allocated_bytes,
            });
        }
        self.allocated_bytes
            .checked_add(self.protected_free_bytes)
            .ok_or(FullCapacityValidationError::ArithmeticOverflow)?;
        Ok(())
    }
}

/// Bounded V1 capacity observations for the full-progress model.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FullCapacityPlan {
    version: u16,
    allocations: Vec<FullCapacityAllocation>,
}

impl FullCapacityPlan {
    /// Construct a version-one plan after validating all bounded allocation rows.
    pub fn new(
        allocations: Vec<FullCapacityAllocation>,
    ) -> Result<Self, FullCapacityValidationError> {
        let plan = Self {
            version: FULL_CAPACITY_PLAN_VERSION,
            allocations,
        };
        plan.validate()?;
        Ok(plan)
    }

    /// Convert a legacy plan by copying every row and validating the resulting full shape.
    ///
    /// No recovery allocation class or protected-floor value is inferred during conversion.  A
    /// legacy row that lacks the full contract's required facts is rejected by validation.
    pub fn from_legacy(legacy: &DurableCapacityPlan) -> Result<Self, FullCapacityValidationError> {
        validate_allocation_count(legacy.volumes.len())?;
        let mut allocations = Vec::with_capacity(legacy.volumes.len());
        for row in &legacy.volumes {
            allocations.push(FullCapacityAllocation {
                volume: row.identity.clone(),
                allocation_unit: row.allocation_unit,
                class: row.allocation_class.into(),
                logical_bytes: row.logical_bytes,
                allocated_bytes: row.allocated_bytes,
                protected_free_bytes: row.protected_free_bytes,
            });
        }
        Self::new(allocations)
    }

    /// Return the full-capacity plan version.
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// Borrow the exact allocation rows in captured order.
    pub fn allocations(&self) -> &[FullCapacityAllocation] {
        &self.allocations
    }

    /// Validate version, bounds, uniqueness, and every allocation arithmetic invariant.
    pub fn validate(&self) -> Result<(), FullCapacityValidationError> {
        if self.version != FULL_CAPACITY_PLAN_VERSION {
            return Err(FullCapacityValidationError::VersionMismatch {
                expected: FULL_CAPACITY_PLAN_VERSION,
                actual: self.version,
            });
        }
        validate_allocation_count(self.allocations.len())?;
        for allocation in &self.allocations {
            allocation.validate()?;
        }
        for (index, allocation) in self.allocations.iter().enumerate() {
            if self.allocations[..index]
                .iter()
                .any(|prior| prior.volume == allocation.volume && prior.class == allocation.class)
            {
                return Err(FullCapacityValidationError::DuplicateAllocation {
                    volume: allocation.volume.clone(),
                    class: allocation.class,
                });
            }
        }
        let mut by_volume: BTreeMap<VolumeIdentity, (u64, u64, u64)> = BTreeMap::new();
        for allocation in &self.allocations {
            let entry = by_volume.entry(allocation.volume.clone()).or_insert((
                allocation.allocation_unit,
                0,
                allocation.protected_free_bytes,
            ));
            if entry.0 != allocation.allocation_unit {
                return Err(FullCapacityValidationError::ConflictingAllocationUnit {
                    volume: allocation.volume.clone(),
                    expected: entry.0,
                    actual: allocation.allocation_unit,
                });
            }
            entry.1 = entry
                .1
                .checked_add(allocation.allocated_bytes)
                .ok_or_else(
                    || FullCapacityValidationError::AggregateArithmeticOverflow {
                        volume: allocation.volume.clone(),
                    },
                )?;
            entry.2 = entry.2.max(allocation.protected_free_bytes);
        }
        for (volume, (_, allocated_bytes, protected_free_bytes)) in by_volume {
            allocated_bytes
                .checked_add(protected_free_bytes)
                .ok_or(FullCapacityValidationError::AggregateArithmeticOverflow { volume })?;
        }
        Ok(())
    }
}

impl TryFrom<&DurableCapacityPlan> for FullCapacityPlan {
    type Error = FullCapacityValidationError;

    fn try_from(value: &DurableCapacityPlan) -> Result<Self, Self::Error> {
        Self::from_legacy(value)
    }
}

fn validate_allocation_count(count: usize) -> Result<(), FullCapacityValidationError> {
    if count == 0 {
        return Err(FullCapacityValidationError::EmptyPlan);
    }
    if count > MAX_FULL_CAPACITY_ALLOCATIONS {
        return Err(FullCapacityValidationError::TooManyAllocations {
            actual: count,
            maximum: MAX_FULL_CAPACITY_ALLOCATIONS,
        });
    }
    Ok(())
}

/// Failure while validating full-capacity observations.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum FullCapacityValidationError {
    /// The plan did not contain any allocation rows.
    #[error("full capacity plan must contain at least one allocation")]
    EmptyPlan,
    /// The plan exceeded the bounded allocation-row count.
    #[error("full capacity plan contains {actual} allocations, maximum is {maximum}")]
    TooManyAllocations {
        /// Number of allocation rows supplied.
        actual: usize,
        /// Maximum number of allocation rows accepted.
        maximum: usize,
    },
    /// The plan version was not the supported full V1 shape.
    #[error("full capacity plan version {actual} does not match expected {expected}")]
    VersionMismatch {
        /// Supported full-capacity plan version.
        expected: u16,
        /// Decoded or supplied plan version.
        actual: u16,
    },
    /// An allocation repeated one `(volume, class)` key.
    #[error("full capacity allocation repeats volume {volume:?} and class {class:?}")]
    DuplicateAllocation {
        /// Repeated volume identity.
        volume: VolumeIdentity,
        /// Repeated allocation class.
        class: FullCapacityAllocationClass,
    },
    /// An allocation unit was zero.
    #[error("full capacity allocation has zero unit for volume {volume:?} and class {class:?}")]
    ZeroAllocationUnit {
        /// Volume with the invalid allocation unit.
        volume: VolumeIdentity,
        /// Allocation class with the invalid unit.
        class: FullCapacityAllocationClass,
    },
    /// An allocation reported zero physical bytes.
    #[error("full capacity allocation has zero bytes for volume {volume:?} and class {class:?}")]
    ZeroAllocatedBytes {
        /// Volume with the zero allocation.
        volume: VolumeIdentity,
        /// Allocation class with the zero allocation.
        class: FullCapacityAllocationClass,
    },
    /// The observed protected floor was below the explicit policy floor.
    #[error(
        "full capacity protected floor {observed} is below {required} for volume {volume:?} and class {class:?}"
    )]
    ProtectedFloorTooLow {
        /// Volume whose floor was below policy.
        volume: VolumeIdentity,
        /// Allocation class whose floor was below policy.
        class: FullCapacityAllocationClass,
        /// Observed protected free bytes.
        observed: u64,
        /// Required protected free bytes.
        required: u64,
    },
    /// Physical bytes were smaller than logical bytes.
    #[error(
        "full capacity allocation has {allocated_bytes} bytes below logical {logical_bytes} for volume {volume:?} and class {class:?}"
    )]
    AllocatedBelowLogical {
        /// Volume with insufficient physical bytes.
        volume: VolumeIdentity,
        /// Allocation class with insufficient physical bytes.
        class: FullCapacityAllocationClass,
        /// Logical bytes requested.
        logical_bytes: u64,
        /// Physical bytes observed.
        allocated_bytes: u64,
    },
    /// Physical bytes were not aligned to the allocation unit.
    #[error(
        "full capacity allocation bytes {allocated_bytes} are not aligned to unit {allocation_unit} for volume {volume:?} and class {class:?}"
    )]
    AllocationMisaligned {
        /// Volume with the misaligned allocation.
        volume: VolumeIdentity,
        /// Allocation class with the misaligned allocation.
        class: FullCapacityAllocationClass,
        /// Physical bytes observed.
        allocated_bytes: u64,
        /// Required physical allocation unit.
        allocation_unit: u64,
    },
    /// Physical bytes were below checked allocation rounding of logical bytes.
    #[error(
        "full capacity allocation has {allocated_bytes} bytes below rounded minimum {minimum_allocated} for volume {volume:?} and class {class:?}"
    )]
    AllocatedBelowRoundedLogical {
        /// Volume with insufficient rounded bytes.
        volume: VolumeIdentity,
        /// Allocation class with insufficient rounded bytes.
        class: FullCapacityAllocationClass,
        /// Checked minimum physical bytes.
        minimum_allocated: u64,
        /// Physical bytes observed.
        allocated_bytes: u64,
    },
    /// Checked allocation arithmetic overflowed.
    #[error("full capacity allocation arithmetic overflowed")]
    ArithmeticOverflow,
    /// Rows for one physical volume described conflicting allocation units.
    #[error("full capacity allocations for volume {volume:?} use units {expected} and {actual}")]
    ConflictingAllocationUnit {
        /// Volume whose rows disagree about the physical unit.
        volume: VolumeIdentity,
        /// First allocation unit observed for the volume.
        expected: u64,
        /// Conflicting allocation unit observed later.
        actual: u64,
    },
    /// Checked aggregate allocation plus protected-floor arithmetic overflowed.
    #[error("full capacity aggregate arithmetic overflowed for volume {volume:?}")]
    AggregateArithmeticOverflow {
        /// Volume whose aggregate could not be represented.
        volume: VolumeIdentity,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(device: u64) -> VolumeIdentity {
        VolumeIdentity { device }
    }

    fn allocation(
        device: u64,
        class: FullCapacityAllocationClass,
        logical_bytes: u64,
        allocated_bytes: u64,
    ) -> FullCapacityAllocation {
        FullCapacityAllocation::new(
            volume(device),
            4096,
            class,
            logical_bytes,
            allocated_bytes,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("valid allocation")
    }

    #[test]
    fn full_plan_round_trips_all_classes_and_is_bounded() {
        let plan = FullCapacityPlan::new(vec![allocation(
            1,
            FullCapacityAllocationClass::OperationRecoveryPayload,
            1,
            4096,
        )])
        .expect("full plan");
        let json = serde_json::to_value(&plan).expect("plan JSON");
        assert_eq!(json["version"], FULL_CAPACITY_PLAN_VERSION);
        assert_eq!(json["allocations"][0]["class"], "OperationRecoveryPayload");
        let decoded: FullCapacityPlan = serde_json::from_value(json).expect("round trip");
        assert_eq!(decoded, plan);

        let too_many = (0..=MAX_FULL_CAPACITY_ALLOCATIONS)
            .map(|device| {
                allocation(
                    device as u64,
                    FullCapacityAllocationClass::DestinationStaging,
                    1,
                    4096,
                )
            })
            .collect();
        assert!(matches!(
            FullCapacityPlan::new(too_many),
            Err(FullCapacityValidationError::TooManyAllocations { .. })
        ));
    }

    #[test]
    fn invalid_shape_and_arithmetic_fail_closed() {
        assert_eq!(
            FullCapacityPlan::new(Vec::new()),
            Err(FullCapacityValidationError::EmptyPlan)
        );
        assert!(matches!(
            FullCapacityAllocation::new(
                volume(1),
                0,
                FullCapacityAllocationClass::DestinationStaging,
                1,
                1,
                PROTECTED_FREE_SPACE_FLOOR,
            ),
            Err(FullCapacityValidationError::ZeroAllocationUnit { .. })
        ));
        assert!(matches!(
            FullCapacityAllocation::new(
                volume(1),
                4096,
                FullCapacityAllocationClass::DestinationStaging,
                1,
                0,
                PROTECTED_FREE_SPACE_FLOOR,
            ),
            Err(FullCapacityValidationError::ZeroAllocatedBytes { .. })
        ));
        assert!(matches!(
            FullCapacityAllocation::new(
                volume(1),
                4096,
                FullCapacityAllocationClass::DestinationStaging,
                1,
                4096,
                PROTECTED_FREE_SPACE_FLOOR - 1,
            ),
            Err(FullCapacityValidationError::ProtectedFloorTooLow { .. })
        ));
        assert!(matches!(
            FullCapacityAllocation::new(
                volume(1),
                4096,
                FullCapacityAllocationClass::DestinationStaging,
                1,
                1,
                PROTECTED_FREE_SPACE_FLOOR,
            ),
            Err(FullCapacityValidationError::AllocationMisaligned { .. })
        ));
        assert!(matches!(
            FullCapacityAllocation::new(
                volume(1),
                1,
                FullCapacityAllocationClass::DestinationStaging,
                u64::MAX,
                u64::MAX,
                PROTECTED_FREE_SPACE_FLOOR,
            ),
            Err(FullCapacityValidationError::ArithmeticOverflow)
        ));
    }

    #[test]
    fn duplicate_and_wrong_version_are_rejected_without_legacy_changes() {
        let row = allocation(1, FullCapacityAllocationClass::JournalRecord, 1, 4096);
        assert!(matches!(
            FullCapacityPlan::new(vec![row.clone(), row]),
            Err(FullCapacityValidationError::DuplicateAllocation { .. })
        ));
        let mut value = serde_json::to_value(
            &FullCapacityPlan::new(vec![allocation(
                1,
                FullCapacityAllocationClass::DestinationStaging,
                1,
                4096,
            )])
            .expect("plan"),
        )
        .expect("JSON");
        value["version"] = 2_u16.into();
        let decoded: FullCapacityPlan = serde_json::from_value(value).expect("shape decodes");
        assert!(matches!(
            decoded.validate(),
            Err(FullCapacityValidationError::VersionMismatch {
                expected: FULL_CAPACITY_PLAN_VERSION,
                actual: 2
            })
        ));
    }

    #[test]
    fn same_volume_units_and_aggregate_floor_are_checked() {
        let first = FullCapacityAllocation::new(
            volume(1),
            1,
            FullCapacityAllocationClass::DestinationStaging,
            u64::MAX - PROTECTED_FREE_SPACE_FLOOR,
            u64::MAX - PROTECTED_FREE_SPACE_FLOOR,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("first allocation");
        let second = FullCapacityAllocation::new(
            volume(1),
            1,
            FullCapacityAllocationClass::JournalRecord,
            1,
            1,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("second allocation");
        assert!(matches!(
            FullCapacityPlan::new(vec![first, second]),
            Err(FullCapacityValidationError::AggregateArithmeticOverflow { .. })
        ));

        let first = allocation(2, FullCapacityAllocationClass::DestinationStaging, 1, 4096);
        let second = FullCapacityAllocation::new(
            volume(2),
            8192,
            FullCapacityAllocationClass::JournalRecord,
            1,
            8192,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .expect("second allocation");
        assert!(matches!(
            FullCapacityPlan::new(vec![first, second]),
            Err(FullCapacityValidationError::ConflictingAllocationUnit { .. })
        ));
    }

    #[test]
    fn legacy_conversion_copies_facts_and_does_not_infer_floor_or_recovery() {
        let legacy = DurableCapacityPlan {
            volumes: vec![crate::durable_capacity::DurableVolumeCapacity {
                identity: volume(4),
                allocation_unit: 4096,
                allocation_class: CapacityAllocationClass::DestinationStaging,
                logical_bytes: 1,
                allocated_bytes: 4096,
                protected_free_bytes: PROTECTED_FREE_SPACE_FLOOR,
            }],
        };
        let converted = FullCapacityPlan::from_legacy(&legacy).expect("legacy conversion");
        let row = &converted.allocations()[0];
        assert_eq!(row.volume(), &volume(4));
        assert_eq!(row.allocation_unit(), 4096);
        assert_eq!(row.class(), FullCapacityAllocationClass::DestinationStaging);
        assert_eq!(row.logical_bytes(), 1);
        assert_eq!(row.allocated_bytes(), 4096);
        assert_eq!(row.protected_free_bytes(), PROTECTED_FREE_SPACE_FLOOR);

        let missing_floor = DurableCapacityPlan {
            volumes: vec![crate::durable_capacity::DurableVolumeCapacity {
                protected_free_bytes: 0,
                ..legacy.volumes[0].clone()
            }],
        };
        assert!(matches!(
            FullCapacityPlan::from_legacy(&missing_floor),
            Err(FullCapacityValidationError::ProtectedFloorTooLow { .. })
        ));
        let too_many = DurableCapacityPlan {
            volumes: (0..=MAX_FULL_CAPACITY_ALLOCATIONS)
                .map(|device| crate::durable_capacity::DurableVolumeCapacity {
                    identity: volume(device as u64),
                    allocation_unit: 1,
                    allocation_class: CapacityAllocationClass::DestinationStaging,
                    logical_bytes: 1,
                    allocated_bytes: 1,
                    protected_free_bytes: PROTECTED_FREE_SPACE_FLOOR,
                })
                .collect(),
        };
        assert!(matches!(
            FullCapacityPlan::from_legacy(&too_many),
            Err(FullCapacityValidationError::TooManyAllocations { .. })
        ));
    }

    #[test]
    fn full_plan_preserves_legacy_rows_and_full_only_class() {
        let plan = FullCapacityPlan::new(vec![
            allocation(5, FullCapacityAllocationClass::DestinationStaging, 1, 4096),
            allocation(5, FullCapacityAllocationClass::JournalRecord, 1, 4096),
        ])
        .expect("representable plan");
        assert_eq!(plan.allocations().len(), 2);
        assert_eq!(plan.allocations()[0].logical_bytes(), 1);
        assert_eq!(plan.allocations()[0].allocated_bytes(), 4096);
        assert_eq!(
            plan.allocations()[1].class(),
            FullCapacityAllocationClass::JournalRecord
        );

        let full_only = FullCapacityPlan::new(vec![allocation(
            5,
            FullCapacityAllocationClass::OperationRecoveryPayload,
            1,
            4096,
        )])
        .expect("full-only plan");
        assert_eq!(
            full_only.allocations()[0].class(),
            FullCapacityAllocationClass::OperationRecoveryPayload
        );
    }

    #[test]
    fn new_class_is_not_accepted_by_legacy_serde() {
        let legacy_json = serde_json::json!("OperationRecoveryPayload");
        assert!(serde_json::from_value::<CapacityAllocationClass>(legacy_json).is_err());
        let full_json = serde_json::json!("OperationRecoveryPayload");
        assert_eq!(
            serde_json::from_value::<FullCapacityAllocationClass>(full_json).unwrap(),
            FullCapacityAllocationClass::OperationRecoveryPayload
        );
    }
}
