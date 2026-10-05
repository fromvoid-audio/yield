//! Durable, authority-free observations of exact validated staging objects.
//!
//! A row never grants a file effect. The journal owner must hold a fresh file-owner result and
//! live source authority before it can reserve a publication grant.

use rusqlite::types::ValueRef;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::file_operation_owner::{
    AuthorityFence, EffectOutcome, FileOperationResult, RootRegistrationRef, RootRole,
};
use crate::operation_journal::codec::parse_unique_value;
use crate::operation_journal::model::OperationId;
use crate::operation_journal::participants::{ParticipantId, ParticipantKind};

#[cfg(test)]
use super::{InsertTestBoundary, InsertTestDirective};
use super::{OperationJournalStore, OperationJournalStoreError};
use crate::operation_journal::model::{OperationDisposition, SCHEMA_V16};

pub(super) const TABLE_NAME: &str = "operation_journal_staging_checkpoints";
pub(super) const CREATE_TABLE: &str = "CREATE TABLE operation_journal_staging_checkpoints (
    output_id TEXT NOT NULL PRIMARY KEY,
    sequence INTEGER NOT NULL UNIQUE,
    record BLOB NOT NULL CHECK(length(record) <= 65536),
    grant_reserved INTEGER NOT NULL DEFAULT 0 CHECK(grant_reserved IN (0, 1))
)";

const RECORD_SCHEMA_VERSION: u32 = 1;
pub(super) const MAX_RECORD_BYTES: usize = 64 * 1024;
pub(super) const MAX_ROWS: i64 = 8192;

/// A serialized root observation, intentionally separate from the live registration type.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredRootRef {
    owner: Uuid,
    registration_id: Uuid,
    generation: u64,
}

impl StoredRootRef {
    fn from_live(reference: RootRegistrationRef) -> Self {
        Self {
            owner: reference.owner_identity().as_uuid(),
            registration_id: reference.registration_id(),
            generation: reference.generation(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.owner.is_nil() || self.registration_id.is_nil() || self.generation == 0 {
            return Err(String::from("staging root observation is invalid"));
        }
        Ok(())
    }
}

/// Bounded source fence retained as data; it cannot reacquire a source lease.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredStagingSourceFence {
    source_id: String,
    root_identity: String,
    database_identity: String,
    lifecycle_generation: u64,
    source_revision: Option<u64>,
    lease_epoch: u64,
}

impl StoredStagingSourceFence {
    fn from_live(fence: &crate::file_operation_owner::SourceFence) -> Self {
        Self {
            source_id: fence.source_id().as_str().to_owned(),
            root_identity: fence.root_identity().to_owned(),
            database_identity: fence.database_identity().to_owned(),
            lifecycle_generation: fence.lifecycle_generation(),
            source_revision: fence.source_revision(),
            lease_epoch: fence.lease_epoch(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.source_id.is_empty()
            || self.source_id.len() > 256
            || self.root_identity.is_empty()
            || self.root_identity.len() > 256
            || self.database_identity.is_empty()
            || self.database_identity.len() > 256
            || self.lease_epoch == 0
        {
            return Err(String::from("staging source fence is invalid"));
        }
        Ok(())
    }

    pub(super) fn matches_participant(
        &self,
        retained: &crate::operation_journal::participants::StoredSourceFence,
    ) -> bool {
        self.source_id == retained.source_id().as_str()
            && self.root_identity == retained.root_identity()
            && self.database_identity == retained.database_identity()
            && self.lifecycle_generation == retained.lifecycle_generation()
            && self.source_revision == Some(retained.source_revision())
            && self.lease_epoch == retained.lease_epoch()
    }
}

/// An exact, bounded CopyValidated observation with no execution capability.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StagingCheckpointRecord {
    schema_version: u32,
    operation_id: OperationId,
    participant_id: ParticipantId,
    effect_id: Uuid,
    output_id: Uuid,
    staging_root: StoredRootRef,
    source_root: Option<StoredRootRef>,
    source_fence: Option<StoredStagingSourceFence>,
    locator: String,
    logical_object_id: Uuid,
    physical_object_id: String,
    bytes: u64,
    content_hash: [u8; 32],
    request_fingerprint: [u8; 32],
}

impl StagingCheckpointRecord {
    /// Capture one successful, exact file-owner validation without treating it as authority.
    pub(super) fn from_file_result(result: &FileOperationResult) -> Result<Self, String> {
        let EffectOutcome::Applied { evidence } = result.outcome() else {
            return Err(String::from("file owner did not validate staging"));
        };
        let proof = evidence
            .validated_staging()
            .ok_or_else(|| String::from("file-owner result has no staging proof"))?;
        let [binding] = result.root_bindings() else {
            return Err(String::from("staging validation has unexpected root roles"));
        };
        if binding.role() != RootRole::Staging
            || binding.registration() != result.target_registration()
        {
            return Err(String::from(
                "staging root result does not match its target",
            ));
        }
        let crate::file_operation_owner::ExpectedObject::Exact(logical_object) = result.expected()
        else {
            return Err(String::from(
                "staging validation has no exact logical object",
            ));
        };
        if binding.expected() != result.expected()
            || evidence.observed_identity() != Some(logical_object)
            || evidence.bytes() != Some(proof.bytes())
        {
            return Err(String::from("staging result identity or length conflicts"));
        }
        let source_fence = match result.source_fences() {
            [] if proof.source_root().is_none() => None,
            [fence] if proof.source_root().is_some() => {
                if let AuthorityFence::Source(staging_fence) = binding.authority() {
                    if staging_fence != fence {
                        return Err(String::from("staging root source fence conflicts"));
                    }
                }
                Some(StoredStagingSourceFence::from_live(fence))
            }
            _ => return Err(String::from("staging source/root relation is invalid")),
        };
        let record = Self {
            schema_version: RECORD_SCHEMA_VERSION,
            operation_id: result.operation_id(),
            participant_id: result.participant_id(),
            effect_id: result.effect_id().as_uuid(),
            output_id: proof.output().as_uuid(),
            staging_root: StoredRootRef::from_live(result.target_registration()),
            source_root: proof.source_root().map(StoredRootRef::from_live),
            source_fence,
            locator: binding.locator().as_str().to_owned(),
            logical_object_id: logical_object.as_uuid(),
            physical_object_id: proof.stable_identity().to_owned(),
            bytes: proof.bytes(),
            content_hash: proof.content_hash(),
            request_fingerprint: result.fingerprint().as_bytes(),
        };
        record.validate()?;
        Ok(record)
    }

