//! Normal-build original controls and explicit native-text conversion diagnostics.
use super::*;
use std::hash::{Hash, Hasher};

/// Borrowed original Store and original input pool for allocation diagnostics.
/// No connection, owned row, error backing or authority can escape this wrapper.
pub struct ControlFundingProbe<'a> {
    store: &'a OperationJournalStore,
    input: InputPermit,
}

impl ControlFundingProbe<'_> {
    /// Execute the exact original BEGIN or ROLLBACK and consume its result locally.
    /// Returns native code, error checksum, final autocommit and actual loan debit.
    pub fn measure(
        &self,
        funded: bool,
        begin: bool,
    ) -> Result<(i32, u64, bool, usize), OperationJournalStoreError> {
        let baseline = self.remaining();
        let control = if begin {
            Control::Begin
        } else {
            Control::Rollback
        };
        let (code, checksum, debit) = if funded {
            let mut controls = Scope::new(&self.store.connection, &self.input)
                .map_err(|failure| failure.retain(&self.input))?;
            let result = if begin {
                controls.begin()
            } else {
                controls.rollback()
            };
            match result {
                Ok(()) => {
                    // A successful BEGIN leaves the original read transaction active;
                    // close it through the same funded scope before probe return.
                    if begin {
                        controls
                            .rollback()
                            .map_err(|failure| failure.retain(&self.input))?;
                    }
                    (0, 0, baseline - self.remaining())
                }
                Err(failure) => {
                    let (code, checksum) = describe(failure.error.as_ref().unwrap());
                    let debit = baseline - self.remaining();
                    drop(failure);
                    (code, checksum, debit)
                }
            }
        } else {
            match self
                .store
                .connection
                .execute_batch(control.sql().to_str().unwrap())
            {
                Ok(()) => {
                    if begin {
                        self.store.connection.execute_batch("ROLLBACK")?;
                    }
                    (0, 0, 0)
                }
                Err(error) => {
                    let (code, checksum) = describe(&OperationJournalStoreError::Sql(error));
                    (code, checksum, 0)
                }
            }
        };
        assert_eq!(self.remaining(), baseline);
        Ok((code, checksum, self.store.connection.is_autocommit(), debit))
    }

    /// Exercise the production native-copy producer and named consumer formatting.
    /// The supplied C string is a conversion fixture, not a real SQLite control failure.
    /// Returns complete message checksum and the exact original-pool debit; no owned
    /// message or retained credit escapes. Caller fixture/pool setup is outside measurement.
    pub fn native_text_fixture(
        &self,
        message: &CStr,
    ) -> Result<(u64, usize), OperationJournalStoreError> {
        let baseline = self.remaining();
        let controls = Scope::new(&self.store.connection, &self.input)
            .map_err(|failure| failure.retain(&self.input))?;
        let (message, native) = copy_control_message(&self.input, message)?;
        let error = Box::new(OperationJournalOwnerError::Store(
            OperationJournalStoreError::Sql(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                Some(message),
            )),
        ));
        let queued = error.to_string();
        let startup = error.to_string();
        let shared = format!("ordinary-claim capacity rebuild bootstrap failed: {startup}");
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        queued.hash(&mut hash);
        startup.hash(&mut hash);
        shared.hash(&mut hash);
        let checksum = hash.finish();
        let debit = baseline - self.remaining();
        // Conservatively coexist all named bodies, then destroy payloads and
        // their Box allocation before releasing the native and fixed loans.
        drop(shared);
        drop(startup);
        drop(queued);
        drop(error);
        drop(native);
        drop(controls);
        assert_eq!(self.remaining(), baseline);
        Ok((checksum, debit))
    }

    fn remaining(&self) -> usize {
        self.input.control_probe_remaining()
    }
}

fn describe(error: &OperationJournalStoreError) -> (i32, u64) {
    let code = match error {
        OperationJournalStoreError::Sql(rusqlite::Error::SqliteFailure(error, _)) => {
            error.extended_code
        }
        _ => panic!("actual controlled native SQLite failure"),
    };
    let message = error.to_string();
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    message.hash(&mut hash);
    (code, hash.finish())
}

impl OperationJournalStore {
    /// Prepare original-control allocation diagnostics outside measured work.
    /// This borrows the same original Connection and installs no admission authority.
    pub fn control_funding_probe_for_test(&self) -> ControlFundingProbe<'_> {
        ControlFundingProbe {
            store: self,
            input: InputPermit::historical_test_namespace_input(1),
        }
    }
}
