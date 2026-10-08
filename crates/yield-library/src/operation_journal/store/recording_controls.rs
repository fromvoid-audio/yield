//! Same-transaction reserved recording metadata. No closure, grant or retirement authority.
use super::{OperationJournalStore, OperationJournalStoreError};
mod funded;
mod initial_funded;
use crate::operation_journal::coordinator::UniversalCommandArgs;
use crate::operation_journal::recording_commands::RecordingProposal;
use crate::operation_journal::recording_commands::controls::{
    CONTROL_BYTES, CONTROL_ROWS, ControlRole, FIRST_CONTROL_SLOT, RESERVED_BYTES,
    RecordingControlReservation, reservation_metadata_bound,
};
use rusqlite::{OptionalExtension, params};

fn error(reason: impl ToString) -> OperationJournalStoreError {
    OperationJournalStoreError::RecordingRetention {
        reason: reason.to_string(),
    }
}

impl OperationJournalStore {
    pub(super) fn prepare_recording_initial_policy(
        &self,
        record: &super::StoredOperationRecord,
    ) -> Result<(), OperationJournalStoreError> {
        self.prepare_recording_initial_policy_with_input(record, None)
    }
    pub(super) fn prepare_recording_initial_policy_with_input(
        &self,
        record: &super::StoredOperationRecord,
        input: Option<&crate::operation_journal::owner::InputPermit>,
    ) -> Result<(), OperationJournalStoreError> {
        if let Some(UniversalCommandArgs::Recording(proposal)) = record.universal_command_args() {
            reservation_metadata_bound(proposal).map_err(error)?;
            self.configure_recording_metadata_policy_with_input(input)?;
        }
        Ok(())
    }

