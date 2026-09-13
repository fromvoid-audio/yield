//! Strict bounded codec for stored operation-journal records.
//!
//! Decoding produces untrusted storage data. It does not validate the operation state machine,
//! establish ownership, or authorize a filesystem action. Callers must retain the original bytes
//! whenever decoding returns an error, including an unsupported schema version.

use std::io::{self, Write};

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use uuid::Uuid;

use super::coordinator::{
    CopyIntentArgs, RetryObservation, UniversalCommandArgs, WaveformEditIntentArgs,
    validate_persisted_copy_intent_value, validate_persisted_import_intent_value,
    validate_persisted_retry_observation_value, validate_persisted_universal_command_args_value,
    validate_persisted_waveform_edit_value,
};
use super::evidence::{
    AbsentFinalAdoptionEvidence, AbsentFinalRecoveryObservation, AbsentFinalTransactionOwnedProof,
    FilesystemPublishedWaveformRestore, FilesystemStagedWaveformRestore, FinalNamespaceClaim,
    PreparedAbsentFinalNoReplace, PreparedTargetContract, PreparedWaveformRestore,
    ReplacementQualificationAssessment,
};
use super::model::{
    CancellationRequestObservation, MAX_RECORD_BYTES, MAX_UNIVERSAL_LABEL_BYTES,
    MAX_USER_STATUS_KEY_BYTES, OperationActor, OperationDisposition, OperationId, OperationIntent,
    OperationPhase, SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7,
    SCHEMA_V8, SCHEMA_V9, SCHEMA_V10, SCHEMA_V11, SCHEMA_V12, UniversalIntent,
};
use super::namespace_commands::validate_persisted_namespace_command_args_value;
use super::participants::MAX_PARTICIPANTS;
use super::removal_commands::validate_persisted_removal_command_args_value;
use super::render_commands::validate_persisted_render_command_args_value;
use super::retained_resolution_commands::validate_persisted_retained_resolution_command_args_value;
use super::selection_normalize_commands::validate_persisted_selection_normalize_intent_args_value;
use crate::durable_capacity::DurableCapacityPlan;

/// One complete stored operation record.
///
/// Every field is untrusted data recovered from storage. The record is not transition-valid by
/// construction, and its locators, identities, and publication values do not authorize execution.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredOperationRecord {
    /// Persisted record schema version.
    pub schema_version: u32,
    /// Stored operation identity.
    pub operation_id: OperationId,
    /// Stored operation intent.
    pub intent: OperationIntent,
    /// Optional V3 universal-intent observation. It is required for schema V3 and absent in
    /// legacy V1/V2 records.
    pub universal_intent: Option<UniversalIntent>,
    /// Last stored coordinator phase.
    pub phase: OperationPhase,
    /// Last stored disposition overlay.
    pub disposition: OperationDisposition,
    /// Opaque stored coordinator metadata.
    pub payload: Value,
    /// Optional stored physical-capacity plan.
    pub capacity_plan: Option<DurableCapacityPlan>,
    /// Optional stored preparation evidence.
    pub prepared: Option<PreparedTargetContract>,
    /// Optional stored filesystem staging evidence.
    pub staged: Option<FilesystemStagedWaveformRestore>,
    /// Optional schema-v2 absent-final recovery observation.
    pub absent_final_recovery_observation: Option<AbsentFinalRecoveryObservation>,
    /// Optional schema-v2 absent-final transaction-owned proof.
    pub absent_final_transaction_owned_proof: Option<AbsentFinalTransactionOwnedProof>,
    /// Optional schema-v2 absent-final adoption evidence.
    pub absent_final_adoption_evidence: Option<AbsentFinalAdoptionEvidence>,
    /// Optional stored publication evidence. This value contains no live capability.
    published: Option<FilesystemPublishedWaveformRestore>,
    /// Optional stored replacement qualification assessment.
    pub replacement_qualification: Option<ReplacementQualificationAssessment>,
    /// Optional V4 pre-publication cancellation request observation.
    cancellation_request: Option<CancellationRequestObservation>,
    /// Optional V5 typed coordinator command arguments.
    command_args: Option<CopyIntentArgs>,
    /// Optional V5 durable retry/status observation.
    retry_observation: Option<RetryObservation>,
    /// Optional V6-V12 tagged command arguments.
    universal_command_args: Option<UniversalCommandArgs>,
    /// Stored creation timestamp in Unix milliseconds.
    pub created_unix_ms: i64,
    /// Stored update timestamp in Unix milliseconds.
    pub updated_unix_ms: i64,
}

