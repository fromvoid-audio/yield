//! Original-connection transaction controls with independent owned-error loans.
use super::historical_claim_observation::borrowed_sql::{Statement, copy_error_message_with_bound};
use super::*;
use crate::operation_journal::owner::{InputPermit, OperationJournalOwnerError};
use std::cell::RefCell;
use std::ffi::CStr;

#[derive(Clone, Copy)]
pub(super) enum Control {
    Begin,
    Rollback,
}

impl Control {
    fn sql(self) -> &'static CStr {
        match self {
            Self::Begin => c"BEGIN",
            Self::Rollback => c"ROLLBACK",
        }
    }
}

// Namespace/control outgoing errors share the original reply's private carrier.
// Its payload Box is allocated only after explicit request consumption; prepay
// the actual enum layout before the original prepare/step can produce an error.
pub(super) const WORKING_BYTES: usize = std::mem::size_of::<OperationJournalOwnerError>();

fn copy_control_message(
    input: &InputPermit,
    message: &CStr,
) -> Result<(String, InputPermit), OperationJournalStoreError> {
    copy_error_message_with_bound(input, message, native_message_bytes)
}

fn native_message_bytes(length: usize) -> Option<usize> {
    const STORE: usize = "operation journal SQLite error: ".len();
    const STARTUP: usize = "ordinary-claim capacity rebuild bootstrap failed: ".len();
    // The original lossy native String can grow geometrically, including the
    // pinned eight-byte minimum. Prepay all named potential owned consumers:
    // queued RecoveryRequired.reason, runtime startup String, and Shared startup
    // diagnostic. Their separate runtime loans still pay before each formatting
    // call; this producer envelope conservatively includes coexistence rather
    // than relying on a historical fixed-prefix allowance or a static native cap.
    let original = length.checked_mul(2)?.checked_add(8)?;
    let ordinary = length.checked_add(STORE)?.checked_mul(2)?;
    let shared = length
        .checked_add(STORE)?
        .checked_add(STARTUP)?
        .checked_mul(2)?;
    original
        .checked_add(ordinary.checked_mul(2)?)?
        .checked_add(shared)?
        .checked_add(3 * std::mem::size_of::<String>())
}

pub(super) enum Reader<'a> {
    Funded(Scope<'a>),
    Unfunded(&'a Connection),
}

impl<'a> Reader<'a> {
    pub(super) fn new(
        connection: &'a Connection,
        input: Option<&'a InputPermit>,
    ) -> Result<Self, OperationJournalStoreError> {
        match input {
            Some(input) => Scope::new(connection, input)
                .map(Self::Funded)
                .map_err(|failure| failure.retain(input)),
            None => Ok(Self::Unfunded(connection)),
        }
    }

    pub(super) fn begin(&mut self) -> Result<(), OperationJournalStoreError> {
        match self {
            Self::Funded(scope) => scope.begin().map_err(|failure| failure.retain(scope.input)),
            Self::Unfunded(connection) => connection.execute_batch("BEGIN").map_err(Into::into),
        }
    }

    pub(super) fn rollback(&mut self) -> Result<(), OperationJournalStoreError> {
        match self {
            Self::Funded(scope) => scope
                .rollback()
                .map_err(|failure| failure.retain(scope.input)),
            Self::Unfunded(connection) => connection.execute_batch("ROLLBACK").map_err(Into::into),
        }
    }

    pub(super) fn discard_rollback(&self) {
        match self {
            Self::Funded(scope) => scope.discard_rollback(),
            Self::Unfunded(connection) => {
                let _ = connection.execute_batch("ROLLBACK");
            }
        }
    }
}

pub(super) struct Failure {
    error: Option<OperationJournalStoreError>,
    native: Option<InputPermit>,
    working: Option<InputPermit>,
}

impl Failure {
    pub(super) fn retain(mut self, input: &InputPermit) -> OperationJournalStoreError {
        let error = self.error.take().expect("original control failure");
        match self.working.take() {
            Some(working) => input.retain_control_failure(error, working, self.native.take()),
            None => {
                // Missing consumer/initial pressure produces only a closed scalar error.
                debug_assert!(self.native.is_none());
                error
            }
        }
    }
}

impl Drop for Failure {
    fn drop(&mut self) {
        // An ignored secondary rollback must destroy its own error first and
        // cannot replace, clear or refund a primary failure's original loans.
        drop(self.error.take());
        drop(self.native.take());
        drop(self.working.take());
    }
}

fn refused() -> Failure {
    Failure {
        error: Some(OperationJournalStoreError::OrdinaryRebuildResourceBound),
        native: None,
        working: None,
    }
}

pub(super) struct Scope<'a> {
    connection: &'a Connection,
    input: &'a InputPermit,
    // Prepay before BEGIN and retain through the original final ROLLBACK. A
    // later working-credit shortage must never suppress transaction cleanup.
    working: Option<InputPermit>,
}

