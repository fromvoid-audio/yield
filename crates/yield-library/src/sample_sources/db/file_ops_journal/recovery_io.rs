use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use cap_fs_ext::OpenOptionsFollowExt;
use cap_primitives::ambient_authority;
#[cfg(windows)]
use cap_primitives::fs::OpenOptionsExt;
#[cfg(any(windows, test))]
use cap_primitives::fs::{DirOptions, create_dir};
use cap_primitives::fs::{FollowSymlinks, OpenOptions, open, open_ambient_dir, open_dir_nofollow};
#[cfg(test)]
use cap_primitives::fs::{hard_link, remove_file};

use crate::app_dirs::{ProfileChildLease, ProfileOwnershipError, WritableProfileGuard};
#[cfg(not(windows))]
use crate::directory_sync::sync_directory as sync_retained_directory;
use crate::filesystem_identity::{
    stable_filesystem_identity, stable_filesystem_identity_from_open_file,
};
use crate::sample_sources::finalization::{
    StagedFinalizationAvailability, StagedFinalizationUnsupportedReason,
    staged_file_finalization_capabilities,
};
use crate::timestamps::system_time_to_unix_nanos;

use super::super::{DatabaseRootBindingGuard, SourceDatabase};

/// Result of a descriptor-bound staged publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StagedFinalization {
    /// The staged object now owns the target pathname and no staged cleanup is required.
    Published,
    /// The staged object remains at its journaled pathname and must be cleaned separately.
    NeedsCleanup,
}

/// Narrow boundary for the only operations that may publish or discard staged recovery data.
pub(super) trait StagedFileFinalizer {
    fn publish(
        &self,
        root: &RecoveryRoot,
        staged_relative: &Path,
        staged: &OpenedFile,
        target_relative: &Path,
    ) -> Result<StagedFinalization, String>;

    fn cleanup(
        &self,
        root: &RecoveryRoot,
        staged_relative: &Path,
        staged: &OpenedFile,
    ) -> Result<(), String>;
}

/// Production descriptor-bound finalization. Windows has the required handle APIs; other
/// platforms intentionally fail closed until an equivalent primitive is available.
pub(super) struct PlatformStagedFileFinalizer;

impl StagedFileFinalizer for PlatformStagedFileFinalizer {
    fn publish(
        &self,
        root: &RecoveryRoot,
        staged_relative: &Path,
        staged: &OpenedFile,
        target_relative: &Path,
    ) -> Result<StagedFinalization, String> {
        let availability = staged_file_finalization_capabilities().publication;
        if let StagedFinalizationAvailability::Unsupported(reason) = availability {
            return Err(unsupported_publication_error(reason));
        }

        #[cfg(windows)]
        {
            root.ensure_parent(target_relative)?;
            let target_parent =
                root.open_directory(target_relative.parent().unwrap_or_else(|| Path::new(".")))?;
            let target_name = target_relative.file_name().ok_or_else(|| {
                format!(
                    "invalid target recovery path: {}",
                    target_relative.display()
                )
            })?;
            windows_finalization::rename_no_replace(
                &staged.capability,
                &target_parent,
                target_name,
            )
            .map_err(|error| {
                format!(
                    "failed to finalize {} to {} without replacement: {error}",
                    root.path.join(staged_relative).display(),
                    root.path.join(target_relative).display()
                )
            })?;
            return Ok(StagedFinalization::Published);
        }

        #[cfg(not(windows))]
        {
            let _ = (root, staged_relative, staged, target_relative);
            let _ = availability;
            Err("descriptor-bound staged finalization is unavailable despite platform capability policy; retaining staged data and journal".to_string())
        }
    }

    fn cleanup(
        &self,
        _root: &RecoveryRoot,
        _staged_relative: &Path,
        staged: &OpenedFile,
    ) -> Result<(), String> {
        let availability = staged_file_finalization_capabilities().cleanup;
        if let StagedFinalizationAvailability::Unsupported(reason) = availability {
            return Err(unsupported_cleanup_error(reason));
        }

        #[cfg(windows)]
        {
            windows_finalization::delete_by_handle(&staged.capability).map_err(|error| {
                format!("failed to clean staged recovery file through its opened handle: {error}")
            })?;
            return Ok(());
        }

        #[cfg(not(windows))]
        {
            let _ = staged;
            let _ = availability;
            Err("descriptor-bound staged cleanup is unavailable despite platform capability policy; retaining staged data and journal".to_string())
        }
    }
}

fn unsupported_publication_error(reason: StagedFinalizationUnsupportedReason) -> String {
    format!(
        "descriptor-bound staged finalization is unsupported on this platform ({reason}); retaining staged data and journal"
    )
}

fn unsupported_cleanup_error(reason: StagedFinalizationUnsupportedReason) -> String {
    format!(
        "descriptor-bound staged cleanup is unsupported on this platform ({reason}); retaining staged data and journal"
    )
}

#[cfg(test)]
pub(super) struct TestStagedFileFinalizer;

#[cfg(test)]
impl StagedFileFinalizer for TestStagedFileFinalizer {
    fn publish(
        &self,
        root: &RecoveryRoot,
        staged_relative: &Path,
        _staged: &OpenedFile,
        target_relative: &Path,
    ) -> Result<StagedFinalization, String> {
        root.ensure_parent(target_relative)?;
        let staged_parent =
            root.open_directory(staged_relative.parent().unwrap_or_else(|| Path::new(".")))?;
        let target_parent =
            root.open_directory(target_relative.parent().unwrap_or_else(|| Path::new(".")))?;
        let staged_name = staged_relative.file_name().ok_or_else(|| {
            format!(
                "invalid staged recovery path: {}",
                staged_relative.display()
            )
        })?;
        let target_name = target_relative.file_name().ok_or_else(|| {
            format!(
                "invalid target recovery path: {}",
                target_relative.display()
            )
        })?;
        hard_link(
            &staged_parent,
            Path::new(staged_name),
            &target_parent,
            Path::new(target_name),
        )
        .map_err(|error| format!("test staged publication failed: {error}"))?;
        Ok(StagedFinalization::NeedsCleanup)
    }

    fn cleanup(
        &self,
        root: &RecoveryRoot,
        staged_relative: &Path,
        _staged: &OpenedFile,
    ) -> Result<(), String> {
        let parent =
            root.open_directory(staged_relative.parent().unwrap_or_else(|| Path::new(".")))?;
        let name = staged_relative.file_name().ok_or_else(|| {
            format!(
                "invalid staged recovery path: {}",
                staged_relative.display()
            )
        })?;
        remove_file(&parent, Path::new(name))
            .map_err(|error| format!("test staged cleanup failed: {error}"))
    }
}

