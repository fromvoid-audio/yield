//! Bounded identity checks for profile-owned SQLite connections.
//!
//! This module contains only the file and connection identity primitives.  Database schema,
//! journal mode, sidecar, and recovery policy remain owned by each SQLite store that uses these
//! checks.  In particular, a successful identity check is evidence that a connection still
//! refers to the expected file; it does not grant filesystem or database-write authority.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;

use crate::filesystem_identity::stable_filesystem_identity_from_open_file;

use super::BoundDatabaseRoot;

/// Errors returned by a no-follow SQLite file or connection identity check.
#[derive(Debug)]
pub(crate) enum DatabaseIdentityError {
    /// A filesystem operation failed while inspecting the candidate file.
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// The candidate path or open handle is not a regular file.
    NotRegular {
        /// Path whose file type was rejected.
        path: PathBuf,
    },
    /// The pathname no longer identifies the file captured before SQLite opened.
    Replaced {
        /// Database pathname whose identity changed.
        path: PathBuf,
    },
    /// The host or SQLite VFS cannot provide a stable identity check.
    Unavailable {
        /// Path being checked, when one is available.
        path: PathBuf,
        /// Bounded diagnostic for the unavailable primitive.
        reason: String,
    },
    /// SQLite's native connection handle identifies another file.
    #[allow(dead_code)]
    ConnectionMismatch {
        /// Database pathname being checked.
        path: PathBuf,
        /// Bounded diagnostic for the mismatch.
        reason: String,
    },
    /// A source database WAL or shared-memory sidecar violates the existing path policy.
    UnsafeSidecar {
        /// Sidecar path rejected by the source path policy.
        path: PathBuf,
        /// Stable policy reason.
        reason: &'static str,
    },
}

/// One no-follow file opened before SQLite connects to its pathname.
pub(crate) struct OpenedDatabaseFile {
    /// The handle remains alive across SQLite connection setup and the first identity checks.
    pub(crate) file: File,
    /// Whether the file was created by this open attempt.
    pub(crate) created: bool,
    /// Stable identity captured from the no-follow handle.
    pub(crate) identity: String,
}

/// The retained proof that one profile-owned SQLite connection uses one captured main file.
///
/// The root capability is retained here so later checks reopen the direct child without following
/// a replacement symlink.  The proof contains no SQLite connection and is therefore safe to
/// borrow from a readiness store while that store mutably borrows the connection.
pub(crate) struct DatabaseFileBinding {
    root: Arc<BoundDatabaseRoot>,
    name: PathBuf,
    path: PathBuf,
    identity: String,
}

impl DatabaseFileBinding {
    /// Build a retained main-file proof from a file opened through `root`.
    pub(crate) fn new(
        root: Arc<BoundDatabaseRoot>,
        name: impl AsRef<Path>,
        path: impl AsRef<Path>,
        identity: String,
    ) -> Self {
        Self {
            root,
            name: name.as_ref().to_path_buf(),
            path: path.as_ref().to_path_buf(),
            identity,
        }
    }

    /// Return the retained descriptor-bound database root.
    pub(crate) fn root(&self) -> &Arc<BoundDatabaseRoot> {
        &self.root
    }

    /// Return the expected main-file identity.
    pub(crate) fn identity(&self) -> &str {
        &self.identity
    }

    /// Validate only the retained pathname before a new SQLite connection is opened.
    pub(crate) fn validate_path(&self) -> Result<(), DatabaseIdentityError> {
        validate_database_file_identity(&self.root, &self.name, &self.path, &self.identity)
    }

    /// Validate both the no-follow pathname and the SQLite connection's native file handle.
    pub(crate) fn validate(&self, connection: &Connection) -> Result<(), DatabaseIdentityError> {
        validate_database_file_identity(&self.root, &self.name, &self.path, &self.identity)?;
        validate_source_sidecars(&self.root, &self.name, &self.path)?;
        validate_connection_identity(connection, &self.path, &self.identity)
    }
}

