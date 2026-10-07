//! Renderer-neutral pre-intent capacity accounting extracted from the native journal.
//!
//! These plans are observations and logical claims, not disk reservations or permission
//! to publish files. The serialized owner must supply all outstanding claims, collect
//! fresh facts from its retained destination descriptors, and durably record admission.
//! No filesystem mutation, journal admission, or recovery decision is performed here.
//! This initial contract charges destination staging only; other allocation classes are
//! retained for journal compatibility and are not automatically budgeted by the planner.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::PathBuf;

/// Minimum free space retained after logical claims are admitted.
pub const PROTECTED_FREE_SPACE_FLOOR: u64 = 256 * 1024 * 1024;

/// Stable identity obtained from the destination file descriptor.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct VolumeIdentity {
    /// Unix `st_dev` or Windows handle-derived volume serial; not a qualified native UUID.
    pub device: u64,
}

/// Physical allocation accounting class used by a capacity claim.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CapacityAllocationClass {
    /// Destination-side staging bytes (the only charged class in this bounded slice).
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
}

/// One volume's durable capacity claim.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DurableVolumeCapacity {
    /// Descriptor-derived volume identity.
    pub identity: VolumeIdentity,
    /// Physical allocation unit in bytes.
    pub allocation_unit: u64,
    /// Allocation category retained in the durable plan.
    pub allocation_class: CapacityAllocationClass,
    /// Requested payload bytes before per-object allocation rounding.
    pub logical_bytes: u64,
    /// Total allocation-rounded bytes charged to this volume.
    pub allocated_bytes: u64,
    /// Free-space floor retained after outstanding claims.
    #[serde(default)]
    pub protected_free_bytes: u64,
}

/// Additive journal payload.  The field on `OperationRecord` is optional for legacy records.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DurableCapacityPlan {
    /// Deterministically ordered claims, one per destination volume.
    #[serde(default)]
    pub volumes: Vec<DurableVolumeCapacity>,
}

/// Fail-closed reasons returned before an intent can be written.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RejectedBeforeIntent {
    /// History action shape is outside the bounded capacity gate.
    #[error("history action shape is outside the bounded capacity gate")]
    InvalidShape,
    /// History restore backup is missing: {0}.
    #[error("history restore backup is missing: {0}")]
    MissingBackup(PathBuf),
    /// History restore target is missing: {0}.
    #[error("history restore target is missing: {0}")]
    MissingTarget(PathBuf),
    /// History restore path is not a regular file: {0}.
    #[error("history restore path is not a regular file: {0}")]
    NotRegularFile(PathBuf),
    /// Capacity facts unavailable: {0}.
    #[error("capacity facts unavailable: {0}")]
    Discovery(String),
    /// Capacity arithmetic overflow.
    #[error("capacity arithmetic overflow")]
    Overflow,
    /// Capacity plan is invalid.
    #[error("capacity plan is invalid")]
    InvalidPlan,
    /// Capacity admission is blocked by unresolved journal recovery.
    #[error("capacity admission is blocked by unresolved journal recovery")]
    RecoveryBlocked,
    /// Insufficient free space on volume {0:?}: need {1} bytes, have {2} bytes.
    #[error("insufficient free space on volume {0:?}: need {1} bytes, have {2} bytes")]
    InsufficientSpace(VolumeIdentity, u64, u64),
}

/// Error returned by capacity discovery and accounting.
pub type CapacityGateError = RejectedBeforeIntent;

/// Facts obtained while holding an open destination descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolumeFacts {
    /// Descriptor-derived volume identity.
    pub identity: VolumeIdentity,
    /// Bytes currently available to the caller on this volume.
    pub free_bytes: u64,
    /// Physical allocation unit in bytes.
    pub allocation_unit: u64,
}

/// One logical write requirement before aggregation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapacityRequirement {
    /// Live descriptor-derived capacity observation.
    pub facts: VolumeFacts,
    /// Requested payload bytes before per-object allocation rounding.
    pub logical_bytes: u64,
}

/// Round a logical byte count to physical allocation units, rejecting zero units and overflow.
pub fn round_up_allocation(bytes: u64, allocation_unit: u64) -> Result<u64, CapacityGateError> {
    if allocation_unit == 0 {
        return Err(CapacityGateError::InvalidPlan);
    }
    if bytes == 0 {
        return Ok(0);
    }
    let remainder = bytes % allocation_unit;
    if remainder == 0 {
        return Ok(bytes);
    }
    bytes
        .checked_add(allocation_unit - remainder)
        .ok_or(CapacityGateError::Overflow)
}

