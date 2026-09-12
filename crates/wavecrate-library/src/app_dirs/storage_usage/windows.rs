//! Windows handle-relative implementation for disposable cache maintenance.
//!
//! `cap-primitives` 4.0.x currently implements a few Windows directory operations by
//! reconstructing a path from a retained directory handle.  That is sufficient for ordinary
//! capability use, but it is not sufficient for this namespace: a captured cache must continue
//! to refer to the same filesystem object after its configured name is replaced.  This module
//! therefore uses the native handle-relative NT APIs directly for enumeration, child opens, and
//! deletion.

#![allow(unsafe_code)]

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io,
    mem::{offset_of, size_of},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle},
    },
    path::PathBuf,
};

use cap_std::fs::Dir;
use windows::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_DIRECTORY_FILE, FILE_ID_BOTH_DIR_INFORMATION, FILE_OPEN,
            FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
            FileIdBothDirectoryInformation, NTCREATEFILE_CREATE_OPTIONS, NtCreateFile,
            NtQueryDirectoryFile,
        },
    },
    Win32::{
        Foundation::{
            HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, STATUS_BUFFER_OVERFLOW,
            STATUS_BUFFER_TOO_SMALL, STATUS_NO_MORE_FILES, STATUS_SUCCESS, UNICODE_STRING,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_DEVICE,
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_DISPOSITION_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY,
            FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            FILE_TRAVERSE, FileDispositionInfo, GetFileInformationByHandle, SYNCHRONIZE,
            SetFileInformationByHandle,
        },
        System::IO::IO_STATUS_BLOCK,
    },
    core::PWSTR,
};

use crate::filesystem_identity::stable_filesystem_identity_from_open_file;

use super::{
    CacheClearOutcome, CacheMaintenanceError, DisposableCacheNamespace, MAX_MAINTENANCE_DEPTH,
    MAX_MAINTENANCE_ENTRIES, MAX_MAINTENANCE_OPEN_HANDLES, StorageUsageError, cache_io,
    cache_namespace_replaced,
};

const CACHE_DIR_NAME: &str = "cache";
const ENUMERATION_BUFFER_BYTES: usize = 64 * 1024;

/// A cache root opened with a share-delete handle so its configured name can be replaced while
/// the captured namespace remains usable through this handle.
#[derive(Debug)]
pub(super) struct CacheRoot {
    pub(super) file: File,
}

/// Open the fixed cache child relative to a retained profile-root capability.
pub(super) fn open_cache_root(profile_root: &Dir) -> io::Result<CacheRoot> {
    let profile_file = profile_root.try_clone()?.into_std_file();
    let cache = open_relative_file(
        &profile_file,
        OsStr::new(CACHE_DIR_NAME),
        OpenKind::Directory,
    )?;
    let information = file_information(&cache)?;
    if !is_directory(information.attributes) || is_reparse_point(information.attributes) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache child is not a non-reparse directory",
        ));
    }
    Ok(CacheRoot { file: cache })
}

/// Return the stable identity of a handle-retained cache root.
pub(super) fn identity_from_root(root: &CacheRoot) -> Option<String> {
    stable_filesystem_identity_from_open_file(&root.file)
}

