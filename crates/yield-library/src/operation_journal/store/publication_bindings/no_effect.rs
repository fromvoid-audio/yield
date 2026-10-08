//! Original no-effect completion only. Immutable issuance fields and charges remain retained.
use super::super::{
    historical_claim_observation::borrowed_sql::scalar,
    initial_settlement_backing::{control, statement},
};
use super::*;
use crate::operation_journal::owner::{InputPermit, NoEffectCompletion};
use rusqlite::types::ValueRef;

// Original and readback typed bindings, bounded staging decode, two canonical encodings,
// fixed SQL/control/error scratch. Namespace and native error loans are independently prepaid.
const WORKING_BYTES: usize =
    4 * MAX_RECORD_BYTES + 2 * super::super::staging_checkpoints::MAX_RECORD_BYTES + 32 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::operation_journal) enum NoEffectWriteOutcome {
    Committed,
    Replay,
}

impl OperationJournalStore {
    fn read_no_effect_binding(
        &self,
        output: Uuid,
        input: &InputPermit,
    ) -> Result<PublicationBindingDraft, OperationJournalStoreError> {
        let output_key = output.to_string();
        let binding = statement(&self.connection, input,
            c"SELECT output_id, publication_effect_id, record FROM operation_journal_publication_issuance WHERE output_id = ?1",
            |row| {
                row.bind_text(1, &output_key)?;
                if !row.next()? { return Err(invalid("original no-effect issuance is absent")); }
                let ValueRef::Blob(bytes) = row.get_ref(2)? else { return Err(invalid("original no-effect issuance is not a blob")); };
                let binding = PublicationBindingDraft::decode(bytes)?;
                if binding.output_id != output
                    || row.get_ref(0)? != ValueRef::Text(output_key.as_bytes())
                    || row.get_ref(1)? != ValueRef::Text(binding.publication_effect().to_string().as_bytes()) {
                    return Err(invalid("original no-effect issuance keys changed"));
                }
                Ok(binding)
            })?;
        statement(&self.connection, input,
            c"SELECT sequence, record, grant_reserved FROM operation_journal_staging_checkpoints WHERE output_id = ?1",
            |row| {
                row.bind_text(1, &output_key)?;
                if !row.next()? { return Err(invalid("original no-effect staging checkpoint is absent")); }
                if scalar::<i64>(row, 0)? != binding.checkpoint_sequence || scalar::<i64>(row, 2)? != 1 {
                    return Err(invalid("original no-effect staging reservation changed"));
                }
                let ValueRef::Blob(bytes) = row.get_ref(1)? else { return Err(invalid("original no-effect staging record is not a blob")); };
                let staging = StagingCheckpointRecord::decode(bytes).map_err(invalid)?;
                binding.validate_staging(&staging)
            })?;
        let claim = binding.charge.claim_id().to_string();
        let operation = binding.command.operation_id.to_string();
        let device = binding.charge.volume().device.to_string();
        let allocated = i64::try_from(binding.charge.allocated_bytes())
            .map_err(|_| invalid("original no-effect charge range"))?;
        let valid = statement(&self.connection, input,
            c"SELECT EXISTS(SELECT 1 FROM operation_journal_ordinary_claim WHERE claim_id = ?1 AND operation_id = ?2 AND volume_device = ?3 AND allocation_class = 'DestinationStaging' AND allocated_bytes = ?4) AND NOT EXISTS(SELECT 1 FROM operation_journal_ordinary_claim_release WHERE claim_id = ?1)",
            |row| {
                row.bind_text(1, &claim)?; row.bind_text(2, &operation)?;
                row.bind_text(3, &device)?; row.bind_integer(4, allocated)?;
                if !row.next()? { return Err(rusqlite::Error::QueryReturnedNoRows.into()); }
                scalar::<bool>(row, 0)
            })?;
        if !valid {
            return Err(invalid(
                "original no-effect ordinary charge changed or was released",
            ));
        }
        if let Some(chunk) = &binding.chunk {
            let valid = statement(&self.connection, input,
                c"SELECT EXISTS(SELECT 1 FROM operation_journal_chunk_claim WHERE chunk_id = ?1 AND issue_id = ?2 AND operation_id = ?3 AND claim_id = ?4 AND participant_id = ?5 AND volume_device = ?6 AND allocation_class = 'DestinationStaging' AND allocated_bytes = ?7 AND state = 'Issued' AND release_id IS NULL)",
                |row| {
                    row.bind_text(1, &chunk.chunk_id.to_string())?;
                    row.bind_text(2, &chunk.issue_id.to_string())?;
                    row.bind_text(3, &operation)?; row.bind_text(4, &claim)?;
                    row.bind_text(5, &binding.command.participant_id.to_string())?;
                    row.bind_text(6, &device)?; row.bind_integer(7, allocated)?;
                    if !row.next()? { return Err(rusqlite::Error::QueryReturnedNoRows.into()); }
                    scalar::<bool>(row, 0)
                })?;
            if !valid {
                return Err(invalid(
                    "original no-effect issued chunk changed or was released",
                ));
            }
        }
        Ok(binding)
    }

    pub(in crate::operation_journal) fn settle_original_publication_no_effect(
        &self,
        completion: &NoEffectCompletion,
        input: &InputPermit,
    ) -> Result<NoEffectWriteOutcome, OperationJournalStoreError> {
        let working = input.reserve_historical_scratch(WORKING_BYTES)?;
        let result = self.settle_no_effect_inner(completion, input);
        if result.is_err() {
            input.retain_historical_error(working, None, None, None);
        }
        result
    }

