//! Passive historical workflow intent and disposition, never live request or settlement authority.
use super::super::harvest_facts::budget::HistoricalValueLimit;
use super::*;
use rusqlite::Connection;
use std::sync::{Arc, atomic::AtomicBool};

/// One original ordinary metadata operation. This selector grants no execution authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HarvestAuthoredReceiptKey(OperationId);
impl HarvestAuthoredReceiptKey {
    /// Select a non-nil original operation, independently of a current source or request.
    pub fn new(operation: OperationId) -> Result<Self, LibraryError> {
        if operation.as_uuid().is_nil() {
            return Err(invalid());
        }
        Ok(Self(operation))
    }
    /// Original ordinary operation selected by this lookup.
    pub fn operation(&self) -> OperationId {
        self.0
    }
}

/// Fixed historical metadata facts. No source, journal, request or live receipt is recreated.
/// Requested `Done` is user workflow intent, never genuine settlement or saga completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HarvestAuthoredReceiptObservation {
    operation: OperationId,
    sequence: u64,
    owner: Uuid,
    disposition: HarvestAuthoredDisposition,
    state: HarvestState,
    proposal_identity: [u8; 32],
}
const _: () = assert!(std::mem::size_of::<HarvestAuthoredReceiptObservation>() <= 192);
impl HarvestAuthoredReceiptObservation {
    /// Original ordinary operation.
    pub fn operation(&self) -> OperationId {
        self.operation
    }
    /// Immutable original sequence.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    /// Historical acquisition, separate from the current reader's authority.
    pub fn owner_identity(&self) -> Uuid {
        self.owner
    }
    /// Original disposition; only Applied changed user workflow state at that time.
    pub fn disposition(&self) -> HarvestAuthoredDisposition {
        self.disposition
    }
    /// Original requested workflow state, not current state or settlement.
    pub fn requested_state(&self) -> HarvestState {
        self.state
    }
    /// Digest of the exact canonical original proposal without retaining payload bytes.
    pub fn proposal_identity(&self) -> &[u8; 32] {
        &self.proposal_identity
    }
}
impl LibraryDatabase {
    pub(in super::super) fn observe_harvest_authored_receipt(
        &mut self,
        profile: &str,
        key: HarvestAuthoredReceiptKey,
    ) -> Result<Option<HarvestAuthoredReceiptObservation>, LibraryError> {
        let tx = self.connection.transaction()?;
        let value_limit = HistoricalValueLimit::new(&tx)?;
        let canceled = Arc::new(AtomicBool::new(false));
        let observed = super::super::harvest_facts::budget::run(&tx, &canceled, || {
            read_snapshot(&tx, profile, key)
        })
        .map_err(|error| {
            if matches!(&error, LibraryError::HarvestFactsQueryBudget)
                || matches!(&error, LibraryError::Sql(rusqlite::Error::SqliteFailure(code, _))
                    if code.extended_code == rusqlite::ffi::SQLITE_TOOBIG)
            {
                LibraryError::HarvestAuthoredMalformedReceipt
            } else {
                error
            }
        });
        // Restore native value policy before COMMIT, refusal and worker unwind.
        drop(value_limit);
        let observed = observed?;
        tx.commit()?;
        Ok(observed)
    }
}
fn read_snapshot(
    c: &Connection,
    profile: &str,
    key: HarvestAuthoredReceiptKey,
) -> Result<Option<HarvestAuthoredReceiptObservation>, LibraryError> {
    let rows: i64 = c.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM harvest_authored_receipts LIMIT 8193)",
        [],
        |row| row.get(0),
    )?;
    if rows > HARVEST_AUTHORED_MAX_RECEIPTS {
        return Err(LibraryError::HarvestAuthoredMalformedReceipt);
    }
    ledger::read_stored(c, &key.0.as_uuid().to_string(), None)?
        .map(|(receipt, bytes)| {
            if receipt.payload.profile != profile || receipt.operation() != key.0 {
                return Err(LibraryError::HarvestAuthoredMalformedReceipt);
            }
            Ok(HarvestAuthoredReceiptObservation {
                operation: receipt.operation(),
                sequence: receipt.sequence(),
                owner: receipt.owner_identity(),
                disposition: receipt.disposition(),
                state: receipt.requested_state(),
                proposal_identity: *blake3::hash(&bytes).as_bytes(),
            })
        })
        .transpose()
}