#[cfg(windows)]
mod windows_finalization {
    use std::mem::offset_of;
    use std::os::windows::{ffi::OsStrExt, io::AsRawHandle};

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FILE_RENAME_INFO, FileDispositionInfo, FileRenameInfo,
        SetFileInformationByHandle,
    };

    pub(super) fn rename_no_replace(
        staged: &std::fs::File,
        target_parent: &std::fs::File,
        target_name: &std::ffi::OsStr,
    ) -> Result<(), String> {
        let target_name: Vec<u16> = target_name.encode_wide().collect();
        let name_bytes = target_name
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .ok_or_else(|| "target name is too long".to_string())?;
        let size = offset_of!(FILE_RENAME_INFO, FileName)
            .checked_add(name_bytes)
            .ok_or_else(|| "rename information is too large".to_string())?;
        let word_size = std::mem::size_of::<usize>();
        let word_count = size
            .checked_add(word_size - 1)
            .ok_or_else(|| "rename information alignment overflowed".to_string())?
            / word_size;
        let mut buffer = vec![0usize; word_count];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        unsafe {
            (*info).Anonymous.ReplaceIfExists = false;
            (*info).RootDirectory = HANDLE(target_parent.as_raw_handle());
            (*info).FileNameLength = name_bytes as u32;
            std::ptr::copy_nonoverlapping(
                target_name.as_ptr(),
                (*info).FileName.as_mut_ptr(),
                target_name.len(),
            );
            SetFileInformationByHandle(
                HANDLE(staged.as_raw_handle()),
                FileRenameInfo,
                info.cast(),
                size as u32,
            )
            .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(super) fn delete_by_handle(staged: &std::fs::File) -> Result<(), String> {
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        unsafe {
            SetFileInformationByHandle(
                HANDLE(staged.as_raw_handle()),
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
            .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

/// An already-open source root retained for the complete recovery of one row.
///
/// All relative filesystem operations below are resolved from this descriptor. The
/// descriptor is deliberately not converted back into a path between validation and
/// mutation, so replacement of the named root cannot redirect recovery elsewhere.
pub(super) struct RecoveryRoot {
    path: PathBuf,
    capability: fs::File,
    identity: String,
}

/// Descriptor-derived facts for one regular file.
#[derive(Debug)]
pub(super) struct OpenedFile {
    capability: fs::File,
    pub(super) identity: String,
    pub(super) file_size: u64,
    pub(super) modified_ns: i64,
}

impl RecoveryRoot {
    pub(super) fn open_if_available(
        path: &Path,
        expected_identity: Option<&str>,
    ) -> Result<Option<Self>, String> {
        match fs::symlink_metadata(path) {
            Ok(_) => Self::open(path, expected_identity).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!(
                "failed to inspect recovery root {}: {error}",
                path.display()
            )),
        }
    }

    pub(super) fn open(path: &Path, expected_identity: Option<&str>) -> Result<Self, String> {
        let expected_identity = expected_identity.ok_or_else(|| {
            format!(
                "refusing file-op recovery for {} because the root identity is missing",
                path.display()
            )
        })?;
        let capability = open_root_nofollow(path).map_err(|error| {
            format!(
                "failed to open recovery root {} without following symlinks: {error}",
                path.display()
            )
        })?;
        let actual_identity = root_identity_from_open_file(&capability).map_err(|error| {
            format!(
                "failed to inspect recovery root {} from its open descriptor: {error}",
                path.display()
            )
        })?;
        if actual_identity != expected_identity {
            return Err(format!(
                "refusing file-op recovery for {} because the root identity changed (expected {}, found {})",
                path.display(),
                expected_identity,
                actual_identity
            ));
        }
        Ok(Self {
            path: path.to_path_buf(),
            capability,
            identity: actual_identity,
        })
    }

    pub(super) fn revalidate_named_root(&self) -> Result<(), String> {
        let capability = open_root_nofollow(&self.path).map_err(|error| {
            format!(
                "failed to revalidate recovery root {} without following symlinks: {error}",
                self.path.display()
            )
        })?;
        let actual_identity = root_identity_from_open_file(&capability).map_err(|error| {
            format!(
                "failed to inspect recovery root {} during revalidation: {error}",
                self.path.display()
            )
        })?;
        if actual_identity != self.identity {
            return Err(format!(
                "recovery root {} was replaced during recovery (expected {}, found {})",
                self.path.display(),
                self.identity,
                actual_identity
            ));
        }
        Ok(())
    }

    pub(super) fn open_file(&self, relative: &Path) -> Result<Option<OpenedFile>, String> {
        let Some(parent) =
            self.open_directory_optional(relative.parent().unwrap_or_else(|| Path::new(".")))?
        else {
            return Ok(None);
        };
        let name = relative.file_name().ok_or_else(|| {
            format!(
                "invalid recovery file path relative to {}: {}",
                self.path.display(),
                relative.display()
            )
        })?;
        let mut options = OpenOptions::new();
        options.read(true)._cap_fs_ext_follow(FollowSymlinks::No);
        #[cfg(windows)]
        options
            .access_mode(
                (windows::Win32::Storage::FileSystem::FILE_GENERIC_READ
                    | windows::Win32::Storage::FileSystem::DELETE)
                    .0,
            )
            .share_mode(
                (windows::Win32::Storage::FileSystem::FILE_SHARE_READ
                    | windows::Win32::Storage::FileSystem::FILE_SHARE_WRITE)
                    .0,
            );
        let file = match open(&parent, Path::new(name), &options) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "failed to open {} without following symlinks: {error}",
                    self.path.join(relative).display()
                ));
            }
        };
        let metadata = file.metadata().map_err(|error| {
            format!(
                "failed to inspect {} from its open descriptor: {error}",
                self.path.join(relative).display()
            )
        })?;
        if !metadata.is_file() {
            return Err(format!(
                "refusing {} because it is not a regular file",
                self.path.join(relative).display()
            ));
        }
        let identity = stable_filesystem_identity_from_open_file(&file).ok_or_else(|| {
            format!(
                "failed to obtain a stable identity for {} from its open descriptor",
                self.path.join(relative).display()
            )
        })?;
        let modified_ns = system_time_to_unix_nanos(metadata.modified().map_err(|error| {
            format!(
                "missing modified time for {}: {error}",
                self.path.join(relative).display()
            )
        })?);
        Ok(Some(OpenedFile {
            capability: file,
            identity,
            file_size: metadata.len(),
            modified_ns,
        }))
    }

    #[cfg(any(windows, test))]
    pub(super) fn ensure_parent(&self, relative: &Path) -> Result<(), String> {
        let parent = relative.parent().unwrap_or_else(|| Path::new("."));
        let mut directory = self.capability.try_clone().map_err(|error| {
            format!(
                "failed to retain {} capability: {error}",
                self.path.display()
            )
        })?;
        for component in relative_components(parent)? {
            let component_path = Path::new(component);
            directory = match open_dir_nofollow(&directory, component_path) {
                Ok(directory) => directory,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    create_dir(&directory, component_path, &DirOptions::new()).map_err(
                        |error| {
                            format!(
                                "failed to create {} without following symlinks: {error}",
                                self.path.join(parent).display()
                            )
                        },
                    )?;
                    open_dir_nofollow(&directory, component_path).map_err(|error| {
                        format!(
                            "failed to reopen {} without following symlinks: {error}",
                            self.path.join(parent).display()
                        )
                    })?
                }
                Err(error) => {
                    return Err(format!(
                        "failed to open ancestor {} without following symlinks: {error}",
                        self.path.join(parent).display()
                    ));
                }
            };
        }
        Ok(())
    }

    pub(super) fn bind_database_root(&self) -> Result<BoundDatabaseRoot, String> {
        #[cfg(windows)]
        {
            let capability = windows_database_root_binding(&self.path, &self.identity)
                .map_err(|error| error.to_string())?;
            let path = windows_final_path(&capability).map_err(|error| error.to_string())?;
            validate_database_root_alias(&capability, &path, &self.identity)
                .map_err(|error| error.to_string())?;
            return Ok(BoundDatabaseRoot {
                path,
                _capability: capability,
                identity: self.identity.clone(),
                profile_identity: self.identity.clone(),
                named_path: None,
                namespace_kind: BoundDatabaseRootNamespace::Profile,
                _lease: None,
            });
        }

        #[cfg(not(windows))]
        {
            let capability = self.capability.try_clone().map_err(|error| {
                format!("failed to retain source-root capability for database recovery: {error}")
            })?;
            let path = database_root_alias(&capability, &self.path, &self.identity)
                .map_err(|error| error.to_string())?;
            Ok(BoundDatabaseRoot {
                path,
                _capability: capability,
                identity: self.identity.clone(),
                profile_identity: self.identity.clone(),
                named_path: None,
                namespace_kind: BoundDatabaseRootNamespace::Profile,
                _lease: None,
            })
        }
    }

    #[cfg(any(windows, test))]
    fn open_directory(&self, relative: &Path) -> Result<fs::File, String> {
        self.open_directory_optional(relative)?.ok_or_else(|| {
            format!(
                "recovery ancestor does not exist: {}",
                self.path.join(relative).display()
            )
        })
    }

    fn open_directory_optional(&self, relative: &Path) -> Result<Option<fs::File>, String> {
        let mut directory = self.capability.try_clone().map_err(|error| {
            format!(
                "failed to retain {} capability: {error}",
                self.path.display()
            )
        })?;
        for component in relative_components(relative)? {
            directory = match open_dir_nofollow(&directory, Path::new(component)) {
                Ok(directory) => directory,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(format!(
                        "failed to open ancestor {} without following symlinks: {error}",
                        self.path.join(relative).display()
                    ));
                }
            };
        }
        Ok(Some(directory))
    }
}

