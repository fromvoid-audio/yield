//! On-disk usage reporting for the global database and rebuildable cache.

#[cfg(not(windows))]
use std::ffi::OsString;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(not(windows))]
use cap_fs_ext::DirExt;
use cap_primitives::{ambient_authority, fs as capability_fs};
use cap_std::fs::Dir;
#[cfg(not(windows))]
use cap_std::fs::ReadDir;
use thiserror::Error;

#[cfg(not(windows))]
use crate::filesystem_identity::stable_filesystem_identity_from_open_file;
use crate::sample_sources::LIBRARY_DB_FILE_NAME;

use super::{app_root_dir, profile_ownership::WritableProfileGuard};

#[cfg(windows)]
mod windows;

const CACHE_DIR_NAME: &str = "cache";
const SQLITE_SIDECAR_SUFFIXES: [&str; 3] = ["", "-wal", "-shm"];

/// Maximum number of cache entries visited by one bounded maintenance walk.
pub const MAX_MAINTENANCE_ENTRIES: usize = 100_000;
/// Maximum relative depth visited by one bounded maintenance walk.
pub const MAX_MAINTENANCE_DEPTH: usize = 64;
/// Maximum number of retained directory handles used by one maintenance walk.
pub const MAX_MAINTENANCE_OPEN_HANDLES: usize = 128;

/// Logical on-disk bytes owned by the global library database and cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GlobalStorageUsage {
    /// Bytes in `library.db` and its SQLite WAL/shared-memory sidecars.
    pub database_bytes: u64,
    /// Bytes in regular files below the rebuildable global cache root.
    pub cache_bytes: u64,
}

impl GlobalStorageUsage {
    /// Total bytes represented by the database and cache measurements.
    pub fn total_bytes(self) -> u64 {
        self.database_bytes.saturating_add(self.cache_bytes)
    }
}

/// The terminal counters from one rebuildable-cache cleanup attempt.
///
/// `removed_files` counts regular files and symbolic-link entries removed by the operation;
/// `removed_bytes` counts only regular-file payload bytes. Empty directories are not counted.
/// When cleanup is interrupted or encounters a namespace replacement, the counters describe
/// only work that was actually completed and `remaining_entries` is a bounded observation of the
/// captured namespace. A replaced current path is intentionally not included in that count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheClearOutcome {
    /// Number of regular-file or symbolic-link entries removed by the operation.
    pub removed_files: u64,
    /// Number of bytes removed from regular-file payloads.
    pub removed_bytes: u64,
    /// Number of entries left in the captured namespace when a terminal check was possible.
    pub remaining_entries: u64,
    /// Whether `remaining_entries` was obtained by a bounded terminal observation.
    pub remaining_entries_known: bool,
    /// Whether the operation stopped before proving the captured namespace empty.
    pub incomplete: bool,
    /// Whether the configured cache path no longer names the captured cache directory.
    pub namespace_replaced: bool,
}

impl Default for CacheClearOutcome {
    fn default() -> Self {
        Self {
            removed_files: 0,
            removed_bytes: 0,
            remaining_entries: 0,
            remaining_entries_known: false,
            incomplete: true,
            namespace_replaced: false,
        }
    }
}

/// Errors returned by a bounded profile-bound storage usage query.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum StorageUsageError {
    /// The caller cancelled the query before it could produce a complete answer.
    #[error("storage usage query cancelled")]
    Cancelled,
    /// The retained profile root or owner lock changed during the query.
    #[error("profile changed during storage usage query: {0}")]
    ProfileChanged(String),
    /// The bounded traversal could not safely cover the complete cache namespace.
    #[error("storage usage traversal exceeded {kind} limit {limit}")]
    BoundsExceeded {
        /// Kind of bound that was reached.
        kind: &'static str,
        /// Maximum permitted value.
        limit: usize,
    },
    /// A profile-bound filesystem operation failed.
    #[error("storage usage filesystem operation failed: {0}")]
    Io(String),
}

/// Errors returned by a profile-bound rebuildable-cache cleanup.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CacheMaintenanceError {
    /// The caller cancelled cleanup before the captured namespace was proven empty.
    #[error("cache cleanup cancelled")]
    Cancelled {
        /// Counters completed before cancellation.
        outcome: CacheClearOutcome,
    },
    /// The retained profile root or owner lock changed during cleanup.
    #[error("profile changed during cache cleanup: {reason}")]
    ProfileChanged {
        /// Counters completed before the profile check failed.
        outcome: CacheClearOutcome,
        /// Renderer-neutral profile failure detail.
        reason: String,
    },
    /// The configured cache path no longer names the captured cache directory.
    #[error("cache namespace was replaced: {reason}")]
    NamespaceReplaced {
        /// Counters completed against the captured directory identity.
        outcome: CacheClearOutcome,
        /// Renderer-neutral namespace replacement detail.
        reason: String,
    },
    /// Cleanup stopped with entries remaining in the captured namespace.
    #[error("cache cleanup is incomplete ({remaining_entries} entries remain)")]
    Incomplete {
        /// Bounded counters and remaining-entry observation.
        outcome: CacheClearOutcome,
        /// Number of remaining entries, duplicated for readable diagnostics.
        remaining_entries: u64,
    },
    /// Cleanup reached a bounded traversal limit.
    #[error("cache cleanup exceeded {kind} limit {limit}")]
    BoundsExceeded {
        /// Counters completed before the bound stopped cleanup.
        outcome: CacheClearOutcome,
        /// Kind of bound that was reached.
        kind: &'static str,
        /// Maximum permitted value.
        limit: usize,
    },
    /// A profile-bound filesystem operation failed after some entries may have been removed.
    #[error("cache cleanup filesystem operation failed at {path}: {message}")]
    Io {
        /// Counters completed before the filesystem failure.
        outcome: CacheClearOutcome,
        /// Display path associated with the operation.
        path: PathBuf,
        /// Renderer-neutral filesystem error detail.
        message: String,
    },
    /// The current platform does not expose a verified handle-bound cleanup primitive.
    #[error("cache cleanup is unsupported on this platform: {reason}")]
    Unsupported {
        /// Counters completed before the unsupported operation was rejected.
        outcome: CacheClearOutcome,
        /// Renderer-neutral platform limitation detail.
        reason: String,
    },
}

