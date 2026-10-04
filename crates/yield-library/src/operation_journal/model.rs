//! Pure values for the profile-local durable operation journal.
//!
//! The active values include legacy operation identity/phase data and the initial V3 universal
//! intent shape. They perform no filesystem I/O and grant no ownership or mutation authority.
//! Later journal evidence and codecs can depend on these values without importing renderer or UI
//! types. V3 currently describes a newly durable intent, V4 a pre-publication cancellation
//! request, V5 the first typed copy command and retry observations, and V6 the tagged command
//! envelope. V7 adds a typed in-place WAV edit observation, V8 adds bounded namespace mutation
//! observations, V9 adds whole-file render observations, V10 adds bounded configured-trash and
//! retained-folder removal observations, V11 adds retained-folder resolution observations, V12
//! adds selection-only Normalize observations, and V13 adds source-owned selection-output
//! observations. V14 adds bounded transaction-history Undo and Redo action observations. V15 adds
//! bounded, unproven external filesystem reconciliation observations. V16 adds a separate full
//! allocation-plan initial-intent representation while preserving the V1-V15 wire shapes.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use super::participants::{ParticipantSet, ParticipantValidationError};

/// The first persisted operation-journal schema.
pub const SCHEMA_V1: u32 = 1;
/// The second persisted operation-journal schema.
pub const SCHEMA_V2: u32 = 2;
/// The initial universal operation-journal schema.
pub const SCHEMA_V3: u32 = 3;
/// The durable pre-publication cancellation-request schema.
pub const SCHEMA_V4: u32 = 4;
/// The typed coordinator copy-intent and retry-observation schema.
pub const SCHEMA_V5: u32 = 5;
/// The universal coordinator command envelope schema.
pub const SCHEMA_V6: u32 = 6;
/// The typed in-place WAV edit command envelope schema.
pub const SCHEMA_V7: u32 = 7;
/// The bounded namespace mutation command envelope schema.
pub const SCHEMA_V8: u32 = 8;
/// The whole-file render command envelope schema.
pub const SCHEMA_V9: u32 = 9;
/// The configured-trash and retained-folder removal command envelope schema.
pub const SCHEMA_V10: u32 = 10;
/// The retained-folder restore and purge command envelope schema.
pub const SCHEMA_V11: u32 = 11;
/// The selection-only Normalize command envelope schema.
pub const SCHEMA_V12: u32 = 12;
/// The source-owned selection-output command envelope schema.
pub const SCHEMA_V13: u32 = 13;
/// The bounded transaction-history Undo and Redo command envelope schema.
pub const SCHEMA_V14: u32 = 14;
/// The bounded external filesystem reconciliation command envelope schema.
pub const SCHEMA_V15: u32 = 15;
/// The full allocation-plan initial-intent schema.
pub const SCHEMA_V16: u32 = 16;
/// Finite recording initial intent with full allocation observations.
pub const SCHEMA_V17: u32 = 17;
/// Explicit pre-audit source registry retirement command/fence record version.
pub const SCHEMA_V18: u32 = 18;
/// Current schema identifier retained for compatibility with the reference journal.
pub const CURRENT_SCHEMA_VERSION: u32 = SCHEMA_V1;
/// Maximum number of bytes accepted for one persisted operation record.
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;
/// Maximum byte length of a V3 user-facing operation label.
pub const MAX_UNIVERSAL_LABEL_BYTES: usize = 256;
/// Maximum byte length of a V3 stable user-status key.
pub const MAX_USER_STATUS_KEY_BYTES: usize = 128;

/// Opaque identity for one operation.
///
/// Constructing or decoding this value does not establish durable admission. The UUID
/// representation is an on-disk compatibility detail; callers should carry this type across
/// journal and file-owner boundaries instead of manufacturing or comparing raw UUIDs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct OperationId(Uuid);

impl OperationId {
    /// Generate a new operation identity.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap an existing UUID while preserving its persisted representation.
    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Parse an operation identity from a UUID spelling.
    pub fn parse(value: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(value).map(Self)
    }