pub(crate) struct BoundDatabaseRoot {
    path: PathBuf,
    _capability: fs::File,
    identity: String,
    profile_identity: String,
    named_path: Option<PathBuf>,
    namespace_kind: BoundDatabaseRootNamespace,
    _lease: Option<Arc<ProfileChildLease>>,
}

#[derive(Clone, Copy)]
enum BoundDatabaseRootNamespace {
    Profile,
    ProfileChild,
    ExternalSource,
}

/// Failure while deriving or validating the descriptor-bound namespace used by a database.
///
/// This stays private to the binding layer so callers can distinguish a missing namespace,
/// an uninspectable identity, and a confirmed replacement without turning path-derived strings
/// into ownership decisions.
#[derive(Debug)]
enum BoundDatabaseRootError {
    Unavailable { path: PathBuf, reason: String },
    IdentityUnavailable { path: PathBuf, reason: String },
    Replaced { path: PathBuf },
}

impl fmt::Display for BoundDatabaseRootError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { path, reason } => write!(
                formatter,
                "source database root binding namespace is unavailable at {}: {reason}",
                path.display()
            ),
            Self::IdentityUnavailable { path, reason } => write!(
                formatter,
                "source database root binding namespace identity is uncertain at {}: {reason}",
                path.display()
            ),
            Self::Replaced { path } => write!(
                formatter,
                "source database root binding namespace was replaced (identity mismatch): {}",
                path.display()
            ),
        }
    }
}

/// Descriptor chain for one bounded nested index locator. Never exposes a native pathname.
pub(crate) struct HeldIndexFile {
    #[cfg(target_os = "linux")]
    chain: Vec<fs::File>,
    #[cfg(target_os = "linux")]
    names: Vec<std::ffi::OsString>,
}
impl HeldIndexFile {
    pub(crate) fn validate(
        &self,
        entry: &super::super::SourceIndexEntry,
    ) -> Result<(), super::super::SourceIndexInspectionError> {
        self.validate_facts(
            entry.file_identity.as_deref(),
            entry.file_size,
            entry.modified_ns,
        )
    }

    pub(crate) fn validate_facts(
        &self,
        expected_identity: Option<&str>,
        expected_size: Option<u64>,
        expected_modified: Option<i64>,
    ) -> Result<(), super::super::SourceIndexInspectionError> {
        use super::super::SourceIndexInspectionError as E;
        #[cfg(target_os = "linux")]
        {
            for (i, name) in self.names.iter().enumerate() {
                let current = open_index_component(&self.chain[i], name, i + 1 < self.names.len())?;
                let current_identity =
                    stable_filesystem_identity_from_open_file(&current).ok_or(E::Unavailable)?;
                let held_identity = stable_filesystem_identity_from_open_file(&self.chain[i + 1])
                    .ok_or(E::Unavailable)?;
                if current_identity != held_identity {
                    return Err(E::ChangedEntry);
                }
            }
            let file = self.chain.last().ok_or(E::Unavailable)?;
            let metadata = file.metadata().map_err(|_| E::Unavailable)?;
            if !metadata.is_file() {
                return Err(E::Unavailable);
            }
            let identity = stable_filesystem_identity_from_open_file(file).ok_or(E::Unavailable)?;
            let modified =
                system_time_to_unix_nanos(metadata.modified().map_err(|_| E::Unavailable)?);
            if expected_identity != Some(&identity)
                || expected_size != Some(metadata.len())
                || expected_modified != Some(modified)
            {
                return Err(E::ChangedEntry);
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (expected_identity, expected_size, expected_modified);
            Err(E::Unsupported)
        }
    }
    pub(crate) fn open_for_feature_read(
        &self,
    ) -> Result<fs::File, super::super::SourceIndexInspectionError> {
        use super::super::SourceIndexInspectionError as E;
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::{AsRawFd, FromRawFd};
            use std::os::unix::ffi::OsStrExt;
            let parent = self
                .chain
                .get(self.chain.len().checked_sub(2).ok_or(E::Unavailable)?)
                .ok_or(E::Unavailable)?;
            let name = std::ffi::CString::new(self.names.last().ok_or(E::Unavailable)?.as_bytes())
                .map_err(|_| E::InvalidPath)?;
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                )
            };
            if fd < 0 {
                return Err(E::Unavailable);
            }
            // openat returned a new, uniquely owned descriptor. Validate it against the held
            // O_PATH regular file before data access; never follow a pathname/proc alias.
            let file = unsafe { fs::File::from_raw_fd(fd) };
            let held = self.chain.last().ok_or(E::Unavailable)?;
            if !file.metadata().map_err(|_| E::Unavailable)?.is_file()
                || stable_filesystem_identity_from_open_file(&file).ok_or(E::Unavailable)?
                    != stable_filesystem_identity_from_open_file(held).ok_or(E::Unavailable)?
            {
                return Err(E::ChangedEntry);
            }
            Ok(file)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(E::Unsupported)
        }
    }
}
#[cfg(target_os = "linux")]
fn open_index_component(
    parent: &fs::File,
    name: &OsStr,
    directory: bool,
) -> Result<fs::File, super::super::SourceIndexInspectionError> {
    use super::super::SourceIndexInspectionError as E;
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    };
    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| E::InvalidPath)?;
    // O_PATH never reads data or blocks on FIFO/device open. Each name is one validated
    // normal component, rooted in a held parent descriptor; symlinks are never followed.
    let flags = libc::O_PATH
        | libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | if directory { libc::O_DIRECTORY } else { 0 };
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        return Err(E::Unavailable);
    }
    // The successful openat returns a new descriptor, uniquely owned by this File.
    let file = unsafe { fs::File::from_raw_fd(descriptor) };
    let metadata = file.metadata().map_err(|_| E::Unavailable)?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(E::Unavailable);
    }
    Ok(file)
}

/// Read-only root observation. No profile-child lease or namespace mutation API is exposed.
#[allow(dead_code)] // Private native publication reader, OPT-2272.
pub(crate) struct ReadonlyDatabaseRoot {
    root: BoundDatabaseRoot,
}
#[allow(dead_code)]
impl ReadonlyDatabaseRoot {
    pub(crate) fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.root.path.capacity()
            + self.root.named_path.as_ref().map_or(0, PathBuf::capacity)
            + self.root.identity.capacity()
            + self.root.profile_identity.capacity()
    }
    pub(crate) fn validate(&self) -> io::Result<()> {
        if let Some(named) = &self.root.named_path {
            let current = open_root_nofollow(named)?;
            if root_identity_from_open_file(&current)? != self.root.identity {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "readonly root replaced",
                ));
            }
        }
        validate_database_root_alias(&self.root._capability, &self.root.path, &self.root.identity)
            .map_err(|e| io::Error::other(format!("{e:?}")))
    }
    pub(crate) fn verify_direct_child(&self, name: &Path, expected: &str) -> io::Result<()> {
        self.validate()?;
        let file = self.root.open_observation_file_nofollow(name)?;
        if stable_filesystem_identity_from_open_file(&file).as_deref() != Some(expected) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "readonly child replaced",
            ));
        }
        self.validate()
    }
}

