//! Original admission/resource custody for actual source entry; no namespace qualification.
use super::resource_custody::ResourceUse;
use super::{
    InputPermit, OperationJournalOwnerError, PreparedPublicationTarget, ResourceAdmission,
    SourceEffectPermit, SourceEffectPin,
};
use std::sync::Arc;

/// Fields drop in declaration order: release the source permit before original runtime use.
/// Physical quiescence does not settle the entered attempt or clear retained charges.
pub(crate) struct CountedSourceEntry<T> {
    _source: T,
    _runtime: ResourceUse,
}
pub(crate) struct PublicationCustody {
    _source: SourceEffectPermit,
    _facts: super::native_read_capsule::NativeReadFacts,
    _capsule: super::native_read_capsule::NativeReadCapsule,
    target: PreparedPublicationTarget,
    _input: InputPermit,
    fence: super::publication_fence::PendingFence,
    charge: super::super::capacity_provider::PublicationChargeBinding,
}
pub(crate) type PublicationSourceUse = CountedSourceEntry<PublicationCustody>;
#[cfg(all(test, unix))]
impl PublicationSourceUse {
    pub(crate) fn validation(&self) -> &super::FileOperationResult {
        self._source.target.validation()
    }
}

fn closed(error: super::resource_custody::UseError) -> OperationJournalOwnerError {
    OperationJournalOwnerError::StagingGrant {
        reason: format!("original publication entry admission refused: {error:?}"),
    }
}

pub(super) fn begin_source_entry(
    admission: &Arc<ResourceAdmission>,
    pin: SourceEffectPin,
    target: PreparedPublicationTarget,
    input: InputPermit,
    fence: super::publication_fence::PendingFence,
    charge: super::super::capacity_provider::PublicationChargeBinding,
    capsule: super::native_read_capsule::NativeReadCapsule,
) -> Result<PublicationSourceUse, OperationJournalOwnerError> {
    super::validate_publication_target(&target)?;
    if !charge.is_current() {
        return Err(OperationJournalOwnerError::StagingGrant {
            reason: "original publication charge is no longer current".into(),
        });
    }
    counted_entry_observed(
        admission,
        move |resource| {
            let facts = capsule.read(resource).map_err(|error| {
                OperationJournalOwnerError::StagingGrant {
                    reason: format!("original publication native sample refused: {error:?}"),
                }
            })?;
            Ok((capsule, facts))
        },
        move |(capsule, facts)| {
            let source = pin.begin_effect()?;
            Ok(PublicationCustody {
                _source: source,
                _facts: facts,
                _capsule: capsule,
                target,
                _input: input,
                fence,
                charge,
            })
        },
        |custody, resource| {
            // Association verification takes the original admission mutex outside final entry.
            if !custody._facts.matches(&custody._capsule, resource) {
                return Err(super::resource_custody::UseError::Closed);
            }
            Ok(())
        },
        |custody| {
            if !custody.target.is_current() {
                return Err(super::resource_custody::UseError::Closed);
            }
            // admission→original mirror→fence; native IO and association checks already ended.
            enter_charge_fence_when(&custody.charge, &mut custody.fence, |total| {
                let claim = custody.charge.claim();
                capacity_fits(
                    claim.volume(),
                    claim.allocated_bytes(),
                    custody._facts.facts(),
                    total,
                )
            })
        },
    )
}

fn capacity_fits(
    volume: &crate::durable_capacity::VolumeIdentity,
    allocated: u64,
    facts: &crate::durable_capacity::VolumeFacts,
    incremental: u64,
) -> bool {
    facts.identity == *volume
        && facts.allocation_unit != 0
        && allocated != 0
        && allocated % facts.allocation_unit == 0
        && crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR
            .checked_add(incremental)
            .is_some_and(|required| facts.free_bytes >= required)
}
fn enter_charge_fence_when(
    charge: &super::super::capacity_provider::PublicationChargeBinding,
    fence: &mut super::publication_fence::PendingFence,
    fits: impl FnOnce(u64) -> bool,
) -> Result<(), super::resource_custody::UseError> {
    charge
        .with_current(|observation| {
            if fits(observation.incremental_bytes) {
                fence.enter_checked()
            } else {
                Err(super::resource_custody::UseError::Closed)
            }
        })
        .ok_or(super::resource_custody::UseError::Closed)?
}
#[cfg(test)]
fn enter_charge_fence(
    charge: &super::super::capacity_provider::PublicationChargeBinding,
    fence: &mut super::publication_fence::PendingFence,
) -> Result<(), super::resource_custody::UseError> {
    enter_charge_fence_when(charge, fence, |_| true)
}

#[cfg(test)]
fn counted_entry<T>(
    admission: &Arc<ResourceAdmission>,
    acquire_source: impl FnOnce() -> Result<T, OperationJournalOwnerError>,
) -> Result<CountedSourceEntry<T>, OperationJournalOwnerError> {
    counted_entry_checked(admission, acquire_source, |_| Ok(()))
}

#[cfg(test)]
fn counted_entry_checked<T>(
    admission: &Arc<ResourceAdmission>,
    acquire_source: impl FnOnce() -> Result<T, OperationJournalOwnerError>,
    is_current: impl FnOnce(&mut T) -> Result<(), super::resource_custody::UseError>,
) -> Result<CountedSourceEntry<T>, OperationJournalOwnerError> {
    counted_entry_observed(
        admission,
        |_| Ok(()),
        |_| acquire_source(),
        |_, _| Ok(()),
        is_current,
    )
}

fn counted_entry_observed<O, T>(
    admission: &Arc<ResourceAdmission>,
    read_native: impl FnOnce(&mut ResourceUse) -> Result<O, OperationJournalOwnerError>,
    acquire_source: impl FnOnce(O) -> Result<T, OperationJournalOwnerError>,
    verify_native: impl FnOnce(&T, &ResourceUse) -> Result<(), super::resource_custody::UseError>,
    is_current: impl FnOnce(&mut T) -> Result<(), super::resource_custody::UseError>,
) -> Result<CountedSourceEntry<T>, OperationJournalOwnerError> {
    // The same original use survives native read, source acquisition and effect upgrade.
    let mut runtime = admission.begin_read(|| true).map_err(closed)?;
    let observed = read_native(&mut runtime)?;
    runtime.verify_read_for(admission).map_err(closed)?;
    let mut source = acquire_source(observed)?;
    verify_native(&source, &runtime).map_err(closed)?;
    runtime
        .enter_effect_checked(|| is_current(&mut source))
        .map_err(closed)?;
    Ok(CountedSourceEntry {
        _source: source,
        _runtime: runtime,
    })
}

#[cfg(test)]
mod tests;
