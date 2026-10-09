//! Original funded transaction writes retained terminal facts; no close acknowledgement here.
use super::super::initial_settlement_backing::statement;
use super::*;
use crate::operation_journal::{
    owner::InputPermit,
    recording_commands::controls::{
        RecordingDeclaredFinalizingInventory, RecordingFinalizingInventory,
        RecordingFrozenInventory,
    },
};
use rusqlite::types::ValueRef;
use std::io::{self, Write};

impl OperationJournalStore {
    pub(in crate::operation_journal) fn fund_recording_terminal_encoding(
        proposal: &RecordingProposal,
        input: &InputPermit,
    ) -> Result<InputPermit, OperationJournalStoreError> {
        terminal_encoding_credit(proposal, input)
    }
    /// Only inside the original separately funded terminal snapshot held EXCLUSIVE.
    /// The owner retains its genuine private producer barrier, Source effect pin and current
    /// capacity through this mutation. Initial control identities/sequences never change.
    pub(in crate::operation_journal) fn record_finalizing_inside_original_transaction(
        &self,
        proposal: &RecordingProposal,
        inventory: &RecordingFinalizingInventory,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        self.record_finalizing_encoded_inside_original_transaction(
            proposal,
            || inventory.encode(proposal),
            input,
        )
    }

    pub(in crate::operation_journal) fn record_declared_finalizing_inside_original_transaction(
        &self,
        proposal: &RecordingProposal,
        inventory: &RecordingDeclaredFinalizingInventory,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        self.record_finalizing_encoded_inside_original_transaction(
            proposal,
            || inventory.encode(proposal),
            input,
        )
    }

    fn record_finalizing_encoded_inside_original_transaction(
        &self,
        proposal: &RecordingProposal,
        encode: impl FnOnce() -> Result<
            Vec<u8>,
            crate::operation_journal::recording_commands::RecordingValueError,
        >,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        if !self.recording_terminal_scope.get() || self.connection.is_autocommit() {
            return Err(error(
                "Finalizing requires original exclusive validated transaction",
            ));
        }
        let working = terminal_encoding_credit(proposal, input)?;
        let result = (|| {
            self.verify_recording_reservation_with_input(proposal, true, Some(input))?;
            let expected = RecordingControlReservation::new(proposal, ControlRole::Finalizing)
                .map_err(error)?;
            let empty = expected.encode(proposal).map_err(error)?;
            let encoded = encode().map_err(error)?;
            let mut operation = [0; 36];
            let operation = proposal
                .operation_id()
                .as_uuid()
                .hyphenated()
                .encode_lower(&mut operation);
            let mut id = [0; 36];
            let id = expected.id().hyphenated().encode_lower(&mut id);
            statement(&self.connection, input,
                c"UPDATE operation_journal_recording_chunks SET record=?1 WHERE operation_id=?2 AND chunk_id=?3 AND slot=20000 AND sequence>0 AND record=?4",
                |row| {
                    row.bind_blob(1, &encoded)?;
                    row.bind_text(2, operation)?;
                    row.bind_text(3, id)?;
                    row.bind_blob(4, &empty)?;
                    row.next().map(|_| ())
                })?;
            if self.connection.changes() != 1 {
                return Err(error("original Finalizing inventory CAS changed"));
            }
            #[cfg(test)]
            if let Some(super::super::InsertTestDirective::Fail(error)) = self
                .insert_test_directive(super::super::InsertTestBoundary::BeforePrecommitValidation)
            {
                return Err(error);
            }
            self.verify_finalizing_encoded_readback(proposal, &encoded, input)?;
            self.validate_namespace_with_input(Some(input), true)
        })();
        if result.is_err() {
            input.retain_historical_error(working, None, None, None);
        }
        result
    }

    pub(in crate::operation_journal) fn verify_finalizing_readback(
        &self,
        proposal: &RecordingProposal,
        inventory: &RecordingFinalizingInventory,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        let working = terminal_encoding_credit(proposal, input)?;
        let result = inventory
            .encode(proposal)
            .map_err(error)
            .and_then(|encoded| self.verify_finalizing_encoded_readback(proposal, &encoded, input));
        if result.is_err() {
            input.retain_historical_error(working, None, None, None);
        }
        result
    }

