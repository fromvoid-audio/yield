//! Storage-neutral observations for transaction-history Undo and Redo.
//!
//! This module records the bounded file actions selected by a history owner. It contains no
//! filesystem capability, recovery-root lease, source handle, capacity reservation, or permission
//! to execute a restore. A later owner must revalidate every observation before acting on it.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use super::coordinator::{
    CopyCollisionPolicy, CopyEndpoint, MAX_COPY_CONTENT_IDENTITY_BYTES, StoredRelativePath,
    WaveformFileRevisionObservation,
};
use super::evidence::AbsentFinalObservation;
use super::model::UniversalCommandKind;
use super::namespace_commands::{
    NamespaceDirectoryEndpoint, NamespaceDirectoryLocator, NamespaceEntryEndpoint,
    NamespaceEntryExpectation, NamespaceEntryKind,
};
use super::participants::{
    MAX_SOURCE_IDENTITY_BYTES, ParticipantDescriptor, ParticipantId, ParticipantKind,
    ParticipantRequirement, ParticipantSet, ParticipantValidationError,
};
use crate::sample_sources::SourceId;

/// Maximum number of ordered history actions in one Undo or Redo observation.
pub const MAX_HISTORY_ACTIONS: usize = 32;
/// Maximum number of ordered file relocations across all folder-move actions.
pub const MAX_HISTORY_RELOCATIONS: usize = 128;
/// Maximum aggregate UTF-8 bytes of stored paths and identity observations.
pub const MAX_HISTORY_AGGREGATE_TEXT_BYTES: usize = 256 * 1024;

/// Runtime-session scope for one native transaction-history stream.
///
/// The UUID identifies the observed history scope only. It is not an operation id, a durable
/// generation proof, a source lease, or a filesystem capability. The native history counters remain
/// separate unsigned values.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HistoryScopeId(Uuid);

impl HistoryScopeId {
    /// Generate a non-nil runtime-session scope identity.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap a UUID as an untrusted scope observation.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Parse a scope UUID from its canonical string representation.
    pub fn parse(value: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(value).map(Self)
    }

    /// Return the observed UUID.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }

    /// Reject a nil UUID recovered from storage.
    pub const fn validate(self) -> Result<(), HistoryValidationError> {
        if self.0.is_nil() {
            Err(HistoryValidationError::NilScope)
        } else {
            Ok(())
        }
    }
}

impl Default for HistoryScopeId {
    fn default() -> Self {
        Self::new()
    }
}

/// Direction captured by one history execution request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum HistoryDirection {
    /// Restore the previous transaction state.
    Undo,
    /// Reapply the next transaction state.
    Redo,
}

impl HistoryDirection {
    /// Return the universal command kind for this direction.
    pub const fn command_kind(self) -> UniversalCommandKind {
        match self {
            Self::Undo => UniversalCommandKind::Undo,
            Self::Redo => UniversalCommandKind::Redo,
        }
    }
}

/// Expected current state of the original waveform target.
///
/// `Absent` is an absence observation, not proof that a future target name remains absent. The
/// owner must establish that condition again before a no-replace restore.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum HistoryRestoreTargetExpectation {
    /// Replace a target matching this strong current-file revision.
    Existing(WaveformFileRevisionObservation),
    /// Install the restore only after the target is reverified absent.
    Absent(AbsentFinalObservation),
}

impl HistoryRestoreTargetExpectation {
    /// Return the fixed replacement primitive selected by this observation.
    pub const fn policy(&self) -> HistoryRestoreTargetPolicy {
        match self {
            Self::Existing(_) => HistoryRestoreTargetPolicy::ReplaceExpectedIdentity,
            Self::Absent(_) => HistoryRestoreTargetPolicy::NoReplace,
        }
    }

    fn validate(&self) -> Result<(), HistoryValidationError> {
        if let Self::Existing(revision) = self {
            revision
                .validate()
                .map_err(|_| HistoryValidationError::InvalidShape("target revision"))?;
        }
        Ok(())
    }
}

/// Nonserialized target primitive derived from [`HistoryRestoreTargetExpectation`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryRestoreTargetPolicy {
    /// Replace only the expected existing target identity.
    ReplaceExpectedIdentity,
    /// Create the target with no replacement after live absence verification.
    NoReplace,
}

/// The only recovery-root role supported by this first history contract.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum HistoryRecoveryRootRole {
    /// A root configured by the future history owner and revalidated at execution time.
    ConfiguredRecoveryRoot,
}

/// Fixed recovery leaf selected by a direction-dependent waveform restore plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryRecoveryLeaf {
    /// The retained `before.wav` leaf.
    Before,
    /// The retained `after.wav` leaf.
    After,
    /// The retained `extracted.wav` leaf.
    Extracted,
}

/// Explicit direction-dependent waveform restore steps.
///
/// The plan is derived from the captured direction and optional extracted-file observation. It is
/// intentionally closed so the owner cannot smuggle a second mutation into a history action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryWaveformRestorePlan {
    /// Restore `before.wav`, then remove the extracted target when one was captured.
    Undo {
        /// Fixed backup leaf used for the original target restore.
        restore_backup: HistoryRecoveryLeaf,
        /// Fixed replacement primitive for the original target.
        restore_target_policy: HistoryRestoreTargetPolicy,
        /// Whether the ordered extracted-target removal follows the restore.
        remove_extracted: bool,
    },
    /// Restore `after.wav`, then recreate the extracted target when one was captured.
    Redo {
        /// Fixed backup leaf used for the original target restore.
        restore_backup: HistoryRecoveryLeaf,
        /// Fixed replacement primitive for the original target.
        restore_target_policy: HistoryRestoreTargetPolicy,
        /// Whether the ordered extracted-target recreation follows the restore.
        recreate_extracted: bool,
    },
}

/// Strong revision observation for one fixed recovery leaf.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRecoveryLeafObservation {
    file_revision: WaveformFileRevisionObservation,
}

impl HistoryRecoveryLeafObservation {
    /// Construct a retained recovery-leaf observation.
    pub fn new(
        file_revision: WaveformFileRevisionObservation,
    ) -> Result<Self, HistoryValidationError> {
        let leaf = Self { file_revision };
        leaf.validate()?;
        Ok(leaf)
    }

    /// Validate the strong revision without opening the recovery root.
    pub fn validate(&self) -> Result<(), HistoryValidationError> {
        self.file_revision
            .validate()
            .map_err(|_| HistoryValidationError::InvalidShape("recovery leaf revision"))
    }

    /// Borrow the strong revision observation.
    pub fn file_revision(&self) -> &WaveformFileRevisionObservation {
        &self.file_revision
    }
}

/// Observed members of the fixed `session_recovery/waveform_edits/edit-{UUID}` directory.
///
/// The fixed leaf names are represented by fields rather than persisted paths. The configured root
/// itself remains a future owner concern; the stored root and child identities only support later
/// revalidation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRecoveryBackupObservation {
    filesystem_participant_id: ParticipantId,
    root_role: HistoryRecoveryRootRole,
    root_identity: String,
    directory_id: Uuid,
    directory_identity: String,
    before: Option<HistoryRecoveryLeafObservation>,
    after: Option<HistoryRecoveryLeafObservation>,
    extracted: Option<HistoryRecoveryLeafObservation>,
}

