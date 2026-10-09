//! Read-only bounded Frozen audit after restart. Historical data supplies no live capability.
use super::verification::RecordingFrozenVerification;
use super::*;
use crate::operation_journal::{
    model::{OperationId, SCHEMA_V17},
    owner::InputPermit,
    recording_commands::{
        controls::{
            RecordingDeclaredFinalizingInventory, RecordingFrozenInventory,
            RecordingSealedInventory,
        },
        prefix::RecordingPrefixSeal,
    },
};

pub(in crate::operation_journal) struct RecordingRestartAudit {
    proposal: RecordingProposal,
    finalizing: RecordingDeclaredFinalizingInventory,
    frozen: RecordingFrozenInventory,
    pub(in crate::operation_journal) verifier: Option<RecordingFrozenVerification>,
    // Typed historical controls and proposal disappear before their original debit.
    _backing: InputPermit,
}
impl OperationJournalStore {
    pub(in crate::operation_journal) fn start_recording_restart_audit(
        &self,
        operation: OperationId,
        input: &InputPermit,
    ) -> Result<RecordingRestartAudit, OperationJournalStoreError> {
        let mut audit = None;
        let present = self.inspect_complete_claim_operation(operation, input, |record, _| {
            let Some(UniversalCommandArgs::Recording(proposal)) = record.universal_command_args()
            else {
                return Err(error("restart audit operation is not Recording"));
            };
            if record.schema_version != SCHEMA_V17 {
                return Err(error("restart audit Recording schema unsupported"));
            }
            let backing = Self::fund_recording_terminal_encoding(proposal, input)?;
            let result = (|| {
                self.verify_recording_reservation_with_input(proposal, true, Some(input))?;
                let finalizing =
                    self.read_restart_control(proposal, ControlRole::Finalizing, input, |bytes| {
                        RecordingDeclaredFinalizingInventory::decode(bytes, proposal).map_err(error)
                    })?;
                let frozen =
                    self.read_restart_control(proposal, ControlRole::Frozen, input, |bytes| {
                        RecordingFrozenInventory::decode(bytes, proposal).map_err(error)
                    })?;
                let verifier =
                    self.start_original_frozen_verification(proposal, &finalizing, &frozen, input)?;
                Ok((finalizing, frozen, verifier))
            })();
            match result {
                Ok((finalizing, frozen, verifier)) => {
                    audit = Some(RecordingRestartAudit {
                        proposal: proposal.clone(),
                        finalizing,
                        frozen,
                        verifier: Some(verifier),
                        _backing: backing,
                    });
                    Ok(())
                }
                Err(cause) => {
                    input.retain_historical_error(backing, None, None, None);
                    Err(cause)
                }
            }
        })?;
        if !present {
            return Err(error("restart audit Recording operation absent"));
        }
        audit.ok_or_else(|| error("restart audit input absent"))
    }
    /// Advance one bounded page; the old cursor survives any read/control/claim error.
    pub(in crate::operation_journal) fn recording_restart_audit_page(
        &self,
        audit: &RecordingRestartAudit,
        input: &InputPermit,
    ) -> Result<
        (
            Option<RecordingFrozenVerification>,
            Option<RecordingPrefixSeal>,
        ),
        OperationJournalStoreError,
    > {
        self.verify_recording_reservation_with_input(&audit.proposal, true, Some(input))?;
        self.verify_declared_finalizing_readback(&audit.proposal, &audit.finalizing, input)?;
        let current = audit
            .verifier
            .as_ref()
            .ok_or_else(|| error("restart audit already consumed"))?;
        let next =
            self.original_frozen_verification_page(&audit.proposal, &audit.frozen, current, input)?;
        if !next.complete()? {
            return Ok((Some(next), None));
        }
        let verified = next.finish(input)?;
        let seal = verified.observed_seal();
        // Historical Seal presence is never verification. Compare it to the fresh real-byte
        // result while the same immutable Frozen snapshot is held, or preserve the error.
        self.read_restart_control(&audit.proposal, ControlRole::Seal, input, |bytes| {
            if RecordingControlReservation::decode(bytes, &audit.proposal).is_ok() {
                return Ok(());
            }
            let stored = RecordingSealedInventory::decode(bytes, &audit.proposal).map_err(error)?;
            stored
                .validate_observed_replay(&audit.proposal, &audit.frozen, seal)
                .map_err(error)
        })?;
        let observed = seal.clone(); // Fixed-size bounded data; never a live verification capability.
        Ok((None, Some(observed)))
    }
    fn read_restart_control<T>(
        &self,
        proposal: &RecordingProposal,
        role: ControlRole,
        input: &InputPermit,
        inspect: impl FnOnce(&[u8]) -> Result<T, OperationJournalStoreError>,
    ) -> Result<T, OperationJournalStoreError> {
        let working = Self::fund_recording_terminal_encoding(proposal, input)?;
        let result = (|| {
            let mut operation = [0; 36];
            let operation = proposal
                .operation_id()
                .as_uuid()
                .hyphenated()
                .encode_lower(&mut operation);
            super::super::initial_settlement_backing::statement(&self.connection, input,
                c"SELECT CASE WHEN typeof(record)='blob' AND length(record) BETWEEN 1 AND 4096 THEN record ELSE NULL END FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=?2",
                |row| {
                    row.bind_text(1, operation)?; row.bind_integer(2, role.slot())?;
                    if !row.next()? { return Err(error("restart audit control missing")); }
                    let rusqlite::types::ValueRef::Blob(bytes) = row.get_ref(0)? else { return Err(error("restart audit control shape changed")); };
                    inspect(bytes)
                })
        })();
        if result.is_err() {
            input.retain_historical_error(working, None, None, None);
        }
        result
    }
}