    pub(super) fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub(super) fn participant_id(&self) -> ParticipantId {
        self.participant_id
    }

    pub(super) fn output_id(&self) -> Uuid {
        self.output_id
    }

    pub(super) fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(super) fn source_fence(&self) -> Option<&StoredStagingSourceFence> {
        self.source_fence.as_ref()
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        if self.schema_version != RECORD_SCHEMA_VERSION
            || self.operation_id.as_uuid().is_nil()
            || self.participant_id.as_uuid().is_nil()
            || self.effect_id.is_nil()
            || self.output_id.is_nil()
            || self.logical_object_id.is_nil()
            || self.locator.is_empty()
            || self.locator.len() > 1024
            || self.locator.contains('/')
            || self.locator.contains('\\')
            || self.locator == "."
            || self.locator == ".."
            || self.physical_object_id.is_empty()
            || self.physical_object_id.len() > 256
            || self.source_root.is_some() != self.source_fence.is_some()
        {
            return Err(String::from("staging checkpoint shape is invalid"));
        }
        self.staging_root.validate()?;
        if let Some(root) = &self.source_root {
            root.validate()?;
        }
        if let Some(fence) = &self.source_fence {
            fence.validate()?;
        }
        Ok(())
    }

    pub(super) fn encode(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        let encoded = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        if encoded.len() > MAX_RECORD_BYTES {
            return Err(String::from("staging checkpoint exceeds its record bound"));
        }
        Ok(encoded)
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(String::from("staging checkpoint exceeds its record bound"));
        }
        let value = parse_unique_value(bytes).map_err(|error| error.to_string())?;
        let record: Self = serde_json::from_value(value).map_err(|error| error.to_string())?;
        record.validate()?;
        Ok(record)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StagingCheckpointReceipt {
    Committed { sequence: i64 },
    Replay { sequence: i64 },
}

/// Ephemeral, non-cloneable proof of this process's acknowledged checkpoint write.
/// No stored row or replay path can construct it.
pub(crate) struct StagingCheckpointAck {
    record: StagingCheckpointRecord,
    sequence: i64,
}

fn checkpoint_error(reason: impl Into<String>) -> OperationJournalStoreError {
    OperationJournalStoreError::StagingCheckpoint {
        reason: reason.into(),
    }
}

fn uncertain_outcome(reason: impl Into<String>) -> OperationJournalStoreError {
    OperationJournalStoreError::StagingOutcomeUncertain {
        reason: reason.into(),
    }
}

impl OperationJournalStore {
    fn rollback_staging_error(
        &self,
        error: OperationJournalStoreError,
    ) -> OperationJournalStoreError {
        match self.connection.execute_batch("ROLLBACK") {
            Ok(()) => error,
            Err(rollback) => {
                uncertain_outcome(format!("{error}; rollback unconfirmed: {rollback}"))
            }
        }
    }

