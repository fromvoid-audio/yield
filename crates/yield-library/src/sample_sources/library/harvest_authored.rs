//! Explicit user workflow state through the actual retained owner, never saga completion.
use super::{HarvestState, LibraryDatabase, LibraryError};
use crate::app_dirs::WritableProfileGuard;
use crate::operation_journal::{OperationId, StoredRelativePath};
use crate::sample_sources::db::SourceContentIdentityObservation;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
mod ledger;
mod payload;
#[cfg(test)]
mod tests;
mod transaction;
use payload::{File, Payload};
/// Finite durable outcome count without automatic eviction.
pub const HARVEST_AUTHORED_MAX_RECEIPTS: i64 = 8192;
/// All stored ledger bytes including scalar overhead.
pub const HARVEST_AUTHORED_MAX_BYTES: i64 = 16 * 1024 * 1024;
pub(super) const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS harvest_authored_receipts (
 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
 command_key TEXT NOT NULL UNIQUE CHECK(length(CAST(command_key AS BLOB)) BETWEEN 1 AND 512),
 payload BLOB NOT NULL CHECK(length(payload) BETWEEN 1 AND 65536),
 result BLOB NOT NULL CHECK(length(result) BETWEEN 1 AND 65536),
 identity BLOB NOT NULL CHECK(length(identity)=32),
 owner TEXT NOT NULL CHECK(length(owner)=36))";