/// A captured profile-owned cache directory and its stable object identity.
///
/// The retained directory capability is the authority for cleanup. The display path is used only
/// for diagnostics and conflict checks; cleanup never reopens a replacement by that path.
#[derive(Debug)]
pub struct DisposableCacheNamespace {
    profile: WritableProfileGuard,
    profile_root: Dir,
    #[cfg(not(windows))]
    cache_root: Dir,
    #[cfg(windows)]
    cache_root: windows::CacheRoot,
    profile_path: PathBuf,
    cache_identity: String,
}

impl DisposableCacheNamespace {
    /// Return the profile path used for diagnostics and admission conflict checks.
    pub fn profile_path(&self) -> &Path {
        &self.profile_path
    }

    /// Return the configured cache path associated with this captured namespace.
    pub fn cache_path(&self) -> PathBuf {
        self.profile_path.join(CACHE_DIR_NAME)
    }

    /// Return whether a configured path aliases, contains, or is contained by the cache path.
    ///
    /// Existing symlink aliases are resolved for comparison only. This method never grants
    /// authority to, opens, or mutates the supplied path.
    pub fn conflicts_with_path(&self, path: &Path) -> bool {
        let cache_candidates = overlap_candidates(&self.cache_path());
        let path_candidates = overlap_candidates(path);
        cache_candidates.iter().any(|cache| {
            path_candidates
                .iter()
                .any(|candidate| paths_overlap(cache, candidate))
        })
    }

