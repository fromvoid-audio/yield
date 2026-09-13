//! Profile-local durable operation-journal values and bounded codec.
//!
//! The model, evidence, codec, and consistency validator remain storage-neutral: they do not
//! acquire ownership, publish filesystem entries, or interpret journal data as mutation
//! authority. The separate store module adds the profile-owned SQLite persistence boundary and
//! retains those capabilities for the lifetime of its connection.

pub mod codec;
pub mod coordinator;
pub mod evidence;
pub mod model;
pub mod namespace_commands;
pub mod owner;
pub mod participants;
pub mod render_commands;
pub mod status;
pub mod store;
pub mod validation;

pub use codec::{DecodeError, EncodeError, StoredOperationRecord, decode, encode};

pub use coordinator::{
    CopyCollisionPolicy, CopyEndpoint, CopyEndpointRole, CopyIntentArgs, CopyIntentValidationError,
    CopySourceExpectation, ExternalRootBindingId, FiniteF32Bits, ImportFileExpectation,
    ImportIntentArgs, ImportIntentValidationError, MAX_COPY_CONTENT_IDENTITY_BYTES,
    MAX_COPY_METADATA_REFERENCE_BYTES, MAX_IMPORT_ROOT_IDENTITY_BYTES, MAX_RETRY_CONTEXT_BYTES,
    MAX_RETRY_LEASE_ID_BYTES, MAX_STORED_RELATIVE_PATH_BYTES, MAX_STORED_RELATIVE_PATH_COMPONENTS,
    MAX_WAVEFORM_CHANGE_MARKER_BYTES, MAX_WAVEFORM_EDIT_FRAMES, MAX_WAVEFORM_FILE_IDENTITY_BYTES,
    RetryErrorClass, RetryLeaseId, RetryObservation, RetryObservationValidationError,
    StoredRelativePath, StoredRelativePathError, UniversalCommandArgs,
    UniversalCommandArgsValidationError, WaveformEditFadeArgs, WaveformEditFrameRange,
    WaveformEditIntentArgs, WaveformEditOperationArgs, WaveformEditSettingsArgs,
    WaveformEditValidationError, WaveformFileRevisionObservation, WaveformOutputEncoding,
    WaveformOutputSpec, WaveformSampleFormat, WaveformSourceSpec,
};

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
    SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7, SCHEMA_V8, SCHEMA_V9,
    UniversalCommandKind, UniversalIntent, UniversalIntentTextViolation,
    UniversalIntentValidationError,
};

pub use namespace_commands::{
    CreateFolderIntentArgs, MAX_NAMESPACE_ENTRY_IDENTITY_BYTES, MAX_NAMESPACE_LEAF_NAME_BYTES,
    NamespaceCommandArgs, NamespaceCommandValidationError, NamespaceDirectoryEndpoint,
    NamespaceDirectoryExpectation, NamespaceDirectoryLocator, NamespaceEndpointRole,
    NamespaceEntryEndpoint, NamespaceEntryExpectation, NamespaceEntryKind,
    NamespaceFileExpectation, NamespaceLeafName, NamespaceLeafViolation, NamespaceMoveIntentArgs,
    NamespaceRenameIntentArgs, NamespaceTarget,
};

pub use render_commands::{
    MAX_WAVEFORM_RENDER_FRAMES, RenderCommandArgs, RenderCommandValidationError,
    RenderMetadataPolicy, WaveformDuplicateDoubleIntentArgs, WaveformNormalizeIntentArgs,
    WaveformRenderOutputArithmetic, WaveformRenderOutputSpec,
};

pub use owner::{
    OPERATION_JOURNAL_OWNER_COMMAND_CAPACITY, OPERATION_JOURNAL_OWNER_MAX_QUEUED_INPUT_BYTES,
    OPERATION_JOURNAL_OWNER_RESULT_CAPACITY, OperationJournalClient, OperationJournalOwner,
    OperationJournalOwnerError, OperationJournalOwnerQueueError, OperationJournalOwnerStartError,
    OperationJournalOwnerStatus, OperationJournalRequest, OperationJournalRequestError,
};

pub use participants::{
    MAX_ARTIFACT_REFERENCE_BYTES, MAX_DIAGNOSTIC_CODE_BYTES, MAX_PARTICIPANTS,
    MAX_SOURCE_IDENTITY_BYTES, ParticipantCheckpoint, ParticipantCheckpointState,
    ParticipantDescriptor, ParticipantId, ParticipantKind, ParticipantRequirement, ParticipantSet,
    ParticipantTextField, ParticipantTextViolation, ParticipantValidationError, StoredSourceFence,
};

pub use status::{
    JournalStatusOutcome, JournalStatusSnapshot, MAX_STATUS_DIAGNOSTIC_BYTES,
    ObservedOperationStatus, RetainedJournalStatus, RetryStatusSummary,
};

pub use store::recovery::{
    MAX_RECOVERY_BYTES_PER_PAGE, MAX_RECOVERY_ROWS_PER_PAGE, RecoveryCursor, RecoveryEntry,
    RecoveryField, RecoveryKey, RecoveryPage, RecoveryRetentionReason, RecoveryRowOutcome,
    RecoveryScanError,
};
pub use store::{
    JOURNAL_APPLICATION_ID, JOURNAL_DATABASE_FILE_NAME, JOURNAL_DATABASE_ROOT_NAME,
    JOURNAL_SCHEMA_VERSION, JournalCancelRequestReceipt, JournalInsertFailure,
    JournalInsertOutcome, JournalInsertReceipt, JournalInsertWriteStage, JournalRetryReceipt,
    OperationJournalStore, OperationJournalStoreError,
};
pub use validation::{RecordConsistency, classify_record};
