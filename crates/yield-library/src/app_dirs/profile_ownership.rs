//! Cross-process writable ownership for one Yield persistence profile.

use std::{
    collections::HashSet,
    fs::File,
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, ambient_authority};
use cap_std::fs::{Dir, OpenOptions};
use thiserror::Error;
use uuid::Uuid;

mod generation;

pub(crate) const PROFILE_OWNER_LOCK_FILE_NAME: &str = "profile-owner.lock";

/// Errors returned while acquiring writable ownership of the current profile.
#[derive(Debug, Error)]
pub enum ProfileOwnershipError {
    /// This process exhausted the checked profile-acquisition serial range.
    #[error("profile ownership generation exhausted")]
    OwnershipGenerationExhausted,
    /// Another process currently holds the profile ownership lock.
    #[error("profile is owned by another process: {path}")]
    ProfileOwnedByAnotherProcess {
        /// Profile lock path that could not be acquired.
        path: PathBuf,
    },
    /// Resolving the current profile root failed.
    #[error("profile root unavailable: {0}")]
    AppDirectory(String),
    /// Opening, validating, or writing the profile ownership boundary failed.
    #[error("profile ownership filesystem operation failed at {path}: {source}")]
    Io {
        /// Profile lock path involved in the failure.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// The profile lock path exists but is not a regular file.
    #[error("profile ownership lock is not a regular file: {path}")]
    NotRegularFile {
        /// Profile lock path that failed validation.
        path: PathBuf,
    },
    /// The retained profile root no longer matches the path used to acquire ownership.
    #[error("profile root was replaced after ownership acquisition: {path}")]
    ProfileRootReplaced {
        /// Profile root path whose identity changed.
        path: PathBuf,
    },
    /// The profile-owner lock entry no longer matches the acquired lock file.
    #[error("profile ownership lock was replaced after acquisition: {path}")]
    ProfileOwnerLockReplaced {
        /// Profile lock path whose identity changed.
        path: PathBuf,
    },
    /// An explicitly bound external source root no longer matches its acquired identity.
    #[error("external source root was replaced after binding: {path}")]
    SourceRootReplaced {
        /// External source-root path whose identity changed.
        path: PathBuf,
    },
    /// An explicitly bound external source root could not be opened or inspected.
    #[error("external source root is unavailable at {path}: {reason}")]
    ExternalSourceRootUnavailable {
        /// External source-root path involved in the failed operation.
        path: PathBuf,
        /// Stable bounded reason for the unavailable namespace.
        reason: String,
    },
    /// The host could not establish a stable identity for an external source root.
    #[error("external source root identity is uncertain at {path}: {reason}")]
    ExternalSourceRootIdentityUnavailable {
        /// External source-root path whose identity could not be established.
        path: PathBuf,
        /// Stable bounded reason for the uncertain identity.
        reason: String,
    },
    /// A profile-owned child database namespace could not be opened or inspected.
    #[error("profile child database namespace is unavailable at {path}: {reason}")]
    ProfileChildDatabaseRootUnavailable {
        /// Profile-child path involved in the failed operation.
        path: PathBuf,
        /// Stable bounded reason for the unavailable namespace.
        reason: String,
    },
    /// A profile-owned child database namespace was replaced after binding.
    #[error("profile child database namespace was replaced: {path}")]
    ProfileChildDatabaseRootReplaced {
        /// Profile-child path whose identity changed.
        path: PathBuf,
    },
    /// The host could not establish a stable identity for a profile child namespace.
    #[error("profile child database namespace identity is uncertain at {path}: {reason}")]
    ProfileChildDatabaseRootIdentityUnavailable {
        /// Profile-child path whose identity could not be established.
        path: PathBuf,
        /// Stable bounded reason for the uncertain identity.
        reason: String,
    },
    /// The host could not provide a stable identity for the retained capability.
    #[error("profile ownership identity unavailable at {path}")]
    IdentityUnavailable {
        /// Profile path whose identity could not be established.
        path: PathBuf,
    },
    /// A profile-owned child database is already retained by another participant in this process.
    #[error("profile child database is already open: {path}")]
    ProfileChildAlreadyLeased {
        /// Child path whose retained capability is already leased.
        path: PathBuf,
    },
    /// This platform has no verified nonblocking profile ownership primitive.
    #[error("profile ownership is unsupported on this platform: {path}")]
    Unsupported {
        /// Profile lock path that could not be protected.
        path: PathBuf,
    },
}

/// Process-wide writable ownership for one resolved Yield profile.
///
/// The guard retains the open lock file for its entire lifetime. The lock path is
/// intentionally never removed or renamed; its contents are diagnostic only, while
/// the live descriptor lock is the authority.
#[derive(Debug)]
pub struct WritableProfileGuard {
    profile_root: PathBuf,
    profile_identity: String,
    lock_path: PathBuf,
    lock_identity: String,
    owner_identity: String,
    ownership_generation: u64,
    root_dir: Dir,
    _lock_file: File,
    catalog_leases: Arc<Mutex<HashSet<String>>>,
}

/// Retained in-process exclusivity for one profile child database capability.
///
/// The lease is carried by the database's descriptor-bound root and therefore lives until the
/// database itself is dropped. Profile guard clones share the registry but do not acquire another
/// process-wide profile lock.
pub(crate) struct ProfileChildLease {
    registry: Arc<Mutex<HashSet<String>>>,
    identity: String,
}

impl ProfileChildLease {
    /// Immutable identity backing retained by this lease and its original registry entry.
    /// The shared, mutable registry allocation is owned by the profile and is not counted here.
    pub(crate) fn current_metadata_bytes(&self) -> Option<usize> {
        let registry = self.registry.lock().ok()?;
        let entry = registry.get(&self.identity)?;
        if self.identity.capacity() > 256 || entry.capacity() > 256 {
            return None;
        }
        std::mem::size_of::<(usize, usize, Self)>()
            .checked_add(self.identity.capacity())?
            .checked_add(entry.capacity())
    }
}

impl Drop for ProfileChildLease {
    fn drop(&mut self) {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.remove(&self.identity);
    }
}

impl WritableProfileGuard {
    /// Resolve the current profile and acquire its nonblocking writable-owner lock.
    pub fn acquire_current() -> Result<Self, ProfileOwnershipError> {
        Self::acquire_with_generation_counter(&generation::NEXT_GENERATION)
    }

