//! Operation-scoped exact source metadata enrollment; no physical/journal completion grant.
use super::{SourceScanFence, SourceWriter, SourceWriterError};
use crate::operation_journal::{
    OperationId, StoredRelativePath,
    work_plan::{WorkUnitKey, WorkUnitRole},
};
use crate::sample_sources::db::{
    ManifestCommitResult, SourceDbError, SourceIndexClassification, SourceIndexDiagnostic,
    SourcePublicationFence, SourcePublicationFenceError, SourceWriteBatch,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};
use thiserror::Error;
mod commit;
mod ledger;
#[cfg(test)]
mod tests;

/// Exact prepared metadata classification. It makes no publication/absence assertion.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ManifestEnrollmentKind {
    /// Fully hashed supported audio metadata.
    Audio,
    /// Complete index-only regular-file metadata.
    Index {
        /// Source classification.
        classification: SourceIndexClassification,
        /// Bounded scanner diagnostic.
        diagnostic: Option<SourceIndexDiagnostic>,
        /// Classification policy version.
        policy: u32,
    },
}
/// Bounded command data. Constructing this DTO grants no source authority or receipt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEnrollmentEntry {
    /// Exact UTF-8 source-relative destination.
    pub path: StoredRelativePath,
    /// Actual prepared classification.
    pub kind: ManifestEnrollmentKind,
    /// Observed size.
    pub size: u64,
    /// Observed modification timestamp.
    pub modified_ns: i64,
    /// Held-file identity from scanner preparation.
    pub file_identity: String,
    /// Full prepared hash for audio; absent for index-only entries.
    pub content_hash: Option<String>,
}
impl ManifestEnrollmentEntry {
    fn valid(&self) -> bool {
        self.path.validate().is_ok()
            && self.path.as_str().len() <= 1024
            && self.path.as_str().split('/').count() <= 32
            && self.size <= i64::MAX as u64
            && text(&self.file_identity, 512)
            && match self.kind {
                ManifestEnrollmentKind::Audio => self.content_hash.as_ref().is_some_and(|h| {
                    h.len() == 64
                        && h.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                }),
                ManifestEnrollmentKind::Index { classification, .. } => {
                    classification != SourceIndexClassification::Inaccessible
                        && self.content_hash.is_none()
                }
            }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    source: String,
    root: Vec<u8>,
    database: String,
    binding: String,
    lifecycle: u64,
    lease: u64,
    revision: u64,
}
impl Scope {
    fn from_fence(f: &SourceScanFence) -> Self {
        Self {
            source: f.source_id.as_str().into(),
            root: f.root_identity.as_bytes().to_vec(),
            database: f.database_identity.clone(),
            binding: f.root_binding_identity.clone(),
            lifecycle: f.lifecycle_generation,
            lease: f.lease_epoch,
            revision: f.revision,
        }
    }
    fn valid(&self) -> bool {
        text(&self.source, 256)
            && text(&self.database, 256)
            && text(&self.binding, 256)
            && !self.root.is_empty()
            && self.root.len() <= 4096
            && self.lease > 0
            && [self.lifecycle, self.lease, self.revision]
                .iter()
                .all(|n| *n <= i64::MAX as u64)
    }
    fn same_storage(&self, other: &Self) -> bool {
        self.source == other.source
            && self.root == other.root
            && self.database == other.database
            && self.binding == other.binding
    }
}
fn text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.contains('\0')
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    version: u8,
    operation: OperationId,
    work: WorkUnitKey,
    source: Scope,
    entries: Vec<ManifestEnrollmentEntry>,
}
impl Payload {
    fn validate(&self) -> Result<(), ManifestEnrollmentError> {
        if self.version != 1
            || self.operation.as_uuid().is_nil()
            || self.work.participant_id().as_uuid().is_nil()
            || !matches!(
                self.work.role(),
                WorkUnitRole::SourceAffect | WorkUnitRole::DestinationSourceReconcile
            )
            || !self.source.valid()
            || self.entries.is_empty()
            || self.entries.len() > 64
            || self.entries.iter().any(|e| !e.valid())
            || self
                .entries
                .windows(2)
                .any(|w| w[0].path.as_str() >= w[1].path.as_str())
            || self
                .entries
                .iter()
                .map(|e| e.path.as_str().len())
                .sum::<usize>()
                > 32768
        {
            return Err(ManifestEnrollmentError::Input);
        }
        Ok(())
    }
    fn key(&self) -> Result<String, ManifestEnrollmentError> {
        Ok(serde_json::to_string(&(self.operation, self.work))?)
    }
}
/// Canonical historical proposal plus an owner-issued current scan observation.
/// Persist `proposal_bytes` before dispatch; restoration is custody, never a receipt.
#[derive(Clone, Debug)]
pub struct ManifestEnrollmentRequest {
    payload: Payload,
    bytes: Vec<u8>,
    current: SourceScanFence,
    owner: Weak<()>,
}
impl ManifestEnrollmentRequest {
    /// Bind caller's frozen operation/work key to complete exact prepared metadata.
    /// The family adapter must verify its admitted plan; this metadata API cannot complete it.
    pub fn new(
        operation: OperationId,
        work: WorkUnitKey,
        owner: &SourceWriter,
        mut entries: Vec<ManifestEnrollmentEntry>,
    ) -> Result<Self, ManifestEnrollmentError> {
        let current = owner.current_scan_fence()?;
        entries.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
        let payload = Payload {
            version: 1,
            operation,
            work,
            source: Scope::from_fence(&current),
            entries,
        };
        payload.validate()?;
        let bytes = serde_json::to_vec(&payload)?;
        if bytes.len() > 65536 {
            return Err(ManifestEnrollmentError::Input);
        }
        Ok(Self {
            payload,
            bytes,
            current,
            owner: Arc::downgrade(&owner.reference_liveness),
        })
    }
    /// Preserve exact canonical input, including historical owner observations.
    pub fn proposal_bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Restore bounded canonical custody with a fresh live source context.
    /// Only an already persisted receipt may replay across generations/revisions.
    pub fn restore(bytes: &[u8], owner: &SourceWriter) -> Result<Self, ManifestEnrollmentError> {
        let current = owner.current_scan_fence()?;
        if bytes.is_empty() || bytes.len() > 65536 {
            return Err(ManifestEnrollmentError::Input);
        }
        let payload: Payload = serde_json::from_slice(bytes)?;
        payload.validate()?;
        if serde_json::to_vec(&payload)? != bytes
            || !payload.source.same_storage(&Scope::from_fence(&current))
        {
            return Err(ManifestEnrollmentError::Input);
        }
        Ok(Self {
            payload,
            bytes: bytes.to_vec(),
            current,
            owner: Arc::downgrade(&owner.reference_liveness),
        })
    }
    /// Exact prepared metadata bound by this command.
    pub fn entries(&self) -> &[ManifestEnrollmentEntry] {
        &self.payload.entries
    }
    /// Current owner observation required by the committing adapter.
    pub fn current_fence(&self) -> &SourceScanFence {
        &self.current
    }
}
/// Immutable owner-issued metadata result; no public constructor/decoder exists.
#[derive(Clone, Debug)]
pub struct ManifestEnrollmentReceipt {
    payload: Payload,
    bytes: Vec<u8>,
    result: ManifestCommitResult,
    publication: SourcePublicationFence,
    owner: Weak<()>,
}
impl ManifestEnrollmentReceipt {
    /// Historical stable operation identity.
    pub fn operation(&self) -> OperationId {
        self.payload.operation
    }
    /// Exact supplied frozen work obligation.
    pub fn work_key(&self) -> WorkUnitKey {
        self.payload.work
    }
    /// Actual source ID bound by the issuing owner and historical proposal.
    pub fn source_id(&self) -> &str {
        &self.payload.source.source
    }
    /// Historical source database identity, never an ambient path.
    pub fn database_identity(&self) -> &str {
        &self.payload.source.database
    }
    /// Historical root identity bytes.
    pub fn root_identity(&self) -> &[u8] {
        &self.payload.source.root
    }
    /// Historical prepared revision before this enrollment.
    pub fn prepared_revision(&self) -> u64 {
        self.payload.source.revision
    }
    /// Original committed generic revision, including on replay.
    pub fn committed_revision(&self) -> u64 {
        self.result.revision
    }
    /// Exact stored prepared metadata observations.
    pub fn entries(&self) -> &[ManifestEnrollmentEntry] {
        &self.payload.entries
    }
    /// Original bounded delta; no new source mutation is inferred on replay.
    pub fn manifest(&self) -> &ManifestCommitResult {
        &self.result
    }
    /// Canonical historical proposal for restart correlation, not completion authority.
    pub fn proposal_bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Revalidate retained issuing writer and physical source/profile/lease bindings.
    /// This immutable metadata observation does not promise current file contents/revision.
    pub fn validate(&self) -> Result<(), ManifestEnrollmentError> {
        let _owner = self
            .owner
            .upgrade()
            .ok_or(ManifestEnrollmentError::OwnerEnded)?;
        self.publication.validate()?;
        Ok(())
    }
}
/// Command result distinguishes a fresh delta from immutable acknowledgement replay.
#[derive(Clone, Debug)]
pub struct ManifestEnrollmentResult {
    /// Actual privately issued durable receipt.
    pub receipt: ManifestEnrollmentReceipt,
    /// True means no delta or revision advance was performed by this invocation.
    pub replayed: bool,
}
/// Fail-closed command/receipt failures. Uncertainty retains stable proposal custody.
#[derive(Debug, Error)]
pub enum ManifestEnrollmentError {
    /// Invalid/noncanonical/unbounded input or prepared metadata.
    #[error("invalid manifest enrollment input")]
    Input,
    /// Existing command identity names different exact input.
    #[error("manifest enrollment replay conflict")]
    Conflict,
    /// Persisted evidence is malformed, ambiguous or inconsistent.
    #[error("invalid manifest enrollment evidence")]
    Evidence,
    /// Finite immutable evidence retention is exhausted.
    #[error("manifest enrollment evidence capacity exhausted")]
    Capacity,
    /// Cancellation before commit.
    #[error("manifest enrollment canceled")]
    Canceled,
    /// Issuing writer has ended, including database handoff.
    #[error("manifest enrollment issuing writer ended")]
    OwnerEnded,
    /// Commit/reply cannot be confirmed; recover using the original proposal.
    #[error("manifest enrollment commit acknowledgement uncertain")]
    CommitUncertain,
    /// Actual source owner rejected the operation.
    #[error(transparent)]
    Owner(#[from] SourceWriterError),
    /// Source transaction failed.
    #[error(transparent)]
    Database(#[from] SourceDbError),
    /// SQL read/insert failed.
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    /// Bounded canonical codec failed.
    #[error(transparent)]
    Codec(#[from] serde_json::Error),
    /// Physical retained source ownership ended or changed.
    #[error(transparent)]
    Publication(#[from] SourcePublicationFenceError),
}
fn canceled(cancel: Option<&AtomicBool>) -> Result<(), ManifestEnrollmentError> {
    if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
        Err(ManifestEnrollmentError::Canceled)
    } else {
        Ok(())
    }
}
