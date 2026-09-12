//! Profile-local durable operation-journal values and bounded codec.
//!
//! This module exposes the stable identity, intent, phase, disposition, stored evidence, and
//! strict persisted-record codec. It does not open files, acquire ownership, publish filesystem
//! entries, or interpret journal data as mutation authority.

pub mod codec;
pub mod evidence;
pub mod model;
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

pub use validation::{RecordConsistency, classify_record};
