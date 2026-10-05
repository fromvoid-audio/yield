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
) -> Result<PublicationSourceUse, OperationJournalOwnerError> {
    super::validate_publication_target(&target)?;
    counted_entry_checked(
        admission,
        || {
            let source = pin.begin_effect()?;
            Ok(PublicationCustody {
                _source: source,
                target,
                _input: input,
            })
        },
        |custody| custody.target.is_current(),
    )
}

#[cfg(test)]
fn counted_entry<T>(
    admission: &Arc<ResourceAdmission>,
    acquire_source: impl FnOnce() -> Result<T, OperationJournalOwnerError>,
) -> Result<CountedSourceEntry<T>, OperationJournalOwnerError> {
    counted_entry_checked(admission, acquire_source, |_| true)
}

fn counted_entry_checked<T>(
    admission: &Arc<ResourceAdmission>,
    acquire_source: impl FnOnce() -> Result<T, OperationJournalOwnerError>,
    is_current: impl FnOnce(&T) -> bool,
) -> Result<CountedSourceEntry<T>, OperationJournalOwnerError> {
    // Count before touching the source gate. Source acquisition holds no admission mutex;
    // closing between acquisition and upgrade refuses and drops source before runtime use.
    let mut runtime = admission.begin_read(|| true).map_err(closed)?;
    let source = acquire_source()?;
    // This is only the aggregate original-runtime gate, not a per-operation lifetime slot
    // or capacity/target/native namespace check. The qualified adapter must add those.
    runtime
        .enter_effect(|| is_current(&source))
        .map_err(closed)?;
    Ok(CountedSourceEntry {
        _source: source,
        _runtime: runtime,
    })
}

#[cfg(test)]
mod tests;
