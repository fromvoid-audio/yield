//! One original pre-lease read owner retained by the journal; no migration or capacity grant.
use super::*;
use crate::operation_journal::{
    ParticipantId,
    legacy_adoption_commands::{LegacyAdoptionInputError, LegacyAdoptionIntentArgs},
};
use crate::sample_sources::db::{LegacySourceReadOwner, SourceCommandId};
use std::sync::{Weak, atomic::AtomicBool};

// Charge frozen <=32KiB input plus bounded observation/header metadata on the existing pool.
// The already-open source SQLite connection is transferred, never opened by this route.
const INPUT_BYTES: usize = 64 * 1024;

/// Refusal of original readonly custody; none of these states authorizes a retry or mutation.
#[derive(Debug, Error)]
pub enum LegacyAdoptionCustodyError {
    /// The acquisition already retains an original pre-lease owner, including lost replies.
    #[error("original legacy adoption custody slot occupied")]
    Busy,
    /// The issuing journal acquisition is closing or unavailable.
    #[error("original legacy adoption custody owner closed")]
    Closed,
    /// Original source/profile/root/image or journal namespace no longer matches.
    #[error("original legacy adoption custody changed")]
    Changed,
    /// Genuine original-owner preparation rejected before any intent or writes.
    #[error(transparent)]
    Preparation(#[from] LegacyAdoptionInputError),
}

/// Privately issued observation of original source custody on the exact journal worker.
/// Frozen bytes cannot reconstruct it. It grants no claim, lease, write or migration authority.
///
/// ```compile_fail
/// fn clone(v: yield_library::operation_journal::owner::LegacyAdoptionCustody) { let _ = v.clone(); }
/// ```
/// ```compile_fail
/// let _ = yield_library::operation_journal::owner::LegacyAdoptionCustody {};
/// ```
pub struct LegacyAdoptionCustody {
    args: LegacyAdoptionIntentArgs,
    digest: [u8; 32],
    owner: Weak<OwnerShared>,
}
impl LegacyAdoptionCustody {
    /// Original bounded passive input; cloning/encoding this observation grants no custody.
    pub fn input(&self) -> &LegacyAdoptionIntentArgs {
        &self.args
    }
    /// Queue bounded revalidation on the original retained acquisition. Timeout or lost reply
    /// does not close, transfer or release its sole source slot. Consume the actual result.
    pub fn revalidate(
        &self,
    ) -> Result<OperationJournalRequest<()>, OperationJournalOwnerQueueError> {
        let shared = self
            .owner
            .upgrade()
            .ok_or(OperationJournalOwnerQueueError::Closed)?;
        let command = self.args.source_command_id();
        let digest = self.digest;
        OperationJournalClient { shared }.submit(64, move |input, result| {
            JournalCommand::ValidateLegacyCustody {
                command,
                digest,
                input,
                result,
            }
        })
    }
}

pub(super) struct RetainedLegacyAdoption {
    // Drop original SQLite/root/reservation ownership before refunding metadata credit.
    source: LegacySourceReadOwner,
    args: LegacyAdoptionIntentArgs,
    digest: [u8; 32],
    input: InputPermit,
}
impl OperationJournalClient {
    /// Transfer one genuine original readonly source owner into this journal's sole pre-lease
    /// custody slot. The worker captures its passive input against the original journal/profile.
    /// Source sidecar evidence survives rejection/shutdown. No operation identity is allocated.
    pub fn prepare_legacy_adoption_custody(
        &self,
        source: LegacySourceReadOwner,
        participant: ParticipantId,
        command: SourceCommandId,
    ) -> Result<OperationJournalRequest<LegacyAdoptionCustody>, OperationJournalOwnerQueueError>
    {
        if participant.as_uuid().is_nil() || command.as_uuid().is_nil() {
            return Err(OperationJournalOwnerQueueError::InvalidInput {
                reason: "nil legacy custody participant/command".into(),
            });
        }
        self.submit(INPUT_BYTES, move |input, result| {
            JournalCommand::PrepareLegacyCustody {
                source,
                participant,
                command,
                input,
                result,
            }
        })
    }
}
impl OwnerRuntime {
    pub(super) fn prepare_legacy_custody(
        &mut self,
        source: LegacySourceReadOwner,
        participant: ParticipantId,
        command: SourceCommandId,
        input: InputPermit,
    ) -> Result<LegacyAdoptionCustody, OperationJournalOwnerError> {
        use LegacyAdoptionCustodyError as E;
        if self.legacy_adoption.is_some() {
            return Err(E::Busy.into());
        }
        if input.shared.status() != OperationJournalOwnerStatus::Available {
            return Err(E::Closed.into());
        }
        self.store()
            .validate_for_admission()
            .map_err(|_| E::Changed)?;
        let profile = self.final_profile_guard.as_ref().ok_or(E::Closed)?;
        let args = source
            .prepare_legacy_adoption_input(
                profile,
                self.store(),
                participant,
                command,
                &AtomicBool::new(false),
            )
            .map_err(E::Preparation)?;
        let encoded = args.canonical_bytes().map_err(E::Preparation)?;
        if encoded.len() > super::super::legacy_adoption_commands::MAX_LEGACY_ADOPTION_INPUT_BYTES {
            return Err(E::Changed.into());
        }
        let digest = *blake3::hash(&encoded).as_bytes();
        source.validate(profile).map_err(|_| E::Changed)?;
        self.store()
            .validate_for_admission()
            .map_err(|_| E::Changed)?;
        if input.shared.status() != OperationJournalOwnerStatus::Available {
            return Err(E::Closed.into());
        }
        let observation = LegacyAdoptionCustody {
            args: args.clone(),
            digest,
            owner: Arc::downgrade(&input.shared),
        };
        self.legacy_adoption = Some(RetainedLegacyAdoption {
            source,
            args,
            digest,
            input,
        });
        Ok(observation)
    }
    pub(super) fn validate_legacy_custody(
        &self,
        command: SourceCommandId,
        digest: [u8; 32],
    ) -> Result<(), OperationJournalOwnerError> {
        use LegacyAdoptionCustodyError as E;
        let retained = self.legacy_adoption.as_ref().ok_or(E::Closed)?;
        if retained.input.shared.status() != OperationJournalOwnerStatus::Available {
            return Err(E::Closed.into());
        }
        if retained.args.source_command_id() != command || retained.digest != digest {
            return Err(E::Changed.into());
        }
        let profile = self.final_profile_guard.as_ref().ok_or(E::Closed)?;
        self.store()
            .validate_for_admission()
            .map_err(|_| E::Changed)?;
        let image = retained.source.capture(profile).map_err(|_| E::Changed)?;
        if image.digest() != retained.args.original_image_digest() {
            return Err(E::Changed.into());
        }
        retained.source.validate(profile).map_err(|_| E::Changed)?;
        self.store()
            .validate_for_admission()
            .map_err(|_| E::Changed)?;
        if retained.input.shared.status() != OperationJournalOwnerStatus::Available {
            return Err(E::Closed.into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
