//! Bounded original-handle final namespace capture; no reopen or effect authority.
use std::io;

// Existing readonly-root backing limit, including the native namespace prefix.
const MAX_ENCODED_BYTES: usize = 4096;
const MAX_NAMESPACE_UNITS: usize = 4096;
pub(super) const BUFFER_UNITS: usize = MAX_NAMESPACE_UNITS + 1;

// Fixed 8194-byte query buffer. Native conversion separately checks retained capacity.
// The callback is private and FnOnce: the actual Windows caller performs one query.
pub(super) fn capture(
    buffer: &mut [u16; BUFFER_UNITS],
    query: impl FnOnce(&mut [u16]) -> io::Result<u32>,
) -> io::Result<&[u16]> {
    let count = usize::try_from(query(buffer)?).map_err(|_| refused())?;
    // Windows success excludes the terminator; insufficient space returns a
    // required capacity including it. Never interpret a required length as data.
    if count == 0 || count >= buffer.len() {
        return Err(refused());
    }
    let wide = &buffer[..count];
    validate_wide(wide)?;
    Ok(wide)
}
fn validate_wide(wide: &[u16]) -> io::Result<()> {
    if wide.is_empty() || wide.len() > MAX_NAMESPACE_UNITS || wide.contains(&0) {
        return Err(refused());
    }
    // Windows OsString uses lossless WTF-8 backing. Paired surrogates become one
    // four-byte scalar; each unpaired surrogate retains three bytes. Check the
    // exact encoded size before OsString/PathBuf allocates, without lossy decoding.
    let bytes = char::decode_utf16(wide.iter().copied()).try_fold(0usize, |bytes, c| {
        bytes.checked_add(c.map_or(3, char::len_utf8))
    });
    if bytes.is_none_or(|bytes| bytes > MAX_ENCODED_BYTES) {
        return Err(refused());
    }
    Ok(())
}
#[cfg(windows)]
pub(super) fn into_path(wide: &[u16]) -> io::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    validate_wide(wide)?;
    let mut path = std::ffi::OsString::from_wide(wide);
    // Conversion can geometrically overallocate for multi-byte native units.
    // Do not retain that slack or assume shrink succeeds on every allocator.
    path.shrink_to_fit();
    if path.capacity() > MAX_ENCODED_BYTES {
        return Err(refused());
    }
    Ok(std::path::PathBuf::from(path))
}

