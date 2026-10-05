//! Original admission/resource custody for actual source entry; no namespace qualification.
use super::resource_custody::ResourceUse;
use super::{OperationJournalOwnerError, ResourceAdmission, SourceEffectPermit, SourceEffectPin};
use std::sync::Arc;

/// Fields drop in declaration order: release the source permit before original runtime use.
/// Physical quiescence does not settle the entered attempt or clear retained charges.
pub(crate) struct CountedSourceEntry<T> {
    _source: T,
    _runtime: ResourceUse,
}
pub(crate) type PublicationSourceUse = CountedSourceEntry<SourceEffectPermit>;

fn closed(error: super::resource_custody::UseError) -> OperationJournalOwnerError {
    OperationJournalOwnerError::StagingGrant {
        reason: format!("original publication entry admission refused: {error:?}"),
    }
}

pub(super) fn begin_source_entry(
    admission: &Arc<ResourceAdmission>,
    pin: SourceEffectPin,
) -> Result<PublicationSourceUse, OperationJournalOwnerError> {
    counted_entry(admission, || pin.begin_effect().map_err(Into::into))
}

fn counted_entry<T>(
    admission: &Arc<ResourceAdmission>,
    acquire_source: impl FnOnce() -> Result<T, OperationJournalOwnerError>,
) -> Result<CountedSourceEntry<T>, OperationJournalOwnerError> {
    // Count before touching the source gate. Source acquisition holds no admission mutex;
    // closing between acquisition and upgrade refuses and drops source before runtime use.
    let mut runtime = admission.begin_read(|| true).map_err(closed)?;
    let source = acquire_source()?;
    // This is only the aggregate original-runtime gate, not a per-operation lifetime slot
    // or capacity/target/native namespace check. The qualified adapter must add those.
    runtime.enter_effect(|| true).map_err(closed)?;
    Ok(CountedSourceEntry {
        _source: source,
        _runtime: runtime,
    })
}

#[cfg(test)]
mod tests;
