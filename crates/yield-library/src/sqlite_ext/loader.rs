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
    // Optional initialization must fail fast while metadata scopes hold the gate, including
    // a same-thread caller. No callback has run yet, so this rejection does not taint the bound.
    let _extension_gate = EXTENSION_GATE
        .try_write()
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
        if std::env::var_os("YIELD_SQLITE_EXTENSION_LOADER_CHILD").is_none() {
            use std::{
                process::{Command, Stdio},
                time::{Duration, Instant},
            };
            // Attempted initialization is a permanent production process fact. Test it in
            // an actual child; never reset it or poison unrelated owned allocation fixtures.
            let before = extension_load_attempted();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "sqlite_ext::loader::tests::invalid_extension_file_maps_to_sqlite_error",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("YIELD_SQLITE_EXTENSION_LOADER_CHILD", "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let _ = child.wait();
                    panic!("actual extension-loader child did not finish within its bound");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "actual loader child: {stdout} {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                stdout.contains("running 1 test\n")
                    && stdout.contains("isolated-loader-negative sticky_attempted=true"),
                "exact loader case must execute: {stdout}"
            );
            assert_eq!(extension_load_attempted(), before);
            return;
        }
        assert!(!extension_load_attempted());
        let dir = tempdir().unwrap();
        let path = dir.path().join("invalid_ext");
        std::fs::write(&path, b"not a sqlite extension").unwrap();
        let conn = Connection::open_in_memory().unwrap();

        assert!(load_extension(&conn, &path).is_err());
        assert!(extension_load_attempted());
        assert!(try_extension_free_scope().is_none());
        println!("isolated-loader-negative sticky_attempted=true");
    }
}
