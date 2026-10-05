//! Original-owner readback of immutable metadata history, never execution or completion authority.
use super::*;
use crate::operation_journal::work_plan::WorkUnitRole;
use rusqlite::{Connection, limits::Limit};

// SQLite can compare an unindexed foreign key before any matching-row shape predicate.
// This caps that native value/record allocation, not only Rust row materialization.
const SQLITE_HISTORY_VALUE_LIMIT: i32 = 256 * 1024;
pub(super) struct HistoricalValueLimit<'a> {
    connection: &'a Connection,
    previous: i32,
}
impl<'a> HistoricalValueLimit<'a> {
    pub(super) fn new(connection: &'a Connection) -> Result<Self, LibraryError> {
        let previous = connection.limit(Limit::SQLITE_LIMIT_LENGTH)?;
        connection.set_limit(
            Limit::SQLITE_LIMIT_LENGTH,
            previous.min(SQLITE_HISTORY_VALUE_LIMIT),
        )?;
        Ok(Self {
            connection,
            previous,
        })
    }
}
impl Drop for HistoricalValueLimit<'_> {
    fn drop(&mut self) {
        // Borrowed connection stays live through normal return, refusal and worker unwind.
        let _ = self
            .connection
            .set_limit(Limit::SQLITE_LIMIT_LENGTH, self.previous);
    }
}
fn map_history_error(error: LibraryError) -> LibraryError {
    if matches!(&error, LibraryError::Sql(rusqlite::Error::SqliteFailure(code, _))
        if code.extended_code == rusqlite::ffi::SQLITE_TOOBIG)
    {
        LibraryError::HarvestCopyMalformedReceipt
    } else {
        error
    }
}

/// One exact historical Harvest-copy obligation. It grants no admission or replay authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HarvestCopyReceiptKey {
    operation: OperationId,
    work: WorkUnitKey,
}
impl HarvestCopyReceiptKey {
    /// Select one non-nil operation and required Harvest work identity.
    pub fn new(operation: OperationId, work: WorkUnitKey) -> Result<Self, LibraryError> {
        if operation.as_uuid().is_nil()
            || work.participant_id().as_uuid().is_nil()
            || work.role() != WorkUnitRole::Harvest
        {
            return Err(LibraryError::HarvestCopyInput);
        }
        Ok(Self { operation, work })
    }
    /// Original operation selected by this lookup.
    pub fn operation(&self) -> OperationId {
        self.operation
    }
    /// Original Harvest obligation selected by this lookup.
    pub fn work_key(&self) -> WorkUnitKey {
        self.work
    }
    pub(super) fn encoded(&self) -> Result<String, LibraryError> {
        Ok(serde_json::to_string(&(self.operation, self.work))?)
    }
}

/// Bounded historical facts read by the current original global owner.
///
/// This is separate from a live request or effect receipt: it reconstructs no source,
/// journal or enrollment observation and supplies no retry, physical, settlement or
/// private checkpoint authority. The owner identity describes the historical commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HarvestCopyReceiptObservation {
    key: HarvestCopyReceiptKey,
    sequence: u64,
    owner: Uuid,
    disposition: HarvestCopyDisposition,
    edge: Option<u64>,
    destination_revision: u64,
    proposal_identity: [u8; 32],
}
const _: () = assert!(std::mem::size_of::<HarvestCopyReceiptObservation>() <= 192);
impl HarvestCopyReceiptObservation {
    /// Original committed operation.
    pub fn operation(&self) -> OperationId {
        self.key.operation
    }
    /// Original committed Harvest obligation.
    pub fn work_key(&self) -> WorkUnitKey {
        self.key.work
    }
    /// Original immutable receipt sequence.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    /// Historical profile acquisition, not the current reader's authority.
    pub fn owner_identity(&self) -> Uuid {
        self.owner
    }
    /// Original immutable metadata disposition.
    pub fn disposition(&self) -> HarvestCopyDisposition {
        self.disposition
    }
    /// Original edge identity, only for an Applied metadata outcome.
    pub fn edge_id(&self) -> Option<u64> {
        self.edge
    }
    /// Historical destination enrollment revision.
    pub fn destination_revision(&self) -> u64 {
        self.destination_revision
    }
    /// Digest of the original canonical proposal, without retaining its payload.
    pub fn proposal_identity(&self) -> &[u8; 32] {
        &self.proposal_identity
    }
}

impl LibraryDatabase {
    pub(in super::super) fn observe_harvest_copy_receipt(
        &mut self,
        profile: &str,
        key: HarvestCopyReceiptKey,
    ) -> Result<Option<HarvestCopyReceiptObservation>, LibraryError> {
        // One read snapshot covers cardinality, bounded row materialization and uniqueness.
        // No source/journal queue round trips, mutable family dispatch or SQLite write occurs.
        let tx = self.connection.transaction()?;
        let value_limit = HistoricalValueLimit::new(&tx)?;
        let observed = read_snapshot(&tx, profile, key).map_err(map_history_error);
        drop(value_limit);
        let observed = observed?;
        tx.commit()?;
        Ok(observed)
    }
}

fn read_snapshot(
    tx: &Connection,
    profile: &str,
    key: HarvestCopyReceiptKey,
) -> Result<Option<HarvestCopyReceiptObservation>, LibraryError> {
    // Bound even a schema-corrupted, unindexed relation before looking up a missing key.
    // This read-only policy does not change original apply/replay capacity behavior.
    let rows: i64 = tx.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM harvest_copy_receipts LIMIT 8193)",
        [],
        |row| row.get(0),
    )?;
    if rows > HARVEST_COPY_MAX_RECEIPTS {
        return Err(LibraryError::HarvestCopyMalformedReceipt);
    }
    ledger::read_stored(tx, &key.encoded()?)?
        .map(|(receipt, bytes)| {
            if receipt.payload.profile != profile
                || receipt.operation() != key.operation
                || receipt.work_key() != key.work
            {
                return Err(LibraryError::HarvestCopyMalformedReceipt);
            }
            Ok(HarvestCopyReceiptObservation {
                key,
                sequence: receipt.sequence(),
                owner: receipt.owner_identity(),
                disposition: receipt.disposition(),
                edge: receipt.edge_id(),
                destination_revision: receipt.destination_revision(),
                proposal_identity: *blake3::hash(&bytes).as_bytes(),
            })
        })
        .transpose()
}
