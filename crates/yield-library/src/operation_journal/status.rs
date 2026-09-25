//! Compact, authority-free status projections for recovered journal rows.
//!
//! A status snapshot can only be created from a [`RecoveryEntry`]. The recovery scanner has
//! already bounded and classified the row, so this module copies the small fields useful to a
//! status consumer without cloning the opaque payload or any publication evidence. A retained
//! snapshot remains audit data; it is never projected as a successful or retryable operation.

use super::OperationId;
use super::coordinator::{RetryErrorClass, RetryObservation};
use super::model::{
    CancellationRequestObservation, MAX_UNIVERSAL_LABEL_BYTES, MAX_USER_STATUS_KEY_BYTES,
    OperationDisposition, OperationPhase, UniversalCommandKind,
};
use super::store::recovery::{
    RecoveryEntry, RecoveryField, RecoveryKey, RecoveryRetentionReason, RecoveryRowOutcome,
};

/// Maximum UTF-8 bytes copied into a compact status diagnostic.
pub const MAX_STATUS_DIAGNOSTIC_BYTES: usize = 256;

/// The compact outcome of one recovered journal-row status projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JournalStatusOutcome {
    /// The recovered record passed pure consistency classification.
    Observed(ObservedOperationStatus),
    /// The row remains retained data and requires an audit decision.
    Retained(RetainedJournalStatus),
}

/// A compact status snapshot for one bounded recovery-row observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalStatusSnapshot {
    key: RecoveryKey,
    sequence: i64,
    outcome: JournalStatusOutcome,
}

impl JournalStatusSnapshot {
    /// Return the bounded SQL key observed for the row.
    pub fn key(&self) -> &RecoveryKey {
        &self.key
    }

    /// Return the operation identity from the SQL key when it was valid.
    pub fn operation_id(&self) -> Option<OperationId> {
        match self.key {
            RecoveryKey::OperationId(operation_id) => Some(operation_id),
            RecoveryKey::Text(_) | RecoveryKey::Invalid { .. } => None,
        }
    }

    /// Return the journal observation sequence for this snapshot.
    pub const fn sequence(&self) -> i64 {
        self.sequence
    }

    /// Return the compact observed or retained outcome.
    pub fn outcome(&self) -> &JournalStatusOutcome {
        &self.outcome
    }

    /// Borrow the classified operation status, when the row was internally consistent.
    pub fn observed(&self) -> Option<&ObservedOperationStatus> {
        match &self.outcome {
            JournalStatusOutcome::Observed(status) => Some(status),
            JournalStatusOutcome::Retained(_) => None,
        }
    }

    /// Borrow the typed retention status, when the row requires audit.
    pub fn retained(&self) -> Option<&RetainedJournalStatus> {
        match &self.outcome {
            JournalStatusOutcome::Observed(_) => None,
            JournalStatusOutcome::Retained(status) => Some(status),
        }
    }

    /// Return whether the row remains retained for audit.
    pub fn is_retained(&self) -> bool {
        self.retained().is_some()
    }
}

/// Compact status fields copied from an internally consistent stored record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedOperationStatus {
    operation_id: OperationId,
    schema_version: u32,
    phase: OperationPhase,
    disposition: OperationDisposition,
    command_kind: Option<UniversalCommandKind>,
    label: Option<String>,
    user_status_key: Option<String>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
    retry: Option<RetryStatusSummary>,
    cancellation_request: Option<CancellationRequestObservation>,
}

impl ObservedOperationStatus {
    /// Return the operation identity from the classified record.
    pub const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Return the persisted codec schema version.
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Return the last observed coordinator phase.
    pub const fn phase(&self) -> OperationPhase {
        self.phase
    }

    /// Return the last observed disposition overlay.
    pub const fn disposition(&self) -> OperationDisposition {
        self.disposition
    }

    /// Return the typed command kind when the record carries a universal intent.
    pub const fn command_kind(&self) -> Option<UniversalCommandKind> {
        self.command_kind
    }

    /// Return a bounded user-facing operation label, when one was stored.
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    /// Return a bounded stable status lookup key, when one was stored.
    pub fn user_status_key(&self) -> Option<&str> {
        self.user_status_key.as_deref()
    }

    /// Return the stored creation timestamp.
    pub const fn created_unix_ms(&self) -> i64 {
        self.created_unix_ms
    }

