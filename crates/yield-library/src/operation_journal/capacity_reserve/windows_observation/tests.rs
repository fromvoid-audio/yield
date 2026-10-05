//! Extent checks and real Windows original-handle observations, never physical qualification.
#[cfg(test)]
mod integration {
    use super::super::*;

    #[test]
    fn windows_extent_values_refuse_unknown_shapes_and_preserve_exact_link_observations() {
        assert_eq!(checked_extent(0, 0, 1, false, false).unwrap(), (0, 0, 1));
        assert_eq!(
            checked_extent(16384, 32768, 2, false, false).unwrap(),
            (16384, 32768, 2)
        );
        for (length, allocation, links, directory, deleting) in [
            (-1, 4096, 1, false, false),
            (1, -1, 1, false, false),
            (1, 4096, 0, false, false),
            (1, 4096, 1, true, false),
            (1, 4096, 1, false, true),
            (1, 0, 1, false, false),
        ] {
            assert!(checked_extent(length, allocation, links, directory, deleting).is_err());
        }
    }

    #[cfg(target_os = "windows")]
    mod native {
        use super::*;
        use ::windows::Win32::Foundation::HANDLE;
        use ::windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        use std::fs::{self, File, OpenOptions};
        use std::io::{Read, Write};
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;

        fn reader(path: &std::path::Path, directory: bool) -> File {
            OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
                .custom_flags(
                    FILE_FLAG_OPEN_REPARSE_POINT.0
                        | if directory {
                            FILE_FLAG_BACKUP_SEMANTICS.0
                        } else {
                            0
                        },
                )
                .open(path)
                .unwrap()
        }
        fn write_file(path: &std::path::Path) {
            let mut file = File::create(path).unwrap();
            file.write_all(&[7; 16384]).unwrap();
            file.sync_all().unwrap();
        }

        #[test]
        fn native_windows_original_readonly_reserve_extent_survives_rename_and_replacement() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("original");
            let moved = temp.path().join("moved");
            write_file(&path);
            let mut original = reader(&path, false);
            let before = observe(&original).unwrap();
            let complete_before =
                super::super::super::super::capture_observation(&original).unwrap();
            assert_eq!(before.length, 16384);
            assert!(before.allocated_bytes >= before.length);
            assert_eq!(before.links, 1);
            assert!(before.regular && !before.compressed);
            fs::rename(&path, &moved).unwrap();
            write_file(&path);
            let after = observe(&original).unwrap();
            assert_eq!(before.device, after.device);
            assert_eq!(before.length, after.length);
            assert_eq!(before.allocated_bytes, after.allocated_bytes);
            assert_eq!(
                complete_before,
                super::super::super::super::capture_observation(&original).unwrap()
            );
            assert_ne!(
                complete_before.identity,
                super::super::super::super::capture_observation(&reader(&path, false))
                    .unwrap()
                    .identity
            );
            let mut bytes = Vec::new();
            original.read_to_end(&mut bytes).unwrap();
            assert_eq!(
                bytes,
                vec![7; 16384],
                "metadata query does not seek the original handle"
            );
            assert!(original.write_all(b"cannot-write").is_err());
            // These metadata observations cannot mint a protected reserve or provider grant.
            assert!(
                super::super::super::super::allocate_protected_reserve(File::open(&moved).unwrap())
                    .is_err()
            );
        }

        #[test]
        fn native_windows_original_extent_reports_hardlinks_and_rejects_directory_and_device() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("original");
            write_file(&path);
            let original = reader(&path, false);
            fs::hard_link(&path, temp.path().join("alias")).unwrap();
            assert_eq!(observe(&original).unwrap().links, 2);
            assert!(matches!(
                observe(&reader(temp.path(), true)),
                Err(ReserveFailureReason::NotRegular)
            ));
            let device = OpenOptions::new().read(true).open("NUL").unwrap();
            assert!(matches!(
                observe(&device),
                Err(ReserveFailureReason::NotRegular)
            ));
        }

        #[test]
        fn native_windows_original_extent_rejects_real_sparse_and_reparse_handles() {
            use ::windows::Win32::System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_SPARSE};
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("sparse");
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            let mut returned = 0;
            // Requires a disposable filesystem supporting real sparse files; failure is not
            // a passing skip. This native fixture changes only its own temporary file.
            unsafe {
                DeviceIoControl(
                    HANDLE(file.as_raw_handle()),
                    FSCTL_SET_SPARSE,
                    None,
                    0,
                    None,
                    0,
                    Some(&mut returned),
                    None,
                )
            }
            .unwrap();
            file.set_len(65536).unwrap();
            assert!(matches!(
                observe(&file),
                Err(ReserveFailureReason::Unsupported)
            ));
            let target = temp.path().join("regular");
            write_file(&target);
            let link = temp.path().join("link");
            // Real symlink privilege/Developer Mode is required; do not bypass this fixture.
            std::os::windows::fs::symlink_file(&target, &link).unwrap();
            assert!(matches!(
                observe(&reader(&link, false)),
                Err(ReserveFailureReason::NotRegular)
            ));
            assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
            assert_eq!(target.metadata().unwrap().len(), 16384);
        }
    }
}
