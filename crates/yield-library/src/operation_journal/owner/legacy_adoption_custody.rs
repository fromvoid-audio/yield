//! One original pre-lease read owner retained by the journal; no migration or capacity grant.
use super::*;
use crate::operation_journal::{
    ParticipantId,
    legacy_adoption_commands::{LegacyAdoptionInputError, LegacyAdoptionIntentArgs},
};
use crate::sample_sources::db::{LegacySourceReadOwner, SourceCommandId};
use std::sync::{Weak, atomic::AtomicBool};

// Preserve 64KiB for frozen <=32KiB input and bounded observation/header metadata;
// additionally charge the derived sequential schema/row scratch peak, not audio input bytes.
// One separately bound original MAIN reserve root has its own explicit inventory.
// The already-open source SQLite connection is transferred, never opened by this route.
pub(super) const INPUT_BYTES: usize = 64 * 1024
    + crate::sample_sources::db::LEGACY_CAPTURE_SCRATCH_BYTES
    + super::legacy_adoption_volumes::ROOT_BYTES;

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
    /// Partial reserve bootstrap remains retained; no retry or cleanup is authorized.
    #[error("original legacy reserve custody requires recovery")]
    ReserveRecovery,
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
    pub(super) args: LegacyAdoptionIntentArgs,
    pub(super) digest: [u8; 32],
    pub(super) owner: Weak<OwnerShared>,
    // Newly returned reserve checkpoints retain their own reply-copy credit after delivery.
    pub(super) _reply_input: Option<InputPermit>,
}
impl LegacyAdoptionCustody {
    /// Original bounded passive input; cloning/encoding this observation grants no custody.
    pub fn input(&self) -> &LegacyAdoptionIntentArgs {
        &self.args
    }
    /// Install original pre-lease reserve custody on the same worker and return a newly
    /// captured passive checkpoint. No ordinary claim, migration grant or lease is issued.
    /// A failed physical preparation retains its partial backing until actual shutdown.
    pub fn prepare_volume_custody(
        &self,
    ) -> Result<OperationJournalRequest<LegacyAdoptionCustody>, OperationJournalOwnerQueueError>
    {
        let shared = self
            .owner
            .upgrade()
            .ok_or(OperationJournalOwnerQueueError::Closed)?;
        let command = self.args.source_command_id();
        let digest = self.digest;
        OperationJournalClient { shared }.submit(
            super::legacy_adoption_volumes::REPLY_BYTES,
            move |input, result| JournalCommand::PrepareLegacyVolumes {
                command,
                digest,
                input,
                result,
            },
        )
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
    // Added reserve/failure/volume backing closes before the original source reservation.
    pub(super) volumes: Option<super::legacy_adoption_volumes::RetainedLegacyVolumes>,
    // Original SQLite closes before its own root/reservation, and all backing before credit.
    pub(super) source: LegacySourceReadOwner,
    pub(super) args: LegacyAdoptionIntentArgs,
    pub(super) digest: [u8; 32],
    pub(super) input: InputPermit,
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
        mut source: LegacySourceReadOwner,
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
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let profile = self.final_profile_guard.as_ref().ok_or(E::Closed)?;
        let args = source
            .prepare_legacy_adoption_input_until(
                profile,
                self.store(),
                participant,
                command,
                &AtomicBool::new(false),
                deadline,
            )
            .map_err(E::Preparation)?;
        let encoded = args.canonical_bytes().map_err(E::Preparation)?;
        if encoded.len() > super::super::legacy_adoption_commands::MAX_LEGACY_ADOPTION_INPUT_BYTES {
            return Err(E::Changed.into());
        }
        let digest = *blake3::hash(&encoded).as_bytes();
        drop(encoded);
        source.validate(profile).map_err(|_| E::Changed)?;
        self.store()
            .validate_for_admission()
            .map_err(|_| E::Changed)?;
        if input.shared.status() != OperationJournalOwnerStatus::Available {
            return Err(E::Closed.into());
        }
        source
            .prepare_no_effect_writable_until(profile, deadline)
            .map_err(|_| E::Changed)?;
        // The preparation fence confers no admission. Freeze and compare the complete original
        // input again through the retained no-effect connection before publishing custody.
        let prepared = source
            .prepare_legacy_adoption_input_until(
                profile,
                self.store(),
                participant,
                command,
                &AtomicBool::new(false),
                deadline,
            )
            .map_err(|_| E::Changed)?;
        if prepared != args {
            return Err(E::Changed.into());
        }
        self.store()
            .validate_for_admission()
            .map_err(|_| E::Changed)?;
        if input.shared.status() != OperationJournalOwnerStatus::Available {
            return Err(E::Closed.into());
        }
        let observation = LegacyAdoptionCustody {
            _reply_input: None,
            args: args.clone(),
            digest,
            owner: Arc::downgrade(&input.shared),
        };
        self.legacy_adoption = Some(RetainedLegacyAdoption {
            source,
            args,
            digest,
            volumes: None,
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
        let (bytes, identity) = self.store().rollback_facts().map_err(|_| E::Changed)?;
        if !retained
            .args
            .matches_journal_rollback(bytes, identity.as_deref())
        {
            return Err(E::Changed.into());
        }
        self.store()
            .validate_for_admission()
            .map_err(|_| E::Changed)?;
        // The frozen input includes source and journal policy, all eight original objects,
        // and both control descriptors. A source row digest alone does not revalidate it.
        let current = retained
            .source
            .prepare_legacy_adoption_input(
                profile,
                self.store(),
                retained.args.participant_id(),
                command,
                &AtomicBool::new(false),
            )
            .map_err(|_| E::Changed)?;
        if current != retained.args {
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