    /// Return the stored update timestamp.
    pub const fn updated_unix_ms(&self) -> i64 {
        self.updated_unix_ms
    }

    /// Return the bounded retry observation summary, when one was stored.
    pub fn retry(&self) -> Option<&RetryStatusSummary> {
        self.retry.as_ref()
    }

    /// Return the cancellation request observation, when one was stored.
    pub const fn cancellation_request(&self) -> Option<CancellationRequestObservation> {
        self.cancellation_request
    }
}

/// Compact V5 retry fields suitable for status display or scheduling inspection.
///
/// The durable retry lease identity and redaction-safe context are deliberately omitted. This
/// value contains observations only and no live lease or retry authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryStatusSummary {
    attempt: u32,
    requested_unix_ms: i64,
    next_retry_unix_ms: i64,
    lease_expiry_unix_ms: i64,
    error_class: RetryErrorClass,
    diagnostic_code: String,
}

impl RetryStatusSummary {
    /// Return the one-based retry attempt.
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Return when the retry was requested.
    pub const fn requested_unix_ms(&self) -> i64 {
        self.requested_unix_ms
    }

    /// Return the next retry timestamp observation.
    pub const fn next_retry_unix_ms(&self) -> i64 {
        self.next_retry_unix_ms
    }

    /// Return the observed retry-lease expiry timestamp.
    pub const fn lease_expiry_unix_ms(&self) -> i64 {
        self.lease_expiry_unix_ms
    }

    /// Return the typed retry error class.
    pub const fn error_class(&self) -> RetryErrorClass {
        self.error_class
    }

    /// Return the bounded stable diagnostic code.
    pub fn diagnostic_code(&self) -> &str {
        &self.diagnostic_code
    }
}

/// A retained row's typed audit reason plus a bounded safe diagnostic presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedJournalStatus {
    reason: RecoveryRetentionReason,
    diagnostic: String,
}

impl RetainedJournalStatus {
    /// Return the original typed retention reason.
    pub fn reason(&self) -> &RecoveryRetentionReason {
        &self.reason
    }

    /// Return a bounded, control-free diagnostic suitable for status presentation.
    pub fn diagnostic(&self) -> &str {
        &self.diagnostic
    }
}

/// Project one scanner-produced recovery entry into compact status data.
pub(crate) fn from_recovery_entry(entry: &RecoveryEntry) -> JournalStatusSnapshot {
    let outcome = match entry.outcome() {
        RecoveryRowOutcome::Consistent(record) => {
            JournalStatusOutcome::Observed(ObservedOperationStatus {
                operation_id: record.operation_id,
                schema_version: record.schema_version,
                phase: record.phase,
                disposition: record.disposition,
                command_kind: record
                    .universal_intent
                    .as_ref()
                    .map(|intent| intent.command_kind()),
                label: bounded_optional_text(&record.intent.label, MAX_UNIVERSAL_LABEL_BYTES),
                user_status_key: record.universal_intent.as_ref().and_then(|intent| {
                    bounded_optional_text(intent.user_status_key(), MAX_USER_STATUS_KEY_BYTES)
                }),
                created_unix_ms: record.created_unix_ms,
                updated_unix_ms: record.updated_unix_ms,
                retry: record.retry_observation().map(retry_summary),
                cancellation_request: record.cancellation_request().copied(),
            })
        }
        RecoveryRowOutcome::Retained(reason) => {
            JournalStatusOutcome::Retained(RetainedJournalStatus {
                reason: sanitized_retention_reason(reason),
                diagnostic: retention_diagnostic(reason),
            })
        }
    };

    JournalStatusSnapshot {
        key: status_key(entry.key()),
        sequence: entry.sequence(),
        outcome,
    }
}

fn status_key(key: &RecoveryKey) -> RecoveryKey {
    match key {
        RecoveryKey::OperationId(operation_id) => RecoveryKey::OperationId(*operation_id),
        RecoveryKey::Text(_) => RecoveryKey::Invalid {
            sql_type: String::from("text"),
            preview: String::from("operation key retained for audit"),
        },
        RecoveryKey::Invalid { sql_type, .. } => RecoveryKey::Invalid {
            sql_type: bounded_text(sql_type, 32),
            preview: String::from("operation key retained for audit"),
        },
    }
}

