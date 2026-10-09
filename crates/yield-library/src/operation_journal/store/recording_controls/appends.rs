//! Original declared terminal byte retention; the generic capture route remains fenced.
use super::super::{
    initial_settlement_backing::{next_sequence, statement},
    recording_chunks::RecordingChunkReceipt,
};
use super::*;
use crate::operation_journal::{
    owner::InputPermit,
    recording_commands::{
        RecordingCapturedChunk, RecordingChunkRole, controls::RecordingDeclaredFinalizingInventory,
    },
};

impl OperationJournalStore {
    pub(in crate::operation_journal) fn original_terminal_chunk_is_issued(
        &self,
        chunk: &RecordingCapturedChunk,
        input: &InputPermit,
    ) -> Result<bool, OperationJournalStoreError> {
        if !self.recording_terminal_scope.get() || self.connection.is_autocommit() {
            return Err(error(
                "terminal issued-range observation requires original snapshot",
            ));
        }
        let id = super::super::chunk_claims::ChunkId::from_uuid(chunk.chunk_id()).map_err(error)?;
        let claim = super::super::chunk_claims::read_for_input(&self.connection, id, input)?;
        Ok(claim.is_some_and(|claim| claim.matches_issued_recording_bytes(chunk)))
    }

    pub(in crate::operation_journal) fn retain_original_declared_terminal_chunk(
        &self,
        proposal: &RecordingProposal,
        inventory: &RecordingDeclaredFinalizingInventory,
        chunk: &RecordingCapturedChunk,
        input: &InputPermit,
    ) -> Result<RecordingChunkReceipt, OperationJournalStoreError> {
        if !self.recording_terminal_scope.get() || self.connection.is_autocommit() {
            return Err(error(
                "terminal bytes require original exclusive transaction",
            ));
        }
        let mut working = Self::fund_recording_terminal_encoding(proposal, input)?;
        let native = std::cell::RefCell::new(None);
        let result = (|| {
            inventory.validate_chunk(proposal, chunk).map_err(error)?;
            self.verify_declared_finalizing_readback(proposal, inventory, input)?;
            self.verify_recording_reservation_with_input(proposal, true, Some(input))?;
            // An original declaration is not admission. Require the already issued exact
            // range under the owner's independently revalidated provider/Source context.
            let id =
                super::super::chunk_claims::ChunkId::from_uuid(chunk.chunk_id()).map_err(error)?;
            let claim = super::super::chunk_claims::read_for_input(&self.connection, id, input)?
                .ok_or_else(|| error("declared terminal bytes lack original issued claim"))?;
            if !claim.matches_issued_recording_bytes(chunk) {
                return Err(error("declared terminal issued range changed"));
            }
            if let Some(existing) = super::super::recording_chunks::funded::read(
                self,
                chunk.chunk_id(),
                proposal,
                input,
            )? {
                existing.1.validate_replay(chunk, proposal).map_err(error)?;
                return Ok(RecordingChunkReceipt::Replay {
                    sequence: existing.0,
                });
            }
            // A Frozen observation forbids every new identity, including a declared one.
            self.require_original_empty_frozen(proposal, input)?;
            let length = super::super::database_claims::command_backing::serialized_length(chunk)?;
            let bytes = length
                .checked_mul(2)
                .and_then(|n| n.checked_add(4096))
                .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
            working.absorb_historical_scratch(input.reserve_historical_scratch(bytes)?);
            let encoded = chunk.encode(proposal).map_err(error)?;
            let (rows, bytes) =
                super::super::recording_chunks::funded::totals(self, input, &native)?;
            if rows >= 8192 || bytes.checked_add(encoded.len() as i64)
                .is_none_or(|n| n > crate::operation_journal::recording_commands::MAX_RECORDING_RETAINED_ENCODED_BYTES as i64) {
                return Err(error("declared terminal retention exhausted; original bytes remain"));
            }
            self.validate_original_terminal_append_order(proposal, inventory, chunk, input)?;
            let sequence = next_sequence(self, input)?;
            let slot = terminal_slot(chunk.role());
            let mut operation = [0; 36];
            let operation = proposal
                .operation_id()
                .as_uuid()
                .hyphenated()
                .encode_lower(&mut operation);
            let mut id = [0; 36];
            let id = chunk.chunk_id().hyphenated().encode_lower(&mut id);
            statement(&self.connection, input,
                c"INSERT INTO operation_journal_recording_chunks(chunk_id,operation_id,slot,sequence,record) VALUES(?1,?2,?3,?4,?5)",
                |row| { row.bind_text(1, id)?; row.bind_text(2, operation)?; row.bind_integer(3, slot)?; row.bind_integer(4, sequence)?; row.bind_blob(5, &encoded)?; row.next().map(|_| ()) })?;
            #[cfg(test)]
            if let Some(super::super::InsertTestDirective::Fail(cause)) = self
                .insert_test_directive(super::super::InsertTestBoundary::BeforePrecommitValidation)
            {
                return Err(cause);
            }
            let retained = super::super::recording_chunks::funded::read(
                self,
                chunk.chunk_id(),
                proposal,
                input,
            )?
            .ok_or_else(|| error("original terminal bytes absent at readback"))?;
            if retained.0 != sequence {
                return Err(error("original terminal byte sequence changed"));
            }
            retained.1.validate_replay(chunk, proposal).map_err(error)?;
            self.validate_namespace_with_input(Some(input), true)?;
            Ok(RecordingChunkReceipt::Committed { sequence })
        })();
        if result.is_err() {
            input.retain_historical_error(working, None, None, native.into_inner());
        }
        result
    }
    fn require_original_empty_frozen(
        &self,
        proposal: &RecordingProposal,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        let expected = RecordingControlReservation::new(proposal, ControlRole::Frozen)
            .map_err(error)?
            .encode(proposal)
            .map_err(error)?;
        let mut operation = [0; 36];
        let operation = proposal
            .operation_id()
            .as_uuid()
            .hyphenated()
            .encode_lower(&mut operation);
        statement(&self.connection, input,
            c"SELECT CASE WHEN typeof(record)='blob' AND length(record) BETWEEN 1 AND 4096 THEN record ELSE NULL END FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=20001",
            |row| {
                row.bind_text(1, operation)?;
                if !row.next()? { return Err(error("original Frozen reservation absent")); }
                let rusqlite::types::ValueRef::Blob(bytes) = row.get_ref(0)? else { return Err(error("original Frozen reservation shape changed")); };
                if bytes != expected { return Err(error("Frozen forbids new terminal identities")); }
                Ok(())
            })
    }
    fn validate_original_terminal_append_order(
        &self,
        proposal: &RecordingProposal,
        inventory: &RecordingDeclaredFinalizingInventory,
        chunk: &RecordingCapturedChunk,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        if matches!(chunk.role(), RecordingChunkRole::Header { revision: 1 }) {
            inventory.inspect_pcm_chunk_ids(|id| {
                let retained =
                    super::super::recording_chunks::funded::read(self, id, proposal, input)?
                        .ok_or_else(|| error("final header precedes declared terminal PCM"))?;
                inventory
                    .validate_chunk(proposal, &retained.1)
                    .map_err(error)
            })?;
        }
        let previous = match chunk.role() {
            RecordingChunkRole::Pcm { ordinal: 0 } => 10000,
            RecordingChunkRole::Pcm { ordinal } => i64::from(ordinal - 1),
            RecordingChunkRole::Header { revision } if revision > 0 => {
                10000 + i64::from(revision - 1)
            }
            _ => return Err(error("terminal declaration cannot supply initialization")),
        };
        let mut operation = [0; 36];
        let operation = proposal
            .operation_id()
            .as_uuid()
            .hyphenated()
            .encode_lower(&mut operation);
        let id = statement(&self.connection, input,
            c"SELECT CASE WHEN typeof(chunk_id)='text' AND length(chunk_id)=36 THEN chunk_id ELSE NULL END FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=?2",
            |row| {
                row.bind_text(1, operation)?; row.bind_integer(2, previous)?;
                if !row.next()? { return Err(error("original terminal append prefix gap")); }
                let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(0)? else { return Err(error("original terminal predecessor key shape changed")); };
                uuid::Uuid::parse_str(std::str::from_utf8(bytes).map_err(error)?).map_err(error)
            })?;
        let prior = super::super::recording_chunks::funded::read(self, id, proposal, input)?
            .ok_or_else(|| error("original terminal predecessor vanished"))?;
        if matches!(chunk.role(), RecordingChunkRole::Header { revision: 2 }) {
            inventory
                .validate_chunk(proposal, &prior.1)
                .map_err(error)?;
        }
        if matches!(chunk.role(), RecordingChunkRole::Pcm { ordinal } if ordinal > 0)
            && prior.1.bytes().len() as u64 != proposal.format().pcm_chunk_bytes()
        {
            return Err(error("terminal PCM cannot follow a short slot"));
        }
        if matches!(chunk.role(), RecordingChunkRole::Pcm { .. }) {
            statement(&self.connection, input,
                c"SELECT EXISTS(SELECT 1 FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot BETWEEN 10001 AND 10015)",
                |row| { row.bind_text(1, operation)?; row.next()?;
                    if super::super::historical_claim_observation::borrowed_sql::scalar::<i64>(row, 0)? != 0 { return Err(error("terminal PCM cannot follow final header patches")); }
                    Ok(()) })?;
        }
        Ok(())
    }
}
fn terminal_slot(role: RecordingChunkRole) -> i64 {
    match role {
        RecordingChunkRole::Pcm { ordinal } => i64::from(ordinal),
        RecordingChunkRole::Header { revision } => 10000 + i64::from(revision),
    }
}

