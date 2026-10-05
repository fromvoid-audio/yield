//! Connection-owned forwarding VFS. A limit restricts I/O; it never grants admission.
//! Registration outlives SQLite close, including rollback and sidecar cleanup callbacks.
// The genuine journal permit consumer is a separate issue. These private APIs restrict I/O;
// no production path may use a logical ceiling as live physical admission.
#![allow(dead_code)]
use rusqlite::{Connection, ffi};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ops::Deref;
use std::ptr;
use std::sync::{
    Mutex,
    atomic::{AtomicPtr, Ordering},
};

mod io;
pub(crate) mod no_effect;
#[cfg(test)]
mod tests;
pub(crate) mod wal_objects;

const CLIENT_NAME: &str = "yield-owned-budget-v1";

/// The SQLite connection MUST drop before the registration it borrows.
pub(crate) struct OwnedConnection {
    connection: Option<Connection>,
    registration: Option<Box<Registration>>,
    // Exact procfs alias descriptor stays open through every native close callback.
    directory_descriptor: Option<std::sync::Arc<std::fs::File>>,
}
impl From<Connection> for OwnedConnection {
    fn from(connection: Connection) -> Self {
        Self {
            connection: Some(connection),
            registration: None,
            directory_descriptor: None,
        }
    }
}
impl Deref for OwnedConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.connection.as_ref().expect("live owned connection")
    }
}
impl OwnedConnection {
    /// Inspect the original known native pager used by DELETE allocation observations.
    /// This reads a native scalar only; it installs no budget or mutation permission.
    pub(crate) fn known_delete_sector_size(&self) -> Option<u64> {
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            None
        }
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        {
            if rusqlite::version_number() != 3_050_002 {
                return None;
            }
            let registration = self.registration.as_ref()?;
            let file = registration.context.main.load(Ordering::SeqCst);
            if file.is_null() {
                return None;
            }
            // SAFETY: the private owned constructor verified this main pointer belongs to this
            // registration. Both outlive the borrowed connection. No concurrent close is possible.
            unsafe {
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                io::known_unix_descriptor(file, &registration.context, 0)?;
                #[cfg(windows)]
                io::known_windows_file(file, &registration.context, 0)?;
                let methods = (*file).pMethods.as_ref()?;
                let sector = (methods.xSectorSize?)(file);
                let sector = u64::try_from(sector).ok()?;
                // Bundled pager clamps JOURNAL_HDR_SZ to MAX_SECTOR_SIZE=0x10000.
                // Refuse unknown callbacks/values rather than presume that clamp is sufficient.
                ((512..=65536).contains(&sector) && sector.is_power_of_two()).then_some(sector)
            }
        }
    }

    // Never expose &mut Connection: safe mem::replace could otherwise separate the
    // live original connection from its registration and cause a use-after-free.
    pub(crate) fn transaction(&mut self) -> rusqlite::Result<rusqlite::Transaction<'_>> {
        self.connection
            .as_mut()
            .expect("live owned connection")
            .transaction()
    }
    pub(crate) fn transaction_with_behavior(
        &mut self,
        behavior: rusqlite::TransactionBehavior,
    ) -> rusqlite::Result<rusqlite::Transaction<'_>> {
        self.connection
            .as_mut()
            .expect("live owned connection")
            .transaction_with_behavior(behavior)
    }
    pub(crate) fn readiness_store<'a>(
        &'a mut self,
        ownership: crate::sample_sources::db::ReadinessOwnership<'a>,
    ) -> crate::sample_sources::readiness::ReadinessStore<'a> {
        crate::sample_sources::readiness::ReadinessStore::from_owned_connection(
            self.connection.as_mut().expect("live owned connection"),
            ownership,
        )
    }
    /// Legacy path-only APIs may extract only connections without an owned VFS.
    pub(crate) fn into_legacy(mut self) -> rusqlite::Result<Connection> {
        if self.registration.is_some() {
            return Err(rusqlite::Error::InvalidQuery);
        }
        Ok(self.connection.take().expect("live owned connection"))
    }
    pub(crate) fn close(mut self) -> Result<(), (Self, rusqlite::Error)> {
        let connection = self.connection.take().expect("live owned connection");
        match connection.close() {
            Ok(()) => Ok(()),
            Err((connection, error)) => {
                self.connection = Some(connection);
                Err((self, error))
            }
        }
    }
}
impl Drop for OwnedConnection {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        if let Err((connection, _error)) = connection.close() {
            // A forgotten statement/backup can leave SQLite alive after SQLITE_BUSY. Never
            // free the registration or directory alias it still borrows. Keep the connection
            // unavailable; Drop is not a rollback/settlement proof and cannot refund charges.
            let _ = connection.execute_batch("PRAGMA query_only=ON");
            if let Some(r) = self.registration.as_ref() {
                r.context.quarantine();
            }
            std::mem::forget(connection);
            if let Some(r) = self.registration.take() {
                std::mem::forget(r);
            }
            if let Some(fd) = self.directory_descriptor.take() {
                std::mem::forget(fd);
            }
            tracing::error!("SQLite close was unconfirmed; retained quarantined VFS ownership");
        }
    }
}

