//! Version-one storage layout only. Rows never issue registration, admission or settlement.
use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use rusqlite::{Connection, TransactionBehavior};

use super::{LibraryError, connection::LibraryDatabase};

// Exact compiled DDL is the version identifier. Changing it requires an explicit migration;
// CREATE IF NOT EXISTS must never silently accept or repair a different family image.
const DDL_V1: &str = r#"
CREATE TABLE source_lifecycle_references (
    registration_id TEXT NOT NULL PRIMARY KEY
        CHECK(typeof(registration_id) = 'text' AND length(CAST(registration_id AS BLOB)) = 36 AND instr(registration_id, char(0)) = 0),
    source_id TEXT NOT NULL
        CHECK(typeof(source_id) = 'text' AND length(CAST(source_id AS BLOB)) BETWEEN 1 AND 256 AND instr(source_id, char(0)) = 0),
    subject BLOB NOT NULL CHECK(typeof(subject) = 'blob' AND length(subject) BETWEEN 1 AND 16384),
    state TEXT NOT NULL CHECK(typeof(state) = 'text' AND state IN ('Enrolled', 'Draining', 'Retired')),
    last_operation_id TEXT NOT NULL
        CHECK(typeof(last_operation_id) = 'text' AND length(CAST(last_operation_id AS BLOB)) = 36 AND instr(last_operation_id, char(0)) = 0),
    last_work_key TEXT NOT NULL
        CHECK(typeof(last_work_key) = 'text' AND length(CAST(last_work_key AS BLOB)) BETWEEN 1 AND 256 AND instr(last_work_key, char(0)) = 0),
    row_identity BLOB NOT NULL CHECK(typeof(row_identity) = 'blob' AND length(row_identity) = 32)
);
CREATE UNIQUE INDEX source_lifecycle_references_active_source
    ON source_lifecycle_references(source_id) WHERE state IN ('Enrolled', 'Draining');
CREATE TABLE source_lifecycle_reference_receipts (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT CHECK(typeof(sequence) = 'integer' AND sequence > 0),
    operation_id TEXT NOT NULL
        CHECK(typeof(operation_id) = 'text' AND length(CAST(operation_id AS BLOB)) = 36 AND instr(operation_id, char(0)) = 0),
    work_key TEXT NOT NULL
        CHECK(typeof(work_key) = 'text' AND length(CAST(work_key AS BLOB)) BETWEEN 1 AND 256 AND instr(work_key, char(0)) = 0),
    payload_identity BLOB NOT NULL CHECK(typeof(payload_identity) = 'blob' AND length(payload_identity) = 32),
    owner_identity TEXT NOT NULL
        CHECK(typeof(owner_identity) = 'text' AND length(CAST(owner_identity AS BLOB)) = 36 AND instr(owner_identity, char(0)) = 0),
    registration_id TEXT NOT NULL
        CHECK(typeof(registration_id) = 'text' AND length(CAST(registration_id AS BLOB)) = 36 AND instr(registration_id, char(0)) = 0),
    before_identity BLOB CHECK(before_identity IS NULL OR (typeof(before_identity) = 'blob' AND length(before_identity) = 32)),
    after_identity BLOB CHECK(after_identity IS NULL OR (typeof(after_identity) = 'blob' AND length(after_identity) = 32)),
    disposition TEXT NOT NULL CHECK(typeof(disposition) = 'text' AND disposition IN ('Enrolled', 'Draining', 'Retired', 'ExpectedRelationChanged')),
    receipt BLOB NOT NULL CHECK(typeof(receipt) = 'blob' AND length(receipt) BETWEEN 1 AND 16384),
    UNIQUE(operation_id, work_key)
);
"#;
const MAX_OBJECTS: usize = 5;
const MAX_SQL_BYTES: usize = 16384;
const MAX_NAME_BYTES: usize = 128;
const MAX_VM_OPS: u32 = 100_000;
const PROGRESS_OPS: i32 = 100;
const MAX_DURATION: Duration = Duration::from_secs(2);

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
struct SchemaObject {
    kind: String,
    name: String,
    table: String,
    sql: Option<String>,
}

fn incompatible() -> LibraryError {
    LibraryError::SourceLifecycleSchemaIncompatible
}

