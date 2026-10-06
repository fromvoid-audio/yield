//! Original pre-start producer plumbing, with no startup or terminal/effect authority.
use super::super::participants::{ParticipantKind, ParticipantRequirement};
use super::*;
use reson::recording::{
    RecordingLimits, RecordingSettlements, RecordingSinkBinding, RecordingTransport,
    RecordingTransportReceiver,
};
use std::sync::Weak;

/// Correlate the exact destination observation with this consumer's held original Source.
/// This borrowed check does not register a Source, issue admission or grant an effect.
pub(super) fn matches_original_destination(
    proposal: &RecordingProposal,
    request: &crate::sample_sources::db::SourceAuthorityRequest,
    epoch: u64,
) -> bool {
    let parent = proposal.destination().parent();
    proposal
        .participants()
        .descriptors()
        .iter()
        .any(|participant| {
            participant.id() == parent.participant_id()
                && participant.kind() == ParticipantKind::SourceDatabase
                && participant.requirement() == ParticipantRequirement::Required
                && participant.source_fence().is_some_and(|fence| {
                    fence.source_id() == parent.source_id()
                        && fence.root_identity() == request.media_root().identity()
                        && fence.database_identity() == request.database_file_identity()
                        && fence.lease_epoch() == epoch
                })
        })
}

// Validated proposal (192 KiB), original route backing and original pin/receipt context.
// No callback ring, serializer or pending write is allocated by this pre-start factory.
const ROUTE_BYTES: usize = 64 * 1024;
const INPUT_BYTES: usize = 192 * 1024 + ROUTE_BYTES + 192 * 1024;
// The command and retained slot are separate original-pool debits. All temporary read,
// decode and complete rebuild backing is destroyed before shrinking the factory debit.
const COMMAND_INPUT_BYTES: usize = super::super::store::RECORDING_PRESTART_INPUT_BYTES;

/// Refusal of original pre-start plumbing; no result authorizes capture or terminal SQL.
#[derive(Debug, Error)]
pub enum RecordingProducerCustodyError {
    /// The original worker already retains its single preparation, including a lost reply.
    #[error("original recording producer custody slot occupied")]
    Busy,
    /// The original journal acquisition is closing or unavailable.
    #[error("original recording producer custody closed")]
    Closed,
    /// Original registration, source, proposal, retained inventory or provider has changed.
    #[error("original recording producer custody changed or unavailable")]
    Changed,
    /// The bounded original proposal cannot fit this preparation.
    #[error("original recording producer custody input bound exceeded")]
    Full,
}

/// Weak observation of privately retained original producer parts; never a startup capability.
///
/// ```compile_fail
/// fn duplicate(v: yield_library::operation_journal::owner::RecordingProducerCustody) { let _ = v.clone(); }
/// ```
/// ```compile_fail
/// let _ = yield_library::operation_journal::owner::RecordingProducerCustody {};
/// ```
/// ```compile_fail
/// fn start(v: yield_library::operation_journal::owner::RecordingProducerCustody) { let _ = v.take_sink_binding(); }
/// ```
pub struct RecordingProducerCustody {
    operation: OperationId,
    digest: [u8; 32],
    owner: Weak<OwnerShared>,
}
impl RecordingProducerCustody {
    /// Frozen operation observation, without startup or admission authority.
    pub fn operation_id(&self) -> OperationId {
        self.operation
    }
    /// Exact proposal digest observation; decoding it cannot reconstruct this facade.
    pub fn proposal_hash(&self) -> [u8; 32] {
        self.digest
    }
    /// Revalidate on the original worker. Lost replies never release or replace its slot.
    pub fn revalidate(
        &self,
    ) -> Result<OperationJournalRequest<()>, OperationJournalOwnerQueueError> {
        let shared = self
            .owner
            .upgrade()
            .ok_or(OperationJournalOwnerQueueError::Closed)?;
        let operation = self.operation;
        let digest = self.digest;
        OperationJournalClient { shared }.submit(COMMAND_INPUT_BYTES, move |input, result| {
            JournalCommand::ValidateRecordingCustody {
                operation,
                digest,
                input,
                result,
            }
        })
    }
}

