//! Attribute-only nested source observations; data access uses the exact held leaf.
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    serial: u32,
    index: u64,
    created: u64,
}

fn shape(kind: u32, attributes: u32, id: Identity, directory: bool) -> io::Result<Identity> {
    if kind != 1
        || attributes & (0x40 | 0x400) != 0
        || (attributes & 0x10 != 0) != directory
        || id.serial == 0
        || id.index == 0
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(id)
}

#[cfg(windows)]
fn identity(file: &std::fs::File, directory: bool) -> io::Result<Identity> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle, GetFileType,
        },
    };
    let handle = HANDLE(file.as_raw_handle());
    let kind = unsafe { GetFileType(handle) };
    if kind.0 != 1 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle, &mut info) }
        .map_err(|_| io::Error::from(io::ErrorKind::Other))?;
    shape(
        kind.0,
        info.dwFileAttributes,
        Identity {
            serial: info.dwVolumeSerialNumber,
            index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            created: (u64::from(info.ftCreationTime.dwHighDateTime) << 32)
                | u64::from(info.ftCreationTime.dwLowDateTime),
        },
        directory,
    )
}

#[cfg(windows)]
pub(super) fn component(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    directory: bool,
) -> io::Result<std::fs::File> {
    use cap_fs_ext::OpenOptionsFollowExt;
    use cap_primitives::fs::{FollowSymlinks, OpenOptions, OpenOptionsExt};
    use std::{os::windows::ffi::OsStrExt, path::Component};
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };
    let path = std::path::Path::new(name);
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || name.encode_wide().take(1025).count() > 1024
        || name.encode_wide().any(|c| c == 0 || c == u16::from(b':'))
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    // Verify this retained parent before the child open. cap-primitives' pinned
    // Windows backend passes this original handle as NtCreateFile RootDirectory.
    let parent_id = identity(parent, true)?;
    let dir = cap_std::fs::Dir::from_std_file(parent.try_clone()?);
    let mut options = OpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0);
    let child = dir.open_with(path, &options)?.into_std();
    identity(&child, directory)?;
    if identity(parent, true)? != parent_id {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(child)
}

#[cfg(windows)]
pub(super) fn read(original: &std::fs::File) -> io::Result<std::fs::File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, ReOpenFile,
        },
    };
    let expected = identity(original, false)?;
    // SAFETY: regular disk/nonreparse attributes were established before this
    // first data-access request. ReOpenFile addresses that object, never its name.
    let handle = unsafe {
        ReOpenFile(
            HANDLE(original.as_raw_handle()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_OPEN_REPARSE_POINT,
        )
    }
    .map_err(|_| io::Error::from(io::ErrorKind::Other))?;
    // SAFETY: successful ReOpenFile transfers one uniquely owned valid handle.
    let file = unsafe { std::fs::File::from_raw_handle(handle.0) };
    if identity(original, false)? != expected || identity(&file, false)? != expected {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_index_native_shape_refuses_before_data_access() {
        let id = Identity {
            serial: 7,
            index: 11,
            created: 13,
        };
        assert_eq!(shape(1, 0x80, id, false).unwrap(), id);
        assert_eq!(shape(1, 0x10, id, true).unwrap(), id);
        for directory in [false, true] {
            let valid = if directory { 0x10 } else { 0x80 };
            for kind in [0, 2, 3, 0x8001] {
                assert!(shape(kind, valid, id, directory).is_err());
            }
            for flag in [0x40, 0x400] {
                assert!(shape(1, valid | flag, id, directory).is_err());
            }
            assert!(shape(1, valid, Identity { serial: 0, ..id }, directory).is_err());
            assert!(shape(1, valid, Identity { index: 0, ..id }, directory).is_err());
        }
        assert!(shape(1, 0x10, id, false).is_err());
        assert!(shape(1, 0x80, id, true).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn original_index_exact_held_leaf_reads_original_bytes_after_name_substitution() {
        use std::io::{Read, Write};
        let temp = tempfile::tempdir().unwrap();
        let root = super::super::open_root_nofollow(temp.path()).unwrap();
        std::fs::create_dir(temp.path().join("nested")).unwrap();
        std::fs::write(temp.path().join("nested/leaf"), b"original").unwrap();
        let nested = component(&root, std::ffi::OsStr::new("nested"), true).unwrap();
        let leaf = component(&nested, std::ffi::OsStr::new("leaf"), false).unwrap();
        let id = identity(&leaf, false).unwrap();
        std::fs::rename(
            temp.path().join("nested/leaf"),
            temp.path().join("nested/parked"),
        )
        .unwrap();
        std::fs::write(temp.path().join("nested/leaf"), b"foreign").unwrap();
        let mut original = read(&leaf).unwrap();
        assert_eq!(identity(&original, false).unwrap(), id);
        let mut bytes = Vec::new();
        original.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original");
        assert!(original.write_all(b"forbidden").is_err());
        let current = component(&nested, std::ffi::OsStr::new("leaf"), false).unwrap();
        assert_ne!(identity(&current, false).unwrap(), id);
        assert!(component(&root, std::ffi::OsStr::new("nested"), false).is_err());
        assert!(read(&nested).is_err());
        for invalid in ["NUL", "../leaf", "leaf:stream", "nested/leaf", ".", ""] {
            assert!(component(&root, std::ffi::OsStr::new(invalid), false).is_err());
        }
        assert_eq!(
            std::fs::read(temp.path().join("nested/parked")).unwrap(),
            b"original"
        );
        assert_eq!(
            std::fs::read(temp.path().join("nested/leaf")).unwrap(),
            b"foreign"
        );
    }

    #[cfg(windows)]
    #[test]
    fn original_index_device_handle_refuses_before_read_reopen() {
        let device = std::fs::OpenOptions::new().read(true).open("NUL").unwrap();
        assert!(read(&device).is_err());
    }
}
