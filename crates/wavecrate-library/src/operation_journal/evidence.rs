//! Stored operation evidence values.
//!
//! These values describe untrusted observations retained by a journal record. They are not open
//! handles, ownership proofs, publication authority, or permission to mutate a filesystem. In
//! particular, deserializing a publication value never produces a qualified execution result.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Direction captured by a stored prepared restore observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PreparedRestoreDirection {
    /// Restore the previous file state.
    Undo,
    /// Restore the next file state.
    Redo,
}

/// Stable identity observed for a filesystem object.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparedObjectIdentity {
    /// Opaque backend identity retained from the observation.
    pub stable_id: String,
    /// Optional backend change marker retained from the observation.
    pub change_marker: Option<String>,
    /// Object length retained from the observation.
    pub len: u64,
}

/// Stored root locator and identity observation.
///
/// The path is an untrusted recorded locator. It is not a retained descriptor or a root
/// capability, and it cannot authorize a later filesystem operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecordedRootLocator {
    /// Absolute path spelling recorded during preparation.
    pub path: PathBuf,
    /// Root identity recorded during preparation.
    pub identity: PreparedObjectIdentity,
}

/// Stored regular-leaf locator and identity observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparedLeafLocator {
    /// Relative path spelling recorded during preparation.
    pub relative_path: PathBuf,
    /// Leaf identity recorded during preparation.
    pub identity: PreparedObjectIdentity,
}

/// Stored destination-local staging locator.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparedStagingLocator {
    /// Relative path spelling recorded for the staging name.
    pub relative_path: PathBuf,
    /// Whether the staging name was observed absent.
    pub absent: bool,
}

/// Identity expected by a future replacement owner.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ReplaceExpectedIdentity {
    /// Existing target identity recorded as the expected replacement operand.
    Existing(PreparedObjectIdentity),
}

/// Content and metadata observations retained during preparation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparedRestoreEvidence {
    /// Evidence recorded for the target object.
    pub target: PreparedFileEvidence,
    /// Evidence recorded for the backup object.
    pub backup: PreparedFileEvidence,
}

/// Stored file observation used by preparation and staged-copy records.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PreparedFileEvidence {
    /// The object was observed absent.
    Missing,
    /// Exact content hash observation.
    ContentHash([u8; 32]),
    /// Metadata observation without an exact content hash.
    Metadata {
        /// Observed object length.
        len: u64,
        /// Optional observed modification timestamp in nanoseconds.
        modified_ns: Option<i64>,
        /// Whether the observed object was a directory.
        is_dir: bool,
    },
    /// The object could not be verified further.
    Unverifiable,
}

/// Stored preparation evidence for an existing-target restore.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparedWaveformRestore {
    /// Restore direction observed during preparation.
    pub direction: PreparedRestoreDirection,
    /// Opaque source identity recorded by the owner.
    pub source_id: String,
    /// Stored source-root locator and identity observation.
    pub source_root: RecordedRootLocator,
    /// Stored target-root locator and identity observation.
    pub target_root: RecordedRootLocator,
    /// Stored target leaf locator and identity observation.
    pub target: PreparedLeafLocator,
    /// Stored backup-root locator and identity observation.
    pub backup_root: RecordedRootLocator,
    /// Stored backup leaf locator and identity observation.
    pub backup: PreparedLeafLocator,
    /// Stored expected replacement identity.
    pub replacement: ReplaceExpectedIdentity,
    /// Stored destination-local staging locator.
    pub staging: PreparedStagingLocator,
    /// Stored target and backup observations.
    pub evidence: PreparedRestoreEvidence,
}

/// Stored observation that an absent final name was seen before staging.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AbsentFinalObservation {
    /// The final name was observed absent.
    ObservedAbsent,
}

