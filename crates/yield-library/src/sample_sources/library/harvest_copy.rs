//! One retained-owner copy metadata effect; never physical or journal completion authority.
use super::{LibraryDatabase, LibraryError, ReferenceSnapshot, SourceRegistryParticipantReceipt};
use crate::app_dirs::WritableProfileGuard;
use crate::operation_journal::work_plan::WorkUnitKey;
use crate::operation_journal::{FrozenCopyObservation, OperationId, StoredRelativePath};
use crate::sample_sources::db::{ManifestEnrollmentReceipt, SourceContentIdentityObservation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod historical;
pub use historical::{HarvestCopyReceiptKey, HarvestCopyReceiptObservation};
mod ledger;
mod mutation;
mod payload;
#[cfg(test)]
mod tests;
mod transaction;
mod validation;
use payload::Payload;

/// Maximum immutable copy metadata outcomes retained without implicit eviction.
pub const HARVEST_COPY_MAX_RECEIPTS: i64 = 8192;
/// Maximum all-field ledger bytes, including scalar overhead.
pub const HARVEST_COPY_MAX_BYTES: i64 = 16 * 1024 * 1024;
pub(super) const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS harvest_copy_receipts (
 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
 command_key TEXT NOT NULL UNIQUE CHECK(length(CAST(command_key AS BLOB)) BETWEEN 1 AND 512),
 payload BLOB NOT NULL CHECK(length(payload) BETWEEN 1 AND 65536),
 result BLOB NOT NULL CHECK(length(result) BETWEEN 1 AND 65536),
 identity BLOB NOT NULL CHECK(length(identity)=32),
 owner TEXT NOT NULL CHECK(length(owner)=36))";

/// Exact proposal custody plus fresh privately issued owner observations.
/// No decoder creates source, enrollment or journal observations.
#[derive(Clone, Debug)]
pub struct HarvestCopyRequest {
    owner: Uuid,
    payload: Payload,
    bytes: Vec<u8>,
    family: FrozenCopyObservation,
    parent: SourceContentIdentityObservation,
    child: SourceContentIdentityObservation,
    enrollment: ManifestEnrollmentReceipt,
    registry: SourceRegistryParticipantReceipt,
}
impl HarvestCopyRequest {
    /// Bind one copy family, exact source facts and actual metadata/global prerequisites.
    /// Metadata/tool strings are bounded user-intent data, never ownership evidence.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        owner: &WritableProfileGuard,
        family: FrozenCopyObservation,
        parent: SourceContentIdentityObservation,
        child: SourceContentIdentityObservation,
        enrollment: ManifestEnrollmentReceipt,
        registry: SourceRegistryParticipantReceipt,
        snapshot: ReferenceSnapshot,
        metadata: super::HarvestMetadataSnapshot,
        tool: String,
    ) -> Result<Self, LibraryError> {
        owner.validate_current()?;
        let payload = Payload::new(
            owner,
            &family,
            &parent,
            &child,
            &enrollment,
            &registry,
            snapshot,
            metadata,
            tool,
        )?;
        let bytes = payload.encode()?;
        let request = Self {
            owner: Uuid::parse_str(owner.owner_identity()).map_err(|_| invalid())?,
            payload,
            bytes,
            family,
            parent,
            child,
            enrollment,
            registry,
        };
        request.validate_live(true)?;
        Ok(request)
    }
    /// Restore exact historical proposal with current issuing owners for the same storage.
    /// Later file state may differ on receipt replay; a new effect requires exact old facts.
    #[allow(clippy::too_many_arguments)]
    pub fn restore(
        owner: &WritableProfileGuard,
        bytes: &[u8],
        family: FrozenCopyObservation,
        parent: SourceContentIdentityObservation,
        child: SourceContentIdentityObservation,
        enrollment: ManifestEnrollmentReceipt,
        registry: SourceRegistryParticipantReceipt,
    ) -> Result<Self, LibraryError> {
        owner.validate_current()?;
        let payload = Payload::decode(bytes)?;
        if payload.profile != owner.profile_root_identity() {
            return Err(invalid());
        }
        let request = Self {
            owner: Uuid::parse_str(owner.owner_identity()).map_err(|_| invalid())?,
            payload,
            bytes: bytes.to_vec(),
            family,
            parent,
            child,
            enrollment,
            registry,
        };
        request.validate_live(false)?;
        Ok(request)
    }
    /// Persist these immutable bytes before dispatch; restoring requires genuine current owners.
    pub fn proposal_bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Original stable operation.
    pub fn operation(&self) -> OperationId {
        self.payload.operation
    }
    /// Exactly one original Harvest obligation.
    pub fn work_key(&self) -> WorkUnitKey {
        self.payload.work
    }
}
/// Immutable metadata disposition. Rejections cannot advance required journal checkpoints.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HarvestCopyDisposition {
    /// One copy edge and its bounded file metadata committed.
    Applied,
    /// The bounded graph precondition changed before a new effect.
    GraphChanged,
    /// An existing file row identifies other or unverified content.
    IdentityConflict,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Outcome {
    version: u8,
    disposition: HarvestCopyDisposition,
    edge: Option<u64>,
    created: i64,
}
/// Privately issued immutable global metadata result, never VerifiedStepCompletion.
#[derive(Clone, Debug)]
pub struct HarvestCopyReceipt {
    payload: Payload,
    outcome: Outcome,
    sequence: u64,
    owner: Uuid,
}
impl HarvestCopyReceipt {
    /// Original operation.
    pub fn operation(&self) -> OperationId {
        self.payload.operation
    }
    /// Original required Harvest key.
    pub fn work_key(&self) -> WorkUnitKey {
        self.payload.work
    }
    /// Original metadata disposition.
    pub fn disposition(&self) -> HarvestCopyDisposition {
        self.outcome.disposition
    }
    /// Original edge identity, only for Applied.
    pub fn edge_id(&self) -> Option<u64> {
        self.outcome.edge
    }
    /// Original immutable ledger sequence.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    /// Historical profile acquisition, separate from current replay ownership.
    pub fn owner_identity(&self) -> Uuid {
        self.owner
    }
    /// Source enrollment revision observed for this historical copy.
    pub fn destination_revision(&self) -> u64 {
        self.payload.enrolled_revision
    }
}
/// Exact replay retains the original outcome and performs no graph/source mutation.
#[derive(Clone, Debug)]
pub enum HarvestCopyResult {
    /// New immutable metadata outcome committed.
    Committed(HarvestCopyReceipt),
    /// Original immutable metadata outcome reread.
    Replayed(HarvestCopyReceipt),
}
impl HarvestCopyResult {
    /// Borrow the original immutable outcome.
    pub fn receipt(&self) -> &HarvestCopyReceipt {
        match self {
            Self::Committed(r) | Self::Replayed(r) => r,
        }
    }
}
fn invalid() -> LibraryError {
    LibraryError::HarvestCopyInput
}
fn bounded<T: Serialize>(value: &T) -> Result<Vec<u8>, LibraryError> {
    let mut writer = crate::operation_journal::codec::CappedWriter::new(65536);
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.finish())
}
