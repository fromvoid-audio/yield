//! Permanent pre-open restriction for original existing MAIN preparation, never admission.
use super::*;

impl OwnedConnection {
    pub(crate) fn has_no_effect_fence(&self) -> bool {
        self.registration
            .as_ref()
            .is_some_and(|r| r.context.no_effect)
    }
}

pub(crate) fn open_existing_no_effect(
    path: impl AsRef<std::path::Path>,
    descriptor: Option<std::sync::Arc<std::fs::File>>,
) -> rusqlite::Result<OwnedConnection> {
    if rusqlite::version_number() != 3_050_002 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    super::open_owned_mode(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        descriptor,
        true,
    )
}

// Return before native xFileControl for every opcode outside this exact query/setup list.
// PRAGMA is deliberately handled by SQLite core. No scalar enables writes through this fence.
pub(super) fn control_result(op: c_int) -> Option<c_int> {
    match op {
        ffi::SQLITE_FCNTL_PRAGMA => Some(ffi::SQLITE_NOTFOUND),
        ffi::SQLITE_FCNTL_HAS_MOVED
        | ffi::SQLITE_FCNTL_LOCKSTATE
        | ffi::SQLITE_FCNTL_LAST_ERRNO
        | ffi::SQLITE_FCNTL_VFSNAME
        | ffi::SQLITE_FCNTL_VFS_POINTER
        | ffi::SQLITE_FCNTL_FILE_POINTER
        | ffi::SQLITE_FCNTL_WIN32_GET_HANDLE
        | ffi::SQLITE_FCNTL_DATA_VERSION
        | ffi::SQLITE_FCNTL_BUSYHANDLER
        | ffi::SQLITE_FCNTL_TRACE => None,
        _ => Some(ffi::SQLITE_NOTFOUND),
    }
}

#[cfg(test)]
mod tests;
