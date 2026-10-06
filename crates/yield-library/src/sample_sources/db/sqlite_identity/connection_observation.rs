//! Original native connection query; owned refusal text is materialized separately.
use super::{Connection, DatabaseIdentityError, Path};

pub(super) enum Refusal {
    #[cfg(any(unix, windows))]
    Code { control: &'static str, code: i32 },
    #[cfg(unix)]
    Replaced,
    #[cfg(windows)]
    InvalidHandle,
    #[cfg(windows)]
    FileInformation(i32),
    #[cfg(windows)]
    Mismatch,
    #[cfg(not(any(unix, windows)))]
    Unsupported,
}

pub(super) fn observe(connection: &Connection, expected_identity: &str) -> Result<(), Refusal> {
    #[cfg(unix)]
    {
        let mut moved = 0_i32;
        let code = unsafe {
            // Borrow the same original connection; SQLite writes only the scalar.
            rusqlite::ffi::sqlite3_file_control(
                connection.handle(),
                c"main".as_ptr(),
                rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED,
                (&mut moved as *mut i32).cast(),
            )
        };
        if code != rusqlite::ffi::SQLITE_OK {
            return Err(Refusal::Code {
                control: "SQLITE_FCNTL_HAS_MOVED",
                code,
            });
        }
        if moved != 0 {
            return Err(Refusal::Replaced);
        }
        let _ = expected_identity;
        Ok(())
    }
    #[cfg(windows)]
    {
        use windows::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle},
        };
        let mut native_handle = HANDLE(std::ptr::null_mut());
        let code = unsafe {
            rusqlite::ffi::sqlite3_file_control(
                connection.handle(),
                c"main".as_ptr(),
                rusqlite::ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
                (&mut native_handle as *mut HANDLE).cast(),
            )
        };
        if code != rusqlite::ffi::SQLITE_OK {
            return Err(Refusal::Code {
                control: "SQLITE_FCNTL_WIN32_GET_HANDLE",
                code,
            });
        }
        if native_handle.is_invalid() {
            return Err(Refusal::InvalidHandle);
        }
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(native_handle, &mut information) }
            .map_err(|error| Refusal::FileInformation(error.code().0))?;
        let index =
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
        let created = (u64::from(information.ftCreationTime.dwHighDateTime) << 32)
            | u64::from(information.ftCreationTime.dwLowDateTime);
        if !windows_identity_matches(
            information.dwVolumeSerialNumber,
            index,
            created,
            expected_identity,
        ) {
            return Err(Refusal::Mismatch);
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (connection, expected_identity);
        Err(Refusal::Unsupported)
    }
}

#[cfg(any(windows, test))]
fn windows_identity_matches(volume: u32, index: u64, created: u64, expected: &str) -> bool {
    use std::fmt::Write;
    // Exactly "windows:" + decimal u32 + ':' + decimal u64 + ':' + decimal u64.
    struct Identity {
        bytes: [u8; 8 + 10 + 1 + 20 + 1 + 20],
        used: usize,
    }
    impl std::fmt::Write for Identity {
        fn write_str(&mut self, value: &str) -> std::fmt::Result {
            let end = self.used.checked_add(value.len()).ok_or(std::fmt::Error)?;
            self.bytes
                .get_mut(self.used..end)
                .ok_or(std::fmt::Error)?
                .copy_from_slice(value.as_bytes());
            self.used = end;
            Ok(())
        }
    }
    let mut identity = Identity {
        bytes: [0; 60],
        used: 0,
    };
    write!(identity, "windows:{volume}:{index}:{created}")
        .expect("typed decimal identity fits exact stack bound");
    identity.bytes[..identity.used] == *expected.as_bytes()
}

pub(super) fn materialize<E>(
    refusal: Refusal,
    path: &Path,
    mut reserve: impl FnMut(usize) -> Result<(), E>,
) -> Result<DatabaseIdentityError, E> {
    use std::fmt::Write;
    // The same retained path is copied only AFTER its original byte debit.
    reserve(path.as_os_str().as_encoded_bytes().len())?;
    let path = path.to_path_buf();
    let reason = match refusal {
        #[cfg(unix)]
        Refusal::Replaced => return Ok(DatabaseIdentityError::Replaced { path }),
        #[cfg(any(unix, windows))]
        Refusal::Code { control, code } => {
            let mut magnitude = code.unsigned_abs();
            let mut digits = 1 + usize::from(code < 0);
            while magnitude >= 10 {
                magnitude /= 10;
                digits += 1;
            }
            let bytes = control.len() + " returned ".len() + digits;
            reserve(bytes)?;
            let mut reason = String::with_capacity(bytes);
            write!(reason, "{control} returned {code}").expect("String formatting cannot fail");
            reason
        }
        #[cfg(windows)]
        Refusal::InvalidHandle => {
            const REASON: &str = "SQLite returned an invalid native file handle";
            reserve(REASON.len())?;
            REASON.to_owned()
        }
        #[cfg(windows)]
        Refusal::FileInformation(code) => {
            super::native_error_message::win32_reason(code, &mut reserve)?
        }
        #[cfg(windows)]
        Refusal::Mismatch => {
            const REASON: &str = "SQLite native handle identifies a different file";
            reserve(REASON.len())?;
            return Ok(DatabaseIdentityError::ConnectionMismatch {
                path,
                reason: REASON.to_owned(),
            });
        }
        #[cfg(not(any(unix, windows)))]
        Refusal::Unsupported => {
            const REASON: &str = "no verified SQLite connection identity control exists";
            reserve(REASON.len())?;
            REASON.to_owned()
        }
    };
    Ok(DatabaseIdentityError::Unavailable { path, reason })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn memory_vfs_control_refusal_copies_only_after_each_debit() {
        let connection = Connection::open_in_memory().unwrap();
        let refusal = observe(&connection, "unused")
            .err()
            .expect("memory VFS has no native main file control");
        let path = Path::new("original-retained-path");
        let mut calls = [0; 2];
        let mut at = 0;
        let error = materialize(refusal, path, |bytes| {
            calls[at] = bytes;
            at += 1;
            Ok::<_, ()>(())
        })
        .unwrap();
        let DatabaseIdentityError::Unavailable {
            path: copied,
            reason,
        } = error
        else {
            panic!("original unsupported control reason");
        };
        assert_eq!(copied, path);
        assert_eq!(calls[0], path.as_os_str().as_encoded_bytes().len());
        assert_eq!(calls[1], reason.len());
        assert_eq!(
            reason,
            format!(
                "SQLITE_FCNTL_HAS_MOVED returned {}",
                rusqlite::ffi::SQLITE_NOTFOUND
            )
        );
        assert_eq!(at, 2);
    }
    #[test]
    fn exact_decimal_identity_and_refusal_preserve_original_fields() {
        assert!(windows_identity_matches(
            u32::MAX,
            u64::MAX,
            u64::MAX,
            &format!("windows:{}:{}:{}", u32::MAX, u64::MAX, u64::MAX)
        ));
        assert!(!windows_identity_matches(1, 2, 3, "windows:01:2:3"));
        let refused = materialize(
            Refusal::Code {
                control: "SQLITE_FCNTL_HAS_MOVED",
                code: i32::MIN,
            },
            Path::new("retained"),
            |_| Err::<(), _>("original pool pressure"),
        );
        assert!(matches!(refused, Err("original pool pressure")));
    }
}