impl HistoryRecoveryBackupObservation {
    /// Construct observations for the configured recovery root and fixed child directory.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        filesystem_participant_id: ParticipantId,
        root_identity: impl Into<String>,
        directory_id: Uuid,
        directory_identity: impl Into<String>,
        before: Option<HistoryRecoveryLeafObservation>,
        after: Option<HistoryRecoveryLeafObservation>,
        extracted: Option<HistoryRecoveryLeafObservation>,
    ) -> Result<Self, HistoryValidationError> {
        let observation = Self {
            filesystem_participant_id,
            root_role: HistoryRecoveryRootRole::ConfiguredRecoveryRoot,
            root_identity: root_identity.into(),
            directory_id,
            directory_identity: directory_identity.into(),
            before,
            after,
            extracted,
        };
        observation.validate()?;
        Ok(observation)
    }

    /// Validate bounded recovery observations without acquiring a root or lease.
    pub fn validate(&self) -> Result<(), HistoryValidationError> {
        validate_history_text(&self.root_identity, "recovery root identity")?;
        if self.directory_id.is_nil() {
            return Err(HistoryValidationError::InvalidRecoveryDirectory);
        }
        validate_history_text(&self.directory_identity, "recovery directory identity")?;
        for leaf in [&self.before, &self.after, &self.extracted]
            .into_iter()
            .flatten()
        {
            leaf.validate()?;
        }
        Ok(())
    }

    /// Return the required filesystem participant observation.
    pub const fn filesystem_participant_id(&self) -> ParticipantId {
        self.filesystem_participant_id
    }

    /// Return the configured recovery-root role.
    pub const fn root_role(&self) -> HistoryRecoveryRootRole {
        self.root_role
    }

    /// Return the observed configured-root identity.
    pub fn root_identity(&self) -> &str {
        &self.root_identity
    }

    /// Return the existing recovery-directory UUID observation.
    pub const fn directory_id(&self) -> Uuid {
        self.directory_id
    }

    /// Return the existing recovery-directory child identity.
    pub fn directory_identity(&self) -> &str {
        &self.directory_identity
    }

    /// Borrow the retained `before.wav` observation.
    pub fn before(&self) -> Option<&HistoryRecoveryLeafObservation> {
        self.before.as_ref()
    }

    /// Borrow the retained `after.wav` observation.
    pub fn after(&self) -> Option<&HistoryRecoveryLeafObservation> {
        self.after.as_ref()
    }

    /// Borrow the retained `extracted.wav` observation.
    pub fn extracted(&self) -> Option<&HistoryRecoveryLeafObservation> {
        self.extracted.as_ref()
    }

    /// Select the direction-specific original restore backup.
    pub fn selected_restore_backup(
        &self,
        direction: HistoryDirection,
    ) -> Option<&HistoryRecoveryLeafObservation> {
        match direction {
            HistoryDirection::Undo => self.before(),
            HistoryDirection::Redo => self.after(),
        }
    }
}

/// Direction-specific extracted-file mutation observation captured by an applied waveform edit.
///
/// Undo removes the extracted target only after matching its retained current strong revision.
/// Redo recreates the extracted target only after revalidating the captured absence observation.
/// The retained `extracted.wav` recovery leaf is stored separately in
/// [`HistoryRecoveryBackupObservation`] for both directions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum HistoryExtractedFileObservation {
    /// Undo removes an extracted target matching this current strong revision.
    UndoRemoveExpected {
        /// Source-relative target to remove.
        target: CopyEndpoint,
        /// Strong current revision required before removal.
        expected_current_revision: WaveformFileRevisionObservation,
    },
    /// Redo recreates an extracted target only after the target name is reverified absent.
    RedoRecreateAbsent {
        /// Source-relative target to recreate.
        target: CopyEndpoint,
        /// Captured absence observation requiring live revalidation.
        absence: AbsentFinalObservation,
    },
}

impl HistoryExtractedFileObservation {
    /// Construct the Undo extracted-target removal observation.
    pub fn undo_remove_expected(
        target: CopyEndpoint,
        expected_current_revision: WaveformFileRevisionObservation,
    ) -> Result<Self, HistoryValidationError> {
        let observation = Self::UndoRemoveExpected {
            target,
            expected_current_revision,
        };
        observation.validate_shape()?;
        Ok(observation)
    }

    /// Construct the Redo extracted-target recreation observation.
    pub fn redo_recreate_absent(
        target: CopyEndpoint,
        absence: AbsentFinalObservation,
    ) -> Result<Self, HistoryValidationError> {
        let observation = Self::RedoRecreateAbsent { target, absence };
        observation.validate_shape()?;
        Ok(observation)
    }

    /// Return the source-relative extracted target endpoint.
    pub fn target(&self) -> &CopyEndpoint {
        match self {
            Self::UndoRemoveExpected { target, .. } | Self::RedoRecreateAbsent { target, .. } => {
                target
            }
        }
    }

    /// Return the history direction represented by this extracted-target mutation.
    pub const fn direction(&self) -> HistoryDirection {
        match self {
            Self::UndoRemoveExpected { .. } => HistoryDirection::Undo,
            Self::RedoRecreateAbsent { .. } => HistoryDirection::Redo,
        }
    }

    /// Return the current extracted-target strong evidence used by Undo removal.
    pub fn expected_current_revision(&self) -> Option<&WaveformFileRevisionObservation> {
        match self {
            Self::UndoRemoveExpected {
                expected_current_revision,
                ..
            } => Some(expected_current_revision),
            Self::RedoRecreateAbsent { .. } => None,
        }
    }

    /// Return the captured absence observation used by Redo recreation.
    pub const fn absence(&self) -> Option<AbsentFinalObservation> {
        match self {
            Self::UndoRemoveExpected { .. } => None,
            Self::RedoRecreateAbsent { absence, .. } => Some(*absence),
        }
    }

    fn validate_shape(&self) -> Result<(), HistoryValidationError> {
        validate_copy_endpoint_shape(self.target())?;
        if let Self::UndoRemoveExpected {
            expected_current_revision,
            ..
        } = self
        {
            expected_current_revision
                .validate()
                .map_err(|_| HistoryValidationError::InvalidShape("extracted target evidence"))?;
        }
        Ok(())
    }
}

/// One source-database waveform restore observation from transaction history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryWaveformRestore {
    target: CopyEndpoint,
    source_revision: u64,
    target_expectation: HistoryRestoreTargetExpectation,
    before_content_identity: Option<String>,
    backups: HistoryRecoveryBackupObservation,
    extracted_file: Option<HistoryExtractedFileObservation>,
}