/// Clear all disposable content below a captured Windows cache root.
pub(super) fn clear(
    namespace: &DisposableCacheNamespace,
    mut is_cancelled: impl FnMut() -> bool,
) -> Result<CacheClearOutcome, CacheMaintenanceError> {
    let mut outcome = CacheClearOutcome::default();

    super::check_cancelled(&mut is_cancelled, &outcome)?;
    namespace.ensure_profile(&outcome)?;

    let root_file = namespace.cache_root.file.try_clone().map_err(|error| {
        cache_io(
            &outcome,
            namespace.cache_path(),
            format!("clone cache root: {error}"),
        )
    })?;
    let root = NativeDir::new(root_file)
        .map_err(|error| map_native_clear_error(error, &outcome, namespace.cache_path()))?;
    let mut frames = vec![ClearFrame {
        directory: root,
        display_path: namespace.cache_path(),
        parent_entry: None,
        depth: 0,
    }];
    let mut visited_entries = 0_usize;

    loop {
        super::check_cancelled(&mut is_cancelled, &outcome)?;
        namespace.ensure_profile(&outcome)?;

        let Some(entry) = frames
            .last_mut()
            .expect("root frame remains until final check")
            .directory
            .entries
            .next()
            .cloned()
        else {
            let finished = frames.pop().expect("frame was present above");
            let Some(parent_entry) = finished.parent_entry else {
                break;
            };
            let parent = frames.last().ok_or_else(|| {
                cache_namespace_replaced(outcome, String::from("captured cache parent disappeared"))
            })?;

            verify_directory_entry(&parent.directory, &parent_entry)
                .map_err(|error| map_verify_error(error, &outcome, parent_entry.path.clone()))?;
            mark_deleted(&finished.directory.file).map_err(|error| {
                cache_io(
                    &outcome,
                    parent_entry.path.clone(),
                    format!("remove empty directory: {error}"),
                )
            })?;
            continue;
        };

        visited_entries = visited_entries.saturating_add(1);
        if visited_entries > MAX_MAINTENANCE_ENTRIES {
            return Err(CacheMaintenanceError::BoundsExceeded {
                outcome,
                kind: "entries",
                limit: MAX_MAINTENANCE_ENTRIES,
            });
        }

        let display_path = frames
            .last()
            .expect("root frame remains until final check")
            .display_path
            .clone();
        let depth = frames
            .last()
            .expect("root frame remains until final check")
            .depth;
        let path = display_path.join(&entry.name);

        if entry.is_device() {
            return Err(CacheMaintenanceError::Unsupported {
                outcome,
                reason: format!("unexpected device entry at {}", path.display()),
            });
        }

        if entry.is_directory() && !entry.is_reparse_point() {
            if depth.saturating_add(1) > MAX_MAINTENANCE_DEPTH {
                return Err(CacheMaintenanceError::BoundsExceeded {
                    outcome,
                    kind: "depth",
                    limit: MAX_MAINTENANCE_DEPTH,
                });
            }
            if frames.len() >= MAX_MAINTENANCE_OPEN_HANDLES {
                return Err(CacheMaintenanceError::BoundsExceeded {
                    outcome,
                    kind: "open handles",
                    limit: MAX_MAINTENANCE_OPEN_HANDLES,
                });
            }

            let child = {
                let parent = frames.last().expect("root frame remains until final check");
                open_directory_entry(&parent.directory, &entry)
                    .map_err(|error| map_verify_error(error, &outcome, path.clone()))?
            };
            frames.push(ClearFrame {
                directory: child,
                display_path: path.clone(),
                parent_entry: Some(ParentEntry {
                    name: entry.name,
                    identity: entry.identity,
                    path,
                }),
                depth: depth.saturating_add(1),
            });
            continue;
        }

        let (file, information) = {
            let parent = frames.last().expect("root frame remains until final check");
            open_and_verify_entry(&parent.directory, &entry)
                .map_err(|error| map_verify_error(error, &outcome, path.clone()))?
        };
        if information.attributes & FILE_ATTRIBUTE_DEVICE.0 != 0 {
            return Err(CacheMaintenanceError::Unsupported {
                outcome,
                reason: format!("unexpected device entry at {}", path.display()),
            });
        }
        mark_deleted(&file).map_err(|error| {
            cache_io(
                &outcome,
                path.clone(),
                format!("remove cache entry: {error}"),
            )
        })?;
        outcome.removed_files = outcome.removed_files.saturating_add(1);
        if !is_directory(information.attributes) && !is_reparse_point(information.attributes) {
            outcome.removed_bytes = outcome.removed_bytes.saturating_add(information.bytes);
        }
    }

    super::check_cancelled(&mut is_cancelled, &outcome)?;
    namespace.ensure_profile(&outcome)?;

    let current_cache = match open_cache_root(&namespace.profile_root) {
        Ok(root) => root,
        Err(error) if likely_namespace_replacement(&error) => {
            return Err(cache_namespace_replaced(
                outcome,
                format!("cache path changed: {error}"),
            ));
        }
        Err(error) => {
            return Err(cache_io(
                &outcome,
                namespace.cache_path(),
                format!("reopen cache path for identity check: {error}"),
            ));
        }
    };
    let current_identity =
        identity_from_root(&current_cache).ok_or_else(|| CacheMaintenanceError::Unsupported {
            outcome,
            reason: String::from("stable cache identity unavailable during final check"),
        })?;
    if current_identity != namespace.cache_identity {
        return Err(cache_namespace_replaced(
            outcome,
            String::from("cache path names a replacement directory"),
        ));
    }

    outcome.remaining_entries = count_entries(
        namespace,
        namespace.cache_root.file.try_clone().map_err(|error| {
            cache_io(
                &outcome,
                namespace.cache_path(),
                format!("clone cache root for final check: {error}"),
            )
        })?,
        &mut is_cancelled,
    )
    .map_err(|error| map_count_error(error, &outcome))?;
    outcome.remaining_entries_known = true;
    if outcome.remaining_entries != 0 {
        return Err(CacheMaintenanceError::Incomplete {
            outcome,
            remaining_entries: outcome.remaining_entries,
        });
    }
    outcome.incomplete = false;
    Ok(outcome)
}