struct Registration {
    vfs: Box<ffi::sqlite3_vfs>,
    name: CString,
    context: Box<Context>,
}
// SAFETY: SQLite serializes its VFS registry. The native VFS is process-lifetime;
// this registration moves only with its non-Sync, sole-owner rusqlite Connection.
unsafe impl Send for Registration {}
impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: OwnedConnection closes SQLite first; no callback still borrows this VFS.
        unsafe {
            ffi::sqlite3_vfs_unregister(&mut *self.vfs);
        }
    }
}

struct Context {
    // Immutable from before sqlite3_open until the final native close.
    no_effect: bool,
    #[cfg(test)]
    native_effect_calls: std::sync::atomic::AtomicU64,
    native: *mut ffi::sqlite3_vfs,
    main: AtomicPtr<ffi::sqlite3_file>,
    state: Mutex<State>,
    #[cfg(test)]
    partial_write: Mutex<Option<c_int>>,
    #[cfg(test)]
    fail_sync: std::sync::atomic::AtomicBool,
}
struct State {
    main_name: Vec<u8>,
    open_auxiliary: u32,
    mapped_shm_bytes: u64,
    mapped_shm_region_zero: Option<usize>,
    mapped_database: u32,
    attempt: Option<Attempt>,
}
struct Attempt {
    spec: LogicalBudget,
    charged: [u64; 3],
    failure: Option<BudgetFailure>,
    quarantined: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BudgetFailure {
    Exhausted,
    UnknownCoverage,
    NativeIo,
    Quarantined,
}

/// Logical I/O restrictions only. Physical qualification and live admission are separate.
#[derive(Clone, Copy)]
pub(crate) struct LogicalBudget {
    ceilings: [u64; 3],
    unit: u64,
    overwrite_overhead: u64,
    metadata_overhead: u64,
}
impl LogicalBudget {
    pub(crate) fn new(
        ceilings: [u64; 3],
        unit: u64,
        overwrite_overhead: u64,
        metadata_overhead: u64,
    ) -> Option<Self> {
        if unit == 0
            || !unit.is_power_of_two()
            || unit > 1024 * 1024
            || ceilings.iter().any(|n| *n > i64::MAX as u64)
            || ceilings.iter().any(|n| *n % unit != 0)
            || overwrite_overhead % unit != 0
            || metadata_overhead % unit != 0
        {
            return None;
        }
        Some(Self {
            ceilings,
            unit,
            overwrite_overhead,
            metadata_overhead,
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BudgetReport {
    pub(crate) charged: [u64; 3],
    pub(crate) failure: Option<BudgetFailure>,
}

/// One connection-bound restriction, neither a capacity observation nor a write permit.
pub(crate) struct BudgetAttempt<'a> {
    owner: &'a OwnedConnection,
}
impl OwnedConnection {
    /// Install only inside an already reserved WAL transaction with memory-only temporary data.
    /// The owner still must pass all existing admission gates before its effect.
    pub(crate) fn restrict(&self, spec: LogicalBudget) -> Result<BudgetAttempt<'_>, BudgetFailure> {
        let r = self
            .registration
            .as_ref()
            .ok_or(BudgetFailure::UnknownCoverage)?;
        if self.is_autocommit() {
            return Err(BudgetFailure::UnknownCoverage);
        }
        let mode: String = self
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(|_| BudgetFailure::UnknownCoverage)?;
        let temp: i64 = self
            .pragma_query_value(None, "temp_store", |row| row.get(0))
            .map_err(|_| BudgetFailure::UnknownCoverage)?;
        let mmap: i64 = self
            .pragma_query_value(None, "mmap_size", |row| row.get(0))
            .map_err(|_| BudgetFailure::UnknownCoverage)?;
        let checkpoint: i64 = self
            .pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))
            .map_err(|_| BudgetFailure::UnknownCoverage)?;
        if !mode.eq_ignore_ascii_case("wal") || temp != 2 || mmap != 0 || checkpoint != 0 {
            return Err(BudgetFailure::UnknownCoverage);
        }
        // Disable inherited native preallocation chunking before enforcing a logical bound.
        // SIZE_HINT must not secretly round beyond the extent charged by the wrapper.
        let mut chunk: c_int = 0;
        // SAFETY: sole owner, native control writes only its private preallocation policy.
        let rc = unsafe {
            ffi::sqlite3_file_control(
                self.handle(),
                c"main".as_ptr(),
                ffi::SQLITE_FCNTL_CHUNK_SIZE,
                (&mut chunk as *mut c_int).cast(),
            )
        };
        if rc != ffi::SQLITE_OK {
            return Err(BudgetFailure::UnknownCoverage);
        }
        let mut s = r
            .context
            .state
            .lock()
            .map_err(|_| BudgetFailure::Quarantined)?;
        if s.attempt.is_some() || s.open_auxiliary != 0 || s.mapped_database != 0 {
            return Err(BudgetFailure::Quarantined);
        }
        // Existing SHM mappings can be dirtied without another xShmMap. Charge their complete
        // mapped extent up front. Native CoW/metadata qualification is NOT inferred from this.
        let initial =
            charge_amount(s.mapped_shm_bytes, spec).ok_or(BudgetFailure::UnknownCoverage)?;
        if initial > spec.ceilings[2] {
            return Err(BudgetFailure::Exhausted);
        }
        s.attempt = Some(Attempt {
            spec,
            charged: [0, 0, initial],
            failure: None,
            quarantined: false,
        });
        Ok(BudgetAttempt { owner: self })
    }
}
impl BudgetAttempt<'_> {
    pub(crate) fn report(&self) -> BudgetReport {
        let r = self
            .owner
            .registration
            .as_ref()
            .expect("attempt retains registration");
        let s = r.context.state.lock().unwrap_or_else(|e| e.into_inner());
        let a = s.attempt.as_ref().expect("one attempt per connection");
        BudgetReport {
            charged: a.charged,
            failure: a.failure,
        }
    }
}
impl Drop for BudgetAttempt<'_> {
    fn drop(&mut self) {
        let r = self
            .owner
            .registration
            .as_ref()
            .expect("attempt retains registration");
        let mut s = r.context.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(a) = s.attempt.as_mut() {
            a.quarantined = true;
        }
        // Drop/timeouts/rollback never replenish a durable claim. This primitive deliberately
        // has no scalar reset. A later genuine outcome/reconciliation owner must settle it.
    }
}
fn charge_amount(bytes: u64, spec: LogicalBudget) -> Option<u64> {
    if bytes == 0 {
        return Some(0);
    }
    let rounded = bytes
        .checked_add(spec.unit - 1)?
        .checked_div(spec.unit)?
        .checked_mul(spec.unit)?;
    rounded
        .checked_add(spec.overwrite_overhead)?
        .checked_add(spec.metadata_overhead)
}
impl Context {
    fn quarantine(&self) {
        if let Ok(mut s) = self.state.lock() {
            let a = s.attempt.get_or_insert(Attempt {
                spec: LogicalBudget {
                    ceilings: [0; 3],
                    unit: 4096,
                    overwrite_overhead: 0,
                    metadata_overhead: 0,
                },
                charged: [0; 3],
                failure: Some(BudgetFailure::Quarantined),
                quarantined: true,
            });
            a.quarantined = true;
            a.failure.get_or_insert(BudgetFailure::Quarantined);
        }
    }
    fn charge_extent(&self, role: usize, start: u64, bytes: u64) -> c_int {
        let unit = match self.state.lock() {
            Ok(s) => s.attempt.as_ref().map(|a| a.spec.unit),
            Err(_) => return ffi::SQLITE_IOERR,
        };
        let Some(unit) = unit else {
            return ffi::SQLITE_OK;
        };
        let Some(span) = bytes.checked_add(start % unit) else {
            return self.charge(4, 1);
        };
        self.charge(role, span)
    }
    fn charge(&self, role: usize, bytes: u64) -> c_int {
        let Ok(mut s) = self.state.lock() else {
            return ffi::SQLITE_IOERR;
        };
        let Some(a) = s.attempt.as_mut() else {
            return ffi::SQLITE_OK;
        };
        if a.failure.is_some() || a.quarantined {
            return ffi::SQLITE_IOERR;
        }
        if role > 2 {
            a.failure = Some(BudgetFailure::UnknownCoverage);
            return ffi::SQLITE_IOERR;
        }
        let Some(amount) = charge_amount(bytes, a.spec) else {
            a.failure = Some(BudgetFailure::UnknownCoverage);
            return ffi::SQLITE_IOERR;
        };
        let Some(total) = a.charged[role]
            .checked_add(amount)
            .filter(|n| *n <= a.spec.ceilings[role])
        else {
            a.failure = Some(BudgetFailure::Exhausted);
            return ffi::SQLITE_FULL;
        };
        a.charged[role] = total;
        ffi::SQLITE_OK
    }
    fn native_result(&self, rc: c_int) -> c_int {
        if rc != ffi::SQLITE_OK {
            if let Ok(mut s) = self.state.lock() {
                if let Some(a) = s.attempt.as_mut() {
                    a.failure.get_or_insert(BudgetFailure::NativeIo);
                }
            }
        }
        rc
    }
    fn restricted(&self) -> bool {
        self.state.lock().map_or(true, |s| s.attempt.is_some())
    }
}

