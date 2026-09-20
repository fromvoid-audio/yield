//! Profile-local durable operation-journal values and bounded codec.
//!
//! The model, evidence, codec, and consistency validator remain storage-neutral: they do not
//! acquire ownership, publish filesystem entries, or interpret journal data as mutation
//! authority. The separate store module adds the profile-owned SQLite persistence boundary and
//! retains those capabilities for the lifetime of its connection.

pub mod admission_claims;
pub mod cancellation_cleanup;
pub mod cancellation_dispatch;
pub mod cancellation_effects;
pub mod cancellation_reconciliation;
pub mod cancellation_recovery;
pub mod codec;
pub mod coordinator;
pub mod evidence;
pub mod external_commands;
pub mod full_capacity;
#[allow(dead_code)]
pub(crate) mod full_progress_codec;
pub mod full_progress_record;
pub mod full_status;
pub mod history_commands;
pub mod model;
pub mod namespace_commands;
pub mod owner;
pub mod participants;
pub mod readiness_scheduling;
pub mod removal_commands;
pub mod render_commands;
pub mod retained_resolution_commands;
pub mod selection_normalize_commands;
pub mod selection_output_commands;
pub mod status;
pub mod store;
pub mod validation;
pub mod work_plan;
pub mod work_progress;

pub use codec::{DecodeError, EncodeError, StoredOperationRecord, decode, encode};

pub use admission_claims::{
    AdmissionClaim, AdmissionClaimId, AdmissionClaimInventory, AdmissionClaimSettlement,
    AdmissionClaimSettlementInventory, AdmissionClaimSettlementOutcome,
    AdmissionClaimValidationError, MAX_ADMISSION_CLAIMS,
};

pub use cancellation_recovery::{
    CancellationActionSnapshot, CancellationActionState, CancellationInventory,
    CancellationInventoryError, CancellationRoute, CancellationStepSnapshot,
    MAX_CANCELLATION_ACTIONS, MAX_CANCELLATION_STEPS,
};

pub use cancellation_cleanup::{
    CancellationNamespaceCleanupManifest, CancellationNamespaceCleanupManifestError,
    CancellationNamespaceCleanupManifestValidationError,
    CancellationNamespaceCleanupObservationRecord,
    CancellationNamespaceCleanupObservationRecordError, CancellationNamespaceCleanupObservationRow,
    CancellationNamespaceCleanupOutcome, CancellationResourceCleanupManifest,
    CancellationResourceCleanupManifestError, CancellationResourceCleanupManifestValidationError,
    CancellationResourceCleanupObservationRecord,
    CancellationResourceCleanupObservationRecordError, CancellationResourceCleanupObservationRow,
    CancellationResourceCleanupOutcome, MAX_CANCELLATION_NAMESPACE_CLEANUP_KEYS,
    MAX_CANCELLATION_NAMESPACE_CLEANUP_ROWS, MAX_CANCELLATION_RESOURCE_CLEANUP_KEYS,
    MAX_CANCELLATION_RESOURCE_CLEANUP_ROWS,
};

pub use cancellation_dispatch::{
    CancellationDispatchManifest, CancellationDispatchManifestError,
    CancellationDispatchManifestValidationError, CancellationDispatchOutcome,
    CancellationDispatchOutcomeRecord, CancellationDispatchOutcomeRecordError,
    CancellationDispatchOutcomeRow, MAX_CANCELLATION_DISPATCH_KEYS, MAX_CANCELLATION_DISPATCH_ROWS,
};

pub use cancellation_effects::{
    CancellationCleanupManifest, CancellationCleanupManifestError,
    CancellationCleanupManifestValidationError, CancellationCleanupSegment,
    CancellationEffectBundle, CancellationEffectFact, CancellationEffectFactKind,
    CancellationEffectInventory, CancellationEffectInventoryError,
    CancellationEffectInventoryValidationError, CancellationEffectOutcome,
    CancellationEffectRecipe, CancellationEffectRecoveryInventory, CancellationEffectSourceClosure,
    MAX_CANCELLATION_CLEANUP_KEYS, MAX_CANCELLATION_CLEANUP_SEGMENTS,
    MAX_CANCELLATION_EFFECT_BUNDLES, MAX_CANCELLATION_EFFECT_ROWS,
};

