//! Private statements on the original connection: fund owned error text before copying it.
use super::super::*;
use crate::operation_journal::owner::InputPermit;
use rusqlite::ffi;
use std::cell::RefCell;
use std::ffi::CStr;
use std::ptr;

/// The connection borrow prevents transfer/parallel access; no raw handle or row escapes.
/// Every successful or failed prepare owns its statement until RAII finalization.
pub(in crate::operation_journal::store) struct Statement<'a> {
    connection: &'a Connection,
    input: &'a InputPermit,
    error_credit: &'a RefCell<Option<InputPermit>>,
    raw: *mut ffi::sqlite3_stmt,
    has_row: bool,
}
impl<'a> Statement<'a> {
    pub(in crate::operation_journal::store) fn new(
        connection: &'a Connection,
        input: &'a InputPermit,
        error_credit: &'a RefCell<Option<InputPermit>>,
        sql: &'static CStr,
    ) -> Result<Self, OperationJournalStoreError> {
        let mut statement = Self {
            connection,
            input,
            error_credit,
            raw: ptr::null_mut(),
            has_row: false,
        };
        // SAFETY: the original connection remains borrowed. Static SQL is NUL terminated;
        // SQLite writes only the supplied statement pointer. No other thread can access it.
        let code = unsafe {
            ffi::sqlite3_prepare_v2(
                connection.handle(),
                sql.as_ptr(),
                -1,
                &mut statement.raw,
                ptr::null_mut(),
            )
        };
        statement.check(code)?;
        Ok(statement)
    }
    pub(in crate::operation_journal::store) fn bind_text(
        &mut self,
        index: i32,
        text: &str,
    ) -> Result<(), OperationJournalStoreError> {
        let length = i32::try_from(text.len())
            .map_err(|_| OperationJournalStoreError::HistoricalClaimResourceBound)?;
        // SAFETY: live statement and valid borrowed bytes; TRANSIENT makes SQLite copy
        // before returning, so no binding retains a Rust pointer.
        self.check(unsafe {
            ffi::sqlite3_bind_text(
                self.raw,
                index,
                text.as_ptr().cast(),
                length,
                ffi::SQLITE_TRANSIENT(),
            )
        })
    }
    pub(in crate::operation_journal::store) fn bind_integer(
        &mut self,
        index: i32,
        value: i64,
    ) -> Result<(), OperationJournalStoreError> {
        // SAFETY: live statement; the scalar has no backing lifetime.
        self.check(unsafe { ffi::sqlite3_bind_int64(self.raw, index, value) })
    }
    pub(in crate::operation_journal::store) fn next(
        &mut self,
    ) -> Result<bool, OperationJournalStoreError> {
        self.has_row = false;
        // SAFETY: exclusive mutable statement borrow invalidates any previous row borrow.
        let code = unsafe { ffi::sqlite3_step(self.raw) };
        match code {
            ffi::SQLITE_ROW => {
                self.has_row = true;
                Ok(true)
            }
            ffi::SQLITE_DONE => Ok(false),
            _ => {
                self.check(code)?;
                unreachable!("non-row step cannot report SQLITE_OK")
            }
        }
    }
    pub(in crate::operation_journal::store) fn get_ref(
        &self,
        index: i32,
    ) -> Result<ValueRef<'_>, OperationJournalStoreError> {
        // SAFETY: statement remains live and a row is active. Bounds checked before access.
        if !self.has_row || index < 0 || index >= unsafe { ffi::sqlite3_column_count(self.raw) } {
            return Err(rusqlite::Error::InvalidColumnIndex(index as usize).into());
        }
        // SAFETY: all pointers below are owned by this active statement. Returned slices
        // borrow self, so next/finalize cannot occur until the slice borrow ends.
        unsafe {
            match ffi::sqlite3_column_type(self.raw, index) {
                ffi::SQLITE_NULL => Ok(ValueRef::Null),
                ffi::SQLITE_INTEGER => Ok(ValueRef::Integer(ffi::sqlite3_column_int64(
                    self.raw, index,
                ))),
                ffi::SQLITE_FLOAT => {
                    Ok(ValueRef::Real(ffi::sqlite3_column_double(self.raw, index)))
                }
                ffi::SQLITE_TEXT => {
                    let pointer = ffi::sqlite3_column_text(self.raw, index);
                    let length = ffi::sqlite3_column_bytes(self.raw, index) as usize;
                    if pointer.is_null() {
                        self.check(ffi::SQLITE_NOMEM)?;
                    }
                    Ok(ValueRef::Text(std::slice::from_raw_parts(pointer, length)))
                }
                ffi::SQLITE_BLOB => {
                    let pointer = ffi::sqlite3_column_blob(self.raw, index);
                    let length = ffi::sqlite3_column_bytes(self.raw, index) as usize;
                    if length == 0 {
                        return Ok(ValueRef::Blob(&[]));
                    }
                    if pointer.is_null() {
                        self.check(ffi::SQLITE_NOMEM)?;
                    }
                    Ok(ValueRef::Blob(std::slice::from_raw_parts(
                        pointer.cast::<u8>(),
                        length,
                    )))
                }
                _ => unreachable!("SQLite has five storage classes"),
            }
        }
    }
    fn check(&self, code: i32) -> Result<(), OperationJournalStoreError> {
        if code == ffi::SQLITE_OK {
            return Ok(());
        }
        // SAFETY: original connection is live and exclusively confined to this thread.
        // Match rusqlite's error_msg choice BEFORE any subsequent SQLite call can replace it.
        let message = unsafe {
            let db = self.connection.handle();
            let pointer = if ffi::sqlite3_errcode(db) == code {
                ffi::sqlite3_errmsg(db)
            } else {
                ffi::sqlite3_errstr(code)
            };
            if pointer.is_null() {
                None
            } else {
                Some(CStr::from_ptr(pointer))
            }
        };
        let owned = if let Some(message) = message {
            let (owned, credit) = copy_error_message(self.input, message)?;
            *self.error_credit.borrow_mut() = Some(credit);
            Some(owned)
        } else {
            None
        };
        Err(rusqlite::Error::SqliteFailure(ffi::Error::new(code), owned).into())
    }
}
impl Drop for Statement<'_> {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: this object solely owns the statement; finalization runs on every
            // exit (including failed prepare and panic) before its connection borrow ends.
            unsafe {
                ffi::sqlite3_finalize(self.raw);
            }
        }
    }
}
pub(super) fn copy_error_message(
    input: &InputPermit,
    message: &CStr,
) -> Result<(String, InputPermit), OperationJournalStoreError> {
    let length = lossy_length(message.to_bytes())?;
    // Original lossy String growth plus actual StoreSql Display and the consumer's
    // diagnostic formatting buffer, with both UUIDs and all fixed prefixes named.
    let prefix = "operation journal SQLite error: ".len()
        + "ordinary claim  operation  requires audit: ".len()
        + 2 * 36;
    // Pinned Rust RawVec<u8> uses an eight-byte minimum non-zero growth
    // allocation. Name it independently of geometric growth for tiny lossy input.
    let bytes = length
        .checked_mul(2)
        .and_then(|n| n.checked_add(8))
        .and_then(|original| {
            length
                .checked_add(prefix)
                .and_then(|n| n.checked_mul(2))
                .and_then(|formatted| original.checked_add(formatted))
        })
        .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
    let credit = input.reserve_historical_scratch(bytes)?;
    Ok((message.to_string_lossy().into_owned(), credit))
}