    fn commit_staging_transaction(&self) -> Result<(), OperationJournalStoreError> {
        self.connection.execute_batch("COMMIT").map_err(|error| {
            let rollback = self.connection.execute_batch("ROLLBACK");
            uncertain_outcome(format!(
                "commit outcome unconfirmed: {error}; rollback: {rollback:?}"
            ))
        })
    }
    pub(crate) fn checkpoint_staging_result(
        &self,
        result: &FileOperationResult,
    ) -> Result<Option<StagingCheckpointAck>, OperationJournalStoreError> {
        let record = StagingCheckpointRecord::from_file_result(result).map_err(checkpoint_error)?;
        match self.checkpoint_staging(&record)? {
            StagingCheckpointReceipt::Committed { sequence } => {
                Ok(Some(StagingCheckpointAck { record, sequence }))
            }
            StagingCheckpointReceipt::Replay { .. } => Ok(None),
        }
    }

    /// Acknowledge the exact file-owner observation only after a durable write and readback.
    /// A replay is deliberately read-only and cannot issue a publication grant.
    pub(super) fn checkpoint_staging(
        &self,
        record: &StagingCheckpointRecord,
    ) -> Result<StagingCheckpointReceipt, OperationJournalStoreError> {
        let encoded = record.encode().map_err(checkpoint_error)?;
        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN EXCLUSIVE")?;
        let outcome = (|| {
            let operation = self
                .read_row(record.operation_id())?
                .ok_or_else(|| {
                    checkpoint_error("staging operation is absent from the durable journal")
                })?
                .record;
            if operation.schema_version != SCHEMA_V16
                || !operation.phase.is_pre_publication()
                || operation.disposition != OperationDisposition::None
            {
                return Err(checkpoint_error(
                    "staging operation is cancelled, terminal, or past publication",
                ));
            }
            let participants = operation
                .universal_intent
                .as_ref()
                .ok_or_else(|| checkpoint_error("staging operation has no participant set"))?
                .participants()
                .descriptors();
            let participant = participants
                .iter()
                .find(|participant| participant.id() == record.participant_id())
                .ok_or_else(|| {
                    checkpoint_error("staging participant is absent from the operation")
                })?;
            if participant.kind() != ParticipantKind::Filesystem {
                return Err(checkpoint_error(
                    "staging participant is not a filesystem participant",
                ));
            }
            if let Some(fence) = record.source_fence() {
                if !participants
                    .iter()
                    .filter_map(|participant| participant.source_fence())
                    .any(|retained| fence.matches_participant(retained))
                {
                    return Err(checkpoint_error(
                        "staging source fence differs from durable participants",
                    ));
                }
            }
            if let Some((sequence, existing, reserved)) =
                self.read_staging_checkpoint_row(record.output_id())?
            {
                if existing != *record {
                    return Err(checkpoint_error(
                        "output has a conflicting staging checkpoint",
                    ));
                }
                if reserved {
                    return Err(checkpoint_error("publication grant was already reserved"));
                }
                return Ok(StagingCheckpointReceipt::Replay { sequence });
            }
            let count: i64 = self.connection.query_row(
                "SELECT COUNT(*) FROM operation_journal_staging_checkpoints",
                [],
                |row| row.get(0),
            )?;
            if count >= MAX_ROWS {
                return Err(checkpoint_error(
                    "staging checkpoint row bound is exhausted",
                ));
            }
            let sequence = self.next_sequence_inside_transaction()?;
            self.connection.execute(
                "INSERT INTO operation_journal_staging_checkpoints (output_id, sequence, record, grant_reserved) VALUES (?1, ?2, ?3, 0)",
                params![record.output_id().to_string(), sequence, encoded],
            )?;
            #[cfg(test)]
            if let Some(InsertTestDirective::Fail(error)) =
                self.insert_test_directive(InsertTestBoundary::BeforePrecommitValidation)
            {
                return Err(checkpoint_error(format!(
                    "checkpoint precommit failed: {error}"
                )));
            }
            self.validate_namespace_for_precommit()?;
            Ok(StagingCheckpointReceipt::Committed { sequence })
        })();
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                return Err(self.rollback_staging_error(error));
            }
        };
        if matches!(outcome, StagingCheckpointReceipt::Replay { .. }) {
            self.connection.execute_batch("ROLLBACK").map_err(|error| {
                uncertain_outcome(format!("replay rollback unconfirmed: {error}"))
            })?;
            self.validate_namespace()?;
            return Ok(outcome);
        }
        #[cfg(test)]
        let commit_directive = self.insert_test_directive(InsertTestBoundary::Commit);
        #[cfg(test)]
        if let Some(InsertTestDirective::Fail(error)) = commit_directive {
            return Err(self.rollback_staging_error(checkpoint_error(format!(
                "checkpoint commit was rejected: {error}"
            ))));
        }
        #[cfg(test)]
        if let Some(InsertTestDirective::ExecuteThenFail(error)) = commit_directive {
            let commit = self.connection.execute_batch("COMMIT");
            let _ = self.connection.execute_batch("ROLLBACK");
            return Err(uncertain_outcome(format!(
                "checkpoint commit outcome is uncertain: {error}; {commit:?}"
            )));
        }
        self.commit_staging_transaction()?;
        #[cfg(test)]
        if let Some(InsertTestDirective::Fail(error)) =
            self.insert_test_directive(InsertTestBoundary::AfterCommit)
        {
            return Err(uncertain_outcome(format!(
                "checkpoint committed without acknowledgement: {error}"
            )));
        }
        self.validate_namespace().map_err(|error| {
            uncertain_outcome(format!("checkpoint postcommit namespace: {error}"))
        })?;
        let (sequence, readback, reserved) = self
            .read_staging_checkpoint_row(record.output_id())
            .map_err(|error| uncertain_outcome(format!("checkpoint readback failed: {error}")))?
            .ok_or_else(|| uncertain_outcome("staging checkpoint disappeared during readback"))?;
        if readback != *record
            || reserved
            || outcome != (StagingCheckpointReceipt::Committed { sequence })
        {
            return Err(uncertain_outcome(
                "staging checkpoint readback differs from committed record",
            ));
        }
        Ok(outcome)
    }

