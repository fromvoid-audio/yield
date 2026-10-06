//! Original-handle lock observations, never namespace or physical admission authority.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShapeError {
    NotRegular,
    HardLinked,
}

fn shape(
    disk: bool,
    attributes: u32,
    volume: u32,
    object: u64,
    links: u32,
) -> Result<(), ShapeError> {
    // Native FILE_ATTRIBUTE_DIRECTORY, DEVICE and REPARSE_POINT bits.
    if !disk || attributes & (0x10 | 0x40 | 0x400) != 0 || volume == 0 || object == 0 {
        return Err(ShapeError::NotRegular);
    }
    if links != 1 {
        return Err(ShapeError::HardLinked);
    }
    Ok(())
}

fn sid(bytes: &[u8], offset: usize) -> Option<&[u8]> {
    let header = bytes.get(offset..offset.checked_add(8)?)?;
    if header[0] != 1 || header[1] > 15 {
        return None;
    }
    let length = 8 + usize::from(header[1]) * 4;
    bytes.get(offset..offset.checked_add(length)?)
}

fn owner_sid(bytes: &[u8]) -> Option<&[u8]> {
    // GetKernelObjectSecurity returns SECURITY_DESCRIPTOR_RELATIVE. Reject an
    // unexpected layout before interpreting its bounded owner offset.
    let header = bytes.get(..20)?;
    if header[0] != 1 || u16::from_le_bytes(header[2..4].try_into().ok()?) & 0x8000 == 0 {
        return None;
    }
    let offset = u32::from_le_bytes(header[4..8].try_into().ok()?) as usize;
    if offset < 20 || offset % 4 != 0 {
        return None;
    }
    sid(bytes, offset)
}

fn token_sid_offset(base: usize, length: usize, address: usize, header: usize) -> Option<usize> {
    let offset = address.checked_sub(base)?;
    if offset < header || offset % 4 != 0 || offset.checked_add(8)? > length {
        return None;
    }
    Some(offset)
}

#[cfg(windows)]
mod native {
    use super::*;
    use crate::operation_journal::capacity_volume_owner::VolumeOwnershipError;
    use std::{fs::File, os::windows::io::AsRawHandle, path::Path};
    use windows::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{
            GetKernelObjectSecurity, GetTokenInformation, OWNER_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_TYPE_DISK, GetFileInformationByHandle, GetFileType,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    // Fixed aligned backing for each call; no allocation from native length hints.
    #[repr(C, align(16))]
    struct Buffer([u8; 512]);

    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn unavailable(path: &Path, reason: &'static str) -> VolumeOwnershipError {
        VolumeOwnershipError::LockUnavailable {
            path: path.to_path_buf(),
            reason: reason.into(),
        }
    }

    fn information(handle: HANDLE, path: &Path) -> Result<(u32, u64), VolumeOwnershipError> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(handle, &mut info) }
            .map_err(|_| unavailable(path, "original lock native information unavailable"))?;
        let object = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        shape(
            unsafe { GetFileType(handle) } == FILE_TYPE_DISK,
            info.dwFileAttributes,
            info.dwVolumeSerialNumber,
            object,
            info.nNumberOfLinks,
        )
        .map_err(|error| match error {
            ShapeError::NotRegular => VolumeOwnershipError::LockNotRegular {
                path: path.to_path_buf(),
            },
            ShapeError::HardLinked => VolumeOwnershipError::LockHardLinked {
                path: path.to_path_buf(),
            },
        })?;
        Ok((info.dwVolumeSerialNumber, object))
    }

    pub(in crate::operation_journal::capacity_volume_owner) fn validate(
        file: &File,
        path: &Path,
    ) -> Result<(), VolumeOwnershipError> {
        let handle = HANDLE(file.as_raw_handle());
        let identity = information(handle, path)?;
        let mut raw_token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) }
            .map_err(|_| unavailable(path, "original process account token unavailable"))?;
        let token = Token(raw_token);
        let mut token_buffer = Buffer([0; 512]);
        let mut token_length = 0u32;
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(token_buffer.0.as_mut_ptr().cast()),
                512,
                &mut token_length,
            )
        }
        .map_err(|_| unavailable(path, "bounded process account token unavailable"))?;
        let token_length = token_length as usize;
        if !(std::mem::size_of::<TOKEN_USER>()..=512).contains(&token_length) {
            return Err(unavailable(path, "process account token length invalid"));
        }
        // Actual buffer is aligned and contains a full native header. Copy the
        // header; validate its SID pointer against the returned span before reading.
        let user = unsafe { std::ptr::read(token_buffer.0.as_ptr().cast::<TOKEN_USER>()) };
        let offset = token_sid_offset(
            token_buffer.0.as_ptr() as usize,
            token_length,
            user.User.Sid.0 as usize,
            std::mem::size_of::<TOKEN_USER>(),
        )
        .ok_or_else(|| unavailable(path, "process account SID span invalid"))?;
        let account = sid(&token_buffer.0[..token_length], offset)
            .ok_or_else(|| unavailable(path, "process account SID invalid"))?;

        let mut security = Buffer([0; 512]);
        let mut security_length = 0u32;
        unsafe {
            GetKernelObjectSecurity(
                handle,
                OWNER_SECURITY_INFORMATION.0,
                Some(PSECURITY_DESCRIPTOR(security.0.as_mut_ptr().cast())),
                512,
                &mut security_length,
            )
        }
        .map_err(|_| unavailable(path, "bounded original lock owner security unavailable"))?;
        let security_bytes = security
            .0
            .get(..security_length as usize)
            .ok_or_else(|| unavailable(path, "original lock security length invalid"))?;
        let owner = owner_sid(security_bytes)
            .ok_or_else(|| unavailable(path, "original lock owner SID invalid"))?;
        // Binary SID equality includes revision, authority and every subauthority.
        if account != owner {
            return Err(VolumeOwnershipError::LockNotOwned {
                path: path.to_path_buf(),
            });
        }
        if information(handle, path)? != identity {
            return Err(VolumeOwnershipError::LockReplaced {
                path: path.to_path_buf(),
            });
        }
        Ok(())
    }
}

