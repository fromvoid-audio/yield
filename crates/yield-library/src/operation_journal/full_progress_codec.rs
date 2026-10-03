//! Strict, bounded JSON transport for validated full-progress observations.
//!
//! This codec is intentionally separate from the legacy operation-journal schemas.  It carries
//! only a [`FullProgressSnapshotObservation`] inside an observation envelope; decoding never
//! reconstructs live progress, an owner, a permit, or any other execution authority.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::codec::{CappedWriter, parse_unique_value, validate_json_depth};
use super::work_progress::{FullProgressSnapshotObservation, FullProgressSnapshotObservationError};

/// The separate full-progress observation envelope kind.
pub(crate) const FULL_PROGRESS_RECORD_KIND: &str = "full_progress";
/// The first full-progress observation envelope schema.
pub(crate) const FULL_PROGRESS_SCHEMA_VERSION: u32 = 1;
const SOURCE_LAYOUT_SCHEMA_VERSION: u32 = 2;
/// Maximum encoded or decoded full-progress observation size.
pub(crate) const FULL_PROGRESS_MAX_RECORD_BYTES: usize = 1024 * 1024;
/// Maximum JSON nesting depth accepted by the full-progress observation codec.
pub(crate) const FULL_PROGRESS_MAX_JSON_DEPTH: usize = 64;

/// One bounded full-progress snapshot observation envelope.
///
/// The envelope is crate-internal and remains an untrusted observation.  Its operation identity,
/// immutable plan, and claims are retained exactly once inside the snapshot's frozen intent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredFullProgressRecord {
    record_kind: String,
    schema_version: u32,
    snapshot: FullProgressSnapshotObservation,
}

impl StoredFullProgressRecord {
    /// Construct an envelope after validating the complete snapshot observation.
    pub(crate) fn new(
        snapshot: FullProgressSnapshotObservation,
    ) -> Result<Self, FullProgressCodecError> {
        let record = Self {
            record_kind: FULL_PROGRESS_RECORD_KIND.to_owned(),
            schema_version: if matches!(
                snapshot.intent().args(),
                super::coordinator::UniversalCommandArgs::SourceDerivedLayout(_)
            ) {
                SOURCE_LAYOUT_SCHEMA_VERSION
            } else {
                FULL_PROGRESS_SCHEMA_VERSION
            },
            snapshot,
        };
        record.validate()?;
        Ok(record)
    }

    /// Borrow the retained snapshot observation.
    pub(crate) fn snapshot(&self) -> &FullProgressSnapshotObservation {
        &self.snapshot
    }

    /// Return the envelope record kind.
    pub(crate) fn record_kind(&self) -> &str {
        &self.record_kind
    }

    /// Return the envelope schema version.
    pub(crate) const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Validate the envelope identity, version, and complete snapshot observation.
    pub(crate) fn validate(&self) -> Result<(), FullProgressCodecError> {
        if self.record_kind != FULL_PROGRESS_RECORD_KIND {
            return Err(FullProgressCodecError::WrongRecordKind {
                actual: self.record_kind.clone(),
            });
        }
        let expected = if matches!(
            self.snapshot.intent().args(),
            super::coordinator::UniversalCommandArgs::SourceDerivedLayout(_)
        ) {
            SOURCE_LAYOUT_SCHEMA_VERSION
        } else {
            FULL_PROGRESS_SCHEMA_VERSION
        };
        if self.schema_version != expected {
            return Err(FullProgressCodecError::UnsupportedVersion {
                version: u64::from(self.schema_version),
            });
        }
        self.snapshot
            .validate_base()
            .map_err(|source| FullProgressCodecError::InvalidSnapshot { source })
    }
}

/// Failure while encoding or decoding a full-progress observation envelope.
#[derive(Debug, Error, PartialEq)]
pub(crate) enum FullProgressCodecError {
    /// Input exceeded the decode bound before parsing began.
    #[error("full-progress record exceeds {maximum} bytes: {actual} bytes")]
    InputOversize { actual: usize, maximum: usize },
    /// Encoded output exceeded the bounded transport size.
    #[error("encoded full-progress record exceeds {maximum} bytes: {actual} bytes")]
    OutputOversize { actual: usize, maximum: usize },
    /// JSON was malformed or violated the strict envelope/nested observation shape.
    #[error("malformed full-progress record: {reason}")]
    Malformed { reason: String },
    /// The envelope carries an unsupported full-progress schema version.
    #[error("unsupported full-progress schema version: {version}")]
    UnsupportedVersion { version: u64 },
    /// The envelope carries a different record kind.
    #[error("wrong full-progress record kind: {actual}")]
    WrongRecordKind { actual: String },
    /// The JSON value exceeds the full-progress nesting bound.
    #[error("full-progress JSON nesting exceeds depth {maximum}")]
    JsonDepthExceeded { maximum: usize },
    /// The snapshot failed its existing complete base validator.
    #[error("invalid full-progress snapshot: {source}")]
    InvalidSnapshot {
        #[source]
        source: FullProgressSnapshotObservationError,
    },
    /// The typed observation could not be serialized after validation.
    #[error("full-progress serialization failed: {reason}")]
    Serialization { reason: String },
}

