//! Linux pathname adapter for SQLite opens through retained directory capabilities.
//!
//! The bundled Unix VFS uses O_NOFOLLOW for main/rollback/WAL opens and SHM opens. Its
//! xFullPathname nevertheless resolves procfs directory links before those opens, defeating
//! descriptor binding (or rejecting it when SQLITE_OPEN_NOFOLLOW is requested). Preserve only
//! the strict `/proc/self/fd/<directory fd>/<direct child>` form; delegate all I/O unchanged.

use std::ffi::{CStr, c_char, c_int};
use std::sync::OnceLock;

use rusqlite::ffi;

const VFS_NAME: &CStr = c"yield-linux-retained-root-v1";
static REGISTERED: OnceLock<c_int> = OnceLock::new();

/// Register the process-lifetime adapter without changing SQLite's default VFS.
pub(crate) fn vfs_name() -> rusqlite::Result<&'static str> {
    let code = *REGISTERED.get_or_init(|| {
        // SAFETY: SQLite owns the bundled Unix VFS for process lifetime. Initialization and
        // registry mutations use SQLite's mutexes; our OnceLock publishes the adapter once.
        unsafe {
            let code = ffi::sqlite3_initialize();
            if code != ffi::SQLITE_OK {
                return code;
            }
            let unix = ffi::sqlite3_vfs_find(c"unix".as_ptr());
            if unix.is_null() {
                return ffi::SQLITE_NOTFOUND;
            }
            let mut adapter = Box::new(*unix);
            adapter.pNext = std::ptr::null_mut();
            adapter.zName = VFS_NAME.as_ptr();
            adapter.xFullPathname = Some(retained_full_pathname);
            let pointer = Box::into_raw(adapter);
            let code = ffi::sqlite3_vfs_register(pointer, 0);
            if code != ffi::SQLITE_OK {
                drop(Box::from_raw(pointer));
            }
            // A successful registration deliberately retains one fixed-size VFS for process
            // lifetime. SQLite connections may keep this pointer after the registration call.
            code
        }
    });
    if code != ffi::SQLITE_OK {
        return Err(rusqlite::Error::SqliteFailure(
            ffi::Error::new(code),
            Some("retained Linux SQLite VFS is unavailable".into()),
        ));
    }
    Ok("yield-linux-retained-root-v1")
}

fn directory_descriptor(path: &[u8]) -> Option<c_int> {
    let relative = path.strip_prefix(b"/proc/self/fd/")?;
    let separator = relative.iter().position(|byte| *byte == b'/')?;
    let digits = &relative[..separator];
    let leaf = &relative[separator + 1..];
    if digits.is_empty()
        || (digits.len() > 1 && digits[0] == b'0')
        || leaf.is_empty()
        || leaf == b"."
        || leaf == b".."
        || leaf.contains(&b'/')
    {
        return None;
    }
    digits.iter().try_fold(0_i32, |number, byte| {
        if !byte.is_ascii_digit() {
            return None;
        }
        number.checked_mul(10)?.checked_add(i32::from(byte - b'0'))
    })
}