/// Aggregate logical requirements by descriptor-derived volume identity and enforce free space.
pub fn aggregate_capacity_plan(
    requirements: &[CapacityRequirement],
    existing_claims: &BTreeMap<VolumeIdentity, u64>,
) -> Result<DurableCapacityPlan, CapacityGateError> {
    let mut grouped: BTreeMap<VolumeIdentity, DurableVolumeCapacity> = BTreeMap::new();
    for requirement in requirements {
        let allocated_bytes =
            round_up_allocation(requirement.logical_bytes, requirement.facts.allocation_unit)?;
        let entry = grouped
            .entry(requirement.facts.identity.clone())
            .or_insert_with(|| DurableVolumeCapacity {
                identity: requirement.facts.identity.clone(),
                allocation_unit: requirement.facts.allocation_unit,
                allocation_class: CapacityAllocationClass::DestinationStaging,
                logical_bytes: 0,
                allocated_bytes: 0,
                protected_free_bytes: PROTECTED_FREE_SPACE_FLOOR,
            });
        if entry.allocation_unit != requirement.facts.allocation_unit {
            return Err(CapacityGateError::InvalidPlan);
        }
        entry.logical_bytes = entry
            .logical_bytes
            .checked_add(requirement.logical_bytes)
            .ok_or(CapacityGateError::Overflow)?;
        entry.allocated_bytes = entry
            .allocated_bytes
            .checked_add(allocated_bytes)
            .ok_or(CapacityGateError::Overflow)?;
    }
    for claim in grouped.values() {
        let already_claimed = existing_claims.get(&claim.identity).copied().unwrap_or(0);
        let reserved = already_claimed
            .checked_add(claim.allocated_bytes)
            .ok_or(CapacityGateError::Overflow)?;
        let free_bytes = requirements
            .iter()
            .filter(|requirement| requirement.facts.identity == claim.identity)
            .map(|requirement| requirement.facts.free_bytes)
            .min()
            .ok_or(CapacityGateError::InvalidPlan)?;
        if reserved
            .checked_add(PROTECTED_FREE_SPACE_FLOOR)
            .ok_or(CapacityGateError::Overflow)?
            > free_bytes
        {
            return Err(CapacityGateError::InsufficientSpace(
                claim.identity.clone(),
                claim.allocated_bytes,
                free_bytes
                    .saturating_sub(already_claimed)
                    .saturating_sub(PROTECTED_FREE_SPACE_FLOOR),
            ));
        }
    }
    Ok(DurableCapacityPlan {
        volumes: grouped.into_values().collect(),
    })
}

#[cfg(unix)]
fn descriptor_volume_facts(file: &File) -> Result<VolumeFacts, io::Error> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    let mut fs_stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    if unsafe { libc::fstatvfs(fd, fs_stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let fs_stat = unsafe { fs_stat.assume_init() };
    let (free, unit) = statvfs_capacity_values(
        u64::try_from(fs_stat.f_bavail)
            .map_err(|_| io::Error::other("invalid free-space block count"))?,
        u64::try_from(fs_stat.f_frsize)
            .map_err(|_| io::Error::other("invalid filesystem fragment size"))?,
        u64::try_from(fs_stat.f_bsize)
            .map_err(|_| io::Error::other("invalid filesystem block size"))?,
    )?;
    Ok(VolumeFacts {
        identity: VolumeIdentity {
            device: stat.st_dev as u64,
        },
        free_bytes: free,
        allocation_unit: unit,
    })
}

/// Derive live capacity facts from an already-open, capability-backed descriptor.
pub fn descriptor_capacity_facts(file: &File) -> Result<VolumeFacts, CapacityGateError> {
    descriptor_volume_facts(file).map_err(|error| CapacityGateError::Discovery(error.to_string()))
}

// Readonly original descriptor facts with selected native text funding. Callback
// acceptance is accounting only and cannot issue admission or a physical witness.
pub(crate) enum FundedDescriptorError<E> {
    Observed(CapacityGateError, std::marker::PhantomData<E>),
    #[cfg(windows)]
    Funding(E),
}

pub(crate) fn descriptor_capacity_facts_with_funding<E>(
    file: &File,
    reserve: &mut dyn FnMut(usize) -> Result<(), E>,
) -> Result<VolumeFacts, FundedDescriptorError<E>> {
    #[cfg(windows)]
    {
        windows::observe(file, Some(reserve)).map_err(|error| match error {
            windows::ObservationError::Observed(error) => FundedDescriptorError::Observed(
                CapacityGateError::Discovery(error.to_string()),
                std::marker::PhantomData,
            ),
            windows::ObservationError::Funding(error) => FundedDescriptorError::Funding(error),
        })
    }
    #[cfg(not(windows))]
    {
        let _ = reserve;
        descriptor_capacity_facts(file)
            .map_err(|error| FundedDescriptorError::Observed(error, std::marker::PhantomData))
    }
}

#[cfg(unix)]
fn statvfs_capacity_values(
    available_blocks: u64,
    fragment_size: u64,
    block_size: u64,
) -> Result<(u64, u64), io::Error> {
    if fragment_size == 0 || block_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "filesystem allocation unit is zero",
        ));
    }
    let free_bytes = available_blocks
        .checked_mul(fragment_size)
        .ok_or_else(|| io::Error::other("free-space arithmetic overflow"))?;
    Ok((free_bytes, fragment_size))
}