    /// Charge the full reservation, not the current small JSON payload. Unknown SQL row shapes
    /// or slot ranges stay retained and reject the next write; the scan is capped at8193 rows.
    pub(super) fn recording_retained_totals(
        &self,
    ) -> Result<(i64, i64), OperationJournalStoreError> {
        let (rows, bytes, valid): (i64, i64, i64) = self.connection.query_row(
            "SELECT COUNT(*),COALESCE(SUM(CASE WHEN slot BETWEEN 20000 AND 20003 THEN 4096 ELSE length(record) END),0),
             COALESCE(MIN(CASE WHEN typeof(chunk_id)='text' AND length(chunk_id)=36
              AND typeof(operation_id)='text' AND length(operation_id)=36
              AND typeof(sequence)='integer' AND sequence>0 AND typeof(slot)='integer'
              AND (slot BETWEEN 0 AND 8175 OR slot BETWEEN 10000 AND 10015 OR slot BETWEEN 20000 AND 20003)
              AND typeof(record)='blob' AND length(record) BETWEEN 1 AND 131072
              AND (slot NOT BETWEEN 20000 AND 20003 OR length(record)<=4096)
              THEN 1 ELSE 0 END),1)
             FROM (SELECT chunk_id,operation_id,slot,sequence,record FROM operation_journal_recording_chunks LIMIT 8193)",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        if valid != 1
            || rows > 8192
            || bytes > super::super::recording_commands::MAX_RECORDING_RETAINED_ENCODED_BYTES as i64
        {
            return Err(error(
                "unsupported/corrupt or over-limit retained recording inventory",
            ));
        }
        Ok((rows, bytes))
    }

    /// Four exact rows are required for new capture. Zero rows is permitted only for observing
    /// historical bytes; partial/unknown/future controls never become an empty reservation.
    pub(super) fn verify_recording_reservation_with_input(
        &self,
        proposal: &RecordingProposal,
        required: bool,
        input: Option<&crate::operation_journal::owner::InputPermit>,
    ) -> Result<bool, OperationJournalStoreError> {
        match input {
            Some(input) => funded::verify(self, proposal, required, input),
            None => self.verify_recording_reservation(proposal, required),
        }
    }

    pub(super) fn verify_recording_reservation(
        &self,
        proposal: &RecordingProposal,
        required: bool,
    ) -> Result<bool, OperationJournalStoreError> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM (SELECT slot FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot>=?2 LIMIT 5)",
            params![proposal.operation_id().to_string(), FIRST_CONTROL_SLOT], |r|r.get(0))?;
        if count == 0 && !required {
            return Ok(false);
        }
        if count != CONTROL_ROWS {
            return Err(error(
                "recording has no complete reserved terminal inventory",
            ));
        }
        let mut previous_sequence: Option<i64> = None;
        for role in ControlRole::ALL {
            let expected = RecordingControlReservation::new(proposal, role).map_err(error)?;
            let row: Option<(String, i64, String, i64)> = self.connection.query_row(
                "SELECT CASE WHEN typeof(chunk_id)='text' AND length(chunk_id)=36 THEN chunk_id ELSE NULL END,sequence,typeof(record),length(record)
                 FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=?2",
                params![proposal.operation_id().to_string(), role.slot()], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
            let Some((id, sequence, kind, length)) = row else {
                return Err(error("reserved recording control absent"));
            };
            if id != expected.id().to_string()
                || sequence <= 0
                || kind != "blob"
                || !(1..=CONTROL_BYTES as i64).contains(&length)
            {
                return Err(error("unsupported recording control row shape/identity"));
            }
            if previous_sequence.is_some_and(|previous| previous.checked_add(1) != Some(sequence)) {
                return Err(error(
                    "reserved recording controls have conflicting sequence order",
                ));
            }
            previous_sequence = Some(sequence);
            #[cfg(test)]
            super::recording_chunks::payload_read_boundary(
                super::recording_chunks::PayloadReader::Control,
                &self.connection,
            );
            // Guard the actual payload projection, even when it differs from the preceding
            // scalar observation. Zero-length BLOBs retain the original decoder error.
            let (bytes, payload_kind): (Option<Vec<u8>>, String) = self.connection.query_row(
                "SELECT CASE WHEN typeof(record)='blob' AND length(record) BETWEEN 0 AND ?3 THEN record ELSE NULL END AS record, typeof(record) FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=?2",
                params![proposal.operation_id().to_string(), role.slot(), CONTROL_BYTES as i64],
                |r| Ok((r.get(0)?, r.get(1)?)))?;
            if payload_kind != "blob" {
                return Err(super::recording_chunks::recording_payload_type_error(
                    &payload_kind,
                ));
            }
            let bytes = bytes.ok_or_else(|| {
                if self.recording_prestart_scope.get() {
                    OperationJournalStoreError::RecordingPrestartResourceBound
                } else {
                    error("invalid recording observation: recording control exceeds reserved bound")
                }
            })?;
            #[cfg(test)]
            super::recording_chunks::PAYLOAD_COPIED_BYTES.with(|n| n.set(n.get() + bytes.len()));
            if self.recording_prestart_scope.get() {
                // Complete preflight bounds this BLOB inside the same read snapshot. The
                // narrow factory supports only genuine canonical empty reservations: never
                // build an arbitrary JSON graph from even a small hostile control value.
                if bytes != expected.encode(proposal).map_err(error)? {
                    return Err(OperationJournalStoreError::RecordingPrestartResourceBound);
                }
            } else {
                // General historical readers preserve their typed/noncanonical acceptance.
                let observed =
                    RecordingControlReservation::decode(&bytes, proposal).map_err(error)?;
                if observed != expected {
                    return Err(error("conflicting reserved recording control"));
                }
            }
        }
        // Use the operation/slot index and a capped inventory, not an ordered global sequence
        // scan whose work could grow without bound on a corrupt reopened database.
        let (capture_rows, first_capture): (i64, Option<i64>) = self.connection.query_row(
            "SELECT COUNT(*), MIN(sequence) FROM (
             SELECT sequence FROM operation_journal_recording_chunks WHERE operation_id=?1
             AND slot NOT BETWEEN 20000 AND 20003 LIMIT 8193)",
            params![proposal.operation_id().to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if capture_rows > 8192 - CONTROL_ROWS {
            return Err(error(
                "retained recording capture exceeds its reserved inventory bound",
            ));
        }
        if first_capture.is_some_and(|sequence| sequence <= previous_sequence.unwrap()) {
            return Err(error("recording capture precedes its terminal reservation"));
        }
        Ok(true)
    }

    pub(super) fn reserve_recording_initial_inside_transaction_with_input(
        &self,
        record: &super::StoredOperationRecord,
        encoded: &[u8],
        input: Option<&crate::operation_journal::owner::InputPermit>,
    ) -> Result<(), OperationJournalStoreError> {
        match input {
            Some(input) => initial_funded::reserve(self, record, encoded, input),
            None => self.reserve_recording_initial_inside_transaction(record),
        }
    }
    pub(super) fn reserve_recording_initial_inside_transaction(
        &self,
        record: &super::StoredOperationRecord,
    ) -> Result<(), OperationJournalStoreError> {
        let Some(UniversalCommandArgs::Recording(proposal)) = record.universal_command_args()
        else {
            return Ok(());
        };
        if self.connection.is_autocommit()
            || self
                .read_row(record.operation_id)?
                .is_none_or(|row| row.record != *record)
        {
            return Err(error(
                "recording reservation requires the exact initial intent transaction",
            ));
        }
        // There is no retrofit after a historical intent/capture. The normal initial insertion
        // rejects duplicate operation IDs; helper replay only verifies four already exact rows.
        let count: i64 = self.connection.query_row("SELECT COUNT(*) FROM (SELECT slot FROM operation_journal_recording_chunks WHERE operation_id=?1 LIMIT 5)",
            params![proposal.operation_id().to_string()], |r|r.get(0))?;
        if count != 0 {
            self.verify_recording_reservation(proposal, true)?;
            return Ok(());
        }
        reservation_metadata_bound(proposal).map_err(error)?;
        let (rows, bytes) = self.recording_retained_totals()?;
        if rows.checked_add(CONTROL_ROWS).is_none_or(|n| n > 8192)
            || bytes.checked_add(RESERVED_BYTES as i64).is_none_or(|n| {
                n > super::super::recording_commands::MAX_RECORDING_RETAINED_ENCODED_BYTES as i64
            })
        {
            return Err(error(
                "recording retention leaves no terminal metadata reservation",
            ));
        }
        for role in ControlRole::ALL {
            let value = RecordingControlReservation::new(proposal, role).map_err(error)?;
            let encoded = value.encode(proposal).map_err(error)?;
            let sequence = self.next_sequence_inside_transaction()?;
            self.connection.execute("INSERT INTO operation_journal_recording_chunks(chunk_id,operation_id,slot,sequence,record) VALUES(?1,?2,?3,?4,?5)",
                params![value.id().to_string(),proposal.operation_id().to_string(),value.slot(),sequence,encoded])?;
        }
        self.verify_recording_reservation(proposal, true)?;
        Ok(())
    }

    pub(super) fn verify_recording_initial_readback(
        &self,
        record: &super::StoredOperationRecord,
    ) -> Result<(), OperationJournalStoreError> {
        self.verify_recording_initial_readback_with_input(record, None)
    }

    pub(super) fn verify_recording_initial_readback_with_input(
        &self,
        record: &super::StoredOperationRecord,
        input: Option<&crate::operation_journal::owner::InputPermit>,
    ) -> Result<(), OperationJournalStoreError> {
        if let Some(UniversalCommandArgs::Recording(proposal)) = record.universal_command_args() {
            // Test the actual post-COMMIT acknowledgement boundary through the retained owner.
            // A second connection is correctly excluded by the production locking policy.
            #[cfg(test)]
            if let Some(id) = self.recording_readback_corruption.lock().unwrap().take() {
                assert!(self.connection.is_autocommit());
                self.connection.execute(
                    "UPDATE operation_journal_recording_chunks SET record=?1 WHERE chunk_id=?2",
                    params![b"unknown control".as_slice(), id.to_string()],
                )?;
            }
            self.verify_recording_reservation_with_input(proposal, true, input)?;
        }
        Ok(())
    }
}
