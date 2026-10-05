//! Capped scalar validation of original retained V1 rows, not receipt authentication.
use super::{LibraryDatabase, LifecycleAllocationError};

// Match the compiled V1 storage contract, including byte extents instead of text characters.
// At most 8193 rows are examined; no subject/receipt/identity value enters Rust.
const REFERENCES: &str = "SELECT COUNT(*),
 COALESCE(SUM(CASE WHEN state IN ('Enrolled','Draining') THEN 1 ELSE 0 END),0),
 COALESCE(MIN(CASE WHEN
 typeof(registration_id)='text' AND length(CAST(registration_id AS BLOB))=36 AND instr(registration_id,char(0))=0
 AND typeof(source_id)='text' AND length(CAST(source_id AS BLOB)) BETWEEN 1 AND 256 AND instr(source_id,char(0))=0
 AND typeof(subject)='blob' AND length(subject) BETWEEN 1 AND 16384
 AND typeof(state)='text' AND state IN ('Enrolled','Draining','Retired')
 AND typeof(last_operation_id)='text' AND length(CAST(last_operation_id AS BLOB))=36 AND instr(last_operation_id,char(0))=0
 AND typeof(last_work_key)='text' AND length(CAST(last_work_key AS BLOB)) BETWEEN 1 AND 256 AND instr(last_work_key,char(0))=0
 AND typeof(row_identity)='blob' AND length(row_identity)=32
 THEN 1 ELSE 0 END),1)
 FROM (SELECT * FROM source_lifecycle_references LIMIT 8193)";
const RECEIPTS: &str = "SELECT COUNT(*),COALESCE(MIN(CASE WHEN
 typeof(sequence)='integer' AND sequence>0
 AND typeof(operation_id)='text' AND length(CAST(operation_id AS BLOB))=36 AND instr(operation_id,char(0))=0
 AND typeof(work_key)='text' AND length(CAST(work_key AS BLOB)) BETWEEN 1 AND 256 AND instr(work_key,char(0))=0
 AND typeof(payload_identity)='blob' AND length(payload_identity)=32
 AND typeof(owner_identity)='text' AND length(CAST(owner_identity AS BLOB))=36 AND instr(owner_identity,char(0))=0
 AND typeof(registration_id)='text' AND length(CAST(registration_id AS BLOB))=36 AND instr(registration_id,char(0))=0
 AND (before_identity IS NULL OR (typeof(before_identity)='blob' AND length(before_identity)=32))
 AND (after_identity IS NULL OR (typeof(after_identity)='blob' AND length(after_identity)=32))
 AND typeof(disposition)='text' AND disposition IN ('Enrolled','Draining','Retired','ExpectedRelationChanged')
 AND typeof(receipt)='blob' AND length(receipt) BETWEEN 1 AND 16384
 THEN 1 ELSE 0 END),1)
 FROM (SELECT * FROM source_lifecycle_reference_receipts LIMIT 8193)";

pub(super) fn counts(
    database: &LibraryDatabase,
) -> Result<(u64, u64, u64), LifecycleAllocationError> {
    use LifecycleAllocationError::Unknown;
    let (references, active, valid_references): (u64, u64, u64) = database
        .connection
        .query_row(REFERENCES, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(|_| Unknown)?;
    if references > 8192 || active > 4096 || valid_references != 1 {
        return Err(Unknown);
    }
    let (receipts, valid_receipts): (u64, u64) = database
        .connection
        .query_row(RECEIPTS, [], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|_| Unknown)?;
    if receipts > 8192 || valid_receipts != 1 {
        return Err(Unknown);
    }
    Ok((references, receipts, active))
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests;
