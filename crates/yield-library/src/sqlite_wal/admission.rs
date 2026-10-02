//! Admission observations from the SQLite connection's retained WAL VFS handle.
use rusqlite::{Connection, ffi};
use thiserror::Error;

/// Initial soft threshold: eligible for off-thread checkpoint catch-up.
pub const WAL_SOFT_WATERMARK_BYTES: u64 = 32 * 1024 * 1024;
/// Initial hard threshold: new WAL-growing work must wait for maintenance.
pub const WAL_HARD_WATERMARK_BYTES: u64 = 64 * 1024 * 1024;

/// Typed, authority-free diagnostic from the existing writable SQLite owner.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum WalAdmissionError {
    /// New work was rejected before its metadata delta or receipt was written.
    #[error(
        "WalHardWatermark: database maintenance is catching up; new work is paused ({observed_bytes} WAL bytes, limit {hard_bytes}); retry after owner maintenance"
    )]
    HardWatermark {
        /// Size reported by the connection's retained WAL descriptor.
        observed_bytes: u64,
        /// Applicable hard admission threshold.
        hard_bytes: u64,
    },
    /// This owner cannot prove its current WAL size; new writes fail closed.
    #[error("WAL observation unavailable: {reason}; retain state and retry owner recovery")]
    ObservationUnavailable {
        /// Fixed bounded diagnostic, never a caller-supplied path or scalar.
        reason: &'static str,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WalAdmissionState {
    Normal { bytes: u64 },
    CheckpointCatchUp { bytes: u64 },
}

impl WalAdmissionState {
    pub(crate) fn observed_bytes(self) -> u64 {
        match self {
            Self::Normal { bytes } | Self::CheckpointCatchUp { bytes } => bytes,
        }
    }
}

fn unavailable(reason: &'static str) -> WalAdmissionError {
    WalAdmissionError::ObservationUnavailable { reason }
}

/// Observe the actual journal descriptor retained by this connection, without a pathname open.
/// Call on the sole owning worker while its write transaction holds SQLite's writer lock.
/// Post-commit maintenance may also take an authority-free size snapshot on that worker;
/// it must not use that snapshot as admission evidence for a later write.
pub(crate) fn retained_wal_bytes(connection: &Connection) -> Result<u64, WalAdmissionError> {
    let mode: String = connection
        .pragma_query_value(Some("main"), "journal_mode", |row| row.get(0))
        .map_err(|_| unavailable("journal mode cannot be read"))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(unavailable("writable database is not in WAL mode"));
    }
    retained_sqlite_file_bytes(connection, ffi::SQLITE_FCNTL_JOURNAL_POINTER)
}

pub(crate) fn retained_database_bytes(connection: &Connection) -> Result<u64, WalAdmissionError> {
    retained_sqlite_file_bytes(connection, ffi::SQLITE_FCNTL_FILE_POINTER)
}

fn retained_sqlite_file_bytes(
    connection: &Connection,
    opcode: i32,
) -> Result<u64, WalAdmissionError> {
    let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
    // SAFETY: rusqlite's non-Sync Connection is held on its sole owner thread; there are no
    // concurrent calls on it. SQLite documents JOURNAL_POINTER as writing a borrowed sqlite3_file
    // pointer (database or journal depending on opcode). It is used only within this call and never closes/escapes.
    let result = unsafe {
        ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            opcode,
            (&mut file as *mut *mut ffi::sqlite3_file).cast(),
        )
    };
    if result != ffi::SQLITE_OK || file.is_null() {
        return Err(unavailable(
            "SQLite did not expose its retained WAL descriptor",
        ));
    }
    // SAFETY: the checked pointer is owned by the still-live, exclusively used Connection.
    // SQLite VFS methods are valid while pMethods is non-null. We call only xFileSize and do not
    // mutate, transfer or retain the file object. A closed/unsupported descriptor rejects.
    let bytes = unsafe {
        let methods = (*file).pMethods;
        if methods.is_null() {
            return Err(unavailable("retained WAL descriptor is closed"));
        }
        let size = (*methods)
            .xFileSize
            .ok_or_else(|| unavailable("WAL VFS does not implement file-size observation"))?;
        let mut bytes: ffi::sqlite3_int64 = -1;
        if size(file, &mut bytes) != ffi::SQLITE_OK {
            return Err(unavailable("retained WAL VFS size query failed"));
        }
        bytes
    };
    u64::try_from(bytes).map_err(|_| unavailable("retained WAL VFS reported a negative size"))
}