/// Stored preparation evidence for an absent-final no-replace operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparedAbsentFinalNoReplace {
    /// Restore direction observed during preparation.
    pub direction: PreparedRestoreDirection,
    /// Opaque source identity recorded by the owner.
    pub source_id: String,
    /// Stored source-root locator and identity observation.
    pub source_root: RecordedRootLocator,
    /// Stored target-parent locator and identity observation.
    pub target_parent: RecordedRootLocator,
    /// Stored destination-local final leaf.
    pub final_leaf: PathBuf,
    /// Stored destination-local staging locator.
    pub staging: PreparedStagingLocator,
    /// Stored absence observation for the final name.
    pub final_observation: AbsentFinalObservation,
    /// Stored content evidence for the copy-validated participant.
    pub copy_validated_evidence: PreparedFileEvidence,
}

/// Stored observation made after an absent-final staged entry became a final entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AbsentFinalRecoveryObservation {
    /// Stable identity observed for the target parent.
    pub target_parent_stable_id: String,
    /// Stable identity observed for the final entry.
    pub final_stable_id: String,
    /// Length observed for the final entry.
    pub final_len: u64,
    /// Content evidence observed for the final entry.
    pub final_content: PreparedFileEvidence,
}

/// Stored proof that an absent-final observation was reopened and verified.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AbsentFinalTransactionOwnedProof {
    /// Stable identity retained for the target parent.
    pub target_parent_stable_id: String,
    /// Stable identity retained for the final entry.
    pub final_stable_id: String,
    /// Length retained for the final entry.
    pub final_len: u64,
    /// Content evidence retained for the final entry.
    pub final_content: PreparedFileEvidence,
}

/// Stored evidence that an absent-final entry was qualified for adoption.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AbsentFinalAdoptionEvidence {
    /// Stable identity retained for the target parent.
    pub target_parent_stable_id: String,
    /// Stable identity retained for the final entry.
    pub final_stable_id: String,
    /// Length retained for the final entry.
    pub final_len: u64,
    /// Content evidence retained for the final entry.
    pub final_content: PreparedFileEvidence,
}

/// Tagged stored preparation contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreparedTargetContract {
    /// Existing-target preparation evidence.
    ExistingExpectedIdentity(PreparedWaveformRestore),
    /// Schema-v2 absent-final preparation evidence.
    AbsentFinalNoReplace(PreparedAbsentFinalNoReplace),
}

impl PreparedTargetContract {
    /// Borrow the existing-target evidence when this contract contains it.
    pub fn as_existing(&self) -> Option<&PreparedWaveformRestore> {
        match self {
            Self::ExistingExpectedIdentity(prepared) => Some(prepared),
            Self::AbsentFinalNoReplace(_) => None,
        }
    }

    /// Borrow mutable existing-target evidence when this contract contains it.
    pub fn as_existing_mut(&mut self) -> Option<&mut PreparedWaveformRestore> {
        match self {
            Self::ExistingExpectedIdentity(prepared) => Some(prepared),
            Self::AbsentFinalNoReplace(_) => None,
        }
    }
}

/// Stored staging participant observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum FilesystemStagedParticipant {
    /// Staging bytes were observed copied and validated by an owner.
    CopyValidated {
        /// Stored staging locator and identity.
        staging: PreparedLeafLocator,
        /// Stored content or metadata evidence for the staged bytes.
        evidence: PreparedFileEvidence,
    },
}

/// Stored evidence for a validated filesystem staging participant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FilesystemStagedWaveformRestore {
    /// Stored staging participant.
    pub participant: FilesystemStagedParticipant,
}

/// Transfer mode retained in stored publication evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum FilesystemPublicationMode {
    /// Bytewise transfer into destination-local staging.
    NonAtomicCopyValidatePublish,
    /// Reserved for a future all-atomic transfer sequence.
    AtomicDestinationNoReplace,
}

/// Whole-publication atomicity value retained as untrusted evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum WholePublicationAtomicity {
    /// The owner reported whole-publication atomicity.
    WholePublicationAtomic,
    /// The owner reported a non-atomic publication sequence.
    WholePublicationNonAtomic,
}