impl<'a> Scope<'a> {
    pub(super) fn new(connection: &'a Connection, input: &'a InputPermit) -> Result<Self, Failure> {
        if !input.can_retain_control_failure() {
            return Err(refused());
        }
        let working = input
            .reserve_historical_scratch(WORKING_BYTES)
            .map_err(|_| refused())?;
        Ok(Self {
            connection,
            input,
            working: Some(working),
        })
    }

    pub(super) fn begin(&mut self) -> Result<(), Failure> {
        self.selected(Control::Begin)
    }

    pub(super) fn rollback(&mut self) -> Result<(), Failure> {
        self.selected(Control::Rollback)
    }

    fn selected(&mut self, control: Control) -> Result<(), Failure> {
        if self.working.is_none() {
            return Err(refused());
        }
        run(self.connection, self.input, control).map_err(|mut failure| Failure {
            error: failure.error.take(),
            native: failure.native.take(),
            working: self.working.take(),
        })
    }

    pub(super) fn discard_rollback(&self) {
        // Always attempt the exact original SQL, even with no remaining Rust
        // credit. A native message is copied only if its independent loan fits;
        // otherwise the scalar refusal is discarded after the actual attempt.
        drop(run(self.connection, self.input, Control::Rollback));
    }
}

struct NativeFailure {
    error: Option<OperationJournalStoreError>,
    native: Option<InputPermit>,
}

impl Drop for NativeFailure {
    fn drop(&mut self) {
        drop(self.error.take());
        drop(self.native.take());
    }
}

fn run(
    connection: &Connection,
    input: &InputPermit,
    control: Control,
) -> Result<(), NativeFailure> {
    let native = RefCell::new(None);
    let result = (|| {
        // Same sqlite3_prepare_v2 flags and single step as pinned rusqlite
        // execute_batch on these exact one-statement strings. Finalization
        // follows prepare/step success, failure and unwind on the same borrow.
        let mut statement = Statement::new_with_message_copy(
            connection,
            input,
            &native,
            control.sql(),
            copy_control_message,
        )?;
        statement.next().map(|_| ())
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(NativeFailure {
            error: Some(error),
            native: native.into_inner(),
        }),
    }
}

#[cfg(test)]
#[path = "transaction_control/tests.rs"]
mod tests;

#[cfg(any(test, feature = "test-support"))]
mod allocation_probe;

// Test-only fault installation on the same original connection. The callback
// allocates nothing, retains no user pointer, and does not create a transaction.
#[cfg(test)]
impl OperationJournalStore {
    pub(in crate::operation_journal) fn deny_original_control_begin_for_test(&self) {
        unsafe extern "C" fn deny_begin(
            _: *mut std::ffi::c_void,
            action: i32,
            name: *const std::ffi::c_char,
            _: *const std::ffi::c_char,
            _: *const std::ffi::c_char,
            _: *const std::ffi::c_char,
        ) -> i32 {
            if action == rusqlite::ffi::SQLITE_TRANSACTION
                && !name.is_null()
                // SAFETY: SQLite passes a NUL-terminated name for this callback.
                && unsafe { CStr::from_ptr(name) }.to_bytes() == b"BEGIN"
            {
                rusqlite::ffi::SQLITE_DENY
            } else {
                rusqlite::ffi::SQLITE_OK
            }
        }
        // SAFETY: exclusive test setup on the original thread-confined connection;
        // callback stores no references and remains valid for the connection lifetime.
        assert_eq!(
            unsafe {
                rusqlite::ffi::sqlite3_set_authorizer(
                    self.connection.handle(),
                    Some(deny_begin),
                    std::ptr::null_mut(),
                )
            },
            rusqlite::ffi::SQLITE_OK
        );
    }
}
