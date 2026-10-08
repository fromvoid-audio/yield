//! Exact eight-object initial accounting, without capacity or mutable-source authority.
use super::*;
use crate::durable_capacity::{PROTECTED_FREE_SPACE_FLOOR, VolumeIdentity};
use crate::operation_journal::full_capacity::{
    FullCapacityAllocation, FullCapacityAllocationClass as Class, FullCapacityPlan,
};

impl LegacyAdoptionIntentArgs {
    pub(crate) fn initial_capacity_plan(
        &self,
    ) -> Result<FullCapacityPlan, LegacyAdoptionInputError> {
        self.validate()?;
        let mut totals = BTreeMap::<(u64, Class), (u64, u64, u64)>::new();
        // Fixed original order is the wire role encoding. MAIN's entire allowance
        // includes retained MAIN once; prospective sidecars remain separate objects.
        for (object, class) in self.data.objects.iter().zip([
            Class::SourceDatabaseCommit,
            Class::SourceDatabaseCommit,
            Class::SourceWalShm,
            Class::SourceWalShm,
            Class::SourceDatabaseCommit,
            Class::JournalRecord,
            Class::JournalRecord,
            Class::JournalRecord,
        ]) {
            let invalid = LegacyAdoptionInputError::Invalid;
            let logical = object.conditional_logical_bytes;
            if logical == 0 || object.unit == 0 {
                return Err(invalid);
            }
            let allocated = logical
                .checked_add(object.unit - 1)
                .and_then(|n| n.checked_div(object.unit))
                .and_then(|n| n.checked_mul(object.unit))
                .ok_or(invalid)?;
            let (unit, total_logical, total_allocated) = totals
                .entry((object.device, class))
                .or_insert((object.unit, 0, 0));
            if *unit != object.unit {
                return Err(invalid);
            }
            *total_logical = total_logical.checked_add(logical).ok_or(invalid)?;
            *total_allocated = total_allocated.checked_add(allocated).ok_or(invalid)?;
        }
        let allocations = totals
            .into_iter()
            .map(|((device, class), (unit, logical, allocated))| {
                FullCapacityAllocation::new(
                    VolumeIdentity { device },
                    unit,
                    class,
                    logical,
                    allocated,
                    PROTECTED_FREE_SPACE_FLOOR,
                )
                .map_err(|_| LegacyAdoptionInputError::Invalid)
            })
            .collect::<Result<Vec<_>, _>>()?;
        FullCapacityPlan::new(allocations).map_err(|_| LegacyAdoptionInputError::Invalid)
    }
}