    pub(in crate::operation_journal) fn verify_declared_finalizing_readback(
        &self,
        proposal: &RecordingProposal,
        inventory: &RecordingDeclaredFinalizingInventory,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        let working = terminal_encoding_credit(proposal, input)?;
        let result = inventory
            .encode(proposal)
            .map_err(error)
            .and_then(|encoded| self.verify_finalizing_encoded_readback(proposal, &encoded, input));
        if result.is_err() {
            input.retain_historical_error(working, None, None, None);
        }
        result
    }

    fn verify_finalizing_encoded_readback(
        &self,
        proposal: &RecordingProposal,
        expected: &[u8],
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        let working = terminal_encoding_credit(proposal, input)?;
        let result = (|| {
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
                if !row.next()? { return Err(error("original Finalizing inventory absent")); }
                let ValueRef::Blob(bytes) = row.get_ref(0)? else { return Err(error("original Finalizing inventory shape changed")); };
                if bytes != expected { return Err(error("original Finalizing inventory readback changed")); }
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
    pub(in crate::operation_journal) fn begin_recording_terminal_write_with_input(
        &self,
        input: &InputPermit,
    ) -> Result<Option<InputPermit>, OperationJournalStoreError> {
        #[cfg(test)]
        if let Some(super::super::InsertTestDirective::Fail(error)) =
            self.insert_test_directive(super::super::InsertTestBoundary::BeforeBegin)
        {
            return Err(error);
        }
        let working = input.reserve_historical_scratch(32 * 1024)?;
        let result = (|| {
            self.configure_recording_metadata_policy_with_input(Some(input))?;
            super::super::initial_settlement_backing::control(
                &self.connection,
                Some(input),
                c"BEGIN EXCLUSIVE",
            )?;
            self.recording_terminal_scope.set(true);
            Ok(())
        })();
        match result {
            Ok(()) => Ok(Some(working)),
            Err(error) => {
                input.retain_historical_error(working, None, None, None);
                Err(error)
            }
        }
    }

    /// Borrow the original complete funded operation graph; never clone it into an uncharged
    /// owner request. Captured PCM and admitted chunk rows are audited by their existing readers.
    pub(in crate::operation_journal) fn validate_original_recording_terminal_context(
        &self,
        proposal: &RecordingProposal,
        input: &InputPermit,
    ) -> Result<super::super::admission_rebuild_backing::FundedSnapshot, OperationJournalStoreError>
    {
        if !self.recording_terminal_scope.get() || self.connection.is_autocommit() {
            return Err(error(
                "terminal context requires original exclusive snapshot",
            ));
        }
        let present = self.inspect_complete_claim_operation(proposal.operation_id(), input, |record, _| {
            if record.schema_version != super::super::super::model::SCHEMA_V17
                || record.phase != super::super::super::model::OperationPhase::IntentDurable
                || record.disposition != super::super::super::model::OperationDisposition::None
                || !matches!(record.universal_command_args(), Some(UniversalCommandArgs::Recording(current)) if current == proposal) {
                return Err(error("original recording terminal context changed or cancelled"));
            }
            Ok(())
        })?;
        if !present {
            return Err(error("original recording terminal operation absent"));
        }
        let mut collection = None;
        let snapshot =
            self.rebuild_ordinary_claim_snapshot_with_funding(Some(input), &mut collection)?;
        self.validate_recording_inventory_inside_transaction_with_input(Some(input))?;
        self.validate_database_claims_inside_transaction_with_input(Some(input))?;
        self.validate_database_attempts_inside_transaction_with_input(Some(input))?;
        super::super::chunk_claims::validate_original_relations(self, input)?;
        super::super::recovery_reserve::prove_no_active_charges_for_input(
            &self.connection,
            &self.database_path,
            input,
        )?;
        self.validate_namespace_with_input(Some(input), true)?;
        Ok(
            super::super::admission_rebuild_backing::FundedSnapshot::new(
                snapshot,
                collection.map(super::super::admission_rebuild_backing::Collection::into_credit),
            ),
        )
    }

    /// Read original exact declared bytes and issued range claims under one exclusive
    /// snapshot. Each original row keeper stays live through comparison; no row clone,
    /// physical completion, closure or publication authority escapes this audit.
    pub(in crate::operation_journal) fn validate_original_declared_terminal_retention(
        &self,
        proposal: &RecordingProposal,
        inventory: &RecordingDeclaredFinalizingInventory,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        if !self.recording_terminal_scope.get() || self.connection.is_autocommit() {
            return Err(error(
                "declared terminal retention needs original exclusive snapshot",
            ));
        }
        let working = terminal_encoding_credit(proposal, input)?;
        let result = (|| {
            self.verify_declared_finalizing_readback(proposal, inventory, input)?;
            inventory.inspect_chunk_ids(|id| {
                let chunk =
                    super::super::recording_chunks::funded::read(self, id, proposal, input)?
                        .ok_or_else(|| error("original declared terminal bytes are missing"))?;
                inventory
                    .validate_chunk(proposal, &chunk.1)
                    .map_err(error)?;
                let claim_id =
                    crate::operation_journal::store::chunk_claims::ChunkId::from_uuid(id)
                        .map_err(error)?;
                let claim =
                    super::super::chunk_claims::read_for_input(&self.connection, claim_id, input)?
                        .ok_or_else(|| {
                            error("original declared terminal range claim is missing")
                        })?;
                if !claim.matches_issued_recording_bytes(&chunk.1) {
                    return Err(error("original declared terminal issued range changed"));
                }
                Ok(())
            })
        })();
        if result.is_err() {
            input.retain_historical_error(working, None, None, None);
        }
        result
    }

    pub(in crate::operation_journal) fn original_frozen_control_sequence(
        &self,
        proposal: &RecordingProposal,
        input: &InputPermit,
    ) -> Result<u64, OperationJournalStoreError> {
        if !self.recording_terminal_scope.get() || self.connection.is_autocommit() {
            return Err(error("Frozen sequence needs original exclusive snapshot"));
        }
        let mut operation = [0; 36];
        let operation = proposal
            .operation_id()
            .as_uuid()
            .hyphenated()
            .encode_lower(&mut operation);
        statement(&self.connection, input,
            c"SELECT sequence FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=20001 AND typeof(sequence)='integer' AND sequence>0",
            |row| {
                row.bind_text(1, operation)?;
                if !row.next()? { return Err(error("original Frozen reservation sequence missing")); }
                let sequence = super::super::historical_claim_observation::borrowed_sql::scalar::<i64>(row, 0)?;
                u64::try_from(sequence).map_err(error)
            })
    }

    pub(in crate::operation_journal) fn record_original_frozen_inventory(
        &self,
        proposal: &RecordingProposal,
        finalizing: &RecordingDeclaredFinalizingInventory,
        frozen: &RecordingFrozenInventory,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        if !self.recording_terminal_scope.get() || self.connection.is_autocommit() {
            return Err(error("Frozen requires original exclusive transaction"));
        }
        let working = terminal_encoding_credit(proposal, input)?;
        let result = (|| {
            self.validate_original_declared_terminal_retention(proposal, finalizing, input)?;
            frozen
                .validate_finalizing(proposal, finalizing)
                .map_err(error)?;
            if frozen.prefix().freeze_sequence()
                != self.original_frozen_control_sequence(proposal, input)?
            {
                return Err(error(
                    "Frozen prefix sequence differs from original reservation",
                ));
            }
            let reservation =
                RecordingControlReservation::new(proposal, ControlRole::Frozen).map_err(error)?;
            let empty = reservation.encode(proposal).map_err(error)?;
            let encoded = frozen.encode(proposal).map_err(error)?;
            let mut operation = [0; 36];
            let operation = proposal
                .operation_id()
                .as_uuid()
                .hyphenated()
                .encode_lower(&mut operation);
            let mut id = [0; 36];
            let id = reservation.id().hyphenated().encode_lower(&mut id);
            statement(&self.connection, input,
                c"UPDATE operation_journal_recording_chunks SET record=?1 WHERE operation_id=?2 AND chunk_id=?3 AND slot=20001 AND typeof(sequence)='integer' AND sequence>0 AND record=?4",
                |row| {
                    row.bind_blob(1, &encoded)?; row.bind_text(2, operation)?;
                    row.bind_text(3, id)?; row.bind_blob(4, &empty)?;
                    row.next().map(|_| ())
                })?;
            if self.connection.changes() != 1 {
                return Err(error("original Frozen inventory CAS changed"));
            }
            #[cfg(test)]
            if let Some(super::super::InsertTestDirective::Fail(error)) = self
                .insert_test_directive(super::super::InsertTestBoundary::BeforePrecommitValidation)
            {
                return Err(error);
            }
            self.verify_original_frozen_readback(proposal, frozen, input)?;
            self.validate_namespace_with_input(Some(input), true)
        })();
        if result.is_err() {
            input.retain_historical_error(working, None, None, None);
        }
        result
    }

    pub(in crate::operation_journal) fn verify_original_frozen_readback(
        &self,
        proposal: &RecordingProposal,
        frozen: &RecordingFrozenInventory,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        let working = terminal_encoding_credit(proposal, input)?;
        let result = (|| {
            let expected = frozen.encode(proposal).map_err(error)?;
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
                    if !row.next()? { return Err(error("original Frozen inventory missing")); }
                    let ValueRef::Blob(bytes) = row.get_ref(0)? else { return Err(error("original Frozen inventory shape changed")); };
                    if bytes != expected { return Err(error("original Frozen inventory readback changed")); }
                    Ok(())
                })
        })();
        if result.is_err() {
            input.retain_historical_error(working, None, None, None);
        }
        result
    }

    pub(in crate::operation_journal) fn connection_is_autocommit_for_recording(&self) -> bool {
        self.connection.is_autocommit()
    }

    pub(in crate::operation_journal) fn finish_recording_terminal_write(
        &self,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        self.recording_terminal_scope.set(false);
        #[cfg(test)]
        match self.insert_test_directive(super::super::InsertTestBoundary::Commit) {
            Some(
                super::super::InsertTestDirective::Fail(error)
                | super::super::InsertTestDirective::FailAndLeaveForRollback(error),
            ) => return Err(error),
            Some(super::super::InsertTestDirective::ExecuteThenFail(error)) => {
                super::super::initial_settlement_backing::control(
                    &self.connection,
                    Some(input),
                    c"COMMIT",
                )?;
                return Err(error);
            }
            _ => {}
        }
        super::super::initial_settlement_backing::control(
            &self.connection,
            Some(input),
            c"COMMIT",
        )?;
        #[cfg(test)]
        if let Some(
            super::super::InsertTestDirective::Fail(error)
            | super::super::InsertTestDirective::ExecuteThenFail(error),
        ) = self.insert_test_directive(super::super::InsertTestBoundary::AfterCommit)
        {
            return Err(error);
        }
        self.validate_namespace_with_input(Some(input), false)
    }

    pub(in crate::operation_journal) fn rollback_recording_terminal_write(
        &self,
        input: &InputPermit,
    ) -> Result<(), OperationJournalStoreError> {
        self.recording_terminal_scope.set(false);
        #[cfg(test)]
        if let Some(super::super::InsertTestDirective::Fail(error)) =
            self.insert_test_directive(super::super::InsertTestBoundary::Rollback)
        {
            return Err(error);
        }
        super::super::initial_settlement_backing::control(
            &self.connection,
            Some(input),
            c"ROLLBACK",
        )?;
        self.validate_namespace_with_input(Some(input), false)
    }
}

/// Count the original borrowed proposal before owning any canonical encoding. Both original
/// encodings, geometric writer growth, bounded control data and error/SQL scratch are prepaid.
fn terminal_encoding_credit(
    proposal: &RecordingProposal,
    input: &InputPermit,
) -> Result<InputPermit, OperationJournalStoreError> {
    struct Count(usize);
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or(io::ErrorKind::OutOfMemory)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut fixed = input.reserve_historical_scratch(32 * 1024)?;
    let result = (|| {
        let mut count = Count(0);
        serde_json::to_writer(&mut count, proposal).map_err(error)?;
        let bytes = count
            .0
            .checked_mul(4)
            .and_then(|n| n.checked_add(4 * CONTROL_BYTES))
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        input.reserve_historical_scratch(bytes)
    })();
    match result {
        Ok(encoding) => {
            fixed.absorb_historical_scratch(encoding);
            Ok(fixed)
        }
        Err(error) => {
            input.retain_historical_error(fixed, None, None, None);
            Err(error)
        }
    }
}
