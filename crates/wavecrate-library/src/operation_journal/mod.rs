//! Profile-local durable operation-journal values and bounded codec.
//!
//! The model, evidence, codec, and consistency validator remain storage-neutral: they do not
//! acquire ownership, publish filesystem entries, or interpret journal data as mutation
//! authority. The separate store module adds the profile-owned SQLite persistence boundary and
//! retains those capabilities for the lifetime of its connection.

pub mod codec;
pub mod evidence;
pub mod model;
pub mod store;
pub mod validation;

pub use codec::{DecodeError, EncodeError, StoredOperationRecord, decode, encode};

pub use evidence::{
    AbsentFinalAdoptionEvidence, AbsentFinalObservation, AbsentFinalRecoveryObservation,
    AbsentFinalTransactionOwnedProof, FilesystemPublishedWaveformRestore,
    FilesystemStagedParticipant, FilesystemStagedWaveformRestore, ObservedFilesystemClassification,
    PreparedAbsentFinalNoReplace, PreparedFileEvidence, PreparedLeafLocator,
    PreparedObjectIdentity, PreparedRestoreDirection, PreparedRestoreEvidence,
    PreparedStagingLocator, PreparedTargetContract, PreparedWaveformRestore, RecordedRootLocator,
    ReplaceExpectedIdentity, ReplacementCandidateAssessment, ReplacementCandidatePrimitive,
    ReplacementMissingInvariant, ReplacementPlatformFamily, ReplacementQualificationAssessment,
    ReplacementQualificationDecision, ReplacementQualificationRetryCondition, VolumeIdentity,
};

pub use model::{
    CURRENT_SCHEMA_VERSION, MAX_RECORD_BYTES, OperationActor, OperationDisposition, OperationId,
    OperationIntent, OperationKind, OperationPhase, SCHEMA_V1, SCHEMA_V2,
};

pub use store::recovery::{
    MAX_RECOVERY_BYTES_PER_PAGE, MAX_RECOVERY_ROWS_PER_PAGE, RecoveryCursor, RecoveryEntry,
    RecoveryField, RecoveryKey, RecoveryPage, RecoveryRetentionReason, RecoveryRowOutcome,
    RecoveryScanError,
};
pub use store::{
    JOURNAL_APPLICATION_ID, JOURNAL_DATABASE_FILE_NAME, JOURNAL_DATABASE_ROOT_NAME,
    JOURNAL_SCHEMA_VERSION, JournalInsertReceipt, OperationJournalStore,
    OperationJournalStoreError,
};
pub use validation::{RecordConsistency, classify_record};
