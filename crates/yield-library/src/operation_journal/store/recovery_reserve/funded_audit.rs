//! Complete recovery history on the original admission connection and ordinary pool.
use super::*;
use crate::operation_journal::owner::InputPermit;
use crate::operation_journal::store::historical_claim_observation::borrowed_sql::Statement;
use crate::operation_journal::store::{OperationJournalStoreError, audit_error};
use rusqlite::types::{FromSql, FromSqlError, ValueRef};
use std::cell::RefCell;
use std::mem::size_of;
use std::path::Path;

// Two charge UUIDs, two devices, purpose and state; a simultaneous transition,
// prior state and canonical UUID/device validation renderings. No history-sized graph.
const ROW_TEXT_BYTES: usize = 2 * 36 + 2 * 20 + 17 + 15 + 36 + 2 * 15 + 15 + 3 * 36 + 20;
// Invalid closed fields are rejected before projection. This names the longest
// semantic diagnostic plus escaping of the largest allowed closed field and
// every surrounding ledger/admission prefix, including geometric formatting growth.
const DIAGNOSTIC_BYTES: usize = 4 * (
    "recovery reserve admission audit failed: recovery-reserve record is corrupt: recovery-reserve ledger requires audit: release observation exceeds unreleased spend or increases budget".len()
    + 4 * 36
    + "charge  remains active in state ReleaseObserved".len()
);
const FIXED_BYTES: usize = 3 * size_of::<Statement<'static>>()
    + size_of::<(
        String,
        String,
        Option<String>,
        String,
        String,
        i64,
        i64,
        i64,
        i64,
        String,
        i64,
    )>()
    + size_of::<TransitionRow>()
    + size_of::<TransitionValidation<'static>>()
    + 2 * size_of::<OperationJournalStoreError>()
    + size_of::<LedgerError>()
    + 3 * size_of::<InputPermit>()
    + size_of::<RefCell<Option<InputPermit>>>()
    + 2 * ROW_TEXT_BYTES
    + DIAGNOSTIC_BYTES;

/// The owned diagnostic always drops before its original debit, including unwind.
/// Component callers retain this wrapper; only a real reply consumer can detach it.
struct FundedFailure {
    error: OperationJournalStoreError,
    fixed: InputPermit,
    native: Option<InputPermit>,
}
impl FundedFailure {
    fn into_original_reply(self, input: &InputPermit) -> OperationJournalStoreError {
        if !input.has_historical_error_consumer() {
            // An isolated component input has no retained reply backing. Do not let
            // an owned diagnostic escape its debit through that compatibility seam.
            return OperationJournalStoreError::HistoricalClaimResourceBound;
        }
        let Self {
            error,
            fixed,
            native,
        } = self;
        input.retain_historical_error(fixed, None, None, native);
        error
    }
}