    fn acquire_with_generation_counter(
        counter: &std::sync::atomic::AtomicU64,
    ) -> Result<Self, ProfileOwnershipError> {
        let profile_root = super::resolution::resolve_persistence()
            .map_err(|error| ProfileOwnershipError::AppDirectory(error.to_string()))?
            .app_root;
        let lock_path = profile_root.join(PROFILE_OWNER_LOCK_FILE_NAME);
        #[cfg(all(not(unix), not(windows)))]
        return Err(ProfileOwnershipError::Unsupported { path: lock_path });

        let root_dir =
            open_profile_root(&profile_root).map_err(|source| ProfileOwnershipError::Io {
                path: profile_root.clone(),
                source,
            })?;
        let profile_identity = identity_for_root(&profile_root, &root_dir)?;
        let lock_file = acquire_lock(&root_dir, &lock_path)?;
        let ownership_generation = generation::issue_from(counter)?;
        // Generate this only after the OS lock has been acquired. The diagnostic is never used
        // as an ownership authority; the live descriptor lock remains the sole authority.
        let owner_identity = Uuid::new_v4().to_string();
        let lock_file = write_diagnostic(lock_file, &lock_path, &owner_identity)?;
        let lock_identity = identity_for_lock(&lock_path, &lock_file)?;
        let guard = Self {
            profile_root,
            profile_identity,
            lock_path,
            lock_identity,
            owner_identity,
            ownership_generation,
            root_dir,
            _lock_file: lock_file,
            catalog_leases: Arc::new(Mutex::new(HashSet::new())),
        };
        guard.validate_current()?;
        Ok(guard)
    }

    /// Return the profile root protected by this guard.
    pub fn profile_root(&self) -> &Path {
        &self.profile_root
    }

    /// Return the opaque identity for this successful profile-owner acquisition.
    ///
    /// Cloned guards retain the same identity. A later acquisition after all guards
    /// are dropped receives a fresh identity. This value is diagnostic and useful
    /// for future owner/coordinator fences; it is never used to decide ownership.
    pub fn owner_identity(&self) -> &str {
        &self.owner_identity
    }

    /// Acquisition-local serial, meaningful only with this original live guard and UUID.
    /// It is not durable, cross-process monotonic, or permission to mutate a database.
    pub(crate) fn ownership_generation(&self) -> u64 {
        self.ownership_generation
    }

    /// Return the identity captured from the retained profile-root capability.
    pub(crate) fn profile_root_identity(&self) -> &str {
        &self.profile_identity
    }

    /// Return the identity captured from the retained profile-owner lock file.
    pub(crate) fn profile_lock_identity(&self) -> &str {
        &self.lock_identity
    }

    /// Observe the original retained lock descriptor; no pathname reopen or capacity authority.
    pub(crate) fn retained_lock_allocation_facts(
        &self,
    ) -> Result<(u64, crate::durable_capacity::VolumeFacts), ProfileOwnershipError> {
        self.validate_current()?;
        let bytes = self
            ._lock_file
            .metadata()
            .map_err(|source| ProfileOwnershipError::Io {
                path: self.lock_path.clone(),
                source,
            })?
            .len();
        let facts = crate::durable_capacity::descriptor_capacity_facts(&self._lock_file).map_err(
            |error| ProfileOwnershipError::Io {
                path: self.lock_path.clone(),
                source: io::Error::other(error.to_string()),
            },
        )?;
        self.validate_current()?;
        Ok((bytes, facts))
    }

    /// Clone this acquired profile capability for another profile-owned participant.
    ///
    /// The clone retains the same lock identity and open lock descriptor. It therefore does not
    /// acquire a second profile lock, and the profile remains owned until every participant clone
    /// has been dropped.
    pub fn try_clone(&self) -> Result<Self, ProfileOwnershipError> {
        self.validate_current()?;
        let root_dir = self
            .root_dir
            .try_clone()
            .map_err(|source| ProfileOwnershipError::Io {
                path: self.profile_root.clone(),
                source,
            })?;
        let lock_file =
            self._lock_file
                .try_clone()
                .map_err(|source| ProfileOwnershipError::Io {
                    path: self.lock_path.clone(),
                    source,
                })?;
        Ok(Self {
            profile_root: self.profile_root.clone(),
            profile_identity: self.profile_identity.clone(),
            lock_path: self.lock_path.clone(),
            lock_identity: self.lock_identity.clone(),
            owner_identity: self.owner_identity.clone(),
            ownership_generation: self.ownership_generation,
            root_dir,
            _lock_file: lock_file,
            catalog_leases: Arc::clone(&self.catalog_leases),
        })
    }

    /// Revalidate the acquired root and lock identities without following links.
    ///
    /// A successful check means the configured profile path still names the same root and
    /// lock entries that were acquired. The retained directory capability remains the authority
    /// for child access; this check detects a path replacement so the owner can fail closed.
    pub fn validate_current(&self) -> Result<(), ProfileOwnershipError> {
        let current_root = open_profile_root(&self.profile_root).map_err(|_| {
            ProfileOwnershipError::ProfileRootReplaced {
                path: self.profile_root.clone(),
            }
        })?;
        let current_identity = identity_from_dir(&current_root).ok_or_else(|| {
            ProfileOwnershipError::ProfileRootReplaced {
                path: self.profile_root.clone(),
            }
        })?;
        if current_identity != self.profile_identity {
            return Err(ProfileOwnershipError::ProfileRootReplaced {
                path: self.profile_root.clone(),
            });
        }

        let current_lock = open_lock_file(&current_root, false).map_err(|_| {
            ProfileOwnershipError::ProfileOwnerLockReplaced {
                path: self.lock_path.clone(),
            }
        })?;
        if !is_regular_file(&current_lock).unwrap_or(false) {
            return Err(ProfileOwnershipError::ProfileOwnerLockReplaced {
                path: self.lock_path.clone(),
            });
        }
        let current_lock_identity =
            identity_from_file(&self.lock_path, &current_lock).ok_or_else(|| {
                ProfileOwnershipError::ProfileOwnerLockReplaced {
                    path: self.lock_path.clone(),
                }
            })?;
        if current_lock_identity != self.lock_identity {
            return Err(ProfileOwnershipError::ProfileOwnerLockReplaced {
                path: self.lock_path.clone(),
            });
        }
        generation::validate(self.ownership_generation())?;
        Ok(())
    }