    /// Remove disposable entries from the captured namespace without blocking the caller.
    ///
    /// The operation is synchronous at this low-level boundary and is intended to run on the
    /// session maintenance worker. It recurses only through no-follow directory capabilities,
    /// leaves the captured cache root in place, and checks cancellation and profile ownership at
    /// every bounded step.
    pub fn clear(
        &self,
        is_cancelled: impl FnMut() -> bool,
    ) -> Result<CacheClearOutcome, CacheMaintenanceError> {
        #[cfg(windows)]
        {
            return windows::clear(self, is_cancelled);
        }

        #[cfg(not(windows))]
        {
            let mut is_cancelled = is_cancelled;
            let mut outcome = CacheClearOutcome {
                incomplete: true,
                ..CacheClearOutcome::default()
            };

            check_cancelled(&mut is_cancelled, &outcome)?;
            self.ensure_profile(&outcome)?;

            let root = self.cache_root.try_clone().map_err(|error| {
                cache_io(
                    &outcome,
                    self.cache_path(),
                    format!("clone cache root: {error}"),
                )
            })?;
            let entries = root.entries().map_err(|error| {
                cache_io(
                    &outcome,
                    self.cache_path(),
                    format!("read cache root: {error}"),
                )
            })?;
            let mut frames = vec![ClearFrame {
                directory: root,
                display_path: self.cache_path(),
                parent_entry: None,
                entries,
                depth: 0,
            }];
            let mut visited_entries = 0_usize;

            loop {
                check_cancelled(&mut is_cancelled, &outcome)?;
                self.ensure_profile(&outcome)?;
                let (next, display_path, depth) = {
                    let Some(frame) = frames.last_mut() else {
                        break;
                    };
                    (
                        frame.entries.next(),
                        frame.display_path.clone(),
                        frame.depth,
                    )
                };
                let Some(entry) = next else {
                    let finished = frames.pop().expect("frame retained above");
                    let Some(parent_entry) = finished.parent_entry else {
                        continue;
                    };
                    let Some(parent) = frames.last() else {
                        return Err(cache_namespace_replaced(
                            outcome,
                            String::from("captured cache parent disappeared"),
                        ));
                    };
                    let (name, identity, path) = parent_entry;
                    let current = match parent.directory.symlink_metadata(&name) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            return Err(cache_namespace_replaced(
                                outcome,
                                format!(
                                    "captured directory moved or disappeared: {}: {error}",
                                    path.display()
                                ),
                            ));
                        }
                        Err(error) => {
                            return Err(cache_io(
                                &outcome,
                                path,
                                format!("revalidate directory: {error}"),
                            ));
                        }
                    };
                    let current_identity =
                        cap_metadata_identity(&path, &current).ok_or_else(|| {
                            cache_io(
                                &outcome,
                                path.clone(),
                                String::from("directory identity unavailable during cleanup"),
                            )
                        })?;
                    if current_identity != identity {
                        return Err(cache_namespace_replaced(
                            outcome,
                            format!("captured directory was replaced at {}", path.display()),
                        ));
                    }
                    parent.directory.remove_dir(&name).map_err(|error| {
                        cache_io(&outcome, path, format!("remove empty directory: {error}"))
                    })?;
                    continue;
                };
                let entry = entry.map_err(|error| {
                    cache_io(
                        &outcome,
                        display_path.clone(),
                        format!("read directory entry: {error}"),
                    )
                })?;

                visited_entries = visited_entries.saturating_add(1);
                if visited_entries > MAX_MAINTENANCE_ENTRIES {
                    outcome.incomplete = true;
                    return Err(CacheMaintenanceError::BoundsExceeded {
                        outcome,
                        kind: "entries",
                        limit: MAX_MAINTENANCE_ENTRIES,
                    });
                }
                let name = entry.file_name();
                let path = display_path.join(&name);
                let metadata = frames
                    .last()
                    .expect("frame retained above")
                    .directory
                    .symlink_metadata(&name)
                    .map_err(|error| {
                        cache_io(
                            &outcome,
                            path.clone(),
                            format!("read entry metadata: {error}"),
                        )
                    })?;
                let file_type = metadata.file_type();
                let identity = cap_metadata_identity(&path, &metadata).ok_or_else(|| {
                    CacheMaintenanceError::Unsupported {
                        outcome,
                        reason: format!("stable identity unavailable for {}", path.display()),
                    }
                })?;
                if file_type.is_dir() {
                    if depth.saturating_add(1) > MAX_MAINTENANCE_DEPTH {
                        outcome.incomplete = true;
                        return Err(CacheMaintenanceError::BoundsExceeded {
                            outcome,
                            kind: "depth",
                            limit: MAX_MAINTENANCE_DEPTH,
                        });
                    }
                    if frames.len() >= MAX_MAINTENANCE_OPEN_HANDLES {
                        outcome.incomplete = true;
                        return Err(CacheMaintenanceError::BoundsExceeded {
                            outcome,
                            kind: "open handles",
                            limit: MAX_MAINTENANCE_OPEN_HANDLES,
                        });
                    }
                    let child = {
                        let frame = frames.last().expect("frame retained above");
                        frame.directory.open_dir_nofollow(&name).map_err(|error| {
                            if entry_changed_before_open(&error) {
                                cache_namespace_replaced(
                                    outcome,
                                    format!("directory changed before open: {}", path.display()),
                                )
                            } else {
                                cache_io(&outcome, path.clone(), format!("open directory: {error}"))
                            }
                        })?
                    };
                    let child_identity = identity_from_dir(&child).ok_or_else(|| {
                        CacheMaintenanceError::Unsupported {
                            outcome,
                            reason: format!("stable identity unavailable for {}", path.display()),
                        }
                    })?;
                    if child_identity != identity {
                        return Err(cache_namespace_replaced(
                            outcome,
                            format!("directory changed while opening: {}", path.display()),
                        ));
                    }
                    let child_entries = child.entries().map_err(|error| {
                        cache_io(&outcome, path.clone(), format!("read directory: {error}"))
                    })?;
                    frames.push(ClearFrame {
                        directory: child,
                        display_path: path.clone(),
                        parent_entry: Some((name, identity, path)),
                        entries: child_entries,
                        depth: depth.saturating_add(1),
                    });
                    continue;
                }
                if !file_type.is_file() && !file_type.is_symlink() {
                    return Err(CacheMaintenanceError::Unsupported {
                        outcome,
                        reason: format!("unexpected non-regular cache entry at {}", path.display()),
                    });
                }

                let current = match frames
                    .last()
                    .expect("frame retained above")
                    .directory
                    .symlink_metadata(&name)
                {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(cache_io(
                            &outcome,
                            path,
                            format!("revalidate entry: {error}"),
                        ));
                    }
                };
                let current_identity = cap_metadata_identity(&path, &current).ok_or_else(|| {
                    CacheMaintenanceError::Unsupported {
                        outcome,
                        reason: format!("stable identity unavailable for {}", path.display()),
                    }
                })?;
                if current_identity != identity {
                    return Err(cache_namespace_replaced(
                        outcome,
                        format!("entry was replaced at {}", path.display()),
                    ));
                }
                frames
                    .last()
                    .expect("frame retained above")
                    .directory
                    .remove_file(&name)
                    .map_err(|error| {
                        cache_io(&outcome, path.clone(), format!("remove entry: {error}"))
                    })?;
                outcome.removed_files = outcome.removed_files.saturating_add(1);
                if current.file_type().is_file() {
                    outcome.removed_bytes = outcome.removed_bytes.saturating_add(current.len());
                }
            }

            check_cancelled(&mut is_cancelled, &outcome)?;
            self.ensure_profile(&outcome)?;
            let current_cache = match self
                .profile_root
                .open_dir_nofollow(Path::new(CACHE_DIR_NAME))
            {
                Ok(directory) => directory,
                Err(error) if entry_changed_before_open(&error) => {
                    return Err(cache_namespace_replaced(
                        outcome,
                        format!("cache path changed: {error}"),
                    ));
                }
                Err(error) => {
                    return Err(cache_io(
                        &outcome,
                        self.cache_path(),
                        format!("reopen cache path for identity check: {error}"),
                    ));
                }
            };
            let current_identity = identity_from_dir(&current_cache).ok_or_else(|| {
                CacheMaintenanceError::Unsupported {
                    outcome,
                    reason: String::from("stable cache identity unavailable during final check"),
                }
            })?;
            if current_identity != self.cache_identity {
                return Err(cache_namespace_replaced(
                    outcome,
                    String::from("cache path names a replacement directory"),
                ));
            }

            outcome.remaining_entries = count_entries(
                &self.profile,
                self.cache_root.try_clone().map_err(|error| {
                    cache_io(
                        &outcome,
                        self.cache_path(),
                        format!("clone cache root for final check: {error}"),
                    )
                })?,
                self.cache_path(),
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
    }

    pub(crate) fn capture(
        profile: &WritableProfileGuard,
    ) -> Result<Self, super::ProfileOwnershipError> {
        let profile = profile.try_clone()?;
        let profile_path = profile.profile_root().to_path_buf();
        let profile_root = profile.profile_root_dir()?;
        #[cfg(windows)]
        let cache_root = match windows::open_cache_root(&profile_root) {
            Ok(cache_root) => cache_root,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                profile.open_child_dir(Path::new(CACHE_DIR_NAME))?;
                windows::open_cache_root(&profile_root).map_err(|source| {
                    super::ProfileOwnershipError::Io {
                        path: profile_path.join(CACHE_DIR_NAME),
                        source,
                    }
                })?
            }
            Err(source) => {
                return Err(super::ProfileOwnershipError::Io {
                    path: profile_path.join(CACHE_DIR_NAME),
                    source,
                });
            }
        };
        #[cfg(not(windows))]
        let cache_root = match profile_root.open_dir_nofollow(Path::new(CACHE_DIR_NAME)) {
            Ok(cache_root) => cache_root,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                profile.open_child_dir(Path::new(CACHE_DIR_NAME))?
            }
            Err(error) => {
                return Err(super::ProfileOwnershipError::Io {
                    path: profile_path.join(CACHE_DIR_NAME),
                    source: error,
                });
            }
        };
        #[cfg(windows)]
        let cache_identity = windows::identity_from_root(&cache_root).ok_or_else(|| {
            super::ProfileOwnershipError::IdentityUnavailable {
                path: profile_path.join(CACHE_DIR_NAME),
            }
        })?;
        #[cfg(not(windows))]
        let cache_identity = identity_from_dir(&cache_root).ok_or_else(|| {
            super::ProfileOwnershipError::IdentityUnavailable {
                path: profile_path.join(CACHE_DIR_NAME),
            }
        })?;
        Ok(Self {
            profile,
            profile_root,
            cache_root,
            profile_path,
            cache_identity,
        })
    }

    fn ensure_profile(&self, outcome: &CacheClearOutcome) -> Result<(), CacheMaintenanceError> {
        self.profile
            .validate_current()
            .map_err(|error| CacheMaintenanceError::ProfileChanged {
                outcome: *outcome,
                reason: error.to_string(),
            })
    }
}

#[cfg(not(windows))]
struct ClearFrame {
    directory: Dir,
    display_path: PathBuf,
    parent_entry: Option<(OsString, String, PathBuf)>,
    entries: ReadDir,
    depth: usize,
}

fn check_cancelled(
    is_cancelled: &mut impl FnMut() -> bool,
    outcome: &CacheClearOutcome,
) -> Result<(), CacheMaintenanceError> {
    if is_cancelled() {
        return Err(CacheMaintenanceError::Cancelled { outcome: *outcome });
    }
    Ok(())
}