/// Measure regular files below a profile-bound cache root without following reparse points.
pub(super) fn bounded_directory_size(
    profile: &super::super::profile_ownership::WritableProfileGuard,
    root: CacheRoot,
    display_path: PathBuf,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Result<u64, StorageUsageError> {
    let root =
        NativeDir::new(root.file).map_err(|error| map_native_usage_error(error, &display_path))?;
    let mut stack = vec![UsageFrame {
        directory: root,
        display_path,
        depth: 0,
    }];
    let mut visited_entries = 0_usize;
    let mut total = 0_u64;

    while !stack.is_empty() {
        if is_cancelled() {
            return Err(StorageUsageError::Cancelled);
        }
        profile
            .validate_current()
            .map_err(|error| StorageUsageError::ProfileChanged(error.to_string()))?;

        let Some(entry) = stack
            .last_mut()
            .expect("stack checked above")
            .directory
            .entries
            .next()
            .cloned()
        else {
            stack.pop();
            continue;
        };
        visited_entries = visited_entries.saturating_add(1);
        if visited_entries > MAX_MAINTENANCE_ENTRIES {
            return Err(StorageUsageError::BoundsExceeded {
                kind: "entries",
                limit: MAX_MAINTENANCE_ENTRIES,
            });
        }

        let path = stack
            .last()
            .expect("stack checked above")
            .display_path
            .join(&entry.name);
        let depth = stack.last().expect("stack checked above").depth;
        if entry.is_device() {
            continue;
        }
        if entry.is_directory() {
            if entry.is_reparse_point() {
                continue;
            }
            if depth.saturating_add(1) > MAX_MAINTENANCE_DEPTH {
                return Err(StorageUsageError::BoundsExceeded {
                    kind: "depth",
                    limit: MAX_MAINTENANCE_DEPTH,
                });
            }
            if stack.len() >= MAX_MAINTENANCE_OPEN_HANDLES {
                return Err(StorageUsageError::BoundsExceeded {
                    kind: "open handles",
                    limit: MAX_MAINTENANCE_OPEN_HANDLES,
                });
            }
            let child = {
                let parent = stack.last().expect("stack checked above");
                open_directory_entry(&parent.directory, &entry)
                    .map_err(|error| map_verify_usage_error(error, &path))?
            };
            stack.push(UsageFrame {
                directory: child,
                display_path: path,
                depth: depth.saturating_add(1),
            });
            continue;
        }
        if entry.is_reparse_point() {
            continue;
        }

        let (file, information) = {
            let parent = stack.last().expect("stack checked above");
            open_and_verify_entry(&parent.directory, &entry)
                .map_err(|error| map_verify_usage_error(error, &path))?
        };
        if information.attributes & FILE_ATTRIBUTE_DEVICE.0 != 0 {
            continue;
        }
        if is_directory(information.attributes) || is_reparse_point(information.attributes) {
            return Err(StorageUsageError::Io(format!(
                "cache entry changed type at {}",
                path.display()
            )));
        }
        let _ = file;
        total = total
            .checked_add(information.bytes)
            .ok_or(StorageUsageError::BoundsExceeded {
                kind: "bytes",
                limit: usize::MAX,
            })?;
    }

    Ok(total)
}

#[derive(Debug)]
struct ClearFrame {
    directory: NativeDir,
    display_path: PathBuf,
    parent_entry: Option<ParentEntry>,
    depth: usize,
}

#[derive(Debug)]
struct UsageFrame {
    directory: NativeDir,
    display_path: PathBuf,
    depth: usize,
}

#[derive(Clone, Debug)]
struct ParentEntry {
    name: OsString,
    identity: String,
    path: PathBuf,
}

#[derive(Debug)]
struct NativeDir {
    file: File,
    entries: NativeReadDir,
}

impl NativeDir {
    fn new(file: File) -> Result<Self, NativeError> {
        let entries = NativeReadDir::new(&file)?;
        Ok(Self { file, entries })
    }
}

#[derive(Debug)]
struct NativeReadDir {
    entries: Vec<NativeEntry>,
    next: usize,
}

impl NativeReadDir {
    fn new(directory: &File) -> Result<Self, NativeError> {
        let parent = file_information(directory).map_err(NativeError::Io)?;
        let mut buffer = vec![0_u64; ENUMERATION_BUFFER_BYTES / size_of::<u64>()];
        let mut entries = Vec::new();
        let mut restart_scan = true;

        loop {
            let mut status_block = IO_STATUS_BLOCK::default();
            let status = unsafe {
                NtQueryDirectoryFile(
                    HANDLE(directory.as_raw_handle()),
                    None,
                    None,
                    None,
                    &mut status_block,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * size_of::<u64>()) as u32,
                    FileIdBothDirectoryInformation,
                    false,
                    None,
                    restart_scan,
                )
            };
            restart_scan = false;

            let bytes = status_block.Information;
            if status == STATUS_NO_MORE_FILES {
                break;
            }
            if status == STATUS_BUFFER_TOO_SMALL {
                return Err(NativeError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory entry does not fit the bounded enumeration buffer",
                )));
            }
            if status != STATUS_SUCCESS && status != STATUS_BUFFER_OVERFLOW {
                return Err(NativeError::Io(ntstatus_error(status)));
            }
            if bytes == 0 {
                if status == STATUS_SUCCESS {
                    break;
                }
                return Err(NativeError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory enumeration returned no records",
                )));
            }
            if bytes > buffer.len() * size_of::<u64>() {
                return Err(NativeError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory enumeration length exceeded its buffer",
                )));
            }
            parse_records(&buffer, bytes, parent.volume, &mut entries)?;
            if entries.len() > MAX_MAINTENANCE_ENTRIES {
                return Err(NativeError::Bounds);
            }
        }

        Ok(Self { entries, next: 0 })
    }

    fn next(&mut self) -> Option<&NativeEntry> {
        let entry = self.entries.get(self.next)?;
        self.next = self.next.saturating_add(1);
        Some(entry)
    }
}