    /// Clone the retained no-follow profile-root capability for a production participant.
    pub fn profile_root_dir(&self) -> Result<Dir, ProfileOwnershipError> {
        self.validate_current()?;
        self.root_dir
            .try_clone()
            .map_err(|source| ProfileOwnershipError::Io {
                path: self.profile_root.clone(),
                source,
            })
    }

    /// Capture the profile-owned rebuildable cache namespace for a bounded maintenance task.
    ///
    /// The returned namespace retains a cloned profile capability and the cache directory
    /// capability. It does not consult process-global profile resolution after this call.
    pub fn capture_disposable_cache_namespace(
        &self,
    ) -> Result<super::DisposableCacheNamespace, ProfileOwnershipError> {
        super::storage_usage::DisposableCacheNamespace::capture(self)
    }

    /// Create or open one direct child directory relative to the retained profile capability.
    pub fn open_child_dir(&self, child: &Path) -> Result<Dir, ProfileOwnershipError> {
        let child_name =
            single_normal_component(child).map_err(|source| ProfileOwnershipError::Io {
                path: self.profile_root.join(child),
                source,
            })?;
        self.validate_current()?;
        match self.root_dir.create_dir(child_name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(ProfileOwnershipError::Io {
                    path: self.profile_root.join(child),
                    source,
                });
            }
        }
        let child_dir = self
            .root_dir
            .open_dir_nofollow(child_name)
            .map_err(|source| ProfileOwnershipError::Io {
                path: self.profile_root.join(child),
                source,
            })?;
        self.validate_current()?;
        Ok(child_dir)
    }

    /// Open one direct child and retain an in-process exclusive lease for its physical identity.
    pub(crate) fn open_child_dir_with_lease(
        &self,
        child: &Path,
    ) -> Result<(Dir, ProfileChildLease), ProfileOwnershipError> {
        let child_dir = self.open_child_dir(child)?;
        let identity = identity_from_dir(&child_dir).ok_or_else(|| {
            ProfileOwnershipError::IdentityUnavailable {
                path: self.profile_root.join(child),
            }
        })?;
        let mut registry = self
            .catalog_leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !registry.insert(identity.clone()) {
            return Err(ProfileOwnershipError::ProfileChildAlreadyLeased {
                path: self.profile_root.join(child),
            });
        }
        let lease = ProfileChildLease {
            registry: Arc::clone(&self.catalog_leases),
            identity,
        };
        drop(registry);
        if let Err(error) = self.validate_current() {
            drop(lease);
            return Err(error);
        }
        Ok((child_dir, lease))
    }

    /// Reserve one profile-scoped in-process capability without opening another path.
    ///
    /// This is used for profile-owned participants whose physical database is the profile root
    /// itself.  The registry is shared by cloned profile guards, so a second participant cannot
    /// accidentally open another writable connection for the same logical role.  The returned
    /// RAII lease remains the only authority for the reservation and releases it on every startup
    /// failure, worker panic, and normal teardown path.
    pub(crate) fn acquire_named_lease(
        &self,
        name: &str,
    ) -> Result<ProfileChildLease, ProfileOwnershipError> {
        self.validate_current()?;
        let identity = format!("profile-participant:{name}:{}", self.profile_identity);
        let mut registry = self
            .catalog_leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !registry.insert(identity.clone()) {
            return Err(ProfileOwnershipError::ProfileChildAlreadyLeased {
                path: self.profile_root.join(name),
            });
        }
        let lease = ProfileChildLease {
            registry: Arc::clone(&self.catalog_leases),
            identity,
        };
        drop(registry);
        if let Err(error) = self.validate_current() {
            drop(lease);
            return Err(error);
        }
        Ok(lease)
    }
}

/// Open the profile-independent per-user application root used by shared ownership helpers.
///
/// This deliberately derives the root from the operating-system user account on macOS rather
/// than from profile or process-local configuration overrides. Other targets remain unsupported
/// until an equivalent identity-safe application-data location is qualified.
pub(crate) fn open_canonical_global_app_root() -> Result<(PathBuf, Dir), ProfileOwnershipError> {
    #[cfg(target_os = "macos")]
    {
        let home = canonical_user_home().map_err(|source| ProfileOwnershipError::Io {
            path: PathBuf::from("~"),
            source,
        })?;
        let home_dir = open_profile_root(&home).map_err(|source| ProfileOwnershipError::Io {
            path: home.clone(),
            source,
        })?;
        ensure_current_user_owned(&home, &home_dir)?;

        let library_dir = open_existing_child(&home_dir, "Library", &home)?;
        ensure_current_user_owned(&home.join("Library"), &library_dir)?;
        let support_path = home.join("Library").join("Application Support");
        let support_dir =
            open_existing_child(&library_dir, "Application Support", &home.join("Library"))?;
        ensure_current_user_owned(&support_path, &support_dir)?;
        let app_path = support_path.join(super::APP_DIR_NAME);
        let (app_dir, _) =
            open_or_create_final_child(&support_dir, super::APP_DIR_NAME, &support_path)?;
        ensure_current_user_owned(&app_path, &app_dir)?;
        return Ok((app_path, app_dir));
    }

    #[cfg(not(target_os = "macos"))]
    {
        Err(ProfileOwnershipError::Unsupported {
            path: PathBuf::from(super::APP_DIR_NAME),
        })
    }
}