pub(super) struct RetainedRecordingProducer {
    // Original parts never leave this worker. Drop them/pin before refunding memory credit.
    _binding: RecordingSinkBinding,
    _receiver: RecordingTransportReceiver,
    _settlements: RecordingSettlements,
    source_pin: SourceEffectPin,
    registration: SourceAuthorityRegistrationReceipt,
    proposal: RecordingProposal,
    digest: [u8; 32],
    invalidated: bool,
    input: InputPermit,
}
impl OperationJournalClient {
    /// Retain pre-start memory plumbing only after the original admitted/source/provider checks.
    /// Future startup must additionally consume genuine required-WAL/physical admission.
    pub fn prepare_recording_producer_custody(
        &self,
        operation: OperationId,
        registration: SourceAuthorityRegistrationReceipt,
        source: &crate::sample_sources::db::SourceAuthorityRegistration,
    ) -> Result<OperationJournalRequest<RecordingProducerCustody>, OperationJournalOwnerQueueError>
    {
        if operation.as_uuid().is_nil() {
            return Err(OperationJournalOwnerQueueError::InvalidInput {
                reason: "nil recording operation".into(),
            });
        }
        let (_request, source_pin) =
            source
                .metadata_handoff_parts(&registration)
                .map_err(|error| OperationJournalOwnerQueueError::InvalidInput {
                    reason: error.to_string(),
                })?;
        self.submit(COMMAND_INPUT_BYTES, move |input, result| {
            JournalCommand::PrepareRecordingCustody {
                operation,
                registration,
                source_pin,
                input,
                result,
            }
        })
    }
}
impl OwnerRuntime {
    fn original_recording_prestart(
        &mut self,
        operation: OperationId,
        registration: &SourceAuthorityRegistrationReceipt,
        pin: &SourceEffectPin,
    ) -> Result<RecordingProposal, OperationJournalOwnerError> {
        self.store().begin_recording_prestart_read().map_err(
            |error| -> OperationJournalOwnerError {
                match error {
                super::super::store::OperationJournalStoreError::RecordingPrestartResourceBound => {
                    RecordingProducerCustodyError::Full.into()
                }
                error => error.into(),
            }
            },
        )?;
        let result = self.original_recording_prestart_inside(operation, registration, pin);
        // Always end the original read snapshot before producing any facade or factory parts.
        self.store().finish_recording_prestart_read()?;
        result.map_err(|error| match error {
            OperationJournalOwnerError::Store(
                OperationJournalStoreError::RecordingPrestartResourceBound,
            ) => RecordingProducerCustodyError::Full.into(),
            error => error,
        })
    }

    fn original_recording_prestart_inside(
        &mut self,
        operation: OperationId,
        registration: &SourceAuthorityRegistrationReceipt,
        pin: &SourceEffectPin,
    ) -> Result<RecordingProposal, OperationJournalOwnerError> {
        use RecordingProducerCustodyError as E;
        self.resource_admission
            .validate_open()
            .map_err(|_| E::Closed)?;
        self.revalidate_capacity_boundary()
            .map_err(|_| E::Changed)?;
        let source = self
            .source_registrations
            .get(&registration.registration_id)
            .ok_or(E::Changed)?;
        if source.receipt != *registration
            || !matches!(source.readiness, SourceRegistrationReadiness::Registered)
        {
            return Err(E::Changed.into());
        }
        let profile = self.final_profile_guard.as_ref().ok_or(E::Closed)?;
        pin.validate_for_issuance(&source.request, profile)?;
        let _source_effect = pin.begin_effect()?;
        let proposal = self.store().recording_prestart_proposal(operation)?;
        let epoch = u64::try_from(registration.lease_identity.epoch()).map_err(|_| E::Changed)?;
        if !matches_original_destination(&proposal, &source.request, epoch) {
            return Err(E::Changed.into());
        }
        let snapshot = self.store().rebuild_recording_prestart_snapshot()?;
        if matches!(
            snapshot.readiness(),
            CapacityRebuildReadiness::ReconciliationRequired { .. }
        ) {
            return Err(E::Changed.into());
        }
        let provider = self.capacity_provider.as_ref().ok_or(E::Changed)?;
        provider
            .validate_rebuilt_claims(&snapshot)
            .map_err(|_| E::Changed)?;
        provider.validate_recovery_state().map_err(|_| E::Changed)?;
        pin.validate_for_issuance(&source.request, profile)?;
        Ok(proposal)
    }

