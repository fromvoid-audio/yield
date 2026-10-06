//! Opaque original-file diagnostic. It cannot allocate operation/claim identities or grant authority.
use super::*;

pub struct ChunkScalarAllocationProbe {
    connection: Connection,
}

impl ChunkScalarAllocationProbe {
    pub fn accepts_original_stored_row_for_test(&self, original: bool) -> bool {
        let mut statement = self.connection.prepare(
            "SELECT chunk_id, operation_id, claim_id, participant_id, output_key, logical_offset,
             logical_length, volume_device, allocation_class, allocated_bytes, allocation_unit,
             protected_free_bytes, state, issue_id, release_id FROM operation_journal_chunk_claim",
        ).expect("original fixture query");
        let mut rows = statement.query([]).expect("original fixture rows");
        let row = rows
            .next()
            .expect("original fixture row")
            .expect("one original row");
        if original {
            original_scalar_probe::original_decode_row(row).is_ok()
        } else {
            decode_row(row).is_ok()
        }
    }
}

impl crate::operation_journal::store::OperationJournalStore {
    /// Diagnostic-only file-backed decoder setup; not an original actor or admission fixture.
    pub fn prepare_chunk_scalar_allocation_probe_for_test(
        path: &std::path::Path,
        field: &str,
        bytes: usize,
    ) -> Result<ChunkScalarAllocationProbe, rusqlite::Error> {
        let column = match field {
            "output_key" => "output_key",
            "state" => "state",
            "allocation_class" => "allocation_class",
            "volume_device" => "volume_device",
            "operation_id" => "operation_id",
            _ => {
                return Err(rusqlite::Error::InvalidParameterName(
                    "unknown scalar probe field".into(),
                ));
            }
        };
        let connection = Connection::open(path)?;
        // Isolated original-table row diagnostic, not a complete journal/admission fixture.
        connection.execute_batch("PRAGMA foreign_keys=OFF")?;
        connection.execute_batch(CREATE_TABLE)?;
        connection.execute(
            "INSERT INTO operation_journal_chunk_claim VALUES(?1,?2,?3,?4,'segment',0,4096,'7','DestinationStaging',4096,4096,268435456,'Claimed',NULL,NULL)",
            params![Uuid::from_u128(1).to_string(),Uuid::from_u128(2).to_string(),Uuid::from_u128(3).to_string(),Uuid::from_u128(4).to_string()],
        )?;
        connection.execute_batch("PRAGMA ignore_check_constraints=ON")?;
        connection.execute(
            &format!("UPDATE {TABLE_NAME} SET {column}=?1"),
            ["x".repeat(bytes)],
        )?;
        Ok(ChunkScalarAllocationProbe { connection })
    }
}