/// Exact native directory backing after a refused legacy-only app bootstrap. No profile grant.
#[derive(Default)]
pub(crate) struct LegacyAppRootBacking {
    // Close the affected child before its retained ancestor namespace.
    child: Option<Dir>,
    parent: Option<Dir>,
}
impl LegacyAppRootBacking {
    pub(crate) fn metadata_bytes(&self) -> usize {
        let _held = (&self.parent, &self.child);
        std::mem::size_of::<Self>()
    }
}

/// Bootstrap only under the original sealed pre-lease source proof. Existing Current APIs
/// keep their original behavior; failure moves affected parent/child descriptors to the caller.
pub(crate) fn open_legacy_canonical_app_root(
    proof: &crate::sample_sources::db::LegacyReserveUseProof<'_>,
) -> Result<(PathBuf, Dir), LegacyAppRootBacking> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = proof;
        Err(LegacyAppRootBacking::default())
    }
    #[cfg(target_os = "macos")]
    {
        let mut backing = LegacyAppRootBacking::default();
        let result = (|| {
            proof.validate_for_bootstrap().map_err(|_| ())?;
            let home = canonical_user_home().map_err(|_| ())?;
            let bounded = |p: &PathBuf| {
                p.as_os_str().as_encoded_bytes().len() <= 4096 && p.capacity() <= 8192
            };
            if !bounded(&home) {
                return Err(());
            }
            let home_dir = open_profile_root(&home).map_err(|_| ())?;
            ensure_current_user_owned(&home, &home_dir).map_err(|_| ())?;
            let library = open_existing_child(&home_dir, "Library", &home).map_err(|_| ())?;
            let library_path = home.join("Library");
            ensure_current_user_owned(&library_path, &library).map_err(|_| ())?;
            let support_path = library_path.join("Application Support");
            let support = open_existing_child(&library, "Application Support", &library_path)
                .map_err(|_| ())?;
            ensure_current_user_owned(&support_path, &support).map_err(|_| ())?;
            let app_path = support_path.join(super::APP_DIR_NAME);
            if !bounded(&app_path) {
                return Err(());
            }
            backing.parent = Some(support);
            let parent = backing.parent.as_ref().unwrap();
            let app = match parent.open_dir_nofollow(super::APP_DIR_NAME) {
                Ok(app) => app,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    proof.validate_for_bootstrap().map_err(|_| ())?;
                    parent
                        .create_dir(super::APP_DIR_NAME)
                        .or_else(|e| {
                            if e.kind() == io::ErrorKind::AlreadyExists {
                                Ok(())
                            } else {
                                Err(e)
                            }
                        })
                        .map_err(|_| ())?;
                    // Retain the parent even if a refused/raced child cannot be opened.
                    let app = parent
                        .open_dir_nofollow(super::APP_DIR_NAME)
                        .map_err(|_| ())?;
                    backing.child = Some(app);
                    proof.validate_for_bootstrap().map_err(|_| ())?;
                    sync_directory(parent, &support_path).map_err(|_| ())?;
                    backing.child.take().unwrap()
                }
                Err(_) => return Err(()),
            };
            backing.child = Some(app);
            ensure_current_user_owned(&app_path, backing.child.as_ref().unwrap())
                .map_err(|_| ())?;
            proof.validate_for_bootstrap().map_err(|_| ())?;
            Ok((app_path, backing.child.take().unwrap()))
        })();
        result.map_err(|()| backing)
    }
}

/// Open an absolute directory by walking every component without following symlinks.
pub(crate) fn open_no_follow_directory(path: &Path) -> io::Result<Dir> {
    open_profile_root(path)
}

#[cfg(target_os = "macos")]
fn canonical_user_home() -> io::Result<PathBuf> {
    use std::{ffi::CStr, mem::MaybeUninit, os::unix::ffi::OsStringExt, ptr};

    let uid = unsafe { libc::geteuid() };
    let mut passwd = MaybeUninit::<libc::passwd>::zeroed();
    let mut buffer = vec![0_u8; 16 * 1024];
    let mut result = ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            passwd.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status));
    }
    if result.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "effective user has no passwd entry",
        ));
    }
    let directory = unsafe { CStr::from_ptr((*result).pw_dir) };
    if directory.to_bytes().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "effective user has an empty home directory",
        ));
    }
    Ok(PathBuf::from(std::ffi::OsString::from_vec(
        directory.to_bytes().to_vec(),
    )))
}

#[cfg(target_os = "macos")]
fn ensure_current_user_owned(path: &Path, dir: &Dir) -> Result<(), ProfileOwnershipError> {
    use std::os::unix::fs::MetadataExt;

    let file = dir
        .try_clone()
        .map_err(|source| ProfileOwnershipError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .into_std_file();
    let metadata = file
        .metadata()
        .map_err(|source| ProfileOwnershipError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid {
        return Err(ProfileOwnershipError::Io {
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::PermissionDenied,
                "shared ownership root is not owned by the effective user",
            ),
        });
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_existing_child(
    parent: &Dir,
    name: &str,
    parent_path: &Path,
) -> Result<Dir, ProfileOwnershipError> {
    parent
        .open_dir_nofollow(name)
        .map_err(|source| ProfileOwnershipError::Io {
            path: parent_path.join(name),
            source,
        })
}

#[cfg(target_os = "macos")]
fn open_or_create_final_child(
    parent: &Dir,
    name: &str,
    parent_path: &Path,
) -> Result<(Dir, bool), ProfileOwnershipError> {
    match parent.open_dir_nofollow(name) {
        Ok(directory) => Ok((directory, false)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            parent
                .create_dir(name)
                .or_else(|create_error| {
                    if create_error.kind() == io::ErrorKind::AlreadyExists {
                        Ok(())
                    } else {
                        Err(create_error)
                    }
                })
                .map_err(|source| ProfileOwnershipError::Io {
                    path: parent_path.join(name),
                    source,
                })?;
            sync_directory(parent, parent_path)?;
            parent
                .open_dir_nofollow(name)
                .map(|directory| (directory, true))
                .map_err(|source| ProfileOwnershipError::Io {
                    path: parent_path.join(name),
                    source,
                })
        }
        Err(source) => Err(ProfileOwnershipError::Io {
            path: parent_path.join(name),
            source,
        }),
    }
}

#[cfg(target_os = "macos")]
fn sync_directory(directory: &Dir, path: &Path) -> Result<(), ProfileOwnershipError> {
    directory
        .try_clone()
        .and_then(|directory| directory.into_std_file().sync_all())
        .map_err(|source| ProfileOwnershipError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn single_normal_component(path: &Path) -> io::Result<&Path> {
    let mut components = path.components();
    let Some(Component::Normal(name)) = components.next() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "profile child must be a single normal component",
        ));
    };
    if components.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "profile child must be a single normal component",
        ));
    }
    Ok(Path::new(name))
}