#[derive(Clone, Debug)]
struct NativeEntry {
    name: OsString,
    identity: String,
    attributes: u32,
}

impl NativeEntry {
    fn is_directory(&self) -> bool {
        is_directory(self.attributes)
    }

    fn is_reparse_point(&self) -> bool {
        is_reparse_point(self.attributes)
    }

    fn is_device(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DEVICE.0 != 0
    }
}

#[derive(Debug)]
struct FileInformation {
    identity: String,
    volume: u32,
    attributes: u32,
    bytes: u64,
}

#[derive(Clone, Copy, Debug)]
enum OpenKind {
    Directory,
    Leaf,
}

#[derive(Debug)]
enum NativeError {
    Bounds,
    Io(io::Error),
}

#[derive(Debug)]
enum VerifyError {
    NamespaceReplaced(String),
    Native(NativeError),
}

#[derive(Debug)]
enum CountError {
    Cancelled,
    Profile(String),
    Bounds(&'static str, usize),
    NamespaceReplaced(String),
    Io(PathBuf, String),
}

fn open_relative_file(parent: &File, name: &OsStr, kind: OpenKind) -> io::Result<File> {
    let wide: Vec<u16> = name.encode_wide().collect();
    if wide.is_empty() || wide.iter().any(|character| *character == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "relative Windows entry name is empty or contains NUL",
        ));
    }
    let byte_length = wide
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "relative entry name is too long",
            )
        })?;
    let unicode_name = UNICODE_STRING {
        Length: byte_length,
        MaximumLength: byte_length,
        Buffer: PWSTR(wide.as_ptr() as *mut u16),
    };
    let object_attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: HANDLE(parent.as_raw_handle()),
        ObjectName: &unicode_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        ..OBJECT_ATTRIBUTES::default()
    };
    let mut handle = HANDLE::default();
    let mut status_block = IO_STATUS_BLOCK::default();
    let desired_access: FILE_ACCESS_RIGHTS = match kind {
        OpenKind::Directory => {
            DELETE | FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE
        }
        OpenKind::Leaf => DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
    };
    let create_options = FILE_OPEN_REPARSE_POINT
        | FILE_OPEN_FOR_BACKUP_INTENT
        | FILE_SYNCHRONOUS_IO_NONALERT
        | match kind {
            OpenKind::Directory => FILE_DIRECTORY_FILE,
            OpenKind::Leaf => NTCREATEFILE_CREATE_OPTIONS(0),
        };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access,
            &object_attributes,
            &mut status_block,
            None,
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            create_options,
            None,
            0,
        )
    };
    if status != STATUS_SUCCESS {
        return Err(ntstatus_error(status));
    }
    if handle.0.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "NtCreateFile returned a null handle",
        ));
    }
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

