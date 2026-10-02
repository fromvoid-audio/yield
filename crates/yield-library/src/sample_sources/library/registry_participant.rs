//! One operation-scoped registry participant in the existing global-library owner.
//!
//! Receipts are durable observations, not journal admission or coordinator completion authority.
//! Profile acquisition identity is an equality fence; the retained guard remains the authority.

use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use super::LibraryError;
use super::connection::LibraryDatabase;
use super::owner::SourceRegistrySnapshot;
use super::registry_limits::{bounded_json, validate_sources_encoded};
use super::sources::{load_sources_from_connection, map_registry_bound_error, normalized_source};
use crate::operation_journal::OperationId;
use crate::operation_journal::work_plan::{WorkUnitKey, WorkUnitRole};

/// Maximum retained operation receipts; exhaustion fails closed without evicting replay evidence.
pub const SOURCE_REGISTRY_PARTICIPANT_MAX_RECEIPTS: i64 = 8192;

pub(super) const CREATE_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS source_registry_participant_receipts (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    operation_id TEXT NOT NULL UNIQUE CHECK(length(operation_id) = 36),
    work_key TEXT NOT NULL CHECK(length(CAST(work_key AS BLOB)) BETWEEN 1 AND 256),
    payload_identity BLOB NOT NULL CHECK(length(payload_identity) = 32),
    owner_identity TEXT NOT NULL CHECK(length(owner_identity) = 36),
    disposition TEXT NOT NULL CHECK(disposition IN ('applied', 'registry_changed'))
)";

/// Immutable request for one registry replacement per operation.
///
/// The expected snapshot is a content fence, not a source lease or monotonic revision. The owner
/// persists either application or a stale-content rejection, so replay never reapplies an old
/// replacement over later edits. Family routing must supply the actual frozen global work key.
#[derive(Clone, Debug)]
pub struct SourceRegistryParticipantRequest {
    owner_identity: Uuid,
    operation_id: OperationId,
    key: WorkUnitKey,
    expected_identity: [u8; 32],
    replacement: SourceRegistrySnapshot,
    payload_identity: [u8; 32],
    encoded_key: String,
}

impl SourceRegistryParticipantRequest {
    /// Bind a bounded replacement and expected current snapshot to the caller's durable identity.
    /// A UUID spelling is only an observation; execution checks the retained live profile guard.
    pub fn new(
        owner_identity: &str,
        operation_id: OperationId,
        key: WorkUnitKey,
        expected: &SourceRegistrySnapshot,
        replacement: SourceRegistrySnapshot,
    ) -> Result<Self, LibraryError> {
        let owner_identity =
            Uuid::parse_str(owner_identity).map_err(|_| invalid("owner_identity"))?;
        if owner_identity.is_nil()
            || operation_id.as_uuid().is_nil()
            || key.participant_id().as_uuid().is_nil()
            || key.role() != WorkUnitRole::GlobalLibrary
        {
            return Err(invalid("operation/work-unit identity"));
        }
        let encoded_key = serde_json::to_string(&key)?;
        if encoded_key.len() > 256 {
            return Err(invalid("work_key"));
        }
        let expected_identity = registry_identity(expected)?;
        // Normalize exactly as the existing source-registry transaction does. This is stored
        // registry policy only; no filesystem authority or canonical-path ownership is inferred.
        validate_sources_encoded(replacement.as_slice()).map_err(map_registry_bound_error)?;
        let replacement = SourceRegistrySnapshot::new(
            replacement
                .as_slice()
                .iter()
                .cloned()
                .map(normalized_source)
                .collect(),
        );
        let replacement_identity = registry_identity(&replacement)?;
        let mut digest = blake3::Hasher::new();
        digest.update(b"yield.registry-participant.replace.v1\0");
        digest.update(&expected_identity);
        digest.update(&replacement_identity);
        Ok(Self {
            owner_identity,
            operation_id,
            key,
            expected_identity,
            replacement,
            payload_identity: *digest.finalize().as_bytes(),
            encoded_key,
        })
    }

    /// Return the immutable schema/payload binding for correlating a returned owner observation.
    pub fn payload_identity(&self) -> &[u8; 32] {
        &self.payload_identity
    }
}

/// Durable outcome of the bounded source-registry participant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceRegistryParticipantDisposition {
    /// Registry and retained source mappings committed with this receipt.
    Applied,
    /// The expected snapshot no longer matched; only the rejection receipt committed.
    RegistryChanged,
}

/// Immutable committed receipt, returned unchanged on exact replay through a current owner.
///
/// Its sequence orders this ledger's receipts, not physical source revisions. Neither this value
/// nor a successful queue submission can mint a private journal/coordinator completion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceRegistryParticipantReceipt {
    operation_id: OperationId,
    key: WorkUnitKey,
    sequence: u64,
    owner_identity: Uuid,
    payload_identity: [u8; 32],
    disposition: SourceRegistryParticipantDisposition,
}

impl SourceRegistryParticipantReceipt {
    /// Return the durable operation link.
    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }
    /// Return the frozen participant/work-unit link.
    pub fn key(&self) -> WorkUnitKey {
        self.key
    }
    /// Return the committed ledger sequence (not a source revision).
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    /// Return the historical profile acquisition that committed the receipt.
    pub fn owner_identity(&self) -> Uuid {
        self.owner_identity
    }
    /// Return the immutable schema/payload binding.
    pub fn payload_identity(&self) -> &[u8; 32] {
        &self.payload_identity
    }
    /// Return the committed application or retained rejection disposition.
    pub fn disposition(&self) -> SourceRegistryParticipantDisposition {
        self.disposition
    }
}