/// Visibility value retained independently from synchronization.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum PublicationVisibility {
    /// The owner reported verified visibility.
    VisibilityVerified,
    /// Visibility was not verified.
    VisibilityUnverified,
}

/// Synchronization value retained independently from visibility and atomicity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum PublicationSynchronization {
    /// The owner reported power-loss synchronization.
    PowerLossSynchronized,
    /// The owner reported best-effort synchronization.
    BestEffortSync,
    /// Synchronization was unsupported or unverified.
    SyncUnsupportedOrUnverified,
}

/// Primitive qualification value retained in publication data.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum FinalClaimPrimitive {
    /// Expected-identity replacement was qualified by an owner.
    QualifiedExpectedIdentityReplacement,
    /// Absent-final installation was qualified by an owner.
    QualifiedAbsentFinalNoReplace,
    /// The owner retained an unqualified result and reason.
    Unqualified { reason: String },
}

/// Final-name primitive result retained in publication data.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum FinalClaimResult {
    /// An expected target was replaced.
    ExpectedIdentityReplaced,
    /// An absent final name was installed.
    AbsentFinalInstalled,
    /// A final claim was not established.
    NotEstablished,
}

/// Capability scope retained in absent-final publication data.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum AbsentFinalCapabilityScope {
    /// Target-parent identity and explicit pathname-continuity value.
    TargetParentDescriptor {
        /// Target-parent identity retained by the owner.
        target_parent_identity: PreparedObjectIdentity,
        /// Root pathname continuity value retained by the owner.
        root_path_continuity: RootPathContinuity,
    },
}

/// Final namespace claim retained as untrusted publication data.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum FinalNamespaceClaim {
    /// Expected-identity replacement claim.
    ExpectedIdentityReplacement {
        /// Primitive qualification value.
        primitive: FinalClaimPrimitive,
        /// Primitive result value.
        result: FinalClaimResult,
        /// Expected target identity.
        expected_target: PreparedObjectIdentity,
        /// Displaced target identity.
        displaced_target: PreparedObjectIdentity,
    },
    /// Absent-final no-replace claim.
    AbsentFinalNoReplace {
        /// Primitive qualification value.
        primitive: FinalClaimPrimitive,
        /// Primitive result value.
        result: FinalClaimResult,
        /// Stored owner scope.
        capability_scope: AbsentFinalCapabilityScope,
    },
}

/// Identity and content evidence retained after reopening a final object.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ReopenedFinalEvidence {
    /// Reopened final identity.
    identity: PreparedObjectIdentity,
    /// Reopened final content evidence.
    content: PreparedFileEvidence,
}

/// Stored publication evidence value.
///
/// This type contains only serialized observations. It has no constructor from a qualified
/// result and no filesystem handle; decoding it does not authorize publication or recovery.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FilesystemPublishedWaveformRestore {
    pub(crate) mode: FilesystemPublicationMode,
    pub(crate) final_claim: FinalNamespaceClaim,
    pub(crate) reopened_final: ReopenedFinalEvidence,
    pub(crate) visibility: PublicationVisibility,
    pub(crate) whole_publication: WholePublicationAtomicity,
    pub(crate) synchronization: PublicationSynchronization,
}

/// Stable platform family retained in a replacement assessment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplacementPlatformFamily {
    /// Apple macOS.
    Macos,
    /// Microsoft Windows.
    Windows,
    /// Linux.
    Linux,
    /// Other platform.
    Other,
}

/// Filesystem relationship retained before a replacement assessment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedFilesystemClassification {
    /// Source and destination were observed on one volume.
    SameVolume,
    /// Source and destination were observed on different volumes.
    DifferentVolume,
    /// The relationship was unavailable.
    Unavailable,
}

/// Candidate primitive name retained in a replacement assessment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplacementCandidatePrimitive {
    /// macOS `renameatx_np` exclusive no-replace candidate.
    MacosRenameAtxNpRenameExcl,
    /// macOS `renameatx_np` swap candidate.
    MacosRenameAtxNpRenameSwap,
    /// No public candidate was available.
    NoPublicCandidate,
}

