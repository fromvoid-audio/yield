//! Low-level SQLite extension loading mechanics.

use std::path::Path;

use rusqlite::Connection;

// Extension initialization may register process-wide auto extensions even when loading fails.
// A later allocation-bound metadata transaction cannot prove those callbacks are write-free.
static EXTENSION_GATE: std::sync::RwLock<()> = std::sync::RwLock::new(());
static EXTENSION_LOAD_ATTEMPTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(super) fn extension_load_attempted() -> bool {
    EXTENSION_LOAD_ATTEMPTED.load(std::sync::atomic::Ordering::Acquire)
}

pub(super) fn try_extension_free_scope() -> Option<std::sync::RwLockReadGuard<'static, ()>> {
    let guard = EXTENSION_GATE.try_read().ok()?;
    (!extension_load_attempted()).then_some(guard)
}

/// Enable SQLite extension loading, load one extension, and disable loading.
pub(super) fn load_extension(conn: &Connection, path: &Path) -> Result<(), rusqlite::Error> {
    let _extension_gate = EXTENSION_GATE
        .write()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    EXTENSION_LOAD_ATTEMPTED.store(true, std::sync::atomic::Ordering::Release);
    unsafe {
        conn.load_extension_enable()?;
    }
    let load_result = unsafe { conn.load_extension(path, Option::<&str>::None) };
    let _ = conn.load_extension_disable();
    load_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn invalid_extension_file_maps_to_sqlite_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("invalid_ext");
        std::fs::write(&path, b"not a sqlite extension").unwrap();
        let conn = Connection::open_in_memory().unwrap();

        assert!(load_extension(&conn, &path).is_err());
    }
}