/// Actual source-backed bounded read request. UUID correlation is checked by the retained owner.
#[derive(Clone, Debug)]
pub struct HarvestAuthoredSnapshotRequest {
    owner: Uuid,
    source: SourceContentIdentityObservation,
}
impl HarvestAuthoredSnapshotRequest {
    /// Bind one current content observation to the current profile acquisition.
    pub fn new(
        owner: &WritableProfileGuard,
        source: SourceContentIdentityObservation,
    ) -> Result<Self, LibraryError> {
        owner.validate_current()?;
        source.validate().map_err(|_| invalid())?;
        File::from_observation(&source)?;
        Ok(Self {
            owner: Uuid::parse_str(owner.owner_identity()).map_err(|_| invalid())?,
            source,
        })
    }
    pub(super) fn validate_sources(&self) -> Result<(), LibraryError> {
        if self.source.file().profile_acquisition_identity().as_deref()
            != Some(self.owner.to_string().as_str())
        {
            return Err(LibraryError::HarvestAuthoredStaleOwner);
        }
        self.source.validate().map_err(|_| invalid())
    }
}
/// Privately issued CAS snapshot of one file and at most 64 incident edges (64 KiB total).
/// It is a metadata precondition, never filesystem qualification or ownership authority.
#[derive(Clone, Debug)]
pub struct HarvestAuthoredSnapshot {
    file: File,
    identity: [u8; 32],
    owner: Uuid,
}
/// Immutable ordinary metadata operation plus fresh actual source observation.
#[derive(Clone, Debug)]
pub struct HarvestAuthoredRequest {
    owner: Uuid,
    payload: Payload,
    bytes: Vec<u8>,
    source: SourceContentIdentityObservation,
}
impl HarvestAuthoredRequest {
    /// Capture user intent. Persist proposal bytes before dispatch for recoverable retries.
    pub fn new(
        owner: &WritableProfileGuard,
        operation: OperationId,
        snapshot: HarvestAuthoredSnapshot,
        source: SourceContentIdentityObservation,
        state: HarvestState,
    ) -> Result<Self, LibraryError> {
        owner.validate_current()?;
        if Uuid::parse_str(owner.owner_identity()).ok() != Some(snapshot.owner)
            || snapshot.file != File::from_observation(&source)?
        {
            return Err(invalid());
        }
        let payload = Payload {
            version: 1,
            operation,
            profile: owner.profile_root_identity().into(),
            file: snapshot.file,
            snapshot: snapshot.identity,
            state: state.as_str().into(),
        };
        let bytes = payload.encode()?;
        let request = Self::restore(owner, &bytes, source)?;
        request.validate_sources(true)?;
        Ok(request)
    }
    /// Restore exact historical intent with current owners for the same storage/key.
    /// Existing replay may use later content; a new effect requires all historical facts.
    pub fn restore(
        owner: &WritableProfileGuard,
        bytes: &[u8],
        source: SourceContentIdentityObservation,
    ) -> Result<Self, LibraryError> {
        owner.validate_current()?;
        let payload = Payload::decode(bytes)?;
        if payload.profile != owner.profile_root_identity() {
            return Err(invalid());
        }
        let request = Self {
            owner: Uuid::parse_str(owner.owner_identity()).map_err(|_| invalid())?,
            payload,
            bytes: bytes.into(),
            source,
        };
        request.validate_sources(false)?;
        Ok(request)
    }
    /// Exact durable custody bytes; decoding cannot manufacture source observations.
    pub fn proposal_bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Ordinary stable metadata operation, independent of universal journal work keys.
    pub fn operation(&self) -> OperationId {
        self.payload.operation
    }
    pub(super) fn validate_sources(&self, exact: bool) -> Result<(), LibraryError> {
        if self.source.file().profile_acquisition_identity().as_deref()
            != Some(self.owner.to_string().as_str())
        {
            return Err(LibraryError::HarvestAuthoredStaleOwner);
        }
        self.source.validate().map_err(|_| invalid())?;
        let fresh = File::from_observation(&self.source)?;
        let old = &self.payload.file;
        if old.path != fresh.path
            || !old.scope.same_storage(&fresh.scope)
            || (exact && *old != fresh)
        {
            return Err(invalid());
        }
        Ok(())
    }
}
/// Durable metadata result; `Done` here is a user workflow state, never journal completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HarvestAuthoredDisposition {
    /// Explicit state committed on the exact verified identity.
    Applied,
    /// The captured metadata precondition changed.
    GraphChanged,
    /// Existing state belongs to other or unverified content.
    IdentityConflict,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Outcome {
    version: u8,
    disposition: HarvestAuthoredDisposition,
    created: i64,
}
/// Privately issued retained immutable outcome.
#[derive(Clone, Debug)]
pub struct HarvestAuthoredReceipt {
    payload: Payload,
    outcome: Outcome,
    sequence: u64,
    owner: Uuid,
}
impl HarvestAuthoredReceipt {
    /// Original stable ordinary metadata operation.
    pub fn operation(&self) -> OperationId {
        self.payload.operation
    }
    /// Original disposition; only Applied changed the user state.
    pub fn disposition(&self) -> HarvestAuthoredDisposition {
        self.outcome.disposition
    }
    /// Original user intent (not a live-state read).
    pub fn requested_state(&self) -> HarvestState {
        HarvestState::from_stored(&self.payload.state)
    }
    /// Original durable sequence.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    /// Historical acquisition, separate from current retry authority.
    pub fn owner_identity(&self) -> Uuid {
        self.owner
    }
}
/// A new outcome or exact immutable replay without a second mutation.
#[derive(Clone, Debug)]
pub enum HarvestAuthoredResult {
    /// A new durable outcome.
    Committed(HarvestAuthoredReceipt),
    /// Original durable outcome, preserving later user edits.
    Replayed(HarvestAuthoredReceipt),
}
impl HarvestAuthoredResult {
    /// Original outcome.
    pub fn receipt(&self) -> &HarvestAuthoredReceipt {
        match self {
            Self::Committed(r) | Self::Replayed(r) => r,
        }
    }
}
fn invalid() -> LibraryError {
    LibraryError::HarvestAuthoredInput
}
fn bounded<T: Serialize>(value: &T) -> Result<Vec<u8>, LibraryError> {
    let mut writer = crate::operation_journal::codec::CappedWriter::new(65536);
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.finish())
}