    /// Return the UUID used by the persisted representation.
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for OperationId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Typed actor that admitted an operation to the journal.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum OperationActor {
    /// A user initiated operation.
    User,
    /// An operation observed from an external source.
    ExternalFs,
}

/// Typed operation family. Payload details remain bounded and opaque to this foundation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum OperationKind {
    /// Retire one original registered source through bounded global and lease-maintenance work.
    SourceRegistryRetirement,
    /// Adopt one exact pre-lease legacy source, separate from files and audited source edits.
    SourceLegacyAdoption,
    /// Source-local rebuildable SQL layout, separate from user history and files.
    SourceDerivedLayout,
    /// An operation originating from transaction history.
    FileHistory,
    /// An application-owned file operation.
    FileMutation,
    /// An external filesystem reconciliation operation.
    ExternalReconciliation,
}

/// Durable intent recorded before filesystem execution is admitted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OperationIntent {
    /// Actor that requested or observed the operation.
    pub actor: OperationActor,
    /// Operation family.
    pub kind: OperationKind,
    /// Bounded user-facing label, when available.
    pub label: String,
}

/// Concrete command recorded by a V3 universal intent.
///
/// This list follows the documented file and transaction workflows. Adding another command is
/// an explicit model and schema decision; unknown command values remain retained data.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum UniversalCommandKind {
    /// Original source registry drain/release/retired metadata, without authoritative audit coercion.
    SourceRegistryRetirement,
    /// Adopt one original reservation-owned known legacy source.
    SourceLegacyAdoption,
    /// Replace one bounded source-owned canonical layout generation.
    SourceDerivedLayout,
    /// Extract a rendered result from a source or waveform selection.
    Extract,
    /// Copy an existing file into a destination.
    Copy,
    /// Import one external regular file into a source database.
    Import,
    /// Create a new file.
    CreateFile,
    /// Create a new folder.
    CreateFolder,
    /// Duplicate an existing file.
    Duplicate,
    /// Export a file to a chosen destination.
    Export,
    /// Rename an existing file or folder.
    Rename,
    /// Move a file or folder.
    Move,
    /// Move an item to trash.
    Trash,
    /// Delete an item through an explicit delete workflow.
    Delete,
    /// Restore one existing retained folder through an explicit resolution workflow.
    RestoreRetained,
    /// Purge one existing retained folder through an explicit resolution workflow.
    PurgeRetained,
    /// Apply an explicitly destructive audio edit.
    DestructiveEdit,
    /// Undo a prior user transaction.
    Undo,
    /// Redo a prior user transaction.
    Redo,
    /// Reconcile an operation observed from an external filesystem owner.
    ExternalReconciliation,
}

impl UniversalCommandKind {
    /// Return the stable documentation identifier for this command family.
    pub const fn stable_id(self) -> &'static str {
        match self {
            Self::SourceRegistryRetirement => "family.source_registry_retirement",
            Self::SourceLegacyAdoption => "family.source_legacy_adoption",
            Self::SourceDerivedLayout => "family.source_derived_layout",
            Self::Extract => "family.extract",
            Self::Copy => "family.copy",
            Self::Import => "family.import",
            Self::CreateFile => "family.create_file",
            Self::CreateFolder => "family.create_folder",
            Self::Duplicate => "family.duplicate",
            Self::Export => "family.export",
            Self::Rename => "family.rename",
            Self::Move => "family.move",
            Self::Trash => "family.trash",
            Self::Delete => "family.delete",
            Self::RestoreRetained => "family.restore_retained",
            Self::PurgeRetained => "family.purge_retained",
            Self::DestructiveEdit => "family.destructive_edit",
            Self::Undo => "family.undo",
            Self::Redo => "family.redo",
            Self::ExternalReconciliation => "family.external_reconciliation",
        }
    }
}