fn cache_io(outcome: &CacheClearOutcome, path: PathBuf, message: String) -> CacheMaintenanceError {
    CacheMaintenanceError::Io {
        outcome: *outcome,
        path,
        message,
    }
}

fn cache_namespace_replaced(
    mut outcome: CacheClearOutcome,
    reason: String,
) -> CacheMaintenanceError {
    outcome.incomplete = true;
    outcome.namespace_replaced = true;
    CacheMaintenanceError::NamespaceReplaced { outcome, reason }
}

#[cfg(not(windows))]
fn identity_from_dir(dir: &Dir) -> Option<String> {
    let file = dir.try_clone().ok()?.into_std_file();
    stable_filesystem_identity_from_open_file(&file)
}

#[cfg(not(windows))]
fn cap_metadata_identity(_path: &Path, metadata: &cap_std::fs::Metadata) -> Option<String> {
    #[cfg(unix)]
    {
        use cap_primitives::fs::MetadataExt;
        let created = metadata
            .created()
            .ok()?
            .into_std()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        return Some(format!(
            "unix:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            created.as_nanos()
        ));
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

#[cfg(not(windows))]
fn count_entries(
    profile: &WritableProfileGuard,
    root: Dir,
    display_path: PathBuf,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Result<u64, CountError> {
    let entries = root
        .entries()
        .map_err(|error| CountError::Io(display_path.clone(), error.to_string()))?;
    let mut stack = vec![(root, display_path, entries, 0_usize)];
    let mut count = 0_u64;
    while let Some((directory, path, mut entries, depth)) = stack.pop() {
        if is_cancelled() {
            return Err(CountError::Cancelled);
        }
        profile
            .validate_current()
            .map_err(|error| CountError::Profile(error.to_string()))?;
        while let Some(entry) = entries.next() {
            if is_cancelled() {
                return Err(CountError::Cancelled);
            }
            profile
                .validate_current()
                .map_err(|error| CountError::Profile(error.to_string()))?;
            let entry = entry.map_err(|error| CountError::Io(path.clone(), error.to_string()))?;
            count = count.saturating_add(1);
            if count > MAX_MAINTENANCE_ENTRIES as u64 {
                return Err(CountError::Bounds("entries", MAX_MAINTENANCE_ENTRIES));
            }
            let name = entry.file_name();
            if entry
                .file_type()
                .map_err(|error| CountError::Io(path.join(&name), error.to_string()))?
                .is_dir()
            {
                if depth.saturating_add(1) > MAX_MAINTENANCE_DEPTH {
                    return Err(CountError::Bounds("depth", MAX_MAINTENANCE_DEPTH));
                }
                if stack.len() >= MAX_MAINTENANCE_OPEN_HANDLES {
                    return Err(CountError::Bounds(
                        "open handles",
                        MAX_MAINTENANCE_OPEN_HANDLES,
                    ));
                }
                let child = directory
                    .open_dir_nofollow(&name)
                    .map_err(|error| CountError::Io(path.join(&name), error.to_string()))?;
                let child_path = path.join(&name);
                let child_entries = child
                    .entries()
                    .map_err(|error| CountError::Io(child_path.clone(), error.to_string()))?;
                stack.push((directory, path, entries, depth));
                stack.push((child, child_path, child_entries, depth.saturating_add(1)));
                break;
            }
        }
    }
    Ok(count)
}

#[cfg(not(windows))]
enum CountError {
    Cancelled,
    Profile(String),
    Bounds(&'static str, usize),
    Io(PathBuf, String),
}

#[cfg(not(windows))]
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
        CountError::Io(path, message) => cache_io(outcome, path, message),
    }
}

fn overlap_candidates(path: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![lexically_normalized(path)];
    if let Some(canonical) = canonicalized_existing_prefix(path) {
        if !candidates.iter().any(|candidate| candidate == &canonical) {
            candidates.push(canonical);
        }
    }
    candidates
}

fn lexically_normalized(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn canonicalized_existing_prefix(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = fs::canonicalize(path) {
        return Some(canonical);
    }
    let mut existing = path.to_path_buf();
    let mut suffix = Vec::new();
    while fs::symlink_metadata(&existing).is_err() {
        let name = existing.file_name()?.to_os_string();
        suffix.push(name);
        existing.pop();
    }
    let mut canonical = fs::canonicalize(existing).ok()?;
    for component in suffix.iter().rev() {
        canonical.push(component);
    }
    Some(canonical)
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

/// Measure the current global library database and rebuildable cache footprint.
///
/// The traversal does not follow symbolic links, so a linked cache root or
/// descendant cannot make the reported size escape the app-owned boundary.
pub fn global_storage_usage() -> Result<GlobalStorageUsage, String> {
    let root = app_root_dir().map_err(|error| error.to_string())?;
    global_storage_usage_at(&root)
}

/// Measure global storage through a retained writable-profile capability.
///
/// The callback is polled between bounded filesystem steps. A cancelled query returns an error
/// rather than a partial byte count. The profile guard remains borrowed only during admission;
/// callers that move the work to another thread must clone the guard first.
pub fn global_storage_usage_for_profile(
    profile: &WritableProfileGuard,
    mut is_cancelled: impl FnMut() -> bool,
) -> Result<GlobalStorageUsage, StorageUsageError> {
    if is_cancelled() {
        return Err(StorageUsageError::Cancelled);
    }
    profile
        .validate_current()
        .map_err(|error| StorageUsageError::ProfileChanged(error.to_string()))?;
    let root = profile
        .profile_root_dir()
        .map_err(|error| StorageUsageError::ProfileChanged(error.to_string()))?;

    let mut database_bytes = 0_u64;
    for suffix in SQLITE_SIDECAR_SUFFIXES {
        if is_cancelled() {
            return Err(StorageUsageError::Cancelled);
        }
        profile
            .validate_current()
            .map_err(|error| StorageUsageError::ProfileChanged(error.to_string()))?;
        let path = PathBuf::from(format!("{LIBRARY_DB_FILE_NAME}{suffix}"));
        let metadata = match root.symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(StorageUsageError::Io(format!(
                    "failed to read {}: {error}",
                    profile.profile_root().join(&path).display()
                )));
            }
        };
        if metadata.file_type().is_file() {
            database_bytes = database_bytes.checked_add(metadata.len()).ok_or(
                StorageUsageError::BoundsExceeded {
                    kind: "bytes",
                    limit: usize::MAX,
                },
            )?;
        }
    }

    #[cfg(windows)]
    {
        let cache_root = match windows::open_cache_root(&root) {
            Ok(cache_root) => cache_root,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                profile
                    .validate_current()
                    .map_err(|error| StorageUsageError::ProfileChanged(error.to_string()))?;
                return Ok(GlobalStorageUsage {
                    database_bytes,
                    cache_bytes: 0,
                });
            }
            Err(error) => {
                return Err(StorageUsageError::Io(format!(
                    "failed to open {}: {error}",
                    profile.profile_root().join(CACHE_DIR_NAME).display()
                )));
            }
        };
        let cache_bytes = windows::bounded_directory_size(
            profile,
            cache_root,
            profile.profile_root().join(CACHE_DIR_NAME),
            &mut is_cancelled,
        )?;
        profile
            .validate_current()
            .map_err(|error| StorageUsageError::ProfileChanged(error.to_string()))?;
        return Ok(GlobalStorageUsage {
            database_bytes,
            cache_bytes,
        });
    }

    #[cfg(not(windows))]
    {
        let cache_root = match root.open_dir_nofollow(Path::new(CACHE_DIR_NAME)) {
            Ok(cache_root) => cache_root,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                profile
                    .validate_current()
                    .map_err(|error| StorageUsageError::ProfileChanged(error.to_string()))?;
                return Ok(GlobalStorageUsage {
                    database_bytes,
                    cache_bytes: 0,
                });
            }
            Err(error) if entry_changed_before_open(&error) => {
                return Err(StorageUsageError::Io(format!(
                    "cache root changed or is a symbolic link at {}: {error}",
                    profile.profile_root().join(CACHE_DIR_NAME).display()
                )));
            }
            Err(error) => {
                return Err(StorageUsageError::Io(format!(
                    "failed to open {}: {error}",
                    profile.profile_root().join(CACHE_DIR_NAME).display()
                )));
            }
        };
        let cache_bytes = bounded_directory_size(
            profile,
            cache_root,
            profile.profile_root().join(CACHE_DIR_NAME),
            &mut is_cancelled,
        )?;
        profile
            .validate_current()
            .map_err(|error| StorageUsageError::ProfileChanged(error.to_string()))?;
        Ok(GlobalStorageUsage {
            database_bytes,
            cache_bytes,
        })
    }
}