impl HistoryWaveformRestore {
    /// Construct one bounded waveform restore observation.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        target: CopyEndpoint,
        source_revision: u64,
        target_expectation: HistoryRestoreTargetExpectation,
        before_content_identity: Option<String>,
        backups: HistoryRecoveryBackupObservation,
        extracted_file: Option<HistoryExtractedFileObservation>,
    ) -> Result<Self, HistoryValidationError> {
        let restore = Self {
            target,
            source_revision,
            target_expectation,
            before_content_identity,
            backups,
            extracted_file,
        };
        restore.validate_shape_common()?;
        Ok(restore)
    }

    /// Validate shape and direction-specific retained backup selection.
    pub fn validate_for_direction(
        &self,
        direction: HistoryDirection,
    ) -> Result<(), HistoryValidationError> {
        self.validate_shape(direction)
    }

    /// Return the source-database target endpoint.
    pub fn target(&self) -> &CopyEndpoint {
        &self.target
    }

    /// Return the source revision expected by the target participant fence.
    pub const fn source_revision(&self) -> u64 {
        self.source_revision
    }

    /// Return the expected current target state.
    pub fn target_expectation(&self) -> &HistoryRestoreTargetExpectation {
        &self.target_expectation
    }

    /// Return the optional bounded pre-edit content identity observation.
    pub fn before_content_identity(&self) -> Option<&str> {
        self.before_content_identity.as_deref()
    }

    /// Return the retained recovery-directory observation.
    pub fn backups(&self) -> &HistoryRecoveryBackupObservation {
        &self.backups
    }

    /// Return the optional extracted target and evidence.
    pub fn extracted_file(&self) -> Option<&HistoryExtractedFileObservation> {
        self.extracted_file.as_ref()
    }

    /// Return the closed ordered plan for the supplied history direction.
    pub const fn planned_steps(&self, direction: HistoryDirection) -> HistoryWaveformRestorePlan {
        match direction {
            HistoryDirection::Undo => HistoryWaveformRestorePlan::Undo {
                restore_backup: HistoryRecoveryLeaf::Before,
                restore_target_policy: self.target_expectation.policy(),
                remove_extracted: matches!(
                    self.extracted_file,
                    Some(HistoryExtractedFileObservation::UndoRemoveExpected { .. })
                ),
            },
            HistoryDirection::Redo => HistoryWaveformRestorePlan::Redo {
                restore_backup: HistoryRecoveryLeaf::After,
                restore_target_policy: self.target_expectation.policy(),
                recreate_extracted: matches!(
                    self.extracted_file,
                    Some(HistoryExtractedFileObservation::RedoRecreateAbsent { .. })
                ),
            },
        }
    }

    fn validate_shape(&self, direction: HistoryDirection) -> Result<(), HistoryValidationError> {
        self.validate_shape_common()?;
        match direction {
            HistoryDirection::Undo => {
                if self.backups.before().is_none() || self.backups.after().is_some() {
                    return Err(HistoryValidationError::DirectionBackupMismatch);
                }
            }
            HistoryDirection::Redo => {
                if self.backups.after().is_none() || self.backups.before().is_some() {
                    return Err(HistoryValidationError::DirectionBackupMismatch);
                }
            }
        }
        if let Some(extracted) = &self.extracted_file {
            if extracted.direction() != direction {
                return Err(HistoryValidationError::DirectionExtractedTargetMismatch);
            }
        }
        Ok(())
    }

    fn validate_shape_common(&self) -> Result<(), HistoryValidationError> {
        validate_copy_endpoint_shape(&self.target)?;
        self.target_expectation.validate()?;
        if let Some(identity) = &self.before_content_identity {
            validate_bounded_history_text(
                identity,
                MAX_COPY_CONTENT_IDENTITY_BYTES,
                "content identity",
            )?;
        }
        self.backups.validate()?;
        if self.extracted_file.is_some() != self.backups.extracted().is_some() {
            return Err(HistoryValidationError::ExtractedBackupMismatch);
        }
        if let Some(extracted) = &self.extracted_file {
            extracted.validate_shape()?;
            if same_copy_endpoint(&self.target, extracted.target()) {
                return Err(HistoryValidationError::IdenticalPhysicalEntry);
            }
        }
        Ok(())
    }

    fn validate_against_participants(
        &self,
        direction: HistoryDirection,
        participants: &ParticipantSet,
    ) -> Result<(), HistoryValidationError> {
        self.validate_shape(direction)?;
        let source = required_source_descriptor(
            participants,
            self.target.participant_id(),
            self.target.source_id(),
        )?;
        let source_fence = source
            .source_fence()
            .ok_or(HistoryValidationError::SourceFenceMissing)?;
        if source_fence.source_revision() != self.source_revision {
            return Err(HistoryValidationError::SourceRevisionMismatch);
        }
        validate_required_filesystem_participant(
            participants,
            self.backups.filesystem_participant_id(),
        )?;
        if let Some(extracted) = &self.extracted_file {
            let extracted_descriptor = required_source_descriptor(
                participants,
                extracted.target().participant_id(),
                extracted.target().source_id(),
            )?;
            if extracted_descriptor.id() != source.id()
                || extracted.target().source_id() != self.target.source_id()
            {
                return Err(HistoryValidationError::WaveformSourcesMustMatch);
            }
        }
        Ok(())
    }
}

/// Fixed nonserialized preparation order for one native folder relocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryFolderRelocationPlan {
    /// Create and verify contained destination parents, then perform a `NoReplace` relocation.
    EnsureContainedParentDirectoriesThenNoReplace,
}

/// One ordered directory relocation retained by a native folder move action.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryFolderRelocation {
    source: NamespaceEntryEndpoint,
    destination: CopyEndpoint,
}

impl HistoryFolderRelocation {
    /// Construct one source-relative directory relocation with an implicit fixed plan.
    pub fn new(
        source: NamespaceEntryEndpoint,
        destination: CopyEndpoint,
    ) -> Result<Self, HistoryValidationError> {
        let relocation = Self {
            source,
            destination,
        };
        relocation.validate_shape()?;
        Ok(relocation)
    }

    /// Return the existing source entry observation.
    pub fn source(&self) -> &NamespaceEntryEndpoint {
        &self.source
    }

    /// Return the concrete source-relative destination locator.
    pub fn destination(&self) -> &CopyEndpoint {
        &self.destination
    }

    /// Return the fixed parent-creation and collision plan.
    pub const fn planned_steps(&self) -> HistoryFolderRelocationPlan {
        HistoryFolderRelocationPlan::EnsureContainedParentDirectoriesThenNoReplace
    }

    /// Return the fixed collision policy for this relocation.
    pub const fn collision_policy(&self) -> CopyCollisionPolicy {
        CopyCollisionPolicy::NoReplace
    }

    fn validate_shape(&self) -> Result<(), HistoryValidationError> {
        validate_namespace_entry_shape(&self.source)?;
        if self.source.expectation().kind() != NamespaceEntryKind::Directory {
            return Err(HistoryValidationError::FolderMoveRequiresDirectories);
        }
        validate_copy_endpoint_shape(&self.destination)
    }
}

/// Native folder move action represented as ordered directory relocations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryFolderMove {
    source_root: NamespaceDirectoryEndpoint,
    moves: Vec<HistoryFolderRelocation>,
}

