//! Funded bounded retained-byte verification. No Source or physical capability is issued here.
use super::*;
use crate::operation_journal::{
    owner::InputPermit,
    recording_commands::{
        RecordingChunkRole,
        controls::{RecordingDeclaredFinalizingInventory, RecordingFrozenInventory},
        prefix::{MAX_PREFIX_PAGE_CHUNKS, RecordingPrefixSeal, RecordingPrefixVerifier},
    },
};

/// The original working credit follows the verifier through every yielded page.
/// No Clone or extraction can leave an owned proposal/verifier without its debit.
pub(in crate::operation_journal) struct RecordingFrozenVerification {
    verifier: RecordingPrefixVerifier,
    frozen_hash: [u8; 32],
    _backing: InputPermit,
}
impl RecordingFrozenVerification {
    pub(in crate::operation_journal) fn complete(
        &self,
    ) -> Result<bool, OperationJournalStoreError> {
        Ok(self.verifier.next_retained_role().map_err(error)?.is_none())
    }
    pub(in crate::operation_journal) fn finish(
        self,
        input: &InputPermit,
    ) -> Result<VerifiedRecordingSeal, OperationJournalStoreError> {
        let Self {
            verifier,
            frozen_hash,
            _backing,
        } = self;
        match verifier.finish() {
            Ok(seal) => Ok(VerifiedRecordingSeal {
                seal,
                frozen_hash,
                _backing,
            }),
            Err(cause) => {
                let result = error(cause);
                // Keep failure formatting/codec backing until the caller drops its input.
                input.retain_historical_error(_backing, None, None, None);
                Err(result)
            }
        }
    }
}

pub(in crate::operation_journal) struct VerifiedRecordingSeal {
    seal: RecordingPrefixSeal,
    frozen_hash: [u8; 32],
    _backing: InputPermit,
}

impl VerifiedRecordingSeal {
    pub(super) fn observed_seal(&self) -> &RecordingPrefixSeal {
        &self.seal
    }
}

impl OperationJournalStore {
    pub(in crate::operation_journal) fn start_original_frozen_verification(
        &self,
        proposal: &RecordingProposal,
        finalizing: &RecordingDeclaredFinalizingInventory,
        frozen: &RecordingFrozenInventory,
        input: &InputPermit,
    ) -> Result<RecordingFrozenVerification, OperationJournalStoreError> {
        self.require_original_verification_snapshot()?;
        let backing = Self::fund_recording_terminal_encoding(proposal, input)?;
        let result = (|| {
            frozen
                .validate_finalizing(proposal, finalizing)
                .map_err(error)?;
            self.verify_original_frozen_readback(proposal, frozen, input)?;
            self.verify_original_frozen_row_count(proposal, frozen, input)?;
            self.verify_recording_reservation_with_input(proposal, true, Some(input))?;
            let verifier =
                RecordingPrefixVerifier::new(proposal, frozen.prefix().clone()).map_err(error)?;
            let frozen_hash = *blake3::hash(&frozen.encode(proposal).map_err(error)?).as_bytes();
            Ok((verifier, frozen_hash))
        })();
        match result {
            Ok((verifier, frozen_hash)) => Ok(RecordingFrozenVerification {
                verifier,
                frozen_hash,
                _backing: backing,
            }),
            Err(cause) => {
                input.retain_historical_error(backing, None, None, None);
                Err(cause)
            }
        }
    }

