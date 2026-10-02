//! Durable source-receipt correlations in the existing global owner; never completion authority.
use rusqlite::params;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{LibraryDatabase, LibraryError};
use crate::operation_journal::OperationId;
use crate::operation_journal::work_plan::{WorkUnitKey, WorkUnitRole};
use crate::sample_sources::db::{SourceCommandId, SourceWriterReceipt};

/// Maximum retained source/global correlations; replay evidence is never implicitly evicted.
pub const OPERATION_LINK_MAX_RECEIPTS: i64 = 8192;
/// Maximum retained canonical payload bytes.
pub const OPERATION_LINK_MAX_BYTES: i64 = 16 * 1024 * 1024;
const MAX_PAYLOAD: usize = 32 * 1024;
pub(super) const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS operation_link_receipts (
 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
 link_key TEXT NOT NULL UNIQUE CHECK(length(CAST(link_key AS BLOB)) BETWEEN 1 AND 1024),
 payload BLOB NOT NULL CHECK(length(payload) BETWEEN 1 AND 32768),
 payload_identity BLOB NOT NULL CHECK(length(payload_identity) = 32),
 owner_identity TEXT NOT NULL CHECK(length(owner_identity) = 36)
)";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    version: u8,
    operation: OperationId,
    global: WorkUnitKey,
    source: WorkUnitKey,
    command: SourceCommandId,
    source_id: String,
    root: Vec<u8>,
    database: String,
    root_binding: String,
    lifecycle: u64,
    lease: u64,
    revision: u64,
}

/// Bounded immutable correlation derived from an actual committed source-owner receipt.
/// The family caller must establish membership in its exact admitted frozen work plan.
#[derive(Clone, Debug)]
pub struct OperationLinkRequest {
    owner: Uuid,
    key: String,
    bytes: Vec<u8>,
    identity: [u8; 32],
}
impl OperationLinkRequest {
    /// Bind source receipt observations to the family's source and global work keys.
    /// Profile acquisition spelling is an equality fence, never a live capability.
    pub fn new(
        owner: &str,
        source: WorkUnitKey,
        global: WorkUnitKey,
        receipt: &SourceWriterReceipt,
    ) -> Result<Self, LibraryError> {
        if receipt.identity().participant_id() != source.participant_id() {
            return Err(LibraryError::OperationLinkInput);
        }
        let revision = receipt.source_revision();
        // Bound owner observations before cloning them into the queue payload.
        if revision.source_id().as_str().len() > 256
            || revision.root_identity().as_bytes().len() > 4096
            || revision.database_identity().len() > 256
            || revision.root_binding_identity().len() > 256
        {
            return Err(LibraryError::OperationLinkInput);
        }
        Self::from_payload(
            owner,
            Payload {
                version: 1,
                operation: receipt.identity().operation_id(),
                source,
                global,
                command: receipt.identity().command_id(),
                source_id: revision.source_id().as_str().to_owned(),
                root: revision.root_identity().as_bytes().to_vec(),
                database: revision.database_identity().to_owned(),
                root_binding: revision.root_binding_identity().to_owned(),
                lifecycle: revision.lifecycle_generation(),
                lease: revision.lease_epoch(),
                revision: revision.revision(),
            },
        )
    }
    fn from_payload(owner: &str, payload: Payload) -> Result<Self, LibraryError> {
        let owner = Uuid::parse_str(owner).map_err(|_| LibraryError::OperationLinkInput)?;
        if owner.is_nil() {
            return Err(LibraryError::OperationLinkInput);
        }
        payload.validate()?;
        let bytes = serde_json::to_vec(&payload)?;
        if bytes.len() > MAX_PAYLOAD {
            return Err(LibraryError::OperationLinkInput);
        }
        let key = payload.key()?;
        let identity = digest(&bytes);
        Ok(Self {
            owner,
            key,
            bytes,
            identity,
        })
    }
    /// Immutable payload fingerprint; replay also compares exact canonical bytes.
    pub fn payload_identity(&self) -> &[u8; 32] {
        &self.identity
    }
}
impl Payload {
    fn validate(&self) -> Result<(), LibraryError> {
        let valid_text = |s: &str| !s.is_empty() && s.len() <= 256 && !s.contains('\0');
        if self.version != 1
            || self.operation.as_uuid().is_nil()
            || self.command.as_uuid().is_nil()
            || self.global.participant_id().as_uuid().is_nil()
            || self.source.participant_id().as_uuid().is_nil()
            || self.global.role() != WorkUnitRole::GlobalLibrary
            || self.source.role() != WorkUnitRole::SourceAffect
            || !valid_text(&self.source_id)
            || !valid_text(&self.database)
            || !valid_text(&self.root_binding)
            || self.root.is_empty()
            || self.root.len() > 4096
            || self.lease == 0
            || [self.lifecycle, self.lease, self.revision]
                .iter()
                .any(|v| *v > i64::MAX as u64)
            || serde_json::to_vec(&self.global)?.len() > 256
            || serde_json::to_vec(&self.source)?.len() > 256
        {
            return Err(LibraryError::OperationLinkInput);
        }
        Ok(())
    }
    fn key(&self) -> Result<String, LibraryError> {
        let key = serde_json::to_string(&(self.operation, self.global, self.source, self.command))?;
        if key.len() > 1024 {
            return Err(LibraryError::OperationLinkInput);
        }
        Ok(key)
    }
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"yield.operation-link.source-curation.v1\0");
    h.update(bytes);
    *h.finalize().as_bytes()
}

