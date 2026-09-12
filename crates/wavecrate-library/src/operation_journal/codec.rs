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

use super::evidence::{
    AbsentFinalAdoptionEvidence, AbsentFinalRecoveryObservation, AbsentFinalTransactionOwnedProof,
    FilesystemPublishedWaveformRestore, FilesystemStagedWaveformRestore, FinalNamespaceClaim,
    PreparedAbsentFinalNoReplace, PreparedTargetContract, PreparedWaveformRestore,
    ReplacementQualificationAssessment,
};
use super::model::{
    MAX_RECORD_BYTES, OperationDisposition, OperationId, OperationIntent, OperationPhase,
    SCHEMA_V1, SCHEMA_V2,
};
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
    /// Stored creation timestamp in Unix milliseconds.
    pub created_unix_ms: i64,
    /// Stored update timestamp in Unix milliseconds.
    pub updated_unix_ms: i64,
}

impl StoredOperationRecord {
    /// Borrow stored publication evidence without exposing a construction path for it.
    pub fn published(&self) -> Option<&FilesystemPublishedWaveformRestore> {
        self.published.as_ref()
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
            created_unix_ms: record.created_unix_ms,
            updated_unix_ms: record.updated_unix_ms,
        }
    }
}

fn persisted_v1_from_record(
    record: &StoredOperationRecord,
) -> Result<PersistedOperationRecordV1Ref<'_>, EncodeError> {
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
            created_unix_ms: record.created_unix_ms,
            updated_unix_ms: record.updated_unix_ms,
        }
    }
}

fn persisted_v2_from_record(record: &StoredOperationRecord) -> PersistedOperationRecordV2Ref<'_> {
    PersistedOperationRecordV2Ref {
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
    }
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
/// Encoding preserves the exact V1/V2 field and enum representation. It does not perform phase or
/// publication-policy validation; those checks belong to the later transition/owner boundary.
pub fn encode(record: &StoredOperationRecord) -> Result<Vec<u8>, EncodeError> {
    let mut writer = CappedWriter::new(MAX_RECORD_BYTES);
    let result = match record.schema_version {
        SCHEMA_V1 => {
            let persisted = persisted_v1_from_record(record)?;
            serde_json::to_writer_pretty(&mut writer, &persisted)
        }
        SCHEMA_V2 => {
            let persisted = persisted_v2_from_record(record);
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

#[cfg(test)]
mod tests {
    use super::*;

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