/// Candidate assessment retained without invoking a primitive.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplacementCandidateAssessment {
    /// Candidate supports absent-final installation only.
    AbsentFinalOnly,
    /// Candidate lacks an expected-target identity operand.
    SwapWithoutExpectedTargetIdentityOperand,
    /// No candidate is qualified.
    NoQualifiedCandidate,
}

/// Missing invariant retained for an unsupported replacement assessment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplacementMissingInvariant {
    /// Atomic comparison against the expected target identity is unavailable.
    AtomicExpectedTargetIdentityComparison,
}

/// Decision retained for an unsupported replacement assessment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplacementQualificationDecision {
    /// Platform qualification is required before retry.
    PlatformQualificationRequired,
}

/// Retry condition retained for an unsupported replacement assessment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplacementQualificationRetryCondition {
    /// A platform build or policy change is required.
    PlatformBuildOrQualificationPolicyChange,
}

/// Opaque volume identity retained with replacement assessment evidence.
///
/// This is a stored observation and is intentionally separate from the live capacity-planning
/// type until a journal record needs to connect the two models.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VolumeIdentity {
    /// Platform volume/device identity observed by the owner.
    pub device: u64,
}

/// Bounded evidence explaining an unsupported replacement assessment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReplacementQualificationAssessment {
    /// Platform family observed by the owner.
    pub platform_family: ReplacementPlatformFamily,
    /// Source/destination filesystem relationship observed by the owner.
    pub observed_filesystem: ObservedFilesystemClassification,
    /// Volume identity observed by the owner.
    pub volume: VolumeIdentity,
    /// Candidate primitive considered by the owner.
    pub candidate: ReplacementCandidatePrimitive,
    /// Candidate assessment retained by the owner.
    pub candidate_assessment: ReplacementCandidateAssessment,
    /// Missing invariant retained by the owner.
    pub missing_invariant: ReplacementMissingInvariant,
    /// Durable decision retained by the owner.
    pub decision: ReplacementQualificationDecision,
    /// Retry condition retained by the owner.
    pub retry_condition: ReplacementQualificationRetryCondition,
}