/// Stored universal-intent fields added by schema V3.
///
/// This is an untrusted data value. It identifies the command, parent relationship, status
/// lookup key, and participant observations needed by a future coordinator. It contains no live
/// lease, queue admission, profile ownership, filesystem capability, or publication authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UniversalIntent {
    parent_operation_id: Option<OperationId>,
    command_kind: UniversalCommandKind,
    user_status_key: String,
    participants: ParticipantSet,
}

impl UniversalIntent {
    /// Construct a universal-intent observation after checking its bounded local fields.
    pub fn new(
        parent_operation_id: Option<OperationId>,
        command_kind: UniversalCommandKind,
        user_status_key: impl Into<String>,
        participants: ParticipantSet,
    ) -> Result<Self, UniversalIntentValidationError> {
        let intent = Self {
            parent_operation_id,
            command_kind,
            user_status_key: user_status_key.into(),
            participants,
        };
        intent.validate_shape()?;
        Ok(intent)
    }

    /// Validate this observation for a new `IntentDurable` record.
    pub fn validate_for_new_intent(
        &self,
        operation_id: OperationId,
        actor: OperationActor,
        kind: &OperationKind,
    ) -> Result<(), UniversalIntentValidationError> {
        self.validate_shape()?;
        if self.parent_operation_id == Some(operation_id) {
            return Err(UniversalIntentValidationError::SelfParent);
        }
        if !command_matches_intent(self.command_kind, actor, kind) {
            return Err(UniversalIntentValidationError::IncompatibleCommand {
                command_kind: self.command_kind,
            });
        }
        self.participants.validate_for_new_intent()?;
        Ok(())
    }

    /// Return the optional parent or compound operation identity.
    pub fn parent_operation_id(&self) -> Option<OperationId> {
        self.parent_operation_id
    }

    /// Return the typed command kind.
    pub const fn command_kind(&self) -> UniversalCommandKind {
        self.command_kind
    }

    /// Return the stable status lookup key.
    pub fn user_status_key(&self) -> &str {
        &self.user_status_key
    }

    /// Borrow the stored participant observations.
    pub fn participants(&self) -> &ParticipantSet {
        &self.participants
    }

    fn validate_shape(&self) -> Result<(), UniversalIntentValidationError> {
        if self.user_status_key.is_empty() {
            return Err(UniversalIntentValidationError::InvalidUserStatusKey {
                violation: UniversalIntentTextViolation::Empty,
            });
        }
        if self.user_status_key.len() > MAX_USER_STATUS_KEY_BYTES {
            return Err(UniversalIntentValidationError::InvalidUserStatusKey {
                violation: UniversalIntentTextViolation::TooLong,
            });
        }
        if self.user_status_key.chars().any(char::is_control) {
            return Err(UniversalIntentValidationError::InvalidUserStatusKey {
                violation: UniversalIntentTextViolation::ContainsControl,
            });
        }
        self.participants.validate_for_new_operation()?;
        Ok(())
    }
}

fn command_matches_intent(
    command_kind: UniversalCommandKind,
    actor: OperationActor,
    kind: &OperationKind,
) -> bool {
    match command_kind {
        UniversalCommandKind::SourceRegistryRetirement => {
            actor == OperationActor::User && *kind == OperationKind::SourceRegistryRetirement
        }
        UniversalCommandKind::SourceLegacyAdoption => {
            actor == OperationActor::User && *kind == OperationKind::SourceLegacyAdoption
        }
        UniversalCommandKind::SourceDerivedLayout => {
            actor == OperationActor::User && *kind == OperationKind::SourceDerivedLayout
        }
        UniversalCommandKind::ExternalReconciliation => {
            actor == OperationActor::ExternalFs && *kind == OperationKind::ExternalReconciliation
        }
        UniversalCommandKind::Undo | UniversalCommandKind::Redo => {
            actor == OperationActor::User && *kind == OperationKind::FileHistory
        }
        _ => actor == OperationActor::User && *kind == OperationKind::FileMutation,
    }
}