#[cfg(not(windows))]
struct StorageFrame {
    directory: Dir,
    display_path: PathBuf,
    entries: ReadDir,
    depth: usize,
}

#[cfg(not(windows))]
fn bounded_directory_size(
    profile: &WritableProfileGuard,
    root: Dir,
    display_path: PathBuf,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Result<u64, StorageUsageError> {
    let entries = root.entries().map_err(|error| {
        StorageUsageError::Io(format!(
            "failed to read {}: {error}",
            display_path.display()
        ))
    })?;
    let mut stack = vec![StorageFrame {
        directory: root,
        display_path,
        entries,
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
        let handle_count = stack.len();
        let frame = stack.last_mut().expect("stack checked above");
        let Some(entry) = frame.entries.next() else {
            stack.pop();
            continue;
        };
        let entry = entry.map_err(|error| {
            StorageUsageError::Io(format!(
                "failed to read directory {}: {error}",
                frame.display_path.display()
            ))
        })?;
        visited_entries = visited_entries.saturating_add(1);
        if visited_entries > MAX_MAINTENANCE_ENTRIES {
            return Err(StorageUsageError::BoundsExceeded {
                kind: "entries",
                limit: MAX_MAINTENANCE_ENTRIES,
            });
        }
        let name = entry.file_name();
        let path = frame.display_path.join(&name);
        let file_type = entry.file_type().map_err(|error| {
            StorageUsageError::Io(format!("failed to classify {}: {error}", path.display()))
        })?;
        if file_type.is_dir() {
            if frame.depth.saturating_add(1) > MAX_MAINTENANCE_DEPTH {
                return Err(StorageUsageError::BoundsExceeded {
                    kind: "depth",
                    limit: MAX_MAINTENANCE_DEPTH,
                });
            }
            if handle_count >= MAX_MAINTENANCE_OPEN_HANDLES {
                return Err(StorageUsageError::BoundsExceeded {
                    kind: "open handles",
                    limit: MAX_MAINTENANCE_OPEN_HANDLES,
                });
            }
            let child = match frame.directory.open_dir_nofollow(&name) {
                Ok(child) => child,
                Err(error) if entry_changed_before_open(&error) => {
                    return Err(StorageUsageError::Io(format!(
                        "cache directory changed before opening {}: {error}",
                        path.display()
                    )));
                }
                Err(error) => {
                    return Err(StorageUsageError::Io(format!(
                        "failed to open {}: {error}",
                        path.display()
                    )));
                }
            };
            let child_entries = child.entries().map_err(|error| {
                StorageUsageError::Io(format!("failed to read {}: {error}", path.display()))
            })?;
            let depth = frame.depth.saturating_add(1);
            stack.push(StorageFrame {
                directory: child,
                display_path: path,
                entries: child_entries,
                depth,
            });
        } else if file_type.is_file() {
            let metadata = match frame.directory.symlink_metadata(&name) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(StorageUsageError::Io(format!(
                        "cache entry disappeared before reading {}: {error}",
                        path.display()
                    )));
                }
                Err(error) => {
                    return Err(StorageUsageError::Io(format!(
                        "failed to read metadata for {}: {error}",
                        path.display()
                    )));
                }
            };
            if metadata.file_type().is_file() {
                total =
                    total
                        .checked_add(metadata.len())
                        .ok_or(StorageUsageError::BoundsExceeded {
                            kind: "bytes",
                            limit: usize::MAX,
                        })?;
            }
        }
    }
    Ok(total)
}

fn global_storage_usage_at(root: &Path) -> Result<GlobalStorageUsage, String> {
    let database_bytes = SQLITE_SIDECAR_SUFFIXES
        .into_iter()
        .try_fold(0_u64, |total, suffix| {
            let path = root.join(format!("{LIBRARY_DB_FILE_NAME}{suffix}"));
            checked_add(total, regular_file_size(&path)?, &path)
        })?;
    let cache_root = root.join(CACHE_DIR_NAME);
    let cache_bytes = directory_regular_file_size(&cache_root)?;
    Ok(GlobalStorageUsage {
        database_bytes,
        cache_bytes,
    })
}