#[cfg(windows)]
pub(super) use native::validate;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_windows_lock_shape_refuses_ambiguous_objects() {
        assert_eq!(shape(true, 0x80, 7, 9, 1), Ok(()));
        for attributes in [0x10, 0x40, 0x400, 0x410] {
            assert_eq!(
                shape(true, attributes, 7, 9, 1),
                Err(ShapeError::NotRegular)
            );
        }
        for (disk, volume, object) in [(false, 7, 9), (true, 0, 9), (true, 7, 0)] {
            assert_eq!(
                shape(disk, 0, volume, object, 1),
                Err(ShapeError::NotRegular)
            );
        }
        for links in [0, 2, u32::MAX] {
            assert_eq!(shape(true, 0, 7, 9, links), Err(ShapeError::HardLinked));
        }
    }

    #[test]
    fn original_windows_lock_sid_spans_are_bounded_before_interpretation() {
        let mut descriptor = [0u8; 32];
        descriptor[0] = 1;
        descriptor[3] = 0x80;
        descriptor[4] = 20;
        descriptor[20] = 1;
        descriptor[21] = 1;
        descriptor[27] = 5;
        descriptor[28] = 42;
        assert_eq!(owner_sid(&descriptor), Some(&descriptor[20..]));
        for length in 0..32 {
            assert!(owner_sid(&descriptor[..length]).is_none());
        }
        for offset in [0u32, 16, 21, 28, u32::MAX] {
            let mut bad = descriptor;
            bad[4..8].copy_from_slice(&offset.to_le_bytes());
            assert!(owner_sid(&bad).is_none());
        }
        for (revision, count) in [(0, 1), (2, 1), (1, 16), (1, 255)] {
            let mut bad = descriptor;
            bad[20] = revision;
            bad[21] = count;
            assert!(owner_sid(&bad).is_none());
        }
        assert_eq!(token_sid_offset(1024, 64, 1040, 16), Some(16));
        for address in [0, 1023, 1024, 1036, 1041, 1084, usize::MAX] {
            assert_eq!(token_sid_offset(1024, 64, address, 16), None);
        }
        assert_eq!(token_sid_offset(usize::MAX - 4, 64, 8, 16), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_original_lock_validator_retains_exact_handle_and_refuses_hardlinks() {
        use crate::operation_journal::capacity_volume_owner::{
            VolumeOwnershipError, validate_lock_file,
        };
        use std::{fs, fs::OpenOptions, os::windows::fs::OpenOptionsExt};
        use windows::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original.lock");
        let retained = temp.path().join("retained.lock");
        let alias = temp.path().join("alias.lock");
        fs::write(&original, b"original marker").unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            .open(&original)
            .unwrap();
        validate_lock_file(&file, &original).unwrap();
        fs::rename(&original, &retained).unwrap();
        fs::write(&original, b"foreign marker").unwrap();
        validate_lock_file(&file, &original).unwrap();
        fs::hard_link(&retained, &alias).unwrap();
        assert!(matches!(
            validate_lock_file(&file, &original),
            Err(VolumeOwnershipError::LockHardLinked { .. })
        ));
        fs::remove_file(&alias).unwrap();
        validate_lock_file(&file, &original).unwrap();
        assert_eq!(fs::read(&original).unwrap(), b"foreign marker");
        assert_eq!(fs::read(&retained).unwrap(), b"original marker");
        let device = fs::File::open("NUL").unwrap();
        assert!(validate_lock_file(&device, std::path::Path::new("NUL")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_original_lock_validator_refuses_genuine_foreign_account_fixture() {
        use crate::operation_journal::capacity_volume_owner::{
            VolumeOwnershipError, validate_lock_file,
        };
        use std::{
            fs::OpenOptions,
            io::{Read, Seek, SeekFrom},
            os::windows::fs::OpenOptionsExt,
        };
        use windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        // Native verification explicitly supplies a disposable singleton owned by a
        // second account. Never create/change its owner or silently skip this assertion.
        let path = std::path::PathBuf::from(
            std::env::var_os("YIELD_WINDOWS_FOREIGN_LOCK_FIXTURE")
                .expect("OPT-2364 requires a controlled foreign-account-owned singleton file"),
        );
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&path)
            .unwrap();
        let length = file.metadata().unwrap().len();
        assert!(
            length <= 128,
            "controlled fixture must have at most 128 bytes"
        );
        let mut before = vec![0; length as usize];
        file.read_exact(&mut before).unwrap();
        assert!(matches!(
            validate_lock_file(&file, &path),
            Err(VolumeOwnershipError::LockNotOwned { .. })
        ));
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut after = vec![0; length as usize];
        file.read_exact(&mut after).unwrap();
        assert_eq!(before, after);
        assert_eq!(file.metadata().unwrap().len(), length);
    }
}