    pub(super) fn prepare_recording_custody(
        &mut self,
        operation: OperationId,
        registration: SourceAuthorityRegistrationReceipt,
        source_pin: SourceEffectPin,
        mut input: InputPermit,
    ) -> Result<RecordingProducerCustody, OperationJournalOwnerError> {
        use RecordingProducerCustodyError as E;
        if self.recording_producer.is_some() {
            return Err(E::Busy.into());
        }
        if input.shared.status() != OperationJournalOwnerStatus::Available {
            return Err(E::Closed.into());
        }
        let proposal = self.original_recording_prestart(operation, &registration, &source_pin)?;
        super::super::recording_commands::controls::reservation_metadata_bound(&proposal)
            .map_err(|_| E::Changed)?;
        let encoded = proposal.encode().map_err(|_| E::Changed)?;
        let bound = encoded
            .len()
            .checked_mul(3)
            .and_then(|n| n.checked_add(ROUTE_BYTES))
            .ok_or(E::Full)?;
        if bound > INPUT_BYTES || input.bytes != COMMAND_INPUT_BYTES {
            return Err(E::Full.into());
        }
        let digest = *blake3::hash(&encoded).as_bytes();
        let format = proposal.format();
        let limits =
            RecordingLimits::new(format.sample_rate(), format.channels(), format.max_frames())
                .map_err(|_| E::Changed)?;
        #[cfg(test)]
        FACTORIES.fetch_add(1, Ordering::AcqRel);
        let (binding, receiver, settlements) = RecordingTransport::bind(limits);
        let facade = RecordingProducerCustody {
            operation,
            digest,
            owner: Arc::downgrade(&input.shared),
        };
        drop(encoded);
        // Read snapshot, decoded outer records, audit maps and canonical scratch are gone.
        // This releases memory credit only; it changes no durable reservation or settlement.
        let transient_bytes = input.bytes.checked_sub(INPUT_BYTES).ok_or(E::Full)?;
        input.bytes = INPUT_BYTES;
        input
            .shared
            .release_input(input.class, transient_bytes, input.publication);
        self.recording_producer = Some(RetainedRecordingProducer {
            _binding: binding,
            _receiver: receiver,
            _settlements: settlements,
            source_pin,
            registration,
            proposal,
            digest,
            invalidated: false,
            input,
        });
        Ok(facade)
    }

    pub(super) fn validate_recording_custody(
        &mut self,
        operation: OperationId,
        digest: [u8; 32],
    ) -> Result<(), OperationJournalOwnerError> {
        use RecordingProducerCustodyError as E;
        let mut retained = self.recording_producer.take().ok_or(E::Changed)?;
        let result = (|| {
            if retained.invalidated
                || retained.proposal.operation_id() != operation
                || retained.digest != digest
            {
                return Err(E::Changed.into());
            }
            if retained.input.shared.status() != OperationJournalOwnerStatus::Available {
                return Err(E::Closed.into());
            }
            let current = self.original_recording_prestart(
                operation,
                &retained.registration,
                &retained.source_pin,
            )?;
            if current != retained.proposal
                || *blake3::hash(&current.encode().map_err(|_| E::Changed)?).as_bytes() != digest
            {
                return Err(E::Changed.into());
            }
            Ok(())
        })();
        if result.is_err() {
            retained.invalidated = true;
        }
        self.recording_producer = Some(retained);
        result
    }
}

#[cfg(test)]
static FACTORIES: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
mod tests;
