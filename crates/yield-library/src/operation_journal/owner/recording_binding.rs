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
// decode and complete rebuild backing is held by the original request reply.
const COMMAND_INPUT_BYTES: usize = super::super::store::RECORDING_PRESTART_INPUT_BYTES;
// Revalidation retains the original producer while its independent temporary read
// runs. Both complete debits must fit without borrowing the reserved control pool.
const _: () = assert!(COMMAND_INPUT_BYTES == INPUT_BYTES);
const _: () = assert!(COMMAND_INPUT_BYTES + INPUT_BYTES <= super::ORDINARY_INPUT_BYTES);

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
        OperationJournalClient { shared }.submit_funded_with_reply(
            InputClass::Ordinary,
            COMMAND_INPUT_BYTES,
            false,
            true,
            move |input, result| JournalCommand::ValidateRecordingCustody {
                operation,
                digest,
                input,
                result,
            },
        )
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
    terminal: Option<super::super::recording_commands::controls::RecordingFinalizingInventory>,
    _terminal_barrier: Option<reson::recording::RecordingTerminalBarrier>,
    _terminal_joined: Option<reson::recording::RecordingJoinedCompletion>,
    _terminal_error: Option<OperationJournalOwnerError>,
    _terminal_backing: Option<InputPermit>,
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
        self.submit_funded_with_reply(
            InputClass::Ordinary,
            COMMAND_INPUT_BYTES,
            false,
            true,
            move |input, result| JournalCommand::PrepareRecordingCustody {
                operation,
                registration,
                source_pin,
                input,
                result,
            },
        )
    }
}
impl OwnerRuntime {
    fn original_recording_prestart(
        &mut self,
        operation: OperationId,
        registration: &SourceAuthorityRegistrationReceipt,
        pin: &SourceEffectPin,
        input: &InputPermit,
    ) -> Result<RecordingProposal, OperationJournalOwnerError> {
        let working = self
            .store()
            .begin_recording_prestart_read_with_input(Some(input))
            .map_err(|error| -> OperationJournalOwnerError {
                match error {
                super::super::store::OperationJournalStoreError::RecordingPrestartResourceBound => {
                    RecordingProducerCustodyError::Full.into()
                }
                error => error.into(),
            }
            })?
            .expect("funded recording read retains its working loan");
        let result = self.original_recording_prestart_inside(operation, registration, pin);
        // Always end the original read snapshot before producing any facade or factory parts.
        let result = match self.store().finish_recording_prestart_read() {
            Ok(()) => result,
            Err(error) => Err(error.into()),
        };
        let result = result.map_err(|error| match error {
            OperationJournalOwnerError::Store(
                OperationJournalStoreError::RecordingPrestartResourceBound,
            ) => RecordingProducerCustodyError::Full.into(),
            error => error,
        });
        // Keep working backing through proposal validation/encoding and any
        // selected error. The original reply owns it until consumption; success
        // detaches that reply from the independently funded retained slot.
        input.retain_historical_error(working, None, None, None);
        result
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
        let proposal =
            self.original_recording_prestart(operation, &registration, &source_pin, &input)?;
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
        // Success retains exactly the original slot debit. Reply backing belongs
        // to the request consumer, never to the retained producer.
        input.historical_reply = None;
        // The fixed command debit becomes the retained slot debit. Its
        // independent reply releases only temporary read/decode working memory.
        self.recording_producer = Some(RetainedRecordingProducer {
            _binding: binding,
            _receiver: receiver,
            _settlements: settlements,
            source_pin,
            registration,
            proposal,
            digest,
            invalidated: false,
            terminal: None,
            _terminal_barrier: None,
            _terminal_joined: None,
            _terminal_error: None,
            _terminal_backing: None,
            input,
        });
        Ok(facade)
    }

    /// Inspect original one-shot producer evidence without waiting on the actor. This
    /// factory has no required-WAL/startup capability: an unexpected terminal observation is retained
    /// for audit with acknowledgement withheld, never acknowledged on the strength of serialized observations.
    pub(super) fn poll_recording_custody(&mut self) {
        let Some(mut retained) = self.recording_producer.take() else {
            return;
        };
        if let Ok(barrier) = retained._settlements.try_barrier() {
            let observed = (|| {
                let profile = self.final_profile_guard.as_ref()?;
                let acquisition = uuid::Uuid::parse_str(profile.owner_identity()).ok()?;
                let inventory = super::super::recording_commands::controls::RecordingFinalizingInventory::capture(
                    &retained.proposal,
                    uuid::Uuid::new_v4(),
                    acquisition,
                    1,
                    &retained._settlements,
                    &barrier,
                ).ok()?;
                Some(inventory)
            })();
            if let Some(inventory) = observed.as_ref() {
                if let Ok(input) = retained.input.shared.startup_input() {
                    if let Err(error) =
                        self.retain_original_finalizing(&retained, inventory, &barrier, &input)
                    {
                        // Keep original selected error loans under the retained producer.
                        // Neither failed COMMIT nor failed readback permits a replacement.
                        if !self.store().connection_is_autocommit_for_recording()
                            || matches!(&error,
                                OperationJournalOwnerError::RecordingTerminalRollbackUncertain { .. }
                                | OperationJournalOwnerError::Store(
                                    OperationJournalStoreError::RecordingTerminalOutcomeUncertain { .. }
                                    | OperationJournalStoreError::RecordingTerminalRollbackUncertain { .. }
                                )) {
                            retained.input.shared.mark_unavailable("original Recording terminal outcome needs audit");
                        }
                        retained._terminal_error = Some(error);
                        retained._terminal_backing = Some(input);
                    }
                }
            }
            retained.terminal = observed;
            retained.invalidated = true;
            retained._terminal_barrier = Some(barrier);
        }
        if let Ok(joined) = retained._settlements.try_joined() {
            // Genuine join evidence must also belong to the original private instance and
            // agree with the inventory. Even success cannot upgrade this pre-start factory.
            let _matches = retained.terminal.as_ref().is_some_and(|inventory| {
                inventory
                    .validate_joined(&retained.proposal, &retained._settlements, &joined)
                    .is_ok()
            });
            retained._terminal_joined = Some(joined);
            retained.invalidated = true;
        }
        self.recording_producer = Some(retained);
    }