impl BoundDatabaseRoot {
    /// Hold a bounded normal-component chain without data access or symlink traversal.
    pub(crate) fn inspect_index_file(
        &self,
        relative: &Path,
    ) -> Result<HeldIndexFile, super::super::SourceIndexInspectionError> {
        use super::super::SourceIndexInspectionError as E;
        if relative.as_os_str().as_encoded_bytes().len() > 1024 {
            return Err(E::ByteLimit);
        }
        let names = relative
            .components()
            .map(|component| match component {
                Component::Normal(name) => Ok(name.to_os_string()),
                _ => Err(E::InvalidPath),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if names.is_empty() || names.len() > 32 {
            return Err(E::InvalidPath);
        }
        #[cfg(target_os = "linux")]
        {
            let mut chain = vec![self._capability.try_clone().map_err(|_| E::Unavailable)?];
            for (i, name) in names.iter().enumerate() {
                let next = open_index_component(chain.last().unwrap(), name, i + 1 < names.len())?;
                chain.push(next);
            }
            Ok(HeldIndexFile { chain, names })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = names;
            Err(E::Unsupported)
        }
    }

    /// Open a direct database child without resolving the retained Linux root to an ambient path.
    /// The caller must retain this root until the connection closes and keep its existing
    /// profile/main-file identity checks before and after opening.
    pub(crate) fn open_sqlite(
        &self,
        name: &Path,
        flags: rusqlite::OpenFlags,
    ) -> rusqlite::Result<rusqlite::Connection> {
        let components = relative_components(name)
            .map_err(|_| rusqlite::Error::InvalidPath(name.to_path_buf()))?;
        if components.len() != 1 {
            return Err(rusqlite::Error::InvalidPath(name.to_path_buf()));
        }
        let flags = flags | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
        #[cfg(target_os = "linux")]
        {
            rusqlite::Connection::open_with_flags_and_vfs(
                self.path.join(name),
                flags,
                crate::sqlite_linux::vfs_name()?,
            )
        }
        #[cfg(not(target_os = "linux"))]
        {
            rusqlite::Connection::open_with_flags(self.path.join(name), flags)
        }
    }

    /// Open a connection-owned forwarding VFS without changing journal/store open semantics.
    pub(crate) fn open_owned_sqlite(
        &self,
        name: &Path,
        flags: rusqlite::OpenFlags,
    ) -> rusqlite::Result<crate::sqlite_budget::OwnedConnection> {
        let components = relative_components(name)
            .map_err(|_| rusqlite::Error::InvalidPath(name.to_path_buf()))?;
        if components.len() != 1 {
            return Err(rusqlite::Error::InvalidPath(name.to_path_buf()));
        }
        let descriptor = self
            .try_clone_descriptor()
            .map_err(|_| rusqlite::Error::InvalidPath(name.to_path_buf()))?;
        #[cfg(target_os = "linux")]
        let path = {
            use std::os::fd::AsRawFd;
            PathBuf::from(format!("/proc/self/fd/{}", descriptor.as_raw_fd())).join(name)
        };
        #[cfg(not(target_os = "linux"))]
        let path = self.path.join(name);
        let connection = crate::sqlite_budget::open_owned(
            path,
            flags | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
            Some(descriptor),
        )?;
        Ok(connection)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Duplicate the retained directory descriptor for an owner-owned capability.
    ///
    /// The original profile-child lease remains held by the store and is shared by the duplicate
    /// through the retained lease handle. The duplicate acquires no second in-process lease and
    /// keeps an owner-owned reserve capsule separate from the store while that shared lease lives.
    pub(crate) fn try_clone_for_owned_capability(&self) -> io::Result<Self> {
        Ok(Self {
            path: self.path.clone(),
            _capability: self._capability.try_clone()?,
            identity: self.identity.clone(),
            profile_identity: self.profile_identity.clone(),
            named_path: self.named_path.clone(),
            namespace_kind: self.namespace_kind,
            _lease: self._lease.as_ref().map(Arc::clone),
        })
    }

    /// Duplicate the retained directory descriptor for descriptor-derived volume facts.
    /// Capture only observation descriptors; the original owner retains its child lease.
    #[allow(dead_code)]
    pub(crate) fn readonly_observation(&self) -> io::Result<ReadonlyDatabaseRoot> {
        let bounded = |path: &Path| {
            path.as_os_str().as_encoded_bytes().len() <= 4096 && path.components().count() <= 64
        };
        if !bounded(&self.path)
            || self.named_path.as_ref().is_some_and(|p| !bounded(p))
            || self.identity.len() > 256
            || self.profile_identity.len() > 256
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "readonly root backing limit",
            ));
        }
        let capability = self._capability.try_clone()?;
        // A copied /proc alias must refer to THIS retained descriptor, not the original
        // temporary/actor descriptor. The readonly witness owns no lease or authority.
        let path = database_root_alias(
            &capability,
            self.named_path.as_deref().unwrap_or(&self.path),
            &self.identity,
        )
        .map_err(|e| io::Error::other(format!("{e:?}")))?;
        let observer = ReadonlyDatabaseRoot {
            root: Self {
                path,
                _capability: capability,
                identity: self.identity.to_string(),
                profile_identity: self.profile_identity.to_string(),
                named_path: self.named_path.as_ref().map(|p| p.to_path_buf()),
                namespace_kind: self.namespace_kind,
                _lease: None,
            },
        };
        observer.validate()?;
        Ok(observer)
    }

    pub(crate) fn try_clone_descriptor(&self) -> io::Result<fs::File> {
        self._capability.try_clone()
    }