pub(in crate::operation_journal::store) fn prove_no_active_charges_for_input(
    connection: &Connection,
    path: &Path,
    input: &InputPermit,
) -> Result<(), OperationJournalStoreError> {
    audit(connection, path, input).map_err(|error| match error {
        AuditFailure::Unfunded(error) => error,
        AuditFailure::Funded(error) => error.into_original_reply(input),
    })
}
enum AuditFailure {
    Unfunded(OperationJournalStoreError),
    Funded(FundedFailure),
}
fn audit(connection: &Connection, path: &Path, input: &InputPermit) -> Result<(), AuditFailure> {
    // Exact original PathBuf clone (n) plus geometric String growth (2 * 3n)
    // for invalid-UTF8 Path::display in the original owner error consumer.
    let bytes = path
        .as_os_str()
        .as_encoded_bytes()
        .len()
        .checked_mul(7)
        .and_then(|n| n.checked_add(FIXED_BYTES))
        .ok_or(AuditFailure::Unfunded(
            OperationJournalStoreError::HistoricalClaimResourceBound,
        ))?;
    let fixed = input
        .reserve_historical_scratch(bytes)
        .map_err(AuditFailure::Unfunded)?;
    let native = RefCell::new(None);
    let result = run(connection, input, &native);
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let error = match error {
                AuditError::Semantic(error) => audit_error(
                    path,
                    format!("recovery reserve admission audit failed: {error}"),
                ),
                AuditError::Sql(error) => match error {
                    OperationJournalStoreError::HistoricalClaimResourceBound => error,
                    _ => audit_error(
                        path,
                        format!("recovery reserve admission audit failed: {error}"),
                    ),
                },
            };
            Err(AuditFailure::Funded(FundedFailure {
                error,
                fixed,
                native: native.into_inner(),
            }))
        }
    }
}
enum AuditError {
    Semantic(LedgerError),
    Sql(OperationJournalStoreError),
}
impl From<OperationJournalStoreError> for AuditError {
    fn from(error: OperationJournalStoreError) -> Self {
        Self::Sql(error)
    }
}
impl From<LedgerError> for AuditError {
    fn from(error: LedgerError) -> Self {
        Self::Semantic(error)
    }
}
fn corrupt_field() -> AuditError {
    LedgerError::Corrupt("recovery audit closed field is invalid".into()).into()
}
fn scalar<T: FromSql>(statement: &Statement<'_>, index: i32) -> Result<T, AuditError> {
    let value = statement.get_ref(index)?;
    T::column_result(value).map_err(|error| {
        let error = match error {
            FromSqlError::InvalidType => rusqlite::Error::InvalidColumnType(
                index as usize,
                "recovery audit column".into(),
                value.data_type(),
            ),
            FromSqlError::OutOfRange(value) => {
                rusqlite::Error::IntegralValueOutOfRange(index as usize, value)
            }
            other => rusqlite::Error::FromSqlConversionFailure(
                index as usize,
                value.data_type(),
                Box::new(other),
            ),
        };
        AuditError::Sql(error.into())
    })
}
fn text(statement: &Statement<'_>, index: i32, maximum: usize) -> Result<String, AuditError> {
    if let ValueRef::Text(bytes) = statement.get_ref(index)? {
        if bytes.len() > maximum {
            return Err(corrupt_field());
        }
        let value = std::str::from_utf8(bytes).map_err(|_| corrupt_field())?;
        let valid = match maximum {
            36 => Uuid::parse_str(value).ok().is_some_and(|id| {
                let mut buffer = [0; 36];
                id.hyphenated().encode_lower(&mut buffer) == value
            }),
            20 => {
                !value.is_empty()
                    && (value.len() == 1 || !value.starts_with('0'))
                    && value.bytes().all(|byte| byte.is_ascii_digit())
                    && value.parse::<u64>().is_ok()
            }
            17 => PURPOSES.contains(&value),
            15 => CHARGE_KINDS.contains(&value),
            _ => unreachable!("closed recovery field"),
        };
        if !valid {
            return Err(corrupt_field());
        }
    }
    scalar(statement, index)
}
fn optional_device(statement: &Statement<'_>, index: i32) -> Result<Option<String>, AuditError> {
    if statement.get_ref(index)? == ValueRef::Null {
        Ok(None)
    } else {
        text(statement, index, 20).map(Some)
    }
}
fn run(
    connection: &Connection,
    input: &InputPermit,
    native: &RefCell<Option<InputPermit>>,
) -> Result<(), AuditError> {
    {
        let mut orphan = Statement::new(connection, input, native, c"SELECT transition_id FROM operation_journal_recovery_reserve_transition AS transition LEFT JOIN operation_journal_recovery_reserve AS charge ON charge.charge_id = transition.charge_id WHERE charge.charge_id IS NULL ORDER BY transition.charge_id, transition.transition_sequence LIMIT 1")?;
        if orphan.next()? {
            let id = text(&orphan, 0, 36)?;
            return Err(
                LedgerError::Corrupt(format!("orphan recovery-reserve transition {id}")).into(),
            );
        }
    }
    let mut charges = Statement::new(connection, input, native, c"SELECT charge_id, volume_device, active_volume_device, operation_id, purpose, budget_bytes, margin_bytes, precharge_capacity_bytes, remaining_bytes, state, transition_sequence FROM operation_journal_recovery_reserve ORDER BY charge_id")?;
    while charges.next()? {
        let row = validate_charge_row(
            text(&charges, 0, 36)?,
            text(&charges, 1, 20)?,
            optional_device(&charges, 2)?,
            text(&charges, 3, 36)?,
            text(&charges, 4, 17)?,
            scalar(&charges, 5)?,
            scalar(&charges, 6)?,
            scalar(&charges, 7)?,
            scalar(&charges, 8)?,
            text(&charges, 9, 15)?,
            scalar(&charges, 10)?,
        )
        .map_err(|error| AuditError::Semantic(LedgerError::Corrupt(error)))?;
        let (charge, _, _, operation, budget, _, _, remaining, state, sequence) = row;
        let operation_id = OperationId::parse(&operation).map_err(|_| corrupt_field())?;
        {
            let mut exists = Statement::new(connection, input, native, c"SELECT EXISTS(SELECT 1 FROM operation_journal WHERE operation_id = ?1 UNION ALL SELECT 1 FROM operation_journal_full_progress WHERE operation_id = ?1)")?;
            exists.bind_text(1, &operation)?;
            if !exists.next()? || !scalar::<bool>(&exists, 0)? {
                return Err(LedgerError::OperationNotFound(operation_id).into());
            }
        }
        let mut transitions = Statement::new(connection, input, native, c"SELECT transition_id, transition_sequence, kind, amount_bytes, remaining_bytes, state FROM operation_journal_recovery_reserve_transition WHERE charge_id = ?1 ORDER BY transition_sequence")?;
        transitions.bind_text(1, &charge.to_string())?;
        let mut history = TransitionValidation::new(budget, remaining, &state, sequence);
        while transitions.next()? {
            let row = (
                text(&transitions, 0, 36)?,
                scalar(&transitions, 1)?,
                text(&transitions, 2, 15)?,
                scalar(&transitions, 3)?,
                scalar(&transitions, 4)?,
                text(&transitions, 5, 15)?,
            );
            history
                .push(row)
                .map_err(|error| AuditError::Semantic(LedgerError::Corrupt(error)))?;
        }
        history
            .finish()
            .map_err(|error| AuditError::Semantic(LedgerError::Corrupt(error)))?;
        let state = state_from_str(&state)?;
        if state != RecoveryReserveState::Reconstituted {
            return Err(LedgerError::ActiveForCleanHandoff {
                charge_id: charge,
                state,
            }
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Connection, OperationId, Uuid) {
        let directory = tempfile::tempdir().unwrap();
        let connection = Connection::open(directory.path().join("original.sqlite")).unwrap();
        connection.execute_batch(&format!("CREATE TABLE operation_journal(operation_id TEXT PRIMARY KEY); CREATE TABLE operation_journal_full_progress(operation_id TEXT PRIMARY KEY); {CREATE_CHARGE_TABLE}; {CREATE_TRANSITION_TABLE};")).unwrap();
        let operation = OperationId::new();
        let charge = Uuid::new_v4();
        connection
            .execute(
                "INSERT INTO operation_journal VALUES(?1)",
                params![operation.to_string()],
            )
            .unwrap();
        (directory, connection, operation, charge)
    }
    fn history(connection: &Connection, operation: OperationId, charge: Uuid, spends: usize) {
        let budget = i64::try_from(spends + 10).unwrap();
        connection.execute("INSERT INTO operation_journal_recovery_reserve VALUES(?1,'7',NULL,?2,'OperationRecovery',?3,0,0,0,'Reconstituted',?4)", params![charge.to_string(), operation.to_string(), budget, spends as i64 + 3]).unwrap();
        connection.execute("INSERT INTO operation_journal_recovery_reserve_transition VALUES(?1,?2,1,'Charged',?3,?3,'Charged')",params![charge.to_string(),Uuid::new_v4().to_string(),budget]).unwrap();
        for index in 0..spends {
            connection.execute("INSERT INTO operation_journal_recovery_reserve_transition VALUES(?1,?2,?3,'Spent',1,?4,'Spent')",params![charge.to_string(),Uuid::new_v4().to_string(),index as i64+2,budget-index as i64-1]).unwrap();
        }
        // A release observation is legal only after at least one actual spend.
        assert!(spends > 0);
        connection.execute("INSERT INTO operation_journal_recovery_reserve_transition VALUES(?1,?2,?3,'ReleaseObserved',1,10,'ReleaseObserved')",params![charge.to_string(),Uuid::new_v4().to_string(),spends as i64+2]).unwrap();
        connection.execute("INSERT INTO operation_journal_recovery_reserve_transition VALUES(?1,?2,?3,'Reconstituted',0,0,'Reconstituted')",params![charge.to_string(),Uuid::new_v4().to_string(),spends as i64+3]).unwrap();
    }
    fn succeeded(connection: &Connection, input: &InputPermit) -> bool {
        audit(connection, Path::new("original.sqlite"), input).is_ok()
    }
    #[test]
    fn recovery_funded_complete_original_history_matches_compatibility() {
        for spends in [1, 259, 768] {
            let (_directory, connection, operation, charge) = fixture();
            let input = InputPermit::historical_test_input(1);
            let before = input.historical_test_remaining();
            assert!(succeeded(&connection, &input));
            history(&connection, operation, charge, spends);
            assert!(prove_no_active_charges(&connection).is_ok());
            assert!(succeeded(&connection, &input));
            assert_eq!(input.historical_test_remaining(), before);
        }
    }
    #[test]
    fn recovery_funded_corruption_parity_and_diagnostic_drop_order() {
        for sql in [
            "UPDATE operation_journal_recovery_reserve SET active_volume_device='7', state='Spent'",
            "DELETE FROM operation_journal",
            "DELETE FROM operation_journal_recovery_reserve_transition WHERE transition_sequence=2",
            "UPDATE operation_journal_recovery_reserve_transition SET remaining_bytes=9 WHERE transition_sequence=2",
            "UPDATE operation_journal_recovery_reserve SET transition_sequence=5",
            "UPDATE operation_journal_recovery_reserve SET budget_bytes=-1",
            "UPDATE operation_journal_recovery_reserve SET margin_bytes=268435456",
            "UPDATE operation_journal_recovery_reserve SET purpose='Unknown'",
            "UPDATE operation_journal_recovery_reserve SET volume_device='07'",
            "UPDATE operation_journal_recovery_reserve SET charge_id='invalid'",
            "UPDATE operation_journal_recovery_reserve SET state=CAST(x'ff' AS TEXT)",
            "UPDATE operation_journal_recovery_reserve SET purpose=zeroblob(1048576)",
            "PRAGMA foreign_keys=OFF; DELETE FROM operation_journal_recovery_reserve",
        ] {
            let (_directory, connection, operation, charge) = fixture();
            history(&connection, operation, charge, 1);
            // Deliberate invalid stored fixtures bypass CHECK/FK only on this original connection.
            connection
                .execute_batch("PRAGMA ignore_check_constraints=ON; PRAGMA foreign_keys=OFF;")
                .unwrap();
            connection.execute_batch(sql).unwrap();
            assert!(prove_no_active_charges(&connection).is_err(), "{sql}");
            let input = InputPermit::historical_test_input(1);
            let before = input.historical_test_remaining();
            let error = match audit(&connection, Path::new("original.sqlite"), &input) {
                Err(AuditFailure::Funded(error)) => error,
                _ => panic!("expected retained failure: {sql}"),
            };
            assert!(input.historical_test_remaining() < before);
            assert!(format!("{}", error.error).len() < DIAGNOSTIC_BYTES);
            drop(error);
            assert_eq!(input.historical_test_remaining(), before);
        }
    }
    #[test]
    fn recovery_funded_all_active_states_and_native_sql_selection() {
        for (state, sequence, remaining) in [
            ("Charged", 1, 11),
            ("Spent", 2, 10),
            ("ReleaseObserved", 3, 10),
        ] {
            let (_directory, connection, operation, charge) = fixture();
            history(&connection, operation, charge, 1);
            connection.execute("DELETE FROM operation_journal_recovery_reserve_transition WHERE transition_sequence>?1",params![sequence]).unwrap();
            connection.execute("UPDATE operation_journal_recovery_reserve SET active_volume_device='7',state=?1,remaining_bytes=?2,transition_sequence=?3",params![state,remaining,sequence]).unwrap();
            assert!(matches!(
                prove_no_active_charges(&connection),
                Err(LedgerError::ActiveForCleanHandoff { .. })
            ));
            let input = InputPermit::historical_test_input(1);
            assert!(!succeeded(&connection, &input));
        }
        let (_directory, connection, _, _) = fixture();
        connection
            .execute_batch("DROP TABLE operation_journal_recovery_reserve_transition")
            .unwrap();
        let original = prove_no_active_charges(&connection)
            .unwrap_err()
            .to_string();
        let input = InputPermit::historical_test_input(1);
        let before = input.historical_test_remaining();
        let Err(AuditFailure::Funded(error)) =
            audit(&connection, Path::new("original.sqlite"), &input)
        else {
            panic!("native failure");
        };
        assert!(error.native.is_some());
        assert!(
            error
                .error
                .to_string()
                .contains("no such table: operation_journal_recovery_reserve_transition")
        );
        assert!(original.contains("no such table: operation_journal_recovery_reserve_transition"));
        drop(error);
        assert_eq!(input.historical_test_remaining(), before);
    }
    #[test]
    fn recovery_funded_quota_refuses_before_sql_and_effects() {
        let (_directory, connection, operation, charge) = fixture();
        history(&connection, operation, charge, 1);
        let input =
            InputPermit::historical_test_input(InputPermit::historical_test_ordinary_limit() - 1);
        assert!(matches!(
            audit(&connection, Path::new("original.sqlite"), &input),
            Err(AuditFailure::Unfunded(
                OperationJournalStoreError::HistoricalClaimResourceBound
            ))
        ));
        assert!(prove_no_active_charges(&connection).is_ok());
        assert_eq!(input.historical_test_remaining(), 1);
    }
}

#[cfg(any(test, feature = "test-support"))]
impl crate::operation_journal::store::OperationJournalStore {
    /// Stored history fixture on this original Store connection. No worker or effects.
    pub fn install_recovery_audit_fixture_for_test(
        &self,
        operation: OperationId,
        spends: usize,
        corrupt_text_bytes: usize,
    ) -> Result<(), OperationJournalStoreError> {
        let connection = &self.connection;
        connection.execute_batch("DELETE FROM operation_journal_recovery_reserve_transition; DELETE FROM operation_journal_recovery_reserve")?;
        let charge = Uuid::from_u128(1).to_string();
        if spends == 0 {
            return Ok(());
        }
        let budget = i64::try_from(spends)
            .ok()
            .and_then(|n| n.checked_add(10))
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        connection.execute("INSERT INTO operation_journal_recovery_reserve VALUES(?1,'7',NULL,?2,'OperationRecovery',?3,0,0,0,'Reconstituted',?4)", params![charge,operation.to_string(),budget,budget-7])?;
        connection.execute("INSERT INTO operation_journal_recovery_reserve_transition VALUES(?1,?2,1,'Charged',?3,?3,'Charged')",params![charge,Uuid::from_u128(2).to_string(),budget])?;
        for index in 0..spends {
            connection.execute("INSERT INTO operation_journal_recovery_reserve_transition VALUES(?1,?2,?3,'Spent',1,?4,'Spent')",params![charge,Uuid::from_u128(index as u128+3).to_string(),index as i64+2,budget-index as i64-1])?;
        }
        connection.execute("INSERT INTO operation_journal_recovery_reserve_transition VALUES(?1,?2,?3,'ReleaseObserved',1,10,'ReleaseObserved')",params![charge,Uuid::from_u128(spends as u128+3).to_string(),spends as i64+2])?;
        connection.execute("INSERT INTO operation_journal_recovery_reserve_transition VALUES(?1,?2,?3,'Reconstituted',0,0,'Reconstituted')",params![charge,Uuid::from_u128(spends as u128+4).to_string(),spends as i64+3])?;
        if corrupt_text_bytes != 0 {
            connection.execute_batch("PRAGMA ignore_check_constraints=ON")?;
            let result = connection.execute(
                "UPDATE operation_journal_recovery_reserve SET purpose=?1",
                params!["x".repeat(corrupt_text_bytes)],
            );
            connection.execute_batch("PRAGMA ignore_check_constraints=OFF")?;
            result?;
        }
        Ok(())
    }
    /// Scalar normal-build diagnostic; the isolated pool grants no production authority.
    /// Returned values contain no row, path, error or backing that can escape its debit.
    pub fn recovery_audit_allocation_probe_for_test(
        &self,
        funded: bool,
    ) -> Result<(bool, usize), OperationJournalStoreError> {
        self.connection.execute_batch("BEGIN")?;
        let result = if funded {
            let input = InputPermit::historical_test_input(1);
            let bytes = FIXED_BYTES + 7 * self.database_path.as_os_str().as_encoded_bytes().len();
            let valid = match audit(&self.connection, &self.database_path, &input) {
                Ok(()) => true,
                Err(AuditFailure::Funded(error)) => {
                    drop(error);
                    false
                }
                Err(AuditFailure::Unfunded(error)) => {
                    self.connection.execute_batch("ROLLBACK")?;
                    return Err(error);
                }
            };
            (valid, bytes)
        } else {
            (prove_no_active_charges(&self.connection).is_ok(), 0)
        };
        self.connection.execute_batch("ROLLBACK")?;
        Ok(result)
    }
}