    fn rollback_no_effect(
        &self,
        input: &InputPermit,
        error: OperationJournalStoreError,
    ) -> OperationJournalStoreError {
        #[cfg(test)]
        if matches!(
            self.insert_test_directive(super::super::InsertTestBoundary::Rollback),
            Some(super::super::InsertTestDirective::Fail(_))
        ) {
            return super::super::staging_checkpoints::uncertain_outcome(
                "original no-effect rollback remains unconfirmed",
            );
        }
        match control(&self.connection, Some(input), c"ROLLBACK") {
            Ok(()) => error,
            Err(_) => super::super::staging_checkpoints::uncertain_outcome(
                "original no-effect rollback remains unconfirmed",
            ),
        }
    }

    fn settle_no_effect_inner(
        &self,
        completion: &NoEffectCompletion,
        input: &InputPermit,
    ) -> Result<NoEffectWriteOutcome, OperationJournalStoreError> {
        let (operation, output, incarnation, generation, _) = completion.binding();
        let (kind, reason, digest) = completion.evidence();
        self.validate_namespace_with_input(Some(input), false)?;
        #[cfg(test)]
        if let Some(super::super::InsertTestDirective::Fail(error)) =
            self.insert_test_directive(super::super::InsertTestBoundary::BeforeBegin)
        {
            return Err(error);
        }
        control(&self.connection, Some(input), c"BEGIN EXCLUSIVE")?;
        let prepared = (|| {
            let mut binding = self.read_no_effect_binding(output, input)?;
            if binding.command.operation_id != operation
                || binding.issuer_incarnation != incarnation
                || binding.slot_generation != generation
            {
                return Err(invalid(
                    "original no-effect completion belongs to another issuance",
                ));
            }
            let captured = StoredNoEffect {
                kind,
                reason: reason.to_owned(),
                digest,
            };
            captured.validate(binding.command_fingerprint)?;
            if let Some(original) = &binding.no_effect {
                if original != &captured {
                    return Err(invalid("original no-effect completion conflicts"));
                }
                return Ok((NoEffectWriteOutcome::Replay, binding.encode()?));
            }
            let original = binding.encode()?;
            binding.schema_version = 2;
            binding.no_effect = Some(captured);
            let encoded = binding.encode()?;
            statement(&self.connection, input,
                c"UPDATE operation_journal_publication_issuance SET record = ?1 WHERE output_id = ?2 AND publication_effect_id = ?3 AND record = ?4",
                |row| {
                    row.bind_blob(1, &encoded)?; row.bind_text(2, &output.to_string())?;
                    row.bind_text(3, &binding.publication_effect().to_string())?;
                    row.bind_blob(4, &original)?;
                    row.next().map(|_| ())
                })?;
            if self.connection.changes() != 1 {
                return Err(invalid("original no-effect issuance CAS changed"));
            }
            #[cfg(test)]
            if let Some(super::super::InsertTestDirective::Fail(error)) = self
                .insert_test_directive(super::super::InsertTestBoundary::BeforePrecommitValidation)
            {
                return Err(error);
            }
            self.validate_namespace_with_input(Some(input), true)?;
            Ok((NoEffectWriteOutcome::Committed, encoded))
        })();
        let (outcome, expected) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return Err(self.rollback_no_effect(input, error)),
        };
        if outcome == NoEffectWriteOutcome::Replay {
            control(&self.connection, Some(input), c"ROLLBACK").map_err(|_| {
                super::super::staging_checkpoints::uncertain_outcome(
                    "original no-effect replay rollback remains unconfirmed",
                )
            })?;
        } else {
            #[cfg(test)]
            match self.insert_test_directive(super::super::InsertTestBoundary::Commit) {
                Some(super::super::InsertTestDirective::Fail(error)) => {
                    return Err(self.rollback_no_effect(input, error));
                }
                Some(super::super::InsertTestDirective::ExecuteThenFail(_)) => {
                    let _ = control(&self.connection, Some(input), c"COMMIT");
                    let _ = control(&self.connection, Some(input), c"ROLLBACK");
                    return Err(super::super::staging_checkpoints::uncertain_outcome(
                        "original no-effect commit acknowledgement lost",
                    ));
                }
                _ => {}
            }
            if control(&self.connection, Some(input), c"COMMIT").is_err() {
                let _ = control(&self.connection, Some(input), c"ROLLBACK");
                return Err(super::super::staging_checkpoints::uncertain_outcome(
                    "original no-effect commit remains unconfirmed",
                ));
            }
            #[cfg(test)]
            if matches!(
                self.insert_test_directive(super::super::InsertTestBoundary::AfterCommit),
                Some(super::super::InsertTestDirective::Fail(_))
            ) {
                return Err(super::super::staging_checkpoints::uncertain_outcome(
                    "original no-effect committed without readback acknowledgement",
                ));
            }
        }
        self.validate_namespace_with_input(Some(input), false)
            .map_err(|error| {
                super::super::staging_checkpoints::postcommit_namespace_error(
                    "original no-effect",
                    error,
                )
            })?;
        let readback = self.read_no_effect_binding(output, input).map_err(|_| {
            super::super::staging_checkpoints::uncertain_outcome(
                "original no-effect readback remains unconfirmed",
            )
        })?;
        if readback.encode()? != expected {
            return Err(super::super::staging_checkpoints::uncertain_outcome(
                "original no-effect exact readback changed",
            ));
        }
        Ok(outcome)
    }
}
