//! Actual bounded immutable SQLite observations, not dispatch or settlement authority.
use super::{
    JournalInsertWriteStage, OperationJournalStore, OperationJournalStoreError, database_claims,
};
use crate::operation_journal::database_attempts::{
    AUDIT_PAGE_ROWS, DatabaseAttemptOutcome, DatabaseAttemptStart, MAX_ATTEMPTS,
    MAX_ATTEMPTS_PER_OPERATION, MAX_OUTCOME_BYTES, MAX_START_BYTES,
};
use crate::operation_journal::database_claims::DatabaseClaimInventory;
use crate::operation_journal::{OperationId, StoredOperationRecord};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

mod scalar_projection;
use scalar_projection::UuidProjection;
#[cfg(test)]
thread_local! {
    pub(super) static SCALAR_COPIED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(super) const START_TABLE: &str = "operation_journal_database_attempt";
pub(super) const OUTCOME_TABLE: &str = "operation_journal_database_attempt_outcome";
pub(super) const CREATE_START:&str="CREATE TABLE operation_journal_database_attempt (
    attempt_id TEXT NOT NULL PRIMARY KEY CHECK(length(attempt_id)=36),
    operation_id TEXT NOT NULL REFERENCES operation_journal(operation_id) CHECK(length(operation_id)=36),
    binding_id TEXT NOT NULL CHECK(length(binding_id)=36),
    start_nonce TEXT NOT NULL UNIQUE CHECK(length(start_nonce)=36),
    database_slice TEXT NOT NULL CHECK(length(database_slice)=36),
    sidecar_slice TEXT NOT NULL CHECK(length(sidecar_slice)=36),
    database_bytes INTEGER NOT NULL CHECK(typeof(database_bytes)='integer' AND database_bytes>0),
    wal_bytes INTEGER NOT NULL CHECK(typeof(wal_bytes)='integer' AND wal_bytes>=0),
    shm_bytes INTEGER NOT NULL CHECK(typeof(shm_bytes)='integer' AND shm_bytes>=0),
    image BLOB NOT NULL CHECK(typeof(image)='blob' AND length(image) BETWEEN 1 AND 2097152),
    UNIQUE(operation_id,attempt_id),
    FOREIGN KEY(operation_id,binding_id) REFERENCES operation_journal_database_binding(operation_id,binding_id),
    FOREIGN KEY(operation_id,database_slice) REFERENCES operation_journal_database_claim_slice(operation_id,slice_id),
    FOREIGN KEY(operation_id,sidecar_slice) REFERENCES operation_journal_database_claim_slice(operation_id,slice_id)
)";
pub(super) const CREATE_OUTCOME:&str="CREATE TABLE operation_journal_database_attempt_outcome (
    attempt_id TEXT NOT NULL PRIMARY KEY CHECK(length(attempt_id)=36),
    operation_id TEXT NOT NULL CHECK(length(operation_id)=36),
    observation_id TEXT NOT NULL UNIQUE CHECK(length(observation_id)=36),
    start_digest BLOB NOT NULL CHECK(typeof(start_digest)='blob' AND length(start_digest)=32),
    image BLOB NOT NULL CHECK(typeof(image)='blob' AND length(image) BETWEEN 1 AND 16384),
    FOREIGN KEY(operation_id,attempt_id) REFERENCES operation_journal_database_attempt(operation_id,attempt_id)
)";
pub(super) fn create(c: &Connection) -> Result<()> {
    c.execute_batch(CREATE_START)?;
    c.execute_batch(CREATE_OUTCOME)?;
    Ok(())
}
fn error(s: impl ToString) -> OperationJournalStoreError {
    database_claims::error(format!("attempt: {}", s.to_string()))
}
type Result<T> = std::result::Result<T, OperationJournalStoreError>;