/// Explicit value for root pathname continuity in stored absent-final evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum RootPathContinuity {
    /// No continuity claim is retained.
    NotClaimed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const EXISTING_PREPARED: &str = r#"
{
  "direction": "Undo",
  "source_id": "source-7",
  "source_root": {
    "path": "/Volumes/Source",
    "identity": {"stable_id": "root-source", "change_marker": "m1", "len": 4096}
  },
  "target_root": {
    "path": "/Volumes/Target",
    "identity": {"stable_id": "root-target", "change_marker": null, "len": 4096}
  },
  "target": {
    "relative_path": "target.wav",
    "identity": {"stable_id": "target-1", "change_marker": "t1", "len": 12}
  },
  "backup_root": {
    "path": "/Volumes/Backup",
    "identity": {"stable_id": "root-backup", "change_marker": null, "len": 4096}
  },
  "backup": {
    "relative_path": "backup.wav",
    "identity": {"stable_id": "backup-1", "change_marker": "b1", "len": 12}
  },
  "replacement": {"Existing": {"stable_id": "target-1", "change_marker": "t1", "len": 12}},
  "staging": {"relative_path": "staging.wav", "absent": true},
  "evidence": {
    "target": {"Metadata": {"len": 12, "modified_ns": 10, "is_dir": false}},
    "backup": {"ContentHash": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31]}
  }
}
"#;

    const ABSENT_PREPARED: &str = r#"
{
  "direction": "Redo",
  "source_id": "source-8",
  "source_root": {
    "path": "/Volumes/Source",
    "identity": {"stable_id": "root-source", "change_marker": null, "len": 4096}
  },
  "target_parent": {
    "path": "/Volumes/Target",
    "identity": {"stable_id": "root-target", "change_marker": null, "len": 4096}
  },
  "final_leaf": "new.wav",
  "staging": {"relative_path": "staging.wav", "absent": true},
  "final_observation": "ObservedAbsent",
  "copy_validated_evidence": {"ContentHash": [31, 30, 29, 28, 27, 26, 25, 24, 23, 22, 21, 20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0]}
}
"#;

    const STORED_PUBLICATION: &str = r#"
{
  "mode": "NonAtomicCopyValidatePublish",
  "final_claim": {
    "ExpectedIdentityReplacement": {
      "primitive": "QualifiedExpectedIdentityReplacement",
      "result": "ExpectedIdentityReplaced",
      "expected_target": {"stable_id": "target-1", "change_marker": "t1", "len": 12},
      "displaced_target": {"stable_id": "target-1", "change_marker": "t1", "len": 12}
    }
  },
  "reopened_final": {
    "identity": {"stable_id": "target-1", "change_marker": "t2", "len": 12},
    "content": {"ContentHash": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31]}
  },
  "visibility": "VisibilityVerified",
  "whole_publication": "WholePublicationNonAtomic",
  "synchronization": "BestEffortSync"
}
"#;

    fn round_trip_value<T>(fixture: &str) -> Value
    where
        T: for<'de> Deserialize<'de> + Serialize,
    {
        let parsed: T = serde_json::from_str(fixture).expect("fixed evidence fixture decodes");
        let encoded = serde_json::to_value(parsed).expect("evidence fixture re-encodes");
        let original: Value = serde_json::from_str(fixture).expect("fixture is valid JSON");
        assert_eq!(encoded, original);
        encoded
    }

    #[test]
    fn existing_prepared_fixture_preserves_wire_fields_after_root_rename() {
        let value = round_trip_value::<PreparedWaveformRestore>(EXISTING_PREPARED);
        assert_eq!(value["source_root"]["path"], "/Volumes/Source");
        let prepared: PreparedWaveformRestore =
            serde_json::from_str(EXISTING_PREPARED).expect("existing fixture decodes");
        assert_eq!(prepared.source_root.identity.stable_id, "root-source");
        assert_eq!(prepared.target.relative_path, PathBuf::from("target.wav"));
    }

    #[test]
    fn absent_prepared_fixture_preserves_schema_v2_wire_fields() {
        let value = round_trip_value::<PreparedAbsentFinalNoReplace>(ABSENT_PREPARED);
        assert_eq!(value["final_observation"], "ObservedAbsent");
        let prepared: PreparedAbsentFinalNoReplace =
            serde_json::from_str(ABSENT_PREPARED).expect("absent fixture decodes");
        assert_eq!(prepared.final_leaf, PathBuf::from("new.wav"));
        assert!(prepared.staging.absent);
    }

    #[test]
    fn stored_publication_fixture_is_data_only_and_wire_stable() {
        let value = round_trip_value::<FilesystemPublishedWaveformRestore>(STORED_PUBLICATION);
        assert_eq!(value["mode"], "NonAtomicCopyValidatePublish");
        assert_eq!(value["visibility"], "VisibilityVerified");
        let publication: FilesystemPublishedWaveformRestore =
            serde_json::from_str(STORED_PUBLICATION).expect("publication fixture decodes");
        assert!(matches!(
            publication.final_claim,
            FinalNamespaceClaim::ExpectedIdentityReplacement { .. }
        ));
    }

    #[test]
    fn unknown_stored_enum_values_are_rejected() {
        let unknown_direction = EXISTING_PREPARED.replace("\"Undo\"", "\"Sideways\"");
        assert!(serde_json::from_str::<PreparedWaveformRestore>(&unknown_direction).is_err());

        let unknown_publication =
            STORED_PUBLICATION.replace("\"BestEffortSync\"", "\"UnverifiedSync\"");
        assert!(
            serde_json::from_str::<FilesystemPublishedWaveformRestore>(&unknown_publication)
                .is_err()
        );
    }
}