impl StoredOperationRecord {
    pub(crate) fn new_v5_copy(
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: CopyIntentArgs,
        capacity_plan: DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Self {
        Self {
            schema_version: SCHEMA_V5,
            operation_id,
            intent,
            universal_intent: Some(universal_intent),
            phase: OperationPhase::IntentDurable,
            disposition: OperationDisposition::None,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: None,
            command_args: Some(command_args),
            retry_observation: None,
            universal_command_args: None,
            created_unix_ms: timestamp_unix_ms,
            updated_unix_ms: timestamp_unix_ms,
        }
    }

    pub(crate) fn new_v6_command(
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Self {
        Self {
            schema_version: SCHEMA_V6,
            operation_id,
            intent,
            universal_intent: Some(universal_intent),
            phase: OperationPhase::IntentDurable,
            disposition: OperationDisposition::None,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: None,
            command_args: None,
            retry_observation: None,
            universal_command_args: Some(command_args),
            created_unix_ms: timestamp_unix_ms,
            updated_unix_ms: timestamp_unix_ms,
        }
    }

    pub(crate) fn new_v7_waveform_edit(
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: WaveformEditIntentArgs,
        capacity_plan: DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Self {
        Self::new_v7_command(
            operation_id,
            intent,
            universal_intent,
            UniversalCommandArgs::WaveformEdit(command_args),
            capacity_plan,
            timestamp_unix_ms,
        )
    }

    pub(crate) fn new_v7_command(
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Self {
        Self {
            schema_version: SCHEMA_V7,
            operation_id,
            intent,
            universal_intent: Some(universal_intent),
            phase: OperationPhase::IntentDurable,
            disposition: OperationDisposition::None,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: None,
            command_args: None,
            retry_observation: None,
            universal_command_args: Some(command_args),
            created_unix_ms: timestamp_unix_ms,
            updated_unix_ms: timestamp_unix_ms,
        }
    }

    pub(crate) fn new_v8_command(
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Self {
        Self {
            schema_version: SCHEMA_V8,
            operation_id,
            intent,
            universal_intent: Some(universal_intent),
            phase: OperationPhase::IntentDurable,
            disposition: OperationDisposition::None,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: None,
            command_args: None,
            retry_observation: None,
            universal_command_args: Some(command_args),
            created_unix_ms: timestamp_unix_ms,
            updated_unix_ms: timestamp_unix_ms,
        }
    }

    pub(crate) fn new_v9_command(
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Self {
        Self {
            schema_version: SCHEMA_V9,
            operation_id,
            intent,
            universal_intent: Some(universal_intent),
            phase: OperationPhase::IntentDurable,
            disposition: OperationDisposition::None,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: None,
            command_args: None,
            retry_observation: None,
            universal_command_args: Some(command_args),
            created_unix_ms: timestamp_unix_ms,
            updated_unix_ms: timestamp_unix_ms,
        }
    }

    pub(crate) fn new_v10_command(
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Self {
        Self {
            schema_version: SCHEMA_V10,
            operation_id,
            intent,
            universal_intent: Some(universal_intent),
            phase: OperationPhase::IntentDurable,
            disposition: OperationDisposition::None,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: None,
            command_args: None,
            retry_observation: None,
            universal_command_args: Some(command_args),
            created_unix_ms: timestamp_unix_ms,
            updated_unix_ms: timestamp_unix_ms,
        }
    }

    pub(crate) fn new_v11_command(
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Self {
        Self {
            schema_version: SCHEMA_V11,
            operation_id,
            intent,
            universal_intent: Some(universal_intent),
            phase: OperationPhase::IntentDurable,
            disposition: OperationDisposition::None,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: None,
            command_args: None,
            retry_observation: None,
            universal_command_args: Some(command_args),
            created_unix_ms: timestamp_unix_ms,
            updated_unix_ms: timestamp_unix_ms,
        }
    }

    pub(crate) fn new_v12_command(
        operation_id: OperationId,
        intent: OperationIntent,
        universal_intent: UniversalIntent,
        command_args: UniversalCommandArgs,
        capacity_plan: DurableCapacityPlan,
        timestamp_unix_ms: i64,
    ) -> Self {
        Self {
            schema_version: SCHEMA_V12,
            operation_id,
            intent,
            universal_intent: Some(universal_intent),
            phase: OperationPhase::IntentDurable,
            disposition: OperationDisposition::None,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: None,
            command_args: None,
            retry_observation: None,
            universal_command_args: Some(command_args),
            created_unix_ms: timestamp_unix_ms,
            updated_unix_ms: timestamp_unix_ms,
        }
    }

    /// Borrow stored publication evidence without exposing a construction path for it.
    pub fn published(&self) -> Option<&FilesystemPublishedWaveformRestore> {
        self.published.as_ref()
    }

    /// Borrow the persisted cancellation request observation, when present.
    pub fn cancellation_request(&self) -> Option<&CancellationRequestObservation> {
        self.cancellation_request.as_ref()
    }

    /// Borrow the persisted V5 typed copy arguments, when present.
    pub fn command_args(&self) -> Option<&CopyIntentArgs> {
        self.command_args.as_ref()
    }

    /// Borrow the persisted V5 retry/status observation, when present.
    pub fn retry_observation(&self) -> Option<&RetryObservation> {
        self.retry_observation.as_ref()
    }

    /// Borrow the persisted V6 tagged command arguments, when present.
    pub fn universal_command_args(&self) -> Option<&UniversalCommandArgs> {
        self.universal_command_args.as_ref()
    }

    pub(crate) fn with_cancellation_request(
        &self,
        observation: CancellationRequestObservation,
    ) -> Self {
        let mut record = self.clone();
        if !matches!(
            record.schema_version,
            SCHEMA_V5
                | SCHEMA_V6
                | SCHEMA_V7
                | SCHEMA_V8
                | SCHEMA_V9
                | SCHEMA_V10
                | SCHEMA_V11
                | SCHEMA_V12
        ) {
            record.schema_version = SCHEMA_V4;
        }
        record.disposition = OperationDisposition::CancelRequestedBeforePublish;
        record.updated_unix_ms = observation.requested_unix_ms();
        record.cancellation_request = Some(observation);
        record
    }

    pub(crate) fn with_retry_pending(&self, observation: RetryObservation) -> Self {
        let mut record = self.clone();
        if !matches!(
            record.schema_version,
            SCHEMA_V6 | SCHEMA_V7 | SCHEMA_V8 | SCHEMA_V9 | SCHEMA_V10 | SCHEMA_V11 | SCHEMA_V12
        ) {
            record.schema_version = SCHEMA_V5;
        }
        record.disposition = OperationDisposition::RetryPending;
        record.updated_unix_ms = observation.requested_unix_ms();
        record.retry_observation = Some(observation);
        record.cancellation_request = None;
        record
    }
}

/// Failure while decoding an untrusted record.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DecodeError {
    /// Input exceeded the bounded record size before JSON parsing began.
    #[error("operation journal record exceeds {maximum} bytes: {actual} bytes")]
    Oversize {
        /// Input byte count.
        actual: usize,
        /// Maximum accepted byte count.
        maximum: usize,
    },
    /// Input was malformed JSON or violated the strict known-schema boundary.
    #[error("malformed operation journal record: {reason}")]
    Malformed {
        /// Stable diagnostic describing the rejected boundary.
        reason: String,
    },
    /// The record uses a newer or otherwise unsupported schema and must remain retained raw.
    #[error("unsupported operation journal schema version: {version}")]
    UnsupportedVersion {
        /// Unsupported schema identifier.
        version: u32,
    },
}

/// Failure while encoding a stored record into the bounded persisted representation.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum EncodeError {
    /// The record schema cannot be written by this codec.
    #[error("unsupported operation journal schema version: {version}")]
    UnsupportedSchemaVersion {
        /// Unsupported schema identifier.
        version: u32,
    },
    /// Schema-v1 cannot represent a schema-v2-only evidence value.
    #[error("schema-v1 cannot encode {kind}")]
    V2EvidenceInV1 {
        /// Evidence kind that cannot be represented in schema-v1.
        kind: &'static str,
    },
    /// Schema V3 requires the universal intent wrapper and cannot silently omit it.
    #[error("schema-v3 requires a universal intent observation")]
    MissingV3UniversalIntent,
    /// Schema V3 does not carry execution evidence from the legacy waveform workflow.
    #[error("schema-v3 cannot encode legacy execution evidence: {kind}")]
    LegacyEvidenceInV3 {
        /// Legacy evidence field that would be discarded.
        kind: &'static str,
    },
    /// A V3 universal-intent value cannot be silently dropped while encoding a legacy schema.
    #[error("schema-v{version} cannot encode a V3 universal intent observation")]
    UniversalIntentInLegacy {
        /// Legacy schema identifier that cannot carry the universal intent.
        version: u32,
    },
    /// A V3 initial record violates its bounded shape or initial-state contract.
    #[error("invalid schema-v3 universal intent record: {reason}")]
    InvalidV3Record {
        /// Bounded validation diagnostic.
        reason: String,
    },
    /// A legacy schema would silently discard a V4 cancellation observation.
    #[error("schema-v{version} cannot encode a cancellation request observation")]
    CancellationRequestInLegacy {
        /// Legacy schema identifier that cannot carry the cancellation request.
        version: u32,
    },
    /// A V4 cancellation-request record violates its bounded transition shape.
    #[error("invalid schema-v4 cancellation request record: {reason}")]
    InvalidV4Record {
        /// Bounded validation diagnostic.
        reason: String,
    },
    /// A prior schema would silently discard typed coordinator fields.
    #[error("schema-v{version} cannot encode V5 coordinator fields: {kind}")]
    CoordinatorFieldsInPriorSchema {
        /// Prior schema identifier that cannot carry the fields.
        version: u32,
        /// Coordinator field that would be discarded.
        kind: &'static str,
    },
    /// A V5 typed coordinator record violates its bounded state or argument contract.
    #[error("invalid schema-v5 coordinator record: {reason}")]
    InvalidV5Record {
        /// Bounded validation diagnostic.
        reason: String,
    },
    /// A V6 tagged coordinator record violates its bounded state or argument contract.
    #[error("invalid schema-v6 coordinator record: {reason}")]
    InvalidV6Record {
        /// Bounded validation diagnostic.
        reason: String,
    },
    /// A V7 typed waveform-edit record violates its bounded state or argument contract.
    #[error("invalid schema-v7 waveform edit record: {reason}")]
    InvalidV7Record {
        /// Bounded validation diagnostic.
        reason: String,
    },
    /// A V8 namespace-command record violates its bounded state or argument contract.
    #[error("invalid schema-v8 namespace command record: {reason}")]
    InvalidV8Record {
        /// Bounded validation diagnostic.
        reason: String,
    },
    /// A V9 whole-file render record violates its bounded state or argument contract.
    #[error("invalid schema-v9 render command record: {reason}")]
    InvalidV9Record {
        /// Bounded validation diagnostic.
        reason: String,
    },
    /// A V10 removal-command record violates its bounded state or argument contract.
    #[error("invalid schema-v10 removal command record: {reason}")]
    InvalidV10Record {
        /// Bounded validation diagnostic.
        reason: String,
    },
    /// A V11 retained-resolution record violates its bounded state or argument contract.
    #[error("invalid schema-v11 retained resolution record: {reason}")]
    InvalidV11Record {
        /// Bounded validation diagnostic.
        reason: String,
    },
    /// A V12 selection-Normalize record violates its bounded state or argument contract.
    #[error("invalid schema-v12 selection Normalize record: {reason}")]
    InvalidV12Record {
        /// Bounded validation diagnostic.
        reason: String,
    },
    /// Serialized output exceeded the bounded record size.
    #[error("encoded operation journal record exceeds {maximum} bytes: {actual} bytes")]
    Oversize {
        /// Bytes attempted by the serializer.
        actual: usize,
        /// Maximum output byte count.
        maximum: usize,
    },
    /// JSON serialization failed before the output bound was reached.
    #[error("could not encode operation journal record: {0}")]
    Serialization(String),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedOperationRecordV1 {
    schema_version: u32,
    operation_id: Uuid,
    intent: OperationIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    payload: Value,
    #[serde(default)]
    capacity_plan: Option<DurableCapacityPlan>,
    #[serde(default)]
    prepared: Option<PreparedWaveformRestore>,
    #[serde(default)]
    staged: Option<FilesystemStagedWaveformRestore>,
    #[serde(default)]
    published: Option<FilesystemPublishedWaveformRestore>,
    #[serde(default)]
    replacement_qualification: Option<ReplacementQualificationAssessment>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

impl From<PersistedOperationRecordV1> for StoredOperationRecord {
    fn from(record: PersistedOperationRecordV1) -> Self {
        Self {
            schema_version: record.schema_version,
            operation_id: OperationId::from_uuid(record.operation_id),
            intent: record.intent,
            universal_intent: None,
            phase: record.phase,
            disposition: record.disposition,
            payload: record.payload,
            capacity_plan: record.capacity_plan,
            prepared: record
                .prepared
                .map(PreparedTargetContract::ExistingExpectedIdentity),
            staged: record.staged,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: record.published,
            replacement_qualification: record.replacement_qualification,
            cancellation_request: None,
            command_args: None,
            retry_observation: None,
            universal_command_args: None,
            created_unix_ms: record.created_unix_ms,
            updated_unix_ms: record.updated_unix_ms,
        }
    }
}

fn persisted_v1_from_record(
    record: &StoredOperationRecord,
) -> Result<PersistedOperationRecordV1Ref<'_>, EncodeError> {
    if record.universal_intent.is_some() {
        return Err(EncodeError::UniversalIntentInLegacy {
            version: record.schema_version,
        });
    }
    if record.cancellation_request.is_some() {
        return Err(EncodeError::CancellationRequestInLegacy {
            version: record.schema_version,
        });
    }
    if record.command_args.is_some() {
        return Err(EncodeError::CoordinatorFieldsInPriorSchema {
            version: record.schema_version,
            kind: "typed command arguments",
        });
    }
    if record.universal_command_args.is_some() {
        return Err(EncodeError::CoordinatorFieldsInPriorSchema {
            version: record.schema_version,
            kind: "V6 tagged command arguments",
        });
    }
    if record.retry_observation.is_some() {
        return Err(EncodeError::CoordinatorFieldsInPriorSchema {
            version: record.schema_version,
            kind: "retry observation",
        });
    }
    if record.absent_final_recovery_observation.is_some() {
        return Err(EncodeError::V2EvidenceInV1 {
            kind: "absent-final recovery observation evidence",
        });
    }
    if record.absent_final_transaction_owned_proof.is_some() {
        return Err(EncodeError::V2EvidenceInV1 {
            kind: "absent-final transaction-owned proof evidence",
        });
    }
    if record.absent_final_adoption_evidence.is_some() {
        return Err(EncodeError::V2EvidenceInV1 {
            kind: "absent-final adoption evidence",
        });
    }
    let prepared = match record.prepared.as_ref() {
        None => None,
        Some(PreparedTargetContract::ExistingExpectedIdentity(prepared)) => Some(prepared),
        Some(PreparedTargetContract::AbsentFinalNoReplace(_)) => {
            return Err(EncodeError::V2EvidenceInV1 {
                kind: "absent-final preparation evidence",
            });
        }
    };
    if record
        .published
        .as_ref()
        .is_some_and(is_absent_final_publication)
    {
        return Err(EncodeError::V2EvidenceInV1 {
            kind: "absent-final publication evidence",
        });
    }
    Ok(PersistedOperationRecordV1Ref {
        schema_version: record.schema_version,
        operation_id: record.operation_id.as_uuid(),
        intent: &record.intent,
        phase: record.phase,
        disposition: record.disposition,
        payload: &record.payload,
        capacity_plan: record.capacity_plan.as_ref(),
        prepared,
        staged: record.staged.as_ref(),
        published: record.published.as_ref(),
        replacement_qualification: record.replacement_qualification.as_ref(),
        created_unix_ms: record.created_unix_ms,
        updated_unix_ms: record.updated_unix_ms,
    })
}

#[derive(Serialize)]
struct PersistedOperationRecordV1Ref<'a> {
    schema_version: u32,
    operation_id: Uuid,
    intent: &'a OperationIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    payload: &'a Value,
    capacity_plan: Option<&'a DurableCapacityPlan>,
    prepared: Option<&'a PreparedWaveformRestore>,
    staged: Option<&'a FilesystemStagedWaveformRestore>,
    published: Option<&'a FilesystemPublishedWaveformRestore>,
    replacement_qualification: Option<&'a ReplacementQualificationAssessment>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedOperationRecordV2 {
    schema_version: u32,
    operation_id: Uuid,
    intent: OperationIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    payload: Value,
    #[serde(default)]
    capacity_plan: Option<DurableCapacityPlan>,
    #[serde(default)]
    prepared: Option<PersistedPreparedTargetContractV2>,
    #[serde(default)]
    staged: Option<FilesystemStagedWaveformRestore>,
    #[serde(default)]
    absent_final_recovery_observation: Option<AbsentFinalRecoveryObservation>,
    #[serde(default)]
    absent_final_transaction_owned_proof: Option<AbsentFinalTransactionOwnedProof>,
    #[serde(default)]
    absent_final_adoption_evidence: Option<AbsentFinalAdoptionEvidence>,
    #[serde(default)]
    published: Option<FilesystemPublishedWaveformRestore>,
    #[serde(default)]
    replacement_qualification: Option<ReplacementQualificationAssessment>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
enum PersistedPreparedTargetContractV2 {
    ExistingExpectedIdentity(PreparedWaveformRestore),
    AbsentFinalNoReplace(PreparedAbsentFinalNoReplace),
}

impl From<PersistedPreparedTargetContractV2> for PreparedTargetContract {
    fn from(contract: PersistedPreparedTargetContractV2) -> Self {
        match contract {
            PersistedPreparedTargetContractV2::ExistingExpectedIdentity(prepared) => {
                Self::ExistingExpectedIdentity(prepared)
            }
            PersistedPreparedTargetContractV2::AbsentFinalNoReplace(prepared) => {
                Self::AbsentFinalNoReplace(prepared)
            }
        }
    }
}

impl From<PersistedOperationRecordV2> for StoredOperationRecord {
    fn from(record: PersistedOperationRecordV2) -> Self {
        Self {
            schema_version: record.schema_version,
            operation_id: OperationId::from_uuid(record.operation_id),
            intent: record.intent,
            universal_intent: None,
            phase: record.phase,
            disposition: record.disposition,
            payload: record.payload,
            capacity_plan: record.capacity_plan,
            prepared: record.prepared.map(Into::into),
            staged: record.staged,
            absent_final_recovery_observation: record.absent_final_recovery_observation,
            absent_final_transaction_owned_proof: record.absent_final_transaction_owned_proof,
            absent_final_adoption_evidence: record.absent_final_adoption_evidence,
            published: record.published,
            replacement_qualification: record.replacement_qualification,
            cancellation_request: None,
            command_args: None,
            retry_observation: None,
            universal_command_args: None,
            created_unix_ms: record.created_unix_ms,
            updated_unix_ms: record.updated_unix_ms,
        }
    }
}

fn persisted_v2_from_record(
    record: &StoredOperationRecord,
) -> Result<PersistedOperationRecordV2Ref<'_>, EncodeError> {
    if record.universal_intent.is_some() {
        return Err(EncodeError::UniversalIntentInLegacy {
            version: record.schema_version,
        });
    }
    if record.cancellation_request.is_some() {
        return Err(EncodeError::CancellationRequestInLegacy {
            version: record.schema_version,
        });
    }
    if record.command_args.is_some() {
        return Err(EncodeError::CoordinatorFieldsInPriorSchema {
            version: record.schema_version,
            kind: "typed command arguments",
        });
    }
    if record.universal_command_args.is_some() {
        return Err(EncodeError::CoordinatorFieldsInPriorSchema {
            version: record.schema_version,
            kind: "V6 tagged command arguments",
        });
    }
    if record.retry_observation.is_some() {
        return Err(EncodeError::CoordinatorFieldsInPriorSchema {
            version: record.schema_version,
            kind: "retry observation",
        });
    }
    Ok(PersistedOperationRecordV2Ref {
        schema_version: record.schema_version,
        operation_id: record.operation_id.as_uuid(),
        intent: &record.intent,
        phase: record.phase,
        disposition: record.disposition,
        payload: &record.payload,
        capacity_plan: record.capacity_plan.as_ref(),
        prepared: record.prepared.as_ref().map(|contract| match contract {
            PreparedTargetContract::ExistingExpectedIdentity(prepared) => {
                PersistedPreparedTargetContractV2Ref::ExistingExpectedIdentity(prepared)
            }
            PreparedTargetContract::AbsentFinalNoReplace(prepared) => {
                PersistedPreparedTargetContractV2Ref::AbsentFinalNoReplace(prepared)
            }
        }),
        staged: record.staged.as_ref(),
        absent_final_recovery_observation: record.absent_final_recovery_observation.as_ref(),
        absent_final_transaction_owned_proof: record.absent_final_transaction_owned_proof.as_ref(),
        absent_final_adoption_evidence: record.absent_final_adoption_evidence.as_ref(),
        published: record.published.as_ref(),
        replacement_qualification: record.replacement_qualification.as_ref(),
        created_unix_ms: record.created_unix_ms,
        updated_unix_ms: record.updated_unix_ms,
    })
}

#[derive(Serialize)]
struct PersistedOperationRecordV2Ref<'a> {
    schema_version: u32,
    operation_id: Uuid,
    intent: &'a OperationIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    payload: &'a Value,
    capacity_plan: Option<&'a DurableCapacityPlan>,
    prepared: Option<PersistedPreparedTargetContractV2Ref<'a>>,
    staged: Option<&'a FilesystemStagedWaveformRestore>,
    absent_final_recovery_observation: Option<&'a AbsentFinalRecoveryObservation>,
    absent_final_transaction_owned_proof: Option<&'a AbsentFinalTransactionOwnedProof>,
    absent_final_adoption_evidence: Option<&'a AbsentFinalAdoptionEvidence>,
    published: Option<&'a FilesystemPublishedWaveformRestore>,
    replacement_qualification: Option<&'a ReplacementQualificationAssessment>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

#[derive(Serialize)]
enum PersistedPreparedTargetContractV2Ref<'a> {
    ExistingExpectedIdentity(&'a PreparedWaveformRestore),
    AbsentFinalNoReplace(&'a PreparedAbsentFinalNoReplace),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedOperationRecordV3 {
    schema_version: u32,
    operation_id: Uuid,
    intent: OperationIntent,
    universal_intent: UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    payload: Value,
    capacity_plan: DurableCapacityPlan,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

impl From<PersistedOperationRecordV3> for StoredOperationRecord {
    fn from(record: PersistedOperationRecordV3) -> Self {
        Self {
            schema_version: record.schema_version,
            operation_id: OperationId::from_uuid(record.operation_id),
            intent: record.intent,
            universal_intent: Some(record.universal_intent),
            phase: record.phase,
            disposition: record.disposition,
            payload: record.payload,
            capacity_plan: Some(record.capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: None,
            command_args: None,
            retry_observation: None,
            universal_command_args: None,
            created_unix_ms: record.created_unix_ms,
            updated_unix_ms: record.updated_unix_ms,
        }
    }
}

fn persisted_v3_from_record(
    record: &StoredOperationRecord,
) -> Result<PersistedOperationRecordV3Ref<'_>, EncodeError> {
    if record.universal_intent.is_none() {
        return Err(EncodeError::MissingV3UniversalIntent);
    }
    for (present, kind) in [
        (record.prepared.is_some(), "prepared"),
        (record.staged.is_some(), "staged"),
        (record.published().is_some(), "published"),
        (
            record.absent_final_recovery_observation.is_some(),
            "absent-final recovery observation",
        ),
        (
            record.absent_final_transaction_owned_proof.is_some(),
            "absent-final transaction-owned proof",
        ),
        (
            record.absent_final_adoption_evidence.is_some(),
            "absent-final adoption evidence",
        ),
        (
            record.replacement_qualification.is_some(),
            "replacement qualification",
        ),
        (
            record.cancellation_request.is_some(),
            "cancellation request",
        ),
    ] {
        if present {
            return Err(EncodeError::LegacyEvidenceInV3 { kind });
        }
    }
    if record.command_args.is_some() {
        return Err(EncodeError::CoordinatorFieldsInPriorSchema {
            version: record.schema_version,
            kind: "typed command arguments",
        });
    }
    if record.universal_command_args.is_some() {
        return Err(EncodeError::CoordinatorFieldsInPriorSchema {
            version: record.schema_version,
            kind: "V6 tagged command arguments",
        });
    }
    if record.retry_observation.is_some() {
        return Err(EncodeError::CoordinatorFieldsInPriorSchema {
            version: record.schema_version,
            kind: "retry observation",
        });
    }
    if record.phase != OperationPhase::IntentDurable {
        return Err(EncodeError::InvalidV3Record {
            reason: format!(
                "schema-v3 foundation only encodes IntentDurable, found {:?}",
                record.phase
            ),
        });
    }
    if record.disposition != OperationDisposition::None {
        return Err(EncodeError::InvalidV3Record {
            reason: String::from("schema-v3 initial records require the None disposition"),
        });
    }
    if record.capacity_plan.is_none() {
        return Err(EncodeError::InvalidV3Record {
            reason: String::from("schema-v3 initial records require a capacity plan"),
        });
    }
    validate_v3_initial_shape(record).map_err(|reason| EncodeError::InvalidV3Record { reason })?;
    Ok(PersistedOperationRecordV3Ref {
        schema_version: record.schema_version,
        operation_id: record.operation_id.as_uuid(),
        intent: &record.intent,
        universal_intent: record
            .universal_intent
            .as_ref()
            .expect("V3 universal intent checked above"),
        phase: record.phase,
        disposition: record.disposition,
        payload: &record.payload,
        capacity_plan: record
            .capacity_plan
            .as_ref()
            .expect("V3 capacity plan checked above"),
        created_unix_ms: record.created_unix_ms,
        updated_unix_ms: record.updated_unix_ms,
    })
}

#[derive(Serialize)]
struct PersistedOperationRecordV3Ref<'a> {
    schema_version: u32,
    operation_id: Uuid,
    intent: &'a OperationIntent,
    universal_intent: &'a UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    payload: &'a Value,
    capacity_plan: &'a DurableCapacityPlan,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedOperationRecordV4 {
    schema_version: u32,
    operation_id: Uuid,
    intent: OperationIntent,
    universal_intent: UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    payload: Value,
    capacity_plan: DurableCapacityPlan,
    cancellation_request: CancellationRequestObservation,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

impl From<PersistedOperationRecordV4> for StoredOperationRecord {
    fn from(record: PersistedOperationRecordV4) -> Self {
        Self {
            schema_version: record.schema_version,
            operation_id: OperationId::from_uuid(record.operation_id),
            intent: record.intent,
            universal_intent: Some(record.universal_intent),
            phase: record.phase,
            disposition: record.disposition,
            payload: record.payload,
            capacity_plan: Some(record.capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: Some(record.cancellation_request),
            command_args: None,
            retry_observation: None,
            universal_command_args: None,
            created_unix_ms: record.created_unix_ms,
            updated_unix_ms: record.updated_unix_ms,
        }
    }
}

fn persisted_v4_from_record(
    record: &StoredOperationRecord,
) -> Result<PersistedOperationRecordV4Ref<'_>, EncodeError> {
    if record.command_args.is_some() {
        return Err(EncodeError::CoordinatorFieldsInPriorSchema {
            version: record.schema_version,
            kind: "typed command arguments",
        });
    }
    if record.universal_command_args.is_some() {
        return Err(EncodeError::CoordinatorFieldsInPriorSchema {
            version: record.schema_version,
            kind: "V6 tagged command arguments",
        });
    }
    if record.retry_observation.is_some() {
        return Err(EncodeError::CoordinatorFieldsInPriorSchema {
            version: record.schema_version,
            kind: "retry observation",
        });
    }
    validate_v4_cancel_shape(record).map_err(|reason| EncodeError::InvalidV4Record { reason })?;
    Ok(PersistedOperationRecordV4Ref {
        schema_version: record.schema_version,
        operation_id: record.operation_id.as_uuid(),
        intent: &record.intent,
        universal_intent: record
            .universal_intent
            .as_ref()
            .expect("V4 universal intent checked above"),
        phase: record.phase,
        disposition: record.disposition,
        payload: &record.payload,
        capacity_plan: record
            .capacity_plan
            .as_ref()
            .expect("V4 capacity plan checked above"),
        cancellation_request: record
            .cancellation_request
            .as_ref()
            .expect("V4 cancellation request checked above"),
        created_unix_ms: record.created_unix_ms,
        updated_unix_ms: record.updated_unix_ms,
    })
}

#[derive(Serialize)]
struct PersistedOperationRecordV4Ref<'a> {
    schema_version: u32,
    operation_id: Uuid,
    intent: &'a OperationIntent,
    universal_intent: &'a UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    payload: &'a Value,
    capacity_plan: &'a DurableCapacityPlan,
    cancellation_request: &'a CancellationRequestObservation,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedOperationRecordV5 {
    schema_version: u32,
    operation_id: Uuid,
    intent: OperationIntent,
    universal_intent: UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    command_args: CopyIntentArgs,
    capacity_plan: DurableCapacityPlan,
    #[serde(default)]
    retry_observation: Option<RetryObservation>,
    #[serde(default)]
    cancellation_request: Option<CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

impl From<PersistedOperationRecordV5> for StoredOperationRecord {
    fn from(record: PersistedOperationRecordV5) -> Self {
        Self {
            schema_version: record.schema_version,
            operation_id: OperationId::from_uuid(record.operation_id),
            intent: record.intent,
            universal_intent: Some(record.universal_intent),
            phase: record.phase,
            disposition: record.disposition,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(record.capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: record.cancellation_request,
            command_args: Some(record.command_args),
            retry_observation: record.retry_observation,
            universal_command_args: None,
            created_unix_ms: record.created_unix_ms,
            updated_unix_ms: record.updated_unix_ms,
        }
    }
}

fn persisted_v5_from_record(
    record: &StoredOperationRecord,
) -> Result<PersistedOperationRecordV5Ref<'_>, EncodeError> {
    validate_v5_coordinator_shape(record)
        .map_err(|reason| EncodeError::InvalidV5Record { reason })?;
    Ok(PersistedOperationRecordV5Ref {
        schema_version: record.schema_version,
        operation_id: record.operation_id.as_uuid(),
        intent: &record.intent,
        universal_intent: record
            .universal_intent
            .as_ref()
            .expect("V5 universal intent checked above"),
        phase: record.phase,
        disposition: record.disposition,
        command_args: record
            .command_args
            .as_ref()
            .expect("V5 command arguments checked above"),
        capacity_plan: record
            .capacity_plan
            .as_ref()
            .expect("V5 capacity plan checked above"),
        retry_observation: record.retry_observation.as_ref(),
        cancellation_request: record.cancellation_request.as_ref(),
        created_unix_ms: record.created_unix_ms,
        updated_unix_ms: record.updated_unix_ms,
    })
}

#[derive(Serialize)]
struct PersistedOperationRecordV5Ref<'a> {
    schema_version: u32,
    operation_id: Uuid,
    intent: &'a OperationIntent,
    universal_intent: &'a UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    command_args: &'a CopyIntentArgs,
    capacity_plan: &'a DurableCapacityPlan,
    retry_observation: Option<&'a RetryObservation>,
    cancellation_request: Option<&'a CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedOperationRecordV6 {
    schema_version: u32,
    operation_id: Uuid,
    intent: OperationIntent,
    universal_intent: UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    universal_command_args: UniversalCommandArgs,
    capacity_plan: DurableCapacityPlan,
    #[serde(default)]
    retry_observation: Option<RetryObservation>,
    #[serde(default)]
    cancellation_request: Option<CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

impl From<PersistedOperationRecordV6> for StoredOperationRecord {
    fn from(record: PersistedOperationRecordV6) -> Self {
        Self {
            schema_version: record.schema_version,
            operation_id: OperationId::from_uuid(record.operation_id),
            intent: record.intent,
            universal_intent: Some(record.universal_intent),
            phase: record.phase,
            disposition: record.disposition,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(record.capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: record.cancellation_request,
            command_args: None,
            retry_observation: record.retry_observation,
            universal_command_args: Some(record.universal_command_args),
            created_unix_ms: record.created_unix_ms,
            updated_unix_ms: record.updated_unix_ms,
        }
    }
}

fn persisted_v6_from_record(
    record: &StoredOperationRecord,
) -> Result<PersistedOperationRecordV6Ref<'_>, EncodeError> {
    validate_v6_coordinator_shape(record)
        .map_err(|reason| EncodeError::InvalidV6Record { reason })?;
    Ok(PersistedOperationRecordV6Ref {
        schema_version: record.schema_version,
        operation_id: record.operation_id.as_uuid(),
        intent: &record.intent,
        universal_intent: record
            .universal_intent
            .as_ref()
            .expect("V6 universal intent checked above"),
        phase: record.phase,
        disposition: record.disposition,
        universal_command_args: record
            .universal_command_args
            .as_ref()
            .expect("V6 command arguments checked above"),
        capacity_plan: record
            .capacity_plan
            .as_ref()
            .expect("V6 capacity plan checked above"),
        retry_observation: record.retry_observation.as_ref(),
        cancellation_request: record.cancellation_request.as_ref(),
        created_unix_ms: record.created_unix_ms,
        updated_unix_ms: record.updated_unix_ms,
    })
}

#[derive(Serialize)]
struct PersistedOperationRecordV6Ref<'a> {
    schema_version: u32,
    operation_id: Uuid,
    intent: &'a OperationIntent,
    universal_intent: &'a UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    universal_command_args: &'a UniversalCommandArgs,
    capacity_plan: &'a DurableCapacityPlan,
    retry_observation: Option<&'a RetryObservation>,
    cancellation_request: Option<&'a CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedOperationRecordV7 {
    schema_version: u32,
    operation_id: Uuid,
    intent: OperationIntent,
    universal_intent: UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    universal_command_args: UniversalCommandArgs,
    capacity_plan: DurableCapacityPlan,
    #[serde(default)]
    retry_observation: Option<RetryObservation>,
    #[serde(default)]
    cancellation_request: Option<CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

impl From<PersistedOperationRecordV7> for StoredOperationRecord {
    fn from(record: PersistedOperationRecordV7) -> Self {
        Self {
            schema_version: record.schema_version,
            operation_id: OperationId::from_uuid(record.operation_id),
            intent: record.intent,
            universal_intent: Some(record.universal_intent),
            phase: record.phase,
            disposition: record.disposition,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(record.capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: record.cancellation_request,
            command_args: None,
            retry_observation: record.retry_observation,
            universal_command_args: Some(record.universal_command_args),
            created_unix_ms: record.created_unix_ms,
            updated_unix_ms: record.updated_unix_ms,
        }
    }
}

fn persisted_v7_from_record(
    record: &StoredOperationRecord,
) -> Result<PersistedOperationRecordV7Ref<'_>, EncodeError> {
    validate_v7_coordinator_shape(record)
        .map_err(|reason| EncodeError::InvalidV7Record { reason })?;
    Ok(PersistedOperationRecordV7Ref {
        schema_version: record.schema_version,
        operation_id: record.operation_id.as_uuid(),
        intent: &record.intent,
        universal_intent: record
            .universal_intent
            .as_ref()
            .expect("V7 universal intent checked above"),
        phase: record.phase,
        disposition: record.disposition,
        universal_command_args: record
            .universal_command_args
            .as_ref()
            .expect("V7 command arguments checked above"),
        capacity_plan: record
            .capacity_plan
            .as_ref()
            .expect("V7 capacity plan checked above"),
        retry_observation: record.retry_observation.as_ref(),
        cancellation_request: record.cancellation_request.as_ref(),
        created_unix_ms: record.created_unix_ms,
        updated_unix_ms: record.updated_unix_ms,
    })
}

#[derive(Serialize)]
struct PersistedOperationRecordV7Ref<'a> {
    schema_version: u32,
    operation_id: Uuid,
    intent: &'a OperationIntent,
    universal_intent: &'a UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    universal_command_args: &'a UniversalCommandArgs,
    capacity_plan: &'a DurableCapacityPlan,
    retry_observation: Option<&'a RetryObservation>,
    cancellation_request: Option<&'a CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedOperationRecordV8 {
    schema_version: u32,
    operation_id: Uuid,
    intent: OperationIntent,
    universal_intent: UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    universal_command_args: UniversalCommandArgs,
    capacity_plan: DurableCapacityPlan,
    #[serde(default)]
    retry_observation: Option<RetryObservation>,
    #[serde(default)]
    cancellation_request: Option<CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

impl From<PersistedOperationRecordV8> for StoredOperationRecord {
    fn from(record: PersistedOperationRecordV8) -> Self {
        Self {
            schema_version: record.schema_version,
            operation_id: OperationId::from_uuid(record.operation_id),
            intent: record.intent,
            universal_intent: Some(record.universal_intent),
            phase: record.phase,
            disposition: record.disposition,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(record.capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: record.cancellation_request,
            command_args: None,
            retry_observation: record.retry_observation,
            universal_command_args: Some(record.universal_command_args),
            created_unix_ms: record.created_unix_ms,
            updated_unix_ms: record.updated_unix_ms,
        }
    }
}

fn persisted_v8_from_record(
    record: &StoredOperationRecord,
) -> Result<PersistedOperationRecordV8Ref<'_>, EncodeError> {
    validate_v8_coordinator_shape(record)
        .map_err(|reason| EncodeError::InvalidV8Record { reason })?;
    Ok(PersistedOperationRecordV8Ref {
        schema_version: record.schema_version,
        operation_id: record.operation_id.as_uuid(),
        intent: &record.intent,
        universal_intent: record
            .universal_intent
            .as_ref()
            .expect("V8 universal intent checked above"),
        phase: record.phase,
        disposition: record.disposition,
        universal_command_args: record
            .universal_command_args
            .as_ref()
            .expect("V8 command arguments checked above"),
        capacity_plan: record
            .capacity_plan
            .as_ref()
            .expect("V8 capacity plan checked above"),
        retry_observation: record.retry_observation.as_ref(),
        cancellation_request: record.cancellation_request.as_ref(),
        created_unix_ms: record.created_unix_ms,
        updated_unix_ms: record.updated_unix_ms,
    })
}

#[derive(Serialize)]
struct PersistedOperationRecordV8Ref<'a> {
    schema_version: u32,
    operation_id: Uuid,
    intent: &'a OperationIntent,
    universal_intent: &'a UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    universal_command_args: &'a UniversalCommandArgs,
    capacity_plan: &'a DurableCapacityPlan,
    retry_observation: Option<&'a RetryObservation>,
    cancellation_request: Option<&'a CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedOperationRecordV9 {
    schema_version: u32,
    operation_id: Uuid,
    intent: OperationIntent,
    universal_intent: UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    universal_command_args: UniversalCommandArgs,
    capacity_plan: DurableCapacityPlan,
    #[serde(default)]
    retry_observation: Option<RetryObservation>,
    #[serde(default)]
    cancellation_request: Option<CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

impl From<PersistedOperationRecordV9> for StoredOperationRecord {
    fn from(record: PersistedOperationRecordV9) -> Self {
        Self {
            schema_version: record.schema_version,
            operation_id: OperationId::from_uuid(record.operation_id),
            intent: record.intent,
            universal_intent: Some(record.universal_intent),
            phase: record.phase,
            disposition: record.disposition,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(record.capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: record.cancellation_request,
            command_args: None,
            retry_observation: record.retry_observation,
            universal_command_args: Some(record.universal_command_args),
            created_unix_ms: record.created_unix_ms,
            updated_unix_ms: record.updated_unix_ms,
        }
    }
}

fn persisted_v9_from_record(
    record: &StoredOperationRecord,
) -> Result<PersistedOperationRecordV9Ref<'_>, EncodeError> {
    validate_v9_coordinator_shape(record)
        .map_err(|reason| EncodeError::InvalidV9Record { reason })?;
    Ok(PersistedOperationRecordV9Ref {
        schema_version: record.schema_version,
        operation_id: record.operation_id.as_uuid(),
        intent: &record.intent,
        universal_intent: record
            .universal_intent
            .as_ref()
            .expect("V9 universal intent checked above"),
        phase: record.phase,
        disposition: record.disposition,
        universal_command_args: record
            .universal_command_args
            .as_ref()
            .expect("V9 command arguments checked above"),
        capacity_plan: record
            .capacity_plan
            .as_ref()
            .expect("V9 capacity plan checked above"),
        retry_observation: record.retry_observation.as_ref(),
        cancellation_request: record.cancellation_request.as_ref(),
        created_unix_ms: record.created_unix_ms,
        updated_unix_ms: record.updated_unix_ms,
    })
}

#[derive(Serialize)]
struct PersistedOperationRecordV9Ref<'a> {
    schema_version: u32,
    operation_id: Uuid,
    intent: &'a OperationIntent,
    universal_intent: &'a UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    universal_command_args: &'a UniversalCommandArgs,
    capacity_plan: &'a DurableCapacityPlan,
    retry_observation: Option<&'a RetryObservation>,
    cancellation_request: Option<&'a CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedOperationRecordV10 {
    schema_version: u32,
    operation_id: Uuid,
    intent: OperationIntent,
    universal_intent: UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    universal_command_args: UniversalCommandArgs,
    capacity_plan: DurableCapacityPlan,
    #[serde(default)]
    retry_observation: Option<RetryObservation>,
    #[serde(default)]
    cancellation_request: Option<CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

impl From<PersistedOperationRecordV10> for StoredOperationRecord {
    fn from(record: PersistedOperationRecordV10) -> Self {
        Self {
            schema_version: record.schema_version,
            operation_id: OperationId::from_uuid(record.operation_id),
            intent: record.intent,
            universal_intent: Some(record.universal_intent),
            phase: record.phase,
            disposition: record.disposition,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(record.capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: record.cancellation_request,
            command_args: None,
            retry_observation: record.retry_observation,
            universal_command_args: Some(record.universal_command_args),
            created_unix_ms: record.created_unix_ms,
            updated_unix_ms: record.updated_unix_ms,
        }
    }
}

fn persisted_v10_from_record(
    record: &StoredOperationRecord,
) -> Result<PersistedOperationRecordV10Ref<'_>, EncodeError> {
    validate_v10_coordinator_shape(record)
        .map_err(|reason| EncodeError::InvalidV10Record { reason })?;
    Ok(PersistedOperationRecordV10Ref {
        schema_version: record.schema_version,
        operation_id: record.operation_id.as_uuid(),
        intent: &record.intent,
        universal_intent: record
            .universal_intent
            .as_ref()
            .expect("V10 universal intent checked above"),
        phase: record.phase,
        disposition: record.disposition,
        universal_command_args: record
            .universal_command_args
            .as_ref()
            .expect("V10 command arguments checked above"),
        capacity_plan: record
            .capacity_plan
            .as_ref()
            .expect("V10 capacity plan checked above"),
        retry_observation: record.retry_observation.as_ref(),
        cancellation_request: record.cancellation_request.as_ref(),
        created_unix_ms: record.created_unix_ms,
        updated_unix_ms: record.updated_unix_ms,
    })
}

#[derive(Serialize)]
struct PersistedOperationRecordV10Ref<'a> {
    schema_version: u32,
    operation_id: Uuid,
    intent: &'a OperationIntent,
    universal_intent: &'a UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    universal_command_args: &'a UniversalCommandArgs,
    capacity_plan: &'a DurableCapacityPlan,
    retry_observation: Option<&'a RetryObservation>,
    cancellation_request: Option<&'a CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedOperationRecordV11 {
    schema_version: u32,
    operation_id: Uuid,
    intent: OperationIntent,
    universal_intent: UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    universal_command_args: UniversalCommandArgs,
    capacity_plan: DurableCapacityPlan,
    #[serde(default)]
    retry_observation: Option<RetryObservation>,
    #[serde(default)]
    cancellation_request: Option<CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

impl From<PersistedOperationRecordV11> for StoredOperationRecord {
    fn from(record: PersistedOperationRecordV11) -> Self {
        Self {
            schema_version: record.schema_version,
            operation_id: OperationId::from_uuid(record.operation_id),
            intent: record.intent,
            universal_intent: Some(record.universal_intent),
            phase: record.phase,
            disposition: record.disposition,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(record.capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: record.cancellation_request,
            command_args: None,
            retry_observation: record.retry_observation,
            universal_command_args: Some(record.universal_command_args),
            created_unix_ms: record.created_unix_ms,
            updated_unix_ms: record.updated_unix_ms,
        }
    }
}

fn persisted_v11_from_record(
    record: &StoredOperationRecord,
) -> Result<PersistedOperationRecordV11Ref<'_>, EncodeError> {
    validate_v11_coordinator_shape(record)
        .map_err(|reason| EncodeError::InvalidV11Record { reason })?;
    Ok(PersistedOperationRecordV11Ref {
        schema_version: record.schema_version,
        operation_id: record.operation_id.as_uuid(),
        intent: &record.intent,
        universal_intent: record
            .universal_intent
            .as_ref()
            .expect("V11 universal intent checked above"),
        phase: record.phase,
        disposition: record.disposition,
        universal_command_args: record
            .universal_command_args
            .as_ref()
            .expect("V11 command arguments checked above"),
        capacity_plan: record
            .capacity_plan
            .as_ref()
            .expect("V11 capacity plan checked above"),
        retry_observation: record.retry_observation.as_ref(),
        cancellation_request: record.cancellation_request.as_ref(),
        created_unix_ms: record.created_unix_ms,
        updated_unix_ms: record.updated_unix_ms,
    })
}

#[derive(Serialize)]
struct PersistedOperationRecordV11Ref<'a> {
    schema_version: u32,
    operation_id: Uuid,
    intent: &'a OperationIntent,
    universal_intent: &'a UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    universal_command_args: &'a UniversalCommandArgs,
    capacity_plan: &'a DurableCapacityPlan,
    retry_observation: Option<&'a RetryObservation>,
    cancellation_request: Option<&'a CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedOperationRecordV12 {
    schema_version: u32,
    operation_id: Uuid,
    intent: OperationIntent,
    universal_intent: UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    universal_command_args: UniversalCommandArgs,
    capacity_plan: DurableCapacityPlan,
    #[serde(default)]
    retry_observation: Option<RetryObservation>,
    #[serde(default)]
    cancellation_request: Option<CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

impl From<PersistedOperationRecordV12> for StoredOperationRecord {
    fn from(record: PersistedOperationRecordV12) -> Self {
        Self {
            schema_version: record.schema_version,
            operation_id: OperationId::from_uuid(record.operation_id),
            intent: record.intent,
            universal_intent: Some(record.universal_intent),
            phase: record.phase,
            disposition: record.disposition,
            payload: Value::Object(Map::new()),
            capacity_plan: Some(record.capacity_plan),
            prepared: None,
            staged: None,
            absent_final_recovery_observation: None,
            absent_final_transaction_owned_proof: None,
            absent_final_adoption_evidence: None,
            published: None,
            replacement_qualification: None,
            cancellation_request: record.cancellation_request,
            command_args: None,
            retry_observation: record.retry_observation,
            universal_command_args: Some(record.universal_command_args),
            created_unix_ms: record.created_unix_ms,
            updated_unix_ms: record.updated_unix_ms,
        }
    }
}

fn persisted_v12_from_record(
    record: &StoredOperationRecord,
) -> Result<PersistedOperationRecordV12Ref<'_>, EncodeError> {
    validate_v12_coordinator_shape(record)
        .map_err(|reason| EncodeError::InvalidV12Record { reason })?;
    Ok(PersistedOperationRecordV12Ref {
        schema_version: record.schema_version,
        operation_id: record.operation_id.as_uuid(),
        intent: &record.intent,
        universal_intent: record
            .universal_intent
            .as_ref()
            .expect("V12 universal intent checked above"),
        phase: record.phase,
        disposition: record.disposition,
        universal_command_args: record
            .universal_command_args
            .as_ref()
            .expect("V12 command arguments checked above"),
        capacity_plan: record
            .capacity_plan
            .as_ref()
            .expect("V12 capacity plan checked above"),
        retry_observation: record.retry_observation.as_ref(),
        cancellation_request: record.cancellation_request.as_ref(),
        created_unix_ms: record.created_unix_ms,
        updated_unix_ms: record.updated_unix_ms,
    })
}

#[derive(Serialize)]
struct PersistedOperationRecordV12Ref<'a> {
    schema_version: u32,
    operation_id: Uuid,
    intent: &'a OperationIntent,
    universal_intent: &'a UniversalIntent,
    phase: OperationPhase,
    disposition: OperationDisposition,
    universal_command_args: &'a UniversalCommandArgs,
    capacity_plan: &'a DurableCapacityPlan,
    retry_observation: Option<&'a RetryObservation>,
    cancellation_request: Option<&'a CancellationRequestObservation>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

/// Decode one bounded persisted operation record.
///
/// The input is checked against [`MAX_RECORD_BYTES`] before JSON parsing. Unknown schema versions
/// and malformed records are returned as distinct errors so the caller can retain the original
/// bytes and surface an actionable recovery state.
pub fn decode(bytes: &[u8]) -> Result<StoredOperationRecord, DecodeError> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(DecodeError::Oversize {
            actual: bytes.len(),
            maximum: MAX_RECORD_BYTES,
        });
    }
    let value = parse_unique_value(bytes).map_err(|error| DecodeError::Malformed {
        reason: format!("invalid JSON: {error}"),
    })?;
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| DecodeError::Malformed {
            reason: String::from("schema_version must be an unsigned 32-bit integer"),
        })?;
    match version {
        SCHEMA_V1 => {
            validate_persisted_v1_object_boundaries(&value).map_err(malformed)?;
            validate_v1_publication_applicability(&value).map_err(malformed)?;
            serde_json::from_value::<PersistedOperationRecordV1>(value)
                .map(Into::into)
                .map_err(|error| malformed(error.to_string()))
        }
        SCHEMA_V2 => {
            validate_persisted_v2_object_boundaries(&value).map_err(malformed)?;
            serde_json::from_value::<PersistedOperationRecordV2>(value)
                .map(Into::into)
                .map_err(|error| malformed(error.to_string()))
        }
        SCHEMA_V3 => {
            validate_json_depth(&value, MAX_V3_JSON_DEPTH).map_err(malformed)?;
            validate_persisted_v3_object_boundaries(&value).map_err(malformed)?;
            let record = serde_json::from_value::<PersistedOperationRecordV3>(value)
                .map(Into::into)
                .map_err(|error| malformed(error.to_string()))?;
            validate_v3_initial_shape(&record).map_err(malformed)?;
            Ok(record)
        }
        SCHEMA_V4 => {
            validate_json_depth(&value, MAX_V3_JSON_DEPTH).map_err(malformed)?;
            validate_persisted_v4_object_boundaries(&value).map_err(malformed)?;
            let record = serde_json::from_value::<PersistedOperationRecordV4>(value)
                .map(Into::into)
                .map_err(|error| malformed(error.to_string()))?;
            validate_v4_cancel_shape(&record).map_err(malformed)?;
            Ok(record)
        }
        SCHEMA_V5 => {
            validate_json_depth(&value, MAX_V3_JSON_DEPTH).map_err(malformed)?;
            validate_persisted_v5_object_boundaries(&value).map_err(malformed)?;
            let record = serde_json::from_value::<PersistedOperationRecordV5>(value)
                .map(Into::into)
                .map_err(|error| malformed(error.to_string()))?;
            validate_v5_coordinator_shape(&record).map_err(malformed)?;
            Ok(record)
        }
        SCHEMA_V6 => {
            validate_json_depth(&value, MAX_V3_JSON_DEPTH).map_err(malformed)?;
            validate_persisted_v6_object_boundaries(&value).map_err(malformed)?;
            let record = serde_json::from_value::<PersistedOperationRecordV6>(value)
                .map(Into::into)
                .map_err(|error| malformed(error.to_string()))?;
            validate_v6_coordinator_shape(&record).map_err(malformed)?;
            Ok(record)
        }
        SCHEMA_V7 => {
            validate_json_depth(&value, MAX_V3_JSON_DEPTH).map_err(malformed)?;
            validate_persisted_v7_object_boundaries(&value).map_err(malformed)?;
            let record = serde_json::from_value::<PersistedOperationRecordV7>(value)
                .map(Into::into)
                .map_err(|error| malformed(error.to_string()))?;
            validate_v7_coordinator_shape(&record).map_err(malformed)?;
            Ok(record)
        }
        SCHEMA_V8 => {
            validate_json_depth(&value, MAX_V3_JSON_DEPTH).map_err(malformed)?;
            validate_persisted_v8_object_boundaries(&value).map_err(malformed)?;
            let record = serde_json::from_value::<PersistedOperationRecordV8>(value)
                .map(Into::into)
                .map_err(|error| malformed(error.to_string()))?;
            validate_v8_coordinator_shape(&record).map_err(malformed)?;
            Ok(record)
        }
        SCHEMA_V9 => {
            validate_json_depth(&value, MAX_V3_JSON_DEPTH).map_err(malformed)?;
            validate_persisted_v9_object_boundaries(&value).map_err(malformed)?;
            let record = serde_json::from_value::<PersistedOperationRecordV9>(value)
                .map(Into::into)
                .map_err(|error| malformed(error.to_string()))?;
            validate_v9_coordinator_shape(&record).map_err(malformed)?;
            Ok(record)
        }
        SCHEMA_V10 => {
            validate_json_depth(&value, MAX_V3_JSON_DEPTH).map_err(malformed)?;
            validate_persisted_v10_object_boundaries(&value).map_err(malformed)?;
            let record = serde_json::from_value::<PersistedOperationRecordV10>(value)
                .map(Into::into)
                .map_err(|error| malformed(error.to_string()))?;
            validate_v10_coordinator_shape(&record).map_err(malformed)?;
            Ok(record)
        }
        SCHEMA_V11 => {
            validate_json_depth(&value, MAX_V3_JSON_DEPTH).map_err(malformed)?;
            validate_persisted_v11_object_boundaries(&value).map_err(malformed)?;
            let record = serde_json::from_value::<PersistedOperationRecordV11>(value)
                .map(Into::into)
                .map_err(|error| malformed(error.to_string()))?;
            validate_v11_coordinator_shape(&record).map_err(malformed)?;
            Ok(record)
        }
        SCHEMA_V12 => {
            validate_json_depth(&value, MAX_V3_JSON_DEPTH).map_err(malformed)?;
            validate_persisted_v12_object_boundaries(&value).map_err(malformed)?;
            let record = serde_json::from_value::<PersistedOperationRecordV12>(value)
                .map(Into::into)
                .map_err(|error| malformed(error.to_string()))?;
            validate_v12_coordinator_shape(&record).map_err(malformed)?;
            Ok(record)
        }
        version => Err(DecodeError::UnsupportedVersion { version }),
    }
}

fn malformed(reason: impl Into<String>) -> DecodeError {
    DecodeError::Malformed {
        reason: reason.into(),
    }
}

fn parse_unique_value(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = UniqueValueSeed.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

struct UniqueValueSeed;

impl<'de> DeserializeSeed<'de> for UniqueValueSeed {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueValueVisitor)
    }
}

struct UniqueValueVisitor;

impl<'de> Visitor<'de> for UniqueValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("JSON number is not finite"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        UniqueValueSeed.deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(UniqueValueSeed)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(de::Error::custom(format!(
                    "duplicate JSON object field `{key}`"
                )));
            }
            let value = map.next_value_seed(UniqueValueSeed)?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

/// Encode one stored record using its persisted schema and a capped writer.
///
/// Encoding preserves the exact V1/V2 field and enum representation and emits the explicit V3
/// universal-intent envelope. The V3 branch accepts only the initial durable-intent shape, while
/// V4 and V5 encode their explicitly bounded pre-publication observations; later transition and
/// publication-policy validation belongs to the owner boundary.
pub fn encode(record: &StoredOperationRecord) -> Result<Vec<u8>, EncodeError> {
    let mut writer = CappedWriter::new(MAX_RECORD_BYTES);
    let result = match record.schema_version {
        SCHEMA_V1 => {
            let persisted = persisted_v1_from_record(record)?;
            serde_json::to_writer_pretty(&mut writer, &persisted)
        }
        SCHEMA_V2 => {
            let persisted = persisted_v2_from_record(record)?;
            serde_json::to_writer_pretty(&mut writer, &persisted)
        }
        SCHEMA_V3 => {
            let persisted = persisted_v3_from_record(record)?;
            serde_json::to_writer_pretty(&mut writer, &persisted)
        }
        SCHEMA_V4 => {
            let persisted = persisted_v4_from_record(record)?;
            serde_json::to_writer_pretty(&mut writer, &persisted)
        }
        SCHEMA_V5 => {
            let persisted = persisted_v5_from_record(record)?;
            serde_json::to_writer_pretty(&mut writer, &persisted)
        }
        SCHEMA_V6 => {
            let persisted = persisted_v6_from_record(record)?;
            serde_json::to_writer_pretty(&mut writer, &persisted)
        }
        SCHEMA_V7 => {
            let persisted = persisted_v7_from_record(record)?;
            serde_json::to_writer_pretty(&mut writer, &persisted)
        }
        SCHEMA_V8 => {
            let persisted = persisted_v8_from_record(record)?;
            serde_json::to_writer_pretty(&mut writer, &persisted)
        }
        SCHEMA_V9 => {
            let persisted = persisted_v9_from_record(record)?;
            serde_json::to_writer_pretty(&mut writer, &persisted)
        }
        SCHEMA_V10 => {
            let persisted = persisted_v10_from_record(record)?;
            serde_json::to_writer_pretty(&mut writer, &persisted)
        }
        SCHEMA_V11 => {
            let persisted = persisted_v11_from_record(record)?;
            serde_json::to_writer_pretty(&mut writer, &persisted)
        }
        SCHEMA_V12 => {
            let persisted = persisted_v12_from_record(record)?;
            serde_json::to_writer_pretty(&mut writer, &persisted)
        }
        version => {
            return Err(EncodeError::UnsupportedSchemaVersion { version });
        }
    };
    match result {
        Ok(()) => Ok(writer.finish()),
        Err(_error) if writer.exceeded => Err(EncodeError::Oversize {
            actual: writer.attempted,
            maximum: writer.limit,
        }),
        Err(error) => Err(EncodeError::Serialization(error.to_string())),
    }
}

struct CappedWriter {
    bytes: Vec<u8>,
    attempted: usize,
    limit: usize,
    exceeded: bool,
}

impl CappedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(4096)),
            attempted: 0,
            limit,
            exceeded: false,
        }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for CappedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.attempted = self.attempted.saturating_add(buffer.len());
        let remaining = self.limit.saturating_sub(self.bytes.len());
        if buffer.len() > remaining {
            self.bytes.extend_from_slice(&buffer[..remaining]);
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "operation journal record exceeds the bounded output size",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn is_absent_final_publication(published: &FilesystemPublishedWaveformRestore) -> bool {
    matches!(
        published.final_claim,
        FinalNamespaceClaim::AbsentFinalNoReplace { .. }
    )
}

fn strict_object<'a>(
    value: &'a Value,
    allowed_fields: &[&str],
) -> Result<&'a Map<String, Value>, &'static str> {
    let object = value
        .as_object()
        .ok_or("persisted value must be a JSON object")?;
    if object
        .keys()
        .any(|field| !allowed_fields.contains(&field.as_str()))
    {
        return Err("persisted object contains an unknown field");
    }
    Ok(object)
}

fn tagged_object<'a>(value: &'a Value) -> Result<(&'a str, &'a Value), &'static str> {
    let object = value
        .as_object()
        .ok_or("persisted tagged value must be a JSON object")?;
    if object.len() != 1 {
        return Err("persisted tagged object must contain exactly one variant");
    }
    let (variant, value) = object
        .iter()
        .next()
        .ok_or("persisted tagged object must contain exactly one variant")?;
    Ok((variant.as_str(), value))
}