impl HistoryFolderMove {
    /// Construct one same-source-database folder move observation.
    pub fn new(
        source_root: NamespaceDirectoryEndpoint,
        moves: Vec<HistoryFolderRelocation>,
    ) -> Result<Self, HistoryValidationError> {
        let folder_move = Self { source_root, moves };
        folder_move.validate_shape()?;
        Ok(folder_move)
    }

    /// Return the source-root directory observation.
    pub fn source_root(&self) -> &NamespaceDirectoryEndpoint {
        &self.source_root
    }

    /// Return the ordered relocation list.
    pub fn moves(&self) -> &[HistoryFolderRelocation] {
        &self.moves
    }

    /// Return the fixed collision policy for all relocations.
    pub const fn collision_policy(&self) -> CopyCollisionPolicy {
        CopyCollisionPolicy::NoReplace
    }

    fn validate_shape(&self) -> Result<(), HistoryValidationError> {
        validate_namespace_directory_shape(&self.source_root)?;
        if !matches!(self.source_root.locator(), NamespaceDirectoryLocator::Root) {
            return Err(HistoryValidationError::InvalidShape(
                "folder move source root must use the root locator",
            ));
        }
        if self.moves.is_empty() {
            return Err(HistoryValidationError::EmptyFolderMoves);
        }
        if self.moves.len() > MAX_HISTORY_RELOCATIONS {
            return Err(HistoryValidationError::TooManyRelocations);
        }
        for relocation in &self.moves {
            relocation.validate_shape()?;
        }
        for (index, left) in self.moves.iter().enumerate() {
            for right in self.moves.iter().skip(index + 1) {
                if left.source().relative_path() == right.source().relative_path() {
                    return Err(HistoryValidationError::DuplicateRelocationSource);
                }
                if same_copy_endpoint(left.destination(), right.destination()) {
                    return Err(HistoryValidationError::DuplicateRelocationDestination);
                }
            }
            if left.source().relative_path() == left.destination().relative_path() {
                return Err(HistoryValidationError::InvalidShape(
                    "folder move contains a no-op relocation",
                ));
            }
            if path_contains_path(
                left.source().relative_path(),
                left.destination().relative_path(),
            ) {
                return Err(HistoryValidationError::FolderMoveIntoItself);
            }
            if !path_is_contained_by_locator(
                left.source().relative_path(),
                self.source_root.locator(),
            ) {
                return Err(HistoryValidationError::SourceEntryOutsideRoot);
            }
        }
        Ok(())
    }

    fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), HistoryValidationError> {
        self.validate_shape()?;
        let source_root = required_source_descriptor(
            participants,
            self.source_root.participant_id(),
            self.source_root.source_id(),
        )?;
        let source_fence = source_root
            .source_fence()
            .ok_or(HistoryValidationError::SourceFenceMissing)?;
        if self.source_root.expectation().filesystem_identity() != source_fence.root_identity() {
            return Err(HistoryValidationError::SourceRootIdentityMismatch);
        }
        if self.source_root.expectation().source_revision() != source_fence.source_revision() {
            return Err(HistoryValidationError::SourceRevisionMismatch);
        }
        for relocation in &self.moves {
            let source = required_source_descriptor(
                participants,
                relocation.source().participant_id(),
                relocation.source().source_id(),
            )?;
            let destination = required_source_descriptor(
                participants,
                relocation.destination().participant_id(),
                relocation.destination().source_id(),
            )?;
            ensure_same_source_database(source_root, source)?;
            ensure_same_source_database(source_root, destination)?;
            if relocation.source().expectation().source_revision() != source_fence.source_revision()
            {
                return Err(HistoryValidationError::SourceRevisionMismatch);
            }
            if !path_is_contained_by_locator(
                relocation.source().relative_path(),
                self.source_root.locator(),
            ) {
                return Err(HistoryValidationError::SourceEntryOutsideRoot);
            }
        }
        validate_required_filesystem_participant(
            participants,
            participants
                .descriptors()
                .iter()
                .find(|descriptor| {
                    descriptor.kind() == ParticipantKind::Filesystem
                        && descriptor.requirement() == ParticipantRequirement::Required
                })
                .map(ParticipantDescriptor::id)
                .ok_or(HistoryValidationError::RequiredFilesystemParticipantMissing)?,
        )
    }
}

/// One ordered history file action.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum HistoryAction {
    /// Relocate the ordered folder-move entries.
    FolderMove(HistoryFolderMove),
    /// Restore one applied waveform edit and its optional extracted file.
    WaveformRestore(HistoryWaveformRestore),
}

impl HistoryAction {
    fn validate_shape(&self, direction: HistoryDirection) -> Result<(), HistoryValidationError> {
        match self {
            Self::FolderMove(folder_move) => folder_move.validate_shape(),
            Self::WaveformRestore(restore) => restore.validate_shape(direction),
        }
    }

    fn relocation_count(&self) -> usize {
        match self {
            Self::FolderMove(folder_move) => folder_move.moves.len(),
            Self::WaveformRestore(_) => 0,
        }
    }

    fn is_non_extracted_restore(&self) -> bool {
        matches!(
            self,
            Self::WaveformRestore(restore) if restore.extracted_file.is_none()
        )
    }

    fn validate_against_participants(
        &self,
        direction: HistoryDirection,
        participants: &ParticipantSet,
    ) -> Result<(), HistoryValidationError> {
        match self {
            Self::FolderMove(folder_move) => {
                folder_move.validate_against_participants(participants)
            }
            Self::WaveformRestore(restore) => {
                restore.validate_against_participants(direction, participants)
            }
        }
    }
}

/// Durable V14 transaction-history Undo or Redo observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryUndoRedoIntentArgs {
    scope: HistoryScopeId,
    execution_id: u64,
    transaction_id: u64,
    through_target: Option<u64>,
    direction: HistoryDirection,
    actions: Vec<HistoryAction>,
}

impl HistoryUndoRedoIntentArgs {
    /// Construct one bounded history action list without acquiring runtime history or filesystem
    /// authority.
    pub fn new(
        scope: HistoryScopeId,
        execution_id: u64,
        transaction_id: u64,
        through_target: Option<u64>,
        direction: HistoryDirection,
        actions: Vec<HistoryAction>,
    ) -> Result<Self, HistoryValidationError> {
        let args = Self {
            scope,
            execution_id,
            transaction_id,
            through_target,
            direction,
            actions,
        };
        args.validate()?;
        Ok(args)
    }

    /// Validate bounded shape and direction-dependent action rules.
    pub fn validate(&self) -> Result<(), HistoryValidationError> {
        self.scope.validate()?;
        if self.actions.is_empty() {
            return Err(HistoryValidationError::EmptyActions);
        }
        if self.actions.len() > MAX_HISTORY_ACTIONS {
            return Err(HistoryValidationError::TooManyActions);
        }
        let mut relocation_count = 0_usize;
        let mut text_budget = HistoryTextBudget::default();
        let mut non_extracted_restores = 0_usize;
        for action in &self.actions {
            action.validate_shape(self.direction)?;
            action.add_text_budget(&mut text_budget)?;
            relocation_count = relocation_count
                .checked_add(action.relocation_count())
                .ok_or(HistoryValidationError::TooManyRelocations)?;
            if relocation_count > MAX_HISTORY_RELOCATIONS {
                return Err(HistoryValidationError::TooManyRelocations);
            }
            if action.is_non_extracted_restore() {
                non_extracted_restores = non_extracted_restores
                    .checked_add(1)
                    .ok_or(HistoryValidationError::MixedNonExtractedRestore)?;
            }
        }
        if non_extracted_restores > 0 && (non_extracted_restores != 1 || self.actions.len() != 1) {
            return Err(HistoryValidationError::MixedNonExtractedRestore);
        }
        Ok(())
    }