fn lossy_length(mut bytes: &[u8]) -> Result<usize, OperationJournalStoreError> {
    let mut length = 0usize;
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(_) => {
                return length
                    .checked_add(bytes.len())
                    .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound);
            }
            Err(error) => {
                length = length
                    .checked_add(error.valid_up_to())
                    .and_then(|n| n.checked_add(3))
                    .ok_or(OperationJournalStoreError::HistoricalClaimResourceBound)?;
                let consumed = error.valid_up_to()
                    + error
                        .error_len()
                        .unwrap_or(bytes.len() - error.valid_up_to());
                bytes = &bytes[consumed..];
            }
        }
    }
    Ok(length)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_claim_native_sql_preserves_typed_borrows_and_original_errors() {
        let connection = Connection::open_in_memory().unwrap();
        let input = InputPermit::historical_test_input(1);
        let available = input.historical_test_remaining();
        let credit = RefCell::new(None);
        let mut statement = Statement::new(
            &connection,
            &input,
            &credit,
            c"SELECT typeof(?1), ?1, ?2, x'', NULL, 1.5",
        )
        .unwrap();
        statement.bind_text(1, "é\0😀").unwrap();
        statement.bind_integer(2, i64::MAX).unwrap();
        assert!(statement.next().unwrap());
        assert_eq!(statement.get_ref(0).unwrap(), ValueRef::Text(b"text"));
        assert_eq!(
            statement.get_ref(1).unwrap(),
            ValueRef::Text("é\0😀".as_bytes())
        );
        assert_eq!(statement.get_ref(2).unwrap(), ValueRef::Integer(i64::MAX));
        assert_eq!(statement.get_ref(3).unwrap(), ValueRef::Blob(&[]));
        assert_eq!(statement.get_ref(4).unwrap(), ValueRef::Null);
        assert_eq!(statement.get_ref(5).unwrap(), ValueRef::Real(1.5));
        assert!(!statement.next().unwrap());
        drop(statement);
        assert_eq!(input.historical_test_remaining(), available);
        let old = connection
            .prepare("SELECT * FROM \"nonexistent_é\"")
            .err()
            .unwrap();
        let new = Statement::new(
            &connection,
            &input,
            &credit,
            c"SELECT * FROM \"nonexistent_é\"",
        )
        .err()
        .unwrap();
        let OperationJournalStoreError::Sql(new) = new else {
            panic!("changed typed SQLite error")
        };
        assert_eq!(new, old);
        assert!(input.historical_test_remaining() < available);
        drop(new);
        drop(credit.borrow_mut().take());
        assert_eq!(input.historical_test_remaining(), available);
        // Failed prepare and the completed statement both finalized their original handles.
        assert!(unsafe { ffi::sqlite3_next_stmt(connection.handle(), ptr::null_mut()) }.is_null());
    }

    #[test]
    fn historical_claim_native_sql_refuses_before_owned_message_and_finalizes_on_unwind() {
        let connection = Connection::open_in_memory().unwrap();
        let input =
            InputPermit::historical_test_input(InputPermit::historical_test_ordinary_limit() - 1);
        let credit = RefCell::new(None);
        assert!(matches!(
            Statement::new(&connection, &input, &credit, c"SELECT * FROM absent"),
            Err(OperationJournalStoreError::HistoricalClaimResourceBound)
        ));
        assert!(credit.borrow().is_none());
        assert!(unsafe { ffi::sqlite3_next_stmt(connection.handle(), ptr::null_mut()) }.is_null());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let statement = Statement::new(&connection, &input, &credit, c"SELECT 1").unwrap();
                assert!(!statement.raw.is_null());
                panic!("exercise native statement RAII");
            }))
            .is_err()
        );
        assert!(unsafe { ffi::sqlite3_next_stmt(connection.handle(), ptr::null_mut()) }.is_null());
    }

    #[test]
    fn historical_claim_native_error_lossy_size_matches_original_conversion() {
        for bytes in [
            b"ascii".as_slice(),
            "é😀".as_bytes(),
            &[0xff, 0x80, b'x'],
            &[0xf0, 0x9f],
            &[0xed, 0xa0, 0x80],
            &[0xf0, 0x9f, 0x98, 0x80, 0xff],
        ] {
            assert_eq!(
                lossy_length(bytes).unwrap(),
                String::from_utf8_lossy(bytes).len()
            );
        }
    }
}