    /// Open one direct child through the retained database-root capability.
    ///
    /// SQLite is given the resulting descriptor's namespace alias, while all file creation and
    /// final-component checks stay anchored to this retained capability. The final component is
    /// never followed when it is a symlink.
    pub(crate) fn open_file_nofollow(&self, name: &Path, create_new: bool) -> io::Result<fs::File> {
        let components = relative_components(name).map_err(io::Error::other)?;
        if components.len() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "profile database child file name must be one normal component",
            ));
        }
        let directory = cap_std::fs::Dir::from_std_file(self._capability.try_clone()?);
        let mut options = OpenOptions::new();
        options.read(true).write(true).follow(FollowSymlinks::No);
        if create_new {
            options.create_new(true);
        }
        directory
            .open_with(name, &options)
            .map(|file| file.into_std())
    }

    /// Open one direct child for read-only validation through the retained directory descriptor.
    /// The final component is never followed when it is a symlink.
    pub(crate) fn open_file_read_nofollow(&self, name: &Path) -> io::Result<fs::File> {
        let components = relative_components(name).map_err(io::Error::other)?;
        if components.len() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "staging file name must be one normal component",
            ));
        }
        let directory = cap_std::fs::Dir::from_std_file(self._capability.try_clone()?);
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        directory
            .open_with(name, &options)
            .map(|file| file.into_std())
    }

    /// Read one observation child without following links or waiting for a substituted FIFO.
    pub(crate) fn open_observation_file_nofollow(&self, name: &Path) -> io::Result<fs::File> {
        #[cfg(not(unix))]
        {
            let _ = name;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "bounded observation open unavailable",
            ))
        }
        #[cfg(unix)]
        {
            use cap_primitives::fs::OpenOptionsExt;
            let components = relative_components(name).map_err(io::Error::other)?;
            if components.len() != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "observation child must be direct",
                ));
            }
            let directory = cap_std::fs::Dir::from_std_file(self._capability.try_clone()?);
            let mut options = OpenOptions::new();
            options
                .read(true)
                .follow(FollowSymlinks::No)
                .custom_flags(libc::O_NONBLOCK);
            let file = directory.open_with(name, &options)?.into_std();
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "observation child is not regular",
                ));
            }
            Ok(file)
        }
    }

    /// Inspect one direct child without following a final symlink.
    pub(crate) fn symlink_metadata(&self, name: &Path) -> io::Result<cap_std::fs::Metadata> {
        let components = relative_components(name).map_err(io::Error::other)?;
        if components.len() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "profile database child file name must be one normal component",
            ));
        }
        let directory = cap_std::fs::Dir::from_std_file(self._capability.try_clone()?);
        directory.symlink_metadata(name)
    }

    /// Flush the retained profile and child directory entries before a durable child-file write.
    pub(crate) fn sync_namespace(
        &self,
        profile_guard: &WritableProfileGuard,
    ) -> Result<(), ProfileOwnershipError> {
        #[cfg(windows)]
        {
            // SQLite's bundled Windows VFS flushes committed database content. Windows directory
            // handles do not provide a portable, verified equivalent of Unix fsync for this
            // namespace, so the store deliberately makes no stronger power-loss claim here.
            let _ = (self, profile_guard);
            return Ok(());
        }

        #[cfg(not(windows))]
        {
            let profile_root = profile_guard.profile_root_dir()?;
            sync_retained_directory(&profile_root.into_std_file()).map_err(|source| {
                profile_binding_error(
                    profile_guard,
                    format!("failed to flush profile namespace: {source}"),
                )
            })?;
            sync_retained_directory(&self._capability).map_err(|source| {
                profile_binding_error(
                    profile_guard,
                    format!("failed to flush operation-journal namespace: {source}"),
                )
            })
        }
    }

    pub(crate) fn for_profile_guard(
        profile_guard: &WritableProfileGuard,
    ) -> Result<Self, ProfileOwnershipError> {
        let expected_identity = profile_guard.profile_root_identity();

        #[cfg(windows)]
        let capability =
            windows_database_root_binding(profile_guard.profile_root(), expected_identity)
                .map_err(|error| profile_binding_error(profile_guard, error.to_string()))?;

        #[cfg(not(windows))]
        let capability = profile_guard.profile_root_dir()?.into_std_file();

        #[cfg(windows)]
        let path = windows_final_path(&capability)
            .map_err(|error| profile_binding_error(profile_guard, error.to_string()))?;

        #[cfg(not(windows))]
        let path =
            database_root_alias(&capability, profile_guard.profile_root(), expected_identity)
                .map_err(|error| profile_binding_error(profile_guard, error.to_string()))?;

        validate_database_root_alias(&capability, &path, expected_identity)
            .map_err(|error| profile_binding_error(profile_guard, error.to_string()))?;
        Ok(Self {
            path,
            _capability: capability,
            identity: expected_identity.to_owned(),
            profile_identity: expected_identity.to_owned(),
            named_path: Some(profile_guard.profile_root().to_path_buf()),
            namespace_kind: BoundDatabaseRootNamespace::Profile,
            _lease: None,
        })
    }

    pub(crate) fn for_profile_child(
        profile_guard: &WritableProfileGuard,
        child: &Path,
    ) -> Result<Self, ProfileOwnershipError> {
        let child_path = profile_guard.profile_root().join(child);
        let (child_dir, lease) = profile_guard
            .open_child_dir_with_lease(child)
            .map_err(|error| map_profile_child_open_error(&child_path, error))?;
        let child_identity_capability = child_dir
            .try_clone()
            .map_err(
                |source| ProfileOwnershipError::ProfileChildDatabaseRootUnavailable {
                    path: child_path.clone(),
                    reason: format!("failed to retain profile database child capability: {source}"),
                },
            )?
            .into_std_file();
        let child_identity =
            root_identity_from_open_file(&child_identity_capability).map_err(|source| {
                ProfileOwnershipError::ProfileChildDatabaseRootIdentityUnavailable {
                    path: child_path.clone(),
                    reason: format!(
                        "failed to inspect profile database child capability: {source}"
                    ),
                }
            })?;

        #[cfg(windows)]
        let capability = windows_database_root_binding(
            &profile_guard.profile_root().join(child),
            &child_identity,
        )
        .map_err(|error| map_profile_child_binding_error(&child_path, error))?;

        #[cfg(not(windows))]
        let capability = child_dir.into_std_file();

        let identity = root_identity_from_open_file(&capability).map_err(|source| {
            ProfileOwnershipError::ProfileChildDatabaseRootIdentityUnavailable {
                path: child_path.clone(),
                reason: format!("failed to inspect profile database child capability: {source}"),
            }
        })?;
        if identity != child_identity {
            return Err(ProfileOwnershipError::ProfileChildDatabaseRootReplaced {
                path: child_path.clone(),
            });
        }

        #[cfg(windows)]
        let path = windows_final_path(&capability).map_err(|reason| {
            ProfileOwnershipError::ProfileChildDatabaseRootUnavailable {
                path: child_path.clone(),
                reason: reason.to_string(),
            }
        })?;

        #[cfg(not(windows))]
        let path = database_root_alias(&capability, profile_guard.profile_root(), &identity)
            .map_err(|error| map_profile_child_binding_error(&child_path, error))?;

        validate_database_root_alias(&capability, &path, &identity)
            .map_err(|error| map_profile_child_binding_error(&child_path, error))?;
        Ok(Self {
            path,
            _capability: capability,
            identity,
            profile_identity: profile_guard.profile_root_identity().to_owned(),
            named_path: Some(profile_guard.profile_root().join(child)),
            namespace_kind: BoundDatabaseRootNamespace::ProfileChild,
            _lease: Some(Arc::new(lease)),
        })
    }

    /// Bind an external source-root namespace to a profile owner.
    ///
    /// The profile owner and external source root are retained and revalidated independently.
    /// This capability allows two profiles to address one physical source database while keeping
    /// source-root replacement distinguishable from replacement of the profile itself.
    pub(crate) fn for_external_source_root(
        profile_guard: &WritableProfileGuard,
        source_root: &Path,
    ) -> Result<Self, ProfileOwnershipError> {
        profile_guard.validate_current()?;
        let capability = open_root_nofollow(source_root).map_err(|source| {
            ProfileOwnershipError::ExternalSourceRootUnavailable {
                path: source_root.to_path_buf(),
                reason: format!(
                    "failed to open external source root without following symlinks: {source}"
                ),
            }
        })?;
        let identity = root_identity_from_open_file(&capability).map_err(|source| {
            ProfileOwnershipError::ExternalSourceRootIdentityUnavailable {
                path: source_root.to_path_buf(),
                reason: format!("failed to inspect external source root: {source}"),
            }
        })?;

        #[cfg(windows)]
        let capability = windows_database_root_binding(source_root, &identity)
            .map_err(|error| map_external_source_binding_error(source_root, error))?;

        #[cfg(windows)]
        let path = windows_final_path(&capability).map_err(|reason| {
            ProfileOwnershipError::ExternalSourceRootUnavailable {
                path: source_root.to_path_buf(),
                reason: reason.to_string(),
            }
        })?;

        #[cfg(not(windows))]
        let path = database_root_alias(&capability, source_root, &identity)
            .map_err(|error| map_external_source_binding_error(source_root, error))?;

        validate_database_root_alias(&capability, &path, &identity)
            .map_err(|error| map_external_source_binding_error(source_root, error))?;
        Ok(Self {
            path,
            _capability: capability,
            identity,
            profile_identity: profile_guard.profile_root_identity().to_owned(),
            named_path: Some(source_root.to_path_buf()),
            namespace_kind: BoundDatabaseRootNamespace::ExternalSource,
            _lease: None,
        })
    }

    pub(crate) fn identity(&self) -> &str {
        &self.identity
    }

    /// Return whether this binding is the retained profile-child namespace used by catalog data.
    ///
    /// External source roots intentionally cannot be retargeted through a catalog handoff: their
    /// physical-source ownership must be reacquired through the ordinary external-source path.
    pub(crate) fn is_profile_namespace(&self) -> bool {
        matches!(
            self.namespace_kind,
            BoundDatabaseRootNamespace::Profile | BoundDatabaseRootNamespace::ProfileChild
        )
    }

    pub(crate) fn validate_profile_guard(
        &self,
        profile_guard: &WritableProfileGuard,
    ) -> Result<(), ProfileOwnershipError> {
        profile_guard.validate_current()?;
        if let Some(named_path) = &self.named_path {
            let named_capability = open_root_nofollow(named_path).map_err(|error| {
                map_namespace_unavailable(
                    self.namespace_kind,
                    named_path,
                    format!(
                        "failed to revalidate retained database root without following symlinks: {error}"
                    ),
                    profile_guard,
                )
            })?;
            let named_identity =
                root_identity_from_open_file(&named_capability).map_err(|error| {
                    map_namespace_identity_unavailable(
                        self.namespace_kind,
                        named_path,
                        format!(
                            "failed to inspect retained database root during revalidation: {error}"
                        ),
                        profile_guard,
                    )
                })?;
            if named_identity != self.identity {
                return Err(match self.namespace_kind {
                    BoundDatabaseRootNamespace::Profile => {
                        ProfileOwnershipError::ProfileRootReplaced {
                            path: named_path.clone(),
                        }
                    }
                    BoundDatabaseRootNamespace::ProfileChild => {
                        ProfileOwnershipError::ProfileChildDatabaseRootReplaced {
                            path: named_path.clone(),
                        }
                    }
                    BoundDatabaseRootNamespace::ExternalSource => {
                        ProfileOwnershipError::SourceRootReplaced {
                            path: named_path.clone(),
                        }
                    }
                });
            }
        }
        let namespace_path = self.named_path.as_deref().unwrap_or(&self.path);
        validate_database_root_alias(&self._capability, &self.path, &self.identity).map_err(
            |error| {
                map_namespace_binding_error(
                    self.namespace_kind,
                    namespace_path,
                    error,
                    profile_guard,
                )
            },
        )?;
        if self.profile_identity != profile_guard.profile_root_identity() {
            return Err(ProfileOwnershipError::ProfileRootReplaced {
                path: profile_guard.profile_root().to_path_buf(),
            });
        }
        Ok(())
    }
}