/// Reason a V3 status key failed its bounded text contract.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum UniversalIntentTextViolation {
    /// The key was empty.
    #[error("is empty")]
    Empty,
    /// The key exceeded [`MAX_USER_STATUS_KEY_BYTES`].
    #[error("exceeds its byte bound")]
    TooLong,
    /// The key contains a NUL or another control character.
    #[error("contains a control character")]
    ContainsControl,
}

/// Typed validation failure for a V3 universal-intent observation.
#[derive(Clone, Copy, Debug, Error, PartialEq)]
pub enum UniversalIntentValidationError {
    /// The parent identity cannot point back to this operation.
    #[error("a universal intent cannot name itself as its parent operation")]
    SelfParent,
    /// The command kind does not match the stored actor and operation family.
    #[error(
        "universal command {command_kind:?} is incompatible with its actor and operation family"
    )]
    IncompatibleCommand {
        /// Command kind whose compatibility failed.
        command_kind: UniversalCommandKind,
    },
    /// The stable status lookup key is outside its bounded text contract.
    #[error("invalid universal user-status key: {violation}")]
    InvalidUserStatusKey {
        /// Bounded text failure.
        violation: UniversalIntentTextViolation,
    },
    /// The participant inventory failed its bounded shape validation.
    #[error("invalid universal participants: {0}")]
    Participants(#[from] ParticipantValidationError),
}

/// Ordered journal phases. This foundation stores state but never executes a phase.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum OperationPhase {
    /// Intent and its initial record are durable.
    IntentDurable,
    /// Capabilities and participants have been prepared.
    Prepared,
    /// A filesystem staging participant has been created.
    FilesystemStaged,
    /// Filesystem publication has been observed and verified by a future coordinator.
    FilesystemPublished,
    /// Source participants have reconciled.
    SourceReconciled,
    /// Global participants have reconciled.
    GlobalReconciled,
    /// The UI projection has been published.
    ProjectionPublished,
    /// Readiness work has been scheduled.
    ReadinessScheduled,
    /// The operation has reached a terminal state.
    Terminal,
}

impl OperationPhase {
    /// Return whether this phase precedes filesystem publication.
    pub fn is_pre_publication(self) -> bool {
        matches!(
            self,
            Self::IntentDurable | Self::Prepared | Self::FilesystemStaged
        )
    }

    /// Return whether this phase follows filesystem publication.
    pub fn is_post_publication(self) -> bool {
        matches!(
            self,
            Self::FilesystemPublished
                | Self::SourceReconciled
                | Self::GlobalReconciled
                | Self::ProjectionPublished
                | Self::ReadinessScheduled
        )
    }
}

/// Durable disposition overlay for a journal record.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum OperationDisposition {
    /// No disposition overlay is currently active.
    None,
    /// Work may be retried by a future coordinator.
    RetryPending,
    /// A participant is incomplete and needs retry.
    PartialNeedsRetry,
    /// Evidence is uncertain and requires an audit.
    AuditRequired,
    /// Cancellation was requested before publication.
    CancelRequestedBeforePublish,
    /// Cancellation was requested after publication.
    CancelRequestedAfterPublish,
    /// Operation completed successfully.
    Succeeded,
    /// Operation completed while optional artifacts remain deferred.
    SucceededWithDeferredArtifacts,
    /// Operation was cancelled before publication.
    CancelledBeforePublish,
    /// Operation was cancelled after publication.
    CancelledAfterPublish,
    /// Operation was rolled back.
    RolledBack,
    /// Operation is blocked pending a user decision.
    BlockedByUser,
    /// Operation failed while preserving evidence.
    FailedPreservingData,
    /// Operation has unresolved data-loss risk.
    FailedDataLossRisk,
}

/// A bounded observation of when a user requested cancellation before publication.
///
/// This value is persisted as part of a V4 journal record. It contains no live cancellation
/// handle, ownership capability, cleanup authority, or permission to advance the operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CancellationRequestObservation {
    requested_unix_ms: i64,
}