fn required_field<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a Value, &'static str> {
    object
        .get(field)
        .ok_or("persisted object is missing a required field")
}

fn validate_optional_field(
    object: &Map<String, Value>,
    field: &str,
    validate: fn(&Value) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    if let Some(value) = object.get(field) {
        if !value.is_null() {
            validate(value)?;
        }
    }
    Ok(())
}

fn validate_persisted_intent_value(value: &Value) -> Result<(), &'static str> {
    strict_object(value, &["actor", "kind", "label"])?;
    Ok(())
}

fn validate_persisted_volume_identity_value(value: &Value) -> Result<(), &'static str> {
    strict_object(value, &["device"])?;
    Ok(())
}

fn validate_persisted_capacity_plan_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["volumes"])?;
    let volumes = required_field(object, "volumes")?
        .as_array()
        .ok_or("persisted capacity volumes must be an array")?;
    for volume in volumes {
        let volume = strict_object(
            volume,
            &[
                "identity",
                "allocation_unit",
                "allocation_class",
                "logical_bytes",
                "allocated_bytes",
                "protected_free_bytes",
            ],
        )?;
        validate_persisted_volume_identity_value(required_field(volume, "identity")?)?;
    }
    Ok(())
}

fn validate_persisted_object_identity_value(value: &Value) -> Result<(), &'static str> {
    strict_object(value, &["stable_id", "change_marker", "len"])?;
    Ok(())
}

fn validate_persisted_root_locator_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["path", "identity"])?;
    validate_persisted_object_identity_value(required_field(object, "identity")?)?;
    Ok(())
}

fn validate_persisted_leaf_locator_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["relative_path", "identity"])?;
    validate_persisted_object_identity_value(required_field(object, "identity")?)?;
    Ok(())
}

fn validate_persisted_replace_expected_identity_value(value: &Value) -> Result<(), &'static str> {
    let (variant, value) = tagged_object(value)?;
    if variant != "Existing" {
        return Err("persisted replacement identity contains an unknown variant");
    }
    validate_persisted_object_identity_value(value)
}

fn validate_persisted_file_evidence_value(value: &Value) -> Result<(), &'static str> {
    match value {
        Value::String(_) => Ok(()),
        Value::Object(_) => {
            let (variant, value) = tagged_object(value)?;
            match variant {
                "ContentHash" => value
                    .as_array()
                    .map(|_| ())
                    .ok_or("persisted content hash must be an array"),
                "Metadata" => {
                    strict_object(value, &["len", "modified_ns", "is_dir"])?;
                    Ok(())
                }
                _ => Err("persisted file evidence contains an unknown variant"),
            }
        }
        _ => Err("persisted file evidence must be a string or tagged object"),
    }
}

fn validate_persisted_prepared_evidence_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["target", "backup"])?;
    validate_persisted_file_evidence_value(required_field(object, "target")?)?;
    validate_persisted_file_evidence_value(required_field(object, "backup")?)?;
    Ok(())
}

fn validate_persisted_staging_locator_value(value: &Value) -> Result<(), &'static str> {
    strict_object(value, &["relative_path", "absent"])?;
    Ok(())
}

fn validate_persisted_prepared_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "direction",
            "source_id",
            "source_root",
            "target_root",
            "target",
            "backup_root",
            "backup",
            "replacement",
            "staging",
            "evidence",
        ],
    )?;
    validate_persisted_root_locator_value(required_field(object, "source_root")?)?;
    validate_persisted_root_locator_value(required_field(object, "target_root")?)?;
    validate_persisted_leaf_locator_value(required_field(object, "target")?)?;
    validate_persisted_root_locator_value(required_field(object, "backup_root")?)?;
    validate_persisted_leaf_locator_value(required_field(object, "backup")?)?;
    validate_persisted_replace_expected_identity_value(required_field(object, "replacement")?)?;
    validate_persisted_staging_locator_value(required_field(object, "staging")?)?;
    validate_persisted_prepared_evidence_value(required_field(object, "evidence")?)?;
    Ok(())
}

fn validate_persisted_absent_final_observation_value(value: &Value) -> Result<(), &'static str> {
    match value {
        Value::String(observation) if observation == "ObservedAbsent" => Ok(()),
        Value::String(_) => Err("persisted absent-final observation contains an unknown variant"),
        _ => Err("persisted absent-final observation must be a string"),
    }
}

fn validate_persisted_absent_final_prepared_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "direction",
            "source_id",
            "source_root",
            "target_parent",
            "final_leaf",
            "staging",
            "final_observation",
            "copy_validated_evidence",
        ],
    )?;
    validate_persisted_root_locator_value(required_field(object, "source_root")?)?;
    validate_persisted_root_locator_value(required_field(object, "target_parent")?)?;
    if !required_field(object, "final_leaf")?.is_string() {
        return Err("persisted absent-final leaf must be a string");
    }
    validate_persisted_staging_locator_value(required_field(object, "staging")?)?;
    validate_persisted_absent_final_observation_value(required_field(
        object,
        "final_observation",
    )?)?;
    validate_persisted_file_evidence_value(required_field(object, "copy_validated_evidence")?)?;
    Ok(())
}

fn validate_persisted_absent_final_recovery_observation_value(
    value: &Value,
) -> Result<(), &'static str> {
    validate_persisted_absent_final_evidence_value(value, "persisted absent-final recovery")
}

fn validate_persisted_absent_final_transaction_owned_proof_value(
    value: &Value,
) -> Result<(), &'static str> {
    validate_persisted_absent_final_evidence_value(
        value,
        "persisted absent-final transaction-owned proof",
    )
}

fn validate_persisted_absent_final_adoption_evidence_value(
    value: &Value,
) -> Result<(), &'static str> {
    validate_persisted_absent_final_evidence_value(value, "persisted absent-final adoption")
}

fn validate_persisted_absent_final_evidence_value(
    value: &Value,
    label: &'static str,
) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "target_parent_stable_id",
            "final_stable_id",
            "final_len",
            "final_content",
        ],
    )?;
    for field in ["target_parent_stable_id", "final_stable_id"] {
        if required_field(object, field)?
            .as_str()
            .is_none_or(str::is_empty)
        {
            return Err("persisted absent-final evidence identity must be a non-empty string");
        }
    }
    if !required_field(object, "final_len")?.is_u64() {
        return Err("persisted absent-final evidence final length must be an unsigned integer");
    }
    let final_content = required_field(object, "final_content")?;
    validate_persisted_file_evidence_value(final_content)?;
    let (variant, hash) = tagged_object(final_content)?;
    if variant != "ContentHash" {
        return Err(label);
    }
    let hash = hash
        .as_array()
        .ok_or("persisted absent-final evidence content hash must be an array")?;
    if hash.len() != 32
        || hash
            .iter()
            .any(|byte| byte.as_u64().is_none_or(|byte| byte > 255))
    {
        return Err("persisted absent-final evidence content hash must contain 32 bytes");
    }
    Ok(())
}

fn validate_persisted_prepared_contract_v2_value(value: &Value) -> Result<(), &'static str> {
    let (variant, value) = tagged_object(value)?;
    match variant {
        "ExistingExpectedIdentity" => validate_persisted_prepared_value(value),
        "AbsentFinalNoReplace" => validate_persisted_absent_final_prepared_value(value),
        _ => Err("persisted preparation contract contains an unknown variant"),
    }
}

fn validate_persisted_staged_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["participant"])?;
    let (variant, value) = tagged_object(required_field(object, "participant")?)?;
    if variant != "CopyValidated" {
        return Err("persisted staged participant contains an unknown variant");
    }
    let participant = strict_object(value, &["staging", "evidence"])?;
    validate_persisted_leaf_locator_value(required_field(participant, "staging")?)?;
    validate_persisted_file_evidence_value(required_field(participant, "evidence")?)?;
    Ok(())
}

fn validate_persisted_final_claim_primitive_value(value: &Value) -> Result<(), &'static str> {
    if value.is_string() {
        return Ok(());
    }
    let (variant, value) = tagged_object(value)?;
    if variant != "Unqualified" {
        return Err("persisted final claim primitive contains an unknown variant");
    }
    strict_object(value, &["reason"])?;
    Ok(())
}

fn validate_persisted_capability_scope_value(value: &Value) -> Result<(), &'static str> {
    let (variant, value) = tagged_object(value)?;
    if variant != "TargetParentDescriptor" {
        return Err("persisted capability scope contains an unknown variant");
    }
    let scope = strict_object(value, &["target_parent_identity", "root_path_continuity"])?;
    validate_persisted_object_identity_value(required_field(scope, "target_parent_identity")?)?;
    Ok(())
}

fn validate_persisted_final_claim_value(value: &Value) -> Result<(), &'static str> {
    let (variant, value) = tagged_object(value)?;
    match variant {
        "ExpectedIdentityReplacement" => {
            let claim = strict_object(
                value,
                &["primitive", "result", "expected_target", "displaced_target"],
            )?;
            validate_persisted_final_claim_primitive_value(required_field(claim, "primitive")?)?;
            validate_persisted_object_identity_value(required_field(claim, "expected_target")?)?;
            validate_persisted_object_identity_value(required_field(claim, "displaced_target")?)?;
        }
        "AbsentFinalNoReplace" => {
            let claim = strict_object(value, &["primitive", "result", "capability_scope"])?;
            validate_persisted_final_claim_primitive_value(required_field(claim, "primitive")?)?;
            validate_persisted_capability_scope_value(required_field(claim, "capability_scope")?)?;
        }
        _ => return Err("persisted final claim contains an unknown variant"),
    }
    Ok(())
}