// Row limits bound retained output; the VM/deadline limit also bounds scanning unrelated
// schema objects. CASE prevents an oversized SQL/name field from entering Rust at all.
fn image(connection: &Connection) -> Result<Vec<SchemaObject>, LibraryError> {
    let deadline = Instant::now() + MAX_DURATION;
    let remaining = Arc::new(AtomicU32::new(MAX_VM_OPS));
    let progress_remaining = Arc::clone(&remaining);
    connection.progress_handler(
        PROGRESS_OPS,
        Some(move || {
            let next = progress_remaining
                .load(Ordering::Relaxed)
                .saturating_sub(PROGRESS_OPS as u32);
            progress_remaining.store(next, Ordering::Relaxed);
            next == 0 || Instant::now() >= deadline
        }),
    );
    let result = (|| {
        let mut objects = Vec::with_capacity(MAX_OBJECTS);
        for schema in ["main", "temp"] {
            // schema is a fixed private identifier, never supplied by a caller.
            let query = format!(
                "SELECT
                CASE WHEN length(CAST(type AS BLOB)) <= 16 THEN type END,
                CASE WHEN length(CAST(name AS BLOB)) <= {MAX_NAME_BYTES} THEN name END,
                CASE WHEN length(CAST(tbl_name AS BLOB)) <= {MAX_NAME_BYTES} THEN tbl_name END,
                CASE WHEN sql IS NULL OR length(CAST(sql AS BLOB)) <= {MAX_SQL_BYTES} THEN sql END,
                sql IS NULL OR length(CAST(sql AS BLOB)) <= {MAX_SQL_BYTES}
                FROM {schema}.sqlite_schema
                WHERE name COLLATE NOCASE IN (?1, ?2, ?3)
                   OR tbl_name COLLATE NOCASE IN (?1, ?2)
                LIMIT 6"
            );
            let mut statement = connection.prepare(&query)?;
            let mut rows = statement.query([
                "source_lifecycle_references",
                "source_lifecycle_reference_receipts",
                "source_lifecycle_references_active_source",
            ])?;
            while let Some(row) = rows.next()? {
                if schema != "main"
                    || objects.len() == MAX_OBJECTS
                    || Instant::now() >= deadline
                    || remaining.load(Ordering::Relaxed) == 0
                    || !row.get::<_, bool>(4)?
                {
                    return Err(incompatible());
                }
                let kind = row.get::<_, Option<String>>(0)?.ok_or_else(incompatible)?;
                let name = row.get::<_, Option<String>>(1)?.ok_or_else(incompatible)?;
                let table = row.get::<_, Option<String>>(2)?.ok_or_else(incompatible)?;
                let sql = row.get(3)?;
                objects.push(SchemaObject {
                    kind,
                    name,
                    table,
                    sql,
                });
            }
        }
        if Instant::now() >= deadline || remaining.load(Ordering::Relaxed) == 0 {
            return Err(LibraryError::SourceLifecycleSchemaBudget);
        }
        objects.sort();
        Ok(objects)
    })();
    connection.progress_handler(0, Option::<fn() -> bool>::None);
    match result {
        Err(LibraryError::Sql(error))
            if error.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted) =>
        {
            Err(LibraryError::SourceLifecycleSchemaBudget)
        }
        other => other,
    }
}

fn expected_image() -> Result<Vec<SchemaObject>, LibraryError> {
    let reference = Connection::open_in_memory()?;
    reference.execute_batch(DDL_V1)?;
    image(&reference)
}

fn validate(connection: &Connection, expected: &[SchemaObject]) -> Result<bool, LibraryError> {
    let actual = image(connection)?;
    if actual.is_empty() {
        Ok(false)
    } else if actual == expected {
        Ok(true)
    } else {
        Err(incompatible())
    }
}

impl LibraryDatabase {
    pub(super) fn install_source_lifecycle_schema(&mut self) -> Result<(), LibraryError> {
        install(&mut self.connection)
    }
}

fn install(connection: &mut crate::sqlite_budget::OwnedConnection) -> Result<(), LibraryError> {
    let expected = expected_image()?;
    // This happens before pragmas, optional extensions and all other migrations.
    if validate(connection, &expected)? {
        return Ok(());
    }
    // Recheck with writer exclusion before installing. The transaction drops/rolls back
    // on any failure; no partial repair, evidence eviction or alternate writer follows.
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !validate(&transaction, &expected)? {
        transaction.execute_batch(DDL_V1)?;
        if !validate(&transaction, &expected)? {
            return Err(incompatible());
        }
    }
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests;
