//! Profile-local durable operation-journal values and bounded codec.
//!
//! The model, evidence, codec, and consistency validator remain storage-neutral: they do not
//! acquire ownership, publish filesystem entries, or interpret journal data as mutation
//! authority. The separate store module adds the profile-owned SQLite persistence boundary and
//! retains those capabilities for the lifetime of its connection.

pub mod codec;
pub mod evidence;
pub mod model;
pub mod participants;
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
    CURRENT_SCHEMA_VERSION, CancellationRequestObservation, CancellationRequestValidationError,
    MAX_RECORD_BYTES, MAX_UNIVERSAL_LABEL_BYTES, MAX_USER_STATUS_KEY_BYTES, OperationActor,
    OperationDisposition, OperationId, OperationIntent, OperationKind, OperationPhase, SCHEMA_V1,
    SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, UniversalCommandKind, UniversalIntent,
    UniversalIntentTextViolation, UniversalIntentValidationError,
};

pub use participants::{
    MAX_ARTIFACT_REFERENCE_BYTES, MAX_DIAGNOSTIC_CODE_BYTES, MAX_PARTICIPANTS,
    MAX_SOURCE_IDENTITY_BYTES, ParticipantCheckpoint, ParticipantCheckpointState,
    ParticipantDescriptor, ParticipantId, ParticipantKind, ParticipantRequirement, ParticipantSet,
    ParticipantTextField, ParticipantTextViolation, ParticipantValidationError, StoredSourceFence,
};

pub use store::recovery::{
    MAX_RECOVERY_BYTES_PER_PAGE, MAX_RECOVERY_ROWS_PER_PAGE, RecoveryCursor, RecoveryEntry,
    RecoveryField, RecoveryKey, RecoveryPage, RecoveryRetentionReason, RecoveryRowOutcome,
    RecoveryScanError,
};
pub use store::{
    JOURNAL_APPLICATION_ID, JOURNAL_DATABASE_FILE_NAME, JOURNAL_DATABASE_ROOT_NAME,
    JOURNAL_SCHEMA_VERSION, JournalCancelRequestReceipt, JournalInsertReceipt,
    OperationJournalStore, OperationJournalStoreError,
};
pub use validation::{RecordConsistency, classify_record};