/// Register one private named VFS and retain it with the opened connection. No default changes.
pub(crate) fn open_owned(
    path: impl AsRef<std::path::Path>,
    flags: rusqlite::OpenFlags,
    directory_descriptor: Option<std::sync::Arc<std::fs::File>>,
) -> rusqlite::Result<OwnedConnection> {
    open_owned_mode(path, flags, directory_descriptor, false)
}

pub(super) fn open_owned_mode(
    path: impl AsRef<std::path::Path>,
    flags: rusqlite::OpenFlags,
    directory_descriptor: Option<std::sync::Arc<std::fs::File>>,
    no_effect: bool,
) -> rusqlite::Result<OwnedConnection> {
    #[cfg(target_os = "linux")]
    let native_name = CString::new(crate::sqlite_linux::vfs_name()?).expect("fixed VFS name");
    #[cfg(target_os = "windows")]
    let native_name = c"win32";
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let native_name = c"unix";
    // SAFETY: SQLite initialization and VFS registration are mutex protected. The native VFS
    // remains registered for process lifetime; our boxes remain pinned through Connection close.
    let registration = unsafe {
        let rc = ffi::sqlite3_initialize();
        if rc != ffi::SQLITE_OK {
            return Err(rusqlite::Error::SqliteFailure(ffi::Error::new(rc), None));
        }
        let native = ffi::sqlite3_vfs_find(native_name.as_ptr());
        if native.is_null()
            || (*native).szOsFile <= 0
            || !(1..=3).contains(&(*native).iVersion)
            || (*native).xOpen.is_none()
        {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let name =
            CString::new(format!("{CLIENT_NAME}-{}", uuid::Uuid::new_v4())).expect("UUID name");
        let mut context = Box::new(Context {
            no_effect,
            #[cfg(test)]
            native_effect_calls: std::sync::atomic::AtomicU64::new(0),
            native,
            main: AtomicPtr::new(ptr::null_mut()),
            state: Mutex::new(State {
                main_name: Vec::new(),
                open_auxiliary: 0,
                mapped_shm_bytes: 0,
                mapped_shm_region_zero: None,
                mapped_database: 0,
                attempt: None,
            }),
            #[cfg(test)]
            partial_write: Mutex::new(None),
            #[cfg(test)]
            fail_sync: std::sync::atomic::AtomicBool::new(false),
        });
        let mut vfs = Box::new(*native);
        vfs.pNext = ptr::null_mut();
        vfs.zName = name.as_ptr();
        vfs.pAppData = (&mut *context as *mut Context).cast();
        vfs.szOsFile = (*native)
            .szOsFile
            .checked_add(io::HEADER_BYTES as c_int)
            .ok_or(rusqlite::Error::InvalidQuery)?;
        io::install_vfs(&mut vfs);
        let mut registration = Box::new(Registration { vfs, name, context });
        let rc = ffi::sqlite3_vfs_register(&mut *registration.vfs, 0);
        if rc != ffi::SQLITE_OK {
            return Err(rusqlite::Error::SqliteFailure(ffi::Error::new(rc), None));
        }
        registration
    };
    // Keep construction sealed: a caller-supplied opening closure could leak a raw
    // Connection that still borrows this registration on its error path.
    let connection = Connection::open_with_flags_and_vfs(
        path,
        flags,
        registration.name.to_str().expect("ASCII VFS name"),
    )?;
    let mut actual: *mut ffi::sqlite3_file = ptr::null_mut();
    // SAFETY: query a borrowed main descriptor on the exclusive newly opened connection.
    // A closure returning an unrelated raw connection cannot join this private context.
    let rc = unsafe {
        ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            ffi::SQLITE_FCNTL_FILE_POINTER,
            (&mut actual as *mut *mut ffi::sqlite3_file).cast(),
        )
    };
    if rc != ffi::SQLITE_OK
        || actual.is_null()
        || actual != registration.context.main.load(Ordering::SeqCst)
    {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(OwnedConnection {
        connection: Some(connection),
        registration: Some(registration),
        directory_descriptor,
    })
}