fn validate_persisted_published_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "mode",
            "final_claim",
            "reopened_final",
            "visibility",
            "whole_publication",
            "synchronization",
        ],
    )?;
    validate_persisted_final_claim_value(required_field(object, "final_claim")?)?;
    let reopened = strict_object(
        required_field(object, "reopened_final")?,
        &["identity", "content"],
    )?;
    validate_persisted_object_identity_value(required_field(reopened, "identity")?)?;
    validate_persisted_file_evidence_value(required_field(reopened, "content")?)?;
    Ok(())
}

fn validate_persisted_replacement_qualification_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "platform_family",
            "observed_filesystem",
            "volume",
            "candidate",
            "candidate_assessment",
            "missing_invariant",
            "decision",
            "retry_condition",
        ],
    )?;
    validate_persisted_volume_identity_value(required_field(object, "volume")?)?;
    Ok(())
}

fn validate_persisted_v1_object_boundaries(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "schema_version",
            "operation_id",
            "intent",
            "phase",
            "disposition",
            "payload",
            "capacity_plan",
            "prepared",
            "staged",
            "published",
            "replacement_qualification",
            "created_unix_ms",
            "updated_unix_ms",
        ],
    )?;
    validate_persisted_intent_value(required_field(object, "intent")?)?;
    validate_optional_field(
        object,
        "capacity_plan",
        validate_persisted_capacity_plan_value,
    )?;
    validate_optional_field(object, "prepared", validate_persisted_prepared_value)?;
    validate_optional_field(object, "staged", validate_persisted_staged_value)?;
    validate_optional_field(object, "published", validate_persisted_published_value)?;
    validate_optional_field(
        object,
        "replacement_qualification",
        validate_persisted_replacement_qualification_value,
    )?;
    Ok(())
}

fn validate_v1_publication_applicability(value: &Value) -> Result<(), &'static str> {
    let Some(published) = value.get("published") else {
        return Ok(());
    };
    if published.is_null() {
        return Ok(());
    }
    let published = published
        .as_object()
        .ok_or("persisted publication must be a JSON object")?;
    let (variant, _) = tagged_object(required_field(published, "final_claim")?)?;
    if variant == "AbsentFinalNoReplace" {
        return Err("schema-v1 cannot contain absent-final publication evidence");
    }
    Ok(())
}

fn validate_persisted_v2_object_boundaries(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "schema_version",
            "operation_id",
            "intent",
            "phase",
            "disposition",
            "payload",
            "capacity_plan",
            "prepared",
            "staged",
            "absent_final_recovery_observation",
            "absent_final_transaction_owned_proof",
            "absent_final_adoption_evidence",
            "published",
            "replacement_qualification",
            "created_unix_ms",
            "updated_unix_ms",
        ],
    )?;
    validate_persisted_intent_value(required_field(object, "intent")?)?;
    validate_optional_field(
        object,
        "capacity_plan",
        validate_persisted_capacity_plan_value,
    )?;
    if let Some(prepared) = object.get("prepared") {
        if !prepared.is_null() {
            validate_persisted_prepared_contract_v2_value(prepared)?;
        }
    }
    validate_optional_field(object, "staged", validate_persisted_staged_value)?;
    validate_optional_field(
        object,
        "absent_final_recovery_observation",
        validate_persisted_absent_final_recovery_observation_value,
    )?;
    validate_optional_field(
        object,
        "absent_final_transaction_owned_proof",
        validate_persisted_absent_final_transaction_owned_proof_value,
    )?;
    validate_optional_field(
        object,
        "absent_final_adoption_evidence",
        validate_persisted_absent_final_adoption_evidence_value,
    )?;
    validate_optional_field(object, "published", validate_persisted_published_value)?;
    validate_optional_field(
        object,
        "replacement_qualification",
        validate_persisted_replacement_qualification_value,
    )?;
    Ok(())
}

const MAX_V3_JSON_DEPTH: usize = 64;

fn validate_persisted_v3_object_boundaries(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "schema_version",
            "operation_id",
            "intent",
            "universal_intent",
            "phase",
            "disposition",
            "payload",
            "capacity_plan",
            "created_unix_ms",
            "updated_unix_ms",
        ],
    )?;
    for field in [
        "schema_version",
        "operation_id",
        "intent",
        "universal_intent",
        "phase",
        "disposition",
        "payload",
        "capacity_plan",
        "created_unix_ms",
        "updated_unix_ms",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_intent_value(required_field(object, "intent")?)?;
    validate_persisted_universal_intent_value(required_field(object, "universal_intent")?)?;
    if !required_field(object, "payload")?.is_object() {
        return Err("schema-v3 payload must be a JSON object");
    }
    validate_persisted_capacity_plan_v3_value(required_field(object, "capacity_plan")?)?;
    Ok(())
}

fn validate_persisted_universal_intent_value(value: &Value) -> Result<(), &'static str> {
    validate_persisted_universal_intent_value_with_retained_resolution(value, false)
}

fn validate_persisted_universal_intent_value_with_retained_resolution(
    value: &Value,
    allow_retained_resolution: bool,
) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "parent_operation_id",
            "command_kind",
            "user_status_key",
            "participants",
        ],
    )?;
    let parent = required_field(object, "parent_operation_id")?;
    if !parent.is_null() && !parent.is_string() {
        return Err("schema-v3 parent operation ID must be a UUID string or null");
    }
    let command_kind = required_field(object, "command_kind")?
        .as_str()
        .ok_or("schema-v3 command kind must be a string")?;
    if !allow_retained_resolution && matches!(command_kind, "RestoreRetained" | "PurgeRetained") {
        return Err("persisted universal intent contains schema-v11 retained resolution command");
    }
    let status = required_field(object, "user_status_key")?
        .as_str()
        .ok_or("schema-v3 user status key must be a string")?;
    if status.len() > MAX_USER_STATUS_KEY_BYTES {
        return Err("schema-v3 user status key exceeds its byte bound");
    }
    let participants = strict_object(
        required_field(object, "participants")?,
        &["descriptors", "checkpoints"],
    )?;
    for field in ["descriptors", "checkpoints"] {
        let values = required_field(participants, field)?
            .as_array()
            .ok_or("schema-v3 participant collection must be an array")?;
        if values.len() > MAX_PARTICIPANTS {
            return Err("schema-v3 participant collection exceeds its bound");
        }
        if values.iter().any(|value| !value.is_object()) {
            return Err("schema-v3 participant entries must be JSON objects");
        }
    }
    Ok(())
}

fn validate_persisted_capacity_plan_v3_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["volumes"])?;
    let volumes = required_field(object, "volumes")?
        .as_array()
        .ok_or("schema-v3 capacity volumes must be an array")?;
    for volume in volumes {
        let volume = strict_object(
            volume,
            &[
                "identity",
                "allocation_unit",
                "allocation_class",
                "logical_bytes",
                "allocated_bytes",
                "protected_free_bytes",
            ],
        )?;
        for field in [
            "identity",
            "allocation_unit",
            "allocation_class",
            "logical_bytes",
            "allocated_bytes",
            "protected_free_bytes",
        ] {
            required_field(volume, field)?;
        }
        validate_persisted_volume_identity_value(required_field(volume, "identity")?)?;
    }
    Ok(())
}

fn validate_persisted_cancellation_request_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["requested_unix_ms"])?;
    let timestamp = required_field(object, "requested_unix_ms")?
        .as_i64()
        .ok_or("cancellation request timestamp must be a signed 64-bit integer")?;
    if timestamp < 0 {
        return Err("cancellation request timestamp must be non-negative");
    }
    Ok(())
}

fn validate_persisted_v4_object_boundaries(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "schema_version",
            "operation_id",
            "intent",
            "universal_intent",
            "phase",
            "disposition",
            "payload",
            "capacity_plan",
            "cancellation_request",
            "created_unix_ms",
            "updated_unix_ms",
        ],
    )?;
    for field in [
        "schema_version",
        "operation_id",
        "intent",
        "universal_intent",
        "phase",
        "disposition",
        "payload",
        "capacity_plan",
        "cancellation_request",
        "created_unix_ms",
        "updated_unix_ms",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_intent_value(required_field(object, "intent")?)?;
    validate_persisted_universal_intent_value(required_field(object, "universal_intent")?)?;
    if !required_field(object, "payload")?.is_object() {
        return Err("schema-v4 payload must be a JSON object");
    }
    validate_persisted_capacity_plan_v3_value(required_field(object, "capacity_plan")?)?;
    validate_persisted_cancellation_request_value(required_field(object, "cancellation_request")?)?;
    Ok(())
}

fn validate_persisted_v5_object_boundaries(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "schema_version",
            "operation_id",
            "intent",
            "universal_intent",
            "phase",
            "disposition",
            "command_args",
            "capacity_plan",
            "retry_observation",
            "cancellation_request",
            "created_unix_ms",
            "updated_unix_ms",
        ],
    )?;
    for field in [
        "schema_version",
        "operation_id",
        "intent",
        "universal_intent",
        "phase",
        "disposition",
        "command_args",
        "capacity_plan",
        "retry_observation",
        "cancellation_request",
        "created_unix_ms",
        "updated_unix_ms",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_intent_value(required_field(object, "intent")?)?;
    validate_persisted_universal_intent_value(required_field(object, "universal_intent")?)?;
    validate_persisted_copy_intent_value(required_field(object, "command_args")?)?;
    validate_persisted_capacity_plan_v3_value(required_field(object, "capacity_plan")?)?;
    if let Some(retry) = object.get("retry_observation") {
        if !retry.is_null() {
            validate_persisted_retry_observation_value(retry)?;
        }
    }
    if let Some(cancellation) = object.get("cancellation_request") {
        if !cancellation.is_null() {
            validate_persisted_cancellation_request_value(cancellation)?;
        }
    }
    Ok(())
}

fn validate_persisted_v6_object_boundaries(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "schema_version",
            "operation_id",
            "intent",
            "universal_intent",
            "phase",
            "disposition",
            "universal_command_args",
            "capacity_plan",
            "retry_observation",
            "cancellation_request",
            "created_unix_ms",
            "updated_unix_ms",
        ],
    )?;
    for field in [
        "schema_version",
        "operation_id",
        "intent",
        "universal_intent",
        "phase",
        "disposition",
        "universal_command_args",
        "capacity_plan",
        "retry_observation",
        "cancellation_request",
        "created_unix_ms",
        "updated_unix_ms",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_intent_value(required_field(object, "intent")?)?;
    validate_persisted_universal_intent_value(required_field(object, "universal_intent")?)?;
    validate_persisted_universal_command_args_value(required_field(
        object,
        "universal_command_args",
    )?)?;
    validate_persisted_capacity_plan_v3_value(required_field(object, "capacity_plan")?)?;
    if let Some(retry) = object.get("retry_observation") {
        if !retry.is_null() {
            validate_persisted_retry_observation_value(retry)?;
        }
    }
    if let Some(cancellation) = object.get("cancellation_request") {
        if !cancellation.is_null() {
            validate_persisted_cancellation_request_value(cancellation)?;
        }
    }
    Ok(())
}

fn validate_persisted_v7_object_boundaries(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "schema_version",
            "operation_id",
            "intent",
            "universal_intent",
            "phase",
            "disposition",
            "universal_command_args",
            "capacity_plan",
            "retry_observation",
            "cancellation_request",
            "created_unix_ms",
            "updated_unix_ms",
        ],
    )?;
    for field in [
        "schema_version",
        "operation_id",
        "intent",
        "universal_intent",
        "phase",
        "disposition",
        "universal_command_args",
        "capacity_plan",
        "retry_observation",
        "cancellation_request",
        "created_unix_ms",
        "updated_unix_ms",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_intent_value(required_field(object, "intent")?)?;
    validate_persisted_universal_intent_value(required_field(object, "universal_intent")?)?;
    validate_persisted_universal_command_args_value(required_field(
        object,
        "universal_command_args",
    )?)?;
    validate_persisted_capacity_plan_v3_value(required_field(object, "capacity_plan")?)?;
    if let Some(retry) = object.get("retry_observation") {
        if !retry.is_null() {
            validate_persisted_retry_observation_value(retry)?;
        }
    }
    if let Some(cancellation) = object.get("cancellation_request") {
        if !cancellation.is_null() {
            validate_persisted_cancellation_request_value(cancellation)?;
        }
    }
    Ok(())
}

fn validate_persisted_v8_object_boundaries(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "schema_version",
            "operation_id",
            "intent",
            "universal_intent",
            "phase",
            "disposition",
            "universal_command_args",
            "capacity_plan",
            "retry_observation",
            "cancellation_request",
            "created_unix_ms",
            "updated_unix_ms",
        ],
    )?;
    for field in [
        "schema_version",
        "operation_id",
        "intent",
        "universal_intent",
        "phase",
        "disposition",
        "universal_command_args",
        "capacity_plan",
        "retry_observation",
        "cancellation_request",
        "created_unix_ms",
        "updated_unix_ms",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_intent_value(required_field(object, "intent")?)?;
    validate_persisted_universal_intent_value(required_field(object, "universal_intent")?)?;
    validate_persisted_v8_universal_command_args_value(required_field(
        object,
        "universal_command_args",
    )?)?;
    validate_persisted_capacity_plan_v3_value(required_field(object, "capacity_plan")?)?;
    if let Some(retry) = object.get("retry_observation") {
        if !retry.is_null() {
            validate_persisted_retry_observation_value(retry)?;
        }
    }
    if let Some(cancellation) = object.get("cancellation_request") {
        if !cancellation.is_null() {
            validate_persisted_cancellation_request_value(cancellation)?;
        }
    }
    Ok(())
}

fn validate_persisted_v8_universal_command_args_value(value: &Value) -> Result<(), &'static str> {
    validate_persisted_tagged_command_args_value(
        value,
        &[
            "Copy",
            "Import",
            "DuplicateSame",
            "WaveformEdit",
            "Namespace",
        ],
        "schema-v8",
    )
}

fn validate_persisted_v9_object_boundaries(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "schema_version",
            "operation_id",
            "intent",
            "universal_intent",
            "phase",
            "disposition",
            "universal_command_args",
            "capacity_plan",
            "retry_observation",
            "cancellation_request",
            "created_unix_ms",
            "updated_unix_ms",
        ],
    )?;
    for field in [
        "schema_version",
        "operation_id",
        "intent",
        "universal_intent",
        "phase",
        "disposition",
        "universal_command_args",
        "capacity_plan",
        "retry_observation",
        "cancellation_request",
        "created_unix_ms",
        "updated_unix_ms",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_intent_value(required_field(object, "intent")?)?;
    validate_persisted_universal_intent_value(required_field(object, "universal_intent")?)?;
    validate_persisted_v9_universal_command_args_value(required_field(
        object,
        "universal_command_args",
    )?)?;
    validate_persisted_capacity_plan_v3_value(required_field(object, "capacity_plan")?)?;
    if let Some(retry) = object.get("retry_observation") {
        if !retry.is_null() {
            validate_persisted_retry_observation_value(retry)?;
        }
    }
    if let Some(cancellation) = object.get("cancellation_request") {
        if !cancellation.is_null() {
            validate_persisted_cancellation_request_value(cancellation)?;
        }
    }
    Ok(())
}

fn validate_persisted_v9_universal_command_args_value(value: &Value) -> Result<(), &'static str> {
    validate_persisted_tagged_command_args_value(
        value,
        &[
            "Copy",
            "Import",
            "DuplicateSame",
            "WaveformEdit",
            "Namespace",
            "Render",
        ],
        "schema-v9",
    )
}

fn validate_persisted_v10_object_boundaries(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "schema_version",
            "operation_id",
            "intent",
            "universal_intent",
            "phase",
            "disposition",
            "universal_command_args",
            "capacity_plan",
            "retry_observation",
            "cancellation_request",
            "created_unix_ms",
            "updated_unix_ms",
        ],
    )?;
    for field in [
        "schema_version",
        "operation_id",
        "intent",
        "universal_intent",
        "phase",
        "disposition",
        "universal_command_args",
        "capacity_plan",
        "retry_observation",
        "cancellation_request",
        "created_unix_ms",
        "updated_unix_ms",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_intent_value(required_field(object, "intent")?)?;
    validate_persisted_universal_intent_value(required_field(object, "universal_intent")?)?;
    validate_persisted_v10_universal_command_args_value(required_field(
        object,
        "universal_command_args",
    )?)?;
    validate_persisted_capacity_plan_v3_value(required_field(object, "capacity_plan")?)?;
    if let Some(retry) = object.get("retry_observation") {
        if !retry.is_null() {
            validate_persisted_retry_observation_value(retry)?;
        }
    }
    if let Some(cancellation) = object.get("cancellation_request") {
        if !cancellation.is_null() {
            validate_persisted_cancellation_request_value(cancellation)?;
        }
    }
    Ok(())
}

fn validate_persisted_v10_universal_command_args_value(value: &Value) -> Result<(), &'static str> {
    validate_persisted_tagged_command_args_value(
        value,
        &[
            "Copy",
            "Import",
            "DuplicateSame",
            "WaveformEdit",
            "Namespace",
            "Render",
            "Removal",
        ],
        "schema-v10",
    )
}

fn validate_persisted_v11_object_boundaries(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "schema_version",
            "operation_id",
            "intent",
            "universal_intent",
            "phase",
            "disposition",
            "universal_command_args",
            "capacity_plan",
            "retry_observation",
            "cancellation_request",
            "created_unix_ms",
            "updated_unix_ms",
        ],
    )?;
    for field in [
        "schema_version",
        "operation_id",
        "intent",
        "universal_intent",
        "phase",
        "disposition",
        "universal_command_args",
        "capacity_plan",
        "retry_observation",
        "cancellation_request",
        "created_unix_ms",
        "updated_unix_ms",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_intent_value(required_field(object, "intent")?)?;
    validate_persisted_v11_universal_intent_value(required_field(object, "universal_intent")?)?;
    validate_persisted_v11_universal_command_args_value(required_field(
        object,
        "universal_command_args",
    )?)?;
    validate_persisted_capacity_plan_v3_value(required_field(object, "capacity_plan")?)?;
    if let Some(retry) = object.get("retry_observation") {
        if !retry.is_null() {
            validate_persisted_retry_observation_value(retry)?;
        }
    }
    if let Some(cancellation) = object.get("cancellation_request") {
        if !cancellation.is_null() {
            validate_persisted_cancellation_request_value(cancellation)?;
        }
    }
    Ok(())
}

fn validate_persisted_v11_universal_intent_value(value: &Value) -> Result<(), &'static str> {
    validate_persisted_universal_intent_value_with_retained_resolution(value, true)
}

fn validate_persisted_v11_universal_command_args_value(value: &Value) -> Result<(), &'static str> {
    validate_persisted_tagged_command_args_value(
        value,
        &[
            "Copy",
            "Import",
            "DuplicateSame",
            "WaveformEdit",
            "Namespace",
            "Render",
            "Removal",
            "RetainedResolution",
        ],
        "schema-v11",
    )
}

fn validate_persisted_v12_object_boundaries(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "schema_version",
            "operation_id",
            "intent",
            "universal_intent",
            "phase",
            "disposition",
            "universal_command_args",
            "capacity_plan",
            "retry_observation",
            "cancellation_request",
            "created_unix_ms",
            "updated_unix_ms",
        ],
    )?;
    for field in [
        "schema_version",
        "operation_id",
        "intent",
        "universal_intent",
        "phase",
        "disposition",
        "universal_command_args",
        "capacity_plan",
        "retry_observation",
        "cancellation_request",
        "created_unix_ms",
        "updated_unix_ms",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_intent_value(required_field(object, "intent")?)?;
    validate_persisted_v12_universal_intent_value(required_field(object, "universal_intent")?)?;
    validate_persisted_v12_universal_command_args_value(required_field(
        object,
        "universal_command_args",
    )?)?;
    validate_persisted_capacity_plan_v3_value(required_field(object, "capacity_plan")?)?;
    if let Some(retry) = object.get("retry_observation") {
        if !retry.is_null() {
            validate_persisted_retry_observation_value(retry)?;
        }
    }
    if let Some(cancellation) = object.get("cancellation_request") {
        if !cancellation.is_null() {
            validate_persisted_cancellation_request_value(cancellation)?;
        }
    }
    Ok(())
}

fn validate_persisted_v12_universal_intent_value(value: &Value) -> Result<(), &'static str> {
    validate_persisted_universal_intent_value_with_retained_resolution(value, true)
}

fn validate_persisted_v12_universal_command_args_value(value: &Value) -> Result<(), &'static str> {
    validate_persisted_tagged_command_args_value(
        value,
        &[
            "Copy",
            "Import",
            "DuplicateSame",
            "WaveformEdit",
            "Namespace",
            "Render",
            "Removal",
            "RetainedResolution",
            "SelectionNormalize",
        ],
        "schema-v12",
    )
}

fn validate_persisted_tagged_command_args_value(
    value: &Value,
    allowed_variants: &[&str],
    schema_name: &str,
) -> Result<(), &'static str> {
    let object = value
        .as_object()
        .ok_or("persisted command arguments must be a tagged object")?;
    if object.len() != 1 {
        return Err("persisted command arguments must contain exactly one variant");
    }
    let (variant, arguments) = object
        .iter()
        .next()
        .ok_or("persisted command arguments must contain exactly one variant")?;
    if !allowed_variants.contains(&variant.as_str()) {
        return match variant.as_str() {
            "Namespace" if schema_name == "schema-v7" => {
                Err("schema-v7 command arguments cannot carry schema-v8 namespace arguments")
            }
            "Render" if schema_name == "schema-v8" => {
                Err("schema-v8 command arguments cannot carry schema-v9 render arguments")
            }
            "Removal" if schema_name == "schema-v8" => {
                Err("schema-v8 command arguments cannot carry schema-v10 removal arguments")
            }
            "Removal" if schema_name == "schema-v9" => {
                Err("schema-v9 command arguments cannot carry schema-v10 removal arguments")
            }
            "RetainedResolution" => {
                Err("command arguments carry schema-v11 retained resolution arguments")
            }
            "SelectionNormalize" => {
                Err("command arguments carry schema-v12 selection Normalize arguments")
            }
            "Namespace" => Err("command arguments carry schema-v8 namespace arguments"),
            "Render" => Err("command arguments carry schema-v9 render arguments"),
            "Removal" => Err("command arguments carry schema-v10 removal arguments"),
            _ => Err("persisted command arguments contain an unknown or disallowed variant"),
        };
    }
    match variant.as_str() {
        "Copy" | "DuplicateSame" => validate_persisted_copy_intent_value(arguments),
        "Import" => validate_persisted_import_intent_value(arguments),
        "WaveformEdit" => validate_persisted_waveform_edit_value(arguments),
        "Namespace" => validate_persisted_namespace_command_args_value(arguments),
        "Render" => validate_persisted_render_command_args_value(arguments),
        "Removal" => validate_persisted_removal_command_args_value(arguments),
        "RetainedResolution" => {
            validate_persisted_retained_resolution_command_args_value(arguments)
        }
        "SelectionNormalize" => validate_persisted_selection_normalize_intent_args_value(arguments),
        _ => Err("persisted command arguments contain an unknown variant"),
    }
}

pub(crate) fn validate_v3_initial_shape(record: &StoredOperationRecord) -> Result<(), String> {
    if record.schema_version != SCHEMA_V3 {
        return Err(String::from("schema-v3 validation received another schema"));
    }
    if record.phase != OperationPhase::IntentDurable {
        return Err(format!(
            "schema-v3 foundation only supports IntentDurable, found {:?}",
            record.phase
        ));
    }
    if record.disposition != OperationDisposition::None {
        return Err(String::from(
            "schema-v3 initial records require the None disposition",
        ));
    }
    if record.cancellation_request.is_some() {
        return Err(String::from(
            "schema-v3 cannot contain a cancellation request",
        ));
    }
    if record.command_args.is_some() {
        return Err(String::from(
            "schema-v3 cannot contain typed coordinator arguments",
        ));
    }
    if record.retry_observation.is_some() {
        return Err(String::from("schema-v3 cannot contain a retry observation"));
    }
    if record.universal_command_args.is_some() {
        return Err(String::from(
            "schema-v3 cannot contain V6 tagged command arguments",
        ));
    }
    if let Some(intent) = record.universal_intent.as_ref() {
        match intent.command_kind() {
            super::model::UniversalCommandKind::Import => {
                return Err(String::from(
                    "schema-v3 cannot encode the V6 Import command",
                ));
            }
            super::model::UniversalCommandKind::RestoreRetained
            | super::model::UniversalCommandKind::PurgeRetained => {
                return Err(String::from(
                    "schema-v3 cannot encode schema-v11 retained resolution commands",
                ));
            }
            _ => {}
        }
    }
    validate_universal_intent_envelope_shape(record)
}