/// Decode one strictly bounded full-progress observation envelope.
pub(crate) fn decode(bytes: &[u8]) -> Result<StoredFullProgressRecord, FullProgressCodecError> {
    if bytes.len() > FULL_PROGRESS_MAX_RECORD_BYTES {
        return Err(FullProgressCodecError::InputOversize {
            actual: bytes.len(),
            maximum: FULL_PROGRESS_MAX_RECORD_BYTES,
        });
    }

    let value = parse_unique_value(bytes).map_err(|error| FullProgressCodecError::Malformed {
        reason: format!("invalid JSON: {error}"),
    })?;
    validate_json_depth(&value, FULL_PROGRESS_MAX_JSON_DEPTH).map_err(|_| {
        FullProgressCodecError::JsonDepthExceeded {
            maximum: FULL_PROGRESS_MAX_JSON_DEPTH,
        }
    })?;
    validate_envelope_header(&value)?;

    let record = serde_json::from_value::<StoredFullProgressRecord>(value).map_err(|error| {
        FullProgressCodecError::Malformed {
            reason: error.to_string(),
        }
    })?;
    record.validate()?;
    Ok(record)
}

/// Encode one validated full-progress observation envelope with a capped writer.
pub(crate) fn encode(record: &StoredFullProgressRecord) -> Result<Vec<u8>, FullProgressCodecError> {
    encode_with_limit(record, FULL_PROGRESS_MAX_RECORD_BYTES)
}

fn encode_with_limit(
    record: &StoredFullProgressRecord,
    maximum: usize,
) -> Result<Vec<u8>, FullProgressCodecError> {
    record.validate()?;

    let mut writer = CappedWriter::new(maximum);
    let bytes = match serde_json::to_writer(&mut writer, record) {
        Ok(()) => Ok(writer.finish()),
        Err(_error) if writer.exceeded() => Err(FullProgressCodecError::OutputOversize {
            actual: writer.attempted(),
            maximum: writer.limit(),
        }),
        Err(error) => Err(FullProgressCodecError::Serialization {
            reason: error.to_string(),
        }),
    }?;
    let value =
        parse_unique_value(&bytes).map_err(|error| FullProgressCodecError::Serialization {
            reason: format!("encoded full-progress JSON could not be parsed: {error}"),
        })?;
    validate_json_depth(&value, FULL_PROGRESS_MAX_JSON_DEPTH).map_err(|_| {
        FullProgressCodecError::JsonDepthExceeded {
            maximum: FULL_PROGRESS_MAX_JSON_DEPTH,
        }
    })?;
    Ok(bytes)
}

#[cfg(test)]
pub(crate) fn encode_for_test_with_limit(
    record: &StoredFullProgressRecord,
    maximum: usize,
) -> Result<Vec<u8>, FullProgressCodecError> {
    encode_with_limit(record, maximum)
}

fn validate_envelope_header(value: &Value) -> Result<(), FullProgressCodecError> {
    let object = value
        .as_object()
        .ok_or_else(|| FullProgressCodecError::Malformed {
            reason: String::from("full-progress envelope must be a JSON object"),
        })?;

    match object.get("record_kind") {
        Some(Value::String(kind)) if kind == FULL_PROGRESS_RECORD_KIND => {}
        Some(Value::String(kind)) => {
            return Err(FullProgressCodecError::WrongRecordKind {
                actual: kind.clone(),
            });
        }
        Some(_) => {
            return Err(FullProgressCodecError::Malformed {
                reason: String::from("record_kind must be a string"),
            });
        }
        None => {
            return Err(FullProgressCodecError::Malformed {
                reason: String::from("missing record_kind"),
            });
        }
    }

    let version = object
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| FullProgressCodecError::Malformed {
            reason: String::from("schema_version must be an unsigned integer"),
        })?;
    if !matches!(version, 1 | 2) {
        return Err(FullProgressCodecError::UnsupportedVersion { version });
    }
    Ok(())
}
