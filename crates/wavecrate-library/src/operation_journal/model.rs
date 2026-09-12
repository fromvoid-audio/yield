//! Pure values for the profile-local durable operation journal.
//!
//! This first active slice contains only the stable operation identity, intent, phase, and
//! disposition model. It performs no filesystem I/O and grants no ownership or mutation
//! authority. Later journal evidence and codecs can depend on these values without importing
//! renderer or UI types.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The first persisted operation-journal schema.
pub const SCHEMA_V1: u32 = 1;
/// The second persisted operation-journal schema.
pub const SCHEMA_V2: u32 = 2;
/// Current schema identifier retained for compatibility with the reference journal.
pub const CURRENT_SCHEMA_VERSION: u32 = SCHEMA_V1;

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
}
