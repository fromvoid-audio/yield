//! Captured acquisition fencing for ordinary profile metadata mutations.
//!
//! This is acquisition-local correlation, not durable OperationId replay or authority.
use super::connection::LibraryDatabase;
use super::error::LibraryError;
use super::owner::{GlobalLibraryWriterQueueError, SourceRegistrySnapshot};
use super::registry_limits::RegistryBoundError;
use super::tag_vocabulary::{
    ProfileTagCategory, ProfileTagVocabularyDeleteReceipt, ProfileTagVocabularySetReceipt,
    validate_delete_input, validate_set_input,
};
use crate::app_dirs::WritableProfileGuard;
use crate::sample_sources::UI_PREFERENCES_MAX_BYTES;

/// Equality observation of one retained profile acquisition; carries no writable capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileAcquisitionId(String);

impl ProfileAcquisitionId {
    pub(super) fn from_guard(guard: &WritableProfileGuard) -> Self {
        Self(guard.owner_identity().to_owned())
    }

    #[cfg(test)]
    pub(super) fn test_only() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Diagnostic acquisition UUID. Reacquisition changes it; clones retain it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The finite set of ordinary profile mutation routes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GlobalMutationKind {
    /// Current configured source snapshot, not physical source retirement.
    ConfiguredRegistry,
    /// Versioned bounded UI preference payload.
    UiPreferences,
    /// Persistent revision CAS for a profile label/category.
    VocabularySet,
    /// Persistent revision CAS with a transient removed-row result.
    VocabularyDelete,
}

/// Immutable proposal correlation, captured before delayed dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalMutationContext {
    acquisition: ProfileAcquisitionId,
    correlation: u64,
    kind: GlobalMutationKind,
    payload_identity: [u8; 32],
}

impl GlobalMutationContext {
    /// Acquisition observed when the proposal was constructed.
    pub fn acquisition(&self) -> &ProfileAcquisitionId {
        &self.acquisition
    }
    /// Caller-local result correlation. Snapshot routes use their strictly ordered proposal
    /// revision; vocabulary routes use a local request ID. Neither is a durable operation ID.
    pub fn correlation(&self) -> u64 {
        self.correlation
    }
    /// Typed route covered by this proposal.
    pub fn kind(&self) -> GlobalMutationKind {
        self.kind
    }
    /// Domain-separated identity of the frozen bounded payload, including CAS preconditions.
    pub fn payload_identity(&self) -> &[u8; 32] {
        &self.payload_identity
    }
}

#[derive(Clone, Debug)]
enum Mutation {
    Registry(SourceRegistrySnapshot),
    Preferences(String),
    Set {
        expected_revision: u64,
        label: String,
        category: ProfileTagCategory,
    },
    Delete {
        expected_revision: u64,
        label: String,
    },
}

/// A bounded immutable ordinary mutation. Construct using the acquisition captured by its
/// producer, never a replacement writer's acquisition at delayed submission time.
#[derive(Clone, Debug)]
pub struct GlobalMutationRequest {
    context: GlobalMutationContext,
    mutation: Mutation,
}

impl GlobalMutationRequest {
    fn build(
        acquisition: ProfileAcquisitionId,
        correlation: u64,
        kind: GlobalMutationKind,
        mutation: Mutation,
        fields: &[&[u8]],
    ) -> Self {
        let mut hash = blake3::Hasher::new();
        hash.update(b"yield-global-ordinary-mutation-v1\0");
        hash.update(&[match kind {
            GlobalMutationKind::ConfiguredRegistry => 0,
            GlobalMutationKind::UiPreferences => 1,
            GlobalMutationKind::VocabularySet => 2,
            GlobalMutationKind::VocabularyDelete => 3,
        }]);
        for field in fields {
            hash.update(&(field.len() as u64).to_le_bytes());
            hash.update(field);
        }
        Self {
            context: GlobalMutationContext {
                acquisition,
                correlation,
                kind,
                payload_identity: *hash.finalize().as_bytes(),
            },
            mutation,
        }
    }