fn open_entry_handle(parent: &NativeDir, entry: &NativeEntry) -> Result<File, NativeError> {
    let kind = if entry.is_directory() && !entry.is_reparse_point() {
        OpenKind::Directory
    } else {
        OpenKind::Leaf
    };
    match open_relative_file(&parent.file, &entry.name, kind) {
        Ok(file) => Ok(file),
        Err(_error) if entry.is_directory() && entry.is_reparse_point() => {
            open_relative_file(&parent.file, &entry.name, OpenKind::Directory)
                .map_err(NativeError::Io)
        }
        Err(error) => Err(NativeError::Io(error)),
    }
}

fn open_directory_entry(parent: &NativeDir, entry: &NativeEntry) -> Result<NativeDir, VerifyError> {
    let file = open_relative_file(&parent.file, &entry.name, OpenKind::Directory)
        .map_err(|error| VerifyError::Native(NativeError::Io(error)))?;
    let information =
        file_information(&file).map_err(|error| VerifyError::Native(NativeError::Io(error)))?;
    if information.identity != entry.identity
        || !is_directory(information.attributes)
        || is_reparse_point(information.attributes)
    {
        return Err(VerifyError::NamespaceReplaced(format!(
            "directory changed before opening {}",
            entry.name.to_string_lossy()
        )));
    }
    NativeDir::new(file).map_err(VerifyError::Native)
}

fn open_and_verify_entry(
    parent: &NativeDir,
    entry: &NativeEntry,
) -> Result<(File, FileInformation), VerifyError> {
    let file = open_entry_handle(parent, entry).map_err(VerifyError::Native)?;
    let information =
        file_information(&file).map_err(|error| VerifyError::Native(NativeError::Io(error)))?;
    let type_matches = is_directory(information.attributes) == entry.is_directory()
        && is_reparse_point(information.attributes) == entry.is_reparse_point();
    if information.identity != entry.identity || !type_matches {
        return Err(VerifyError::NamespaceReplaced(format!(
            "entry changed before mutation: {}",
            entry.name.to_string_lossy()
        )));
    }
    Ok((file, information))
}

fn verify_directory_entry(parent: &NativeDir, entry: &ParentEntry) -> Result<(), VerifyError> {
    let native_entry = NativeEntry {
        name: entry.name.clone(),
        identity: entry.identity.clone(),
        attributes: FILE_ATTRIBUTE_DIRECTORY.0,
    };
    let file = open_relative_file(&parent.file, &native_entry.name, OpenKind::Directory)
        .map_err(|error| VerifyError::Native(NativeError::Io(error)))?;
    let information =
        file_information(&file).map_err(|error| VerifyError::Native(NativeError::Io(error)))?;
    if information.identity != native_entry.identity
        || !is_directory(information.attributes)
        || is_reparse_point(information.attributes)
    {
        return Err(VerifyError::NamespaceReplaced(format!(
            "captured directory was replaced at {}",
            entry.path.display()
        )));
    }
    Ok(())
}

