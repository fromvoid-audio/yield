//! Actual owner-scoped Harvest rekeys and immutable dispositions, never physical completion.
use super::{
    LibraryDatabase, LibraryError,
    reference_snapshot::{self, ReferenceSnapshot},
};
use crate::operation_journal::work_plan::{WorkUnitKey, WorkUnitRole};
use crate::operation_journal::{OperationId, StoredRelativePath};
use crate::sample_sources::{SourceId, db::SourceReferenceFence};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod receipt;
use receipt::read_receipt;
pub use receipt::{ReferenceDisposition, ReferenceReceipt, ReferenceRekeyResult};

/// Retained command count, with no implicit evidence eviction.
pub const REFERENCE_MAX_RECEIPTS: i64 = 8192;
/// Retained command and receipt field bytes, including fixed scalar overhead.
pub const REFERENCE_MAX_BYTES: i64 = 16 * 1024 * 1024;
pub(super) const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS reference_receipts (
 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
 command_key TEXT NOT NULL UNIQUE CHECK(length(CAST(command_key AS BLOB)) BETWEEN 1 AND 512),
 payload BLOB NOT NULL CHECK(length(payload) BETWEEN 1 AND 65536),
 payload_identity BLOB NOT NULL CHECK(length(payload_identity)=32),
 disposition TEXT NOT NULL CHECK(disposition IN ('applied','changed','missing','occupied')),
 owner_identity TEXT NOT NULL CHECK(length(CAST(owner_identity AS BLOB))=36))";

/// Validated UTF-8 source-relative endpoint; no path loss or ambient root lookup.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceFileKey {
    pub(super) source: String,
    pub(super) path: StoredRelativePath,
}
impl ReferenceFileKey {
    /// Bound source spelling and portable relative path before admission.
    pub fn new(source: SourceId, path: StoredRelativePath) -> Result<Self, LibraryError> {
        let key = Self {
            source: source.as_str().to_owned(),
            path,
        };
        key.validate()?;
        Ok(key)
    }
    fn validate(&self) -> Result<(), LibraryError> {
        if !text(&self.source) || self.path.validate().is_err() {
            return Err(LibraryError::ReferenceInput);
        }
        Ok(())
    }
}
fn text(s: &str) -> bool {
    !s.is_empty() && s.len() <= 256 && !s.contains('\0')
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
    fn from_fence(f: &SourceReferenceFence) -> Self {
        let r = f.source_revision();
        Self {
            source: r.source_id().as_str().into(),
            root: r.root_identity().as_bytes().to_vec(),
            database: r.database_identity().into(),
            binding: r.root_binding_identity().into(),
            lifecycle: r.lifecycle_generation(),
            lease: r.lease_epoch(),
            revision: r.revision(),
        }
    }
    fn valid(&self) -> bool {
        text(&self.source)
            && text(&self.database)
            && text(&self.binding)
            && !self.root.is_empty()
            && self.root.len() <= 4096
            && self.lease > 0
            && [self.lifecycle, self.lease, self.revision]
                .iter()
                .all(|n| *n <= i64::MAX as u64)
    }
    fn same_scope(&self, other: &Self) -> bool {
        self.source == other.source
            && self.root == other.root
            && self.database == other.database
            && self.binding == other.binding
    }
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    version: u8,
    operation: OperationId,
    work: WorkUnitKey,
    old: ReferenceFileKey,
    new: ReferenceFileKey,
    origin: Scope,
    destination: Scope,
    snapshot: [u8; 32],
}
impl Payload {
    fn validate(&self) -> Result<(), LibraryError> {
        self.old.validate()?;
        self.new.validate()?;
        if self.version != 1
            || self.operation.as_uuid().is_nil()
            || self.work.participant_id().as_uuid().is_nil()
            || self.work.role() != WorkUnitRole::GlobalLibrary
            || self.old.source == self.new.source
            || self.origin.source != self.old.source
            || self.destination.source != self.new.source
            || !self.origin.valid()
            || !self.destination.valid()
            || serde_json::to_vec(&self.work)?.len() > 256
        {
            return Err(LibraryError::ReferenceInput);
        }
        Ok(())
    }
    fn key(&self) -> Result<String, LibraryError> {
        Ok(serde_json::to_string(&(self.operation, self.work))?)
    }
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"yield.harvest-reference.command.v1\0");
    h.update(bytes);
    *h.finalize().as_bytes()
}