/// Storage acknowledgement only. Exact replay never recreates dispatch permission.
#[derive(Debug)]
#[allow(dead_code)] // Storage observations consumed by the private issuer in OPT-2168.
pub(crate) struct AttemptStorageObservation {
    pub(crate) replay: bool,
}
/// Retains the full image on uncertain boundaries. The future issuer must keep its ordinary
/// capacity custody and this extension exclusion until the original connection quiesces.
#[derive(Debug)]
#[allow(dead_code)] // Owner-thread uncertainty custody consumed by OPT-2168.
pub(crate) struct AttemptStorageFailure {
    pub(crate) stage: JournalInsertWriteStage,
    pub(crate) source: OperationJournalStoreError,
    pub(crate) retained_start: Option<DatabaseAttemptStart>,
    pub(crate) rollback_error: Option<Box<OperationJournalStoreError>>,
    _extension_exclusion: Option<std::sync::RwLockReadGuard<'static, ()>>,
}
fn failure(
    stage: JournalInsertWriteStage,
    source: OperationJournalStoreError,
    start: Option<DatabaseAttemptStart>,
    rollback_error: Option<Box<OperationJournalStoreError>>,
    scope: Option<std::sync::RwLockReadGuard<'static, ()>>,
) -> AttemptStorageFailure {
    AttemptStorageFailure {
        stage,
        source,
        retained_start: start,
        rollback_error,
        _extension_exclusion: scope,
    }
}
fn count(c: &Connection, table: &str, operation: Option<Uuid>) -> Result<usize> {
    let n: i64 = if let Some(op) = operation {
        c.query_row(
            &format!("SELECT COUNT(*) FROM (SELECT 1 FROM {table} WHERE operation_id=?1 LIMIT ?2)"),
            params![op.to_string(), MAX_ATTEMPTS_PER_OPERATION as i64 + 1],
            |r| r.get(0),
        )?
    } else {
        c.query_row(
            &format!("SELECT COUNT(*) FROM (SELECT 1 FROM {table} LIMIT ?1)"),
            [MAX_ATTEMPTS as i64 + 1],
            |r| r.get(0),
        )?
    };
    let cap = if operation.is_some() {
        MAX_ATTEMPTS_PER_OPERATION
    } else {
        MAX_ATTEMPTS
    };
    if n < 0 || n > cap as i64 {
        return Err(error("retained row ceiling"));
    }
    Ok(n as usize)
}
fn read_start(c: &Connection, id: Uuid) -> Result<Option<DatabaseAttemptStart>> {
    type Row = (
        UuidProjection,
        UuidProjection,
        UuidProjection,
        UuidProjection,
        UuidProjection,
        u64,
        u64,
        u64,
        Option<Vec<u8>>,
    );
    let row:Option<Row>=c.query_row(
        "SELECT CASE WHEN length(operation_id)=36 THEN operation_id END,
         CASE WHEN length(binding_id)=36 THEN binding_id END,CASE WHEN length(start_nonce)=36 THEN start_nonce END,
         CASE WHEN length(database_slice)=36 THEN database_slice END,CASE WHEN length(sidecar_slice)=36 THEN sidecar_slice END,
         database_bytes,wal_bytes,shm_bytes,
         CASE WHEN typeof(image)='blob' AND length(image) BETWEEN 1 AND ?2 THEN image END
         FROM operation_journal_database_attempt WHERE attempt_id=?1",
        params![id.to_string(),MAX_START_BYTES as i64],|r|Ok((scalar_projection::required_uuid(r,0)?,scalar_projection::required_uuid(r,1)?,scalar_projection::required_uuid(r,2)?,scalar_projection::required_uuid(r,3)?,scalar_projection::required_uuid(r,4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?))).optional()?;
    let Some((op, binding, nonce, db, sidecar, db_bytes, wal, shm, image)) = row else {
        return Ok(None);
    };
    let start =
        DatabaseAttemptStart::decode(&image.ok_or_else(|| error("start image type/length"))?)
            .map_err(error)?;
    if start.attempt != id
        || !op.matches(start.operation)
        || !binding.matches(start.binding)
        || !nonce.matches(start.nonce)
        || !db.matches(start.database_slice)
        || !sidecar.matches(start.sidecar_slice)
        || start.database_bytes != db_bytes
        || start.wal_bytes != wal
        || start.shm_bytes != shm
    {
        return Err(error("start columns differ from canonical image"));
    }
    Ok(Some(start))
}
fn read_outcome(
    c: &Connection,
    start: &DatabaseAttemptStart,
) -> Result<Option<DatabaseAttemptOutcome>> {
    let row: Option<(
        UuidProjection,
        UuidProjection,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
    )> = c
        .query_row(
            "SELECT CASE WHEN length(operation_id)=36 THEN operation_id END,
         CASE WHEN length(observation_id)=36 THEN observation_id END,
         CASE WHEN typeof(start_digest)='blob' AND length(start_digest)=32 THEN start_digest END,
         CASE WHEN typeof(image)='blob' AND length(image) BETWEEN 1 AND ?2 THEN image END
         FROM operation_journal_database_attempt_outcome WHERE attempt_id=?1",
            params![start.attempt.to_string(), MAX_OUTCOME_BYTES as i64],
            |r| {
                Ok((
                    scalar_projection::required_uuid(r, 0)?,
                    scalar_projection::required_uuid(r, 1)?,
                    r.get(2)?,
                    r.get(3)?,
                ))
            },
        )
        .optional()?;
    let Some((op, observation, digest, image)) = row else {
        return Ok(None);
    };
    let outcome =
        DatabaseAttemptOutcome::decode(&image.ok_or_else(|| error("outcome image type/length"))?)
            .map_err(error)?;
    outcome.validate(start).map_err(error)?;
    if !op.matches(start.operation)
        || !observation.matches(outcome.observation)
        || digest.as_deref() != Some(outcome.start_digest.as_slice())
    {
        return Err(error("outcome columns differ from image"));
    }
    Ok(Some(outcome))
}

#[derive(Default)]
struct Audit {
    physical: BTreeSet<(u64, String)>,
    charged: BTreeMap<(Uuid, Uuid), u64>,
    operations: BTreeMap<Uuid, usize>,
}
impl OperationJournalStore {
    fn attempt_context(
        &self,
        start: &DatabaseAttemptStart,
        new_start: bool,
    ) -> Result<(
        StoredOperationRecord,
        database_claims::RetainedDatabaseInventory,
    )> {
        self.attempt_context_with_input(start, new_start, None)
    }

    fn attempt_context_with_input(
        &self,
        start: &DatabaseAttemptStart,
        new_start: bool,
        input: Option<&crate::operation_journal::owner::InputPermit>,
    ) -> Result<(
        StoredOperationRecord,
        database_claims::RetainedDatabaseInventory,
    )> {
        let operation = OperationId::from_uuid(start.operation);
        let inventory = database_claims::read_with_input(&self.connection, operation, input)?
            .ok_or_else(|| error("missing actual binding"))?;
        self.validate_database_claim_operation(operation, &inventory)?;
        let record = self
            .read_row(operation)?
            .ok_or_else(|| error("orphan start"))?
            .record;
        self.validate_database_attempt_journal_claims_with_input(&record, input)?;
        start
            .validate_bound(&record, &inventory, new_start)
            .map_err(error)?;
        Ok((record, inventory))
    }
    fn audit_attempts(&self) -> Result<Audit> {
        self.audit_attempts_with_input(None)
    }

    fn audit_attempts_with_input(
        &self,
        input: Option<&crate::operation_journal::owner::InputPermit>,
    ) -> Result<Audit> {
        count(&self.connection, START_TABLE, None)?;
        count(&self.connection, OUTCOME_TABLE, None)?;
        let mut audit = Audit::default();
        for table in [START_TABLE, OUTCOME_TABLE] {
            let bad:bool=self.connection.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE typeof(attempt_id)!='text' OR length(attempt_id)!=36)"),[],|r|r.get(0))?;
            if bad {
                return Err(error("audit key type/length"));
            }
            let mut after = String::new();
            loop {
                let mut statement=self.connection.prepare(&format!("SELECT CASE WHEN typeof(attempt_id)='text' AND length(attempt_id)=36 THEN attempt_id END FROM {table} WHERE attempt_id>?1 ORDER BY attempt_id LIMIT ?2"))?;
                let page = statement
                    .query_map(params![after, AUDIT_PAGE_ROWS as i64], |r| {
                        scalar_projection::nullable_uuid(r, 0)
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                if page.is_empty() {
                    break;
                }
                for text in page {
                    let text = text.ok_or_else(|| error("audit key shape"))?;
                    let id = text.id.map_err(error)?;
                    if id.is_nil() || !text.canonical {
                        return Err(error("audit key identity"));
                    }
                    let start =
                        read_start(&self.connection, id)?.ok_or_else(|| error("orphan outcome"))?;
                    let (_, inventory) = self.attempt_context_with_input(&start, false, input)?;
                    read_outcome(&self.connection, &start)?;
                    if table == START_TABLE {
                        let binding = inventory
                            .bindings()
                            .iter()
                            .find(|b| b.id == start.binding)
                            .ok_or_else(|| error("binding disappeared"))?;
                        // No outcome observation supplies genuine settlement authority. Every
                        // retained start remains unresolved, across operations and restarts.
                        if !audit
                            .physical
                            .insert((binding.volume, binding.main_file.clone()))
                        {
                            return Err(error("overlapping unresolved physical database attempts"));
                        }
                        let n = audit.operations.entry(start.operation).or_default();
                        *n += 1;
                        if *n > MAX_ATTEMPTS_PER_OPERATION {
                            return Err(error("operation attempt ceiling"));
                        }
                        let sidecar = start
                            .wal_bytes
                            .checked_add(start.shm_bytes)
                            .ok_or_else(|| error("sidecar charge overflow"))?;
                        for (slice, charge) in [
                            (start.database_slice, start.database_bytes),
                            (start.sidecar_slice, sidecar),
                        ] {
                            let ceiling = inventory
                                .slices()
                                .iter()
                                .find(|s| s.id == slice)
                                .ok_or_else(|| error("slice disappeared"))?
                                .ceiling;
                            let spent = audit.charged.entry((start.operation, slice)).or_default();
                            *spent = spent
                                .checked_add(charge)
                                .ok_or_else(|| error("conserved ceiling overflow"))?;
                            if *spent > ceiling {
                                return Err(error("retained charge exceeds slice"));
                            }
                        }
                    }
                    after = id.to_string();
                }
            }
        }
        Ok(audit)
    }
    #[allow(dead_code)] // Preserve the original unfunded entry point.
    pub(super) fn validate_database_attempts_inside_transaction(&self) -> Result<()> {
        self.validate_database_attempts_inside_transaction_with_input(None)
    }

    pub(super) fn validate_database_attempts_inside_transaction_with_input(
        &self,
        input: Option<&crate::operation_journal::owner::InputPermit>,
    ) -> Result<()> {
        self.audit_attempts_with_input(input).map(|_| ())
    }

    /// Point readback is observation only, including exact nonce response-loss recovery.
    #[allow(dead_code)]
    pub(crate) fn read_database_attempt(
        &self,
        id: Uuid,
    ) -> Result<Option<(DatabaseAttemptStart, Option<DatabaseAttemptOutcome>)>> {
        if !self.connection.is_autocommit() {
            return Err(error(
                "unconfirmed transaction cannot provide durable attempt readback",
            ));
        }
        self.validate_namespace()?;
        super::validate_existing_schema(
            &self.connection,
            &self.database_path,
            super::JOURNAL_SCHEMA_VERSION,
        )?;
        self.audit_attempts()?;
        let value = if let Some(start) = read_start(&self.connection, id)? {
            let outcome = read_outcome(&self.connection, &start)?;
            Some((start, outcome))
        } else {
            None
        };
        self.validate_namespace()?;
        Ok(value)
    }
    #[allow(dead_code)]
    pub(crate) fn read_database_attempt_for_nonce(
        &self,
        nonce: Uuid,
        expected_digest: [u8; 32],
    ) -> Result<Option<(DatabaseAttemptStart, Option<DatabaseAttemptOutcome>)>> {
        if nonce.is_nil() || !self.connection.is_autocommit() {
            return Err(error("invalid nonce/readback state"));
        }
        self.validate_namespace()?;
        let text:Option<Option<UuidProjection>>=self.connection.query_row("SELECT CASE WHEN typeof(attempt_id)='text' AND length(attempt_id)=36 THEN attempt_id END FROM operation_journal_database_attempt WHERE start_nonce=?1",[nonce.to_string()],|r|scalar_projection::nullable_uuid(r,0)).optional()?;
        let Some(text) = text else {
            self.audit_attempts()?;
            self.validate_namespace()?;
            return Ok(None);
        };
        let id = text
            .ok_or_else(|| error("nonce attempt shape"))?
            .id
            .map_err(error)?;
        let value = self
            .read_database_attempt(id)?
            .ok_or_else(|| error("nonce start vanished"))?;
        if value.0.nonce != nonce || value.0.digest().map_err(error)? != expected_digest {
            return Err(error("nonce digest conflict"));
        }
        Ok(Some(value))
    }

    /// Actual atomic metadata mutation for the private future issuer. It can never issue a ticket.
    #[allow(dead_code)]
    pub(crate) fn persist_database_attempt_start(
        &self,
        start: &DatabaseAttemptStart,
    ) -> std::result::Result<AttemptStorageObservation, AttemptStorageFailure> {
        let bytes = start.encode().map_err(|e| {
            failure(
                JournalInsertWriteStage::NoMutationAttempted,
                error(e),
                None,
                None,
                None,
            )
        })?;
        let scope = self
            .prepare_database_attempt_metadata(OperationId::from_uuid(start.operation))
            .map_err(|e| {
                failure(
                    JournalInsertWriteStage::NoMutationAttempted,
                    e,
                    None,
                    None,
                    None,
                )
            })?;
        let result = (|| -> Result<bool> {
            if let Some(old) = read_start(&self.connection, start.attempt)? {
                self.audit_attempts()?;
                if old != *start {
                    return Err(error("immutable start replay conflict"));
                }
                return Ok(true);
            }
            self.attempt_context(start, true)?;
            if count(&self.connection, START_TABLE, None)? == MAX_ATTEMPTS
                || count(&self.connection, START_TABLE, Some(start.operation))?
                    == MAX_ATTEMPTS_PER_OPERATION
            {
                return Err(error("attempt relation exhausted"));
            }
            Ok(false)
        })();
        match result {
            Ok(true) => return Ok(AttemptStorageObservation { replay: true }),
            Err(e) => {
                return Err(failure(
                    JournalInsertWriteStage::NoMutationAttempted,
                    e,
                    None,
                    None,
                    None,
                ));
            }
            Ok(false) => {}
        }
        self.write_attempt_image(start,&scope,||{
            let audit=self.audit_attempts()?;
            let inventory=DatabaseClaimInventory::decode(&start.inventory).map_err(error)?;
            let b=inventory.bindings().iter().find(|b|b.id==start.binding).ok_or_else(||error("binding disappeared"))?;
            if audit.physical.contains(&(b.volume,b.main_file.clone())){return Err(error("unresolved attempt retains original full reservation"));}
            for (id,amount) in [(start.database_slice,start.database_bytes),(start.sidecar_slice,start.wal_bytes.checked_add(start.shm_bytes).ok_or_else(||error("reservation overflow"))?)] {
                let ceiling=inventory.slices().iter().find(|s|s.id==id).ok_or_else(||error("slice disappeared"))?.ceiling;
                if audit.charged.get(&(start.operation,id)).copied().unwrap_or(0).checked_add(amount).is_none_or(|n|n>ceiling){return Err(error("conserved remaining ceiling"));}
            }
            self.connection.execute("INSERT INTO operation_journal_database_attempt(attempt_id,operation_id,binding_id,start_nonce,database_slice,sidecar_slice,database_bytes,wal_bytes,shm_bytes,image) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",params![start.attempt.to_string(),start.operation.to_string(),start.binding.to_string(),start.nonce.to_string(),start.database_slice.to_string(),start.sidecar_slice.to_string(),start.database_bytes as i64,start.wal_bytes as i64,start.shm_bytes as i64,bytes])?;
            if read_start(&self.connection,start.attempt)?.as_ref()!=Some(start){return Err(error("start exact readback"));}
            self.audit_attempts()?;Ok(())
        }).map_err(|mut e|{if !self.connection.is_autocommit() {e._extension_exclusion=Some(scope);}e})?;
        self.read_database_attempt(start.attempt)
            .map_err(|e| {
                failure(
                    JournalInsertWriteStage::CommittedButNotAcknowledged,
                    e,
                    Some(start.clone()),
                    None,
                    None,
                )
            })?
            .filter(|v| v.0 == *start)
            .ok_or_else(|| {
                failure(
                    JournalInsertWriteStage::CommittedButNotAcknowledged,
                    error("committed start disappeared"),
                    Some(start.clone()),
                    None,
                    None,
                )
            })?;
        Ok(AttemptStorageObservation { replay: false })
    }

    #[allow(dead_code)]
    pub(crate) fn persist_database_attempt_outcome(
        &self,
        outcome: &DatabaseAttemptOutcome,
    ) -> std::result::Result<AttemptStorageObservation, AttemptStorageFailure> {
        let read = self.read_database_attempt(outcome.attempt).map_err(|e| {
            failure(
                JournalInsertWriteStage::NoMutationAttempted,
                e,
                None,
                None,
                None,
            )
        })?;
        let (start, old) = read.ok_or_else(|| {
            failure(
                JournalInsertWriteStage::NoMutationAttempted,
                error("outcome without durable start"),
                None,
                None,
                None,
            )
        })?;
        outcome.validate(&start).map_err(|e| {
            failure(
                JournalInsertWriteStage::NoMutationAttempted,
                error(e),
                None,
                None,
                None,
            )
        })?;
        if let Some(old) = old {
            return if old == *outcome {
                Ok(AttemptStorageObservation { replay: true })
            } else {
                Err(failure(
                    JournalInsertWriteStage::NoMutationAttempted,
                    error("immutable outcome replay conflict"),
                    None,
                    None,
                    None,
                ))
            };
        }
        let scope = self
            .prepare_database_attempt_metadata(OperationId::from_uuid(start.operation))
            .map_err(|e| {
                failure(
                    JournalInsertWriteStage::NoMutationAttempted,
                    e,
                    Some(start.clone()),
                    None,
                    None,
                )
            })?;
        let bytes = outcome.encode().map_err(|e| {
            failure(
                JournalInsertWriteStage::NoMutationAttempted,
                error(e),
                Some(start.clone()),
                None,
                None,
            )
        })?;
        self.write_attempt_image(&start,&scope,||{
            self.audit_attempts()?;
            self.connection.execute("INSERT INTO operation_journal_database_attempt_outcome(attempt_id,operation_id,observation_id,start_digest,image) VALUES(?1,?2,?3,?4,?5)",params![start.attempt.to_string(),start.operation.to_string(),outcome.observation.to_string(),outcome.start_digest.as_slice(),bytes])?;
            if read_outcome(&self.connection,&start)?.as_ref()!=Some(outcome){return Err(error("outcome exact readback"));}Ok(())
        }).map_err(|mut e|{if !self.connection.is_autocommit(){e._extension_exclusion=Some(scope);}e})?;
        let actual = self.read_database_attempt(start.attempt).map_err(|e| {
            failure(
                JournalInsertWriteStage::CommittedButNotAcknowledged,
                e,
                Some(start.clone()),
                None,
                None,
            )
        })?;
        if actual.as_ref().and_then(|v| v.1.as_ref()) != Some(outcome) {
            return Err(failure(
                JournalInsertWriteStage::CommittedButNotAcknowledged,
                error("outcome lost readback"),
                Some(start),
                None,
                None,
            ));
        }
        Ok(AttemptStorageObservation { replay: false })
    }

    fn write_attempt_image(
        &self,
        start: &DatabaseAttemptStart,
        _scope: &std::sync::RwLockReadGuard<'static, ()>,
        write: impl FnOnce() -> Result<()>,
    ) -> std::result::Result<(), AttemptStorageFailure> {
        #[cfg(test)]
        if let Some(super::InsertTestDirective::Fail(e)) =
            self.insert_test_directive(super::InsertTestBoundary::BeforeBegin)
        {
            return Err(failure(
                JournalInsertWriteStage::NoMutationAttempted,
                e,
                None,
                None,
                None,
            ));
        }
        self.connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| {
                failure(
                    JournalInsertWriteStage::NoMutationAttempted,
                    e.into(),
                    None,
                    None,
                    None,
                )
            })?;
        let result = (|| {
            write()?;
            #[cfg(test)]
            if let Some(
                super::InsertTestDirective::Fail(e)
                | super::InsertTestDirective::FailAndLeaveForRollback(e),
            ) = self.insert_test_directive(super::InsertTestBoundary::BeforePrecommitValidation)
            {
                return Err(e);
            }
            self.validate_namespace_for_precommit()
        })();
        if let Err(e) = result {
            let rollback = self.rollback_insert_transaction();
            let uncertain = rollback.is_some() || !self.connection.is_autocommit();
            return Err(failure(
                if uncertain {
                    JournalInsertWriteStage::RollbackNotConfirmed
                } else {
                    JournalInsertWriteStage::RolledBackBeforeCommit
                },
                e,
                if uncertain { Some(start.clone()) } else { None },
                rollback,
                None,
            ));
        }
        #[cfg(test)]
        match self.insert_test_directive(super::InsertTestBoundary::Commit) {
            Some(super::InsertTestDirective::ExecuteThenFail(e)) => {
                let result = self.connection.execute_batch("COMMIT");
                let rollback = self.rollback_insert_transaction();
                return Err(failure(
                    JournalInsertWriteStage::CommitOutcomeUnknown,
                    result.err().map(Into::into).unwrap_or(e),
                    Some(start.clone()),
                    rollback,
                    None,
                ));
            }
            Some(
                super::InsertTestDirective::Fail(e)
                | super::InsertTestDirective::FailAndLeaveForRollback(e),
            ) => {
                let rollback = self.rollback_insert_transaction();
                return Err(failure(
                    JournalInsertWriteStage::CommitOutcomeUnknown,
                    e,
                    Some(start.clone()),
                    rollback,
                    None,
                ));
            }
            _ => {}
        }
        if let Err(e) = self.connection.execute_batch("COMMIT") {
            let rollback = self.rollback_insert_transaction();
            return Err(failure(
                JournalInsertWriteStage::CommitOutcomeUnknown,
                e.into(),
                Some(start.clone()),
                rollback,
                None,
            ));
        }
        #[cfg(test)]
        if let Some(super::InsertTestDirective::Fail(e)) =
            self.insert_test_directive(super::InsertTestBoundary::AfterCommit)
        {
            return Err(failure(
                JournalInsertWriteStage::CommittedButNotAcknowledged,
                e,
                Some(start.clone()),
                None,
                None,
            ));
        }
        self.validate_namespace().map_err(|e| {
            failure(
                JournalInsertWriteStage::CommittedButNotAcknowledged,
                e,
                Some(start.clone()),
                None,
                None,
            )
        })
    }
}