    /// Validate source fences, the required filesystem participant, and all endpoint roles.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), HistoryValidationError> {
        self.validate()?;
        participants.validate_for_new_intent()?;
        for action in &self.actions {
            action.validate_against_participants(self.direction, participants)?;
        }
        Ok(())
    }

    /// Return the observed runtime history scope.
    pub const fn scope(&self) -> HistoryScopeId {
        self.scope
    }

    /// Return the native execution counter without reinterpreting it as an operation id.
    pub const fn execution_id(&self) -> u64 {
        self.execution_id
    }

    /// Return the native transaction counter without reinterpreting it as an operation id.
    pub const fn transaction_id(&self) -> u64 {
        self.transaction_id
    }

    /// Return the optional native through-target counter.
    pub const fn through_target(&self) -> Option<u64> {
        self.through_target
    }

    /// Return the captured direction.
    pub const fn direction(&self) -> HistoryDirection {
        self.direction
    }

    /// Return the universal Undo or Redo command kind.
    pub const fn command_kind(&self) -> UniversalCommandKind {
        self.direction.command_kind()
    }

    /// Return the ordered action observations.
    pub fn actions(&self) -> &[HistoryAction] {
        &self.actions
    }
}

/// Validate the strict V14 history command-arguments boundary before typed record decoding.
pub(crate) fn validate_persisted_history_command_args_value(
    value: &Value,
) -> Result<(), &'static str> {
    let args = serde_json::from_value::<HistoryUndoRedoIntentArgs>(value.clone())
        .map_err(|_| "persisted history command arguments are malformed")?;
    args.validate()
        .map_err(|_| "persisted history command arguments violate their bounded contract")
}