fn result_digest(bytes: &[u8], disposition: ReferenceDisposition) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"yield.harvest-reference.result.v1\0");
    h.update(bytes);
    h.update(disposition.spelling().as_bytes());
    *h.finalize().as_bytes()
}
/// Live owner-scoped snapshot read, also fenced to both actual issuing source writers.
#[derive(Clone, Debug)]
pub struct ReferenceSnapshotRequest {
    owner: Uuid,
    old: ReferenceFileKey,
    new: ReferenceFileKey,
    origin: SourceReferenceFence,
    destination: SourceReferenceFence,
}
impl ReferenceSnapshotRequest {
    pub(super) fn validate_sources(&self) -> Result<(), LibraryError> {
        self.origin.validate()?;
        self.destination.validate()?;
        Ok(())
    }

    /// Prepare a bounded read; source fences are capabilities issued by current writers.
    pub fn new(
        owner: &str,
        old: ReferenceFileKey,
        new: ReferenceFileKey,
        origin: SourceReferenceFence,
        destination: SourceReferenceFence,
    ) -> Result<Self, LibraryError> {
        old.validate()?;
        new.validate()?;
        let owner = owner_id(owner)?;
        if old.source == new.source
            || old.source != origin.source_revision().source_id().as_str()
            || new.source != destination.source_revision().source_id().as_str()
        {
            return Err(LibraryError::ReferenceInput);
        }
        origin.validate()?;
        destination.validate()?;
        Ok(Self {
            owner,
            old,
            new,
            origin,
            destination,
        })
    }
}
fn owner_id(owner: &str) -> Result<Uuid, LibraryError> {
    Uuid::parse_str(owner)
        .ok()
        .filter(|u| !u.is_nil())
        .ok_or(LibraryError::ReferenceInput)
}