fn file_information(file: &File) -> io::Result<FileInformation> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }
        .map_err(|_| io::Error::last_os_error())?;
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    let creation_time = (u64::from(information.ftCreationTime.dwHighDateTime) << 32)
        | u64::from(information.ftCreationTime.dwLowDateTime);
    let bytes = (u64::from(information.nFileSizeHigh) << 32) | u64::from(information.nFileSizeLow);
    Ok(FileInformation {
        identity: format!(
            "windows:{}:{}:{}",
            information.dwVolumeSerialNumber, file_index, creation_time
        ),
        volume: information.dwVolumeSerialNumber,
        attributes: information.dwFileAttributes,
        bytes,
    })
}

fn parse_records(
    buffer: &[u64],
    bytes: usize,
    volume: u32,
    entries: &mut Vec<NativeEntry>,
) -> Result<(), NativeError> {
    let bytes = bytes.min(buffer.len() * size_of::<u64>());
    let base = buffer.as_ptr().cast::<u8>();
    let header_size = offset_of!(FILE_ID_BOTH_DIR_INFORMATION, FileName);
    let mut offset = 0_usize;
    while offset < bytes {
        let remaining = bytes.saturating_sub(offset);
        if remaining < header_size {
            return Err(NativeError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated FILE_ID_BOTH_DIR_INFORMATION record",
            )));
        }
        let record = unsafe { &*base.add(offset).cast::<FILE_ID_BOTH_DIR_INFORMATION>() };
        let next_offset = record.NextEntryOffset as usize;
        let record_length = if next_offset == 0 {
            remaining
        } else {
            next_offset
        };
        if record_length < header_size
            || record_length > remaining
            || (next_offset != 0 && next_offset % size_of::<u64>() != 0)
        {
            return Err(NativeError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid FILE_ID_BOTH_DIR_INFORMATION record length",
            )));
        }
        let name_length = record.FileNameLength as usize;
        if name_length % size_of::<u16>() != 0 || name_length > record_length - header_size {
            return Err(NativeError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid FILE_ID_BOTH_DIR_INFORMATION name length",
            )));
        }
        let name = unsafe {
            OsString::from_wide(std::slice::from_raw_parts(
                record.FileName.as_ptr(),
                name_length / size_of::<u16>(),
            ))
        };
        if name.as_os_str() != OsStr::new(".") && name.as_os_str() != OsStr::new("..") {
            let file_id = record.FileId as u64;
            let creation_time = record.CreationTime as u64;
            entries.push(NativeEntry {
                name,
                identity: format!("windows:{volume}:{file_id}:{creation_time}"),
                attributes: record.FileAttributes,
            });
        }
        if next_offset == 0 {
            break;
        }
        offset = offset.saturating_add(next_offset);
    }
    Ok(())
}

fn mark_deleted(file: &File) -> io::Result<()> {
    let information = FILE_DISPOSITION_INFO { DeleteFile: true };
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FILE_INFO_BY_HANDLE_CLASS(FileDispositionInfo.0),
            (&information as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    }
    .map_err(|_| io::Error::last_os_error())
}