fn directory_regular_file_size(root: &Path) -> Result<u64, String> {
    directory_regular_file_size_with_observer(root, |_| {})
}

fn directory_regular_file_size_with_observer(
    root: &Path,
    mut before_descend: impl FnMut(&Path),
) -> Result<u64, String> {
    let root_metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(read_error("metadata", root, error)),
    };
    if !root_metadata.file_type().is_dir() {
        return Ok(0);
    }

    let parent = root
        .parent()
        .ok_or_else(|| format!("Global cache root has no parent: {}", root.display()))?;
    let root_name = root
        .file_name()
        .ok_or_else(|| format!("Global cache root has no file name: {}", root.display()))?;
    let parent_directory = capability_fs::open_ambient_dir(parent, ambient_authority())
        .map_err(|error| read_error("cache parent directory", parent, error))?;
    let root_directory =
        match capability_fs::open_dir_nofollow(&parent_directory, Path::new(root_name)) {
            Ok(directory) => directory,
            Err(error) if entry_changed_before_open(&error) => return Ok(0),
            Err(error) => return Err(read_error("cache directory", root, error)),
        };

    let mut total = 0_u64;
    let mut pending = vec![(root_directory, root.to_path_buf())];
    while let Some((directory, display_path)) = pending.pop() {
        let entries = match capability_fs::read_base_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(read_error("directory", &display_path, error)),
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(read_error("directory entry", &display_path, error)),
            };
            let path = display_path.join(entry.file_name());
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(read_error("file type", &path, error)),
            };
            if file_type.is_dir() {
                before_descend(&path);
                match capability_fs::open_dir_nofollow(&directory, Path::new(&entry.file_name())) {
                    Ok(child) => pending.push((child, path)),
                    Err(error) if entry_changed_before_open(&error) => continue,
                    Err(error) => return Err(read_error("directory", &path, error)),
                }
            } else if file_type.is_file() {
                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(read_error("metadata", &path, error)),
                };
                if metadata.file_type().is_file() {
                    total = checked_add(total, metadata.len(), &path)?;
                }
            }
        }
    }
    Ok(total)
}

fn entry_changed_before_open(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    ) {
        return true;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return true;
    }
    #[cfg(target_os = "windows")]
    if error.raw_os_error() == Some(::windows::Win32::Foundation::ERROR_STOPPED_ON_SYMLINK.0 as i32)
    {
        return true;
    }
    false
}

fn regular_file_size(path: &Path) -> Result<u64, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(metadata.len()),
        Ok(_) => Ok(0),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(read_error("metadata", path, error)),
    }
}

fn checked_add(total: u64, bytes: u64, path: &Path) -> Result<u64, String> {
    total.checked_add(bytes).ok_or_else(|| {
        format!(
            "Global storage size overflow while measuring {}",
            path.display()
        )
    })
}