fn open_profile_root(path: &Path) -> io::Result<Dir> {
    let mut components = path.components();
    #[cfg(unix)]
    {
        if !matches!(components.next(), Some(Component::RootDir)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "profile root must be absolute",
            ));
        }
        let mut dir = Dir::open_ambient_dir(Path::new("/"), ambient_authority())?;
        for component in components {
            let Component::Normal(name) = component else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "profile root contains a non-normal component",
                ));
            };
            dir = dir.open_dir_nofollow(name)?;
        }
        Ok(dir)
    }
    #[cfg(windows)]
    {
        let Some(Component::Prefix(prefix)) = components.next() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "profile root must have a Windows path prefix",
            ));
        };
        if !matches!(components.next(), Some(Component::RootDir)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "profile root must be absolute",
            ));
        }
        let mut anchor = PathBuf::from(prefix.as_os_str());
        anchor.push(std::path::MAIN_SEPARATOR_STR);
        let mut dir = Dir::open_ambient_dir(anchor, ambient_authority())?;
        for component in components {
            let Component::Normal(name) = component else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "profile root contains a non-normal component",
                ));
            };
            dir = dir.open_dir_nofollow(name)?;
        }
        return Ok(dir);
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (path, components);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no verified profile-root capability on this platform",
        ))
    }
}

fn identity_for_root(path: &Path, dir: &Dir) -> Result<String, ProfileOwnershipError> {
    identity_from_dir(dir).ok_or_else(|| ProfileOwnershipError::IdentityUnavailable {
        path: path.to_path_buf(),
    })
}

fn identity_for_lock(path: &Path, file: &File) -> Result<String, ProfileOwnershipError> {
    identity_from_file(path, file).ok_or_else(|| ProfileOwnershipError::IdentityUnavailable {
        path: path.to_path_buf(),
    })
}

fn identity_from_dir(dir: &Dir) -> Option<String> {
    let retained_file = dir.try_clone().ok()?.into_std_file();
    crate::filesystem_identity::stable_filesystem_identity_from_open_file(&retained_file)
}

fn identity_from_file(_path: &Path, file: &File) -> Option<String> {
    crate::filesystem_identity::stable_filesystem_identity_from_open_file(file)
}

fn is_regular_file(file: &File) -> io::Result<bool> {
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        return Ok(
            metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0
        );
    }
    #[cfg(not(windows))]
    {
        Ok(metadata.is_file() && !metadata.is_symlink())
    }
}

fn open_lock_file(root_dir: &Dir, create: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create)
        .follow(FollowSymlinks::No);
    root_dir
        .open_with(Path::new(PROFILE_OWNER_LOCK_FILE_NAME), &options)
        .map(|file| file.into_std())
}