fn invalid(field: &'static str) -> LibraryError {
    LibraryError::SourceRegistryParticipantInput { field }
}

fn registry_identity(snapshot: &SourceRegistrySnapshot) -> Result<[u8; 32], LibraryError> {
    validate_sources_encoded(snapshot.as_slice()).map_err(map_registry_bound_error)?;
    let normalized = snapshot
        .as_slice()
        .iter()
        .cloned()
        .map(normalized_source)
        .collect::<Vec<_>>();
    let bytes = bounded_json(&normalized).map_err(map_registry_bound_error)?;
    Ok(*blake3::hash(&bytes).as_bytes())
}

impl LibraryDatabase {
    pub(super) fn apply_source_registry_participant(
        &mut self,
        current_owner_identity: &str,
        request: &SourceRegistryParticipantRequest,
    ) -> Result<SourceRegistryParticipantReceipt, LibraryError> {
        // run_owner validates its live root/lock guard before and after this command. Check the
        // acquisition equality before even serving a duplicate; a stale request gets no effect.
        if Uuid::parse_str(current_owner_identity).ok() != Some(request.owner_identity) {
            return Err(LibraryError::SourceRegistryParticipantStaleOwner);
        }
        let tx = self.connection.transaction()?;
        if let Some(receipt) = read_receipt(&tx, request)? {
            tx.commit()?;
            return Ok(receipt);
        }
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM (SELECT 1 FROM source_registry_participant_receipts LIMIT ?1)",
            [SOURCE_REGISTRY_PARTICIPANT_MAX_RECEIPTS],
            |row| row.get(0),
        )?;
        if count >= SOURCE_REGISTRY_PARTICIPANT_MAX_RECEIPTS {
            return Err(LibraryError::SourceRegistryParticipantReceiptCapacity);
        }
        let current = SourceRegistrySnapshot::new(load_sources_from_connection(&tx)?);
        let disposition = if registry_identity(&current)? == request.expected_identity {
            Self::replace_source_registry_in_tx(&tx, &request.replacement)?;
            "applied"
        } else {
            "registry_changed"
        };
        tx.execute(
            "INSERT INTO source_registry_participant_receipts
            (operation_id, work_key, payload_identity, owner_identity, disposition)
            VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                request.operation_id.to_string(),
                request.encoded_key,
                request.payload_identity.as_slice(),
                request.owner_identity.to_string(),
                disposition
            ],
        )?;
        // Read back the exact relation before committing. Any failure rolls back both registry
        // and receipt. A lost post-commit response is recovered by this same immutable key.
        let receipt = read_receipt(&tx, request)?.ok_or_else(|| invalid("receipt readback"))?;
        tx.commit()?;
        Ok(receipt)
    }
}

fn read_receipt(
    connection: &rusqlite::Connection,
    request: &SourceRegistryParticipantRequest,
) -> Result<Option<SourceRegistryParticipantReceipt>, LibraryError> {
    // A retained relation may have lost its unique constraint. Never choose an arbitrary
    // duplicate as authoritative replay evidence; bound this ambiguity check to two rows.
    let matches: i64 = connection.query_row(
        "SELECT COUNT(*) FROM (SELECT 1 FROM source_registry_participant_receipts
         WHERE operation_id = ?1 LIMIT 2)",
        [request.operation_id.to_string()],
        |row| row.get(0),
    )?;
    if matches > 1 {
        return Err(LibraryError::SourceRegistryParticipantReplayConflict);
    }
    if matches == 0 {
        return Ok(None);
    }
    // Bound before materializing text/blob even if a retained database has corrupt rows/schema.
    let row = connection
        .query_row(
            "SELECT sequence,
        CASE WHEN length(CAST(work_key AS BLOB)) <= 256 THEN work_key END,
        CASE WHEN length(payload_identity) = 32 THEN payload_identity END,
        CASE WHEN length(owner_identity) = 36 THEN owner_identity END,
        CASE WHEN length(disposition) <= 16 THEN disposition END
        FROM source_registry_participant_receipts WHERE operation_id = ?1",
            [request.operation_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((sequence, key, payload, owner, disposition)) = row else {
        return Ok(None);
    };
    let key = key.ok_or_else(|| invalid("retained work_key"))?;
    let payload: [u8; 32] = payload
        .ok_or_else(|| invalid("retained payload"))?
        .try_into()
        .map_err(|_| invalid("retained payload"))?;
    if key != request.encoded_key || payload != request.payload_identity {
        return Err(LibraryError::SourceRegistryParticipantReplayConflict);
    }
    let owner_identity = Uuid::parse_str(&owner.ok_or_else(|| invalid("retained owner"))?)
        .map_err(|_| invalid("retained owner"))?;
    if owner_identity.is_nil() || sequence <= 0 {
        return Err(invalid("retained receipt identity"));
    }
    let disposition = match disposition.as_deref() {
        Some("applied") => SourceRegistryParticipantDisposition::Applied,
        Some("registry_changed") => SourceRegistryParticipantDisposition::RegistryChanged,
        _ => return Err(invalid("retained disposition")),
    };
    Ok(Some(SourceRegistryParticipantReceipt {
        operation_id: request.operation_id,
        key: request.key,
        sequence: sequence as u64,
        owner_identity,
        payload_identity: payload,
        disposition,
    }))
}