fn read_error(kind: &str, path: &Path, error: io::Error) -> String {
    format!("Failed to read {kind} at {}: {error}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_database_sidecars_and_nested_cache_only() {
        let root = tempfile::tempdir().expect("create storage root");
        fs::write(root.path().join(LIBRARY_DB_FILE_NAME), [0_u8; 7]).expect("write database");
        fs::write(root.path().join("library.db-wal"), [0_u8; 5]).expect("write database WAL");
        fs::write(root.path().join("library.db-shm"), [0_u8; 3])
            .expect("write database shared memory");
        fs::write(root.path().join("config.toml"), [0_u8; 19]).expect("write unrelated config");
        let cache = root.path().join(CACHE_DIR_NAME).join("waveforms");
        fs::create_dir_all(&cache).expect("create cache tree");
        fs::write(cache.join("one.cache"), [0_u8; 11]).expect("write first cache payload");
        fs::write(cache.join("two.cache"), [0_u8; 13]).expect("write second cache payload");

        let usage = global_storage_usage_at(root.path()).expect("measure global storage");

        assert_eq!(usage.database_bytes, 15);
        assert_eq!(usage.cache_bytes, 24);
        assert_eq!(usage.total_bytes(), 39);
    }

    #[test]
    fn missing_database_and_cache_report_zero_bytes() {
        let root = tempfile::tempdir().expect("create empty storage root");

        assert_eq!(
            global_storage_usage_at(root.path()).expect("measure empty global storage"),
            GlobalStorageUsage::default()
        );
    }

    #[cfg(unix)]
    #[test]
    fn cache_traversal_does_not_follow_symbolic_links() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("create storage root");
        let external = tempfile::tempdir().expect("create external root");
        fs::write(external.path().join("large.cache"), [0_u8; 61]).expect("write external payload");
        let cache = root.path().join(CACHE_DIR_NAME);
        fs::create_dir_all(&cache).expect("create cache root");
        fs::write(cache.join("owned.cache"), [0_u8; 5]).expect("write owned payload");
        symlink(external.path(), cache.join("external")).expect("link external directory");

        let usage = global_storage_usage_at(root.path()).expect("measure linked cache");

        assert_eq!(usage.cache_bytes, 5);
    }

    #[cfg(unix)]
    #[test]
    fn cache_traversal_does_not_follow_symbolic_cache_root() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("create storage root");
        let external = tempfile::tempdir().expect("create external cache root");
        fs::write(external.path().join("external.cache"), [0_u8; 61])
            .expect("write external payload");
        symlink(external.path(), root.path().join(CACHE_DIR_NAME)).expect("link cache root");

        let usage = global_storage_usage_at(root.path()).expect("measure linked cache root");

        assert_eq!(usage.cache_bytes, 0);
    }

    #[cfg(unix)]
    #[test]
    fn cache_traversal_does_not_follow_directory_replaced_by_symbolic_link() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("create storage root");
        let external = tempfile::tempdir().expect("create external cache root");
        fs::write(external.path().join("external.cache"), [0_u8; 61])
            .expect("write external payload");
        let cache = root.path().join(CACHE_DIR_NAME);
        let replaceable = cache.join("replaceable");
        fs::create_dir_all(&replaceable).expect("create replaceable cache directory");
        fs::write(replaceable.join("owned.cache"), [0_u8; 5]).expect("write owned payload");
        let detached = root.path().join("detached-cache");
        let mut replaced = false;

        let cache_bytes = directory_regular_file_size_with_observer(&cache, |path| {
            if path == replaceable && !replaced {
                fs::rename(&replaceable, &detached).expect("detach enumerated directory");
                symlink(external.path(), &replaceable).expect("replace directory with symlink");
                replaced = true;
            }
        })
        .expect("measure cache during directory replacement");

        assert!(replaced, "replacement hook should observe the directory");
        assert_eq!(cache_bytes, 0);
    }

    #[cfg(unix)]
    #[test]
    fn captured_cache_clear_preserves_siblings_and_external_symlink_targets() {
        use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
        use crate::test_runtime::TestRuntimeGuard;
        use std::os::unix::fs::symlink;

        let _runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir_in(
            fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root"),
        )
        .expect("create profile base");
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("maintenance-clear");
        let profile = WritableProfileGuard::acquire_current().expect("profile ownership");
        let cache = profile.profile_root().join(CACHE_DIR_NAME);
        fs::create_dir_all(cache.join("nested")).expect("cache tree");
        fs::write(cache.join("nested/payload.cache"), [0_u8; 11]).expect("cache payload");
        let external = tempfile::tempdir().expect("external root");
        fs::write(external.path().join("external.cache"), [0_u8; 97]).expect("external payload");
        symlink(external.path(), cache.join("external")).expect("external symlink");
        fs::create_dir_all(profile.profile_root().join("logs")).expect("sibling directory");
        fs::write(profile.profile_root().join("logs/keep.log"), [0_u8; 7])
            .expect("sibling payload");

        let namespace = profile
            .capture_disposable_cache_namespace()
            .expect("capture cache namespace");
        let outcome = namespace.clear(|| false).expect("clear captured cache");

        assert_eq!(outcome.removed_files, 2);
        assert_eq!(outcome.removed_bytes, 11);
        assert!(outcome.remaining_entries_known);
        assert_eq!(outcome.remaining_entries, 0);
        assert!(!outcome.incomplete);
        assert!(cache.is_dir());
        assert!(!cache.join("nested").exists());
        assert!(!cache.join("external").exists());
        assert_eq!(
            fs::read(external.path().join("external.cache"))
                .unwrap()
                .len(),
            97
        );
        assert_eq!(
            fs::read(profile.profile_root().join("logs/keep.log"))
                .unwrap()
                .len(),
            7
        );
    }

    #[cfg(unix)]
    #[test]
    fn captured_cache_clear_reports_root_replacement_without_touching_replacement() {
        use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
        use crate::test_runtime::TestRuntimeGuard;

        let _runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir_in(
            fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root"),
        )
        .expect("create profile base");
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("maintenance-root-replacement");
        let profile = WritableProfileGuard::acquire_current().expect("profile ownership");
        let cache = profile.profile_root().join(CACHE_DIR_NAME);
        fs::create_dir_all(&cache).expect("cache root");
        fs::write(cache.join("captured.cache"), [0_u8; 19]).expect("captured payload");

        let namespace = profile
            .capture_disposable_cache_namespace()
            .expect("capture cache namespace");
        let relocated = profile.profile_root().join("cache-relocated");
        fs::rename(&cache, &relocated).expect("relocate captured cache");
        fs::create_dir_all(&cache).expect("replacement cache root");
        fs::write(cache.join("replacement.cache"), [0_u8; 23]).expect("replacement payload");

        let error = namespace
            .clear(|| false)
            .expect_err("replacement must fail closed");
        let CacheMaintenanceError::NamespaceReplaced { outcome, .. } = error else {
            panic!("expected namespace replacement");
        };
        assert!(outcome.namespace_replaced);
        assert!(outcome.incomplete);
        assert_eq!(outcome.removed_files, 1);
        assert_eq!(outcome.removed_bytes, 19);
        assert!(!outcome.remaining_entries_known);
        assert_eq!(fs::read(cache.join("replacement.cache")).unwrap().len(), 23);
        assert!(
            relocated.is_dir(),
            "captured root remains retained after relocation"
        );
        assert!(!relocated.join("captured.cache").exists());
    }

    #[cfg(unix)]
    #[test]
    fn captured_cache_clear_stops_after_profile_root_replacement() {
        use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
        use crate::test_runtime::TestRuntimeGuard;

        let _runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir_in(
            fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root"),
        )
        .expect("create profile base");
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("maintenance-profile-replacement");
        let profile = WritableProfileGuard::acquire_current().expect("profile ownership");
        let profile_path = profile.profile_root().to_path_buf();
        let cache = profile_path.join(CACHE_DIR_NAME);
        fs::create_dir_all(&cache).expect("cache root");
        fs::write(cache.join("first.cache"), [0_u8; 53]).expect("first payload");
        fs::write(cache.join("second.cache"), [0_u8; 59]).expect("second payload");
        let namespace = profile
            .capture_disposable_cache_namespace()
            .expect("capture cache namespace");
        let displaced = profile_path.with_file_name("gpui-development-displaced");

        let mut checks = 0_u32;
        let error = namespace
            .clear(|| {
                checks = checks.saturating_add(1);
                if checks == 3 {
                    fs::rename(&profile_path, &displaced).expect("displace profile root");
                    let replacement_cache = profile_path.join(CACHE_DIR_NAME);
                    fs::create_dir_all(&replacement_cache).expect("replacement cache root");
                    fs::write(replacement_cache.join("replacement.cache"), [0_u8; 61])
                        .expect("replacement payload");
                }
                false
            })
            .expect_err("profile replacement must stop cleanup");
        let CacheMaintenanceError::ProfileChanged { outcome, .. } = error else {
            panic!("expected profile replacement");
        };
        assert_eq!(outcome.removed_files, 1);
        assert!(outcome.incomplete);
        assert!(!outcome.remaining_entries_known);
        assert!(
            profile_path
                .join(CACHE_DIR_NAME)
                .join("replacement.cache")
                .is_file()
        );
        assert_eq!(
            fs::read_dir(displaced.join(CACHE_DIR_NAME))
                .expect("displaced cache")
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn captured_cache_clear_reports_nested_relocation_without_removing_relocated_directory() {
        use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
        use crate::test_runtime::TestRuntimeGuard;

        let _runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir_in(
            fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root"),
        )
        .expect("create profile base");
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("maintenance-nested-relocation");
        let profile = WritableProfileGuard::acquire_current().expect("profile ownership");
        let cache = profile.profile_root().join(CACHE_DIR_NAME);
        let nested = cache.join("nested");
        fs::create_dir_all(&nested).expect("nested cache directory");
        fs::write(nested.join("captured.cache"), [0_u8; 29]).expect("captured payload");
        let relocated = profile.profile_root().join("nested-relocated");
        let namespace = profile
            .capture_disposable_cache_namespace()
            .expect("capture cache namespace");

        let mut checks = 0_u32;
        let error = namespace
            .clear(|| {
                checks = checks.saturating_add(1);
                if checks == 3 {
                    fs::rename(&nested, &relocated).expect("relocate captured child");
                    fs::create_dir_all(&nested).expect("replacement child");
                    fs::write(nested.join("replacement.cache"), [0_u8; 41])
                        .expect("replacement payload");
                }
                false
            })
            .expect_err("nested relocation must fail closed");
        let CacheMaintenanceError::NamespaceReplaced { outcome, .. } = error else {
            panic!("expected nested namespace replacement");
        };
        assert!(outcome.namespace_replaced);
        assert!(outcome.incomplete);
        assert_eq!(outcome.removed_files, 1);
        assert_eq!(outcome.removed_bytes, 29);
        assert!(!outcome.remaining_entries_known);
        assert!(
            relocated.is_dir(),
            "relocated directory remains user-visible"
        );
        assert!(!relocated.join("captured.cache").exists());
        assert_eq!(
            fs::read(nested.join("replacement.cache")).unwrap().len(),
            41
        );
        assert!(cache.is_dir(), "captured cache root remains in place");
    }

    #[cfg(unix)]
    #[test]
    fn captured_cache_clear_cancellation_reports_only_completed_entries() {
        use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
        use crate::test_runtime::TestRuntimeGuard;

        let _runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir_in(
            fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root"),
        )
        .expect("create profile base");
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("maintenance-cancellation");
        let profile = WritableProfileGuard::acquire_current().expect("profile ownership");
        let cache = profile.profile_root().join(CACHE_DIR_NAME);
        fs::create_dir_all(&cache).expect("cache root");
        fs::write(cache.join("first.cache"), [0_u8; 31]).expect("first payload");
        fs::write(cache.join("second.cache"), [0_u8; 37]).expect("second payload");
        let namespace = profile
            .capture_disposable_cache_namespace()
            .expect("capture cache namespace");

        let mut checks = 0_u32;
        let error = namespace
            .clear(|| {
                checks = checks.saturating_add(1);
                checks > 2
            })
            .expect_err("cancellation must stop cleanup");
        let CacheMaintenanceError::Cancelled { outcome } = error else {
            panic!("expected cancellation");
        };
        assert_eq!(outcome.removed_files, 1);
        assert!(outcome.removed_bytes == 31 || outcome.removed_bytes == 37);
        assert!(outcome.incomplete);
        assert!(!outcome.remaining_entries_known);
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn captured_cache_clear_rejects_special_entries_without_mutation() {
        use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
        use crate::test_runtime::TestRuntimeGuard;
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let _runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir_in(
            fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root"),
        )
        .expect("create profile base");
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("maintenance-special-entry");
        let profile = WritableProfileGuard::acquire_current().expect("profile ownership");
        let cache = profile.profile_root().join(CACHE_DIR_NAME);
        fs::create_dir_all(&cache).expect("cache root");
        let fifo_path = cache.join("cache.fifo");
        let fifo = CString::new(fifo_path.as_os_str().as_bytes()).expect("FIFO path");
        assert_eq!(
            unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) },
            0,
            "cache FIFO"
        );
        let namespace = profile
            .capture_disposable_cache_namespace()
            .expect("capture cache namespace");

        let error = namespace
            .clear(|| false)
            .expect_err("special entry must fail closed");
        let CacheMaintenanceError::Unsupported { outcome, reason } = error else {
            panic!("expected unsupported special entry");
        };
        assert!(reason.contains("non-regular"));
        assert_eq!(outcome.removed_files, 0);
        assert!(outcome.incomplete);
        assert!(!outcome.remaining_entries_known);
        assert!(fifo_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn captured_cache_clear_rejects_depth_beyond_bound_without_mutation() {
        use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
        use crate::test_runtime::TestRuntimeGuard;

        let _runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir_in(
            fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root"),
        )
        .expect("create profile base");
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("maintenance-depth-bound");
        let profile = WritableProfileGuard::acquire_current().expect("profile ownership");
        let cache = profile.profile_root().join(CACHE_DIR_NAME);
        let mut deep = cache.clone();
        for index in 0..=MAX_MAINTENANCE_DEPTH {
            deep.push(format!("level-{index}"));
        }
        fs::create_dir_all(&deep).expect("deep cache tree");
        fs::write(deep.join("payload.cache"), [0_u8; 43]).expect("deep payload");
        let namespace = profile
            .capture_disposable_cache_namespace()
            .expect("capture cache namespace");

        let error = namespace
            .clear(|| false)
            .expect_err("depth bound must fail closed");
        let CacheMaintenanceError::BoundsExceeded {
            outcome,
            kind,
            limit,
        } = error
        else {
            panic!("expected depth bound");
        };
        assert_eq!(kind, "depth");
        assert_eq!(limit, MAX_MAINTENANCE_DEPTH);
        assert_eq!(outcome.removed_files, 0);
        assert!(outcome.incomplete);
        assert!(!outcome.remaining_entries_known);
        assert!(deep.join("payload.cache").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn captured_cache_namespace_rejects_contained_and_alias_paths() {
        use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
        use crate::test_runtime::TestRuntimeGuard;
        use std::os::unix::fs::symlink;

        let _runtime = TestRuntimeGuard::acquire();
        let base = tempfile::tempdir_in(
            fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root"),
        )
        .expect("create profile base");
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("maintenance-conflicts");
        let profile = WritableProfileGuard::acquire_current().expect("profile ownership");
        let cache = profile.profile_root().join(CACHE_DIR_NAME);
        fs::create_dir_all(cache.join("nested")).expect("cache tree");
        let alias = profile.profile_root().join("cache-alias");
        symlink(&cache, &alias).expect("cache alias");
        let namespace = profile
            .capture_disposable_cache_namespace()
            .expect("capture cache namespace");

        assert!(namespace.conflicts_with_path(&cache));
        assert!(namespace.conflicts_with_path(&cache.join("nested")));
        assert!(namespace.conflicts_with_path(profile.profile_root()));
        assert!(namespace.conflicts_with_path(&alias));
        assert!(!namespace.conflicts_with_path(&profile.profile_root().join("logs")));
    }
}