fn validate_universal_intent_envelope_shape(record: &StoredOperationRecord) -> Result<(), String> {
    if record.capacity_plan.is_none() {
        return Err(String::from(
            "universal intent records require a capacity plan",
        ));
    }
    for (present, kind) in [
        (record.prepared.is_some(), "prepared"),
        (record.staged.is_some(), "staged"),
        (record.published().is_some(), "published"),
        (
            record.absent_final_recovery_observation.is_some(),
            "absent-final recovery observation",
        ),
        (
            record.absent_final_transaction_owned_proof.is_some(),
            "absent-final transaction-owned proof",
        ),
        (
            record.absent_final_adoption_evidence.is_some(),
            "absent-final adoption evidence",
        ),
        (
            record.replacement_qualification.is_some(),
            "replacement qualification",
        ),
    ] {
        if present {
            return Err(format!(
                "universal intent record cannot contain {kind} evidence"
            ));
        }
    }
    if record.intent.label.is_empty() {
        return Err(String::from("schema-v3 operation label must not be empty"));
    }
    if record.intent.label.len() > MAX_UNIVERSAL_LABEL_BYTES {
        return Err(String::from(
            "schema-v3 operation label exceeds its byte bound",
        ));
    }
    if record.intent.label.chars().any(char::is_control) {
        return Err(String::from(
            "schema-v3 operation label contains a control character",
        ));
    }
    if !record.payload.is_object() {
        return Err(String::from("schema-v3 payload must be a JSON object"));
    }
    validate_json_depth(&record.payload, MAX_V3_JSON_DEPTH.saturating_sub(1))
        .map_err(String::from)?;
    if record.created_unix_ms < 0 || record.updated_unix_ms < 0 {
        return Err(String::from("schema-v3 timestamps must be non-negative"));
    }
    if record.updated_unix_ms < record.created_unix_ms {
        return Err(String::from(
            "schema-v3 updated timestamp must not precede creation timestamp",
        ));
    }
    let universal = record
        .universal_intent
        .as_ref()
        .ok_or_else(|| String::from("schema-v3 record requires a universal intent"))?;
    universal
        .validate_for_new_intent(
            record.operation_id,
            record.intent.actor,
            &record.intent.kind,
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub(crate) fn validate_v4_cancel_shape(record: &StoredOperationRecord) -> Result<(), String> {
    if record.schema_version != SCHEMA_V4 {
        return Err(String::from("schema-v4 validation received another schema"));
    }
    if record.phase != OperationPhase::IntentDurable {
        return Err(format!(
            "schema-v4 cancellation request only supports IntentDurable, found {:?}",
            record.phase
        ));
    }
    if record.disposition != OperationDisposition::CancelRequestedBeforePublish {
        return Err(String::from(
            "schema-v4 cancellation records require CancelRequestedBeforePublish",
        ));
    }
    if record.intent.actor != OperationActor::User {
        return Err(String::from(
            "schema-v4 cancellation records require a user intent",
        ));
    }
    if record.command_args.is_some() {
        return Err(String::from(
            "schema-v4 cannot contain typed coordinator arguments",
        ));
    }
    if record.retry_observation.is_some() {
        return Err(String::from("schema-v4 cannot contain a retry observation"));
    }
    if record.universal_command_args.is_some() {
        return Err(String::from(
            "schema-v4 cannot contain V6 tagged command arguments",
        ));
    }
    if let Some(intent) = record.universal_intent.as_ref() {
        match intent.command_kind() {
            super::model::UniversalCommandKind::Import => {
                return Err(String::from(
                    "schema-v4 cannot encode the V6 Import command",
                ));
            }
            super::model::UniversalCommandKind::RestoreRetained
            | super::model::UniversalCommandKind::PurgeRetained => {
                return Err(String::from(
                    "schema-v4 cannot encode schema-v11 retained resolution commands",
                ));
            }
            _ => {}
        }
    }
    let observation = record
        .cancellation_request
        .ok_or_else(|| String::from("schema-v4 cancellation record requires an observation"))?;
    observation.validate().map_err(|error| error.to_string())?;
    if observation.requested_unix_ms() != record.updated_unix_ms {
        return Err(String::from(
            "schema-v4 cancellation request timestamp must equal the updated timestamp",
        ));
    }

    validate_universal_intent_envelope_shape(record)
}

pub(crate) fn validate_v5_coordinator_shape(record: &StoredOperationRecord) -> Result<(), String> {
    if record.schema_version != SCHEMA_V5 {
        return Err(String::from("schema-v5 validation received another schema"));
    }
    if record.phase != OperationPhase::IntentDurable {
        return Err(format!(
            "schema-v5 coordinator only supports IntentDurable, found {:?}",
            record.phase
        ));
    }
    if record.universal_command_args.is_some() {
        return Err(String::from(
            "schema-v5 cannot contain V6 tagged command arguments",
        ));
    }
    if record.intent.actor != OperationActor::User
        || record.intent.kind != super::model::OperationKind::FileMutation
    {
        return Err(String::from(
            "schema-v5 coordinator records require a user file-mutation intent",
        ));
    }
    if !record.payload.as_object().is_some_and(Map::is_empty) {
        return Err(String::from(
            "schema-v5 coordinator records cannot carry opaque payload",
        ));
    }
    let universal = record
        .universal_intent
        .as_ref()
        .ok_or_else(|| String::from("schema-v5 record requires a universal intent"))?;
    if universal.command_kind() != super::model::UniversalCommandKind::Copy {
        return Err(String::from(
            "schema-v5 coordinator currently supports only the Copy command",
        ));
    }
    let command_args = record
        .command_args
        .as_ref()
        .ok_or_else(|| String::from("schema-v5 record requires typed copy arguments"))?;
    command_args
        .validate_against_participants(universal.participants())
        .map_err(|error| error.to_string())?;
    validate_universal_intent_envelope_shape(record)?;

    match record.disposition {
        OperationDisposition::None => {
            if record.retry_observation.is_some() || record.cancellation_request.is_some() {
                return Err(String::from(
                    "schema-v5 None records cannot carry retry or cancellation observations",
                ));
            }
        }
        OperationDisposition::RetryPending => {
            if record.cancellation_request.is_some() {
                return Err(String::from(
                    "schema-v5 retry records cannot carry a cancellation request",
                ));
            }
            let retry = record.retry_observation.as_ref().ok_or_else(|| {
                String::from("schema-v5 RetryPending requires a retry observation")
            })?;
            retry.validate().map_err(|error| error.to_string())?;
            if retry.requested_unix_ms() < record.created_unix_ms {
                return Err(String::from(
                    "schema-v5 retry request timestamp precedes creation",
                ));
            }
            if retry.requested_unix_ms() != record.updated_unix_ms {
                return Err(String::from(
                    "schema-v5 retry request timestamp must equal the updated timestamp",
                ));
            }
        }
        OperationDisposition::CancelRequestedBeforePublish => {
            let cancellation = record.cancellation_request.ok_or_else(|| {
                String::from("schema-v5 cancellation requires a cancellation observation")
            })?;
            cancellation.validate().map_err(|error| error.to_string())?;
            if cancellation.requested_unix_ms() != record.updated_unix_ms {
                return Err(String::from(
                    "schema-v5 cancellation request timestamp must equal the updated timestamp",
                ));
            }
            if let Some(retry) = record.retry_observation.as_ref() {
                retry.validate().map_err(|error| error.to_string())?;
                if retry.requested_unix_ms() < record.created_unix_ms
                    || retry.requested_unix_ms() > cancellation.requested_unix_ms()
                {
                    return Err(String::from(
                        "schema-v5 retry request timestamp is outside the creation-to-cancellation interval",
                    ));
                }
            }
        }
        disposition => {
            return Err(format!(
                "schema-v5 coordinator does not support disposition {disposition:?}"
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_v6_coordinator_shape(record: &StoredOperationRecord) -> Result<(), String> {
    if record.schema_version != SCHEMA_V6 {
        return Err(String::from("schema-v6 validation received another schema"));
    }
    if record.phase != OperationPhase::IntentDurable {
        return Err(format!(
            "schema-v6 coordinator only supports IntentDurable, found {:?}",
            record.phase
        ));
    }
    if record.intent.actor != OperationActor::User
        || record.intent.kind != super::model::OperationKind::FileMutation
    {
        return Err(String::from(
            "schema-v6 coordinator records require a user file-mutation intent",
        ));
    }
    if !record.payload.as_object().is_some_and(Map::is_empty) {
        return Err(String::from(
            "schema-v6 coordinator records cannot carry opaque payload",
        ));
    }
    if record.command_args.is_some() {
        return Err(String::from(
            "schema-v6 coordinator records cannot carry the V5 copy-only command_args field",
        ));
    }
    let universal = record
        .universal_intent
        .as_ref()
        .ok_or_else(|| String::from("schema-v6 record requires a universal intent"))?;
    let command_args = record
        .universal_command_args
        .as_ref()
        .ok_or_else(|| String::from("schema-v6 record requires tagged command arguments"))?;
    match command_args {
        UniversalCommandArgs::Copy(_)
        | UniversalCommandArgs::Import(_)
        | UniversalCommandArgs::DuplicateSame(_) => {}
        UniversalCommandArgs::WaveformEdit(_) => {
            return Err(String::from(
                "schema-v6 cannot carry schema-v7 waveform edit arguments",
            ));
        }
        UniversalCommandArgs::Namespace(_) => {
            return Err(String::from(
                "schema-v6 cannot carry schema-v8 namespace arguments",
            ));
        }
        UniversalCommandArgs::Render(_) => {
            return Err(String::from(
                "schema-v6 cannot carry schema-v9 render arguments",
            ));
        }
        UniversalCommandArgs::Removal(_) => {
            return Err(String::from(
                "schema-v6 cannot carry schema-v10 removal arguments",
            ));
        }
        UniversalCommandArgs::RetainedResolution(_) => {
            return Err(String::from(
                "schema-v6 cannot carry schema-v11 retained resolution arguments",
            ));
        }
        UniversalCommandArgs::SelectionNormalize(_) => {
            return Err(String::from(
                "schema-v6 cannot carry schema-v12 selection Normalize arguments",
            ));
        }
    }
    if universal.command_kind() != command_args.command_kind() {
        return Err(String::from(
            "schema-v6 universal command kind does not match tagged command arguments",
        ));
    }
    command_args
        .validate_against_participants(universal.participants())
        .map_err(|error| error.to_string())?;
    validate_universal_intent_envelope_shape(record)?;

    match record.disposition {
        OperationDisposition::None => {
            if record.retry_observation.is_some() || record.cancellation_request.is_some() {
                return Err(String::from(
                    "schema-v6 None records cannot carry retry or cancellation observations",
                ));
            }
        }
        OperationDisposition::RetryPending => {
            if record.cancellation_request.is_some() {
                return Err(String::from(
                    "schema-v6 retry records cannot carry a cancellation request",
                ));
            }
            let retry = record.retry_observation.as_ref().ok_or_else(|| {
                String::from("schema-v6 RetryPending requires a retry observation")
            })?;
            retry.validate().map_err(|error| error.to_string())?;
            if retry.requested_unix_ms() < record.created_unix_ms {
                return Err(String::from(
                    "schema-v6 retry request timestamp precedes creation",
                ));
            }
            if retry.requested_unix_ms() != record.updated_unix_ms {
                return Err(String::from(
                    "schema-v6 retry request timestamp must equal the updated timestamp",
                ));
            }
        }
        OperationDisposition::CancelRequestedBeforePublish => {
            let cancellation = record.cancellation_request.ok_or_else(|| {
                String::from("schema-v6 cancellation requires a cancellation observation")
            })?;
            cancellation.validate().map_err(|error| error.to_string())?;
            if cancellation.requested_unix_ms() != record.updated_unix_ms {
                return Err(String::from(
                    "schema-v6 cancellation request timestamp must equal the updated timestamp",
                ));
            }
            if let Some(retry) = record.retry_observation.as_ref() {
                retry.validate().map_err(|error| error.to_string())?;
                if retry.requested_unix_ms() < record.created_unix_ms
                    || retry.requested_unix_ms() > cancellation.requested_unix_ms()
                {
                    return Err(String::from(
                        "schema-v6 retry request timestamp is outside the creation-to-cancellation interval",
                    ));
                }
            }
        }
        disposition => {
            return Err(format!(
                "schema-v6 coordinator does not support disposition {disposition:?}"
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_v7_coordinator_shape(record: &StoredOperationRecord) -> Result<(), String> {
    validate_tagged_coordinator_shape(
        record,
        SCHEMA_V7,
        "schema-v7",
        &["Copy", "Import", "DuplicateSame", "WaveformEdit"],
    )
}

pub(crate) fn validate_v8_coordinator_shape(record: &StoredOperationRecord) -> Result<(), String> {
    validate_tagged_coordinator_shape(
        record,
        SCHEMA_V8,
        "schema-v8",
        &[
            "Copy",
            "Import",
            "DuplicateSame",
            "WaveformEdit",
            "Namespace",
        ],
    )
}

pub(crate) fn validate_v9_coordinator_shape(record: &StoredOperationRecord) -> Result<(), String> {
    validate_tagged_coordinator_shape(
        record,
        SCHEMA_V9,
        "schema-v9",
        &[
            "Copy",
            "Import",
            "DuplicateSame",
            "WaveformEdit",
            "Namespace",
            "Render",
        ],
    )
}

pub(crate) fn validate_v10_coordinator_shape(record: &StoredOperationRecord) -> Result<(), String> {
    validate_tagged_coordinator_shape(
        record,
        SCHEMA_V10,
        "schema-v10",
        &[
            "Copy",
            "Import",
            "DuplicateSame",
            "WaveformEdit",
            "Namespace",
            "Render",
            "Removal",
        ],
    )
}

pub(crate) fn validate_v11_coordinator_shape(record: &StoredOperationRecord) -> Result<(), String> {
    validate_tagged_coordinator_shape(
        record,
        SCHEMA_V11,
        "schema-v11",
        &[
            "Copy",
            "Import",
            "DuplicateSame",
            "WaveformEdit",
            "Namespace",
            "Render",
            "Removal",
            "RetainedResolution",
        ],
    )
}

pub(crate) fn validate_v12_coordinator_shape(record: &StoredOperationRecord) -> Result<(), String> {
    validate_tagged_coordinator_shape(
        record,
        SCHEMA_V12,
        "schema-v12",
        &[
            "Copy",
            "Import",
            "DuplicateSame",
            "WaveformEdit",
            "Namespace",
            "Render",
            "Removal",
            "RetainedResolution",
            "SelectionNormalize",
        ],
    )
}

fn validate_tagged_coordinator_shape(
    record: &StoredOperationRecord,
    expected_schema: u32,
    schema_name: &str,
    allowed_variants: &[&str],
) -> Result<(), String> {
    if record.schema_version != expected_schema {
        return Err(format!("{schema_name} validation received another schema"));
    }
    if record.phase != OperationPhase::IntentDurable {
        return Err(format!(
            "{schema_name} coordinator only supports IntentDurable, found {:?}",
            record.phase
        ));
    }
    if record.intent.actor != OperationActor::User
        || record.intent.kind != super::model::OperationKind::FileMutation
    {
        return Err(format!(
            "{schema_name} coordinator records require a user file-mutation intent"
        ));
    }
    if !record.payload.as_object().is_some_and(Map::is_empty) {
        return Err(format!(
            "{schema_name} coordinator records cannot carry opaque payload"
        ));
    }
    if record.command_args.is_some() {
        return Err(format!(
            "{schema_name} coordinator records cannot carry the V5 command_args field"
        ));
    }
    let universal = record
        .universal_intent
        .as_ref()
        .ok_or_else(|| format!("{schema_name} record requires a universal intent"))?;
    let command_args = record
        .universal_command_args
        .as_ref()
        .ok_or_else(|| format!("{schema_name} record requires tagged command arguments"))?;
    let variant = match command_args {
        UniversalCommandArgs::Copy(_) => "Copy",
        UniversalCommandArgs::Import(_) => "Import",
        UniversalCommandArgs::DuplicateSame(_) => "DuplicateSame",
        UniversalCommandArgs::WaveformEdit(_) => "WaveformEdit",
        UniversalCommandArgs::Namespace(_) => "Namespace",
        UniversalCommandArgs::Render(_) => "Render",
        UniversalCommandArgs::Removal(_) => "Removal",
        UniversalCommandArgs::RetainedResolution(_) => "RetainedResolution",
        UniversalCommandArgs::SelectionNormalize(_) => "SelectionNormalize",
    };
    if !allowed_variants.contains(&variant) {
        return Err(match variant {
            "Namespace" => {
                format!("{schema_name} cannot carry schema-v8 namespace arguments")
            }
            "Render" => format!("{schema_name} cannot carry schema-v9 render arguments"),
            "Removal" => format!("{schema_name} cannot carry schema-v10 removal arguments"),
            "RetainedResolution" => {
                format!("{schema_name} cannot carry schema-v11 retained resolution arguments")
            }
            "SelectionNormalize" => {
                format!("{schema_name} cannot carry schema-v12 selection Normalize arguments")
            }
            _ => format!("{schema_name} cannot carry {variant} arguments"),
        });
    }
    if universal.command_kind() != command_args.command_kind() {
        return Err(format!(
            "{schema_name} universal command kind does not match tagged command arguments"
        ));
    }
    command_args
        .validate_against_participants(universal.participants())
        .map_err(|error| error.to_string())?;
    validate_universal_intent_envelope_shape(record)?;

    match record.disposition {
        OperationDisposition::None => {
            if record.retry_observation.is_some() || record.cancellation_request.is_some() {
                return Err(format!(
                    "{schema_name} None records cannot carry retry or cancellation observations"
                ));
            }
        }
        OperationDisposition::RetryPending => {
            if record.cancellation_request.is_some() {
                return Err(format!(
                    "{schema_name} retry records cannot carry a cancellation request"
                ));
            }
            let retry = record.retry_observation.as_ref().ok_or_else(|| {
                format!("{schema_name} RetryPending requires a retry observation")
            })?;
            retry.validate().map_err(|error| error.to_string())?;
            if retry.requested_unix_ms() < record.created_unix_ms {
                return Err(format!(
                    "{schema_name} retry request timestamp precedes creation"
                ));
            }
            if retry.requested_unix_ms() != record.updated_unix_ms {
                return Err(format!(
                    "{schema_name} retry request timestamp must equal the updated timestamp"
                ));
            }
        }
        OperationDisposition::CancelRequestedBeforePublish => {
            let cancellation = record.cancellation_request.ok_or_else(|| {
                format!("{schema_name} cancellation requires a cancellation observation")
            })?;
            cancellation.validate().map_err(|error| error.to_string())?;
            if cancellation.requested_unix_ms() != record.updated_unix_ms {
                return Err(format!(
                    "{schema_name} cancellation request timestamp must equal the updated timestamp"
                ));
            }
            if let Some(retry) = record.retry_observation.as_ref() {
                retry.validate().map_err(|error| error.to_string())?;
                if retry.requested_unix_ms() < record.created_unix_ms
                    || retry.requested_unix_ms() > cancellation.requested_unix_ms()
                {
                    return Err(format!(
                        "{schema_name} retry request timestamp is outside the creation-to-cancellation interval"
                    ));
                }
            }
        }
        disposition => {
            return Err(format!(
                "{schema_name} coordinator does not support disposition {disposition:?}"
            ));
        }
    }
    Ok(())
}

fn validate_json_depth(value: &Value, maximum: usize) -> Result<(), &'static str> {
    fn visit(
        value: &Value,
        depth: usize,
        maximum: usize,
        nodes: &mut usize,
    ) -> Result<(), &'static str> {
        if depth > maximum {
            return Err("schema-v3 JSON nesting exceeds its depth bound");
        }
        *nodes = nodes.saturating_add(1);
        if *nodes > MAX_RECORD_BYTES {
            return Err("schema-v3 JSON node count exceeds its bound");
        }
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, depth.saturating_add(1), maximum, nodes)?;
                }
            }
            Value::Object(values) => {
                for value in values.values() {
                    visit(value, depth.saturating_add(1), maximum, nodes)?;
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
        Ok(())
    }

    let mut nodes = 0;
    visit(value, 0, maximum, &mut nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_journal::UniversalCommandKind;
    use crate::operation_journal::{RecordConsistency, classify_record};

    const V1_FIXTURE: &[u8] = br#"{
  "schema_version": 1,
  "operation_id": "01234567-89ab-cdef-0123-456789abcdef",
  "intent": {"actor": "User", "kind": "FileHistory", "label": "legacy fixture"},
  "phase": "IntentDurable",
  "disposition": "RetryPending",
  "payload": {"legacy": true},
  "capacity_plan": null,
  "prepared": null,
  "staged": null,
  "published": null,
  "replacement_qualification": null,
  "created_unix_ms": 11,
  "updated_unix_ms": 12
}"#;

    const V2_FIXTURE: &[u8] = br#"{
  "schema_version": 2,
  "operation_id": "fedcba98-7654-3210-fedc-ba9876543210",
  "intent": {"actor": "ExternalFs", "kind": "ExternalReconciliation", "label": "v2 fixture"},
  "phase": "IntentDurable",
  "disposition": "None",
  "payload": null,
  "capacity_plan": null,
  "prepared": null,
  "staged": null,
  "absent_final_recovery_observation": null,
  "absent_final_transaction_owned_proof": null,
  "absent_final_adoption_evidence": null,
  "published": null,
  "replacement_qualification": null,
  "created_unix_ms": 21,
  "updated_unix_ms": 22
}"#;

    const V3_FIXTURE: &[u8] = br#"{
  "schema_version": 3,
  "operation_id": "01234567-89ab-cdef-0123-456789abcdef",
  "intent": {"actor": "User", "kind": "FileMutation", "label": "v3 fixture"},
  "universal_intent": {
    "parent_operation_id": null,
    "command_kind": "CreateFile",
    "user_status_key": "operation.status.create-file",
    "participants": {
      "descriptors": [{
        "id": "fedcba98-7654-3210-fedc-ba9876543210",
        "kind": "GlobalLibrary",
        "requirement": "Required",
        "source_fence": null,
        "artifact_key": null
      }],
      "checkpoints": [{
        "participant_id": "fedcba98-7654-3210-fedc-ba9876543210",
        "state": "Pending",
        "committed_revision": null,
        "diagnostic_code": null
      }]
    }
  },
  "phase": "IntentDurable",
  "disposition": "None",
  "payload": {"source": "fixture"},
  "capacity_plan": {
    "volumes": [{
      "identity": {"device": 1},
      "allocation_unit": 1,
      "allocation_class": "DestinationStaging",
      "logical_bytes": 1,
      "allocated_bytes": 1,
      "protected_free_bytes": 268435456
    }]
  },
  "created_unix_ms": 31,
  "updated_unix_ms": 32
}"#;

    const ABSENT_FINAL_PUBLICATION_FIXTURE: &[u8] = br#"{
  "mode": "NonAtomicCopyValidatePublish",
  "final_claim": {
    "AbsentFinalNoReplace": {
      "primitive": "QualifiedAbsentFinalNoReplace",
      "result": "AbsentFinalInstalled",
      "capability_scope": {
        "TargetParentDescriptor": {
          "target_parent_identity": {
            "stable_id": "target-parent",
            "change_marker": null,
            "len": 4096
          },
          "root_path_continuity": "NotClaimed"
        }
      }
    }
  },
  "reopened_final": {
    "identity": {
      "stable_id": "final",
      "change_marker": null,
      "len": 32
    },
    "content": {
      "ContentHash": [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                      0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    }
  },
  "visibility": "VisibilityVerified",
  "whole_publication": "WholePublicationNonAtomic",
  "synchronization": "BestEffortSync"
}"#;

    const ABSENT_FINAL_RECOVERY_FIXTURE: &[u8] = br#"{
  "target_parent_stable_id": "target-parent",
  "final_stable_id": "final",
  "final_len": 32,
  "final_content": {
    "ContentHash": [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
  }
}"#;

    fn value(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).expect("fixture JSON is valid")
    }

    fn v4_cancel_fixture() -> Value {
        let mut fixture = value(V3_FIXTURE);
        fixture["schema_version"] = Value::from(4_u64);
        fixture["disposition"] = Value::from("CancelRequestedBeforePublish");
        fixture["cancellation_request"] = serde_json::json!({
            "requested_unix_ms": 40
        });
        fixture["updated_unix_ms"] = Value::from(40_i64);
        fixture
    }

    fn v5_copy_fixture() -> Value {
        serde_json::json!({
            "schema_version": 5,
            "operation_id": "01234567-89ab-cdef-0123-456789abcdef",
            "intent": {
                "actor": "User",
                "kind": "FileMutation",
                "label": "copy fixture"
            },
            "universal_intent": {
                "parent_operation_id": null,
                "command_kind": "Copy",
                "user_status_key": "operation.status.copy",
                "participants": {
                    "descriptors": [
                        {
                            "id": "fedcba98-7654-3210-fedc-ba9876543210",
                            "kind": "SourceDatabase",
                            "requirement": "Required",
                            "source_fence": {
                                "source_id": "source-a",
                                "root_identity": "root-a",
                                "database_identity": "database-a",
                                "lifecycle_generation": 1,
                                "source_revision": 7,
                                "lease_epoch": 1
                            },
                            "artifact_key": null
                        },
                        {
                            "id": "89abcdef-0123-4567-89ab-cdef01234567",
                            "kind": "Filesystem",
                            "requirement": "Required",
                            "source_fence": null,
                            "artifact_key": null
                        }
                    ],
                    "checkpoints": [
                        {
                            "participant_id": "fedcba98-7654-3210-fedc-ba9876543210",
                            "state": "Pending",
                            "committed_revision": null,
                            "diagnostic_code": null
                        },
                        {
                            "participant_id": "89abcdef-0123-4567-89ab-cdef01234567",
                            "state": "Pending",
                            "committed_revision": null,
                            "diagnostic_code": null
                        }
                    ]
                }
            },
            "phase": "IntentDurable",
            "disposition": "None",
            "command_args": {
                "source": {
                    "participant_id": "fedcba98-7654-3210-fedc-ba9876543210",
                    "source_id": "source-a",
                    "relative_path": "in.wav"
                },
                "destination": {
                    "participant_id": "fedcba98-7654-3210-fedc-ba9876543210",
                    "source_id": "source-a",
                    "relative_path": "out.wav"
                },
                "expected_source": {
                    "source_revision": 7,
                    "file_identity": "file-identity",
                    "file_size": 10,
                    "modified_ns": 11
                },
                "collision_policy": "NoReplace",
                "inherited_metadata_reference": "metadata/entry"
            },
            "capacity_plan": {
                "volumes": [{
                    "identity": {"device": 1},
                    "allocation_unit": 1,
                    "allocation_class": "DestinationStaging",
                    "logical_bytes": 1,
                    "allocated_bytes": 1,
                    "protected_free_bytes": 268435456
                }]
            },
            "retry_observation": null,
            "cancellation_request": null,
            "created_unix_ms": 50,
            "updated_unix_ms": 50
        })
    }

    fn v6_command_fixture(variant: &str) -> Value {
        let mut fixture = v5_copy_fixture();
        fixture["schema_version"] = Value::from(6_u64);
        fixture
            .as_object_mut()
            .expect("v5 fixture object")
            .remove("payload");
        let copy_args = fixture
            .as_object_mut()
            .expect("v6 fixture object")
            .remove("command_args")
            .expect("v5 copy arguments");
        let (command_kind, command_args) = match variant {
            "Copy" => ("Copy", serde_json::json!({ "Copy": copy_args })),
            "DuplicateSame" => (
                "Duplicate",
                serde_json::json!({ "DuplicateSame": copy_args }),
            ),
            "Import" => {
                let filesystem_id =
                    fixture["universal_intent"]["participants"]["descriptors"][1]["id"].clone();
                let destination = copy_args["destination"].clone();
                (
                    "Import",
                    serde_json::json!({
                        "Import": {
                            "external_root_binding_id": "00000000-0000-0000-0000-000000000001",
                            "filesystem_participant_id": filesystem_id,
                            "root_identity": "external-root-identity",
                            "relative_path": "sample.wav",
                            "expected_file": {
                                "file_identity": "external-file-identity",
                                "file_size": 12,
                                "modified_ns": 34
                            },
                            "destination": destination,
                            "collision_policy": "NoReplace",
                            "inherited_metadata_reference": "metadata/entry"
                        }
                    }),
                )
            }
            other => panic!("unknown V6 test variant {other}"),
        };
        fixture["universal_intent"]["command_kind"] = Value::from(command_kind);
        fixture["universal_command_args"] = command_args;
        fixture
    }

    fn v7_waveform_edit_fixture(operation: Value, range: (u64, u64)) -> Value {
        let mut fixture = v6_command_fixture("Copy");
        let source = fixture["universal_command_args"]["Copy"]["source"].clone();
        fixture["schema_version"] = Value::from(7_u64);
        fixture["universal_intent"]["command_kind"] = Value::from("DestructiveEdit");
        fixture["universal_command_args"] = serde_json::json!({
            "WaveformEdit": {
                "source": source,
                "source_file_revision": {
                    "filesystem_identity": "file-identity",
                    "change_marker": "change-marker",
                    "byte_len": 1024,
                    "modified_unix_ns": 12000000000_i64
                },
                "source_spec": {
                    "channels": 2,
                    "sample_rate": 48000,
                    "bits_per_sample": 16,
                    "sample_format": "Integer"
                },
                "total_frames": 10,
                "range": {
                    "start_frame": range.0,
                    "end_frame": range.1
                },
                "operation": operation,
                "output": {
                    "encoding": "Float32",
                    "channels": 2,
                    "sample_rate": 48000
                }
            }
        });
        fixture
    }

    fn v8_namespace_fixture(variant: &str) -> Value {
        let mut fixture = v6_command_fixture("Copy");
        let source_participant =
            fixture["universal_intent"]["participants"]["descriptors"][0]["id"].clone();
        let source_id = fixture["universal_intent"]["participants"]["descriptors"][0]
            ["source_fence"]["source_id"]
            .clone();
        let source = serde_json::json!({
            "participant_id": source_participant,
            "source_id": source_id,
            "relative_path": "folder/old.wav",
            "expectation": {
                "RegularFile": {
                    "source_revision": 7,
                    "filesystem_identity": "file-identity",
                    "byte_len": 10,
                    "modified_unix_ns": -2
                }
            }
        });
        let parent = serde_json::json!({
            "participant_id": source_participant,
            "source_id": source_id,
            "locator": {"Child": "folder"},
            "expectation": {
                "source_revision": 7,
                "filesystem_identity": "folder-identity",
                "modified_unix_ns": null
            }
        });
        let root_parent = serde_json::json!({
            "participant_id": source_participant,
            "source_id": source_id,
            "locator": "Root",
            "expectation": {
                "source_revision": 7,
                "filesystem_identity": "root-a",
                "modified_unix_ns": null
            }
        });
        let (command_kind, command_args) = match variant {
            "Rename" => (
                "Rename",
                serde_json::json!({
                    "Namespace": {
                        "Rename": {
                            "source": source,
                            "destination": {"parent": parent, "leaf": "new.wav"}
                        }
                    }
                }),
            ),
            "Move" => (
                "Move",
                serde_json::json!({
                    "Namespace": {
                        "Move": {
                            "source": source,
                            "destination_parent": {
                                "participant_id": source_participant,
                                "source_id": source_id,
                                "locator": {"Child": "archive"},
                                "expectation": {
                                    "source_revision": 7,
                                    "filesystem_identity": "archive-identity",
                                    "modified_unix_ns": null
                                }
                            }
                        }
                    }
                }),
            ),
            "CreateFolder" => (
                "CreateFolder",
                serde_json::json!({
                    "Namespace": {
                        "CreateFolder": {
                            "parent": root_parent,
                            "leaf": "new-folder"
                        }
                    }
                }),
            ),
            other => panic!("unknown V8 namespace fixture {other}"),
        };
        fixture["schema_version"] = Value::from(8_u64);
        fixture["universal_intent"]["command_kind"] = Value::from(command_kind);
        fixture["universal_command_args"] = command_args;
        fixture
    }

    fn v9_render_fixture(variant: &str) -> Value {
        let mut fixture = v6_command_fixture("Copy");
        let source = fixture["universal_command_args"]["Copy"]["source"].clone();
        fixture["schema_version"] = Value::from(9_u64);
        fixture["universal_intent"]["command_kind"] = Value::from(match variant {
            "Normalize" => "DestructiveEdit",
            "DuplicateDouble" => "Duplicate",
            other => panic!("unknown V9 render fixture {other}"),
        });
        fixture["universal_command_args"] = match variant {
            "Normalize" => serde_json::json!({
                "Render": {
                    "Normalize": {
                        "source": source,
                        "source_file_revision": {
                            "filesystem_identity": "file-identity",
                            "change_marker": "change-marker",
                            "byte_len": 1024,
                            "modified_unix_ns": 12000000000_i64
                        },
                        "source_spec": {
                            "channels": 2,
                            "sample_rate": 48000,
                            "bits_per_sample": 16,
                            "sample_format": "Integer"
                        },
                        "total_frames": 10,
                        "output": {
                            "encoding": "Float32",
                            "channels": 2,
                            "sample_rate": 48000
                        }
                    }
                }
            }),
            "DuplicateDouble" => serde_json::json!({
                "Render": {
                    "DuplicateDouble": {
                        "source": source,
                        "destination": {
                            "participant_id": "fedcba98-7654-3210-fedc-ba9876543210",
                            "source_id": "source-a",
                            "relative_path": "out_doubled.wav"
                        },
                        "source_file_revision": {
                            "filesystem_identity": "file-identity",
                            "change_marker": "change-marker",
                            "byte_len": 1024,
                            "modified_unix_ns": 12000000000_i64
                        },
                        "source_spec": {
                            "channels": 2,
                            "sample_rate": 48000,
                            "bits_per_sample": 16,
                            "sample_format": "Integer"
                        },
                        "total_frames": 10,
                        "output": {
                            "channels": 2,
                            "sample_rate": 48000,
                            "bits_per_sample": 16,
                            "sample_format": "Integer"
                        },
                        "collision_policy": "NoReplace"
                    }
                }
            }),
            _ => unreachable!(),
        };
        fixture
    }

    fn v10_trash_fixture() -> Value {
        let mut fixture = v6_command_fixture("Copy");
        let source_participant =
            fixture["universal_intent"]["participants"]["descriptors"][0]["id"].clone();
        let filesystem_participant =
            fixture["universal_intent"]["participants"]["descriptors"][1]["id"].clone();
        let source = serde_json::json!({
            "participant_id": source_participant,
            "source_id": "source-a",
            "relative_path": "folder/item.wav",
            "expectation": {
                "RegularFile": {
                    "source_revision": 7,
                    "filesystem_identity": "file-identity",
                    "byte_len": 10,
                    "modified_unix_ns": 11
                }
            }
        });
        fixture["schema_version"] = Value::from(10_u64);
        fixture["universal_intent"]["command_kind"] = Value::from("Trash");
        fixture["universal_command_args"] = serde_json::json!({
            "Removal": {
                "Trash": {
                    "source": source,
                    "external_root_binding_id": "00000000-0000-0000-0000-000000000003",
                    "filesystem_participant_id": filesystem_participant,
                    "trash_root_identity": "trash-root"
                }
            }
        });
        fixture
    }

    fn v11_retained_fixture(variant: &str) -> Value {
        let mut fixture = v6_command_fixture("Copy");
        let source_participant =
            fixture["universal_intent"]["participants"]["descriptors"][0]["id"].clone();
        let staged = serde_json::json!({
            "participant_id": source_participant,
            "source_id": "source-a",
            "relative_path": ".wavecrate_delete_staging/folder/retained",
            "expectation": {
                "Directory": {
                    "source_revision": 7,
                    "filesystem_identity": "staged-directory",
                    "modified_unix_ns": null
                }
            }
        });
        fixture["schema_version"] = Value::from(SCHEMA_V11);
        fixture["universal_intent"]["command_kind"] = Value::from(match variant {
            "Restore" => "RestoreRetained",
            "Purge" => "PurgeRetained",
            other => panic!("unknown V11 retained variant {other}"),
        });
        let observation = serde_json::json!({
            "original_relative": "folder/retained",
            "staged": staged,
            "staged_relative": "folder/retained",
            "retained_id": "01234567-89ab-cdef-0123-456789abcdef",
            "stage": "Deleted",
            "recovery_root_kind": "LegacyDeleteStaging",
            "selected_entry_fingerprint": [1, 2, 3, 4, 5, 6, 7, 8,
                9, 10, 11, 12, 13, 14, 15, 16,
                17, 18, 19, 20, 21, 22, 23, 24,
                25, 26, 27, 28, 29, 30, 31, 32]
        });
        fixture["universal_command_args"] = match variant {
            "Restore" => serde_json::json!({
                "RetainedResolution": {
                    "Restore": {"observation": observation}
                }
            }),
            "Purge" => serde_json::json!({
                "RetainedResolution": {
                    "Purge": {"observation": observation, "decision": "UserConfirmed"}
                }
            }),
            _ => unreachable!(),
        };
        fixture
    }

    fn v12_selection_normalize_fixture() -> Value {
        let mut fixture = v9_render_fixture("Normalize");
        let source = fixture["universal_command_args"]["Render"]["Normalize"]["source"].clone();
        fixture["schema_version"] = Value::from(SCHEMA_V12);
        fixture["universal_intent"]["command_kind"] = Value::from("DestructiveEdit");
        fixture["universal_command_args"] = serde_json::json!({
            "SelectionNormalize": {
                "source": source,
                "source_file_revision": {
                    "filesystem_identity": "selection-file-identity",
                    "change_marker": "selection-change-marker",
                    "byte_len": 1024,
                    "modified_unix_ns": -12000000000_i64
                },
                "source_spec": {
                    "channels": 2,
                    "sample_rate": 48000,
                    "bits_per_sample": 16,
                    "sample_format": "Integer"
                },
                "total_frames": 10,
                "range": {
                    "start_frame": 2,
                    "end_frame": 8
                },
                "edge_fade_frames": 2,
                "output": {
                    "encoding": "Float32",
                    "channels": 2,
                    "sample_rate": 48000
                }
            }
        });
        fixture
    }

    #[test]
    fn pinned_v1_fixture_decodes_and_round_trips_by_value() {
        let record = decode(V1_FIXTURE).expect("v1 fixture decodes");
        assert_eq!(record.schema_version, SCHEMA_V1);
        assert_eq!(
            record.operation_id.to_string(),
            "01234567-89ab-cdef-0123-456789abcdef"
        );
        assert_eq!(record.phase, OperationPhase::IntentDurable);
        assert_eq!(record.disposition, OperationDisposition::RetryPending);
        assert_eq!(
            value(&encode(&record).expect("v1 fixture encodes")),
            value(V1_FIXTURE)
        );
    }

    #[test]
    fn pinned_v2_fixture_decodes_and_round_trips_by_value() {
        let record = decode(V2_FIXTURE).expect("v2 fixture decodes");
        assert_eq!(record.schema_version, SCHEMA_V2);
        assert_eq!(
            record.operation_id.to_string(),
            "fedcba98-7654-3210-fedc-ba9876543210"
        );
        assert_eq!(
            value(&encode(&record).expect("v2 fixture encodes")),
            value(V2_FIXTURE)
        );
    }

    #[test]
    fn pinned_v3_fixture_decodes_and_round_trips_by_value() {
        let record = decode(V3_FIXTURE).expect("v3 fixture decodes");
        assert_eq!(record.schema_version, SCHEMA_V3);
        assert_eq!(record.intent.label, "v3 fixture");
        let universal = record
            .universal_intent
            .as_ref()
            .expect("v3 fixture has universal intent");
        assert_eq!(universal.command_kind(), UniversalCommandKind::CreateFile);
        assert_eq!(universal.user_status_key(), "operation.status.create-file");
        assert_eq!(universal.participants().descriptors().len(), 1);
        assert_eq!(
            value(&encode(&record).expect("v3 fixture encodes")),
            value(V3_FIXTURE)
        );
    }

    #[test]
    fn pinned_v4_cancellation_fixture_decodes_and_round_trips_by_value() {
        let fixture = v4_cancel_fixture();
        let bytes = serde_json::to_vec(&fixture).expect("v4 fixture encodes");
        let record = decode(&bytes).expect("v4 cancellation fixture decodes");
        assert_eq!(record.schema_version, SCHEMA_V4);
        assert_eq!(
            record.disposition,
            OperationDisposition::CancelRequestedBeforePublish
        );
        assert_eq!(
            record
                .cancellation_request()
                .expect("cancellation observation")
                .requested_unix_ms(),
            40
        );
        assert_eq!(classify_record(&record), RecordConsistency::Consistent);
        assert_eq!(
            value(&encode(&record).expect("v4 fixture encodes")),
            fixture
        );
    }

    #[test]
    fn pinned_v5_copy_fixture_decodes_and_round_trips_by_value() {
        let fixture = v5_copy_fixture();
        let bytes = serde_json::to_vec(&fixture).expect("v5 fixture encodes");
        let record = decode(&bytes).expect("v5 copy fixture decodes");
        assert_eq!(record.schema_version, SCHEMA_V5);
        assert_eq!(record.phase, OperationPhase::IntentDurable);
        assert_eq!(record.disposition, OperationDisposition::None);
        assert_eq!(classify_record(&record), RecordConsistency::Consistent);
        let args = record.command_args().expect("typed copy arguments");
        assert_eq!(args.source().relative_path().as_str(), "in.wav");
        assert_eq!(args.expected_source().source_revision(), 7);
        assert_eq!(args.expected_source().file_identity(), "file-identity");
        assert!(record.retry_observation().is_none());
        assert_eq!(
            value(&encode(&record).expect("v5 fixture encodes")),
            fixture
        );
    }

    #[test]
    fn v6_tagged_copy_import_and_duplicate_fixtures_round_trip() {
        for (variant, command_kind) in [
            ("Copy", UniversalCommandKind::Copy),
            ("Import", UniversalCommandKind::Import),
            ("DuplicateSame", UniversalCommandKind::Duplicate),
        ] {
            let fixture = v6_command_fixture(variant);
            let record = decode(&serde_json::to_vec(&fixture).expect("v6 fixture bytes"))
                .expect("v6 fixture decodes");
            assert_eq!(record.schema_version, SCHEMA_V6);
            assert_eq!(classify_record(&record), RecordConsistency::Consistent);
            assert_eq!(
                record
                    .universal_command_args()
                    .expect("tagged V6 arguments")
                    .command_kind(),
                command_kind
            );
            assert_eq!(
                value(&encode(&record).expect("v6 fixture encodes")),
                fixture
            );
        }
    }

    #[test]
    fn v7_waveform_edit_operations_round_trip_with_exact_shapes() {
        let operations = [
            (
                serde_json::json!({
                    "GainAndFades": {
                        "gain": 1065353216_u32,
                        "fade_in": null,
                        "fade_out": null
                    }
                }),
                (2, 8),
            ),
            (Value::from("Reverse"), (2, 8)),
            (Value::from("Mute"), (2, 8)),
            (Value::from("Trim"), (2, 8)),
            (Value::from("Crop"), (2, 8)),
            (serde_json::json!({"Slide": {"frame_offset": -3}}), (0, 10)),
            (
                serde_json::json!({
                    "CropWithSilence": {
                        "start_frame": -2,
                        "end_frame": 12
                    }
                }),
                (0, 10),
            ),
        ];

        for (operation, range) in operations {
            let fixture = v7_waveform_edit_fixture(operation, range);
            let record = decode(&serde_json::to_vec(&fixture).expect("V7 fixture bytes"))
                .expect("V7 waveform edit fixture decodes");
            assert_eq!(record.schema_version, SCHEMA_V7);
            assert_eq!(classify_record(&record), RecordConsistency::Consistent);
            assert!(matches!(
                record.universal_command_args(),
                Some(UniversalCommandArgs::WaveformEdit(_))
            ));
            assert_eq!(
                value(&encode(&record).expect("V7 fixture encodes")),
                fixture
            );
        }
    }

    #[test]
    fn v7_preserves_all_existing_v6_command_variants() {
        for variant in ["Copy", "Import", "DuplicateSame"] {
            let mut fixture = v6_command_fixture(variant);
            fixture["schema_version"] = Value::from(7_u64);
            let record = decode(&serde_json::to_vec(&fixture).expect("V7 command bytes"))
                .expect("V7 command fixture decodes");
            assert_eq!(record.schema_version, SCHEMA_V7);
            assert_eq!(classify_record(&record), RecordConsistency::Consistent);
            assert_eq!(
                value(&encode(&record).expect("V7 command encodes")),
                fixture
            );
        }
    }

    #[test]
    fn v8_namespace_commands_round_trip_and_preserve_existing_variants() {
        for variant in ["Rename", "Move", "CreateFolder"] {
            let fixture = v8_namespace_fixture(variant);
            let record = decode(&serde_json::to_vec(&fixture).expect("V8 namespace bytes"))
                .expect("V8 namespace fixture decodes");
            assert_eq!(record.schema_version, super::super::model::SCHEMA_V8);
            assert_eq!(classify_record(&record), RecordConsistency::Consistent);
            assert!(matches!(
                record.universal_command_args(),
                Some(UniversalCommandArgs::Namespace(_))
            ));
            assert_eq!(
                value(&encode(&record).expect("V8 namespace fixture encodes")),
                fixture
            );
        }
        for variant in ["Copy", "Import", "DuplicateSame"] {
            let mut fixture = v6_command_fixture(variant);
            fixture["schema_version"] = Value::from(8_u64);
            let record = decode(&serde_json::to_vec(&fixture).expect("V8 old command bytes"))
                .expect("V8 old command fixture decodes");
            assert_eq!(classify_record(&record), RecordConsistency::Consistent);
            assert_eq!(
                value(&encode(&record).expect("V8 old command encodes")),
                fixture
            );
        }
        let mut waveform_fixture = v7_waveform_edit_fixture(Value::from("Reverse"), (2, 8));
        waveform_fixture["schema_version"] = Value::from(8_u64);
        let waveform_record =
            decode(&serde_json::to_vec(&waveform_fixture).expect("V8 waveform bytes"))
                .expect("V8 waveform fixture decodes");
        assert_eq!(
            classify_record(&waveform_record),
            RecordConsistency::Consistent
        );
        assert_eq!(
            value(&encode(&waveform_record).expect("V8 waveform encodes")),
            waveform_fixture
        );
    }

    #[test]
    fn v9_render_commands_round_trip_with_explicit_outputs() {
        for variant in ["Normalize", "DuplicateDouble"] {
            let fixture = v9_render_fixture(variant);
            let bytes = serde_json::to_vec(&fixture).expect("V9 render bytes");
            let record = decode(&bytes).expect("V9 render fixture decodes");
            assert_eq!(record.schema_version, super::super::model::SCHEMA_V9);
            assert_eq!(classify_record(&record), RecordConsistency::Consistent);
            assert!(matches!(
                record.universal_command_args(),
                Some(UniversalCommandArgs::Render(_))
            ));
            assert_eq!(value(&encode(&record).expect("V9 render encodes")), fixture);
        }
    }

    #[test]
    fn v9_preserves_every_prior_command_variant_family() {
        let mut fixtures = Vec::new();
        for variant in ["Copy", "Import", "DuplicateSame"] {
            let mut fixture = v6_command_fixture(variant);
            fixture["schema_version"] = Value::from(9_u64);
            fixtures.push(fixture);
        }
        let mut waveform = v7_waveform_edit_fixture(Value::from("Reverse"), (2, 8));
        waveform["schema_version"] = Value::from(9_u64);
        fixtures.push(waveform);
        let mut namespace = v8_namespace_fixture("Rename");
        namespace["schema_version"] = Value::from(9_u64);
        fixtures.push(namespace);

        for fixture in fixtures {
            let record = decode(&serde_json::to_vec(&fixture).expect("V9 prior bytes"))
                .expect("V9 prior command decodes");
            assert_eq!(record.schema_version, SCHEMA_V9);
            assert_eq!(classify_record(&record), RecordConsistency::Consistent);
            assert_eq!(value(&encode(&record).expect("V9 prior encodes")), fixture);
        }
    }

    #[test]
    fn v9_render_codec_rejects_malformed_revision_path_output_and_arithmetic() {
        let mut invalid_revision = v9_render_fixture("Normalize");
        invalid_revision["universal_command_args"]["Render"]["Normalize"]["source_file_revision"]
            ["filesystem_identity"] = Value::String(String::new());
        assert!(matches!(
            decode(&serde_json::to_vec(&invalid_revision).expect("invalid revision bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("render file revision")
        ));

        let mut invalid_path = v9_render_fixture("Normalize");
        invalid_path["universal_command_args"]["Render"]["Normalize"]["source"]["relative_path"] =
            Value::from("../in.wav");
        assert!(matches!(
            decode(&serde_json::to_vec(&invalid_path).expect("invalid path bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("render endpoint relative path")
        ));

        let mut invalid_output = v9_render_fixture("DuplicateDouble");
        invalid_output["universal_command_args"]["Render"]["DuplicateDouble"]["output"]["bits_per_sample"] =
            Value::from(8_u64);
        assert!(matches!(
            decode(&serde_json::to_vec(&invalid_output).expect("invalid output bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("preserve source specification")
        ));

        let mut arithmetic_overflow = v9_render_fixture("Normalize");
        arithmetic_overflow["universal_command_args"]["Render"]["Normalize"]["total_frames"] =
            Value::from(1_100_000_000_u64);
        assert!(matches!(
            decode(&serde_json::to_vec(&arithmetic_overflow).expect("overflow bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("output arithmetic")
        ));
    }

    #[test]
    fn v10_removal_round_trip_and_prior_schema_rejection_are_explicit() {
        let fixture = v10_trash_fixture();
        let bytes = serde_json::to_vec(&fixture).expect("V10 removal bytes");
        let record = decode(&bytes).expect("V10 removal fixture decodes");
        assert_eq!(record.schema_version, SCHEMA_V10);
        assert_eq!(classify_record(&record), RecordConsistency::Consistent);
        assert!(matches!(
            record.universal_command_args(),
            Some(UniversalCommandArgs::Removal(_))
        ));
        assert_eq!(
            value(&encode(&record).expect("V10 removal encodes")),
            fixture
        );

        for version in [SCHEMA_V6, SCHEMA_V7, SCHEMA_V8, SCHEMA_V9] {
            let mut older = fixture.clone();
            older["schema_version"] = Value::from(version);
            assert!(matches!(
                decode(&serde_json::to_vec(&older).expect("older removal bytes")),
                Err(DecodeError::Malformed { .. })
            ));
        }
    }

    #[test]
    fn v10_preserves_all_prior_command_families() {
        let mut fixtures = Vec::new();
        for variant in ["Copy", "Import", "DuplicateSame"] {
            let mut fixture = v6_command_fixture(variant);
            fixture["schema_version"] = Value::from(SCHEMA_V10);
            fixtures.push(fixture);
        }
        let mut waveform = v7_waveform_edit_fixture(Value::from("Reverse"), (2, 8));
        waveform["schema_version"] = Value::from(SCHEMA_V10);
        fixtures.push(waveform);
        let mut namespace = v8_namespace_fixture("Rename");
        namespace["schema_version"] = Value::from(SCHEMA_V10);
        fixtures.push(namespace);
        for variant in ["Normalize", "DuplicateDouble"] {
            let mut render = v9_render_fixture(variant);
            render["schema_version"] = Value::from(SCHEMA_V10);
            fixtures.push(render);
        }

        for fixture in fixtures {
            let record = decode(&serde_json::to_vec(&fixture).expect("V10 prior bytes"))
                .expect("V10 prior command decodes");
            assert_eq!(record.schema_version, SCHEMA_V10);
            assert_eq!(classify_record(&record), RecordConsistency::Consistent);
            assert_eq!(value(&encode(&record).expect("V10 prior encodes")), fixture);
        }
    }

    #[test]
    fn v10_removal_raw_boundary_rejects_unknown_fields_and_bad_root_or_path() {
        let mut unknown = v10_trash_fixture();
        unknown["universal_command_args"]["Removal"]["Trash"]["unexpected"] = Value::Bool(true);
        assert!(matches!(
            decode(&serde_json::to_vec(&unknown).expect("unknown field bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut bad_root = v10_trash_fixture();
        bad_root["universal_command_args"]["Removal"]["Trash"]["trash_root_identity"] =
            Value::String(String::new());
        assert!(matches!(
            decode(&serde_json::to_vec(&bad_root).expect("bad root bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut bad_path = v10_trash_fixture();
        bad_path["universal_command_args"]["Removal"]["Trash"]["source"]["relative_path"] =
            Value::String(String::from("../item.wav"));
        assert!(matches!(
            decode(&serde_json::to_vec(&bad_path).expect("bad path bytes")),
            Err(DecodeError::Malformed { .. })
        ));
    }

    #[test]
    fn v10_codec_rejects_file_retained_stage_and_known_self_trash_root() {
        let mut file_stage = v10_trash_fixture();
        let source = file_stage["universal_command_args"]["Removal"]["Trash"]["source"].clone();
        file_stage["universal_intent"]["command_kind"] = Value::from("Delete");
        file_stage["universal_command_args"] = serde_json::json!({
            "Removal": { "StageRetainedFolder": { "source": source } }
        });
        assert!(matches!(
            decode(&serde_json::to_vec(&file_stage).expect("file-stage bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("directory")
        ));

        let mut self_trash = v10_trash_fixture();
        self_trash["universal_command_args"]["Removal"]["Trash"]["source"]["expectation"]["RegularFile"]
            ["filesystem_identity"] = Value::from("trash-root");
        // A regular-file identity equal to the root is not the known directory-root case.
        assert!(decode(&serde_json::to_vec(&self_trash).expect("file identity bytes")).is_ok());

        self_trash["universal_command_args"]["Removal"]["Trash"]["source"]["expectation"] = serde_json::json!({
            "Directory": {
                "source_revision": 7,
                "filesystem_identity": "trash-root",
                "modified_unix_ns": null
            }
        });
        assert!(matches!(
            decode(&serde_json::to_vec(&self_trash).expect("self-trash bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("configured trash root")
        ));
    }

    #[test]
    fn v11_retained_resolution_restore_and_purge_round_trip() {
        for variant in ["Restore", "Purge"] {
            let fixture = v11_retained_fixture(variant);
            let record = decode(&serde_json::to_vec(&fixture).expect("V11 retained bytes"))
                .expect("V11 retained fixture decodes");
            assert_eq!(record.schema_version, SCHEMA_V11);
            assert_eq!(classify_record(&record), RecordConsistency::Consistent);
            assert!(matches!(
                record.universal_command_args(),
                Some(UniversalCommandArgs::RetainedResolution(_))
            ));
            assert_eq!(
                value(&encode(&record).expect("V11 retained encodes")),
                fixture
            );
        }
    }

    #[test]
    fn v11_preserves_all_prior_command_families() {
        let mut fixtures = Vec::new();
        for variant in ["Copy", "Import", "DuplicateSame"] {
            let mut fixture = v6_command_fixture(variant);
            fixture["schema_version"] = Value::from(SCHEMA_V11);
            fixtures.push(fixture);
        }
        let mut waveform = v7_waveform_edit_fixture(Value::from("Reverse"), (2, 8));
        waveform["schema_version"] = Value::from(SCHEMA_V11);
        fixtures.push(waveform);
        let mut namespace = v8_namespace_fixture("Rename");
        namespace["schema_version"] = Value::from(SCHEMA_V11);
        fixtures.push(namespace);
        for variant in ["Normalize", "DuplicateDouble"] {
            let mut render = v9_render_fixture(variant);
            render["schema_version"] = Value::from(SCHEMA_V11);
            fixtures.push(render);
        }
        let mut removal = v10_trash_fixture();
        removal["schema_version"] = Value::from(SCHEMA_V11);
        fixtures.push(removal);

        for fixture in fixtures {
            let record = decode(&serde_json::to_vec(&fixture).expect("V11 prior bytes"))
                .expect("V11 prior command decodes");
            assert_eq!(record.schema_version, SCHEMA_V11);
            assert_eq!(classify_record(&record), RecordConsistency::Consistent);
            assert_eq!(value(&encode(&record).expect("V11 prior encodes")), fixture);
        }
    }

    #[test]
    fn v11_retained_raw_boundary_rejects_malformed_observations_and_old_versions() {
        let fixture = v11_retained_fixture("Restore");
        let mut unknown = fixture.clone();
        unknown["universal_command_args"]["RetainedResolution"]["Restore"]["observation"]["unknown"] =
            Value::Null;
        assert!(matches!(
            decode(&serde_json::to_vec(&unknown).expect("unknown V11 field bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut bad_stage_path = fixture.clone();
        bad_stage_path["universal_command_args"]["RetainedResolution"]["Restore"]["observation"]
            ["staged"]["relative_path"] = Value::from("folder/retained");
        assert!(matches!(
            decode(&serde_json::to_vec(&bad_stage_path).expect("bad staged path bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut bad_alias = fixture.clone();
        bad_alias["universal_command_args"]["RetainedResolution"]["Restore"]["observation"]["original_relative"] =
            Value::from(".WAVECRATE_DELETE_STAGING/recreated");
        assert!(matches!(
            decode(&serde_json::to_vec(&bad_alias).expect("bad alias bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut bad_revision = fixture.clone();
        bad_revision["universal_command_args"]["RetainedResolution"]["Restore"]["observation"]["staged"]
            ["expectation"]["Directory"]["source_revision"] = Value::from(8_u64);
        assert!(matches!(
            decode(&serde_json::to_vec(&bad_revision).expect("bad revision bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        for version in [SCHEMA_V6, SCHEMA_V7, SCHEMA_V8, SCHEMA_V9, SCHEMA_V10] {
            let mut older = fixture.clone();
            older["schema_version"] = Value::from(version);
            assert!(matches!(
                decode(&serde_json::to_vec(&older).expect("old V11 bytes")),
                Err(DecodeError::Malformed { .. })
            ));

            let mut direct = decode(&serde_json::to_vec(&fixture).expect("V11 direct bytes"))
                .expect("V11 direct record");
            direct.schema_version = version;
            assert!(matches!(
                classify_record(&direct),
                RecordConsistency::Invalid(reason)
                    if reason.contains("schema-v11 retained resolution")
                        || reason.contains("schema-v10 removal")
            ));
            assert!(encode(&direct).is_err());
        }
    }

    #[test]
    fn v12_selection_normalize_round_trip_preserves_signed_revision_and_full_file_output() {
        let fixture = v12_selection_normalize_fixture();
        let record = decode(&serde_json::to_vec(&fixture).expect("V12 selection bytes"))
            .expect("V12 selection fixture decodes");
        assert_eq!(record.schema_version, SCHEMA_V12);
        assert_eq!(classify_record(&record), RecordConsistency::Consistent);
        let Some(UniversalCommandArgs::SelectionNormalize(args)) = record.universal_command_args()
        else {
            panic!("V12 selection arguments");
        };
        assert_eq!(args.selected_frames().expect("selected frames"), 6);
        assert_eq!(args.output_frames().expect("full-file frames"), 10);
        assert_eq!(args.edge_fade_frames(), 2);
        assert_eq!(
            args.source_file_revision().filesystem_identity(),
            "selection-file-identity"
        );
        assert_eq!(
            args.source_file_revision().change_marker(),
            "selection-change-marker"
        );
        assert_eq!(args.source_file_revision().byte_len(), 1024);
        assert_eq!(
            args.source_file_revision().modified_unix_ns(),
            Some(-12000000000)
        );
        assert_eq!(
            value(&encode(&record).expect("V12 selection encodes")),
            fixture
        );
    }

    #[test]
    fn v12_preserves_every_prior_command_variant_family() {
        let mut fixtures = Vec::new();
        for variant in ["Copy", "Import", "DuplicateSame"] {
            let mut fixture = v6_command_fixture(variant);
            fixture["schema_version"] = Value::from(SCHEMA_V12);
            fixtures.push(fixture);
        }
        let mut waveform = v7_waveform_edit_fixture(Value::from("Reverse"), (2, 8));
        waveform["schema_version"] = Value::from(SCHEMA_V12);
        fixtures.push(waveform);
        let mut namespace = v8_namespace_fixture("Rename");
        namespace["schema_version"] = Value::from(SCHEMA_V12);
        fixtures.push(namespace);
        for variant in ["Normalize", "DuplicateDouble"] {
            let mut render = v9_render_fixture(variant);
            render["schema_version"] = Value::from(SCHEMA_V12);
            fixtures.push(render);
        }
        let mut removal = v10_trash_fixture();
        removal["schema_version"] = Value::from(SCHEMA_V12);
        fixtures.push(removal);
        for variant in ["Restore", "Purge"] {
            let mut retained = v11_retained_fixture(variant);
            retained["schema_version"] = Value::from(SCHEMA_V12);
            fixtures.push(retained);
        }

        for fixture in fixtures {
            let record = decode(&serde_json::to_vec(&fixture).expect("V12 prior bytes"))
                .expect("V12 prior command decodes");
            assert_eq!(record.schema_version, SCHEMA_V12);
            assert_eq!(classify_record(&record), RecordConsistency::Consistent);
            assert_eq!(value(&encode(&record).expect("V12 prior encodes")), fixture);
        }
    }

    #[test]
    fn v12_selection_raw_boundary_rejects_malformed_fields_and_old_versions() {
        let fixture = v12_selection_normalize_fixture();
        let args = &mut fixture.clone();

        args["universal_command_args"]["SelectionNormalize"]["unknown"] = Value::Null;
        assert!(matches!(
            decode(&serde_json::to_vec(args).expect("unknown V12 selection bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut invalid_revision = fixture.clone();
        invalid_revision["universal_command_args"]["SelectionNormalize"]["source_file_revision"]
            ["filesystem_identity"] = Value::String(String::new());
        assert!(matches!(
            decode(&serde_json::to_vec(&invalid_revision).expect("invalid revision bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("selection Normalize revision")
        ));

        let mut invalid_path = fixture.clone();
        invalid_path["universal_command_args"]["SelectionNormalize"]["source"]["relative_path"] =
            Value::from("../sample.wav");
        assert!(matches!(
            decode(&serde_json::to_vec(&invalid_path).expect("invalid path bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("selection Normalize endpoint relative path")
        ));

        let mut mismatched_source = fixture.clone();
        mismatched_source["universal_command_args"]["SelectionNormalize"]["source"]["source_id"] =
            Value::from("other-source");
        assert!(matches!(
            decode(&serde_json::to_vec(&mismatched_source).expect("source fence bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("source participant")
        ));

        let mut empty_range = fixture.clone();
        empty_range["universal_command_args"]["SelectionNormalize"]["range"] =
            serde_json::json!({"start_frame": 4, "end_frame": 4});
        assert!(matches!(
            decode(&serde_json::to_vec(&empty_range).expect("empty range bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("frame range")
        ));

        let mut outside_range = fixture.clone();
        outside_range["universal_command_args"]["SelectionNormalize"]["range"] =
            serde_json::json!({"start_frame": 9, "end_frame": 11});
        assert!(matches!(
            decode(&serde_json::to_vec(&outside_range).expect("outside range bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("outside the source")
        ));

        let mut long_fade = fixture.clone();
        long_fade["universal_command_args"]["SelectionNormalize"]["edge_fade_frames"] =
            Value::from(4_u64);
        assert!(matches!(
            decode(&serde_json::to_vec(&long_fade).expect("long fade bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("edge fade")
        ));

        let mut invalid_output = fixture.clone();
        invalid_output["universal_command_args"]["SelectionNormalize"]["output"]["channels"] =
            Value::from(1_u64);
        assert!(matches!(
            decode(&serde_json::to_vec(&invalid_output).expect("invalid output bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("preserve source")
        ));

        let mut arithmetic_overflow = fixture.clone();
        arithmetic_overflow["universal_command_args"]["SelectionNormalize"]["total_frames"] =
            Value::from(1_100_000_000_u64);
        assert!(matches!(
            decode(&serde_json::to_vec(&arithmetic_overflow).expect("overflow bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("source frame count")
                || reason.contains("arithmetic")
        ));

        let decoded = decode(&serde_json::to_vec(&fixture).expect("V12 direct bytes"))
            .expect("V12 direct record");
        for schema in [
            SCHEMA_V6, SCHEMA_V7, SCHEMA_V8, SCHEMA_V9, SCHEMA_V10, SCHEMA_V11,
        ] {
            let mut direct = decoded.clone();
            direct.schema_version = schema;
            assert!(matches!(
                classify_record(&direct),
                RecordConsistency::Invalid(reason)
                    if reason.contains("schema-v12 selection Normalize")
            ));
            assert!(matches!(
                encode(&direct),
                Err(EncodeError::InvalidV6Record { .. })
                    | Err(EncodeError::InvalidV7Record { .. })
                    | Err(EncodeError::InvalidV8Record { .. })
                    | Err(EncodeError::InvalidV9Record { .. })
                    | Err(EncodeError::InvalidV10Record { .. })
                    | Err(EncodeError::InvalidV11Record { .. })
            ));

            let mut older = fixture.clone();
            older["schema_version"] = Value::from(schema);
            assert!(matches!(
                decode(&serde_json::to_vec(&older).expect("old V12 bytes")),
                Err(DecodeError::Malformed { .. })
            ));
        }
    }

    #[test]
    fn v3_and_v4_reject_retained_resolution_command_kinds_from_raw_and_direct_records() {
        let mut v3 = serde_json::from_slice::<Value>(V3_FIXTURE).expect("V3 fixture value");
        v3["universal_intent"]["command_kind"] = Value::from("RestoreRetained");
        assert!(matches!(
            decode(&serde_json::to_vec(&v3).expect("V3 retained bytes")),
            Err(DecodeError::Malformed { .. })
        ));
        let v3_record = StoredOperationRecord::from(
            serde_json::from_value::<PersistedOperationRecordV3>(v3).expect("V3 direct record"),
        );
        assert!(matches!(
            classify_record(&v3_record),
            RecordConsistency::Invalid(reason) if reason.contains("retained resolution")
        ));

        let mut v4 = serde_json::to_value(v4_cancel_fixture()).expect("V4 fixture value");
        v4["universal_intent"]["command_kind"] = Value::from("PurgeRetained");
        assert!(matches!(
            decode(&serde_json::to_vec(&v4).expect("V4 retained bytes")),
            Err(DecodeError::Malformed { .. })
        ));
        let v4_record = StoredOperationRecord::from(
            serde_json::from_value::<PersistedOperationRecordV4>(v4).expect("V4 direct record"),
        );
        assert!(matches!(
            classify_record(&v4_record),
            RecordConsistency::Invalid(reason) if reason.contains("retained resolution")
        ));
    }

    #[test]
    fn older_schemas_reject_render_arguments_in_json_and_direct_records() {
        let fixture = v9_render_fixture("Normalize");
        for schema in [6_u64, 7_u64, 8_u64] {
            let mut old_schema = fixture.clone();
            old_schema["schema_version"] = Value::from(schema);
            assert!(matches!(
                decode(&serde_json::to_vec(&old_schema).expect("foreign V9 bytes")),
                Err(DecodeError::Malformed { .. })
            ));
        }

        let mut direct =
            decode(&serde_json::to_vec(&fixture).expect("V9 bytes")).expect("V9 record");
        for schema in [
            super::super::model::SCHEMA_V6,
            super::super::model::SCHEMA_V7,
            super::super::model::SCHEMA_V8,
        ] {
            direct.schema_version = schema;
            assert!(matches!(
                classify_record(&direct),
                RecordConsistency::Invalid(reason) if reason.contains("schema-v9 render")
            ));
            let error = encode(&direct).expect_err("older schema must reject render");
            assert!(error.to_string().contains("schema-v9 render"));
        }
    }

    #[test]
    fn v6_and_v7_reject_namespace_arguments_in_decode_and_direct_records() {
        let fixture = v8_namespace_fixture("Rename");
        for schema in [6_u64, 7_u64] {
            let mut old_schema = fixture.clone();
            old_schema["schema_version"] = Value::from(schema);
            assert!(matches!(
                decode(&serde_json::to_vec(&old_schema).expect("foreign namespace bytes")),
                Err(DecodeError::Malformed { reason }) if reason.contains("unknown variant")
            ));
        }

        let mut direct =
            decode(&serde_json::to_vec(&fixture).expect("V8 bytes")).expect("V8 namespace record");
        direct.schema_version = super::super::model::SCHEMA_V7;
        assert!(matches!(
            classify_record(&direct),
            RecordConsistency::Invalid(reason) if reason.contains("schema-v8 namespace")
        ));
        assert!(matches!(
            encode(&direct),
            Err(EncodeError::InvalidV7Record { reason })
                if reason.contains("schema-v8 namespace")
        ));
        direct.schema_version = super::super::model::SCHEMA_V6;
        assert!(matches!(
            classify_record(&direct),
            RecordConsistency::Invalid(reason) if reason.contains("schema-v8 namespace")
        ));
        assert!(matches!(
            encode(&direct),
            Err(EncodeError::InvalidV6Record { reason })
                if reason.contains("schema-v8 namespace")
        ));
    }

    #[test]
    fn v8_namespace_nested_bounds_and_revision_mismatches_are_retained_as_malformed() {
        let mut invalid_leaf = v8_namespace_fixture("Rename");
        invalid_leaf["universal_command_args"]["Namespace"]["Rename"]["destination"]["leaf"] =
            Value::from("nested/name");
        assert!(matches!(
            decode(&serde_json::to_vec(&invalid_leaf).expect("invalid namespace bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("destination leaf")
        ));

        let mut invalid_revision = v8_namespace_fixture("Rename");
        invalid_revision["universal_command_args"]["Namespace"]["Rename"]["source"]["expectation"]
            ["RegularFile"]["source_revision"] = Value::from(8_u64);
        assert!(matches!(
            decode(&serde_json::to_vec(&invalid_revision).expect("revision mismatch bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("source revision")
        ));
    }

    #[test]
    fn v6_rejects_waveform_edit_arguments_in_decode_and_direct_records() {
        let fixture = v7_waveform_edit_fixture(Value::from("Reverse"), (2, 8));
        let mut old_schema = fixture.clone();
        old_schema["schema_version"] = Value::from(6_u64);
        assert!(matches!(
            decode(&serde_json::to_vec(&old_schema).expect("foreign V7 bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("schema-v7 waveform edit")
        ));

        let mut direct =
            decode(&serde_json::to_vec(&fixture).expect("V7 bytes")).expect("V7 record");
        direct.schema_version = SCHEMA_V6;
        assert!(matches!(
            classify_record(&direct),
            RecordConsistency::Invalid(reason) if reason.contains("schema-v7 waveform edit")
        ));
        assert!(matches!(
            encode(&direct),
            Err(EncodeError::InvalidV6Record { reason })
                if reason.contains("schema-v7 waveform edit")
        ));
    }

    #[test]
    fn v7_waveform_shape_rejects_empty_ranges_and_invalid_wav_layouts() {
        let mut empty_range = v7_waveform_edit_fixture(Value::from("Reverse"), (2, 8));
        empty_range["universal_command_args"]["WaveformEdit"]["range"]["end_frame"] =
            Value::from(2_u64);
        assert!(matches!(
            decode(&serde_json::to_vec(&empty_range).expect("empty-range bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("frame range")
        ));

        let mut source_block_align = v7_waveform_edit_fixture(Value::from("Mute"), (2, 8));
        source_block_align["universal_command_args"]["WaveformEdit"]["source_spec"]["channels"] =
            Value::from(u64::from(u16::MAX));
        source_block_align["universal_command_args"]["WaveformEdit"]["output"]["channels"] =
            Value::from(u64::from(u16::MAX));
        assert!(matches!(
            decode(&serde_json::to_vec(&source_block_align).expect("block-align bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("source specification")
        ));

        let mut source_byte_rate = v7_waveform_edit_fixture(Value::from("Mute"), (2, 8));
        source_byte_rate["universal_command_args"]["WaveformEdit"]["source_spec"]["sample_rate"] =
            Value::from(u64::from(u32::MAX));
        source_byte_rate["universal_command_args"]["WaveformEdit"]["output"]["sample_rate"] =
            Value::from(u64::from(u32::MAX));
        assert!(matches!(
            decode(&serde_json::to_vec(&source_byte_rate).expect("byte-rate bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("source specification")
        ));

        let mut output_block_align = v7_waveform_edit_fixture(Value::from("Mute"), (2, 8));
        output_block_align["universal_command_args"]["WaveformEdit"]["source_spec"]["channels"] =
            Value::from(20000_u64);
        output_block_align["universal_command_args"]["WaveformEdit"]["source_spec"]["bits_per_sample"] =
            Value::from(8_u64);
        output_block_align["universal_command_args"]["WaveformEdit"]["output"]["channels"] =
            Value::from(20000_u64);
        assert!(matches!(
            decode(&serde_json::to_vec(&output_block_align).expect("output layout bytes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("output specification")
        ));
    }

    #[test]
    fn partial_reverse_and_mute_still_account_for_the_whole_source_output() {
        for operation in ["Reverse", "Mute"] {
            let mut fixture = v7_waveform_edit_fixture(Value::from(operation), (1, 2));
            fixture["universal_command_args"]["WaveformEdit"]["total_frames"] =
                Value::from(1_100_000_000_u64);
            assert!(matches!(
                decode(&serde_json::to_vec(&fixture).expect("large-output bytes")),
                Err(DecodeError::Malformed { reason }) if reason.contains("output arithmetic")
            ));
        }
    }

    #[test]
    fn v6_command_shape_rejects_foreign_fields_unknown_variants_and_role_mismatches() {
        let mut old_copy_field = v6_command_fixture("Copy");
        old_copy_field["command_args"] = old_copy_field["universal_command_args"]["Copy"].clone();
        assert!(matches!(
            decode(&serde_json::to_vec(&old_copy_field).expect("foreign field bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut old_schema_field = v5_copy_fixture();
        old_schema_field["universal_command_args"] = serde_json::json!({
            "Copy": old_schema_field["command_args"].clone()
        });
        assert!(matches!(
            decode(&serde_json::to_vec(&old_schema_field).expect("foreign V6 field bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut unknown_variant = v6_command_fixture("Copy");
        unknown_variant["universal_command_args"] = serde_json::json!({
            "DuplicateDouble": {}
        });
        assert!(matches!(
            decode(&serde_json::to_vec(&unknown_variant).expect("unknown variant bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut mismatched_kind = v6_command_fixture("Copy");
        mismatched_kind["universal_intent"]["command_kind"] = Value::from("Duplicate");
        assert!(matches!(
            decode(&serde_json::to_vec(&mismatched_kind).expect("mismatched kind bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut wrong_role = v6_command_fixture("Import");
        wrong_role["universal_command_args"]["Import"]["filesystem_participant_id"] =
            wrong_role["universal_intent"]["participants"]["descriptors"][0]["id"].clone();
        assert!(matches!(
            decode(&serde_json::to_vec(&wrong_role).expect("wrong role bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut invalid_destination = v6_command_fixture("Import");
        invalid_destination["universal_command_args"]["Import"]["destination"]["source_id"] =
            Value::from("s".repeat(super::super::participants::MAX_SOURCE_IDENTITY_BYTES + 1));
        assert!(matches!(
            decode(&serde_json::to_vec(&invalid_destination).expect("invalid destination bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut invalid_import_root = v6_command_fixture("Import");
        invalid_import_root["universal_command_args"]["Import"]["root_identity"] =
            Value::from("r".repeat(crate::operation_journal::MAX_IMPORT_ROOT_IDENTITY_BYTES + 1));
        assert!(matches!(
            decode(&serde_json::to_vec(&invalid_import_root).expect("invalid root bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut invalid_import_path = v6_command_fixture("Import");
        invalid_import_path["universal_command_args"]["Import"]["relative_path"] =
            Value::from("../sample.wav");
        assert!(matches!(
            decode(&serde_json::to_vec(&invalid_import_path).expect("invalid import path bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut duplicate_noop = v6_command_fixture("DuplicateSame");
        duplicate_noop["universal_command_args"]["DuplicateSame"]["destination"] =
            duplicate_noop["universal_command_args"]["DuplicateSame"]["source"].clone();
        assert!(matches!(
            decode(&serde_json::to_vec(&duplicate_noop).expect("duplicate no-op bytes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut old_v3_import = value(V3_FIXTURE);
        old_v3_import["universal_intent"]["command_kind"] = Value::from("Import");
        assert!(matches!(
            decode(&serde_json::to_vec(&old_v3_import).expect("V3 import bytes")),
            Err(DecodeError::Malformed { .. })
        ));
        let mut old_v4_import = v4_cancel_fixture();
        old_v4_import["universal_intent"]["command_kind"] = Value::from("Import");
        assert!(matches!(
            decode(&serde_json::to_vec(&old_v4_import).expect("V4 import bytes")),
            Err(DecodeError::Malformed { .. })
        ));
    }

    #[test]
    fn prior_schema_record_cannot_carry_direct_v6_arguments() {
        let v6 = decode(&serde_json::to_vec(&v6_command_fixture("Copy")).expect("V6 bytes"))
            .expect("V6 record");
        let mut v5 =
            decode(&serde_json::to_vec(&v5_copy_fixture()).expect("V5 bytes")).expect("V5 record");
        v5.universal_command_args = v6.universal_command_args().cloned();
        assert!(matches!(
            classify_record(&v5),
            RecordConsistency::Invalid(reason) if reason.contains("V6 tagged command arguments")
        ));
        assert!(matches!(
            encode(&v5),
            Err(EncodeError::InvalidV5Record { reason })
                if reason.contains("V6 tagged command arguments")
        ));

        let mut v3 = decode(V3_FIXTURE).expect("V3 record");
        v3.universal_command_args = v6.universal_command_args().cloned();
        assert!(matches!(
            classify_record(&v3),
            RecordConsistency::Invalid(reason)
                if reason.contains("V6 tagged command arguments")
        ));

        let mut v4 = decode(&serde_json::to_vec(&v4_cancel_fixture()).expect("V4 bytes"))
            .expect("V4 record");
        v4.universal_command_args = v6.universal_command_args().cloned();
        assert!(matches!(
            classify_record(&v4),
            RecordConsistency::Invalid(reason)
                if reason.contains("V6 tagged command arguments")
        ));
    }

    #[test]
    fn v5_copy_shape_is_strict_and_rejects_inconsistent_retry_chronology() {
        let mut unknown = v5_copy_fixture();
        unknown["unexpected"] = Value::Bool(true);
        assert!(matches!(
            decode(&serde_json::to_vec(&unknown).expect("unknown field encodes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut opaque_payload = v5_copy_fixture();
        opaque_payload["payload"] = serde_json::json!({"future": true});
        assert!(matches!(
            decode(&serde_json::to_vec(&opaque_payload).expect("payload encodes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut invalid_path = v5_copy_fixture();
        invalid_path["command_args"]["source"]["relative_path"] = Value::from("../in.wav");
        assert!(matches!(
            decode(&serde_json::to_vec(&invalid_path).expect("invalid path encodes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("relative path")
        ));

        let mut retry_after_cancel = v5_copy_fixture();
        retry_after_cancel["disposition"] = Value::from("CancelRequestedBeforePublish");
        retry_after_cancel["cancellation_request"] = serde_json::json!({
            "requested_unix_ms": 60
        });
        retry_after_cancel["retry_observation"] = serde_json::json!({
            "attempt": 1,
            "requested_unix_ms": 70,
            "next_retry_unix_ms": 71,
            "lease_id": "lease-1",
            "lease_expiry_unix_ms": 72,
            "error_class": "TransientContention",
            "diagnostic_code": "busy",
            "redacted_context": null
        });
        retry_after_cancel["updated_unix_ms"] = Value::from(60_i64);
        assert!(matches!(
            decode(&serde_json::to_vec(&retry_after_cancel).expect("retry fixture encodes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("creation-to-cancellation")
        ));

        let mut retry_before_creation = v5_copy_fixture();
        retry_before_creation["disposition"] = Value::from("CancelRequestedBeforePublish");
        retry_before_creation["cancellation_request"] = serde_json::json!({
            "requested_unix_ms": 60
        });
        retry_before_creation["retry_observation"] = serde_json::json!({
            "attempt": 1,
            "requested_unix_ms": 40,
            "next_retry_unix_ms": 41,
            "lease_id": "lease-1",
            "lease_expiry_unix_ms": 42,
            "error_class": "TransientContention",
            "diagnostic_code": "busy",
            "redacted_context": null
        });
        retry_before_creation["updated_unix_ms"] = Value::from(60_i64);
        assert!(matches!(
            decode(&serde_json::to_vec(&retry_before_creation).expect("retry fixture encodes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("creation-to-cancellation")
        ));
    }

    #[test]
    fn v4_cancellation_shape_is_strict_and_bounded() {
        let mut unknown = v4_cancel_fixture();
        unknown["unexpected"] = Value::Bool(true);
        assert!(matches!(
            decode(&serde_json::to_vec(&unknown).expect("unknown field encodes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut missing = v4_cancel_fixture();
        missing
            .as_object_mut()
            .expect("v4 object")
            .remove("cancellation_request");
        assert!(matches!(
            decode(&serde_json::to_vec(&missing).expect("missing field encodes")),
            Err(DecodeError::Malformed { .. })
        ));

        let mut negative = v4_cancel_fixture();
        negative["cancellation_request"]["requested_unix_ms"] = Value::from(-1_i64);
        assert!(matches!(
            decode(&serde_json::to_vec(&negative).expect("negative timestamp encodes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("non-negative")
        ));

        let mut mismatched_update = v4_cancel_fixture();
        mismatched_update["updated_unix_ms"] = Value::from(39_i64);
        assert!(matches!(
            decode(&serde_json::to_vec(&mismatched_update).expect("timestamp mismatch encodes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("equal")
        ));

        let mut wrong_disposition = v4_cancel_fixture();
        wrong_disposition["disposition"] = Value::from("None");
        assert!(matches!(
            decode(&serde_json::to_vec(&wrong_disposition).expect("wrong disposition encodes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("CancelRequestedBeforePublish")
        ));

        let mut external = v4_cancel_fixture();
        external["intent"]["actor"] = Value::from("ExternalFs");
        external["intent"]["kind"] = Value::from("ExternalReconciliation");
        external["universal_intent"]["command_kind"] = Value::from("ExternalReconciliation");
        assert!(matches!(
            decode(&serde_json::to_vec(&external).expect("external actor encodes")),
            Err(DecodeError::Malformed { reason }) if reason.contains("user intent")
        ));
    }

    #[test]
    fn v4_bounded_encoder_validates_large_payload_without_cloning_the_record() {
        let fixture = v4_cancel_fixture();
        let mut record = decode(&serde_json::to_vec(&fixture).expect("v4 fixture encodes"))
            .expect("v4 fixture decodes");
        let mut payload = Map::new();
        for index in 0..4_096 {
            payload.insert(format!("key-{index}"), Value::String("x".repeat(256)));
        }
        record.payload = Value::Object(payload);
        assert!(matches!(
            encode(&record),
            Err(EncodeError::Oversize { maximum, .. }) if maximum == MAX_RECORD_BYTES
        ));
    }

    #[test]
    fn classification_rejects_a_v4_cancellation_observation_under_a_legacy_schema() {
        let fixture = v4_cancel_fixture();
        let mut record = decode(&serde_json::to_vec(&fixture).expect("v4 fixture encodes"))
            .expect("v4 fixture decodes");
        record.schema_version = SCHEMA_V1;
        record.universal_intent = None;
        assert!(matches!(
            classify_record(&record),
            RecordConsistency::Invalid(reason) if reason.contains("cancellation request")
        ));
    }

    #[test]
    fn classification_rejects_foreign_v5_fields_under_v3_and_v4() {
        let fixture = v5_copy_fixture();
        let mut record = decode(&serde_json::to_vec(&fixture).expect("v5 fixture encodes"))
            .expect("v5 fixture decodes");

        record.schema_version = SCHEMA_V3;
        record.disposition = OperationDisposition::None;
        record.updated_unix_ms = record.created_unix_ms;
        assert!(matches!(
            classify_record(&record),
            RecordConsistency::Invalid(reason) if reason.contains("typed coordinator")
        ));
        assert!(matches!(
            encode(&record),
            Err(EncodeError::CoordinatorFieldsInPriorSchema {
                version: SCHEMA_V3,
                ..
            })
        ));

        record.schema_version = SCHEMA_V4;
        record.disposition = OperationDisposition::CancelRequestedBeforePublish;
        record.cancellation_request = Some(
            CancellationRequestObservation::new(record.created_unix_ms)
                .expect("cancellation timestamp"),
        );
        assert!(matches!(
            classify_record(&record),
            RecordConsistency::Invalid(reason) if reason.contains("typed coordinator")
        ));
        assert!(matches!(
            encode(&record),
            Err(EncodeError::CoordinatorFieldsInPriorSchema {
                version: SCHEMA_V4,
                ..
            })
        ));

        record.command_args = None;
        record.retry_observation = Some(
            serde_json::from_value(serde_json::json!({
                "attempt": 1,
                "requested_unix_ms": 50,
                "next_retry_unix_ms": 51,
                "lease_id": "lease-1",
                "lease_expiry_unix_ms": 52,
                "error_class": "TransientContention",
                "diagnostic_code": "busy",
                "redacted_context": null
            }))
            .expect("retry observation"),
        );
        assert!(matches!(
            classify_record(&record),
            RecordConsistency::Invalid(reason) if reason.contains("retry observation")
        ));
    }

    #[test]
    fn v3_requires_all_universal_fields_and_rejects_legacy_evidence() {
        let mut missing_parent = value(V3_FIXTURE);
        missing_parent["universal_intent"]
            .as_object_mut()
            .expect("universal object")
            .remove("parent_operation_id");
        let missing_parent_bytes = serde_json::to_vec(&missing_parent).expect("fixture encodes");
        assert!(matches!(
            decode(&missing_parent_bytes),
            Err(DecodeError::Malformed { .. })
        ));

        let mut legacy_evidence = value(V3_FIXTURE);
        legacy_evidence["published"] = Value::Null;
        let legacy_evidence_bytes = serde_json::to_vec(&legacy_evidence).expect("fixture encodes");
        assert!(matches!(
            decode(&legacy_evidence_bytes),
            Err(DecodeError::Malformed { .. })
        ));

        let mut missing_capacity_field = value(V3_FIXTURE);
        missing_capacity_field["capacity_plan"]["volumes"][0]
            .as_object_mut()
            .expect("capacity volume object")
            .remove("protected_free_bytes");
        let missing_capacity_bytes =
            serde_json::to_vec(&missing_capacity_field).expect("fixture encodes");
        assert!(matches!(
            decode(&missing_capacity_bytes),
            Err(DecodeError::Malformed { .. })
        ));

        let mut unknown_participant = value(V3_FIXTURE);
        unknown_participant["universal_intent"]["participants"]["descriptors"][0]["unexpected"] =
            Value::Bool(true);
        let unknown_participant_bytes =
            serde_json::to_vec(&unknown_participant).expect("fixture encodes");
        assert!(matches!(
            decode(&unknown_participant_bytes),
            Err(DecodeError::Malformed { .. })
        ));
    }

    #[test]
    fn v3_initial_shape_rejects_incompatible_command_and_non_object_payload() {
        let mut incompatible = value(V3_FIXTURE);
        incompatible["universal_intent"]["command_kind"] = Value::from("Undo");
        let incompatible_bytes = serde_json::to_vec(&incompatible).expect("fixture encodes");
        assert!(matches!(
            decode(&incompatible_bytes),
            Err(DecodeError::Malformed { reason }) if reason.contains("incompatible")
        ));

        let mut non_object_payload = value(V3_FIXTURE);
        non_object_payload["payload"] = Value::Array(Vec::new());
        let non_object_payload_bytes =
            serde_json::to_vec(&non_object_payload).expect("fixture encodes");
        assert!(matches!(
            decode(&non_object_payload_bytes),
            Err(DecodeError::Malformed { reason }) if reason.contains("payload")
        ));

        let mut later_phase = value(V3_FIXTURE);
        later_phase["phase"] = Value::from("Prepared");
        let later_phase_bytes = serde_json::to_vec(&later_phase).expect("fixture encodes");
        assert!(matches!(
            decode(&later_phase_bytes),
            Err(DecodeError::Malformed { reason }) if reason.contains("IntentDurable")
        ));

        let mut later_disposition = value(V3_FIXTURE);
        later_disposition["disposition"] = Value::from("RetryPending");
        let later_disposition_bytes =
            serde_json::to_vec(&later_disposition).expect("fixture encodes");
        assert!(matches!(
            decode(&later_disposition_bytes),
            Err(DecodeError::Malformed { reason }) if reason.contains("None disposition")
        ));

        let mut long_status = value(V3_FIXTURE);
        long_status["universal_intent"]["user_status_key"] =
            Value::String("x".repeat(MAX_USER_STATUS_KEY_BYTES + 1));
        let long_status_bytes = serde_json::to_vec(&long_status).expect("fixture encodes");
        assert!(matches!(
            decode(&long_status_bytes),
            Err(DecodeError::Malformed { reason }) if reason.contains("status key")
        ));

        let mut long_label = value(V3_FIXTURE);
        long_label["intent"]["label"] = Value::String("x".repeat(MAX_UNIVERSAL_LABEL_BYTES + 1));
        let long_label_bytes = serde_json::to_vec(&long_label).expect("fixture encodes");
        assert!(matches!(
            decode(&long_label_bytes),
            Err(DecodeError::Malformed { reason }) if reason.contains("label")
        ));

        let mut self_parent = value(V3_FIXTURE);
        self_parent["universal_intent"]["parent_operation_id"] =
            Value::from("01234567-89ab-cdef-0123-456789abcdef");
        let self_parent_bytes = serde_json::to_vec(&self_parent).expect("fixture encodes");
        assert!(matches!(
            decode(&self_parent_bytes),
            Err(DecodeError::Malformed { reason }) if reason.contains("itself")
        ));

        let mut invalid_timestamps = value(V3_FIXTURE);
        invalid_timestamps["created_unix_ms"] = Value::from(10_i64);
        invalid_timestamps["updated_unix_ms"] = Value::from(9_i64);
        let invalid_timestamps_bytes =
            serde_json::to_vec(&invalid_timestamps).expect("fixture encodes");
        assert!(matches!(
            decode(&invalid_timestamps_bytes),
            Err(DecodeError::Malformed { reason }) if reason.contains("precede")
        ));

        let mut nonpending_checkpoint = value(V3_FIXTURE);
        nonpending_checkpoint["universal_intent"]["participants"]["checkpoints"][0]["state"] =
            Value::from("Applied");
        let nonpending_bytes = serde_json::to_vec(&nonpending_checkpoint).expect("fixture encodes");
        assert!(matches!(
            decode(&nonpending_bytes),
            Err(DecodeError::Malformed { reason }) if reason.contains("Pending")
        ));
    }

    #[test]
    fn v3_record_is_consistent_but_legacy_schema_cannot_carry_it() {
        let record = decode(V3_FIXTURE).expect("v3 fixture decodes");
        assert_eq!(classify_record(&record), RecordConsistency::Consistent);

        let mut legacy = record;
        legacy.schema_version = SCHEMA_V1;
        assert!(matches!(
            classify_record(&legacy),
            RecordConsistency::Invalid(reason) if reason.contains("legacy")
        ));
    }

    #[test]
    fn v3_rejects_excessive_depth_and_nested_capacity_unknown_fields() {
        let mut deep_payload = Value::Null;
        for _ in 0..65 {
            deep_payload = Value::Array(vec![deep_payload]);
        }
        let mut deep = value(V3_FIXTURE);
        deep["payload"] = serde_json::json!({"deep": deep_payload});
        let deep_bytes = serde_json::to_vec(&deep).expect("fixture encodes");
        assert!(matches!(
            decode(&deep_bytes),
            Err(DecodeError::Malformed { reason }) if reason.contains("nesting")
        ));

        let mut unknown_capacity = value(V3_FIXTURE);
        unknown_capacity["capacity_plan"]["volumes"][0]["unexpected"] = Value::Bool(true);
        let unknown_capacity_bytes =
            serde_json::to_vec(&unknown_capacity).expect("fixture encodes");
        assert!(matches!(
            decode(&unknown_capacity_bytes),
            Err(DecodeError::Malformed { .. })
        ));

        let mut encoded_depth = decode(V3_FIXTURE).expect("v3 fixture decodes");
        encoded_depth.payload = serde_json::json!({"deep": deep["payload"]["deep"].clone()});
        assert!(matches!(
            encode(&encoded_depth),
            Err(EncodeError::InvalidV3Record { reason }) if reason.contains("nesting")
        ));
    }

    #[test]
    fn v3_encoding_never_drops_universal_or_legacy_evidence() {
        let mut record = decode(V3_FIXTURE).expect("v3 fixture decodes");
        record.schema_version = SCHEMA_V1;
        assert_eq!(
            encode(&record),
            Err(EncodeError::UniversalIntentInLegacy { version: SCHEMA_V1 })
        );
        record.schema_version = SCHEMA_V2;
        assert_eq!(
            encode(&record),
            Err(EncodeError::UniversalIntentInLegacy { version: SCHEMA_V2 })
        );
        record.schema_version = SCHEMA_V3;
        record.universal_intent = None;
        assert_eq!(encode(&record), Err(EncodeError::MissingV3UniversalIntent));
    }

    #[test]
    fn unsupported_version_is_distinct_from_malformed_data() {
        let mut fixture = value(V1_FIXTURE);
        fixture["schema_version"] = Value::from(99_u64);
        let bytes = serde_json::to_vec(&fixture).expect("unsupported fixture encodes");
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::UnsupportedVersion { version: 99 })
        );
    }

    #[test]
    fn malformed_json_is_distinct_from_unsupported_version() {
        assert!(matches!(
            decode(br#"{"schema_version":1,"#),
            Err(DecodeError::Malformed { .. })
        ));
    }

    #[test]
    fn trailing_json_value_is_malformed() {
        let mut fixture = V1_FIXTURE.to_vec();
        fixture.extend_from_slice(b" {}");
        assert!(matches!(
            decode(&fixture),
            Err(DecodeError::Malformed { .. })
        ));
    }

    #[test]
    fn excessive_json_nesting_is_malformed() {
        let mut fixture = Vec::with_capacity(260);
        fixture.extend(std::iter::repeat_n(b'[', 129));
        fixture.push(b'0');
        fixture.extend(std::iter::repeat_n(b']', 129));
        assert!(matches!(
            decode(&fixture),
            Err(DecodeError::Malformed { .. })
        ));
    }

    #[test]
    fn oversize_input_is_rejected_before_json_parsing() {
        let bytes = vec![b'{'; MAX_RECORD_BYTES + 1];
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::Oversize {
                actual: MAX_RECORD_BYTES + 1,
                maximum: MAX_RECORD_BYTES,
            })
        );
    }

    #[test]
    fn capped_writer_rejects_oversize_output() {
        let mut record = decode(V1_FIXTURE).expect("v1 fixture decodes");
        record.payload = Value::String("x".repeat(MAX_RECORD_BYTES));
        assert!(matches!(
            encode(&record),
            Err(EncodeError::Oversize { maximum, .. }) if maximum == MAX_RECORD_BYTES
        ));
    }

    #[test]
    fn v3_wide_payload_is_rejected_with_bounded_encode_work() {
        let mut record = decode(V3_FIXTURE).expect("v3 fixture decodes");
        let mut payload = Map::new();
        for index in 0..4_096 {
            payload.insert(format!("key-{index}"), Value::String("x".repeat(256)));
        }
        record.payload = Value::Object(payload);

        assert!(matches!(
            encode(&record),
            Err(EncodeError::Oversize { maximum, .. }) if maximum == MAX_RECORD_BYTES
        ));
    }

    #[test]
    fn unknown_nested_field_is_malformed() {
        let mut fixture = value(V1_FIXTURE);
        fixture["intent"]["unexpected"] = Value::Bool(true);
        let bytes = serde_json::to_vec(&fixture).expect("mutated fixture encodes");
        assert!(matches!(decode(&bytes), Err(DecodeError::Malformed { .. })));
    }

    #[test]
    fn duplicate_operation_identity_is_malformed() {
        let duplicate = br#"{
  "schema_version": 1,
  "operation_id": "01234567-89ab-cdef-0123-456789abcdef",
  "operation_id": "fedcba98-7654-3210-fedc-ba9876543210"
}"#;
        assert!(matches!(
            decode(duplicate),
            Err(DecodeError::Malformed { reason }) if reason.contains("duplicate")
        ));
    }

    #[test]
    fn v1_rejects_v2_only_top_level_evidence() {
        let mut fixture = value(V1_FIXTURE);
        fixture["absent_final_recovery_observation"] = Value::Null;
        let bytes = serde_json::to_vec(&fixture).expect("mutated fixture encodes");
        assert!(matches!(decode(&bytes), Err(DecodeError::Malformed { .. })));
    }

    #[test]
    fn v1_rejects_v2_only_absent_final_publication_evidence() {
        let mut fixture = value(V1_FIXTURE);
        fixture["published"] = value(ABSENT_FINAL_PUBLICATION_FIXTURE);
        let bytes = serde_json::to_vec(&fixture).expect("mutated fixture encodes");
        assert!(matches!(
            decode(&bytes),
            Err(DecodeError::Malformed { reason })
                if reason.contains("absent-final publication")
        ));
    }

    #[test]
    fn v1_encode_rejects_v2_only_absent_final_recovery_evidence() {
        let mut record = decode(V1_FIXTURE).expect("v1 fixture decodes");
        record.absent_final_recovery_observation = Some(
            serde_json::from_slice(ABSENT_FINAL_RECOVERY_FIXTURE)
                .expect("absent-final recovery fixture decodes"),
        );
        assert!(matches!(
            encode(&record),
            Err(EncodeError::V2EvidenceInV1 { kind })
                if kind.contains("recovery observation")
        ));
    }
}