fn retry_summary(retry: &RetryObservation) -> RetryStatusSummary {
    RetryStatusSummary {
        attempt: retry.attempt(),
        requested_unix_ms: retry.requested_unix_ms(),
        next_retry_unix_ms: retry.next_retry_unix_ms(),
        lease_expiry_unix_ms: retry.lease_expiry_unix_ms(),
        error_class: retry.error_class(),
        diagnostic_code: bounded_text(retry.diagnostic_code(), MAX_STATUS_DIAGNOSTIC_BYTES),
    }
}

fn bounded_optional_text(value: &str, maximum: usize) -> Option<String> {
    (!value.is_empty()).then(|| bounded_text(value, maximum))
}

fn bounded_text(value: &str, maximum: usize) -> String {
    let mut end = value.len().min(maximum);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end]
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

fn retention_diagnostic(reason: &RecoveryRetentionReason) -> String {
    let text = match reason {
        RecoveryRetentionReason::LegacyMissingPublicationEvidence => String::from(
            "legacy operation journal record lacks publication evidence; audit required",
        ),
        RecoveryRetentionReason::UnsupportedCodec { version } => {
            format!("operation journal schema version {version} is unsupported; audit required")
        }
        RecoveryRetentionReason::Malformed { .. } => {
            String::from("operation journal record is malformed; audit required")
        }
        RecoveryRetentionReason::SizeInvalid { declared, .. } => match declared {
            Some(declared) => format!(
                "operation journal record size {declared} is outside its bound; audit required"
            ),
            None => String::from("operation journal record size is unavailable; audit required"),
        },
        RecoveryRetentionReason::TypeInvalid {
            field, sql_type, ..
        } => format!(
            "operation journal {} value has SQLite type {}; audit required",
            field_name(*field),
            bounded_text(sql_type, 32)
        ),
        RecoveryRetentionReason::KeyMismatch {
            record_operation_id,
        } => format!("journal key does not match decoded operation {record_operation_id}"),
    };
    bounded_text(&text, MAX_STATUS_DIAGNOSTIC_BYTES)
}

fn sanitized_retention_reason(reason: &RecoveryRetentionReason) -> RecoveryRetentionReason {
    match reason {
        RecoveryRetentionReason::LegacyMissingPublicationEvidence => {
            RecoveryRetentionReason::LegacyMissingPublicationEvidence
        }
        RecoveryRetentionReason::UnsupportedCodec { version } => {
            RecoveryRetentionReason::UnsupportedCodec { version: *version }
        }
        RecoveryRetentionReason::Malformed { .. } => RecoveryRetentionReason::Malformed {
            reason: String::from("operation journal record is malformed; audit required"),
        },
        RecoveryRetentionReason::KeyMismatch {
            record_operation_id,
        } => RecoveryRetentionReason::KeyMismatch {
            record_operation_id: *record_operation_id,
        },
        RecoveryRetentionReason::SizeInvalid { declared, .. } => {
            RecoveryRetentionReason::SizeInvalid {
                declared: *declared,
                reason: match declared {
                    Some(declared) => format!(
                        "operation journal record size {declared} is outside its bound; audit required"
                    ),
                    None => {
                        String::from("operation journal record size is unavailable; audit required")
                    }
                },
            }
        }
        RecoveryRetentionReason::TypeInvalid {
            field, sql_type, ..
        } => RecoveryRetentionReason::TypeInvalid {
            field: *field,
            sql_type: bounded_text(sql_type, 32),
            reason: format!(
                "operation journal {} value has SQLite type {}; audit required",
                field_name(*field),
                bounded_text(sql_type, 32)
            ),
        },
    }
}

fn field_name(field: RecoveryField) -> &'static str {
    match field {
        RecoveryField::OperationId => "operation-id",
        RecoveryField::Sequence => "sequence",
        RecoveryField::Record => "record",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_text_truncates_at_utf8_boundary_and_removes_controls() {
        let value = format!("{}\u{0000}z", "é".repeat(200));
        let bounded = bounded_text(&value, 255);
        assert!(bounded.len() <= 255);
        assert!(bounded.chars().all(|character| !character.is_control()));
        assert!(bounded.is_char_boundary(bounded.len()));
    }
}