pub use cancellation_reconciliation::{
    CancellationCoordinatorClosureObservationRecord,
    CancellationCoordinatorClosureObservationRecordError,
    CancellationCoordinatorClosureObservationRow, CancellationCoordinatorClosureOutcome,
    CancellationSourceClosureObservationRecord, CancellationSourceClosureObservationRecordError,
    CancellationSourceClosureObservationRow, CancellationSourceClosureOutcome,
    CoordinatorClosureManifest, CoordinatorClosureManifestError,
    CoordinatorClosureManifestValidationError, MAX_CANCELLATION_COORDINATOR_CLOSURE_KEYS,
    MAX_CANCELLATION_COORDINATOR_CLOSURE_ROWS, MAX_CANCELLATION_SOURCE_CLOSURE_KEYS,
    MAX_CANCELLATION_SOURCE_CLOSURE_ROWS, SourceClosureManifest, SourceClosureManifestError,
    SourceClosureManifestValidationError,
};

pub use readiness_scheduling::{
    DurableReadinessWakeObservation, MAX_READINESS_CONTRACT_VERSION_BYTES,
    ReadinessArtifactBinding, ReadinessDeferralBatchObservation, ReadinessDeferralManifest,
    ReadinessDeferralManifestError, ReadinessSchedulingObservationError,
    ReadinessSchedulingObservationRecord, ReadinessSchedulingObservationRow,
    ReadinessTargetSetSummary, derive_readiness_deferral_manifest,
};

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