    /// Return a separately funded next state. The caller retains the old state until its
    /// original Source/context transaction has succeeded; errors cannot advance its cursor.
    pub(in crate::operation_journal) fn original_frozen_verification_page(
        &self,
        proposal: &RecordingProposal,
        frozen: &RecordingFrozenInventory,
        current: &RecordingFrozenVerification,
        input: &InputPermit,
    ) -> Result<RecordingFrozenVerification, OperationJournalStoreError> {
        self.require_original_verification_snapshot()?;
        let backing = Self::fund_recording_terminal_encoding(proposal, input)?;
        let mut next = None;
        let result = (|| {
            self.verify_original_frozen_readback(proposal, frozen, input)?;
            self.verify_original_frozen_row_count(proposal, frozen, input)?;
            let hash = *blake3::hash(&frozen.encode(proposal).map_err(error)?).as_bytes();
            if hash != current.frozen_hash {
                return Err(error("Frozen verification inventory changed"));
            }
            next = Some(current.verifier.clone());
            let verifier = next.as_mut().expect("funded verifier initialized");
            for _ in 0..MAX_PREFIX_PAGE_CHUNKS {
                let Some(role) = verifier.next_retained_role().map_err(error)? else {
                    break;
                };
                let id = self.original_verification_chunk_id(proposal, role, input)?;
                let chunk =
                    super::super::recording_chunks::funded::read(self, id, proposal, input)?
                        .ok_or_else(|| error("Frozen verification row vanished"))?;
                let claim_id = super::super::chunk_claims::ChunkId::from_uuid(id).map_err(error)?;
                let claim =
                    super::super::chunk_claims::read_for_input(&self.connection, claim_id, input)?
                        .ok_or_else(|| {
                            error("Frozen verification row lacks original issued claim")
                        })?;
                if !claim.matches_issued_recording_bytes(&chunk.1) {
                    return Err(error("Frozen verification issued range changed"));
                }
                let cursor = verifier.cursor();
                let page = std::slice::from_ref(&chunk.1);
                match role {
                    RecordingChunkRole::Header { .. } => verifier.header_page(cursor, page),
                    RecordingChunkRole::Pcm { .. } => verifier.pcm_page(cursor, page),
                }
                .map_err(error)?;
                // Both keepers drop before the next row is decoded.
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(RecordingFrozenVerification {
                verifier: next.expect("successful funded page"),
                frozen_hash: current.frozen_hash,
                _backing: backing,
            }),
            Err(cause) => {
                drop(next);
                input.retain_historical_error(backing, None, None, None);
                Err(cause)
            }
        }
    }

    fn verify_original_frozen_row_count(
        &self,
        proposal: &RecordingProposal,
        frozen: &RecordingFrozenInventory,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        let expected = frozen
            .prefix()
            .retained_row_count(proposal)
            .map_err(error)?;
        let mut operation = [0; 36];
        let operation = proposal
            .operation_id()
            .as_uuid()
            .hyphenated()
            .encode_lower(&mut operation);
        super::super::initial_settlement_backing::statement(&self.connection, input,
            c"SELECT COUNT(*) FROM (SELECT slot FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot NOT BETWEEN 20000 AND 20003 LIMIT 8193)",
            |row| {
                row.bind_text(1, operation)?;
                if !row.next()? { return Err(error("Frozen row count missing")); }
                let count = super::super::historical_claim_observation::borrowed_sql::scalar::<i64>(row, 0)?;
                if u64::try_from(count).map_err(error)? != expected {
                    return Err(error("Frozen byte rows differ from declared complete inventory"));
                }
                Ok(())
            })
    }

    fn require_original_verification_snapshot(&self) -> Result<(), OperationJournalStoreError> {
        if !self.recording_terminal_scope.get() || self.connection.is_autocommit() {
            return Err(error(
                "Frozen verification requires original exclusive snapshot",
            ));
        }
        Ok(())
    }
    fn original_verification_chunk_id(
        &self,
        proposal: &RecordingProposal,
        role: RecordingChunkRole,
        input: &InputPermit,
    ) -> Result<uuid::Uuid, OperationJournalStoreError> {
        let slot = match role {
            RecordingChunkRole::Header { revision } => 10000 + i64::from(revision),
            RecordingChunkRole::Pcm { ordinal } => i64::from(ordinal),
        };
        let mut operation = [0; 36];
        let operation = proposal
            .operation_id()
            .as_uuid()
            .hyphenated()
            .encode_lower(&mut operation);
        super::super::initial_settlement_backing::statement(&self.connection, input,
            c"SELECT CASE WHEN typeof(chunk_id)='text' AND length(chunk_id)=36 THEN chunk_id ELSE NULL END FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=?2",
            |row| {
                row.bind_text(1, operation)?;
                row.bind_integer(2, slot)?;
                if !row.next()? { return Err(error("Frozen verification coverage gap")); }
                let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(0)? else { return Err(error("Frozen verification key shape changed")); };
                let text = std::str::from_utf8(bytes).map_err(error)?;
                let id = uuid::Uuid::parse_str(text).map_err(error)?;
                let mut canonical = [0; 36];
                if id.is_nil() || id.hyphenated().encode_lower(&mut canonical) != text {
                    return Err(error("Frozen verification key is not canonical"));
                }
                Ok(id)
            })
    }
}

impl OperationJournalStore {
    /// Only the original incremental verifier can construct this process-local input.
    /// Persisted seal data cannot be submitted as a successful verification result.
    pub(in crate::operation_journal) fn retain_original_verified_seal(
        &self,
        proposal: &RecordingProposal,
        frozen: &RecordingFrozenInventory,
        verified: &VerifiedRecordingSeal,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        use crate::operation_journal::recording_commands::controls::RecordingSealedInventory;
        self.require_original_verification_snapshot()?;
        let working = Self::fund_recording_terminal_encoding(proposal, input)?;
        let result = (|| {
            self.verify_original_frozen_readback(proposal, frozen, input)?;
            self.verify_original_frozen_row_count(proposal, frozen, input)?;
            if verified.frozen_hash
                != *blake3::hash(&frozen.encode(proposal).map_err(error)?).as_bytes()
            {
                return Err(error(
                    "original verification result changed Frozen inventory",
                ));
            }
            let sealed = RecordingSealedInventory::capture(proposal, frozen, &verified.seal)
                .map_err(error)?;
            let encoded = sealed.encode(proposal).map_err(error)?;
            let reservation =
                RecordingControlReservation::new(proposal, ControlRole::Seal).map_err(error)?;
            let empty = reservation.encode(proposal).map_err(error)?;
            if self.original_seal_encoding_retained(proposal, &encoded, &empty, input)? {
                // Exact replay observes the original row without an UPDATE or sequence allocation.
                self.verify_recording_reservation_with_input(proposal, true, Some(input))?;
                return self.validate_namespace_with_input(Some(input), true);
            }
            let mut operation = [0; 36];
            let operation = proposal
                .operation_id()
                .as_uuid()
                .hyphenated()
                .encode_lower(&mut operation);
            let mut id = [0; 36];
            let id = reservation.id().hyphenated().encode_lower(&mut id);
            super::super::initial_settlement_backing::statement(&self.connection, input,
                c"UPDATE operation_journal_recording_chunks SET record=?1 WHERE operation_id=?2 AND chunk_id=?3 AND slot=20002 AND typeof(sequence)='integer' AND sequence>0 AND record=?4",
                |row| { row.bind_blob(1, &encoded)?; row.bind_text(2, operation)?; row.bind_text(3, id)?; row.bind_blob(4, &empty)?; row.next().map(|_| ()) })?;
            if self.connection.changes() != 1 {
                return Err(error("original Seal reservation CAS changed"));
            }
            #[cfg(test)]
            if let Some(super::super::InsertTestDirective::Fail(cause)) = self
                .insert_test_directive(super::super::InsertTestBoundary::BeforePrecommitValidation)
            {
                return Err(cause);
            }
            self.verify_original_seal_readback(proposal, frozen, verified, input)?;
            self.verify_recording_reservation_with_input(proposal, true, Some(input))?;
            self.validate_namespace_with_input(Some(input), true)
        })();
        if result.is_err() {
            input.retain_historical_error(working, None, None, None);
        }
        result
    }
    pub(in crate::operation_journal) fn verify_original_seal_readback(
        &self,
        proposal: &RecordingProposal,
        frozen: &RecordingFrozenInventory,
        verified: &VerifiedRecordingSeal,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        use crate::operation_journal::recording_commands::controls::RecordingSealedInventory;
        let working = Self::fund_recording_terminal_encoding(proposal, input)?;
        let result = (|| {
            let expected = RecordingSealedInventory::capture(proposal, frozen, &verified.seal)
                .map_err(error)?
                .encode(proposal)
                .map_err(error)?;
            let mut operation = [0; 36];
            let operation = proposal
                .operation_id()
                .as_uuid()
                .hyphenated()
                .encode_lower(&mut operation);
            super::super::initial_settlement_backing::statement(&self.connection, input,
                c"SELECT CASE WHEN typeof(record)='blob' AND length(record) BETWEEN 1 AND 4096 THEN record ELSE NULL END FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=20002",
                |row| {
                    row.bind_text(1, operation)?;
                    if !row.next()? { return Err(error("original Seal absent")); }
                    let rusqlite::types::ValueRef::Blob(bytes) = row.get_ref(0)? else { return Err(error("original Seal row shape changed")); };
                    if bytes != expected { return Err(error("original Seal exact readback changed")); }
                    Ok(())
                })
        })();
        if result.is_err() {
            input.retain_historical_error(working, None, None, None);
        }
        result
    }
}

impl OperationJournalStore {
    fn original_seal_encoding_retained(
        &self,
        proposal: &RecordingProposal,
        encoded: &[u8],
        empty: &[u8],
        input: &InputPermit,
    ) -> Result<bool, OperationJournalStoreError> {
        let mut operation = [0; 36];
        let operation = proposal
            .operation_id()
            .as_uuid()
            .hyphenated()
            .encode_lower(&mut operation);
        super::super::initial_settlement_backing::statement(&self.connection, input,
            c"SELECT CASE WHEN typeof(record)='blob' AND length(record) BETWEEN 1 AND 4096 THEN record ELSE NULL END FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=20002",
            |row| {
                row.bind_text(1, operation)?;
                if !row.next()? { return Err(error("original Seal reservation absent")); }
                let rusqlite::types::ValueRef::Blob(bytes) = row.get_ref(0)? else { return Err(error("original Seal reservation shape changed")); };
                if bytes == encoded { Ok(true) }
                else if bytes == empty { Ok(false) }
                else { Err(error("conflicting immutable Seal replay")) }
            })
    }
}