unsafe extern "C" fn retained_full_pathname(
    _: *mut ffi::sqlite3_vfs,
    input: *const c_char,
    capacity: c_int,
    output: *mut c_char,
) -> c_int {
    if input.is_null() || output.is_null() || capacity <= 0 {
        return ffi::SQLITE_CANTOPEN;
    }
    // SAFETY: SQLite supplies a NUL-terminated input and an output buffer of capacity bytes.
    let path = unsafe { CStr::from_ptr(input) }.to_bytes();
    let Some(descriptor) = directory_descriptor(path) else {
        return ffi::SQLITE_CANTOPEN;
    };
    if path.len() >= capacity as usize {
        return ffi::SQLITE_CANTOPEN;
    }
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat initializes the supplied stat on success and does not take ownership of fd.
    if unsafe { libc::fstat(descriptor, metadata.as_mut_ptr()) } != 0 {
        return ffi::SQLITE_CANTOPEN;
    }
    // SAFETY: the successful fstat above initialized metadata.
    if unsafe { metadata.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return ffi::SQLITE_CANTOPEN;
    }
    // SAFETY: length plus NUL fits the writable output capacity. copy permits overlapping
    // buffers if a VFS caller supplies them, as well as SQLite's usual distinct buffers.
    unsafe {
        std::ptr::copy(input, output, path.len() + 1);
    }
    ffi::SQLITE_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::symlink;

    use cap_std::fs::Dir;
    use rusqlite::{Connection, OpenFlags};

    fn open(root: &Dir) -> rusqlite::Result<Connection> {
        Connection::open_with_flags_and_vfs(
            format!("/proc/self/fd/{}/test.db", root.as_raw_fd()),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            vfs_name()?,
        )
    }

    #[test]
    fn rollback_and_wal_sidecars_stay_bound_after_root_replacement() {
        for mode in ["DELETE", "WAL"] {
            let temp = tempfile::tempdir().expect("parent");
            let original = temp.path().join("original");
            let parked = temp.path().join("parked");
            std::fs::create_dir(&original).expect("original root");
            let root = Dir::open_ambient_dir(&original, cap_std::ambient_authority())
                .expect("retained root");
            {
                let connection = open(&root).expect("initial connection");
                connection
                    .execute_batch(&format!(
                        "PRAGMA journal_mode={mode}; CREATE TABLE values_table(value INTEGER);"
                    ))
                    .expect("schema");
            }
            std::fs::rename(&original, &parked).expect("park retained root");
            std::fs::create_dir(&original).expect("replacement root");
            let replacement = Connection::open(original.join("test.db")).expect("replacement");
            replacement
                .execute_batch("CREATE TABLE marker(value INTEGER); INSERT INTO marker VALUES(99);")
                .expect("replacement contents");
            let before = std::fs::read(original.join("test.db")).expect("replacement bytes");

            let connection = open(&root).expect("open in retained root after replacement");
            assert!(
                connection
                    .path()
                    .expect("path")
                    .starts_with("/proc/self/fd/")
            );
            connection
                .execute_batch("BEGIN IMMEDIATE; INSERT INTO values_table VALUES(42);")
                .expect("transaction");
            let suffix = if mode == "DELETE" { "-journal" } else { "-wal" };
            assert!(parked.join(format!("test.db{suffix}")).exists());
            assert!(!original.join(format!("test.db{suffix}")).exists());
            if mode == "WAL" {
                assert!(parked.join("test.db-shm").exists());
                assert!(!original.join("test.db-shm").exists());
                let reader = open(&root).expect("second retained connection");
                assert_eq!(
                    reader
                        .query_row("SELECT COUNT(*) FROM values_table", [], |row| row
                            .get::<_, i64>(0))
                        .expect("uncommitted isolation"),
                    0
                );
            }
            connection.execute_batch("COMMIT").expect("commit");
            drop(connection);
            let reopened = open(&root).expect("reopen retained database");
            assert_eq!(
                reopened
                    .query_row("SELECT value FROM values_table", [], |row| row
                        .get::<_, i64>(0))
                    .expect("retained data"),
                42
            );
            assert_eq!(
                std::fs::read(original.join("test.db")).expect("replacement"),
                before
            );
        }
    }

    #[test]
    fn main_file_symlink_is_rejected_without_touching_target() {
        let temp = tempfile::tempdir().expect("root");
        let root = Dir::open_ambient_dir(temp.path(), cap_std::ambient_authority()).expect("root");
        let outside = temp.path().join("outside.db");
        let outside_connection = Connection::open(&outside).expect("outside database");
        outside_connection
            .execute_batch("CREATE TABLE untouched(value INTEGER);")
            .expect("schema");
        drop(outside_connection);
        let before = std::fs::read(&outside).expect("outside bytes");
        symlink(&outside, temp.path().join("test.db")).expect("database symlink");
        assert!(open(&root).is_err());
        assert_eq!(std::fs::read(&outside).expect("outside unchanged"), before);
    }

    #[test]
    fn sidecar_symlinks_cannot_write_their_targets() {
        for suffix in ["-journal", "-wal", "-shm"] {
            let temp = tempfile::tempdir().expect("root");
            let root =
                Dir::open_ambient_dir(temp.path(), cap_std::ambient_authority()).expect("root");
            let connection = open(&root).expect("database");
            let mode = if suffix == "-journal" {
                "DELETE"
            } else {
                "WAL"
            };
            connection
                .execute_batch(&format!(
                    "PRAGMA journal_mode={mode}; CREATE TABLE data(value);"
                ))
                .expect("schema");
            drop(connection);
            let outside = temp.path().join("outside-sidecar");
            let before = vec![0x5a; 8192];
            std::fs::write(&outside, &before).expect("outside sidecar");
            let sidecar = temp.path().join(format!("test.db{suffix}"));
            symlink(&outside, &sidecar).expect("sidecar symlink");
            let result = open(&root).and_then(|connection| {
                connection.execute_batch("BEGIN IMMEDIATE; INSERT INTO data VALUES(1); COMMIT;")
            });
            assert!(result.is_err(), "{suffix} must fail closed");
            assert_eq!(std::fs::read(&outside).expect("untouched target"), before);
            // Clean up any remaining fixture link before verifying the failed transaction.
            if std::fs::symlink_metadata(&sidecar).is_ok() {
                std::fs::remove_file(sidecar).expect("fixture cleanup");
            }
            let connection = open(&root).expect("database after removing fixture link");
            assert_eq!(
                connection
                    .query_row("SELECT COUNT(*) FROM data", [], |row| row.get::<_, i64>(0))
                    .expect("no failed transaction rows"),
                0
            );
        }
    }

    #[test]
    fn adapter_rejects_ambient_paths_and_descriptor_traversal() {
        let temp = tempfile::tempdir().expect("root");
        let root = Dir::open_ambient_dir(temp.path(), cap_std::ambient_authority()).expect("root");
        for path in [
            temp.path().join("ambient.db"),
            format!("/proc/self/fd/{}/../escape.db", root.as_raw_fd()).into(),
            format!("/proc/self/fd/{}/nested/test.db", root.as_raw_fd()).into(),
        ] {
            assert!(
                Connection::open_with_flags_and_vfs(
                    &path,
                    OpenFlags::SQLITE_OPEN_READ_WRITE
                        | OpenFlags::SQLITE_OPEN_CREATE
                        | OpenFlags::SQLITE_OPEN_NOFOLLOW,
                    vfs_name().expect("adapter"),
                )
                .is_err()
            );
        }
        assert!(!temp.path().join("ambient.db").exists());
    }
}
