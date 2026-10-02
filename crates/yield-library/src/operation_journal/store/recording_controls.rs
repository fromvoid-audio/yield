//! Same-transaction reserved recording metadata. No closure, grant or retirement authority.
use super::{OperationJournalStore, OperationJournalStoreError};
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
        if let Some(UniversalCommandArgs::Recording(proposal)) = record.universal_command_args() {
            reservation_metadata_bound(proposal).map_err(error)?;
            self.configure_recording_metadata_policy()?;
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
            let bytes: Vec<u8> = self.connection.query_row("SELECT record FROM operation_journal_recording_chunks WHERE operation_id=?1 AND slot=?2",
                params![proposal.operation_id().to_string(), role.slot()], |r|r.get(0))?;
            let observed = RecordingControlReservation::decode(&bytes, proposal).map_err(error)?;
            if observed != expected {
                return Err(error("conflicting reserved recording control"));
            }
        }
        let first_capture: Option<i64> = self
            .connection
            .query_row(
                "SELECT sequence FROM operation_journal_recording_chunks WHERE operation_id=?1
             AND slot NOT BETWEEN 20000 AND 20003 ORDER BY sequence LIMIT 1",
                params![proposal.operation_id().to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if first_capture.is_some_and(|sequence| sequence <= previous_sequence.unwrap()) {
            return Err(error("recording capture precedes its terminal reservation"));
        }
        Ok(true)
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
            self.verify_recording_reservation(proposal, true)?;
        }
        Ok(())
    }
}