pub(crate) fn enforce_wal_admission(
    connection: &Connection,
) -> Result<WalAdmissionState, WalAdmissionError> {
    classify(
        retained_wal_bytes(connection)?,
        WAL_SOFT_WATERMARK_BYTES,
        WAL_HARD_WATERMARK_BYTES,
    )
}

fn classify(bytes: u64, soft: u64, hard: u64) -> Result<WalAdmissionState, WalAdmissionError> {
    if soft == 0 || soft >= hard {
        return Err(unavailable("invalid WAL admission thresholds"));
    }
    if bytes >= hard {
        Err(WalAdmissionError::HardWatermark {
            observed_bytes: bytes,
            hard_bytes: hard,
        })
    } else if bytes >= soft {
        Ok(WalAdmissionState::CheckpointCatchUp { bytes })
    } else {
        Ok(WalAdmissionState::Normal { bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_admission_thresholds_are_inclusive_and_fail_closed() {
        for (bytes, soft_expected) in [(31, false), (32, true), (63, true)] {
            let observed = classify(bytes, 32, 64).unwrap();
            assert_eq!(
                matches!(observed, WalAdmissionState::CheckpointCatchUp { .. }),
                soft_expected
            );
        }
        for bytes in [64, 65, u64::MAX] {
            assert!(matches!(
                classify(bytes, 32, 64),
                Err(WalAdmissionError::HardWatermark { .. })
            ));
        }
        assert!(classify(0, 0, 64).is_err());
        assert!(classify(0, 64, 64).is_err());
    }

    #[test]
    fn wal_admission_reads_actual_retained_vfs_and_preserves_external_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owned.sqlite");
        let mut writer = Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
            CREATE TABLE payload(value BLOB); INSERT INTO payload VALUES (zeroblob(1024));",
            )
            .unwrap();
        let reader =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM payload;")
            .unwrap();
        let before = retained_wal_bytes(&writer).unwrap();
        writer
            .execute("INSERT INTO payload VALUES (zeroblob(65536))", [])
            .unwrap();
        let transaction = writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let after = retained_wal_bytes(&transaction).unwrap();
        assert!(after > before);
        assert_eq!(
            classify(after, before, after + 1).unwrap(),
            WalAdmissionState::CheckpointCatchUp { bytes: after }
        );
        assert!(
            matches!(classify(after, before, after), Err(WalAdmissionError::HardWatermark { observed_bytes, .. }) if observed_bytes==after)
        );
        assert_eq!(
            reader
                .query_row("SELECT count(*) FROM payload", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        transaction.rollback().unwrap();
        reader.execute_batch("ROLLBACK").unwrap();
        writer
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        assert_eq!(retained_wal_bytes(&writer).unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn wal_admission_size_uses_retained_descriptor_after_path_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owned.sqlite");
        let wal_path = dir.path().join("owned.sqlite-wal");
        let held_path = dir.path().join("held-wal");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
            CREATE TABLE payload(value BLOB); INSERT INTO payload VALUES (zeroblob(16384));",
            )
            .unwrap();
        let actual = retained_wal_bytes(&connection).unwrap();
        assert!(actual > 0);
        std::fs::rename(&wal_path, &held_path).unwrap();
        std::fs::write(&wal_path, []).unwrap();
        // Restore the original name before assertions/drop so SQLite teardown never handles
        // a fixture replacement. Observation itself still uses the original open object.
        let observed = retained_wal_bytes(&connection);
        std::fs::remove_file(&wal_path).unwrap();
        std::fs::rename(&held_path, &wal_path).unwrap();
        assert_eq!(observed.unwrap(), actual);
    }

    #[test]
    fn wal_admission_rejects_unsupported_journal_instead_of_treating_it_as_zero() {
        let connection = Connection::open_in_memory().unwrap();
        assert!(matches!(
            enforce_wal_admission(&connection),
            Err(WalAdmissionError::ObservationUnavailable { .. })
        ));
    }
}