fn count_entries(
    namespace: &DisposableCacheNamespace,
    root_file: File,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Result<u64, CountError> {
    let root = NativeDir::new(root_file)
        .map_err(|error| map_native_count_error(error, namespace.cache_path()))?;
    let mut stack = vec![UsageFrame {
        directory: root,
        display_path: namespace.cache_path(),
        depth: 0,
    }];
    let mut count = 0_u64;
    while !stack.is_empty() {
        if is_cancelled() {
            return Err(CountError::Cancelled);
        }
        namespace
            .profile
            .validate_current()
            .map_err(|error| CountError::Profile(error.to_string()))?;
        let Some(entry) = stack
            .last_mut()
            .expect("stack checked above")
            .directory
            .entries
            .next()
            .cloned()
        else {
            stack.pop();
            continue;
        };
        count = count.saturating_add(1);
        if count > MAX_MAINTENANCE_ENTRIES as u64 {
            return Err(CountError::Bounds("entries", MAX_MAINTENANCE_ENTRIES));
        }
        if !entry.is_directory() || entry.is_reparse_point() || entry.is_device() {
            continue;
        }
        let path = stack
            .last()
            .expect("stack checked above")
            .display_path
            .join(&entry.name);
        let depth = stack.last().expect("stack checked above").depth;
        if depth.saturating_add(1) > MAX_MAINTENANCE_DEPTH {
            return Err(CountError::Bounds("depth", MAX_MAINTENANCE_DEPTH));
        }
        if stack.len() >= MAX_MAINTENANCE_OPEN_HANDLES {
            return Err(CountError::Bounds(
                "open handles",
                MAX_MAINTENANCE_OPEN_HANDLES,
            ));
        }
        let parent = stack.last().expect("stack checked above");
        let child =
            open_directory_entry(&parent.directory, &entry).map_err(|error| match error {
                VerifyError::NamespaceReplaced(reason) => CountError::NamespaceReplaced(reason),
                VerifyError::Native(error) => map_native_count_error(error, path.clone()),
            })?;
        stack.push(UsageFrame {
            directory: child,
            display_path: path,
            depth: depth.saturating_add(1),
        });
    }
    Ok(count)
}

fn map_count_error(error: CountError, outcome: &CacheClearOutcome) -> CacheMaintenanceError {
    match error {
        CountError::Cancelled => CacheMaintenanceError::Cancelled { outcome: *outcome },
        CountError::Profile(reason) => CacheMaintenanceError::ProfileChanged {
            outcome: *outcome,
            reason,
        },
        CountError::Bounds(kind, limit) => CacheMaintenanceError::BoundsExceeded {
            outcome: *outcome,
            kind,
            limit,
        },
        CountError::NamespaceReplaced(reason) => cache_namespace_replaced(*outcome, reason),
        CountError::Io(path, message) => cache_io(outcome, path, message),
    }
}

fn map_verify_error(
    error: VerifyError,
    outcome: &CacheClearOutcome,
    path: PathBuf,
) -> CacheMaintenanceError {
    match error {
        VerifyError::NamespaceReplaced(reason) => cache_namespace_replaced(*outcome, reason),
        VerifyError::Native(error) => map_native_clear_error(error, outcome, path),
    }
}

fn map_native_clear_error(
    error: NativeError,
    outcome: &CacheClearOutcome,
    path: PathBuf,
) -> CacheMaintenanceError {
    match error {
        NativeError::Bounds => CacheMaintenanceError::BoundsExceeded {
            outcome: *outcome,
            kind: "entries",
            limit: MAX_MAINTENANCE_ENTRIES,
        },
        NativeError::Io(error) => cache_io(outcome, path, error.to_string()),
    }
}

fn map_native_count_error(error: NativeError, path: PathBuf) -> CountError {
    match error {
        NativeError::Bounds => CountError::Bounds("entries", MAX_MAINTENANCE_ENTRIES),
        NativeError::Io(error) => CountError::Io(path, error.to_string()),
    }
}

fn map_native_usage_error(error: NativeError, path: &PathBuf) -> StorageUsageError {
    match error {
        NativeError::Bounds => StorageUsageError::BoundsExceeded {
            kind: "entries",
            limit: MAX_MAINTENANCE_ENTRIES,
        },
        NativeError::Io(error) => {
            StorageUsageError::Io(format!("failed to read {}: {error}", path.display()))
        }
    }
}

fn map_verify_usage_error(error: VerifyError, path: &PathBuf) -> StorageUsageError {
    match error {
        VerifyError::NamespaceReplaced(reason) => StorageUsageError::Io(reason),
        VerifyError::Native(error) => match error {
            NativeError::Bounds => StorageUsageError::BoundsExceeded {
                kind: "entries",
                limit: MAX_MAINTENANCE_ENTRIES,
            },
            NativeError::Io(error) => {
                StorageUsageError::Io(format!("failed to read {}: {error}", path.display()))
            }
        },
    }
}

fn ntstatus_error(status: NTSTATUS) -> io::Error {
    let error = unsafe { RtlNtStatusToDosError(status) };
    if error == 0 {
        io::Error::new(
            io::ErrorKind::Other,
            format!("Windows NT status 0x{:08x}", status.0 as u32),
        )
    } else {
        io::Error::from_raw_os_error(error as i32)
    }
}

fn likely_namespace_replacement(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory | io::ErrorKind::InvalidData
    ) || error.raw_os_error() == Some(windows::Win32::Foundation::ERROR_STOPPED_ON_SYMLINK.0 as i32)
}

fn is_directory(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0
}

fn is_reparse_point(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
}
