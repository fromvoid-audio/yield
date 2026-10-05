//! Test-only per-file VFS failure; never compiled into a production preparation API.
use super::*;
use rusqlite::ffi;

impl SourceWriter {
    pub(in crate::sample_sources::db::write) fn prepare_wal_reservation_unknown_for_test(
        self,
        expected: &SourceRevision,
        deadline: Instant,
    ) -> Result<SourceWalPreparation, SourceWalPreparationFailure> {
        // HeldWriter is declared before Restore. During unwind the descriptor is restored
        // before HeldWriter closes the ordinary connection and releases its lease.
        let mut held = HeldWriter {
            writer: Some(self),
            reservation: false,
            restore_timeout: None,
            registered: None,
        };
        unsafe extern "C" fn unknown(_: *mut ffi::sqlite3_file, _: *mut ffi::sqlite3_int64) -> i32 {
            ffi::SQLITE_IOERR_FSTAT
        }
        struct Restore {
            file: *mut ffi::sqlite3_file,
            original: *const ffi::sqlite3_io_methods,
            _copy: Box<ffi::sqlite3_io_methods>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                // SAFETY: HeldWriter outlives this guard, including unwind; prepare borrows it.
                // The same WAL object stays live. No shared VFS table is changed.
                unsafe {
                    (*self.file).pMethods = self.original;
                }
            }
        }
        let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
        // SAFETY: exclusive live owner, SQLite writes its borrowed retained WAL pointer.
        let code = unsafe {
            ffi::sqlite3_file_control(
                held.writer().database.connection.handle(),
                c"main".as_ptr(),
                ffi::SQLITE_FCNTL_JOURNAL_POINTER,
                (&mut file as *mut *mut ffi::sqlite3_file).cast(),
            )
        };
        assert_eq!(code, ffi::SQLITE_OK);
        assert!(!file.is_null());
        // SAFETY: checked live object; immutable methods copied into a private stable Box.
        let original = unsafe { (*file).pMethods };
        assert!(!original.is_null());
        let mut copy = Box::new(unsafe { original.read() });
        copy.xFileSize = Some(unknown);
        unsafe {
            (*file).pMethods = &*copy;
        }
        let restore = Restore {
            file,
            original,
            _copy: copy,
        };
        let result = prepare(&mut held, expected, deadline, true);
        drop(restore);
        finish_preparation(held, result, deadline, false)
    }
}
