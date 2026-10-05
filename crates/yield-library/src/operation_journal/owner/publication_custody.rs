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
    target: PreparedPublicationTarget,
    _input: InputPermit,
    fence: super::publication_fence::PendingFence,
    charge: super::super::capacity_provider::PublicationChargeBinding,
}
pub(crate) type PublicationSourceUse = CountedSourceEntry<PublicationCustody>;
#[cfg(all(test, target_os = "macos"))]
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
) -> Result<PublicationSourceUse, OperationJournalOwnerError> {
    super::validate_publication_target(&target)?;
    if !charge.is_current() {
        return Err(OperationJournalOwnerError::StagingGrant {
            reason: "original publication charge is no longer current".into(),
        });
    }
    counted_entry_checked(
        admission,
        || {
            let source = pin.begin_effect()?;
            Ok(PublicationCustody {
                _source: source,
                target,
                _input: input,
                fence,
                charge,
            })
        },
        |custody| {
            if !custody.target.is_current() {
                return Err(super::resource_custody::UseError::Closed);
            }
            // admission→original mirror→fence; only scalar transitions under these locks.
            enter_charge_fence(&custody.charge, &mut custody.fence)
        },
    )
}

fn enter_charge_fence(
    charge: &super::super::capacity_provider::PublicationChargeBinding,
    fence: &mut super::publication_fence::PendingFence,
) -> Result<(), super::resource_custody::UseError> {
    charge
        .with_current(|_| fence.enter_checked())
        .ok_or(super::resource_custody::UseError::Closed)?
}

#[cfg(test)]
fn counted_entry<T>(
    admission: &Arc<ResourceAdmission>,
    acquire_source: impl FnOnce() -> Result<T, OperationJournalOwnerError>,
) -> Result<CountedSourceEntry<T>, OperationJournalOwnerError> {
    counted_entry_checked(admission, acquire_source, |_| Ok(()))
}

fn counted_entry_checked<T>(
    admission: &Arc<ResourceAdmission>,
    acquire_source: impl FnOnce() -> Result<T, OperationJournalOwnerError>,
    is_current: impl FnOnce(&mut T) -> Result<(), super::resource_custody::UseError>,
) -> Result<CountedSourceEntry<T>, OperationJournalOwnerError> {
    // Count before touching the source gate. Source acquisition holds no admission mutex;
    // closing between acquisition and upgrade refuses and drops source before runtime use.
    let mut runtime = admission.begin_read(|| true).map_err(closed)?;
    let mut source = acquire_source()?;
    // Original admission, exact charge generation and issuer fence. Fresh qualified capacity,
    // qualified namespace entry and other mutation fences still belong to the adapter.
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
