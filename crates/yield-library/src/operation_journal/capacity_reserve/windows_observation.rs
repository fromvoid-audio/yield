//! Original disk-handle extent observations; not a protected floor or physical qualification.
#[cfg(target_os = "windows")]
use super::RawFileObservation;
use super::ReserveFailureReason;

fn checked_extent(
    length: i64,
    allocation: i64,
    links: u32,
    directory: bool,
    deleting: bool,
) -> Result<(u64, u64, u64), ReserveFailureReason> {
    if directory || deleting {
        return Err(ReserveFailureReason::NotRegular);
    }
    let length = u64::try_from(length).map_err(|_| ReserveFailureReason::ArithmeticOverflow)?;
    let allocation =
        u64::try_from(allocation).map_err(|_| ReserveFailureReason::ArithmeticOverflow)?;
    if links == 0 {
        return Err(ReserveFailureReason::NotSingleLink { links: 0 });
    }
    if length > 0 && allocation == 0 {
        return Err(ReserveFailureReason::Unsupported);
    }
    Ok((length, allocation, u64::from(links)))
}

#[cfg(target_os = "windows")]
pub(super) fn observe(file: &std::fs::File) -> Result<RawFileObservation, ReserveFailureReason> {
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SPARSE_FILE, FILE_STANDARD_INFO,
        FILE_TYPE_DISK, FileStandardInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        GetFileType,
    };
    use std::os::windows::io::AsRawHandle;
    #[derive(Eq, PartialEq)]
    struct Identity {
        device: u32,
        index: u64,
        created: u64,
        length: u64,
        links: u32,
        attributes: u32,
    }
    fn identity(handle: HANDLE) -> Result<Identity, ReserveFailureReason> {
        if unsafe { GetFileType(handle) } != FILE_TYPE_DISK {
            return Err(ReserveFailureReason::NotRegular);
        }
        let mut value = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(handle, &mut value) }
            .map_err(|e| ReserveFailureReason::Io(e.to_string()))?;
        if value.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0)
            != 0
        {
            return Err(ReserveFailureReason::NotRegular);
        }
        if value.dwFileAttributes & FILE_ATTRIBUTE_SPARSE_FILE.0 != 0 {
            return Err(ReserveFailureReason::Unsupported);
        }
        if value.dwFileAttributes & FILE_ATTRIBUTE_COMPRESSED.0 != 0 {
            return Err(ReserveFailureReason::Compressed);
        }
        let index = (u64::from(value.nFileIndexHigh) << 32) | u64::from(value.nFileIndexLow);
        if value.dwVolumeSerialNumber == 0 || index == 0 {
            return Err(ReserveFailureReason::IdentityUnavailable);
        }
        Ok(Identity {
            device: value.dwVolumeSerialNumber,
            index,
            created: (u64::from(value.ftCreationTime.dwHighDateTime) << 32)
                | u64::from(value.ftCreationTime.dwLowDateTime),
            length: (u64::from(value.nFileSizeHigh) << 32) | u64::from(value.nFileSizeLow),
            links: value.nNumberOfLinks,
            attributes: value.dwFileAttributes,
        })
    }
    fn extent(handle: HANDLE) -> Result<FILE_STANDARD_INFO, ReserveFailureReason> {
        let mut value = FILE_STANDARD_INFO::default();
        // Synchronous query; original HANDLE stays borrowed and the fixed stack record stays
        // live throughout. No path lookup, descriptor replacement, seek or filesystem effect.
        unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileStandardInfo,
                (&mut value as *mut FILE_STANDARD_INFO).cast(),
                std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
            )
        }
        .map_err(|e| ReserveFailureReason::Io(e.to_string()))?;
        Ok(value)
    }
    let handle = HANDLE(file.as_raw_handle());
    let before = identity(handle)?;
    let standard = extent(handle)?;
    let (length, allocated_bytes, links) = checked_extent(
        standard.EndOfFile,
        standard.AllocationSize,
        standard.NumberOfLinks,
        standard.Directory,
        standard.DeletePending,
    )?;
    let repeated = extent(handle)?;
    if before != identity(handle)?
        || standard != repeated
        || before.length != length
        || u64::from(before.links) != links
    {
        return Err(ReserveFailureReason::IdentityMismatch);
    }
    Ok(RawFileObservation {
        device: u64::from(before.device),
        length,
        allocated_bytes,
        links,
        regular: true,
        compressed: false,
    })
}

#[cfg(test)]
mod tests;
