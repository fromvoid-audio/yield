use super::*;
use rusqlite::ffi;

/// Exercise an unavailable observation on this genuine WAL object without modifying a shared
/// VFS method table or abandoning the source lease's retained control connection.
fn claim_with_unknown_vfs(
    writer: &mut SourceWriter,
    revision: &crate::sample_sources::SourceRevision,
    target: &ReadinessTarget,
) -> Result<Option<crate::sample_sources::readiness::ClaimedReadinessWork>, SourceWriterError> {
    unsafe extern "C" fn unavailable_size(
        _file: *mut ffi::sqlite3_file,
        _bytes: *mut ffi::sqlite3_int64,
    ) -> i32 {
        ffi::SQLITE_IOERR_FSTAT
    }
    struct Restore {
        file: *mut ffi::sqlite3_file,
        original: *const ffi::sqlite3_io_methods,
        _methods: Box<ffi::sqlite3_io_methods>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: this private helper retains the same live SourceWriter through its single
            // synchronous claim call. Its WAL object cannot close or move before restoration.
            unsafe { (*self.file).pMethods = self.original };
        }
    }
    let connection = &writer.catalog_database().connection;
    let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
    // SAFETY: the genuine source connection is exclusively used on this test's owner thread.
    // SQLite writes its borrowed retained WAL descriptor; the pointer never escapes this helper.
    let result = unsafe {
        ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            ffi::SQLITE_FCNTL_JOURNAL_POINTER,
            (&mut file as *mut *mut ffi::sqlite3_file).cast(),
        )
    };
    assert_eq!(result, ffi::SQLITE_OK);
    assert!(!file.is_null());
    // SAFETY: SQLite supplied a live file. Copy the immutable methods into this private heap
    // object; only this one descriptor's pointer changes. Other connections keep their tables.
    let original = unsafe { (*file).pMethods };
    assert!(!original.is_null());
    let mut methods = Box::new(unsafe { original.read() });
    methods.xFileSize = Some(unavailable_size);
    // SAFETY: methods is heap-stable and retained by the restoration guard until the claim
    // finishes (including unwind). All other original I/O methods are unchanged.
    unsafe { (*file).pMethods = &*methods };
    let _restore = Restore {
        file,
        original,
        _methods: methods,
    };
    writer.claim_readiness_target(revision, target, 10, 100)
}

#[test]
fn readiness_wal_unknown_vfs_rejects_new_claim_but_not_missing_candidate() {
    let (mut fixture, target) = super::super::feature_artifacts::prepare();
    let mut writer = super::super::feature_artifacts::owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let before = jobs(&writer);
    assert!(matches!(
        claim_with_unknown_vfs(&mut writer, &revision, &target),
        Err(SourceWriterError::Readiness(ReadinessError::WalAdmission(
            WalAdmissionError::ObservationUnavailable { .. }
        )))
    ));
    let mut absent = target.clone();
    absent.scope_id = "absent-file".into();
    assert!(
        claim_with_unknown_vfs(&mut writer, &revision, &absent)
            .unwrap()
            .is_none()
    );
    assert_eq!(jobs(&writer), before);
    assert!(
        writer
            .claim_readiness_target(&revision, &target, 10, 100)
            .unwrap()
            .is_some()
    );
}

#[test]
fn readiness_wal_retained_lease_prevents_unsupported_journal_mode_substitution() {
    let (mut fixture, _target) = super::super::feature_artifacts::prepare();
    let writer = super::super::feature_artifacts::owner(&mut fixture);
    drop(fixture.database);
    let before = jobs(&writer);
    let error = writer
        .catalog_database()
        .connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE")
        .unwrap_err();
    // This source lease deliberately retains its own control connection. SQLite therefore
    // rejects a journal-mode substitution; do not force it closed or report an unexecuted gate.
    assert_eq!(
        error.sqlite_error_code(),
        Some(ffi::ErrorCode::DatabaseBusy)
    );
    let mode: String = writer
        .catalog_database()
        .connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    assert_eq!(jobs(&writer), before);
}