fn write_diagnostic(
    mut file: File,
    path: &Path,
    owner_identity: &str,
) -> Result<File, ProfileOwnershipError> {
    file.set_len(0)
        .and_then(|_| {
            file.write_all(
                format!(
                    "version=2\npid={}\nowner_id={owner_identity}\n",
                    std::process::id()
                )
                .as_bytes(),
            )
        })
        .and_then(|_| file.sync_all())
        .map_err(|source| ProfileOwnershipError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(file)
}

#[cfg(unix)]
fn acquire_lock(root_dir: &Dir, path: &Path) -> Result<File, ProfileOwnershipError> {
    use std::os::fd::AsRawFd;

    let file = open_lock_file(root_dir, true).map_err(|source| ProfileOwnershipError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !is_regular_file(&file).map_err(|source| ProfileOwnershipError::Io {
        path: path.to_path_buf(),
        source,
    })? {
        return Err(ProfileOwnershipError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }

    // Advisory descriptor locking is released by the kernel when this process exits,
    // while the path remains available for diagnostics and future acquisition.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let source = io::Error::last_os_error();
        if source.kind() == io::ErrorKind::WouldBlock {
            return Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess {
                path: path.to_path_buf(),
            });
        }
        return Err(ProfileOwnershipError::Io {
            path: path.to_path_buf(),
            source,
        });
    }

    Ok(file)
}

#[cfg(windows)]
fn acquire_lock(root_dir: &Dir, path: &Path) -> Result<File, ProfileOwnershipError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE, WIN32_ERROR};
    use windows::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows::Win32::System::IO::OVERLAPPED;

    let file = open_lock_file(root_dir, true).map_err(|source| ProfileOwnershipError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !is_regular_file(&file).map_err(|source| ProfileOwnershipError::Io {
        path: path.to_path_buf(),
        source,
    })? {
        return Err(ProfileOwnershipError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }

    let mut overlapped = OVERLAPPED::default();
    let result = unsafe {
        LockFileEx(
            HANDLE(file.as_raw_handle()),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            None,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if let Err(error) = result {
        if WIN32_ERROR::from_error(&error) == Some(ERROR_LOCK_VIOLATION) {
            return Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess {
                path: path.to_path_buf(),
            });
        }
        return Err(ProfileOwnershipError::Io {
            path: path.to_path_buf(),
            source: io::Error::other(error.to_string()),
        });
    }

    Ok(file)
}

#[cfg(all(not(unix), not(windows)))]
fn acquire_lock(_root_dir: &Dir, path: &Path) -> Result<File, ProfileOwnershipError> {
    Err(ProfileOwnershipError::Unsupported {
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app_dirs::{APP_DIR_NAME, ConfigBaseGuard, PROFILE_DIR_NAME, PersistenceProfileGuard},
        test_runtime::TestRuntimeGuard,
    };
    use std::{
        fs,
        io::{self, BufRead, BufReader, Read, Seek, Write},
        process::{Child, ChildStdin, Command, Stdio},
        sync::mpsc::{self, Receiver},
        thread,
        time::Duration,
    };

    const CONFIG_HOME_ENV: &str = "YIELD_CONFIG_HOME";
    const CONFIG_PROFILE_ENV: &str = "YIELD_CONFIG_PROFILE";
    const OWNER_HELPER_MODE_ENV: &str = "YIELD_PROFILE_OWNER_HELPER_MODE";
    const OWNER_HELPER_READY_LINE: &str = "YIELD_PROFILE_OWNER_READY";

    fn test_runtime() -> TestRuntimeGuard {
        TestRuntimeGuard::acquire()
    }

    fn profile_base() -> tempfile::TempDir {
        // macOS exposes the default temporary root through /var -> /private/var.
        // Resolve that ambient alias before exercising the no-follow profile capability.
        let root = fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root");
        tempfile::tempdir_in(root).expect("profile base")
    }

    fn profile_root(base: &std::path::Path, profile: &str) -> PathBuf {
        base.join(APP_DIR_NAME).join(PROFILE_DIR_NAME).join(profile)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn final_global_app_child_creation_is_parent_bound_and_no_follow() {
        let _runtime = test_runtime();
        let base = profile_base();
        let support_path = base.path().join("Library").join("Application Support");
        fs::create_dir_all(&support_path).expect("application support parent");
        let support_dir = open_no_follow_directory(&support_path).expect("open support parent");

        let (app_dir, created) =
            open_or_create_final_child(&support_dir, APP_DIR_NAME, &support_path)
                .expect("create final application child");
        assert!(created);
        assert!(app_dir.try_clone().is_ok());

        let (_, existing) = open_or_create_final_child(&support_dir, APP_DIR_NAME, &support_path)
            .expect("reopen final application child");
        assert!(!existing);

        let symlink_name = "symlink-child";
        let symlink_target = support_path.join("symlink-target");
        fs::create_dir(&symlink_target).expect("symlink target");
        std::os::unix::fs::symlink(&symlink_target, support_path.join(symlink_name))
            .expect("symlink child");
        assert!(open_or_create_final_child(&support_dir, symlink_name, &support_path).is_err());
    }

    fn read_locked_diagnostic(file: &File) -> io::Result<Vec<u8>> {
        let mut file = file;
        file.rewind()?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)?;
        Ok(contents)
    }

    struct OwnerHelperProcess {
        child: Child,
        stdin: Option<ChildStdin>,
        ready_rx: Receiver<Result<(), String>>,
        eof_rx: Receiver<()>,
    }

    impl Drop for OwnerHelperProcess {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
        }
    }

    fn spawn_owner_helper(base: &std::path::Path, profile: &str, mode: &str) -> OwnerHelperProcess {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .arg("--exact")
            .arg("app_dirs::profile_ownership::tests::profile_owner_process_helper")
            .arg("--ignored")
            .arg("--nocapture")
            .env(CONFIG_HOME_ENV, base)
            .env(CONFIG_PROFILE_ENV, profile)
            .env(OWNER_HELPER_MODE_ENV, mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn profile owner helper");
        let stdin = child.stdin.take().expect("helper stdin");
        let stdout = child.stdout.take().expect("helper stdout");
        let stderr = child.stderr.take().expect("helper stderr");
        let (ready_tx, ready_rx) = mpsc::channel();
        let (eof_tx, eof_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let ready = loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        break Err(String::from("profile owner helper exited before handshake"));
                    }
                    Ok(_) if line.trim_end().ends_with(OWNER_HELPER_READY_LINE) => break Ok(()),
                    Ok(_) => continue,
                    Err(error) => {
                        break Err(format!(
                            "failed to read profile owner helper handshake: {error}"
                        ));
                    }
                }
            };
            let _ = ready_tx.send(ready);
            let _ = io::copy(&mut reader, &mut io::sink());
            let _ = eof_tx.send(());
        });
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let _ = io::copy(&mut reader, &mut io::sink());
        });
        OwnerHelperProcess {
            child,
            stdin: Some(stdin),
            ready_rx,
            eof_rx,
        }
    }

    fn release_owner_helper(helper: &mut OwnerHelperProcess, command: &str) {
        let mut stdin = helper.stdin.take().expect("profile owner helper stdin");
        writeln!(stdin, "{command}").expect("send profile owner helper command");
        stdin.flush().expect("flush profile owner helper command");
    }

    fn wait_for_owner_helper_ready(helper: &mut OwnerHelperProcess) {
        match helper.ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                panic!("profile owner helper handshake failed: {error}");
            }
            Err(error) => {
                panic!("timed out waiting for profile owner helper handshake: {error}");
            }
        }
    }

    fn wait_for_owner_helper(helper: &mut OwnerHelperProcess, expected_success: bool) {
        if helper.eof_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            panic!("timed out waiting for profile owner helper to exit");
        }
        let status = helper.child.wait().expect("wait for profile owner helper");
        assert_eq!(
            status.success(),
            expected_success,
            "profile owner helper exit status"
        );
    }

    #[test]
    fn active_profile_owner_conflicts_without_replacing_lock_contents() {
        let _runtime = test_runtime();
        let base = profile_base();
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("ownership-conflict");
        let holder = WritableProfileGuard::acquire_current().expect("profile holder");
        let lock_path = holder.profile_root().join(PROFILE_OWNER_LOCK_FILE_NAME);
        let contents = read_locked_diagnostic(&holder._lock_file).expect("owner diagnostic");
        assert!(String::from_utf8_lossy(&contents).contains("version=2\n"));
        assert!(
            String::from_utf8_lossy(&contents)
                .contains(&format!("owner_id={}\n", holder.owner_identity()))
        );

        let error = WritableProfileGuard::acquire_current().expect_err("active owner conflict");

        assert!(matches!(
            error,
            ProfileOwnershipError::ProfileOwnedByAnotherProcess { path } if path == lock_path
        ));
        assert_eq!(
            read_locked_diagnostic(&holder._lock_file).expect("owner diagnostic after conflict"),
            contents
        );
    }

    #[test]
    fn owner_identity_is_retained_by_clones_and_changes_after_reacquisition() {
        let _runtime = test_runtime();
        let base = profile_base();
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("ownership-generation");
        let holder = WritableProfileGuard::acquire_current().expect("profile holder");
        let owner_identity = holder.owner_identity().to_owned();
        let generation = holder.ownership_generation();
        assert!((1..=i64::MAX as u64).contains(&generation));
        assert!(Uuid::parse_str(&owner_identity).is_ok());
        let clone = holder.try_clone().expect("profile clone");
        assert_eq!(clone.owner_identity(), owner_identity);
        assert_eq!(clone.ownership_generation(), generation);
        drop(holder);
        assert!(clone.validate_current().is_ok());
        assert!(matches!(
            WritableProfileGuard::acquire_current(),
            Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
        ));
        drop(clone);

        let reopened = WritableProfileGuard::acquire_current().expect("reopen profile");
        assert!(Uuid::parse_str(reopened.owner_identity()).is_ok());
        assert_ne!(reopened.owner_identity(), owner_identity);
        assert!(reopened.ownership_generation() > generation);
    }

    #[test]
    fn profile_generation_exhaustion_releases_actual_lock_without_changing_diagnostic() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let _runtime = test_runtime();
        let base = profile_base();
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("ownership-serial-exhaustion");
        let holder = WritableProfileGuard::acquire_current().unwrap();
        let lock_path = holder.lock_path.clone();
        let diagnostic = fs::read(&lock_path).unwrap();
        let counter = AtomicU64::new(i64::MAX as u64 + 1);
        // Native lock conflict wins before the counter is consulted.
        assert!(matches!(
            WritableProfileGuard::acquire_with_generation_counter(&counter),
            Err(ProfileOwnershipError::ProfileOwnedByAnotherProcess { .. })
        ));
        assert_eq!(counter.load(Ordering::Relaxed), i64::MAX as u64 + 1);
        drop(holder);
        assert!(matches!(
            WritableProfileGuard::acquire_with_generation_counter(&counter),
            Err(ProfileOwnershipError::OwnershipGenerationExhausted)
        ));
        assert_eq!(fs::read(&lock_path).unwrap(), diagnostic);
        let reopened = WritableProfileGuard::acquire_current().unwrap();
        assert!(reopened.validate_current().is_ok());
    }

    #[test]
    fn cross_process_profile_owner_conflict_is_typed_and_release_allows_reopen() {
        let _runtime = test_runtime();
        let base = profile_base();
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let profile = "ownership-process-release";
        let _profile_guard = PersistenceProfileGuard::named(profile);
        let mut helper = spawn_owner_helper(base.path(), profile, "release");
        wait_for_owner_helper_ready(&mut helper);

        let lock_path = profile_root(base.path(), profile).join(PROFILE_OWNER_LOCK_FILE_NAME);
        let error = WritableProfileGuard::acquire_current().expect_err("cross-process conflict");
        assert!(matches!(
            error,
            ProfileOwnershipError::ProfileOwnedByAnotherProcess { path } if path == lock_path
        ));

        release_owner_helper(&mut helper, "release");
        wait_for_owner_helper(&mut helper, true);
        let reopened = WritableProfileGuard::acquire_current().expect("reopen after release");
        assert_eq!(reopened.profile_root(), profile_root(base.path(), profile));
        assert!((1..=i64::MAX as u64).contains(&reopened.ownership_generation()));
    }

    #[test]
    fn cross_process_profile_owner_exit_releases_lock() {
        let _runtime = test_runtime();
        let base = profile_base();
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let profile = "ownership-process-crash";
        let _profile_guard = PersistenceProfileGuard::named(profile);
        let mut helper = spawn_owner_helper(base.path(), profile, "exit");
        wait_for_owner_helper_ready(&mut helper);

        let lock_path = profile_root(base.path(), profile).join(PROFILE_OWNER_LOCK_FILE_NAME);
        let error = WritableProfileGuard::acquire_current().expect_err("cross-process conflict");
        assert!(matches!(
            error,
            ProfileOwnershipError::ProfileOwnedByAnotherProcess { path } if path == lock_path
        ));

        release_owner_helper(&mut helper, "exit");
        wait_for_owner_helper(&mut helper, false);
        let reopened = WritableProfileGuard::acquire_current().expect("reopen after crash");
        assert_eq!(reopened.profile_root(), profile_root(base.path(), profile));
        assert!((1..=i64::MAX as u64).contains(&reopened.ownership_generation()));
    }

    #[test]
    #[ignore = "spawned by the cross-process profile ownership tests"]
    fn profile_owner_process_helper() {
        let mode = std::env::var(OWNER_HELPER_MODE_ENV).expect("profile owner helper mode");
        let _guard = WritableProfileGuard::acquire_current().expect("profile owner helper lock");
        println!("{OWNER_HELPER_READY_LINE}");
        io::stdout()
            .flush()
            .expect("flush profile owner helper handshake");
        let mut command = String::new();
        io::stdin()
            .read_line(&mut command)
            .expect("read profile owner helper command");
        match (mode.as_str(), command.trim()) {
            ("release", "release") => {}
            // Exit without running Rust destructors so the OS, rather than guard Drop, releases
            // the descriptor lock. This exercises abrupt process release without crash dialogs.
            ("exit", "exit") => std::process::exit(73),
            _ => panic!("unexpected profile owner helper command"),
        }
    }

    #[test]
    fn dropping_profile_owner_allows_reopen() {
        let _runtime = test_runtime();
        let base = profile_base();
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("ownership-reopen");
        let holder = WritableProfileGuard::acquire_current().expect("profile holder");
        let root = holder.profile_root().to_path_buf();
        drop(holder);

        let reopened = WritableProfileGuard::acquire_current().expect("reopen profile");

        assert_eq!(reopened.profile_root(), root);
    }

    #[test]
    fn stale_lock_contents_do_not_block_acquisition() {
        let _runtime = test_runtime();
        let base = profile_base();
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("ownership-stale");
        let root = profile_root(base.path(), "ownership-stale");
        fs::create_dir_all(&root).expect("profile root");
        let lock_path = root.join(PROFILE_OWNER_LOCK_FILE_NAME);
        fs::write(&lock_path, b"pid=stale\nunknown=diagnostic\n").expect("stale diagnostic");

        let guard = WritableProfileGuard::acquire_current().expect("stale contents ignored");

        assert_eq!(guard.profile_root(), root);
        assert_eq!(
            read_locked_diagnostic(&guard._lock_file).expect("fresh owner diagnostic"),
            format!(
                "version=2\npid={}\nowner_id={}\n",
                std::process::id(),
                guard.owner_identity()
            )
            .as_bytes()
        );
    }

    #[test]
    fn distinct_profile_roots_are_independently_writable() {
        let _runtime = test_runtime();
        let base = profile_base();
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let first_profile = PersistenceProfileGuard::named("ownership-one");
        let first = WritableProfileGuard::acquire_current().expect("first profile holder");
        let first_root = first.profile_root().to_path_buf();
        drop(first_profile);

        let _second_profile = PersistenceProfileGuard::named("ownership-two");
        let second = WritableProfileGuard::acquire_current().expect("second profile holder");

        assert_ne!(first_root, second.profile_root());
        assert!(first_root.ends_with("ownership-one"));
        assert!(second.profile_root().ends_with("ownership-two"));
    }

    #[cfg(unix)]
    #[test]
    fn retained_root_capability_and_identity_detect_profile_root_replacement() {
        let _runtime = test_runtime();
        let base = profile_base();
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("ownership-root-replaced");
        let guard = WritableProfileGuard::acquire_current().expect("profile holder");
        let root = guard.profile_root().to_path_buf();
        let journal = guard
            .open_child_dir(Path::new("operation_journal"))
            .expect("journal capability");

        let displaced = base.path().join("displaced-profile-root");
        fs::rename(&root, &displaced).expect("displace acquired root");
        fs::create_dir(&root).expect("replacement profile root");
        fs::create_dir(root.join("operation_journal")).expect("replacement journal");
        fs::write(
            root.join("operation_journal").join("replacement-marker"),
            b"replacement",
        )
        .expect("replacement marker");

        assert!(matches!(
            guard.validate_current(),
            Err(ProfileOwnershipError::ProfileRootReplaced { path }) if path == root
        ));
        assert!(
            !journal
                .entries()
                .expect("retained journal entries")
                .filter_map(Result::ok)
                .any(|entry| entry.file_name() == "replacement-marker")
        );
    }

    #[cfg(unix)]
    #[test]
    fn retained_lock_identity_detects_profile_lock_replacement() {
        let _runtime = test_runtime();
        let base = profile_base();
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("ownership-lock-replaced");
        let guard = WritableProfileGuard::acquire_current().expect("profile holder");
        let lock_path = guard.profile_root().join(PROFILE_OWNER_LOCK_FILE_NAME);
        let displaced = guard.profile_root().join("profile-owner.lock.old");
        fs::rename(&lock_path, &displaced).expect("displace acquired lock entry");
        fs::write(&lock_path, b"replacement").expect("replacement lock entry");

        assert!(matches!(
            guard.validate_current(),
            Err(ProfileOwnershipError::ProfileOwnerLockReplaced { path }) if path == lock_path
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_profile_lock_fails_closed_without_touching_target() {
        use std::os::unix::fs::symlink;

        let _runtime = test_runtime();
        let base = profile_base();
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("ownership-symlink");
        let root = profile_root(base.path(), "ownership-symlink");
        fs::create_dir_all(&root).expect("profile root");
        let target = base.path().join("target");
        fs::write(&target, b"do not touch").expect("target");
        let lock_path = root.join(PROFILE_OWNER_LOCK_FILE_NAME);
        symlink(&target, &lock_path).expect("profile lock symlink");

        let result = WritableProfileGuard::acquire_current();

        assert!(result.is_err());
        assert_eq!(
            fs::read(&target).expect("target after failed acquire"),
            b"do not touch"
        );
        assert!(
            fs::symlink_metadata(lock_path)
                .expect("symlink after failed acquire")
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(windows)]
    #[test]
    fn reparse_profile_lock_fails_closed_without_touching_target() {
        use std::os::windows::fs::symlink_file;

        let _runtime = test_runtime();
        let base = profile_base();
        let _base_guard = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile_guard = PersistenceProfileGuard::named("ownership-reparse");
        let root = profile_root(base.path(), "ownership-reparse");
        fs::create_dir_all(&root).expect("profile root");
        let target = base.path().join("target");
        fs::write(&target, b"do not touch").expect("target");
        let lock_path = root.join(PROFILE_OWNER_LOCK_FILE_NAME);
        symlink_file(&target, &lock_path).expect("profile lock reparse point");

        let result = WritableProfileGuard::acquire_current();

        assert!(result.is_err());
        assert_eq!(
            fs::read(&target).expect("target after failed acquire"),
            b"do not touch"
        );
    }
}
