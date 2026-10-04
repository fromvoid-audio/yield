//! Actual private per-file retained WAL I/O failure, never positive authority.
use super::*;
use rusqlite::ffi;
impl SourceWriter {
    #[cfg(feature = "test-support")]
    pub(crate) fn prepare_lease_maintenance_unknown_for_tests(
        self,
        input: SourceLeaseMaintenanceAllocation,
        deadline: Instant,
    ) -> Result<SourceLeaseMaintenancePreparation, SourceLeaseMaintenancePreparationFailure> {
        let mut custody = new_custody(self, input, deadline);
        let result = prepare_unknown(&mut custody);
        finish(custody, result)
    }
}
fn prepare_unknown(custody: &mut Custody) -> Result<u64, SourceLeaseMaintenancePreparationError> {
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
            // SAFETY: borrowed custody outlives this local guard, including unwind. Its retained
            // WAL file remains live; only this exclusive file's private methods were replaced.
            unsafe {
                (*self.file).pMethods = self.original;
            }
        }
    }
    let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
    // SAFETY: exclusive retained live connection; SQLite fills its borrowed WAL pointer.
    let code = unsafe {
        ffi::sqlite3_file_control(
            custody.writer().database.connection.handle(),
            c"main".as_ptr(),
            ffi::SQLITE_FCNTL_JOURNAL_POINTER,
            (&mut file as *mut *mut ffi::sqlite3_file).cast(),
        )
    };
    assert_eq!(code, ffi::SQLITE_OK);
    assert!(!file.is_null());
    // SAFETY: checked live file; copied methods have stable private Box allocation.
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
    let result = prepare(custody);
    drop(restore);
    result
}