impl DatabaseRootBindingGuard for BoundDatabaseRoot {
    fn validate_profile_guard(
        &self,
        profile_guard: &WritableProfileGuard,
    ) -> Result<(), ProfileOwnershipError> {
        Self::validate_profile_guard(self, profile_guard)
    }
}

fn profile_binding_error(
    profile_guard: &WritableProfileGuard,
    error: String,
) -> ProfileOwnershipError {
    ProfileOwnershipError::Io {
        path: profile_guard.profile_root().to_path_buf(),
        source: io::Error::other(error),
    }
}

fn map_profile_child_open_error(
    path: &Path,
    error: ProfileOwnershipError,
) -> ProfileOwnershipError {
    match error {
        error @ (ProfileOwnershipError::Io { .. }
        | ProfileOwnershipError::NotRegularFile { .. }) => {
            ProfileOwnershipError::ProfileChildDatabaseRootUnavailable {
                path: path.to_path_buf(),
                reason: error.to_string(),
            }
        }
        error @ ProfileOwnershipError::IdentityUnavailable { .. } => {
            ProfileOwnershipError::ProfileChildDatabaseRootIdentityUnavailable {
                path: path.to_path_buf(),
                reason: error.to_string(),
            }
        }
        other => other,
    }
}

fn map_profile_child_binding_error(
    path: &Path,
    error: BoundDatabaseRootError,
) -> ProfileOwnershipError {
    match error {
        BoundDatabaseRootError::Unavailable { reason, .. } => {
            ProfileOwnershipError::ProfileChildDatabaseRootUnavailable {
                path: path.to_path_buf(),
                reason,
            }
        }
        BoundDatabaseRootError::IdentityUnavailable { reason, .. } => {
            ProfileOwnershipError::ProfileChildDatabaseRootIdentityUnavailable {
                path: path.to_path_buf(),
                reason,
            }
        }
        BoundDatabaseRootError::Replaced { .. } => {
            ProfileOwnershipError::ProfileChildDatabaseRootReplaced {
                path: path.to_path_buf(),
            }
        }
    }
}

fn map_external_source_binding_error(
    path: &Path,
    error: BoundDatabaseRootError,
) -> ProfileOwnershipError {
    match error {
        BoundDatabaseRootError::Unavailable { reason, .. } => {
            ProfileOwnershipError::ExternalSourceRootUnavailable {
                path: path.to_path_buf(),
                reason,
            }
        }
        BoundDatabaseRootError::IdentityUnavailable { reason, .. } => {
            ProfileOwnershipError::ExternalSourceRootIdentityUnavailable {
                path: path.to_path_buf(),
                reason,
            }
        }
        BoundDatabaseRootError::Replaced { .. } => ProfileOwnershipError::SourceRootReplaced {
            path: path.to_path_buf(),
        },
    }
}

fn map_namespace_unavailable(
    namespace: BoundDatabaseRootNamespace,
    path: &Path,
    reason: String,
    profile_guard: &WritableProfileGuard,
) -> ProfileOwnershipError {
    match namespace {
        BoundDatabaseRootNamespace::Profile => profile_binding_error(profile_guard, reason),
        BoundDatabaseRootNamespace::ProfileChild => {
            ProfileOwnershipError::ProfileChildDatabaseRootUnavailable {
                path: path.to_path_buf(),
                reason,
            }
        }
        BoundDatabaseRootNamespace::ExternalSource => {
            ProfileOwnershipError::ExternalSourceRootUnavailable {
                path: path.to_path_buf(),
                reason,
            }
        }
    }
}

fn map_namespace_identity_unavailable(
    namespace: BoundDatabaseRootNamespace,
    path: &Path,
    reason: String,
    profile_guard: &WritableProfileGuard,
) -> ProfileOwnershipError {
    match namespace {
        BoundDatabaseRootNamespace::Profile => profile_binding_error(profile_guard, reason),
        BoundDatabaseRootNamespace::ProfileChild => {
            ProfileOwnershipError::ProfileChildDatabaseRootIdentityUnavailable {
                path: path.to_path_buf(),
                reason,
            }
        }
        BoundDatabaseRootNamespace::ExternalSource => {
            ProfileOwnershipError::ExternalSourceRootIdentityUnavailable {
                path: path.to_path_buf(),
                reason,
            }
        }
    }
}

fn map_namespace_binding_error(
    namespace: BoundDatabaseRootNamespace,
    path: &Path,
    error: BoundDatabaseRootError,
    profile_guard: &WritableProfileGuard,
) -> ProfileOwnershipError {
    match namespace {
        BoundDatabaseRootNamespace::Profile => {
            profile_binding_error(profile_guard, error.to_string())
        }
        BoundDatabaseRootNamespace::ProfileChild => map_profile_child_binding_error(path, error),
        BoundDatabaseRootNamespace::ExternalSource => {
            map_external_source_binding_error(path, error)
        }
    }
}