#[cfg(any(windows, test))]
mod windows;

#[cfg(windows)]
fn descriptor_volume_facts(file: &File) -> Result<VolumeFacts, io::Error> {
    windows::descriptor_volume_facts(file)
}

#[cfg(not(any(unix, windows)))]
fn descriptor_volume_facts(_file: &File) -> Result<VolumeFacts, io::Error> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "descriptor-derived volume facts are not implemented on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(device: u64, free_bytes: u64, allocation_unit: u64) -> VolumeFacts {
        VolumeFacts {
            identity: VolumeIdentity { device },
            free_bytes,
            allocation_unit,
        }
    }

    #[test]
    fn rounds_floor_and_exact_boundary() {
        assert_eq!(round_up_allocation(0, 4096).unwrap(), 0);
        assert_eq!(round_up_allocation(4096, 4096).unwrap(), 4096);
        assert_eq!(round_up_allocation(4097, 4096).unwrap(), 8192);
    }

    #[test]
    fn aggregates_same_and_two_volumes() {
        let requirements = vec![
            CapacityRequirement {
                facts: facts(1, PROTECTED_FREE_SPACE_FLOOR + 16 * 1024, 4096),
                logical_bytes: 4097,
            },
            CapacityRequirement {
                facts: facts(1, PROTECTED_FREE_SPACE_FLOOR + 16 * 1024, 4096),
                logical_bytes: 4096,
            },
            CapacityRequirement {
                facts: facts(2, PROTECTED_FREE_SPACE_FLOOR + 8 * 1024, 4096),
                logical_bytes: 4097,
            },
        ];
        let plan = aggregate_capacity_plan(&requirements, &BTreeMap::new()).unwrap();
        assert_eq!(plan.volumes.len(), 2);
        assert_eq!(plan.volumes[0].logical_bytes, 8193);
        assert_eq!(plan.volumes[0].allocated_bytes, 12288);
        assert_eq!(plan.volumes[1].allocated_bytes, 8192);
    }

    #[test]
    fn claims_consume_free_space_and_overflow_fails_closed() {
        let mut claims = BTreeMap::new();
        claims.insert(VolumeIdentity { device: 1 }, 4096);
        assert!(matches!(
            aggregate_capacity_plan(
                &[CapacityRequirement {
                    facts: facts(1, PROTECTED_FREE_SPACE_FLOOR + 8192, 4096),
                    logical_bytes: 4097,
                }],
                &claims,
            ),
            Err(CapacityGateError::InsufficientSpace(..))
        ));
        assert!(matches!(
            round_up_allocation(u64::MAX, 4096),
            Err(CapacityGateError::Overflow)
        ));
        assert!(
            aggregate_capacity_plan(
                &[CapacityRequirement {
                    facts: facts(3, PROTECTED_FREE_SPACE_FLOOR + 4096, 4096),
                    logical_bytes: 4096,
                }],
                &BTreeMap::new(),
            )
            .is_ok()
        );
    }

    #[cfg(unix)]
    #[test]
    fn statvfs_free_bytes_use_fragment_size_not_block_size() {
        assert_eq!(
            statvfs_capacity_values(2, 4096, 16384).unwrap(),
            (8192, 4096)
        );
        assert!(statvfs_capacity_values(2, 0, 16384).is_err());
        assert!(statvfs_capacity_values(2, 4096, 0).is_err());
    }

    #[test]
    fn conflicting_units_and_lowest_free_observation_fail_closed() {
        let requirement = CapacityRequirement {
            facts: facts(1, PROTECTED_FREE_SPACE_FLOOR + 8192, 4096),
            logical_bytes: 1,
        };
        let mut second = requirement.clone();
        second.facts.allocation_unit = 8192;
        assert_eq!(
            aggregate_capacity_plan(&[requirement.clone(), second.clone()], &BTreeMap::new()),
            Err(CapacityGateError::InvalidPlan)
        );
        second.facts.allocation_unit = 4096;
        second.facts.free_bytes = PROTECTED_FREE_SPACE_FLOOR + 4096;
        assert!(matches!(
            aggregate_capacity_plan(&[requirement, second], &BTreeMap::new()),
            Err(CapacityGateError::InsufficientSpace(
                VolumeIdentity { device: 1 },
                8192,
                4096
            ))
        ));
        assert_eq!(
            round_up_allocation(0, 0),
            Err(CapacityGateError::InvalidPlan)
        );
    }

    #[test]
    fn aggregation_and_outstanding_claim_overflow_fail_closed() {
        let requirement = CapacityRequirement {
            facts: facts(1, u64::MAX, 1),
            logical_bytes: u64::MAX,
        };
        assert_eq!(
            aggregate_capacity_plan(&[requirement.clone(), requirement], &BTreeMap::new()),
            Err(CapacityGateError::Overflow)
        );
        let small = CapacityRequirement {
            facts: facts(1, u64::MAX, 1),
            logical_bytes: 1,
        };
        assert_eq!(
            aggregate_capacity_plan(
                &[small],
                &BTreeMap::from([(VolumeIdentity { device: 1 }, u64::MAX)])
            ),
            Err(CapacityGateError::Overflow)
        );
    }

    #[test]
    fn preserved_journal_payload_and_legacy_defaults_round_trip() {
        let payload = serde_json::json!({"volumes": [{
            "identity": {"device": 7}, "allocation_unit": 4096,
            "allocation_class": "DestinationStaging", "logical_bytes": 1,
            "allocated_bytes": 4096, "protected_free_bytes": 268435456
        }]});
        let plan: DurableCapacityPlan = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(serde_json::to_value(plan).unwrap(), payload);
        let mut legacy = payload;
        legacy["volumes"][0]
            .as_object_mut()
            .unwrap()
            .remove("protected_free_bytes");
        let decoded: DurableCapacityPlan = serde_json::from_value(legacy).unwrap();
        // Deserialization preserves the old payload; it does not authorize admission.
        assert_eq!(decoded.volumes[0].protected_free_bytes, 0);
        assert_eq!(
            serde_json::from_str::<DurableCapacityPlan>("{}").unwrap(),
            DurableCapacityPlan::default()
        );
        let invalid = r#"{"volumes":[{"identity":{"device":7},"allocation_unit":4096,"allocation_class":"Unknown","logical_bytes":1,"allocated_bytes":4096}]}"#;
        assert!(serde_json::from_str::<DurableCapacityPlan>(invalid).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn capacity_discovery_uses_the_retained_descriptor() {
        use std::os::unix::fs::MetadataExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("destination");
        let file = File::create(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        // The removed name cannot be reopened, but the owner's handle still identifies its volume.
        let facts = descriptor_capacity_facts(&file).unwrap();
        assert_eq!(facts.identity.device, file.metadata().unwrap().dev());
        assert!(facts.allocation_unit > 0);
    }

    #[cfg(not(any(unix, windows)))]
    #[test]
    fn unqualified_descriptor_discovery_is_unsupported() {
        let file = tempfile::tempfile().unwrap();
        assert!(matches!(
            descriptor_capacity_facts(&file),
            Err(CapacityGateError::Discovery(_))
        ));
    }
}