/// Immutable durable correlation observation; cannot advance a private journal checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationLinkReceipt {
    payload: Payload,
    sequence: u64,
    owner: Uuid,
    identity: [u8; 32],
}
impl OperationLinkReceipt {
    /// Correlated operation identity.
    pub fn operation_id(&self) -> OperationId {
        self.payload.operation
    }
    /// Frozen global obligation observed by this link.
    pub fn global_key(&self) -> WorkUnitKey {
        self.payload.global
    }
    /// Frozen source obligation observed by this link.
    pub fn source_key(&self) -> WorkUnitKey {
        self.payload.source
    }
    /// Original source command identity.
    pub fn source_command_id(&self) -> SourceCommandId {
        self.payload.command
    }
    /// Original source commit revision, never a current mutation grant.
    pub fn source_revision(&self) -> u64 {
        self.payload.revision
    }
    /// Ledger sequence, not a physical source revision.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    /// Historical profile acquisition that committed this row.
    pub fn owner_identity(&self) -> Uuid {
        self.owner
    }
    /// Immutable canonical payload fingerprint.
    pub fn payload_identity(&self) -> &[u8; 32] {
        &self.identity
    }
}

/// Successful persistence or exact replay, both carrying the same immutable observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationLinkResult {
    /// New row committed and read back.
    Committed(OperationLinkReceipt),
    /// Original row returned without another participant effect.
    Replayed(OperationLinkReceipt),
}
impl OperationLinkResult {
    /// Borrow the immutable correlation observation.
    pub fn receipt(&self) -> &OperationLinkReceipt {
        match self {
            Self::Committed(r) | Self::Replayed(r) => r,
        }
    }
}
impl LibraryDatabase {
    pub(super) fn apply_operation_link(
        &mut self,
        owner: &str,
        request: &OperationLinkRequest,
    ) -> Result<OperationLinkResult, LibraryError> {
        if Uuid::parse_str(owner).ok() != Some(request.owner) {
            return Err(LibraryError::OperationLinkStaleOwner);
        }
        let tx = self.connection.transaction()?;
        if let Some(receipt) = read_receipt(&tx, request)? {
            tx.commit()?;
            return Ok(OperationLinkResult::Replayed(receipt));
        }
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM (SELECT 1 FROM operation_link_receipts LIMIT ?1)",
            [OPERATION_LINK_MAX_RECEIPTS],
            |r| r.get(0),
        )?;
        if count >= OPERATION_LINK_MAX_RECEIPTS {
            return Err(LibraryError::OperationLinkReceiptCapacity);
        }
        let bytes: i64 = tx.query_row(
            "SELECT COALESCE(SUM(length(payload)), 0) FROM operation_link_receipts",
            [],
            |r| r.get(0),
        )?;
        if bytes < 0 || bytes > OPERATION_LINK_MAX_BYTES - request.bytes.len() as i64 {
            return Err(LibraryError::OperationLinkReceiptCapacity);
        }
        tx.execute("INSERT INTO operation_link_receipts (link_key, payload, payload_identity, owner_identity) VALUES (?1, ?2, ?3, ?4)", params![request.key, request.bytes, request.identity.as_slice(), request.owner.to_string()])?;
        let receipt =
            read_receipt(&tx, request)?.ok_or(LibraryError::OperationLinkMalformedReceipt)?;
        tx.commit()?;
        self.note_operation_link_commit();
        Ok(OperationLinkResult::Committed(receipt))
    }
}
fn read_receipt(
    connection: &rusqlite::Connection,
    request: &OperationLinkRequest,
) -> Result<Option<OperationLinkReceipt>, LibraryError> {
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM (SELECT 1 FROM operation_link_receipts WHERE link_key = ?1 LIMIT 2)",
        [&request.key],
        |r| r.get(0),
    )?;
    if count == 0 {
        return Ok(None);
    }
    if count != 1 {
        return Err(LibraryError::OperationLinkMalformedReceipt);
    }
    let (sequence, bytes, hash, owner): (i64, Option<Vec<u8>>, Option<Vec<u8>>, Option<String>) =
        connection.query_row(
            "SELECT sequence, CASE WHEN length(payload) BETWEEN 1 AND 32768 THEN payload END,
         CASE WHEN length(payload_identity) = 32 THEN payload_identity END,
         CASE WHEN length(CAST(owner_identity AS BLOB)) = 36 THEN owner_identity END
         FROM operation_link_receipts WHERE link_key = ?1",
            [&request.key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
    let malformed = || LibraryError::OperationLinkMalformedReceipt;
    let bytes = bytes.ok_or_else(malformed)?;
    let payload: Payload = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
    payload.validate().map_err(|_| malformed())?;
    let identity = digest(&bytes);
    let owner = Uuid::parse_str(&owner.ok_or_else(malformed)?).map_err(|_| malformed())?;
    if sequence <= 0
        || owner.is_nil()
        || hash.as_deref() != Some(identity.as_slice())
        || payload.key().map_err(|_| malformed())? != request.key
        || serde_json::to_vec(&payload)? != bytes
    {
        return Err(malformed());
    }
    if bytes != request.bytes {
        return Err(LibraryError::OperationLinkReplayConflict);
    }
    Ok(Some(OperationLinkReceipt {
        payload,
        sequence: sequence as u64,
        owner,
        identity,
    }))
}

#[cfg(test)]
pub(super) fn test_request(
    owner: &str,
    operation: u128,
    command: u128,
    revision: u64,
) -> OperationLinkRequest {
    use crate::operation_journal::ParticipantId;
    OperationLinkRequest::from_payload(
        owner,
        Payload {
            version: 1,
            operation: OperationId::from_uuid(Uuid::from_u128(operation)),
            global: WorkUnitKey::new(
                255,
                10,
                ParticipantId::from_uuid(Uuid::from_u128(2)),
                WorkUnitRole::GlobalLibrary,
            ),
            source: WorkUnitKey::new(
                0,
                60,
                ParticipantId::from_uuid(Uuid::from_u128(3)),
                WorkUnitRole::SourceAffect,
            ),
            command: SourceCommandId::from_uuid(Uuid::from_u128(command)),
            source_id: "source".into(),
            root: vec![1],
            database: "db".into(),
            root_binding: "binding".into(),
            lifecycle: 1,
            lease: 1,
            revision,
        },
    )
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operation_link_payload_bounds_versions_and_identities_fail_before_admission() {
        let owner = Uuid::from_u128(1).to_string();
        let request = test_request(&owner, 1, 1, 7);
        let original: Payload = serde_json::from_slice(&request.bytes).unwrap();
        let mut mutations = Vec::new();
        let mut p = original.clone();
        p.version = 2;
        mutations.push(p);
        let mut p = original.clone();
        p.source_id = "é".repeat(129);
        mutations.push(p);
        let mut p = original.clone();
        p.root = vec![1; 4097];
        mutations.push(p);
        let mut p = original.clone();
        p.database.clear();
        mutations.push(p);
        let mut p = original.clone();
        p.root_binding = "a\0b".into();
        mutations.push(p);
        let mut p = original.clone();
        p.lease = 0;
        mutations.push(p);
        let mut p = original.clone();
        p.lifecycle = u64::MAX;
        mutations.push(p);
        let mut p = original.clone();
        p.operation = OperationId::from_uuid(Uuid::nil());
        mutations.push(p);
        let mut p = original.clone();
        p.global = p.source;
        mutations.push(p);
        for invalid in mutations {
            assert!(matches!(
                OperationLinkRequest::from_payload(&owner, invalid),
                Err(LibraryError::OperationLinkInput)
            ));
        }
        assert!(matches!(
            OperationLinkRequest::from_payload(&Uuid::nil().to_string(), original),
            Err(LibraryError::OperationLinkInput)
        ));
    }
}