impl CancellationRequestObservation {
    /// Construct a cancellation observation with a non-negative Unix-millisecond timestamp.
    pub const fn new(requested_unix_ms: i64) -> Result<Self, CancellationRequestValidationError> {
        if requested_unix_ms < 0 {
            return Err(CancellationRequestValidationError::NegativeTimestamp);
        }
        Ok(Self { requested_unix_ms })
    }

    /// Return the observed Unix-millisecond request time.
    pub const fn requested_unix_ms(self) -> i64 {
        self.requested_unix_ms
    }

    /// Validate an observation that may have come from an untrusted decoder.
    pub const fn validate(self) -> Result<(), CancellationRequestValidationError> {
        if self.requested_unix_ms < 0 {
            Err(CancellationRequestValidationError::NegativeTimestamp)
        } else {
            Ok(())
        }
    }
}

/// Failure while constructing or validating a cancellation request observation.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CancellationRequestValidationError {
    /// The request timestamp cannot precede the Unix epoch.
    #[error("cancellation request timestamp must be non-negative")]
    NegativeTimestamp,
}

impl OperationDisposition {
    /// Return whether this disposition terminates the operation.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::SucceededWithDeferredArtifacts
                | Self::CancelledBeforePublish
                | Self::CancelledAfterPublish
                | Self::RolledBack
                | Self::BlockedByUser
                | Self::FailedPreservingData
                | Self::FailedDataLossRisk
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_id_serde_round_trips_canonical_uuid_string() {
        let uuid = Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef")
            .expect("fixed UUID fixture is valid");
        let operation_id = OperationId::from_uuid(uuid);

        let encoded = serde_json::to_vec(&operation_id).expect("operation ID serializes");
        assert_eq!(encoded, br#""01234567-89ab-cdef-0123-456789abcdef""#);
        let decoded: OperationId = serde_json::from_slice(&encoded).expect("operation ID decodes");
        assert_eq!(decoded, operation_id);
        assert_eq!(operation_id.as_uuid(), uuid);
        assert_eq!(operation_id.to_string(), uuid.to_string());
        assert_eq!(OperationId::parse(&uuid.to_string()), Ok(operation_id));
        assert!(OperationId::parse("not-an-operation-id").is_err());
    }

    #[test]
    fn legacy_json_labels_and_publication_boundary_remain_compatible() {
        let intent: OperationIntent = serde_json::from_str(
            r#"{"actor":"ExternalFs","kind":"ExternalReconciliation","label":"legacy"}"#,
        )
        .expect("legacy intent fixture decodes");
        assert_eq!(intent.actor, OperationActor::ExternalFs);
        assert_eq!(intent.kind, OperationKind::ExternalReconciliation);
        assert_eq!(intent.label, "legacy");

        let staged: OperationPhase =
            serde_json::from_str(r#""FilesystemStaged""#).expect("staged phase decodes");
        let published: OperationPhase =
            serde_json::from_str(r#""FilesystemPublished""#).expect("published phase decodes");
        assert!(staged.is_pre_publication());
        assert!(!staged.is_post_publication());
        assert!(!published.is_pre_publication());
        assert!(published.is_post_publication());
        assert!(!OperationPhase::Terminal.is_pre_publication());
        assert!(!OperationPhase::Terminal.is_post_publication());
    }

    #[test]
    fn legacy_disposition_labels_keep_terminal_classification() {
        let retry: OperationDisposition =
            serde_json::from_str(r#""RetryPending""#).expect("retry disposition decodes");
        let cancelled: OperationDisposition = serde_json::from_str(r#""CancelledBeforePublish""#)
            .expect("cancel disposition decodes");
        assert!(!retry.is_terminal());
        assert!(cancelled.is_terminal());
    }

    #[test]
    fn cancellation_request_observation_is_bounded_and_opaque() {
        let observation = CancellationRequestObservation::new(42)
            .expect("non-negative cancellation timestamp is valid");
        assert_eq!(observation.requested_unix_ms(), 42);
        assert_eq!(observation.validate(), Ok(()));
        assert_eq!(
            CancellationRequestObservation::new(-1),
            Err(CancellationRequestValidationError::NegativeTimestamp)
        );
    }
}