    /// Freeze a validated source snapshot at its acquisition-local proposal revision.
    /// `correlation` is the ordered registry revision, not an arbitrary request ID.
    pub fn registry(
        acquisition: ProfileAcquisitionId,
        correlation: u64,
        snapshot: SourceRegistrySnapshot,
    ) -> Result<Self, GlobalLibraryWriterQueueError> {
        snapshot.validate_for_admission()?;
        let encoded = serde_json::to_vec(snapshot.as_slice()).map_err(|_| {
            GlobalLibraryWriterQueueError::from_registry_bound_error(
                RegistryBoundError::Serialization,
            )
        })?;
        Ok(Self::build(
            acquisition,
            correlation,
            GlobalMutationKind::ConfiguredRegistry,
            Mutation::Registry(snapshot),
            &[&encoded],
        ))
    }

    /// Freeze bounded preferences at an acquisition-local proposal revision (`correlation`).
    /// The versioned payload codec remains the session's contract.
    pub fn preferences(
        acquisition: ProfileAcquisitionId,
        correlation: u64,
        payload: String,
    ) -> Result<Self, GlobalLibraryWriterQueueError> {
        if payload.len() > UI_PREFERENCES_MAX_BYTES {
            return Err(GlobalLibraryWriterQueueError::PayloadTooLarge {
                limit: UI_PREFERENCES_MAX_BYTES,
                actual: payload.len(),
            });
        }
        let request = Self::build(
            acquisition,
            correlation,
            GlobalMutationKind::UiPreferences,
            Mutation::Preferences(payload.clone()),
            &[payload.as_bytes()],
        );
        Ok(request)
    }

    /// Freeze a bounded vocabulary CAS assignment, including its expected durable revision.
    pub fn vocabulary_set(
        acquisition: ProfileAcquisitionId,
        correlation: u64,
        expected_revision: u64,
        label: String,
        category: ProfileTagCategory,
    ) -> Result<Self, GlobalLibraryWriterQueueError> {
        validate_set_input(&label, category)?;
        let request = Self::build(
            acquisition,
            correlation,
            GlobalMutationKind::VocabularySet,
            Mutation::Set {
                expected_revision,
                label: label.clone(),
                category,
            },
            &[
                &expected_revision.to_le_bytes(),
                label.as_bytes(),
                category.id().as_bytes(),
            ],
        );
        Ok(request)
    }

    /// Freeze one bounded vocabulary CAS deletion. No restart-recoverable Undo is promised.
    pub fn vocabulary_delete(
        acquisition: ProfileAcquisitionId,
        correlation: u64,
        expected_revision: u64,
        label: String,
    ) -> Result<Self, GlobalLibraryWriterQueueError> {
        validate_delete_input(&label)?;
        let request = Self::build(
            acquisition,
            correlation,
            GlobalMutationKind::VocabularyDelete,
            Mutation::Delete {
                expected_revision,
                label: label.clone(),
            },
            &[&expected_revision.to_le_bytes(), label.as_bytes()],
        );
        Ok(request)
    }

    /// Retain this context to validate the eventual completion, including rejected requests.
    pub fn context(&self) -> &GlobalMutationContext {
        &self.context
    }

    pub(super) fn failed(self, error: LibraryError) -> GlobalMutationCompletion {
        GlobalMutationCompletion {
            context: self.context,
            result: Err(error),
        }
    }