#[cfg(not(windows))]
fn database_root_alias(
    capability: &fs::File,
    _named_path: &Path,
    expected_identity: &str,
) -> Result<PathBuf, BoundDatabaseRootError> {
    #[cfg(target_os = "linux")]
    let alias = {
        use std::os::unix::io::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", capability.as_raw_fd()))
    };

    #[cfg(target_os = "macos")]
    let alias = {
        use std::os::unix::fs::MetadataExt;
        let metadata =
            capability
                .metadata()
                .map_err(|error| BoundDatabaseRootError::Unavailable {
                    path: _named_path.to_path_buf(),
                    reason: format!(
                        "failed to inspect source-root capability for /.vol binding: {error}"
                    ),
                })?;
        PathBuf::from(format!("/.vol/{}/{}", metadata.dev(), metadata.ino()))
    };

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = (capability, _named_path, expected_identity);
        return Err(BoundDatabaseRootError::Unavailable {
            path: _named_path.to_path_buf(),
            reason: "source database recovery requires a supported descriptor-bound root namespace"
                .to_string(),
        });
    }

    validate_database_root_alias(capability, &alias, expected_identity)?;
    Ok(alias)
}

fn validate_database_root_alias(
    capability: &fs::File,
    alias: &Path,
    expected_identity: &str,
) -> Result<(), BoundDatabaseRootError> {
    let metadata = fs::metadata(alias).map_err(|error| BoundDatabaseRootError::Unavailable {
        path: alias.to_path_buf(),
        reason: error.to_string(),
    })?;
    if !metadata.is_dir() {
        return Err(BoundDatabaseRootError::Replaced {
            path: alias.to_path_buf(),
        });
    }
    let capability_identity = root_identity_from_open_file(capability).map_err(|error| {
        BoundDatabaseRootError::IdentityUnavailable {
            path: alias.to_path_buf(),
            reason: format!("failed to inspect bound source-root capability: {error}"),
        }
    })?;
    let alias_identity = stable_filesystem_identity(alias, &metadata).ok_or_else(|| {
        BoundDatabaseRootError::IdentityUnavailable {
            path: alias.to_path_buf(),
            reason: "stable identity is unavailable".to_string(),
        }
    })?;
    if capability_identity != expected_identity || alias_identity != expected_identity {
        return Err(BoundDatabaseRootError::Replaced {
            path: alias.to_path_buf(),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn windows_database_root_binding(
    path: &Path,
    expected_identity: &str,
) -> Result<fs::File, BoundDatabaseRootError> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0);
    let capability = options.open(path).map_err(|error| {
        BoundDatabaseRootError::Unavailable {
            path: path.to_path_buf(),
            reason: format!(
                "failed to open source root with a delete-sharing barrier for database recovery: {error}"
            ),
        }
    })?;
    let actual_identity = root_identity_from_open_file(&capability).map_err(|error| {
        BoundDatabaseRootError::IdentityUnavailable {
            path: path.to_path_buf(),
            reason: format!(
                "failed to inspect source root opened for descriptor-bound database recovery: {error}"
            ),
        }
    })?;
    if actual_identity != expected_identity {
        return Err(BoundDatabaseRootError::Replaced {
            path: path.to_path_buf(),
        });
    }
    Ok(capability)
}

#[cfg(windows)]
fn windows_final_path(capability: &fs::File) -> Result<PathBuf, BoundDatabaseRootError> {
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, VOLUME_NAME_DOS};

    let mut buffer = vec![0u16; 256];
    loop {
        let length = unsafe {
            GetFinalPathNameByHandleW(
                HANDLE(capability.as_raw_handle()),
                &mut buffer,
                VOLUME_NAME_DOS,
            )
        };
        if length == 0 {
            return Err(BoundDatabaseRootError::Unavailable {
                path: PathBuf::from("<descriptor>"),
                reason: format!(
                    "failed to derive final source-root namespace from its open descriptor: {}",
                    std::io::Error::last_os_error()
                ),
            });
        }
        let length = length as usize;
        if length < buffer.len() {
            return Ok(PathBuf::from(std::ffi::OsString::from_wide(
                &buffer[..length],
            )));
        }
        buffer.resize(length + 1, 0);
    }
}

pub(super) fn capture_root_identity(path: &Path) -> io::Result<String> {
    let root = open_root_nofollow(path)?;
    root_identity_from_open_file(&root).map_err(io::Error::other)
}

pub(super) fn capture_file_identity(root_path: &Path, relative: &Path) -> io::Result<String> {
    let capability = open_root_nofollow(root_path)?;
    let root = RecoveryRoot {
        path: root_path.to_path_buf(),
        capability,
        identity: String::new(),
    };
    let file = root
        .open_file(relative)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file is missing"))?;
    Ok(file.identity)
}

impl OpenedFile {
    pub(super) fn is_still_same_object(&self) -> bool {
        stable_filesystem_identity_from_open_file(&self.capability).as_deref()
            == Some(self.identity.as_str())
    }
}

fn open_root_nofollow(path: &Path) -> io::Result<fs::File> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("root is not a non-symlink directory: {}", path.display()),
        ));
    }
    if path == Path::new("/") {
        return open_ambient_dir(path, ambient_authority());
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let parent_capability = open_ambient_dir(parent, ambient_authority())?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("root has no final component: {}", path.display()),
        )
    })?;
    open_dir_nofollow(&parent_capability, Path::new(name))
}

fn root_identity_from_open_file(file: &fs::File) -> io::Result<String> {
    stable_filesystem_identity_from_open_file(file)
        .ok_or_else(|| io::Error::other("stable filesystem identity is unavailable"))
}

fn relative_components(path: &Path) -> Result<Vec<&OsStr>, String> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => components.push(component),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "invalid non-relative recovery path: {}",
                    path.display()
                ));
            }
        }
    }
    Ok(components)
}

pub(super) trait RecoverySourceDatabases {
    fn open(&self, root: &RecoveryRoot) -> Result<SourceDatabase, String>;
}

pub(super) struct SourceDatabaseRecoveryAccess;

