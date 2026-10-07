//! Readonly handle-bound observations, never physical qualification or admission.
use std::io;

// FILE_FS_FULL_SIZE_INFORMATION, deliberately kept beside its checked decoder.
// The WDK binding lives behind an otherwise unused SystemServices feature.
#[repr(C)]
#[derive(Default)]
struct FullSizeInformation {
    total: i64,
    caller_available: i64,
    actual_available: i64,
    sectors_per_unit: u32,
    bytes_per_sector: u32,
}

const _: () = assert!(std::mem::size_of::<FullSizeInformation>() == 32);

fn capacity_values(record: &FullSizeInformation, returned: usize) -> io::Result<(u64, u64)> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Windows volume capacity record",
        )
    };
    if returned != std::mem::size_of::<FullSizeInformation>()
        || record.total < 0
        || record.caller_available < 0
        || record.actual_available < record.caller_available
        || record.actual_available > record.total
        || !record.sectors_per_unit.is_power_of_two()
        || !record.bytes_per_sector.is_power_of_two()
        || record.bytes_per_sector < 512
        || record.bytes_per_sector > 65536
    {
        return Err(invalid());
    }
    let unit = u64::from(record.sectors_per_unit)
        .checked_mul(u64::from(record.bytes_per_sector))
        .ok_or_else(invalid)?;
    // Validate the entire geometry, not only a currently small free-space count.
    (record.total as u64)
        .checked_mul(unit)
        .ok_or_else(invalid)?;
    let free = (record.caller_available as u64)
        .checked_mul(unit)
        .ok_or_else(invalid)?;
    Ok((free, unit))
}

#[cfg(windows)]
pub(super) fn descriptor_volume_facts(file: &std::fs::File) -> io::Result<super::VolumeFacts> {
    match observe::<std::convert::Infallible>(file, None) {
        Ok(facts) => Ok(facts),
        Err(ObservationError::Observed(error)) => Err(error),
        Err(ObservationError::Funding(never)) => match never {},
    }
}

#[cfg(windows)]
pub(super) enum ObservationError<E> {
    Observed(io::Error),
    Funding(E),
}