    pub(super) fn execute(
        self,
        database: &mut LibraryDatabase,
        owner_identity: &str,
        fences: &mut SnapshotFences,
    ) -> GlobalMutationCompletion {
        let result = if self.context.acquisition.as_str() != owner_identity {
            Err(LibraryError::GlobalMutationStaleAcquisition)
        } else if let Err(error) = fences.check(&self.context) {
            Err(error)
        } else if fences.is_replay(&self.context) {
            Ok(match self.context.kind {
                GlobalMutationKind::ConfiguredRegistry => GlobalMutationValue::RegistryCommitted,
                GlobalMutationKind::UiPreferences => GlobalMutationValue::PreferencesCommitted,
                _ => unreachable!("only snapshot routes have fence slots"),
            })
        } else {
            match self.mutation {
                Mutation::Registry(snapshot) => database
                    .replace_source_registry(&snapshot)
                    .map(|()| GlobalMutationValue::RegistryCommitted),
                Mutation::Preferences(payload) => database
                    .replace_ui_preferences_payload(&payload)
                    .map(|()| GlobalMutationValue::PreferencesCommitted),
                Mutation::Set {
                    expected_revision,
                    label,
                    category,
                } => database
                    .set_profile_tag_category(expected_revision, &label, category)
                    .map(GlobalMutationValue::VocabularySet),
                Mutation::Delete {
                    expected_revision,
                    label,
                } => database
                    .delete_profile_tag_vocabulary(expected_revision, &label)
                    .map(GlobalMutationValue::VocabularyDelete),
            }
        };
        if result.is_ok() {
            fences.record(&self.context);
        }
        GlobalMutationCompletion {
            context: self.context,
            result,
        }
    }
}

/// Confirmed metadata result after SQLite commit and the owner's post-command guard check.
#[derive(Debug)]
pub enum GlobalMutationValue {
    /// Configured registry transaction committed; no physical lifecycle proof.
    RegistryCommitted,
    /// Preference transaction committed.
    PreferencesCommitted,
    /// Original vocabulary CAS/no-op result, not durable OperationId replay.
    VocabularySet(ProfileTagVocabularySetReceipt),
    /// Original removed-row result, available only through this completion.
    VocabularyDelete(ProfileTagVocabularyDeleteReceipt),
}

/// Owner completion echoing the immutable proposal context even on rejection/unconfirmable I/O.
#[derive(Debug)]
pub struct GlobalMutationCompletion {
    context: GlobalMutationContext,
    result: Result<GlobalMutationValue, LibraryError>,
}

impl GlobalMutationCompletion {
    /// Proposal context carried by this observation.
    pub fn context(&self) -> &GlobalMutationContext {
        &self.context
    }

    /// Consume only a completion matching the original acquisition, route, correlation and
    /// payload. A mismatch supplies no persisted acknowledgement or UI Undo data.
    pub fn into_result_for(
        self,
        expected: &GlobalMutationContext,
    ) -> Result<GlobalMutationValue, LibraryError> {
        if &self.context != expected {
            return Err(LibraryError::GlobalMutationCompletionMismatch);
        }
        self.result
    }

    pub(super) fn replace_result_with_error(mut self, error: LibraryError) -> Self {
        self.result = Err(error);
        self
    }
}

// Two fixed acknowledgement slots; no old payloads, durable history or caller-key map.
#[derive(Default)]
pub(super) struct SnapshotFences {
    slots: [Option<(u64, [u8; 32])>; 2],
}

impl SnapshotFences {
    fn index(context: &GlobalMutationContext) -> Option<usize> {
        match context.kind {
            GlobalMutationKind::ConfiguredRegistry => Some(0),
            GlobalMutationKind::UiPreferences => Some(1),
            _ => None,
        }
    }

    fn check(&self, context: &GlobalMutationContext) -> Result<(), LibraryError> {
        let Some(index) = Self::index(context) else {
            return Ok(());
        };
        let Some((revision, identity)) = self.slots[index] else {
            return Ok(());
        };
        if context.correlation < revision {
            return Err(LibraryError::GlobalSnapshotStaleRevision {
                proposed: context.correlation,
                committed: revision,
            });
        }
        if context.correlation == revision && context.payload_identity != identity {
            return Err(LibraryError::GlobalSnapshotPayloadConflict { revision });
        }
        Ok(())
    }

    fn is_replay(&self, context: &GlobalMutationContext) -> bool {
        Self::index(context).is_some_and(|index| {
            self.slots[index] == Some((context.correlation, context.payload_identity))
        })
    }

    fn record(&mut self, context: &GlobalMutationContext) {
        if let Some(index) = Self::index(context) {
            self.slots[index] = Some((context.correlation, context.payload_identity));
        }
    }
}