fn refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "unknown or oversized original namespace",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_original_query_accepts_exact_backing_boundary_without_resize() {
        let mut buffer = [0; BUFFER_UNITS];
        let mut calls = 0;
        let wide = capture(&mut buffer, |buffer| {
            calls += 1;
            assert_eq!(buffer.len(), BUFFER_UNITS);
            buffer[..MAX_NAMESPACE_UNITS].fill(u16::from(b'x'));
            Ok(MAX_NAMESPACE_UNITS as u32)
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(wide.len(), MAX_ENCODED_BYTES);
        assert_eq!(buffer.len(), BUFFER_UNITS);
    }

    #[test]
    fn required_capacity_zero_and_extreme_lengths_refuse_without_second_query() {
        for count in [0, BUFFER_UNITS as u32, BUFFER_UNITS as u32 + 1, u32::MAX] {
            let mut buffer = [0; BUFFER_UNITS];
            let mut calls = 0;
            assert!(
                capture(&mut buffer, |buffer| {
                    calls += 1;
                    buffer.fill(u16::from(b'x'));
                    Ok(count)
                })
                .is_err()
            );
            assert_eq!(calls, 1, "required capacity must never trigger retry");
        }
        let mut buffer = [0; BUFFER_UNITS];
        let error = capture(&mut buffer, |_| Err(io::Error::from_raw_os_error(5))).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(5));
    }

    #[test]
    fn interior_nul_and_unwritten_reported_data_refuse_without_truncation() {
        for data in [[65, 0, 66], [0, 65, 66], [65, 66, 0]] {
            let mut buffer = [0; BUFFER_UNITS];
            assert!(
                capture(&mut buffer, |buffer| {
                    buffer[..3].copy_from_slice(&data);
                    Ok(3)
                })
                .is_err()
            );
        }
        let mut buffer = [0; BUFFER_UNITS];
        assert!(capture(&mut buffer, |_| Ok(1)).is_err());
    }

    #[test]
    fn lossless_unicode_units_use_checked_exact_encoded_backing_bound() {
        // 1024 surrogate pairs occupy exactly 4096 encoded bytes, not 6144.
        let mut buffer = [0; BUFFER_UNITS];
        let wide = capture(&mut buffer, |buffer| {
            for pair in buffer[..2048].chunks_exact_mut(2) {
                pair.copy_from_slice(&[0xd800, 0xdc00]);
            }
            Ok(2048)
        })
        .unwrap();
        assert_eq!(wide.len(), 2048);
        let mut buffer = [0; BUFFER_UNITS];
        assert!(
            capture(&mut buffer, |buffer| {
                for pair in buffer[..2050].chunks_exact_mut(2) {
                    pair.copy_from_slice(&[0xd800, 0xdc00]);
                }
                Ok(2050)
            })
            .is_err()
        );
        // Preserve unpaired high/low surrogates rather than replacing them.
        let mut buffer = [0; BUFFER_UNITS];
        let wide = capture(&mut buffer, |buffer| {
            buffer[..4].copy_from_slice(&[0xd800, 65, 0xdc00, 66]);
            Ok(4)
        })
        .unwrap();
        assert_eq!(wide, &[0xd800, 65, 0xdc00, 66]);
        let mut buffer = [0; BUFFER_UNITS];
        let wide = capture(&mut buffer, |buffer| {
            buffer[..1365].fill(0xdc00);
            buffer[1365] = 65;
            Ok(1366)
        })
        .unwrap();
        assert_eq!(wide.len(), 1366); // 1365*3 + 1 == 4096 encoded bytes.
        let mut buffer = [0; BUFFER_UNITS];
        assert!(
            capture(&mut buffer, |buffer| {
                buffer[..1366].fill(0xdc00);
                Ok(1366)
            })
            .is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_original_directory_query_preserves_alias_identity_and_delete_barrier() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("original-root");
        std::fs::create_dir(&root).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
            .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
            .open(&root)
            .unwrap();
        let identity = super::super::root_identity_from_open_file(&file).unwrap();
        let path = super::super::windows_final_path(&file).unwrap();
        assert!(path.as_os_str().as_encoded_bytes().len() <= MAX_ENCODED_BYTES);
        assert!(path.capacity() <= MAX_ENCODED_BYTES);
        super::super::validate_database_root_alias(&file, &path, &identity).unwrap();
        // A final namespace string cannot substitute for the held original object.
        assert!(std::fs::rename(&root, temp.path().join("moved-root")).is_err());
        let copy = file.try_clone().unwrap();
        let copied_path = super::super::windows_final_path(&copy).unwrap();
        assert_eq!(copied_path, path);
        super::super::validate_database_root_alias(&copy, &copied_path, &identity).unwrap();
        drop(copy);
        drop(file);
        std::fs::rename(&root, temp.path().join("moved-root")).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn native_oversized_original_directory_namespace_refuses_without_path_fallback() {
        use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt};
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        let temp = tempfile::tempdir().unwrap();
        // Windows canonicalize supplies the extended namespace, avoiding a test
        // dependence on the unrelated application path parser or MAX_PATH mode.
        let mut root = std::fs::canonicalize(temp.path()).unwrap();
        let component = "a".repeat(120);
        while root.as_os_str().encode_wide().count() <= MAX_NAMESPACE_UNITS {
            root.push(&component);
        }
        assert!(root.as_os_str().encode_wide().count() <= MAX_NAMESPACE_UNITS + 121);
        std::fs::create_dir_all(&root).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
            .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
            .open(&root)
            .unwrap();
        let identity = super::super::root_identity_from_open_file(&file).unwrap();
        assert!(matches!(
            super::super::windows_final_path(&file),
            Err(super::super::BoundDatabaseRootError::Unavailable { .. })
        ));
        assert_eq!(
            super::super::root_identity_from_open_file(&file).unwrap(),
            identity
        );
        assert!(
            root.is_dir(),
            "query refusal must preserve the genuine original directory"
        );
        drop(file);
    }

    #[cfg(windows)]
    #[test]
    fn native_path_conversion_preserves_unpaired_units_and_exact_encoded_budget() {
        use std::os::windows::ffi::OsStrExt;
        let mut buffer = [0; BUFFER_UNITS];
        let wide = capture(&mut buffer, |buffer| {
            buffer[..6].copy_from_slice(&[65, 0xd800, 66, 0xdc00, 0xd800, 0xdc00]);
            Ok(6)
        })
        .unwrap();
        let path = into_path(wide).unwrap();
        assert_eq!(path.as_os_str().encode_wide().collect::<Vec<_>>(), wide);
        assert_eq!(path.as_os_str().as_encoded_bytes().len(), 12);
        assert!(path.capacity() <= MAX_ENCODED_BYTES);
        let mut buffer = [0; BUFFER_UNITS];
        let wide = capture(&mut buffer, |buffer| {
            buffer[..1365].fill(0xdc00);
            buffer[1365] = 65;
            Ok(1366)
        })
        .unwrap();
        let path = into_path(wide).unwrap();
        assert_eq!(path.as_os_str().as_encoded_bytes().len(), MAX_ENCODED_BYTES);
        assert!(path.capacity() <= MAX_ENCODED_BYTES);
        assert_eq!(path.as_os_str().encode_wide().collect::<Vec<_>>(), wide);
    }
}
