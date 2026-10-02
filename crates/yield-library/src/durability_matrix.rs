//! Bounded, non-authoritative durability qualification records (OPT-1843).
//!
//! Assessment checks evidence categories, never evidence authenticity. Even an observed,
//! complete row supplies no live owner grant and cannot advance an operation checkpoint.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum diagnostic/reference bytes in a matrix evidence record.
pub const MAX_EVIDENCE_REFERENCE_BYTES: usize = 256;

/// Platform whose exact primitive sequence a row describes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Platform {
    /// macOS qualification lane.
    MacOs,
    /// Windows qualification lane.
    Windows,
    /// Linux qualification lane; no support inferred from POSIX APIs.
    Linux,
}

/// Destination medium class; qualification never transfers between classes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum VolumeClass {
    /// Fixed local storage with a named filesystem/device qualification.
    LocalFixed,
    /// Removable storage with separately qualified device and disconnect behavior.
    Removable,
    /// Remote storage with separately qualified protocol/server behavior.
    Remote,
}

/// Source of a record; synthetic fixtures cannot qualify a production combination.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Provenance {
    /// Model input used solely to test the schema.
    SyntheticFixture,
    /// Recorded qualification observation, still untrusted until independently reviewed.
    Observed,
}

/// Relationship established by live source, staging and final volume observations.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum VolumeLayout {
    /// Source, staging and final are on one proven volume.
    AllOneVolume,
    /// Source differs; staging and final share the destination volume.
    DestinationLocalStaging,
    /// Staging/final co-location is missing, changed or unverified.
    Unverified,
}

/// Actual input-to-staging transfer, independent of the final namespace claim.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Transfer {
    /// Every applicable transfer uses a separately qualified atomic sequence.
    QualifiedAtomic,
    /// Bytewise transfer, including on the same volume.
    Bytewise,
    /// Transfer qualification is missing.
    Unqualified,
}

/// Independent evidence families; publication never implies cleanup or database recovery.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum EvidenceKind {
    /// No-follow root, descriptor-bound staged/final identity and transaction provenance.
    BoundObjectIdentity,
    /// Matching live profile/source, lifecycle, lease and capacity owner fences.
    OwnerFences,
    /// Durable staging validation before final-name claim.
    CopyValidatedCheckpoint,
    /// Qualified object-bound atomic absent-final claim, with no collision/replacement.
    NoReplaceFinalClaim,
    /// Final reopened through the bound root and identity/content verified.
    ReopenedVisibility,
    /// Complete qualified source-to-staging and final-claim atomic sequence.
    WholeAtomicSequence,
    /// Platform-specific staged/final file synchronization at the required positions.
    FileSynchronization,
    /// Platform-specific destination namespace synchronization at required positions.
    NamespaceSynchronization,
    /// Exact device/filesystem or protocol/server power-loss behavior qualification.
    MediaPowerLoss,
    /// Independently qualified identity-bound cleanup with verified absence.
    IndependentCleanup,
    /// Independent rollback/retention and durable capacity-release evidence.
    Rollback,
    /// SQLite WAL/SHM, checkpoint, busy/external-reader and restart evidence.
    WalRecovery,
    /// Journal/participant crash replay and retained uncertainty evidence.
    CrashRecovery,
}

/// Evidence result, with unsupported and failed results distinct from missing data.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum EvidenceStatus {
    /// Recorded successful qualification observation, not live authority.
    Verified,
    /// Completed sync attempt with an explicit weaker guarantee.
    BestEffort,
    /// Primitive or combination is unavailable.
    Unsupported,
    /// No qualification observation is available.
    Missing,
    /// Qualification observation failed.
    Failed,
}

/// Bounded reference to an observation or diagnostic, never a filesystem locator.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRecord {
    /// Independent category described by this observation.
    pub kind: EvidenceKind,
    /// Result of the observation.
    pub status: EvidenceStatus,
    /// Opaque report identifier, including failures/downgrades; not proof by itself.
    pub reference: String,
}

/// One exact platform/medium/transfer assessment input, with bounded evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationRow {
    /// Bounded report ID for the exact build, filesystem, device/protocol and test scope.
    pub scope_reference: String,
    /// Platform scope of all observations.
    pub platform: Platform,
    /// Medium scope of all observations.
    pub volume: VolumeClass,
    /// Synthetic versus observed evidence origin.
    pub provenance: Provenance,
    /// Observed source/staging/final relationship.
    pub layout: VolumeLayout,
    /// Observed input-to-staging mechanism.
    pub transfer: Transfer,
    /// At most one record for each of the thirteen evidence categories.
    pub evidence: Vec<EvidenceRecord>,
}

/// Schema completion state; none of these values authorizes an effect or checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssessmentDisposition {
    /// Publication proof is incomplete; retain uncertainty if an effect already occurred.
    PublicationBlocked,
    /// Publication observations exist, but cleanup/rollback/database/replay evidence is absent.
    RecoveryUnqualified,
    /// Synthetic row has the necessary shape, solely for contract fixtures.
    FixtureComplete,
    /// Observed row has the necessary shape for review, not production qualification.
    ReadyForEvidenceReview,
}

/// Synchronization vocabulary matching the existing journal publication observations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SynchronizationClaim {
    /// Qualified sequence and exact medium observations are all present.
    PowerLossSynchronized,
    /// Only weaker synchronization observations are available.
    BestEffortSync,
    /// No usable synchronization observation is available.
    SyncUnsupportedOrUnverified,
}

