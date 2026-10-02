//! Registration-local dispatch observations; no journal or effect authority.
use super::{GlobalPreparationError, WriterCommand};

// Exact future SQLite inventory fields are positive i64. Never publish an unpersistable fence.
pub(super) const MAX_REVISION: u64 = i64::MAX as u64;

pub(super) struct MutationFence {
    revision: u64,
}
impl MutationFence {
    pub(super) fn new() -> Self {
        Self { revision: 1 }
    }
    pub(super) fn current(&self) -> u64 {
        self.revision
    }
    pub(super) fn advance(&mut self) -> Result<(), ()> {
        let next = self
            .revision
            .checked_add(1)
            .filter(|v| *v <= MAX_REVISION)
            .ok_or(())?;
        self.revision = next;
        Ok(())
    }
    pub(super) fn validate(&self, expected: Option<u64>) -> Result<(), GlobalPreparationError> {
        if expected.is_some_and(|v| v != self.revision) {
            return Err(GlobalPreparationError::StaleMutationFence);
        }
        if self.revision >= MAX_REVISION {
            return Err(GlobalPreparationError::MutationFenceExhausted);
        }
        Ok(())
    }
    #[cfg(test)]
    pub(super) fn from_test_seed(revision: u64) -> Self {
        assert!((1..=MAX_REVISION).contains(&revision));
        Self { revision }
    }
}

// Exhaustive matching makes every new command require an explicit policy. An attempt invalidates
// before validation/SQL, including no-op, rejected and replayed mutation attempts.
pub(super) fn potentially_mutates(command: &WriterCommand) -> bool {
    match command {
        WriterCommand::HarvestAuthored { .. }
        | WriterCommand::HarvestCopy { .. }
        | WriterCommand::ReferenceRekey { .. }
        | WriterCommand::CapturedMutation { .. }
        | WriterCommand::ApplyOperationLink { .. }
        | WriterCommand::ApplySourceRegistryParticipant { .. }
        | WriterCommand::ReplaceSourceRegistry { .. }
        | WriterCommand::ReplaceUiPreferences { .. }
        | WriterCommand::SetProfileTagCategory { .. }
        | WriterCommand::DeleteProfileTagVocabulary { .. } => true,
        WriterCommand::Prepare(_)
        | WriterCommand::HarvestFacts { .. }
        | WriterCommand::HarvestAuthoredSnapshot { .. }
        | WriterCommand::ReferenceSnapshot { .. }
        | WriterCommand::LoadSourceRegistry { .. }
        | WriterCommand::LookupRetainedSource { .. }
        | WriterCommand::LoadUiPreferences { .. }
        | WriterCommand::LookupProfileTagCategories { .. }
        | WriterCommand::ProfileTagVocabularyPage { .. } => false,
        #[cfg(any(test, feature = "test-support"))]
        WriterCommand::PanicForTest { .. } => false,
    }
}

#[cfg(test)]
mod tests;
