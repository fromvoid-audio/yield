//! Compact observations of both journal families, without retained payload or authority.
mod action_status;
pub use action_status::{JournalCancellationBoundary, JournalRetryBlockage, JournalUserStatus};

use super::store::recovery::{
    CoordinatorRecoveryRecord, CoordinatorRecoveryRowOutcome, RecoveryFamily, RecoveryKey,
};
use super::{
    FullProgressDisposition, OperationDisposition, OperationId, OperationJournalStore,
    OperationPhase, RecoveryCursor, RecoveryRetentionReason, RecoveryScanError,
};

/// Persisted family of a recovery observation; dispositions must not cross families.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalStatusFamily {
    /// Legacy operation records.
    Legacy,
    /// Guarded full-progress observations.
    FullProgress,
}

/// Redacted retention category. No SQL key, payload, or free-form diagnostic is copied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalRetentionKind {
    /// Legacy publication evidence is absent.
    LegacyMissingPublicationEvidence,
    /// Codec version is unsupported.
    UnsupportedCodec,
    /// Record or consistency relationships are malformed.
    Malformed,
    /// Stored key and record identity disagree.
    KeyMismatch,
    /// Record size cannot be admitted.
    SizeInvalid,
    /// SQL representation is invalid.
    TypeInvalid,
}

/// Exact durable observation, distinct from execution permission or physical qualification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryStatusOutcome {
    /// Classified legacy phase and disposition, without opaque record data.
    Legacy {
        /// Persisted phase.
        phase: OperationPhase,
        /// Persisted legacy disposition (not a live retry decision).
        disposition: OperationDisposition,
    },
    /// Classified full-progress phase and disposition, including observed-effect cancellation.
    FullProgress {
        /// Persisted phase.
        phase: OperationPhase,
        /// Guarded full-progress disposition, never mapped to legacy publication.
        disposition: FullProgressDisposition,
    },
    /// Evidence retained for audit; never success or retry authority.
    Retained(JournalRetentionKind),
}

/// One compact, scanner-derived observation. Construction remains inside the owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryStatusEntry {
    family: JournalStatusFamily,
    operation_id: Option<OperationId>,
    sequence: i64,
    outcome: RecoveryStatusOutcome,
    retry_blockage: Option<JournalRetryBlockage>,
}
impl RecoveryStatusEntry {
    /// Family used to interpret the disposition and deduplicate observations.
    pub const fn family(&self) -> JournalStatusFamily {
        self.family
    }
    /// Validated SQL operation identity, absent for malformed keys.
    pub const fn operation_id(&self) -> Option<OperationId> {
        self.operation_id
    }
    /// Positive journal observation sequence, not a mutation permit.
    pub const fn sequence(&self) -> i64 {
        self.sequence
    }
    /// Classified or retained outcome.
    pub const fn outcome(&self) -> RecoveryStatusOutcome {
        self.outcome
    }
}

/// A bounded read-only page using the combined scanner's row and byte admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryStatusPage {
    entries: Vec<RecoveryStatusEntry>,
    next_cursor: Option<RecoveryCursor>,
    done: bool,
    encoded_bytes: usize,
}
impl RecoveryStatusPage {
    /// Compact observations in ascending journal sequence order.
    pub fn entries(&self) -> &[RecoveryStatusEntry] {
        &self.entries
    }
    /// Cursor to continue the existing keyset scan.
    pub const fn next_cursor(&self) -> Option<RecoveryCursor> {
        self.next_cursor
    }
    /// Whether this scan reached the current end; not a durable completion guarantee.
    pub const fn is_done(&self) -> bool {
        self.done
    }
    /// Encoded bytes admitted by the scanner before compact projection.
    pub const fn encoded_bytes(&self) -> usize {
        self.encoded_bytes
    }
}
impl OperationJournalStore {
    /// Read one compact family-aware page under the retained profile namespace.
    /// No writable open, checkpoint, mutation, additional count scan, or raw record escapes.
    pub fn scan_recovery_status_page(
        &self,
        cursor: Option<RecoveryCursor>,
    ) -> Result<RecoveryStatusPage, RecoveryScanError> {
        let page = self.scan_coordinator_recovery_page(cursor)?;
        let entries = page
            .entries()
            .iter()
            .map(|entry| {
                let family = match entry.family() {
                    RecoveryFamily::Legacy => JournalStatusFamily::Legacy,
                    RecoveryFamily::FullProgress => JournalStatusFamily::FullProgress,
                };
                let operation_id = match entry.key() {
                    RecoveryKey::OperationId(id) => Some(*id),
                    RecoveryKey::Text(_) | RecoveryKey::Invalid { .. } => None,
                };
                let retry_blockage = match entry.outcome() {
                    CoordinatorRecoveryRowOutcome::Consistent(
                        CoordinatorRecoveryRecord::Legacy(record),
                    ) => record.retry_observation().map(|r| r.error_class()),
                    CoordinatorRecoveryRowOutcome::Consistent(
                        CoordinatorRecoveryRecord::FullProgress(record),
                    ) => record
                        .snapshot()
                        .retry_observation()
                        .map(|r| r.error_class()),
                    CoordinatorRecoveryRowOutcome::Retained(_) => None,
                }
                .map(|class| match class {
                    super::coordinator::RetryErrorClass::TransientContention => {
                        JournalRetryBlockage::Contention
                    }
                    super::coordinator::RetryErrorClass::TransientAvailability => {
                        JournalRetryBlockage::Availability
                    }
                    super::coordinator::RetryErrorClass::CapacityUnavailable => {
                        JournalRetryBlockage::Capacity
                    }
                });
                let outcome = match entry.outcome() {
                    CoordinatorRecoveryRowOutcome::Consistent(
                        CoordinatorRecoveryRecord::Legacy(record),
                    ) => RecoveryStatusOutcome::Legacy {
                        phase: record.phase,
                        disposition: record.disposition,
                    },
                    CoordinatorRecoveryRowOutcome::Consistent(
                        CoordinatorRecoveryRecord::FullProgress(record),
                    ) => RecoveryStatusOutcome::FullProgress {
                        phase: record.snapshot().phase(),
                        disposition: record.snapshot().disposition(),
                    },
                    CoordinatorRecoveryRowOutcome::Retained(reason) => {
                        RecoveryStatusOutcome::Retained(match reason {
                            RecoveryRetentionReason::LegacyMissingPublicationEvidence => {
                                JournalRetentionKind::LegacyMissingPublicationEvidence
                            }
                            RecoveryRetentionReason::UnsupportedCodec { .. } => {
                                JournalRetentionKind::UnsupportedCodec
                            }
                            RecoveryRetentionReason::Malformed { .. } => {
                                JournalRetentionKind::Malformed
                            }
                            RecoveryRetentionReason::KeyMismatch { .. } => {
                                JournalRetentionKind::KeyMismatch
                            }
                            RecoveryRetentionReason::SizeInvalid { .. } => {
                                JournalRetentionKind::SizeInvalid
                            }
                            RecoveryRetentionReason::TypeInvalid { .. } => {
                                JournalRetentionKind::TypeInvalid
                            }
                        })
                    }
                };
                RecoveryStatusEntry {
                    family,
                    operation_id,
                    sequence: entry.sequence(),
                    outcome,
                    retry_blockage,
                }
            })
            .collect();
        Ok(RecoveryStatusPage {
            entries,
            next_cursor: page.next_cursor(),
            done: page.is_done(),
            encoded_bytes: page.encoded_bytes(),
        })
    }
}
