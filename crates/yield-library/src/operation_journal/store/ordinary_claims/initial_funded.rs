//! Original initial claim insert/readback producers; caller holds their envelope.
use super::super::{OperationJournalStoreError, initial_settlement_backing};
use super::*;
use crate::operation_journal::{AdmissionClaimInventory, owner::InputPermit};

pub(in crate::operation_journal::store) fn insert(
    connection: &Connection,
    operation: OperationId,
    claims: &AdmissionClaimInventory,
    input: &InputPermit,
) -> Result<(), OperationJournalStoreError> {
    validate_storage_values(claims).map_err(super::super::ordinary_claim_write_error)?;
    let operation_text = operation.to_string();
    for claim in claims.claims() {
        initial_settlement_backing::statement(
            connection,
            input,
            c"INSERT INTO operation_journal_ordinary_claim
                 (claim_id, operation_id, volume_device, allocation_class, allocated_bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            |row| {
                row.bind_text(1, &claim.claim_id().to_string())?;
                row.bind_text(2, &operation_text)?;
                row.bind_text(3, &claim.volume().device.to_string())?;
                row.bind_text(4, allocation_class_text(claim.allocation_class()))?;
                row.bind_integer(
                    5,
                    i64::try_from(claim.allocated_bytes()).expect("validated SQLite INTEGER range"),
                )?;
                row.next().map(|_| ())
            },
        )
        .map_err(original_sql_error)?;
    }
    Ok(())
}

pub(in crate::operation_journal::store) fn verify(
    connection: &Connection,
    operation: OperationId,
    claims: &AdmissionClaimInventory,
    input: &InputPermit,
) -> Result<(), OperationJournalStoreError> {
    let mut expected = expected_records(operation, claims);
    let mut actual = initial_settlement_backing::statement(
        connection,
        input,
        c"SELECT claim_id, operation_id, volume_device, allocation_class, allocated_bytes
         FROM operation_journal_ordinary_claim
         WHERE operation_id = ?1
         ORDER BY claim_id ASC
         LIMIT ?2",
        |row| {
            row.bind_text(1, &operation.to_string())?;
            row.bind_integer(2, (MAX_PAGE_ROWS + 1) as i64)?;
            let mut records = Vec::with_capacity(MAX_PAGE_ROWS);
            while row.next()? {
                if records.len() == MAX_PAGE_ROWS {
                    return Err(super::super::ordinary_claim_write_error(
                        OrdinaryClaimWriteError::Read(malformed(
                            None,
                            format!(
                                "operation {operation} has more than {MAX_PAGE_ROWS} claim rows"
                            ),
                        )),
                    ));
                }
                let decoded = decode_claim_values([
                    row.get_ref(0)?,
                    row.get_ref(1)?,
                    row.get_ref(2)?,
                    row.get_ref(3)?,
                    row.get_ref(4)?,
                ])
                .map_err(|error| {
                    super::super::ordinary_claim_write_error(OrdinaryClaimWriteError::Read(error))
                })?;
                records.push(decoded);
            }
            Ok(records)
        },
    )
    .map_err(original_read_error)?;
    expected.sort_by_key(OrdinaryClaimRecord::claim_id);
    actual.sort_by_key(OrdinaryClaimRecord::claim_id);
    if actual != expected {
        return Err(super::super::ordinary_claim_write_error(
            OrdinaryClaimWriteError::ReadbackMismatch {
                operation_id: operation,
            },
        ));
    }
    Ok(())
}
fn original_sql_error(error: OperationJournalStoreError) -> OperationJournalStoreError {
    match error {
        OperationJournalStoreError::Sql(error) => {
            super::super::ordinary_claim_write_error(OrdinaryClaimWriteError::Sql(error))
        }
        error => error,
    }
}
fn original_read_error(error: OperationJournalStoreError) -> OperationJournalStoreError {
    match error {
        OperationJournalStoreError::Sql(error) => super::super::ordinary_claim_write_error(
            OrdinaryClaimWriteError::Read(OrdinaryClaimReadError::Sql(error)),
        ),
        error => error,
    }
}