impl RecoverySourceDatabases for SourceDatabaseRecoveryAccess {
    fn open(&self, root: &RecoveryRoot) -> Result<SourceDatabase, String> {
        root.revalidate_named_root()?;
        let binding = root.bind_database_root()?;
        let bound_path = binding.path().to_path_buf();
        let database =
            SourceDatabase::open_for_source_write_with_database_root(&root.path, &bound_path)
                .map_err(|error| format!("Failed to open source DB for recovery: {error}"))?;
        root.revalidate_named_root()?;
        Ok(database.retain_database_root_binding(Arc::new(binding)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn retained_directory_sync_survives_ambient_path_replacement() {
        let temp = tempfile::tempdir().expect("temporary parent");
        let original = temp.path().join("original");
        let moved = temp.path().join("moved");
        fs::create_dir(&original).expect("directory");
        let capability = open_ambient_dir(&original, ambient_authority()).expect("capability");
        sync_retained_directory(&capability).expect("sync retained directory capability");

        fs::rename(&original, &moved).expect("move retained directory");
        std::os::unix::fs::symlink(temp.path().join("missing"), &original)
            .expect("replace ambient path with dangling symlink");
        sync_retained_directory(&capability).expect("sync must not reopen the ambient path");
        assert!(moved.is_dir());
        assert!(
            fs::symlink_metadata(&original)
                .expect("replacement")
                .is_symlink()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unsupported_platform_finalization_retains_staged_file_and_journal_boundary() {
        let temp = tempfile::tempdir().expect("temporary recovery root");
        std::fs::write(temp.path().join("staged.tmp"), b"staged").expect("staged file");
        let identity = capture_root_identity(temp.path()).expect("root identity");
        let root = RecoveryRoot::open(temp.path(), Some(&identity)).expect("recovery root");
        let staged = root
            .open_file(Path::new("staged.tmp"))
            .expect("open staged")
            .expect("staged exists");
        let finalizer = PlatformStagedFileFinalizer;

        let publish = finalizer.publish(
            &root,
            Path::new("staged.tmp"),
            &staged,
            Path::new("published.wav"),
        );
        assert!(
            publish
                .unwrap_err()
                .contains("unsupported on this platform")
        );
        assert!(temp.path().join("staged.tmp").exists());
        assert!(!temp.path().join("published.wav").exists());

        let cleanup = finalizer.cleanup(&root, Path::new("staged.tmp"), &staged);
        assert!(
            cleanup
                .unwrap_err()
                .contains("unsupported on this platform")
        );
        assert!(temp.path().join("staged.tmp").exists());
    }

    #[test]
    fn database_root_alias_validation_rejects_unavailable_and_mismatched_namespaces() {
        let temp = tempfile::tempdir().expect("temporary recovery root");
        let other = tempfile::tempdir().expect("other root");
        let identity = capture_root_identity(temp.path()).expect("root identity");
        let root = RecoveryRoot::open(temp.path(), Some(&identity)).expect("recovery root");
        let capability = root.capability.try_clone().expect("root capability");

        let unavailable = validate_database_root_alias(
            &capability,
            &temp.path().join("missing-namespace"),
            &identity,
        )
        .unwrap_err();
        assert!(unavailable.to_string().contains("namespace is unavailable"));

        let mismatch =
            validate_database_root_alias(&capability, other.path(), &identity).unwrap_err();
        assert!(mismatch.to_string().contains("identity mismatch"));
    }

    #[cfg(unix)]
    #[test]
    fn source_root_replacement_is_rejected_until_the_original_namespace_is_restored() {
        let temp = tempfile::tempdir().expect("temporary source parent");
        let root_path = temp.path().join("source");
        let parked_path = temp.path().join("source-parked");
        std::fs::create_dir(&root_path).expect("source root");
        let identity = capture_root_identity(&root_path).expect("root identity");
        let root = RecoveryRoot::open(&root_path, Some(&identity)).expect("recovery root");

        std::fs::rename(&root_path, &parked_path).expect("park original source root");
        std::fs::create_dir(&root_path).expect("replacement source root");
        let replaced = root.revalidate_named_root().unwrap_err();
        assert!(replaced.contains("was replaced during recovery"));

        std::fs::remove_dir(&root_path).expect("remove replacement root");
        std::fs::rename(&parked_path, &root_path).expect("restore original source root");
        root.revalidate_named_root().expect("restored source root");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn source_database_open_remains_bound_after_named_root_replacement() {
        let temp = tempfile::tempdir().expect("temporary source parent");
        let root_path = temp.path().join("source");
        let parked_path = temp.path().join("source-parked");
        std::fs::create_dir(&root_path).expect("source root");
        let identity = capture_root_identity(&root_path).expect("root identity");
        let root = RecoveryRoot::open(&root_path, Some(&identity)).expect("recovery root");

        let database = SourceDatabaseRecoveryAccess
            .open(&root)
            .expect("descriptor-bound source database");
        database
            .set_metadata("test.bound_database", "original")
            .expect("write bound database");

        std::fs::rename(&root_path, &parked_path).expect("park original source root");
        std::fs::create_dir(&root_path).expect("replacement source root");
        let replacement =
            SourceDatabase::open_for_source_write(&root_path).expect("replacement source database");

        assert_eq!(
            database
                .get_metadata("test.bound_database")
                .expect("read bound database")
                .as_deref(),
            Some("original")
        );
        assert_eq!(
            replacement
                .get_metadata("test.bound_database")
                .expect("read replacement database"),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_finalization_uses_opened_handle_and_retains_collision() {
        let temp = tempfile::tempdir().expect("temporary recovery root");
        std::fs::write(temp.path().join("staged.tmp"), b"staged").expect("staged file");
        let identity = capture_root_identity(temp.path()).expect("root identity");
        let root = RecoveryRoot::open(temp.path(), Some(&identity)).expect("recovery root");
        let staged = root
            .open_file(Path::new("staged.tmp"))
            .expect("open staged")
            .expect("staged exists");
        let finalizer = PlatformStagedFileFinalizer;

        std::fs::write(temp.path().join("collision.wav"), b"existing").expect("collision file");
        let collision = finalizer.publish(
            &root,
            Path::new("staged.tmp"),
            &staged,
            Path::new("collision.wav"),
        );
        assert!(collision.is_err());
        assert_eq!(
            std::fs::read(temp.path().join("staged.tmp")).unwrap(),
            b"staged"
        );
        assert_eq!(
            std::fs::read(temp.path().join("collision.wav")).unwrap(),
            b"existing"
        );

        let published = finalizer.publish(
            &root,
            Path::new("staged.tmp"),
            &staged,
            Path::new("published.wav"),
        );
        assert_eq!(published.unwrap(), StagedFinalization::Published);
        assert!(!temp.path().join("staged.tmp").exists());
        assert_eq!(
            std::fs::read(temp.path().join("published.wav")).unwrap(),
            b"staged"
        );
    }
}

#[cfg(test)]
mod index_inspection_primitive_tests {
    #![cfg(target_os = "linux")]
    use super::super::super::{
        SourceIndexClassification, SourceIndexEntry, SourceIndexInspectionError as E,
    };
    use super::*;
    fn facts(path: &Path, relative: &Path) -> SourceIndexEntry {
        let file = fs::File::open(path).unwrap();
        let metadata = file.metadata().unwrap();
        SourceIndexEntry {
            relative_path: relative.to_path_buf(),
            classification: SourceIndexClassification::UnsupportedNonAudio,
            file_size: Some(metadata.len()),
            modified_ns: Some(system_time_to_unix_nanos(metadata.modified().unwrap())),
            file_identity: stable_filesystem_identity_from_open_file(&file),
            diagnostic: None,
            format_policy_version: 1,
        }
    }
    #[test]
    fn held_index_inspection_revalidates_parent_edges_and_held_metadata() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        let relative = Path::new("nested/child.txt");
        let path = root.path().join(relative);
        fs::write(&path, b"facts").unwrap();
        let entry = facts(&path, relative);
        // Filesystem primitive test only: the production catalog tests separately exercise real profile/lease admission.
        let binding = BoundDatabaseRoot {
            path: root.path().into(),
            _capability: open_root_nofollow(root.path()).unwrap(),
            identity: String::new(),
            profile_identity: String::new(),
            named_path: None,
            namespace_kind: BoundDatabaseRootNamespace::ExternalSource,
            _lease: None,
        };
        let held = binding.inspect_index_file(relative).unwrap();
        held.validate(&entry).unwrap();
        fs::rename(root.path().join("nested"), root.path().join("old")).unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::write(&path, b"facts").unwrap();
        assert_eq!(held.validate(&entry).unwrap_err(), E::ChangedEntry);
        fs::remove_dir_all(root.path().join("nested")).unwrap();
        fs::rename(root.path().join("old"), root.path().join("nested")).unwrap();
        held.validate(&entry).unwrap();
        fs::write(&path, b"changed facts").unwrap();
        assert_eq!(held.validate(&entry).unwrap_err(), E::ChangedEntry);
        for invalid in ["", "../escape", "/absolute", "./nested/child.txt"] {
            assert!(matches!(
                binding.inspect_index_file(Path::new(invalid)),
                Err(E::InvalidPath)
            ));
        }
        assert!(matches!(
            binding.inspect_index_file(Path::new(&"x".repeat(1025))),
            Err(E::ByteLimit)
        ));
        assert!(matches!(
            binding.inspect_index_file(Path::new(&vec!["x"; 33].join("/"))),
            Err(E::InvalidPath)
        ));
    }
}
