//! Original schema producers on the same SQLite connection, with prepaid ownership.
use super::historical_claim_observation::borrowed_sql::{Statement, scalar};
use super::*;
use crate::operation_journal::owner::InputPermit;
use std::{
    cell::RefCell,
    ffi::{CStr, CString},
    mem::size_of,
};

#[cfg(test)]
thread_local! { pub(super) static TEXT_COPIED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

pub(super) struct Scope<'a> {
    connection: &'a Connection,
    input: &'a InputPermit,
    // Declared by the original caller before all owned schema collections.
    credit: RefCell<Option<InputPermit>>,
}
impl<'a> Scope<'a> {
    pub(super) fn new(
        connection: &'a Connection,
        path: &Path,
        input: &'a InputPermit,
    ) -> Result<Self, OperationJournalStoreError> {
        let objects = (2 * (MAX_SCHEMA_OBJECTS as usize + 1) + 4) * size_of::<(String, String)>();
        let columns = (2 * MAX_SCHEMA_COLUMNS + 4) * size_of::<(String, String, i64, i64)>();
        let indexes = (2 * MAX_SCHEMA_INDEXES + 4) * size_of::<(i64, String, i64, String, i64)>();
        let index_columns = MAX_SCHEMA_INDEXES * (2 * MAX_SCHEMA_COLUMNS + 4) * size_of::<String>();
        let bytes = path
            .as_os_str()
            .as_encoded_bytes()
            .len()
            .checked_mul(17)
            .and_then(|n| {
                n.checked_add(
                    2 * objects
                        + 2 * columns
                        + 2 * indexes
                        + 3 * index_columns
                        + 4 * size_of::<Statement<'static>>()
                        + size_of::<OperationJournalStoreError>()
                        + 4096,
                )
            })
            .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
        Ok(Self {
            connection,
            input,
            credit: RefCell::new(Some(input.reserve_historical_scratch(bytes)?)),
        })
    }
    fn prepay(&self, bytes: usize) -> Result<(), OperationJournalStoreError> {
        if bytes != 0 {
            let loan = self.input.reserve_historical_scratch(bytes)?;
            self.credit
                .borrow_mut()
                .as_mut()
                .expect("original schema scope")
                .absorb_historical_scratch(loan);
        }
        Ok(())
    }
    fn row_text(
        &self,
        row: &Statement<'_>,
        index: i32,
    ) -> Result<String, OperationJournalStoreError> {
        if let ValueRef::Text(bytes) = row.get_ref(index)? {
            // Driver UTF-8 errors precede ownership; they need only the fixed
            // conversion header already held by the original scope.
            if std::str::from_utf8(bytes).is_ok() {
                self.prepay(
                    bytes
                        .len()
                        .checked_mul(4)
                        .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?,
                )?;
            }
        }
        let text = scalar::<String>(row, index)?;
        #[cfg(test)]
        TEXT_COPIED_BYTES.with(|n| n.set(n.get() + text.len()));
        Ok(text)
    }
    pub(super) fn query<T>(
        &self,
        sql: &CStr,
        observe: impl FnOnce(&mut Statement<'_>) -> Result<T, OperationJournalStoreError>,
    ) -> Result<T, OperationJournalStoreError> {
        let native = RefCell::new(None);
        let result = (|| {
            let mut row = Statement::new(self.connection, self.input, &native, sql)?;
            observe(&mut row)
        })();
        if let Some(native) = native.into_inner() {
            self.input.retain_historical_error(native, None, None, None);
        }
        result
    }
    pub(super) fn integer(&self, sql: &CStr) -> Result<i64, OperationJournalStoreError> {
        self.query(sql, |row| {
            if !row.next()? {
                return Err(rusqlite::Error::QueryReturnedNoRows.into());
            }
            scalar::<i64>(row, 0)
        })
    }
    pub(super) fn execute(&self, sql: &CStr) -> Result<(), OperationJournalStoreError> {
        self.query(sql, |row| row.next().map(|_| ()))
    }
    pub(super) fn dynamic_integer(
        &self,
        prefix: &str,
        number: u64,
    ) -> Result<i64, OperationJournalStoreError> {
        // At most twenty decimal u64 digits plus exact original SQL framing.
        self.prepay(2 * (prefix.len() + 20 + 1))?;
        let sql = CString::new(format!("{prefix}{number}")).expect("private ASCII numeric SQL");
        self.integer(&sql)
    }
    fn pragma<T>(
        &self,
        prefix: &str,
        table: &str,
        observe: impl FnOnce(&mut Statement<'_>) -> Result<T, OperationJournalStoreError>,
    ) -> Result<T, OperationJournalStoreError> {
        self.prepay(2 * (prefix.len() + table.len() + 2))?;
        let sql =
            CString::new(format!("{prefix}{table})")).expect("private original table constant");
        self.query(&sql, observe)
    }
    pub(super) fn objects(&self) -> Result<Vec<(String, String)>, OperationJournalStoreError> {
        self.query(
            c"SELECT name, type FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_%' ORDER BY name LIMIT ?1",
            |row| {
                row.bind_integer(1, MAX_SCHEMA_OBJECTS + 1)?;
                let mut objects = Vec::new();
                while row.next()? {
                    objects.push((self.row_text(row, 0)?, self.row_text(row, 1)?));
                }
                Ok(objects)
            },
        )
    }
    pub(super) fn table_sql(
        &self,
        table: &str,
        expected: &str,
    ) -> Result<String, OperationJournalStoreError> {
        self.prepay(3 * expected.len() + 16)?;
        self.query(
            c"SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            |row| {
                row.bind_text(1, table)?;
                if !row.next()? {
                    return Err(rusqlite::Error::QueryReturnedNoRows.into());
                }
                self.row_text(row, 0)
            },
        )
    }
    pub(super) fn columns(
        &self,
        table: &str,
    ) -> Result<Vec<(String, String, i64, i64)>, OperationJournalStoreError> {
        self.pragma("PRAGMA table_info(", table, |row| {
            let mut columns = Vec::new();
            for _ in 0..MAX_SCHEMA_COLUMNS {
                if !row.next()? {
                    break;
                }
                columns.push((
                    self.row_text(row, 1)?,
                    self.row_text(row, 2)?,
                    scalar(row, 3)?,
                    scalar(row, 5)?,
                ));
            }
            Ok(columns)
        })
    }
    pub(super) fn indexes(
        &self,
        table: &str,
    ) -> Result<Vec<(i64, String, i64, String, i64)>, OperationJournalStoreError> {
        self.pragma("PRAGMA index_list(", table, |row| {
            let mut indexes = Vec::new();
            for _ in 0..MAX_SCHEMA_INDEXES {
                if !row.next()? {
                    break;
                }
                indexes.push((
                    scalar(row, 0)?,
                    self.row_text(row, 1)?,
                    scalar(row, 2)?,
                    self.row_text(row, 3)?,
                    scalar(row, 4)?,
                ));
            }
            Ok(indexes)
        })
    }
    pub(super) fn index_columns(
        &self,
        name: &str,
    ) -> Result<Vec<String>, OperationJournalStoreError> {
        self.query(
            c"SELECT name FROM pragma_index_info(?1) ORDER BY seqno LIMIT ?2",
            |row| {
                row.bind_text(1, name)?;
                row.bind_integer(2, MAX_SCHEMA_COLUMNS as i64)?;
                let mut columns = Vec::new();
                while row.next()? {
                    columns.push(self.row_text(row, 0)?);
                }
                Ok(columns)
            },
        )
    }
    pub(super) fn finish(self, selected: bool) {
        if selected {
            self.input.retain_historical_error(
                self.credit.into_inner().expect("original schema loan"),
                None,
                None,
                None,
            );
        }
    }
}
