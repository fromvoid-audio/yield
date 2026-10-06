//! Read/share custody from the original object. No created-stage or effect authority.
use super::{BindingError, validate_windows_held_staging};
use crate::filesystem_identity::stable_filesystem_identity_from_open_file;
use std::{
    fs::File,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle},
};
use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_READ, ReOpenFile,
    },
};

/// Consume only the original readonly observation handle. Existing write/delete handles
/// conflict under Windows' symmetric sharing checks; subsequent such opens conflict until
/// this returned handle closes. Mappings can outlive handles and are outside that guarantee.
pub(super) fn retain(original: File) -> io::Result<File> {
    let validate = |file: &File| {
        validate_windows_held_staging(file)
            .map_err(|_: BindingError| io::Error::other("invalid original staging shape"))
    };
    validate(&original)?;
    let expected = stable_filesystem_identity_from_open_file(&original)
        .ok_or_else(|| io::Error::other("unknown original staging identity"))?;
    // SAFETY: validated live disk/regular/nonreparse original, readonly desired access,
    // no locator, no creation, no mutation and no namespace/delete access.
    let handle = unsafe {
        ReOpenFile(
            HANDLE(original.as_raw_handle()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ,
            FILE_FLAG_OPEN_REPARSE_POINT,
        )
    }
    .map_err(io::Error::other)?;
    // SAFETY: successful ReOpenFile transfers one owned handle, closed on every error path.
    let retained = unsafe { File::from_raw_handle(handle.0) };
    validate(&original)?;
    validate(&retained)?;
    if stable_filesystem_identity_from_open_file(&original).as_deref() != Some(&expected)
        || stable_filesystem_identity_from_open_file(&retained).as_deref() != Some(&expected)
    {
        return Err(io::Error::other("original staging identity changed"));
    }
    Ok(retained)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt, sync::Arc};
    use windows::Win32::Storage::FileSystem::{DELETE, FILE_SHARE_DELETE, FILE_SHARE_WRITE};
    const ALL_SHARING: u32 = FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0;
    fn readonly(path: &std::path::Path) -> File {
        OpenOptions::new()
            .read(true)
            .share_mode(ALL_SHARING)
            .open(path)
            .unwrap()
    }
    #[test]
    fn windows_staging_share_original_read_lease_blocks_handles_until_last_alias_drop() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("stage.bin");
        std::fs::write(&path, b"original staged bytes").unwrap();
        let original = readonly(&path);
        let expected = stable_filesystem_identity_from_open_file(&original).unwrap();
        let lease = Arc::new(retain(original).unwrap());
        let alias = lease.clone();
        assert_eq!(
            stable_filesystem_identity_from_open_file(&lease).unwrap(),
            expected
        );
        let reader = readonly(&path);
        drop(reader);
        assert!(
            OpenOptions::new()
                .write(true)
                .share_mode(ALL_SHARING)
                .open(&path)
                .is_err()
        );
        assert!(
            OpenOptions::new()
                .access_mode(DELETE.0)
                .share_mode(ALL_SHARING)
                .open(&path)
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"original staged bytes");
        drop(lease);
        assert!(
            OpenOptions::new()
                .write(true)
                .share_mode(ALL_SHARING)
                .open(&path)
                .is_err()
        );
        drop(alias);
        drop(
            OpenOptions::new()
                .write(true)
                .share_mode(ALL_SHARING)
                .open(&path)
                .unwrap(),
        );
        drop(
            OpenOptions::new()
                .access_mode(DELETE.0)
                .share_mode(ALL_SHARING)
                .open(&path)
                .unwrap(),
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"original staged bytes");
    }
    #[test]
    fn windows_staging_share_existing_write_or_delete_handle_refuses_without_effect() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("stage.bin");
        std::fs::write(&path, b"retained on refusal").unwrap();
        for access in [
            windows::Win32::Storage::FileSystem::FILE_GENERIC_WRITE.0,
            DELETE.0,
        ] {
            let foreign = OpenOptions::new()
                .access_mode(access)
                .share_mode(ALL_SHARING)
                .open(&path)
                .unwrap();
            let original = readonly(&path);
            assert!(retain(original).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"retained on refusal");
            drop(foreign);
            drop(retain(readonly(&path)).unwrap());
        }
    }
}
