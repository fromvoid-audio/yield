//! Original retained-root observation. Attributes precede data access; no effect authority.
use std::io;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Identity {
    serial: u32,
    index: u64,
    created: u64,
}

fn regular_identity(kind: u32, attributes: u32, identity: Identity) -> io::Result<Identity> {
    // FILE_TYPE_DISK; refuse directories, device attributes and opened reparse
    // points before requesting file data. Unknown native IDs cannot prove reopening.
    if kind != 1
        || attributes & (0x10 | 0x40 | 0x400) != 0
        || identity.serial == 0
        || identity.index == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown or nonregular observation object",
        ));
    }
    Ok(identity)
}

#[cfg(windows)]
fn identity(file: &std::fs::File) -> io::Result<Identity> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle, GetFileType,
        },
    };
    let handle = HANDLE(file.as_raw_handle());
    let kind = unsafe { GetFileType(handle) };
    // Reject pipes/devices before even requesting their file-information record.
    if kind.0 != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a disk observation",
        ));
    }
    let mut record = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle, &mut record) }.map_err(io::Error::other)?;
    regular_identity(
        kind.0,
        record.dwFileAttributes,
        Identity {
            serial: record.dwVolumeSerialNumber,
            index: (u64::from(record.nFileIndexHigh) << 32) | u64::from(record.nFileIndexLow),
            created: (u64::from(record.ftCreationTime.dwHighDateTime) << 32)
                | u64::from(record.ftCreationTime.dwLowDateTime),
        },
    )
}

#[cfg(windows)]
fn attributes(
    root: &super::BoundDatabaseRoot,
    name: &std::path::Path,
) -> io::Result<std::fs::File> {
    use cap_fs_ext::OpenOptionsFollowExt;
    use cap_primitives::fs::{FollowSymlinks, OpenOptions, OpenOptionsExt};
    use std::{os::windows::ffi::OsStrExt, path::Component};
    use windows::Win32::Storage::FileSystem::{
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let mut components = name.components();
    if !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || name.as_os_str().encode_wide().take(4097).count() > 4096
        || name.as_os_str().encode_wide().any(|c| c == u16::from(b':'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "observation requires one bounded direct child, without streams",
        ));
    }
    let directory = cap_std::fs::Dir::from_std_file(root._capability.try_clone()?);
    let mut options = OpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0);
    directory
        .open_with(name, &options)
        .map(|file| file.into_std())
}

#[cfg(windows)]
fn reopen_readonly(attributes: &std::fs::File) -> io::Result<std::fs::File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, ReOpenFile,
        },
    };
    let original = identity(attributes)?;
    // SAFETY: only a verified original disk/regular/non-reparse handle reaches
    // this call. ReOpenFile uses that object, never a name; it requests readonly
    // access and creates one independent owned handle, with no create/write flags.
    let handle = unsafe {
        ReOpenFile(
            HANDLE(attributes.as_raw_handle()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_OPEN_REPARSE_POINT,
        )
    }
    .map_err(io::Error::other)?;
    // SAFETY: successful ReOpenFile transfers exactly one valid owned handle.
    // File Drop closes it on every subsequent error; the original stays borrowed.
    let file = unsafe { std::fs::File::from_raw_handle(handle.0) };
    if identity(attributes)? != original || identity(&file)? != original {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "observation object changed during handle reopen",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
pub(super) fn open(
    root: &super::BoundDatabaseRoot,
    name: &std::path::Path,
) -> io::Result<std::fs::File> {
    let attributes = attributes(root, name)?;
    reopen_readonly(&attributes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nonregular_unknown_or_reparse_native_records_refuse_before_data_access() {
        let id = Identity {
            serial: 7,
            index: 11,
            created: 13,
        };
        assert_eq!(regular_identity(1, 0x80, id).unwrap(), id);
        for kind in [0, 2, 3, 0x8001] {
            assert!(regular_identity(kind, 0x80, id).is_err());
        }
        for flag in [0x10, 0x40, 0x400] {
            assert!(regular_identity(1, 0x80 | flag, id).is_err());
        }
        assert!(regular_identity(1, 0x80, Identity { serial: 0, ..id }).is_err());
        assert!(regular_identity(1, 0x80, Identity { index: 0, ..id }).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn native_original_root_regular_reads_and_original_handle_survive_name_replacement() {
        use crate::app_dirs::{AppRootGuard, WritableProfileGuard};
        use std::{
            io::{Read, Write},
            path::Path,
        };
        let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
        let temp = tempfile::tempdir().unwrap();
        let _app = AppRootGuard::set(temp.path().to_owned()).unwrap();
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let root = super::super::BoundDatabaseRoot::for_profile_guard(&guard).unwrap();
        let name = Path::new("observation");
        let path = root.path().join(name);
        std::fs::write(&path, b"original").unwrap();
        let attribute_handle = attributes(&root, name).unwrap();
        let original_id = identity(&attribute_handle).unwrap();
        std::fs::rename(&path, root.path().join("parked")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        let mut original = reopen_readonly(&attribute_handle).unwrap();
        assert_eq!(identity(&original).unwrap(), original_id);
        let mut bytes = Vec::new();
        original.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original");
        assert!(original.write_all(b"forbidden").is_err());
        let mut current = root.open_observation_file_nofollow(name).unwrap();
        assert_ne!(identity(&current).unwrap(), original_id);
        bytes.clear();
        current.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"replacement");
        std::fs::create_dir(root.path().join("directory")).unwrap();
        for invalid in [
            "directory",
            "../escape",
            "nested/child",
            ".",
            "observation:stream",
        ] {
            assert!(
                root.open_observation_file_nofollow(Path::new(invalid))
                    .is_err()
            );
        }
        assert_eq!(
            std::fs::read(root.path().join("parked")).unwrap(),
            b"original"
        );
        assert_eq!(std::fs::read(path).unwrap(), b"replacement");
        guard.validate_current().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn native_symlink_child_is_preserved_and_refused_before_data_access() {
        use crate::app_dirs::{AppRootGuard, WritableProfileGuard};
        use std::path::Path;
        let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
        let temp = tempfile::tempdir().unwrap();
        let _app = AppRootGuard::set(temp.path().to_owned()).unwrap();
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let root = super::super::BoundDatabaseRoot::for_profile_guard(&guard).unwrap();
        let target = root.path().join("target");
        std::fs::write(&target, b"preserved").unwrap();
        let link = root.path().join("link");
        // Genuine Windows symlink privilege/Developer Mode is required; absence
        // is an explicit native verification blocker, never a skipped success.
        std::os::windows::fs::symlink_file(&target, &link).unwrap();
        assert!(
            root.open_observation_file_nofollow(Path::new("link"))
                .is_err()
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(target).unwrap(), b"preserved");
        guard.validate_current().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn native_character_device_never_reaches_data_reopen() {
        let file = std::fs::OpenOptions::new().read(true).open("NUL").unwrap();
        assert!(reopen_readonly(&file).is_err());
    }
}