/// Durable proposal plus fresh live read contexts. Persist proposal bytes before dispatch.
#[derive(Clone, Debug)]
pub struct ReferenceRekeyRequest {
    owner: Uuid,
    payload: Payload,
    bytes: Vec<u8>,
    identity: [u8; 32],
    origin: SourceReferenceFence,
    destination: SourceReferenceFence,
}
impl ReferenceRekeyRequest {
    /// Bind a snapshot to one actual frozen global obligation. Family membership is caller-owned.
    pub fn new(
        owner: &str,
        operation: OperationId,
        work: WorkUnitKey,
        snapshot: ReferenceSnapshot,
        origin: SourceReferenceFence,
        destination: SourceReferenceFence,
    ) -> Result<Self, LibraryError> {
        let payload = Payload {
            version: 1,
            operation,
            work,
            old: snapshot.old,
            new: snapshot.new,
            origin: Scope::from_fence(&origin),
            destination: Scope::from_fence(&destination),
            snapshot: snapshot.identity,
        };
        payload.validate()?;
        let bytes = serde_json::to_vec(&payload)?;
        Self::restore(owner, &bytes, origin, destination)
    }
    /// Restore the exact historical proposal with freshly acquired source contexts. Untrusted
    /// bytes must be bounded/canonical; fresh observations never rewrite historical payload.
    pub fn restore(
        owner: &str,
        bytes: &[u8],
        origin: SourceReferenceFence,
        destination: SourceReferenceFence,
    ) -> Result<Self, LibraryError> {
        if bytes.is_empty() || bytes.len() > 65536 {
            return Err(LibraryError::ReferenceInput);
        }
        let payload: Payload =
            serde_json::from_slice(bytes).map_err(|_| LibraryError::ReferenceInput)?;
        payload.validate()?;
        if serde_json::to_vec(&payload)? != bytes {
            return Err(LibraryError::ReferenceInput);
        }
        let request = Self {
            owner: owner_id(owner)?,
            payload,
            bytes: bytes.to_vec(),
            identity: digest(bytes),
            origin,
            destination,
        };
        request.validate_sources(false)?;
        Ok(request)
    }
    /// Immutable proposal fingerprint; replay also compares exact canonical bytes.
    pub fn payload_identity(&self) -> &[u8; 32] {
        &self.identity
    }
    /// Canonical immutable custody bytes for the caller's durable journal/proposal storage.
    pub fn proposal_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub(super) fn validate_sources(&self, exact: bool) -> Result<(), LibraryError> {
        self.origin.validate()?;
        self.destination.validate()?;
        for (historical, fresh) in [
            (&self.payload.origin, Scope::from_fence(&self.origin)),
            (
                &self.payload.destination,
                Scope::from_fence(&self.destination),
            ),
        ] {
            if !historical.same_scope(&fresh) || (exact && historical != &fresh) {
                return Err(LibraryError::ReferenceInput);
            }
        }
        Ok(())
    }
}
impl LibraryDatabase {
    pub(super) fn reference_snapshot(
        &mut self,
        owner: &str,
        request: &ReferenceSnapshotRequest,
    ) -> Result<ReferenceSnapshot, LibraryError> {
        if Uuid::parse_str(owner).ok() != Some(request.owner) {
            return Err(LibraryError::ReferenceStaleOwner);
        }
        request.origin.validate()?;
        request.destination.validate()?;
        let tx = self.connection.transaction()?;
        let snapshot = reference_snapshot::read(&tx, &request.old, &request.new)?;
        request.origin.validate()?;
        request.destination.validate()?;
        tx.commit()?;
        Ok(snapshot)
    }
    pub(super) fn apply_reference_rekey(
        &mut self,
        owner: &str,
        request: &ReferenceRekeyRequest,
    ) -> Result<ReferenceRekeyResult, LibraryError> {
        if Uuid::parse_str(owner).ok() != Some(request.owner) {
            return Err(LibraryError::ReferenceStaleOwner);
        }
        request.validate_sources(false)?;
        let tx = self.connection.transaction()?;
        if let Some(receipt) = read_receipt(&tx, request)? {
            request.validate_sources(false)?;
            tx.commit()?;
            return Ok(ReferenceRekeyResult::Replayed(receipt));
        }
        request.validate_sources(true)?;
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM (SELECT 1 FROM reference_receipts LIMIT 8192)",
            [],
            |r| r.get(0),
        )?;
        if count >= REFERENCE_MAX_RECEIPTS {
            return Err(LibraryError::ReferenceReceiptCapacity);
        }
        let bytes: i64 = tx.query_row(
            "SELECT COALESCE(SUM(length(payload)+length(CAST(command_key AS BLOB))+length(payload_identity)+length(CAST(disposition AS BLOB))+length(CAST(owner_identity AS BLOB))+8),0) FROM reference_receipts",
            [],
            |r| r.get(0),
        )?;
        if bytes < 0
            || bytes
                > REFERENCE_MAX_BYTES
                    - (request.bytes.len() + request.payload.key()?.len() + 32 + 36 + 8 + 8) as i64
        {
            return Err(LibraryError::ReferenceReceiptCapacity);
        }
        let current = reference_snapshot::read(&tx, &request.payload.old, &request.payload.new)?;
        let disposition = if current.identity != request.payload.snapshot {
            ReferenceDisposition::ReferenceChanged
        } else if current.occupied {
            ReferenceDisposition::DestinationOccupied
        } else if current.missing {
            ReferenceDisposition::ReferenceMissing
        } else {
            ReferenceDisposition::Applied
        };
        if disposition == ReferenceDisposition::Applied {
            let p = &request.payload;
            // Collision was rejected above. Updating only key columns preserves every exact
            // authored value, instead of the compatibility helper's silent merge/reparse.
            tx.execute("UPDATE harvest_files SET source_id=?3,relative_path=?4 WHERE source_id=?1 AND relative_path=?2",params![p.old.source,p.old.path.as_str(),p.new.source,p.new.path.as_str()])?;
            for prefix in ["parent", "child"] {
                tx.execute(&format!("UPDATE harvest_derivations SET {prefix}_source_id=?3,{prefix}_relative_path=?4 WHERE {prefix}_source_id=?1 AND {prefix}_relative_path=?2"),params![p.old.source,p.old.path.as_str(),p.new.source,p.new.path.as_str()])?;
            }
        }
        request.validate_sources(true)?;
        tx.execute("INSERT INTO reference_receipts(command_key,payload,payload_identity,disposition,owner_identity) VALUES(?1,?2,?3,?4,?5)",params![request.payload.key()?,request.bytes,result_digest(&request.bytes, disposition).as_slice(),disposition.spelling(),request.owner.to_string()])?;
        let receipt = read_receipt(&tx, request)?.ok_or(LibraryError::ReferenceMalformedReceipt)?;
        request.validate_sources(true)?;
        tx.commit()?;
        if request.validate_sources(true).is_err() {
            return Err(LibraryError::ReferenceCommitUncertain);
        }
        Ok(ReferenceRekeyResult::Committed(receipt))
    }
}