#[cfg(windows)]
pub(super) fn observe<E>(
    file: &std::fs::File,
    mut reserve: Option<&mut dyn FnMut(usize) -> Result<(), E>>,
) -> Result<super::VolumeFacts, ObservationError<E>> {
    use ::windows::Wdk::Storage::FileSystem::{
        FileFsFullSizeInformation, NtQueryVolumeInformationFile,
    };
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_TYPE_DISK,
        GetFileInformationByHandle, GetFileType,
    };
    use ::windows::Win32::System::IO::IO_STATUS_BLOCK;
    use std::os::windows::io::AsRawHandle;

    fn identity<E>(
        handle: HANDLE,
        reserve: &mut Option<&mut dyn FnMut(usize) -> Result<(), E>>,
    ) -> Result<(u32, u64, u64), ObservationError<E>> {
        // Pipes, devices and opened reparse points do not identify a regular volume object.
        if unsafe { GetFileType(handle) } != FILE_TYPE_DISK {
            return Err(ObservationError::Observed(io::Error::new(
                io::ErrorKind::Unsupported,
                "not a disk handle",
            )));
        }
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        if let Err(error) = unsafe { GetFileInformationByHandle(handle, &mut info) } {
            let error = match reserve.as_deref_mut() {
                None => io::Error::other(error),
                Some(reserve) => io::Error::other(
                    crate::sample_sources::db::sqlite_identity::native_error_message::win32_display(
                        error.code().0, reserve,
                    ).map_err(ObservationError::Funding)?,
                ),
            };
            return Err(ObservationError::Observed(error));
        }
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
            || info.dwVolumeSerialNumber == 0
            || index == 0
        {
            return Err(ObservationError::Observed(io::Error::new(
                io::ErrorKind::Unsupported,
                "unknown disk object identity",
            )));
        }
        let created = (u64::from(info.ftCreationTime.dwHighDateTime) << 32)
            | u64::from(info.ftCreationTime.dwLowDateTime);
        Ok((info.dwVolumeSerialNumber, index, created))
    }

    let handle = HANDLE(file.as_raw_handle());
    let before = identity(handle, &mut reserve)?;
    let mut record = FullSizeInformation::default();
    let mut status = IO_STATUS_BLOCK::default();
    // NtQueryVolumeInformationFile is synchronous, including for asynchronous file
    // handles. Both buffers remain live for the entire call. No seek, path reopen,
    // allocation, permission change or filesystem mutation is performed.
    let result = unsafe {
        NtQueryVolumeInformationFile(
            handle,
            &mut status,
            (&mut record as *mut FullSizeInformation).cast(),
            std::mem::size_of::<FullSizeInformation>() as u32,
            FileFsFullSizeInformation,
        )
    };
    if result.0 != 0 || unsafe { status.Anonymous.Status }.0 != 0 {
        return Err(ObservationError::Observed(io::Error::other(format!(
            "Windows volume query failed: {result:?}"
        ))));
    }
    let (free_bytes, allocation_unit) =
        capacity_values(&record, status.Information).map_err(ObservationError::Observed)?;
    if identity(handle, &mut reserve)? != before {
        return Err(ObservationError::Observed(io::Error::other(
            "Windows disk object identity changed during capacity query",
        )));
    }
    Ok(super::VolumeFacts {
        identity: super::VolumeIdentity {
            device: u64::from(before.0),
        },
        free_bytes,
        allocation_unit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> FullSizeInformation {
        FullSizeInformation {
            total: 100,
            caller_available: 3,
            actual_available: 20,
            sectors_per_unit: 8,
            bytes_per_sector: 512,
        }
    }

    #[test]
    fn caller_quota_and_zero_free_are_preserved() {
        assert_eq!(capacity_values(&record(), 32).unwrap(), (12288, 4096));
        let mut full = record();
        full.caller_available = 0;
        assert_eq!(capacity_values(&full, 32).unwrap(), (0, 4096));
    }

    #[test]
    fn malformed_counts_geometry_and_record_sizes_refuse() {
        for size in [0, 31, 33, usize::MAX] {
            assert!(capacity_values(&record(), size).is_err());
        }
        for field in 0..8 {
            let mut bad = record();
            match field {
                0 => bad.total = -1,
                1 => bad.caller_available = -1,
                2 => bad.actual_available = -1,
                3 => bad.caller_available = 21,
                4 => bad.actual_available = 101,
                5 => bad.sectors_per_unit = 0,
                6 => bad.sectors_per_unit = 3,
                _ => bad.bytes_per_sector = 513,
            }
            assert!(capacity_values(&bad, 32).is_err());
        }
        for sector in [0, 256, 131072] {
            let mut bad = record();
            bad.bytes_per_sector = sector;
            assert!(capacity_values(&bad, 32).is_err());
        }
    }

    #[test]
    fn overflowing_total_refuses_even_when_caller_has_no_free_space() {
        let mut bad = record();
        bad.total = i64::MAX;
        bad.caller_available = 0;
        assert!(capacity_values(&bad, 32).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn native_character_device_is_not_a_volume_observation() {
        let file = std::fs::OpenOptions::new().read(true).open("NUL").unwrap();
        assert_eq!(
            descriptor_volume_facts(&file).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_file_directory_and_renamed_handle_observations() {
        use ::windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
        use std::os::windows::fs::OpenOptionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("original");
        let file = std::fs::File::create(&path).unwrap();
        let before = descriptor_volume_facts(&file).unwrap();
        std::fs::rename(&path, directory.path().join("moved")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        let after = descriptor_volume_facts(&file).unwrap();
        assert_eq!(before.identity, after.identity);
        assert_eq!(before.allocation_unit, after.allocation_unit);
        let root = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(directory.path())
            .unwrap();
        let root_facts = descriptor_volume_facts(&root).unwrap();
        assert_eq!(root_facts.identity, before.identity);
        assert_eq!(root_facts.allocation_unit, before.allocation_unit);
    }
}