/// Why one stored history observation failed its bounded contract.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum HistoryValidationError {
    /// The runtime scope UUID was nil.
    #[error("history scope UUID must be non-nil")]
    NilScope,
    /// The action list was empty.
    #[error("history action list must not be empty")]
    EmptyActions,
    /// The action list exceeded its bound.
    #[error("history action list exceeds its bound")]
    TooManyActions,
    /// A folder action had no relocations.
    #[error("history folder move must contain at least one relocation")]
    EmptyFolderMoves,
    /// The aggregate relocation list exceeded its bound.
    #[error("history relocation count exceeds its bound")]
    TooManyRelocations,
    /// Paths and identity observations exceeded their aggregate bound.
    #[error("history paths and identities exceed their aggregate bound")]
    AggregateTextTooLarge,
    /// A non-extracted waveform restore was mixed with another action or duplicated.
    #[error("a non-extracted waveform restore must be the sole history action")]
    MixedNonExtractedRestore,
    /// The selected direction did not carry exactly its required backup leaf.
    #[error("history direction does not match retained before/after backups")]
    DirectionBackupMismatch,
    /// The extracted-target mutation variant did not match the history direction.
    #[error("history extracted-target mutation does not match its direction")]
    DirectionExtractedTargetMismatch,
    /// Extracted target and backup observations were not supplied together.
    #[error("history extracted target and extracted backup observations must agree")]
    ExtractedBackupMismatch,
    /// A bounded physical identity or endpoint shape was invalid.
    #[error("invalid history {0}")]
    InvalidShape(&'static str),
    /// The recovery directory UUID was nil.
    #[error("history recovery directory UUID must be non-nil")]
    InvalidRecoveryDirectory,
    /// An endpoint was not found in the participant inventory.
    #[error("history source participant is missing")]
    SourceParticipantMissing,
    /// An endpoint participant was not a required source database.
    #[error("history endpoint participant must be a required SourceDatabase")]
    SourceParticipantWrongKind,
    /// An endpoint source identity did not match its participant fence.
    #[error("history endpoint source identity does not match its participant fence")]
    SourceIdentityMismatch,
    /// A source participant did not carry a fence.
    #[error("history source participant has no source fence")]
    SourceFenceMissing,
    /// A source revision did not match its source fence.
    #[error("history source revision does not match its participant fence")]
    SourceRevisionMismatch,
    /// Source and extracted waveform targets were not in one source database.
    #[error("history waveform targets must share one source database")]
    WaveformSourcesMustMatch,
    /// The source root identity did not match the source fence.
    #[error("history source root identity does not match its fence")]
    SourceRootIdentityMismatch,
    /// Folder action roots or entries crossed source databases.
    #[error("history folder move entries must share one physical source database")]
    FolderMoveCrossSource,
    /// A directory relocation targeted itself or a descendant.
    #[error("history folder move cannot target itself or a descendant")]
    FolderMoveIntoItself,
    /// A folder move source was not a directory.
    #[error("history folder move supports directory entries only")]
    FolderMoveRequiresDirectories,
    /// A folder move source escaped its captured source-root locator.
    #[error("history folder move source is outside its captured source root")]
    SourceEntryOutsideRoot,
    /// A relocation repeated a source endpoint.
    #[error("history folder move contains duplicate source endpoints")]
    DuplicateRelocationSource,
    /// A relocation repeated a destination endpoint.
    #[error("history folder move contains duplicate destination endpoints")]
    DuplicateRelocationDestination,
    /// A source and extracted target identified the same physical entry.
    #[error("history waveform restore source and extracted target are identical")]
    IdenticalPhysicalEntry,
    /// The required filesystem participant was absent or invalid.
    #[error("history requires a required Filesystem participant")]
    RequiredFilesystemParticipantMissing,
    /// A decoded participant inventory was invalid.
    #[error("invalid history participants: {0}")]
    Participants(#[from] ParticipantValidationError),
}

impl HistoryAction {
    fn add_text_budget(
        &self,
        budget: &mut HistoryTextBudget,
    ) -> Result<(), HistoryValidationError> {
        match self {
            Self::FolderMove(folder_move) => {
                add_namespace_directory_text(budget, folder_move.source_root())?;
                for relocation in folder_move.moves() {
                    add_namespace_entry_text(budget, relocation.source())?;
                    add_copy_endpoint_text(budget, relocation.destination())?;
                }
            }
            Self::WaveformRestore(restore) => {
                add_copy_endpoint_text(budget, restore.target())?;
                if let HistoryRestoreTargetExpectation::Existing(revision) =
                    restore.target_expectation()
                {
                    add_revision_text(budget, revision)?;
                }
                if let Some(identity) = restore.before_content_identity() {
                    budget.add(identity)?;
                }
                add_recovery_text(budget, restore.backups())?;
                if let Some(extracted) = restore.extracted_file() {
                    add_copy_endpoint_text(budget, extracted.target())?;
                    if let Some(revision) = extracted.expected_current_revision() {
                        add_revision_text(budget, revision)?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct HistoryTextBudget {
    used: usize,
}

impl HistoryTextBudget {
    fn add(&mut self, value: &str) -> Result<(), HistoryValidationError> {
        self.used = self
            .used
            .checked_add(value.len())
            .ok_or(HistoryValidationError::AggregateTextTooLarge)?;
        if self.used > MAX_HISTORY_AGGREGATE_TEXT_BYTES {
            Err(HistoryValidationError::AggregateTextTooLarge)
        } else {
            Ok(())
        }
    }
}

fn add_copy_endpoint_text(
    budget: &mut HistoryTextBudget,
    endpoint: &CopyEndpoint,
) -> Result<(), HistoryValidationError> {
    budget.add(endpoint.source_id().as_str())?;
    budget.add(endpoint.relative_path().as_str())
}

fn add_revision_text(
    budget: &mut HistoryTextBudget,
    revision: &WaveformFileRevisionObservation,
) -> Result<(), HistoryValidationError> {
    budget.add(revision.filesystem_identity())?;
    budget.add(revision.change_marker())
}

fn add_namespace_entry_text(
    budget: &mut HistoryTextBudget,
    endpoint: &NamespaceEntryEndpoint,
) -> Result<(), HistoryValidationError> {
    budget.add(endpoint.source_id().as_str())?;
    budget.add(endpoint.relative_path().as_str())?;
    match endpoint.expectation() {
        NamespaceEntryExpectation::RegularFile(expectation) => {
            budget.add(expectation.filesystem_identity())?
        }
        NamespaceEntryExpectation::Directory(expectation) => {
            budget.add(expectation.filesystem_identity())?
        }
    }
    Ok(())
}

fn add_namespace_directory_text(
    budget: &mut HistoryTextBudget,
    endpoint: &NamespaceDirectoryEndpoint,
) -> Result<(), HistoryValidationError> {
    budget.add(endpoint.source_id().as_str())?;
    if let NamespaceDirectoryLocator::Child(path) = endpoint.locator() {
        budget.add(path.as_str())?;
    }
    budget.add(endpoint.expectation().filesystem_identity())
}

fn add_recovery_text(
    budget: &mut HistoryTextBudget,
    recovery: &HistoryRecoveryBackupObservation,
) -> Result<(), HistoryValidationError> {
    budget.add(recovery.root_identity())?;
    budget.add(recovery.directory_identity())?;
    for leaf in [recovery.before(), recovery.after(), recovery.extracted()]
        .into_iter()
        .flatten()
    {
        add_revision_text(budget, leaf.file_revision())?;
    }
    Ok(())
}

fn validate_history_text(value: &str, field: &'static str) -> Result<(), HistoryValidationError> {
    validate_bounded_history_text(value, MAX_SOURCE_IDENTITY_BYTES, field)
}

fn validate_bounded_history_text(
    value: &str,
    maximum: usize,
    field: &'static str,
) -> Result<(), HistoryValidationError> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        Err(HistoryValidationError::InvalidShape(field))
    } else {
        Ok(())
    }
}

fn validate_source_id(source_id: &SourceId) -> Result<(), HistoryValidationError> {
    validate_history_text(source_id.as_str(), "source identity")
}

fn validate_copy_endpoint_shape(endpoint: &CopyEndpoint) -> Result<(), HistoryValidationError> {
    validate_source_id(endpoint.source_id())?;
    endpoint
        .relative_path()
        .validate()
        .map_err(|_| HistoryValidationError::InvalidShape("endpoint path"))
}

fn validate_namespace_entry_shape(
    endpoint: &NamespaceEntryEndpoint,
) -> Result<(), HistoryValidationError> {
    validate_source_id(endpoint.source_id())?;
    endpoint
        .relative_path()
        .validate()
        .map_err(|_| HistoryValidationError::InvalidShape("namespace entry path"))?;
    endpoint
        .expectation()
        .validate()
        .map_err(|_| HistoryValidationError::InvalidShape("namespace entry expectation"))
}

fn validate_namespace_directory_shape(
    endpoint: &NamespaceDirectoryEndpoint,
) -> Result<(), HistoryValidationError> {
    validate_source_id(endpoint.source_id())?;
    endpoint
        .locator()
        .validate()
        .map_err(|_| HistoryValidationError::InvalidShape("namespace directory locator"))?;
    endpoint
        .expectation()
        .validate()
        .map_err(|_| HistoryValidationError::InvalidShape("namespace directory expectation"))
}

fn required_source_descriptor<'a>(
    participants: &'a ParticipantSet,
    participant_id: ParticipantId,
    source_id: &SourceId,
) -> Result<&'a ParticipantDescriptor, HistoryValidationError> {
    let descriptor = participants
        .descriptors()
        .iter()
        .find(|descriptor| descriptor.id() == participant_id)
        .ok_or(HistoryValidationError::SourceParticipantMissing)?;
    if descriptor.kind() != ParticipantKind::SourceDatabase
        || descriptor.requirement() != ParticipantRequirement::Required
    {
        return Err(HistoryValidationError::SourceParticipantWrongKind);
    }
    let fence = descriptor
        .source_fence()
        .ok_or(HistoryValidationError::SourceFenceMissing)?;
    if fence.source_id() != source_id {
        return Err(HistoryValidationError::SourceIdentityMismatch);
    }
    Ok(descriptor)
}

fn ensure_same_source_database(
    expected: &ParticipantDescriptor,
    actual: &ParticipantDescriptor,
) -> Result<(), HistoryValidationError> {
    let expected_fence = expected
        .source_fence()
        .ok_or(HistoryValidationError::SourceFenceMissing)?;
    let actual_fence = actual
        .source_fence()
        .ok_or(HistoryValidationError::SourceFenceMissing)?;
    if expected.id() != actual.id()
        || expected_fence.root_identity() != actual_fence.root_identity()
        || expected_fence.database_identity() != actual_fence.database_identity()
    {
        Err(HistoryValidationError::FolderMoveCrossSource)
    } else {
        Ok(())
    }
}

fn validate_required_filesystem_participant(
    participants: &ParticipantSet,
    participant_id: ParticipantId,
) -> Result<(), HistoryValidationError> {
    participants
        .descriptors()
        .iter()
        .find(|descriptor| descriptor.id() == participant_id)
        .filter(|descriptor| {
            descriptor.kind() == ParticipantKind::Filesystem
                && descriptor.requirement() == ParticipantRequirement::Required
        })
        .map(|_| ())
        .ok_or(HistoryValidationError::RequiredFilesystemParticipantMissing)
}

fn same_copy_endpoint(left: &CopyEndpoint, right: &CopyEndpoint) -> bool {
    left.participant_id() == right.participant_id()
        && left.source_id() == right.source_id()
        && left.relative_path() == right.relative_path()
}

fn path_contains_path(container: &StoredRelativePath, candidate: &StoredRelativePath) -> bool {
    candidate.as_str() == container.as_str()
        || candidate
            .as_str()
            .strip_prefix(container.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn path_is_contained_by_locator(
    path: &StoredRelativePath,
    locator: &NamespaceDirectoryLocator,
) -> bool {
    match locator {
        NamespaceDirectoryLocator::Root => true,
        NamespaceDirectoryLocator::Child(parent) => {
            path.as_str() != parent.as_str()
                && path
                    .as_str()
                    .strip_prefix(parent.as_str())
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_journal::{
        ParticipantCheckpoint, ParticipantCheckpointState, ParticipantDescriptor, StoredSourceFence,
    };
    use crate::sample_sources::SourceId;

    fn source_participants() -> (ParticipantSet, ParticipantId, SourceId) {
        let source_participant = ParticipantId::from_uuid(Uuid::from_u128(1));
        let filesystem_participant = ParticipantId::from_uuid(Uuid::from_u128(2));
        let source_id = SourceId::from_string("source-a");
        let fence = StoredSourceFence::new(source_id.clone(), "root-a", "database-a", 1, 7, 1)
            .expect("source fence");
        let participants = ParticipantSet::new(
            vec![
                ParticipantDescriptor::new(
                    source_participant,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(fence),
                    None,
                )
                .expect("source descriptor"),
                ParticipantDescriptor::new(
                    filesystem_participant,
                    ParticipantKind::Filesystem,
                    ParticipantRequirement::Required,
                    None,
                    None,
                )
                .expect("filesystem descriptor"),
            ],
            vec![
                ParticipantCheckpoint::new(
                    source_participant,
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("source checkpoint"),
                ParticipantCheckpoint::new(
                    filesystem_participant,
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("filesystem checkpoint"),
            ],
        )
        .expect("participants");
        (participants, source_participant, source_id)
    }

    fn copy_endpoint(
        participant_id: ParticipantId,
        source_id: SourceId,
        path: &str,
    ) -> CopyEndpoint {
        CopyEndpoint::new(
            participant_id,
            source_id,
            StoredRelativePath::new(path).expect("path"),
        )
        .expect("copy endpoint")
    }

    fn revision(identity: &str) -> WaveformFileRevisionObservation {
        WaveformFileRevisionObservation::new(identity, "change", 10, Some(11)).expect("revision")
    }

    fn backup(
        filesystem_participant_id: ParticipantId,
        before: Option<HistoryRecoveryLeafObservation>,
        after: Option<HistoryRecoveryLeafObservation>,
        extracted: Option<HistoryRecoveryLeafObservation>,
    ) -> HistoryRecoveryBackupObservation {
        HistoryRecoveryBackupObservation::new(
            filesystem_participant_id,
            "recovery-root",
            Uuid::from_u128(20),
            "edit-directory",
            before,
            after,
            extracted,
        )
        .expect("backup observation")
    }

    fn restore(
        direction: HistoryDirection,
        extracted: bool,
    ) -> (ParticipantSet, HistoryUndoRedoIntentArgs) {
        let (participants, source_participant, source_id) = source_participants();
        let filesystem_participant = ParticipantId::from_uuid(Uuid::from_u128(2));
        let target = copy_endpoint(source_participant, source_id.clone(), "source.wav");
        let extracted_target = copy_endpoint(source_participant, source_id, "extracted.wav");
        let before = HistoryRecoveryLeafObservation::new(revision("before")).expect("before");
        let after = HistoryRecoveryLeafObservation::new(revision("after")).expect("after");
        let extracted_backup = HistoryRecoveryLeafObservation::new(revision("extracted-backup"))
            .expect("extracted backup");
        let backups = match direction {
            HistoryDirection::Undo => backup(
                filesystem_participant,
                Some(before),
                None,
                extracted.then_some(extracted_backup),
            ),
            HistoryDirection::Redo => backup(
                filesystem_participant,
                None,
                Some(after),
                extracted.then_some(extracted_backup),
            ),
        };
        let extracted_file = extracted
            .then(|| match direction {
                HistoryDirection::Undo => HistoryExtractedFileObservation::undo_remove_expected(
                    extracted_target,
                    revision("current"),
                ),
                HistoryDirection::Redo => HistoryExtractedFileObservation::redo_recreate_absent(
                    extracted_target,
                    AbsentFinalObservation::ObservedAbsent,
                ),
            })
            .transpose()
            .expect("extracted file");
        let args = HistoryUndoRedoIntentArgs::new(
            HistoryScopeId::from_uuid(Uuid::from_u128(30)),
            4,
            5,
            Some(6),
            direction,
            vec![HistoryAction::WaveformRestore(
                HistoryWaveformRestore::new(
                    target,
                    7,
                    HistoryRestoreTargetExpectation::Existing(revision("target")),
                    Some(String::from("before-content")),
                    backups,
                    extracted_file,
                )
                .expect("restore"),
            )],
        )
        .expect("history args");
        (participants, args)
    }

    #[test]
    fn both_directions_select_their_retained_backup_and_closed_plan() {
        let (participants, undo) = restore(HistoryDirection::Undo, true);
        undo.validate_against_participants(&participants)
            .expect("undo participants");
        assert_eq!(undo.command_kind(), UniversalCommandKind::Undo);
        let HistoryAction::WaveformRestore(undo_restore) = &undo.actions()[0] else {
            panic!("waveform restore")
        };
        assert!(
            undo_restore
                .backups()
                .selected_restore_backup(HistoryDirection::Undo)
                .is_some()
        );
        assert!(matches!(
            undo_restore.planned_steps(HistoryDirection::Undo),
            HistoryWaveformRestorePlan::Undo {
                restore_backup: HistoryRecoveryLeaf::Before,
                remove_extracted: true,
                ..
            }
        ));

        let (participants, redo) = restore(HistoryDirection::Redo, true);
        redo.validate_against_participants(&participants)
            .expect("redo participants");
        assert_eq!(redo.command_kind(), UniversalCommandKind::Redo);
        let HistoryAction::WaveformRestore(redo_restore) = &redo.actions()[0] else {
            panic!("waveform restore")
        };
        assert!(
            redo_restore
                .backups()
                .selected_restore_backup(HistoryDirection::Redo)
                .is_some()
        );
        assert!(matches!(
            redo_restore.planned_steps(HistoryDirection::Redo),
            HistoryWaveformRestorePlan::Redo {
                restore_backup: HistoryRecoveryLeaf::After,
                recreate_extracted: true,
                ..
            }
        ));
    }

    #[test]
    fn absent_target_uses_no_replace_and_is_observation_only() {
        let (participants, mut args) = restore(HistoryDirection::Undo, false);
        if let HistoryAction::WaveformRestore(restore) = &mut args.actions[0] {
            restore.target_expectation =
                HistoryRestoreTargetExpectation::Absent(AbsentFinalObservation::ObservedAbsent);
        } else {
            panic!("waveform restore");
        }
        args.validate_against_participants(&participants)
            .expect("absent target observation");
        let HistoryAction::WaveformRestore(restore) = &args.actions[0] else {
            panic!("waveform restore")
        };
        assert_eq!(
            restore.target_expectation().policy(),
            HistoryRestoreTargetPolicy::NoReplace
        );
    }

    #[test]
    fn non_extracted_restore_cannot_mix_with_folder_action() {
        let (participants, mut args) = restore(HistoryDirection::Undo, false);
        let (source_participant, source_id) = {
            let descriptor = participants
                .descriptors()
                .iter()
                .find(|descriptor| descriptor.kind() == ParticipantKind::SourceDatabase)
                .expect("source descriptor");
            (
                descriptor.id(),
                descriptor
                    .source_fence()
                    .expect("fence")
                    .source_id()
                    .clone(),
            )
        };
        let source_root = NamespaceDirectoryEndpoint::new(
            source_participant,
            source_id.clone(),
            NamespaceDirectoryLocator::Root,
            super::super::namespace_commands::NamespaceDirectoryExpectation::new(7, "root-a", None)
                .expect("root expectation"),
        )
        .expect("source root");
        let source = NamespaceEntryEndpoint::new(
            source_participant,
            source_id.clone(),
            StoredRelativePath::new("folder").expect("source path"),
            NamespaceEntryExpectation::Directory(
                super::super::namespace_commands::NamespaceDirectoryExpectation::new(
                    7,
                    "folder",
                    Some(11),
                )
                .expect("directory expectation"),
            ),
        )
        .expect("entry");
        let destination = copy_endpoint(source_participant, source_id, "other/folder");
        let folder = HistoryFolderMove::new(
            source_root.clone(),
            vec![HistoryFolderRelocation::new(source, destination).expect("relocation")],
        );
        args.actions
            .push(HistoryAction::FolderMove(folder.expect("folder")));
        assert_eq!(
            args.validate(),
            Err(HistoryValidationError::MixedNonExtractedRestore)
        );
    }

    #[test]
    fn scope_and_unknown_fields_fail_closed_at_persisted_boundary() {
        let (_, args) = restore(HistoryDirection::Undo, true);
        let mut value = serde_json::to_value(&args).expect("history JSON");
        value["scope"] = serde_json::json!("00000000-0000-0000-0000-000000000000");
        assert!(validate_persisted_history_command_args_value(&value).is_err());
        let mut unknown = serde_json::to_value(&args).expect("history JSON");
        unknown["unknown"] = Value::Null;
        assert!(validate_persisted_history_command_args_value(&unknown).is_err());
    }

    #[test]
    fn direction_identity_route_and_action_limits_fail_closed() {
        let (_participants, mut undo) = restore(HistoryDirection::Undo, true);
        let HistoryAction::WaveformRestore(waveform_restore) = &mut undo.actions[0] else {
            panic!("waveform restore")
        };
        let extracted_target = match waveform_restore.extracted_file.take() {
            Some(HistoryExtractedFileObservation::UndoRemoveExpected { target, .. }) => target,
            Some(HistoryExtractedFileObservation::RedoRecreateAbsent { target, .. }) => target,
            None => panic!("extracted target"),
        };
        waveform_restore.extracted_file =
            Some(HistoryExtractedFileObservation::RedoRecreateAbsent {
                target: extracted_target,
                absence: AbsentFinalObservation::ObservedAbsent,
            });
        assert_eq!(
            undo.validate(),
            Err(HistoryValidationError::DirectionExtractedTargetMismatch)
        );

        let (_, valid) = restore(HistoryDirection::Undo, true);
        let action = valid.actions()[0].clone();
        let too_many = HistoryUndoRedoIntentArgs {
            scope: valid.scope,
            execution_id: valid.execution_id,
            transaction_id: valid.transaction_id,
            through_target: valid.through_target,
            direction: valid.direction,
            actions: vec![action; MAX_HISTORY_ACTIONS + 1],
        };
        assert_eq!(
            too_many.validate(),
            Err(HistoryValidationError::TooManyActions)
        );

        let (_, mut invalid_identity) = restore(HistoryDirection::Undo, false);
        let HistoryAction::WaveformRestore(invalid_restore) = &mut invalid_identity.actions[0]
        else {
            panic!("waveform restore")
        };
        invalid_restore.target_expectation = HistoryRestoreTargetExpectation::Existing(
            serde_json::from_value(serde_json::json!({
                "filesystem_identity": "",
                "change_marker": "change",
                "byte_len": 10,
                "modified_unix_ns": 11
            }))
            .expect("invalid revision can decode before validation"),
        );
        assert!(matches!(
            invalid_identity.validate(),
            Err(HistoryValidationError::InvalidShape("target revision"))
        ));

        let (route_participants, mut route) = restore(HistoryDirection::Undo, false);
        let HistoryAction::WaveformRestore(restore) = &mut route.actions[0] else {
            panic!("waveform restore")
        };
        restore.target = copy_endpoint(
            ParticipantId::from_uuid(Uuid::from_u128(1)),
            SourceId::from_string("other-source"),
            "source.wav",
        );
        assert!(matches!(
            route.validate_against_participants(&route_participants),
            Err(HistoryValidationError::SourceIdentityMismatch)
                | Err(HistoryValidationError::WaveformSourcesMustMatch)
        ));
    }

    #[test]
    fn folder_move_is_directory_only_and_requires_source_root_locator() {
        let (_participants, source_participant, source_id) = source_participants();
        let root = NamespaceDirectoryEndpoint::new(
            source_participant,
            source_id.clone(),
            NamespaceDirectoryLocator::Child(StoredRelativePath::new("folder").expect("root")),
            super::super::namespace_commands::NamespaceDirectoryExpectation::new(7, "folder", None)
                .expect("root expectation"),
        )
        .expect("source root");
        let file = NamespaceEntryEndpoint::new(
            source_participant,
            source_id.clone(),
            StoredRelativePath::new("folder/file.wav").expect("file path"),
            NamespaceEntryExpectation::RegularFile(
                super::super::namespace_commands::NamespaceFileExpectation::new(
                    7,
                    "file",
                    10,
                    Some(11),
                )
                .expect("file expectation"),
            ),
        )
        .expect("file entry");
        let destination = copy_endpoint(source_participant, source_id.clone(), "archive/file.wav");
        assert_eq!(
            HistoryFolderRelocation::new(file, destination),
            Err(HistoryValidationError::FolderMoveRequiresDirectories)
        );

        let outside = NamespaceEntryEndpoint::new(
            source_participant,
            source_id.clone(),
            StoredRelativePath::new("outside").expect("outside path"),
            NamespaceEntryExpectation::Directory(
                super::super::namespace_commands::NamespaceDirectoryExpectation::new(
                    7, "outside", None,
                )
                .expect("directory expectation"),
            ),
        )
        .expect("outside entry");
        let outside_destination = copy_endpoint(source_participant, source_id, "archive/outside");
        let move_action = HistoryFolderMove::new(
            root.clone(),
            vec![
                HistoryFolderRelocation::new(outside, outside_destination)
                    .expect("outside relocation"),
            ],
        );
        assert_eq!(
            move_action,
            Err(HistoryValidationError::InvalidShape(
                "folder move source root must use the root locator",
            ))
        );
    }
}