impl OperationJournalStore {
    /// Generic chunk claim/issuance cannot become a terminal capability. Original terminal
    /// byte retention above consumes only claims already issued for the exact private plan.
    pub(in crate::operation_journal) fn require_recording_claim_capturing_for_input(
        &self,
        proposal: &RecordingProposal,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        let working = Self::fund_recording_terminal_encoding(proposal, input)?;
        let result = (|| {
            self.verify_recording_reservation_with_input(proposal, true, Some(input))?;
            let empty = RecordingControlReservation::new(proposal, ControlRole::Finalizing)
                .map_err(error)?
                .encode(proposal)
                .map_err(error)?;
            let mut operation = [0; 36];
            let operation = proposal
                .operation_id()
                .as_uuid()
                .hyphenated()
                .encode_lower(&mut operation);
            statement(&self.connection, input,
                c"SELECT CASE WHEN typeof(record)='blob' AND length(record) BETWEEN 1 AND 4096 THEN record ELSE NULL END FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=20000",
                |row| {
                    row.bind_text(1, operation)?;
                    if !row.next()? { return Err(error("Recording generic claim reservation absent")); }
                    let rusqlite::types::ValueRef::Blob(bytes) = row.get_ref(0)? else { return Err(error("Recording generic claim control shape changed")); };
                    if bytes != empty { return Err(error("Recording terminal custody fences generic chunk claim/issuance")); }
                    Ok(())
                })
        })();
        if result.is_err() {
            input.retain_historical_error(working, None, None, None);
        }
        result
    }
}