    /// Consume an acknowledged checkpoint exactly once. A decoded row alone cannot call this:
    /// the caller must supply the fresh private file-owner result again.
    pub(crate) fn reserve_staging_grant(
        &self,
        acknowledgement: StagingCheckpointAck,
        result: &FileOperationResult,
    ) -> Result<(), OperationJournalStoreError> {
        let record = StagingCheckpointRecord::from_file_result(result).map_err(checkpoint_error)?;
        if record != acknowledgement.record {
            return Err(checkpoint_error(
                "grant validation differs from acknowledged checkpoint",
            ));
        }
        self.reserve_staging_grant_record(&record, acknowledgement.sequence)
    }

    pub(super) fn reserve_staging_grant_record(
        &self,
        record: &StagingCheckpointRecord,
        acknowledged_sequence: i64,
    ) -> Result<(), OperationJournalStoreError> {
        self.validate_namespace()?;
        self.connection.execute_batch("BEGIN EXCLUSIVE")?;
        let transition = (|| {
            let operation = self
                .read_row(record.operation_id())?
                .ok_or_else(|| checkpoint_error("grant operation is absent"))?
                .record;
            if operation.schema_version != SCHEMA_V16
                || !operation.phase.is_pre_publication()
                || operation.disposition != OperationDisposition::None
            {
                return Err(checkpoint_error(
                    "grant operation is cancelled, terminal, or past publication",
                ));
            }
            let (sequence, retained, reserved) = self
                .read_staging_checkpoint_row(record.output_id())?
                .ok_or_else(|| checkpoint_error("no acknowledged staging checkpoint exists"))?;
            if sequence != acknowledged_sequence || retained != *record || reserved {
                return Err(checkpoint_error(
                    "checkpoint conflicts or grant was already reserved",
                ));
            }
            let updated = self.connection.execute(
                "UPDATE operation_journal_staging_checkpoints SET grant_reserved = 1 WHERE output_id = ?1 AND grant_reserved = 0",
                params![record.output_id().to_string()],
            )?;
            if updated != 1 {
                return Err(checkpoint_error(
                    "grant reservation lost its single-use transition",
                ));
            }
            #[cfg(test)]
            if let Some(InsertTestDirective::Fail(error)) =
                self.insert_test_directive(InsertTestBoundary::BeforePrecommitValidation)
            {
                return Err(checkpoint_error(format!("grant precommit failed: {error}")));
            }
            self.validate_namespace_for_precommit()?;
            Ok(())
        })();
        if let Err(error) = transition {
            return Err(self.rollback_staging_error(error));
        }
        #[cfg(test)]
        let commit_directive = self.insert_test_directive(InsertTestBoundary::Commit);
        #[cfg(test)]
        if let Some(InsertTestDirective::Fail(error)) = commit_directive {
            return Err(self.rollback_staging_error(checkpoint_error(format!(
                "grant commit was rejected: {error}"
            ))));
        }
        #[cfg(test)]
        if let Some(InsertTestDirective::ExecuteThenFail(error)) = commit_directive {
            let commit = self.connection.execute_batch("COMMIT");
            let _ = self.connection.execute_batch("ROLLBACK");
            return Err(uncertain_outcome(format!(
                "grant commit outcome is uncertain: {error}; {commit:?}"
            )));
        }
        self.commit_staging_transaction()?;
        #[cfg(test)]
        if let Some(InsertTestDirective::Fail(error)) =
            self.insert_test_directive(InsertTestBoundary::AfterCommit)
        {
            return Err(uncertain_outcome(format!(
                "grant committed without acknowledgement: {error}"
            )));
        }
        self.validate_namespace()
            .map_err(|error| uncertain_outcome(format!("grant postcommit namespace: {error}")))?;
        let (_, retained, reserved) = self
            .read_staging_checkpoint_row(record.output_id())
            .map_err(|error| uncertain_outcome(format!("grant readback failed: {error}")))?
            .ok_or_else(|| uncertain_outcome("reserved checkpoint disappeared during readback"))?;
        if retained != *record || !reserved {
            return Err(uncertain_outcome(
                "grant reservation readback differs from exact checkpoint",
            ));
        }
        Ok(())
    }

