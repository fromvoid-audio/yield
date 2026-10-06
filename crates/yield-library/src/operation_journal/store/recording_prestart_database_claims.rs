//! Incremental backing for the complete original database-claim audit.
//! Historical rows remain observations; this creates no live admission or effects.
use super::*;
use crate::operation_journal::database_claims::{
    DatabaseBinding, DatabaseClaimSlice, DatabaseParticipant, MAX_DATABASES, MAX_INVENTORY_BYTES,
    MAX_RELATION_ROWS, MAX_SLICES,
};
use crate::operation_journal::participants::MAX_PARTICIPANTS;

// Original binary decoding allocates these vectors once, with the declared bounded
// capacity. All owned V1 strings are disjoint slices of the <=64-KiB input.
const INVENTORY_TYPED_BYTES: usize =
    MAX_DATABASES * 1024 + MAX_SLICES * 256 + MAX_PARTICIPANTS * 128 + MAX_INVENTORY_BYTES;
// validate() simultaneously retains binding IDs, physical keys, participant IDs and
// slice IDs. 512 bytes per entry covers a complete B-tree node even at first insertion.
const VALIDATION_SET_BYTES: usize = (2 * MAX_DATABASES + MAX_PARTICIPANTS + MAX_SLICES) * 512;
// Complete fixed-size SQL pages, readback columns, query strings and temporary keys.
const SQL_CONTEXT_BYTES: usize = 64 * 1024;
pub(super) const ADDITIONAL_INPUT_BYTES: usize =
    INVENTORY_TYPED_BYTES + VALIDATION_SET_BYTES + SQL_CONTEXT_BYTES;
const _: () = {
    assert!(std::mem::size_of::<DatabaseBinding>() <= 1024);
    assert!(std::mem::size_of::<DatabaseClaimSlice>() <= 256);
    assert!(std::mem::size_of::<DatabaseParticipant>() <= 128);
    assert!(ADDITIONAL_INPUT_BYTES <= 512 * 1024);
};

impl OperationJournalStore {
    /// Scalar-only complete preflight, inside the original consistent snapshot.
    pub(super) fn preflight_recording_database_claims(
        &self,
    ) -> Result<(), OperationJournalStoreError> {
        for table in [database_claims::BINDING_TABLE, database_claims::SLICE_TABLE] {
            let count: i64 = self.connection.query_row(
                &format!("SELECT COUNT(*) FROM (SELECT 1 FROM {table} LIMIT ?1)"),
                [MAX_RELATION_ROWS as i64 + 1],
                |row| row.get(0),
            )?;
            if !(0..=MAX_RELATION_ROWS as i64).contains(&count) {
                return Err(OperationJournalStoreError::RecordingPrestartResourceBound);
            }
        }
        // A valid V17 Recording inventory has authoritative/global participants, never
        // a V18 lease/retirement scope. Its canonical encoding is therefore V1. Refuse
        // unsupported nested lease materialization before copying the original blob;
        // the complete semantic audit below still checks every identity and relation.
        let bad: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM operation_journal_database_binding WHERE
             (ordinal=0 AND (typeof(inventory)!='blob' OR length(inventory) NOT BETWEEN 1 AND ?1
              OR substr(inventory,1,6)!=X'595743420100'))
             OR (ordinal!=0 AND inventory IS NOT NULL))",
            [MAX_INVENTORY_BYTES as i64],
            |row| row.get(0),
        )?;
        if bad {
            return Err(OperationJournalStoreError::RecordingPrestartResourceBound);
        }
        Ok(())
    }
}