/// Open one direct child without following a final symlink, creating it only when absent.
pub(crate) fn open_database_file(
    root: &BoundDatabaseRoot,
    name: &Path,
    path: &Path,
) -> Result<OpenedDatabaseFile, DatabaseIdentityError> {
    let (file, created) = open_database_handle(root, name, path)?;
    ensure_regular_file(&file, path)?;
    let identity = stable_filesystem_identity_from_open_file(&file).ok_or_else(|| {
        DatabaseIdentityError::Unavailable {
            path: path.to_path_buf(),
            reason: String::from("stable identity is unavailable for the open database file"),
        }
    })?;
    Ok(OpenedDatabaseFile {
        file,
        created,
        identity,
    })
}

/// Open one already-existing direct child without allowing a create race.
pub(crate) fn open_existing_database_file(
    root: &BoundDatabaseRoot,
    name: &Path,
    path: &Path,
) -> Result<OpenedDatabaseFile, DatabaseIdentityError> {
    let file = root.open_file_nofollow(name, false).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            DatabaseIdentityError::Replaced {
                path: path.to_path_buf(),
            }
        } else {
            io_error(path, source)
        }
    })?;
    ensure_regular_file(&file, path)?;
    let identity = stable_filesystem_identity_from_open_file(&file).ok_or_else(|| {
        DatabaseIdentityError::Unavailable {
            path: path.to_path_buf(),
            reason: String::from("stable identity is unavailable for the open database file"),
        }
    })?;
    Ok(OpenedDatabaseFile {
        file,
        created: false,
        identity,
    })
}