    /// Barrier observation closes generic mutation immediately, even if SQL admission or
    /// persistence then fails. Only exact already retained data remains replayable.
    pub(super) fn validate_recording_custody_append(
        &self,
        chunk: &RecordingCapturedChunk,
    ) -> Result<(), OperationJournalOwnerError> {
        if self.recording_producer.as_ref().is_some_and(|retained| {
            retained.invalidated && retained.proposal.operation_id() == chunk.operation_id()
        }) {
            self.store().require_recording_existing_replay(chunk)?;
        }
        Ok(())
    }

    fn retain_original_finalizing(
        &mut self,
        retained: &RetainedRecordingProducer,
        inventory: &super::super::recording_commands::controls::RecordingFinalizingInventory,
        barrier: &reson::recording::RecordingTerminalBarrier,
        input: &InputPermit,
    ) -> Result<(), OperationJournalOwnerError> {
        use RecordingProducerCustodyError as E;
        if retained.invalidated || !retained._settlements.owns_barrier(barrier) {
            return Err(E::Changed.into());
        }
        self.publication_fences
            .close_for_mutation(retained.proposal.operation_id())?;
        let working = self
            .store()
            .begin_recording_terminal_write_with_input(input)?
            .expect("original funded terminal transaction");
        let mut result = (|| {
            self.resource_admission
                .validate_open()
                .map_err(|_| E::Closed)?;
            self.revalidate_capacity_boundary()
                .map_err(|_| E::Changed)?;
            let source = self
                .source_registrations
                .get(&retained.registration.registration_id)
                .ok_or(E::Changed)?;
            if source.receipt != retained.registration
                || !matches!(source.readiness, SourceRegistrationReadiness::Registered)
            {
                return Err(E::Changed.into());
            }
            let epoch = u64::try_from(retained.registration.lease_identity.epoch())
                .map_err(|_| E::Changed)?;
            if !matches_original_destination(&retained.proposal, &source.request, epoch) {
                return Err(E::Changed.into());
            }
            let profile = self.final_profile_guard.as_ref().ok_or(E::Closed)?;
            retained
                .source_pin
                .validate_for_issuance(&source.request, profile)?;
            let _source_effect = retained.source_pin.begin_effect()?;
            let snapshot = self
                .store()
                .validate_original_recording_terminal_context(&retained.proposal, input)?;
            if matches!(
                snapshot.readiness(),
                CapacityRebuildReadiness::ReconciliationRequired { .. }
            ) {
                return Err(E::Changed.into());
            }
            let provider = self.capacity_provider.as_ref().ok_or(E::Changed)?;
            snapshot
                .validate_provider(provider, Some(input))
                .map_err(|_| E::Changed)?;
            provider.validate_recovery_state().map_err(|_| E::Changed)?;
            drop(snapshot);
            self.store().record_finalizing_inside_original_transaction(
                &retained.proposal,
                inventory,
                input,
            )?;
            self.store()
                .finish_recording_terminal_write(input)
                .map_err(
                    |cause| OperationJournalStoreError::RecordingTerminalOutcomeUncertain {
                        phase: "commit or postcommit namespace",
                        cause: Box::new(cause),
                    },
                )?;
            self.store()
                .verify_finalizing_readback(&retained.proposal, inventory, input)
                .map_err(
                    |cause| OperationJournalStoreError::RecordingTerminalOutcomeUncertain {
                        phase: "postcommit readback",
                        cause: Box::new(cause),
                    },
                )?;
            retained
                .source_pin
                .validate_for_issuance(&source.request, profile)?;
            Ok(())
        })();
        if !self.store().connection_is_autocommit_for_recording() {
            if let Err(cause) = self.store().rollback_recording_terminal_write(input) {
                result = Err(
                    OperationJournalOwnerError::RecordingTerminalRollbackUncertain {
                        original: Box::new(result.err().unwrap_or_else(|| E::Changed.into())),
                        rollback: Box::new(cause),
                    },
                );
            }
        }
        // Original scope backing remains alive through all observed values and selected errors.
        input.retain_historical_error(working, None, None, None);
        result
    }

    pub(super) fn validate_recording_custody(
        &mut self,
        operation: OperationId,
        digest: [u8; 32],
        input: InputPermit,
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
                &input,
            )?;
            if current != retained.proposal
                || *blake3::hash(&current.encode().map_err(|_| E::Changed)?).as_bytes() != digest
            {
                return Err(E::Changed.into());
            }
            drop(current);
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