    pub(super) fn read_staging_checkpoint_row(
        &self,
        output_id: Uuid,
    ) -> Result<Option<(i64, StagingCheckpointRecord, bool)>, OperationJournalStoreError> {
        let raw = self.connection.query_row(
            "SELECT typeof(output_id), typeof(sequence), sequence, typeof(record), length(record), record, typeof(grant_reserved), grant_reserved FROM operation_journal_staging_checkpoints WHERE output_id = ?1",
            params![output_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                    match row.get_ref(2)? { ValueRef::Integer(value) => Some(value), _ => None },
                    row.get::<_, String>(3)?,
                    match row.get_ref(4)? { ValueRef::Integer(value) => Some(value), _ => None },
                    match row.get_ref(5)? { ValueRef::Blob(value) => Some(value.to_vec()), _ => None },
                    row.get::<_, String>(6)?,
                    match row.get_ref(7)? { ValueRef::Integer(value) => Some(value), _ => None },
                ))
            },
        ).optional()?;
        let Some((
            key_type,
            sequence_type,
            sequence,
            record_type,
            length,
            blob,
            reserved_type,
            reserved,
        )) = raw
        else {
            return Ok(None);
        };
        let sequence = sequence
            .ok_or_else(|| checkpoint_error("staging checkpoint sequence is not an integer"))?;
        let length = length
            .ok_or_else(|| checkpoint_error("staging checkpoint length is not an integer"))?;
        let blob =
            blob.ok_or_else(|| checkpoint_error("staging checkpoint record is not a blob"))?;
        if key_type != "text"
            || sequence_type != "integer"
            || sequence <= 0
            || record_type != "blob"
            || length < 0
            || length as usize != blob.len()
            || blob.len() > MAX_RECORD_BYTES
            || reserved_type != "integer"
            || !matches!(reserved, Some(0 | 1))
        {
            return Err(checkpoint_error(
                "staging checkpoint has malformed SQLite cells",
            ));
        }
        let record = StagingCheckpointRecord::decode(&blob).map_err(checkpoint_error)?;
        if record.output_id() != output_id {
            return Err(checkpoint_error(
                "staging checkpoint key differs from its record",
            ));
        }
        self.ensure_sequence_unique(sequence)?;
        Ok(Some((sequence, record, reserved == Some(1))))
    }
}