/// Open one direct child without capturing its identity.
pub(crate) fn open_database_handle(
    root: &BoundDatabaseRoot,
    name: &Path,
    path: &Path,
) -> Result<(File, bool), DatabaseIdentityError> {
    match root.open_file_nofollow(name, false) {
        Ok(file) => Ok((file, false)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match root.open_file_nofollow(name, true) {
                Ok(file) => Ok((file, true)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => root
                    .open_file_nofollow(name, false)
                    .map(|file| (file, false))
                    .map_err(|source| io_error(path, source)),
                Err(source) => return Err(io_error(path, source)),
            }
        }
        Err(source) => return Err(io_error(path, source)),
    }
}

/// Ensure that an already-open database handle refers to a regular file.
pub(crate) fn ensure_regular_file(file: &File, path: &Path) -> Result<(), DatabaseIdentityError> {
    let metadata = file.metadata().map_err(|source| io_error(path, source))?;
    if !metadata.is_file() {
        return Err(DatabaseIdentityError::NotRegular {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// Reopen one direct child without following links and compare its stable identity.
pub(crate) fn validate_database_file_identity(
    root: &BoundDatabaseRoot,
    name: &Path,
    path: &Path,
    expected_identity: &str,
) -> Result<(), DatabaseIdentityError> {
    validate_database_file_identity_with_unavailable_reason(
        root,
        name,
        path,
        expected_identity,
        "stable identity is unavailable for the open database file",
    )
}

/// Reopen a direct child, compare its stable identity, and retain a caller-specific unavailable
/// diagnostic for stores that need to preserve their existing error context.
pub(crate) fn validate_database_file_identity_with_unavailable_reason(
    root: &BoundDatabaseRoot,
    name: &Path,
    path: &Path,
    expected_identity: &str,
    unavailable_reason: &str,
) -> Result<(), DatabaseIdentityError> {
    let file = root.open_file_nofollow(name, false).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            DatabaseIdentityError::Replaced {
                path: path.to_path_buf(),
            }
        } else {
            io_error(path, source)
        }
    })?;
    ensure_regular_file(&file, path)?;
    let actual_identity = stable_filesystem_identity_from_open_file(&file).ok_or_else(|| {
        DatabaseIdentityError::Unavailable {
            path: path.to_path_buf(),
            reason: unavailable_reason.to_owned(),
        }
    })?;
    if actual_identity != expected_identity {
        return Err(DatabaseIdentityError::Replaced {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

pub(crate) fn validate_source_sidecars(
    root: &BoundDatabaseRoot,
    database_name: &Path,
    database_path: &Path,
) -> Result<(), DatabaseIdentityError> {
    for suffix in ["-wal", "-shm"] {
        let mut sidecar_name = database_name.as_os_str().to_os_string();
        sidecar_name.push(suffix);
        let sidecar_path = database_path.with_file_name(&sidecar_name);
        match root.symlink_metadata(Path::new(&sidecar_name)) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(DatabaseIdentityError::UnsafeSidecar {
                    path: sidecar_path,
                    reason: "source database files and sidecars must not be symlinks",
                });
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(DatabaseIdentityError::UnsafeSidecar {
                    path: sidecar_path,
                    reason: "source database paths must be regular files",
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(&sidecar_path, source)),
        }
    }
    Ok(())
}

/// Verify that SQLite's native `main` handle still identifies the captured file.
pub(crate) fn validate_connection_identity(
    connection: &Connection,
    path: &Path,
    expected_identity: &str,
) -> Result<(), DatabaseIdentityError> {
    #[cfg(unix)]
    {
        let mut moved = 0_i32;
        let return_code = unsafe {
            // Connection::handle is valid for the duration of this borrow. SQLite writes only
            // the boolean result into moved; the operation does not retain any pointer.
            rusqlite::ffi::sqlite3_file_control(
                connection.handle(),
                c"main".as_ptr(),
                rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED,
                (&mut moved as *mut i32).cast(),
            )
        };
        if return_code != rusqlite::ffi::SQLITE_OK {
            return Err(DatabaseIdentityError::Unavailable {
                path: path.to_path_buf(),
                reason: format!("SQLITE_FCNTL_HAS_MOVED returned {return_code}"),
            });
        }
        if moved != 0 {
            return Err(DatabaseIdentityError::Replaced {
                path: path.to_path_buf(),
            });
        }
        let _ = expected_identity;
        Ok(())
    }

    #[cfg(windows)]
    {
        use std::ptr::null_mut;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };

        let mut native_handle = HANDLE(null_mut());
        let return_code = unsafe {
            // SQLite returns a borrowed native handle. Inspect it without closing or wrapping it
            // in an owning Rust file object.
            rusqlite::ffi::sqlite3_file_control(
                connection.handle(),
                c"main".as_ptr(),
                rusqlite::ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
                (&mut native_handle as *mut HANDLE).cast(),
            )
        };
        if return_code != rusqlite::ffi::SQLITE_OK {
            return Err(DatabaseIdentityError::Unavailable {
                path: path.to_path_buf(),
                reason: format!("SQLITE_FCNTL_WIN32_GET_HANDLE returned {return_code}"),
            });
        }
        if native_handle.is_invalid() {
            return Err(DatabaseIdentityError::Unavailable {
                path: path.to_path_buf(),
                reason: String::from("SQLite returned an invalid native file handle"),
            });
        }
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(native_handle, &mut information) }.map_err(
            |error| DatabaseIdentityError::Unavailable {
                path: path.to_path_buf(),
                reason: format!("GetFileInformationByHandle failed: {error}"),
            },
        )?;
        let actual_identity = format_windows_identity(&information);
        if actual_identity != expected_identity {
            return Err(DatabaseIdentityError::ConnectionMismatch {
                path: path.to_path_buf(),
                reason: String::from("SQLite native handle identifies a different file"),
            });
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (connection, expected_identity);
        Err(DatabaseIdentityError::Unavailable {
            path: path.to_path_buf(),
            reason: String::from("no verified SQLite connection identity control exists"),
        })
    }
}

#[cfg(windows)]
fn format_windows_identity(
    information: &windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION,
) -> String {
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    let creation_time = (u64::from(information.ftCreationTime.dwHighDateTime) << 32)
        | u64::from(information.ftCreationTime.dwLowDateTime);
    format!(
        "windows:{}:{}:{}",
        information.dwVolumeSerialNumber, file_index, creation_time
    )
}

fn io_error(path: &Path, source: io::Error) -> DatabaseIdentityError {
    DatabaseIdentityError::Io {
        path: path.to_path_buf(),
        source,
    }
}