/// Independent schema claims, not journal receipts or production capabilities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Assessment {
    /// Shape completion classification with provenance preserved.
    pub disposition: AssessmentDisposition,
    /// Whether bound reopened identity/content visibility evidence is present.
    pub visibility_verified: bool,
    /// Whether a qualified final-name claim and visibility evidence are present.
    pub atomic_namespace: bool,
    /// Whether all applicable transfers plus final claim have atomic evidence.
    pub whole_publication_atomic: bool,
    /// Independently evaluated synchronization category.
    pub synchronization: SynchronizationClaim,
    /// Every category without a verified observation; downgrade reasons remain visible.
    pub unqualified: Vec<EvidenceKind>,
}

/// Shape errors reject malformed matrix data before assessment.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum MatrixError {
    /// Missing, oversized or control-containing combination report reference.
    #[error("invalid qualification scope reference")]
    InvalidScope,
    /// More evidence records than distinct categories.
    #[error("too many durability evidence records")]
    TooManyRecords,
    /// Two records claim the same category.
    #[error("duplicate durability evidence category: {0:?}")]
    DuplicateCategory(EvidenceKind),
    /// Empty, oversized or control-containing reference.
    #[error("invalid durability evidence reference: {0:?}")]
    InvalidReference(EvidenceKind),
    /// Best-effort is a sync downgrade, not partial identity/publication qualification.
    #[error("best-effort is only valid for synchronization: {0:?}")]
    InvalidBestEffort(EvidenceKind),
}

const ALL_EVIDENCE: [EvidenceKind; 13] = [
    EvidenceKind::BoundObjectIdentity,
    EvidenceKind::OwnerFences,
    EvidenceKind::CopyValidatedCheckpoint,
    EvidenceKind::NoReplaceFinalClaim,
    EvidenceKind::ReopenedVisibility,
    EvidenceKind::WholeAtomicSequence,
    EvidenceKind::FileSynchronization,
    EvidenceKind::NamespaceSynchronization,
    EvidenceKind::MediaPowerLoss,
    EvidenceKind::IndependentCleanup,
    EvidenceKind::Rollback,
    EvidenceKind::WalRecovery,
    EvidenceKind::CrashRecovery,
];

impl QualificationRow {
    /// Validate bounded stored shape and classify independent evidence requirements.
    ///
    /// A complete row is still untrusted stored data. This method does not inspect media,
    /// acquire ownership, verify report authenticity or return runtime publication authority.
    pub fn assess(&self) -> Result<Assessment, MatrixError> {
        if !valid_reference(&self.scope_reference) {
            return Err(MatrixError::InvalidScope);
        }
        if self.evidence.len() > ALL_EVIDENCE.len() {
            return Err(MatrixError::TooManyRecords);
        }
        for (index, record) in self.evidence.iter().enumerate() {
            if self.evidence[..index]
                .iter()
                .any(|old| old.kind == record.kind)
            {
                return Err(MatrixError::DuplicateCategory(record.kind));
            }
            if !valid_reference(&record.reference) {
                return Err(MatrixError::InvalidReference(record.kind));
            }
            if record.status == EvidenceStatus::BestEffort
                && !matches!(
                    record.kind,
                    EvidenceKind::FileSynchronization | EvidenceKind::NamespaceSynchronization
                )
            {
                return Err(MatrixError::InvalidBestEffort(record.kind));
            }
        }
        let verified = |kind| self.status(kind) == EvidenceStatus::Verified;
        let visibility = [
            EvidenceKind::BoundObjectIdentity,
            EvidenceKind::OwnerFences,
            EvidenceKind::ReopenedVisibility,
        ]
        .into_iter()
        .all(verified);
        let publication = visibility
            && self.layout != VolumeLayout::Unverified
            && [
                EvidenceKind::CopyValidatedCheckpoint,
                EvidenceKind::NoReplaceFinalClaim,
            ]
            .into_iter()
            .all(verified);
        let recovery = [
            EvidenceKind::IndependentCleanup,
            EvidenceKind::Rollback,
            EvidenceKind::WalRecovery,
            EvidenceKind::CrashRecovery,
        ]
        .into_iter()
        .all(verified);
        let whole_publication_atomic = publication
            && self.layout == VolumeLayout::AllOneVolume
            && self.transfer == Transfer::QualifiedAtomic
            && verified(EvidenceKind::WholeAtomicSequence);
        let synchronization = if publication
            && recovery
            && [
                EvidenceKind::FileSynchronization,
                EvidenceKind::NamespaceSynchronization,
                EvidenceKind::MediaPowerLoss,
            ]
            .into_iter()
            .all(verified)
        {
            SynchronizationClaim::PowerLossSynchronized
        } else if publication
            && [
                EvidenceKind::FileSynchronization,
                EvidenceKind::NamespaceSynchronization,
            ]
            .into_iter()
            .any(|kind| {
                matches!(
                    self.status(kind),
                    EvidenceStatus::Verified | EvidenceStatus::BestEffort
                )
            })
        {
            SynchronizationClaim::BestEffortSync
        } else {
            SynchronizationClaim::SyncUnsupportedOrUnverified
        };
        Ok(Assessment {
            disposition: if !publication {
                AssessmentDisposition::PublicationBlocked
            } else if !recovery {
                AssessmentDisposition::RecoveryUnqualified
            } else if self.provenance == Provenance::SyntheticFixture {
                AssessmentDisposition::FixtureComplete
            } else {
                AssessmentDisposition::ReadyForEvidenceReview
            },
            visibility_verified: visibility,
            atomic_namespace: publication,
            whole_publication_atomic,
            synchronization,
            unqualified: ALL_EVIDENCE
                .into_iter()
                .filter(|kind| !verified(*kind))
                .collect(),
        })
    }

    fn status(&self, kind: EvidenceKind) -> EvidenceStatus {
        self.evidence
            .iter()
            .find(|record| record.kind == kind)
            .map_or(EvidenceStatus::Missing, |record| record.status)
    }
}

fn valid_reference(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_EVIDENCE_REFERENCE_BYTES
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests;