pub use removal_commands::{
    RemovalCommandArgs, RemovalCommandValidationError, StageRetainedFolderIntentArgs,
    TrashIntentArgs,
};
pub use retained_resolution_commands::{
    MAX_RETAINED_SELECTED_ENTRY_JSON_BYTES, RETAINED_RECOVERY_DIRECTORY, RetainedDeleteId,
    RetainedEntryFingerprint, RetainedFingerprintValidationError, RetainedFolderObservation,
    RetainedPurgeDecision, RetainedPurgeIntentArgs, RetainedRecoveryRootKind,
    RetainedResolutionCommandArgs, RetainedResolutionStage, RetainedResolutionValidationError,
    RetainedRestoreConflictPolicy, RetainedRestoreIntentArgs, fingerprint_selected_entry_json,
};
pub use selection_normalize_commands::{
    SelectionNormalizeEdgeFadePolicy, SelectionNormalizeIntentArgs, SelectionNormalizePeakPolicy,
    SelectionNormalizeValidationError,
};
pub use selection_output_commands::{
    ExtractAndTrimIntentArgs, MAX_RAW_WAV_FMT_CHUNK_BYTES, MIN_RAW_WAV_FMT_CHUNK_BYTES,
    RawWavEdgeFadePolicy, RawWavExtractionRecipe, RawWavOutputArithmetic,
    SelectionExtractIntentArgs, SelectionExtractMetadataPolicy, SelectionOutputCommandArgs,
    SelectionOutputRecipe, SelectionOutputValidationError, SourcePlaybackType,
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

pub use history_commands::{
    HistoryAction, HistoryDirection, HistoryExtractedFileObservation, HistoryFolderMove,
    HistoryFolderRelocation, HistoryFolderRelocationPlan, HistoryRecoveryBackupObservation,
    HistoryRecoveryLeaf, HistoryRecoveryLeafObservation, HistoryRecoveryRootRole,
    HistoryRestoreTargetExpectation, HistoryRestoreTargetPolicy, HistoryScopeId,
    HistoryUndoRedoIntentArgs, HistoryValidationError, HistoryWaveformRestore,
    HistoryWaveformRestorePlan, MAX_HISTORY_ACTIONS, MAX_HISTORY_AGGREGATE_TEXT_BYTES,
    MAX_HISTORY_RELOCATIONS,
};

pub use external_commands::{
    EvidenceBatchId, ExternalCaptureBoundary, ExternalCaptureSequenceEvidence,
    ExternalNativeString, ExternalRawDurableAccounting, ExternalRawEventKind,
    ExternalRawNativeAccounting, ExternalRawObservation, ExternalRawObservationEnvelope,
    ExternalRawObservationLimits, ExternalRawObservationMetadata, ExternalRawObservationProvenance,
    ExternalRawObservedPath, ExternalRawPathHint, ExternalRawPathRole,
    ExternalReconciliationAuditRequirement, ExternalReconciliationIntentArgs,
    ExternalReconciliationValidationError, MAX_EXTERNAL_RECONCILIATION_EVENTS,
    MAX_EXTERNAL_RECONCILIATION_IDENTITY_BYTES, MAX_EXTERNAL_RECONCILIATION_METADATA_BYTES,
    MAX_EXTERNAL_RECONCILIATION_PATH_BYTES, MAX_EXTERNAL_RECONCILIATION_PATHS,
};

pub use full_capacity::{
    FULL_CAPACITY_PLAN_VERSION, FullCapacityAllocation, FullCapacityAllocationClass,
    FullCapacityPlan, FullCapacityValidationError, MAX_FULL_CAPACITY_ALLOCATIONS,
};

pub use full_progress_record::{FullProgressIntentObservation, FullProgressIntentObservationError};

pub use full_status::FullProgressDisposition;

pub use model::{
    CURRENT_SCHEMA_VERSION, CancellationRequestObservation, CancellationRequestValidationError,
    MAX_RECORD_BYTES, MAX_UNIVERSAL_LABEL_BYTES, MAX_USER_STATUS_KEY_BYTES, OperationActor,
    OperationDisposition, OperationId, OperationIntent, OperationKind, OperationPhase, SCHEMA_V1,
    SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7, SCHEMA_V8, SCHEMA_V9,
    SCHEMA_V10, SCHEMA_V11, SCHEMA_V12, SCHEMA_V13, SCHEMA_V14, SCHEMA_V15, UniversalCommandKind,
    UniversalIntent, UniversalIntentTextViolation, UniversalIntentValidationError,
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
pub use work_plan::{
    MAX_WORK_PLAN_EDGES, MAX_WORK_PLAN_UNITS, WORK_PLAN_VERSION, WORK_PLAN_VERSION_V1, WorkPlan,
    WorkPlanError, WorkPrerequisite, WorkUnit, WorkUnitApplicability, WorkUnitKey, WorkUnitRole,
    derive_work_plan, derive_work_plan_v1, validate_work_plan,
};
pub use work_progress::{
    AfterEffectReconciliationObservation, AuditTarget, AuditTargetValidationError,
    CancellationCleanupObservation, CompletionContext, CoordinatorDeficit, CoordinatorReceiptKind,
    CoordinatorReceiptSummary, FilesystemReceiptKind, FilesystemReceiptSummary,
    FullProgressSnapshotObservation, FullProgressSnapshotObservationError,
    MAX_PRE_PUBLISH_UNSTARTED_QUIESCENCE_ACTIONS, MAX_PRE_PUBLISH_UNSTARTED_QUIESCENCE_KEYS,
    ParticipantScope, PrePublishAbsenceObservation, PrePublishActionCleanup,
    PrePublishCancellationObservation, PrePublishCleanupObservation,
    PrePublishCleanupValidationError, PrePublishFinalClaimObservation,
    PrePublishOriginalStateObservation, PrePublishUnstartedQuiescenceAction,
    PrePublishUnstartedQuiescenceObservation, PrePublishUnstartedQuiescenceResult,
    PrePublishUnstartedQuiescenceValidationError, ProgressAuditObservation, ProgressAuditReason,
    ProgressEvidenceError, ProgressStepStatus, SourceRevisionObservation, StoredStepEvidence,
    WorkProgress, WorkProgressError, WorkProgressStep,
};
