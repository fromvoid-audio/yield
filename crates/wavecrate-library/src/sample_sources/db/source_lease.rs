//! Physical-source ownership for profile-authorized SQLite writers.
//!
//! The sidecar lock is the live cross-process authority. The SQLite row is durable fencing and
//! recovery evidence only; it is never used by itself to infer that another owner is dead.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;
use uuid::Uuid;

use crate::app_dirs::{ProfileOwnershipError, WritableProfileGuard};
use crate::filesystem_identity::stable_filesystem_identity_from_open_file;

use super::file_ops_journal::BoundDatabaseRoot;
use super::sqlite_identity::DatabaseFileBinding;
use super::{SourceDatabaseIdentityError, schema};

/// Filename for the physical-source ownership sidecar.
pub(crate) const SOURCE_LEASE_LOCK_FILE_NAME: &str = ".wavecrate-source-lease.lock";

const LEASE_DURATION_SECONDS: i64 = 300;
const LEASE_RENEWAL_THRESHOLD_SECONDS: i64 = 60;
const SOURCE_LEASE_TABLE: &str = "source_lease";
const SOURCE_LEASE_TABLE_SCHEMA_VERSION: i64 = 1;
const MAX_LEASE_TEXT_BYTES: i64 = 1024;
const MAX_LEASE_SCHEMA_SQL_BYTES: i64 = 4096;
const MAX_LEASE_TRIGGER_COUNT: i64 = 64;

/// Typed failures from the physical-source ownership boundary.
#[derive(Debug, Error)]
pub enum SourceLeaseError {
    /// Another process currently holds the source sidecar lock.
    #[error("source database is owned by another process: {path}")]
    WritableSourceOwnedByAnotherProcess {
        /// Source sidecar path that could not be acquired.
        path: PathBuf,
    },
    /// A profile or source-root capability no longer validates.
    #[error(transparent)]
    ProfileOwnership(#[from] ProfileOwnershipError),
    /// The source root was replaced after its explicit binding was acquired.
    #[error("bound source root was replaced: {path}")]
    SourceRootReplaced {
        /// External source-root path whose identity changed.
        path: PathBuf,
    },
    /// The external source-root namespace could not be opened or inspected.
    #[error("external source root is unavailable at {path}: {reason}")]
    SourceRootUnavailable {
        /// External source-root path involved in the failed operation.
        path: PathBuf,
        /// Stable bounded reason for the unavailable namespace.
        reason: String,
    },
    /// The external source-root identity could not be established.
    #[error("external source root identity is uncertain at {path}: {reason}")]
    SourceRootIdentityUnavailable {
        /// External source-root path whose identity could not be established.
        path: PathBuf,
        /// Stable bounded reason for the uncertain identity.
        reason: String,
    },
    /// The bound database namespace was replaced after its explicit binding was acquired.
    #[error("bound database root was replaced: {path}")]
    DatabaseRootReplaced {
        /// Database-root path whose identity changed.
        path: PathBuf,
    },
    /// The bound database namespace could not be opened or inspected.
    #[error("bound database root is unavailable at {path}: {reason}")]
    DatabaseRootUnavailable {
        /// Database-root path involved in the failed operation.
        path: PathBuf,
        /// Stable bounded reason for the unavailable namespace.
        reason: String,
    },
    /// The bound database-root identity could not be established.
    #[error("bound database root identity is uncertain at {path}: {reason}")]
    DatabaseRootIdentityUnavailable {
        /// Database-root path whose identity could not be established.
        path: PathBuf,
        /// Stable bounded reason for the uncertain identity.
        reason: String,
    },
    /// A retained SQLite database identity check failed.
    #[error(transparent)]
    DatabaseIdentity(#[from] SourceDatabaseIdentityError),
    /// The source sidecar pathname now identifies a different file.
    #[error("source lease sidecar was replaced: {path}")]
    SidecarReplaced {
        /// Sidecar pathname whose identity changed.
        path: PathBuf,
    },
    /// The database has a legacy or unknown schema that this lease bootstrap cannot adopt.
    #[error(
        "source database schema requires migration before source lease bootstrap at {path}: user_version={user_version}"
    )]
    MigrationRequired {
        /// Database pathname whose schema was rejected.
        path: PathBuf,
        /// SQLite user-version observed before any lease-table mutation.
        user_version: i64,
    },
    /// The database or source-lease table has an unsupported shape.
    #[error("source lease schema is unavailable at {path}: {reason}")]
    SchemaUnavailable {
        /// Database pathname whose schema was rejected.
        path: PathBuf,
        /// Stable bounded reason for the rejection.
        reason: &'static str,
    },
    /// The source database pathname disappeared while a prior sidecar lease still existed.
    #[error("source database is missing while its lease sidecar already exists: {path}")]
    DatabaseMissingWithExistingLease {
        /// Database pathname that was deliberately not recreated.
        path: PathBuf,
    },
    /// A lease table or row that records prior ownership is missing and cannot be reset safely.
    #[error("source lease state is unavailable at {path}: {reason}")]
    LeaseStateUnavailable {
        /// Database pathname whose lease state was incomplete.
        path: PathBuf,
        /// Stable bounded reason for retaining the database for audit.
        reason: &'static str,
    },
    /// A persisted source-lease row is malformed or violates its bounded contract.
    #[error("source lease row is invalid at {path}: {reason}")]
    InvalidRow {
        /// Database pathname containing the invalid row.
        path: PathBuf,
        /// Stable bounded reason for the rejection.
        reason: &'static str,
    },
    /// An active row has not expired and cannot be reclaimed.
    #[error("source lease has not expired at {path}: expires_at={expires_at}")]
    LeaseNotExpired {
        /// Database pathname containing the active row.
        path: PathBuf,
        /// Persisted lease expiry timestamp.
        expires_at: i64,
    },
    /// Expiry could not be combined with a proof that the previous profile owner is gone.
    #[error("source lease liveness is unverified at {path}: {reason}")]
    LivenessUnverified {
        /// Database pathname containing the active row.
        path: PathBuf,
        /// Stable bounded reason for refusing takeover.
        reason: &'static str,
    },
    /// The retained owner row was released or superseded.
    #[error("source lease owner is no longer current at {path}")]
    Superseded {
        /// Database pathname containing the stale row.
        path: PathBuf,
    },
    /// The current owner lease has expired before a transaction boundary.
    #[error("source lease expired at {path}")]
    Expired {
        /// Database pathname containing the expired row.
        path: PathBuf,
    },
    /// A writable source database cannot proceed without a retained physical-source lease.
    #[error("source database has no retained physical-source lease")]
    Required,
    /// A source database cannot release its lease while a snapshot fence still retains it.
    #[error("source database has live snapshot fences and cannot release its lease")]
    SnapshotsActive,
    /// SQLite returned a bounded busy/locked outcome.
    #[error("source lease database is busy")]
    Busy,
    /// SQLite returned an unexpected error while managing the lease.
    #[error("source lease SQLite operation failed: {0}")]
    Sql(#[source] rusqlite::Error),
    /// A filesystem operation failed while opening or validating the source sidecar.
    #[error("source lease filesystem operation failed at {path}: {source}")]
    Io {
        /// Sidecar path involved in the failed operation.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// SQLite could not be configured with the required synchronous policy.
    #[error("source lease database did not accept synchronous=EXTRA at {path}")]
    SynchronousUnavailable {
        /// Database pathname whose policy was rejected.
        path: PathBuf,
    },
    /// The host does not expose a verified nonblocking sidecar lock primitive.
    #[error("source lease locking is unsupported on this platform: {path}")]
    Unsupported {
        /// Sidecar path that would have been locked.
        path: PathBuf,
    },
    /// The clock could not provide a nonnegative Unix timestamp.
    #[error("source lease clock is unavailable")]
    ClockUnavailable,
    /// The control connection could not be borrowed safely.
    #[error("source lease control connection is unavailable")]
    ControlUnavailable,
}

/// A typed failure from an immutable source-publication fence.
///
/// A fence only carries a weak reference to the live source lease. It can therefore validate a
/// queued result without keeping the physical-source writer alive after the source is closed.
/// The owner identity and epoch captured by the fence are compared with both the retained lease
/// and its durable row; creating or validating a fence does not grant database or filesystem
/// authority.
#[derive(Debug, Error)]
pub enum SourcePublicationFenceError {
    /// The source lease was dropped, released, or replaced, so this result can no longer be
    /// published.
    #[error("source publication fence lease is no longer alive")]
    LeaseDropped,
    /// The durable owner identity no longer matches the result's immutable fence.
    #[error("source publication fence owner identity changed")]
    OwnerChanged,
    /// The durable source epoch no longer matches the result's immutable fence.
    #[error("source publication fence epoch changed")]
    EpochChanged,
    /// The fence belongs to a different retained source lease than the receiving operation.
    #[error("source publication fence does not belong to the retained source lease")]
    LeaseMismatch,
    /// The retained source ownership proof failed while validating the fence.
    #[error(transparent)]
    Lease(#[from] SourceLeaseError),
}

struct LeaseRow {
    lease_schema_version: i64,
    media_root_identity: String,
    database_root_identity: String,
    database_identity: String,
    source_lock_identity: String,
    profile_root_identity: String,
    profile_lock_identity: String,
    owner_identity: String,
    lease_epoch: i64,
    acquired_at: i64,
    expires_at: i64,
    released_at: Option<i64>,
    state: String,
}

/// A source sidecar lock retained while a profile-owned source database is open.
pub(crate) struct SourceLeaseReservation {
    database_root: Arc<BoundDatabaseRoot>,
    media_root: Arc<BoundDatabaseRoot>,
    lock_name: PathBuf,
    lock_path: PathBuf,
    lock_identity: String,
    lock_file: File,
    fresh_lock: bool,
}

/// Durable source ownership retained by a source connection and any snapshot fences.
///
/// The control connection is opened through the same retained database binding as the ordinary
/// source connection. It exists so final release never needs to reopen a pathname after the
/// ordinary connection has been moved or dropped.
pub(crate) struct SourceLease {
    control_connection: Mutex<Connection>,
    database_root: Arc<BoundDatabaseRoot>,
    media_root: Arc<BoundDatabaseRoot>,
    database_file_binding: Arc<DatabaseFileBinding>,
    lock_name: PathBuf,
    lock_path: PathBuf,
    lock_identity: String,
    lock_file: File,
    owner_identity: String,
    lease_epoch: i64,
    publication_liveness: Arc<PublicationLiveness>,
    profile_guard: WritableProfileGuard,
}

/// Immutable proof that a source result belongs to one live owner and source epoch.
///
/// The fence retains only an in-memory publication-generation token and a weak source lease, so
/// retaining it cannot keep a physical-source lease or its SQLite control connection alive. Call
/// [`Self::validate`] immediately before publishing a source-scoped result. Validation performs
/// read-only checks through the retained control connection and never renews a lease or reopens a
/// database pathname.
#[derive(Clone)]
pub struct SourcePublicationFence {
    owner_identity: String,
    lease_epoch: i64,
    source_lease: Weak<SourceLease>,
    publication_liveness: Arc<PublicationLiveness>,
}

struct PublicationLiveness {
    owner_identity: String,
    lease_epoch: i64,
    live: AtomicBool,
}

impl PublicationLiveness {
    fn new(owner_identity: String, lease_epoch: i64) -> Self {
        Self {
            owner_identity,
            lease_epoch,
            live: AtomicBool::new(true),
        }
    }
}

impl std::fmt::Debug for SourcePublicationFence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SourcePublicationFence")
            .field("owner_identity", &self.owner_identity)
            .field("lease_epoch", &self.lease_epoch)
            .finish_non_exhaustive()
    }
}

impl SourcePublicationFence {
    /// Return the durable lease epoch captured by this fence.
    pub(crate) const fn lease_epoch(&self) -> i64 {
        self.lease_epoch
    }

    /// Return whether two fences identify the same retained source owner and epoch.
    pub(crate) fn same_claim_identity(&self, other: &Self) -> bool {
        self.owner_identity == other.owner_identity
            && self.lease_epoch == other.lease_epoch
            && self.source_lease.ptr_eq(&other.source_lease)
    }

    /// Return whether this fence belongs to the receiving retained source lease.
    pub(crate) fn matches_retained_lease(&self, source_lease: &Arc<SourceLease>) -> bool {
        self.owner_identity == source_lease.owner_identity
            && self.lease_epoch == source_lease.lease_epoch
            && self.source_lease.ptr_eq(&Arc::downgrade(source_lease))
    }

    /// Return whether the retained source owner is still live for this fence's generation.
    ///
    /// This is a cheap in-memory check for UI delivery boundaries. It reads only the retained
    /// publication-generation token and immutable owner/epoch values; it never upgrades the weak
    /// source lease and performs no SQLite, filesystem, or mutex I/O. It is not writable authority
    /// or full filesystem qualification; background publishers must continue to call
    /// [`Self::validate`].
    pub fn is_live(&self) -> bool {
        self.publication_liveness.live.load(Ordering::Acquire)
            && self.publication_liveness.owner_identity == self.owner_identity
            && self.publication_liveness.lease_epoch == self.lease_epoch
    }

    /// Validate the immutable owner/epoch against the retained lease and durable source state.
    ///
    /// This check is read-only and does not renew the lease. A successful result means the fence
    /// was still current at the end of the validation call; callers must perform it at their
    /// publication boundary and retain the existing lifecycle/revision checks as well.
    pub fn validate(&self) -> Result<(), SourcePublicationFenceError> {
        let source_lease = self
            .source_lease
            .upgrade()
            .ok_or(SourcePublicationFenceError::LeaseDropped)?;
        if !self.is_live() {
            return Err(SourcePublicationFenceError::LeaseDropped);
        }
        let result =
            source_lease.validate_publication_fence(&self.owner_identity, self.lease_epoch);
        if !self.is_live() {
            return Err(SourcePublicationFenceError::LeaseDropped);
        }
        result
    }
}

impl SourceLeaseReservation {
    /// Acquire the source sidecar lock after profile/root proof and before SQLite opens.
    pub(crate) fn acquire(
        database_root: Arc<BoundDatabaseRoot>,
        media_root: Arc<BoundDatabaseRoot>,
        profile_guard: &WritableProfileGuard,
    ) -> Result<Self, SourceLeaseError> {
        validate_database_root(&database_root, profile_guard)?;
        media_root
            .validate_profile_guard(profile_guard)
            .map_err(map_profile_error)?;
        let lock_name = PathBuf::from(SOURCE_LEASE_LOCK_FILE_NAME);
        let lock_path = database_root.path().join(&lock_name);
        let (lock_file, fresh_lock) = open_lock_file(&database_root, &lock_name, &lock_path)?;
        lock_file_is_regular(&lock_file, &lock_path)?;
        acquire_nonblocking_lock(&lock_file, &lock_path)?;
        let lock_identity =
            stable_filesystem_identity_from_open_file(&lock_file).ok_or_else(|| {
                SourceLeaseError::SidecarReplaced {
                    path: lock_path.clone(),
                }
            })?;
        let reservation = Self {
            database_root,
            media_root,
            lock_name,
            lock_path,
            lock_identity,
            lock_file,
            fresh_lock,
        };
        reservation.validate_profile(profile_guard)?;
        Ok(reservation)
    }

    /// Revalidate the sidecar/root before SQLite opens or lease-table effects occur.
    pub(crate) fn validate_before_open(
        &self,
        profile_guard: &WritableProfileGuard,
        database_file_binding: &DatabaseFileBinding,
    ) -> Result<(), SourceLeaseError> {
        self.validate_profile(profile_guard)?;
        database_file_binding
            .validate_path()
            .map_err(map_database_error)?;
        Ok(())
    }

    /// Whether this reservation created the sidecar pathname during acquisition.
    pub(crate) fn is_fresh(&self) -> bool {
        self.fresh_lock
    }

    /// Bootstrap or advance the durable lease row while retaining all authorized handles.
    pub(crate) fn bootstrap(
        self,
        profile_guard: &WritableProfileGuard,
        database_file_binding: Arc<DatabaseFileBinding>,
        created: bool,
    ) -> Result<Arc<SourceLease>, SourceLeaseError> {
        self.validate_before_open(profile_guard, &database_file_binding)?;
        let db_path = database_file_binding.path().to_path_buf();
        let mut control = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(map_sql_error)?;
        database_file_binding
            .validate(&control)
            .map_err(map_database_error)?;
        self.validate_profile(profile_guard)?;
        control
            .busy_timeout(Duration::from_millis(250))
            .map_err(map_sql_error)?;
        control
            .execute_batch("PRAGMA synchronous=EXTRA; PRAGMA foreign_keys=ON;")
            .map_err(map_sql_error)?;
        let synchronous = control
            .query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))
            .map_err(map_sql_error)?;
        if synchronous != 3 {
            return Err(SourceLeaseError::SynchronousUnavailable { path: db_path });
        }
        #[cfg(target_os = "macos")]
        {
            control
                .execute_batch("PRAGMA fullfsync=ON;")
                .map_err(map_sql_error)?;
            let fullfsync = control
                .query_row("PRAGMA fullfsync", [], |row| row.get::<_, i64>(0))
                .map_err(map_sql_error)?;
            if fullfsync != 1 {
                return Err(SourceLeaseError::SynchronousUnavailable { path: db_path });
            }
        }
        self.validate_profile(profile_guard)?;
        database_file_binding
            .validate(&control)
            .map_err(map_database_error)?;
        let owner_identity = Uuid::new_v4().to_string();
        let epoch = bootstrap_lease_row(
            &mut control,
            &self,
            &self.media_root,
            profile_guard,
            &database_file_binding,
            created,
            &owner_identity,
        )?;
        let profile_guard = profile_guard
            .try_clone()
            .map_err(SourceLeaseError::ProfileOwnership)?;
        let publication_liveness =
            Arc::new(PublicationLiveness::new(owner_identity.clone(), epoch));
        Ok(Arc::new(SourceLease {
            control_connection: Mutex::new(control),
            database_root: self.database_root,
            media_root: self.media_root,
            database_file_binding,
            lock_name: self.lock_name,
            lock_path: self.lock_path,
            lock_identity: self.lock_identity,
            lock_file: self.lock_file,
            owner_identity,
            lease_epoch: epoch,
            publication_liveness,
            profile_guard,
        }))
    }

    fn validate_profile(
        &self,
        profile_guard: &WritableProfileGuard,
    ) -> Result<(), SourceLeaseError> {
        profile_guard
            .validate_current()
            .map_err(SourceLeaseError::ProfileOwnership)?;
        validate_database_root(&self.database_root, profile_guard)?;
        self.media_root
            .validate_profile_guard(profile_guard)
            .map_err(map_profile_error)?;
        self.validate_lock_identity()
    }

    fn validate_lock_identity(&self) -> Result<(), SourceLeaseError> {
        lock_file_is_regular(&self.lock_file, &self.lock_path)?;
        let retained_identity = stable_filesystem_identity_from_open_file(&self.lock_file)
            .ok_or_else(|| SourceLeaseError::SidecarReplaced {
                path: self.lock_path.clone(),
            })?;
        if retained_identity != self.lock_identity {
            return Err(SourceLeaseError::SidecarReplaced {
                path: self.lock_path.clone(),
            });
        }
        let current = self
            .database_root
            .open_file_nofollow(&self.lock_name, false)
            .map_err(|source| {
                if source.kind() == io::ErrorKind::NotFound {
                    SourceLeaseError::SidecarReplaced {
                        path: self.lock_path.clone(),
                    }
                } else {
                    map_lock_io(&self.lock_path, source)
                }
            })?;
        lock_file_is_regular(&current, &self.lock_path)?;
        let current_identity =
            stable_filesystem_identity_from_open_file(&current).ok_or_else(|| {
                SourceLeaseError::SidecarReplaced {
                    path: self.lock_path.clone(),
                }
            })?;
        if current_identity != self.lock_identity {
            return Err(SourceLeaseError::SidecarReplaced {
                path: self.lock_path.clone(),
            });
        }
        Ok(())
    }
}

impl SourceLease {
    /// Mint a weak publication fence for this retained source lease.
    pub(crate) fn publication_fence(self: &Arc<Self>) -> SourcePublicationFence {
        SourcePublicationFence {
            owner_identity: self.owner_identity.clone(),
            lease_epoch: self.lease_epoch,
            source_lease: Arc::downgrade(self),
            publication_liveness: Arc::clone(&self.publication_liveness),
        }
    }

    /// Return the durable epoch held by this source owner.
    pub(crate) fn epoch(&self) -> i64 {
        self.lease_epoch
    }

    /// Return the control connection's SQLite change count for ownership tests.
    #[cfg(test)]
    pub(crate) fn control_total_changes_for_tests(&self) -> Result<u64, SourceLeaseError> {
        let control = self
            .control_connection
            .lock()
            .map_err(|_| SourceLeaseError::ControlUnavailable)?;
        Ok(control.total_changes())
    }

    /// Return the retained media-root capability used when this lease was acquired.
    pub(crate) fn media_root_binding(&self) -> Arc<BoundDatabaseRoot> {
        Arc::clone(&self.media_root)
    }

    /// Return the retained database namespace used by the ordinary source connection.
    pub(crate) fn database_root_binding(&self) -> Arc<BoundDatabaseRoot> {
        Arc::clone(&self.database_root)
    }

    /// Return whether this lease belongs to a profile-child database namespace.
    pub(crate) fn is_profile_child_namespace(&self) -> bool {
        self.database_root.is_profile_namespace()
    }

    /// Replace the retained media-root identity for an explicit catalog handoff.
    ///
    /// The database namespace, database-file identity, sidecar identity, and profile owner stay
    /// fixed. The source sidecar lock remains held while the authoritative row advances to a new
    /// epoch and owner identity, so another process cannot observe a takeover gap. This operation
    /// is deliberately limited to profile-child databases; external source leases must use the
    /// ordinary source opener and its explicit root proof.
    pub(crate) fn retarget_catalog_root(
        &mut self,
        new_media_root: Arc<BoundDatabaseRoot>,
        profile_guard: &WritableProfileGuard,
    ) -> Result<(), SourceLeaseError> {
        if !self.is_profile_child_namespace() {
            return Err(SourceLeaseError::Unsupported {
                path: self.database_root.path().to_path_buf(),
            });
        }

        // Validate both the owner supplied by the caller and the clone retained by this lease.
        // The latter is the capability that must survive the consuming handoff.
        self.profile_guard
            .validate_current()
            .map_err(SourceLeaseError::ProfileOwnership)?;
        validate_database_root(&self.database_root, profile_guard)?;
        validate_database_root(&self.database_root, &self.profile_guard)?;
        self.media_root
            .validate_profile_guard(profile_guard)
            .map_err(map_profile_error)?;
        new_media_root
            .validate_profile_guard(profile_guard)
            .map_err(map_profile_error)?;
        self.validate_lock_identity()?;

        let mut control = self
            .control_connection
            .lock()
            .map_err(|_| SourceLeaseError::ControlUnavailable)?;
        self.validate_control_connection(&control)?;
        let old_row = self.read_current_row(&control)?;
        self.validate_owned_row(&old_row, false)?;
        let now = lease_now()?;
        if old_row.expires_at <= now {
            return Err(SourceLeaseError::Expired {
                path: self.database_file_binding.path().to_path_buf(),
            });
        }
        let next_epoch = self
            .lease_epoch
            .checked_add(1)
            .ok_or(SourceLeaseError::InvalidRow {
                path: self.database_file_binding.path().to_path_buf(),
                reason: "lease epoch overflow during catalog handoff",
            })?;
        let next_owner_identity = Uuid::new_v4().to_string();
        let expires_at = now
            .checked_add(LEASE_DURATION_SECONDS)
            .ok_or(SourceLeaseError::ClockUnavailable)?;
        self.publication_liveness
            .live
            .store(false, Ordering::Release);
        let transaction = control
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql_error)?;
        let row = self.read_current_row(&transaction)?;
        self.validate_owned_row(&row, false)?;
        new_media_root
            .validate_profile_guard(profile_guard)
            .map_err(map_profile_error)?;
        validate_database_root(&self.database_root, profile_guard)?;
        self.database_file_binding
            .validate(&transaction)
            .map_err(map_database_error)?;
        transaction
            .execute(
                "UPDATE source_lease
                 SET media_root_identity = ?1,
                     owner_identity = ?2,
                     lease_epoch = ?3,
                     acquired_at = ?4,
                     expires_at = ?5,
                     released_at = NULL,
                     state = 'active'
                 WHERE singleton = 1
                   AND media_root_identity = ?6
                   AND owner_identity = ?7
                   AND lease_epoch = ?8
                   AND state = 'active'",
                params![
                    new_media_root.identity(),
                    next_owner_identity,
                    next_epoch,
                    now,
                    expires_at,
                    self.media_root.identity(),
                    self.owner_identity,
                    self.lease_epoch,
                ],
            )
            .map_err(map_sql_error)?;
        if transaction.changes() != 1 {
            return Err(SourceLeaseError::Superseded {
                path: self.database_file_binding.path().to_path_buf(),
            });
        }

        // Validate the updated row and every retained handle before allowing this transaction to
        // commit. Until commit, any failure above rolls the row update back and leaves the old
        // in-memory lease state untouched.
        let updated = self.read_current_row(&transaction)?;
        validate_lease_row(&updated, self.database_file_binding.path())?;
        validate_row_source_identities(
            &updated,
            new_media_root.identity(),
            self.database_root.identity(),
            self.database_file_binding.identity(),
            &self.lock_identity,
            new_media_root.path(),
            self.database_root.path(),
            self.database_file_binding.path(),
            &self.lock_path,
        )?;
        if updated.profile_root_identity != self.profile_guard.profile_root_identity()
            || updated.profile_lock_identity != self.profile_guard.profile_lock_identity()
            || updated.owner_identity != next_owner_identity
            || updated.lease_epoch != next_epoch
            || updated.state != "active"
            || updated.released_at.is_some()
        {
            return Err(SourceLeaseError::InvalidRow {
                path: self.database_file_binding.path().to_path_buf(),
                reason: "catalog handoff row did not retain the expected owner state",
            });
        }
        new_media_root
            .validate_profile_guard(profile_guard)
            .map_err(map_profile_error)?;
        validate_database_root(&self.database_root, profile_guard)?;
        self.media_root
            .validate_profile_guard(profile_guard)
            .map_err(map_profile_error)?;
        self.validate_lock_identity()?;
        self.database_file_binding
            .validate(&transaction)
            .map_err(map_database_error)?;
        transaction.commit().map_err(map_sql_error)?;
        drop(control);

        self.media_root = new_media_root;
        self.owner_identity = next_owner_identity;
        self.lease_epoch = next_epoch;
        self.publication_liveness = Arc::new(PublicationLiveness::new(
            self.owner_identity.clone(),
            self.lease_epoch,
        ));
        Ok(())
    }

    /// Revalidate the retained media-root identity before reopening or mutating the source.
    pub(crate) fn validate_media_root(
        &self,
        profile_guard: &WritableProfileGuard,
    ) -> Result<(), SourceLeaseError> {
        self.media_root
            .validate_profile_guard(profile_guard)
            .map_err(map_profile_error)
    }

    /// Validate the retained source ownership before reopening a connection for a snapshot.
    pub(crate) fn validate_before_open(
        &self,
        profile_guard: &WritableProfileGuard,
        database_file_binding: &DatabaseFileBinding,
    ) -> Result<(), SourceLeaseError> {
        self.profile_guard
            .validate_current()
            .map_err(SourceLeaseError::ProfileOwnership)?;
        validate_database_root(&self.database_root, profile_guard)?;
        validate_database_root(&self.database_root, &self.profile_guard)?;
        self.media_root
            .validate_profile_guard(profile_guard)
            .map_err(map_profile_error)?;
        self.media_root
            .validate_profile_guard(&self.profile_guard)
            .map_err(map_profile_error)?;
        if database_file_binding.identity() != self.database_file_binding.identity() {
            return Err(SourceLeaseError::DatabaseIdentity(
                SourceDatabaseIdentityError::Replaced {
                    path: database_file_binding.path().to_path_buf(),
                },
            ));
        }
        database_file_binding
            .validate_path()
            .map_err(map_database_error)?;
        self.validate_lock_identity()
    }

    /// Validate identities and renew the owner row before an autocommit mutation begins.
    pub(crate) fn prepare_for_mutation(
        &self,
        connection: &Connection,
    ) -> Result<(), SourceLeaseError> {
        let row = self.validate_current_without_expiry(connection)?;
        if !connection.is_autocommit() {
            if row.expires_at <= lease_now()? {
                return Err(SourceLeaseError::Expired {
                    path: self.database_file_binding.path().to_path_buf(),
                });
            }
            return Ok(());
        }
        let now = lease_now()?;
        let renewal_cutoff = now
            .checked_add(LEASE_RENEWAL_THRESHOLD_SECONDS)
            .ok_or(SourceLeaseError::ClockUnavailable)?;
        // A wall clock that moved backwards must never shorten the durable expiry. The retained
        // OS lock still proves this owner is live, so a healthy lease can continue without an
        // unnecessary synchronous control-connection write.
        if now < row.acquired_at || row.expires_at > renewal_cutoff {
            return Ok(());
        }
        let mut control = self
            .control_connection
            .lock()
            .map_err(|_| SourceLeaseError::ControlUnavailable)?;
        self.validate_control_connection(&control)?;
        let expires_at = now
            .checked_add(LEASE_DURATION_SECONDS)
            .ok_or(SourceLeaseError::ClockUnavailable)?;
        let tx = control
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql_error)?;
        let row = self.read_current_row(&tx)?;
        self.validate_owned_row(&row, false)?;
        tx.execute(
            "UPDATE source_lease
             SET acquired_at = acquired_at, expires_at = ?1
             WHERE singleton = 1 AND owner_identity = ?2 AND lease_epoch = ?3 AND state = 'active'",
            params![expires_at, self.owner_identity, self.epoch()],
        )
        .map_err(map_sql_error)?;
        if tx.changes() != 1 {
            return Err(SourceLeaseError::Superseded {
                path: self.database_file_binding.path().to_path_buf(),
            });
        }
        self.validate_authority_handles()?;
        self.database_file_binding
            .validate(&tx)
            .map_err(map_database_error)?;
        tx.commit().map_err(map_sql_error)?;
        drop(control);
        self.validate_current(connection)
    }

    /// Validate retained identities and the durable active owner row.
    pub(crate) fn validate_current(&self, connection: &Connection) -> Result<(), SourceLeaseError> {
        let row = self.validate_current_without_expiry(connection)?;
        let now = lease_now()?;
        if row.expires_at <= now {
            return Err(SourceLeaseError::Expired {
                path: self.database_file_binding.path().to_path_buf(),
            });
        }
        Ok(())
    }

    fn validate_publication_fence(
        &self,
        expected_owner_identity: &str,
        expected_epoch: i64,
    ) -> Result<(), SourcePublicationFenceError> {
        if self.owner_identity != expected_owner_identity {
            return Err(SourcePublicationFenceError::OwnerChanged);
        }
        if self.lease_epoch != expected_epoch {
            return Err(SourcePublicationFenceError::EpochChanged);
        }

        let control = self.control_connection.lock().map_err(|_| {
            SourcePublicationFenceError::Lease(SourceLeaseError::ControlUnavailable)
        })?;
        self.validate_control_connection(&control)
            .map_err(SourcePublicationFenceError::Lease)?;
        let row = self
            .read_current_row(&control)
            .map_err(SourcePublicationFenceError::Lease)?;
        if row.owner_identity != expected_owner_identity {
            return Err(SourcePublicationFenceError::OwnerChanged);
        }
        if row.lease_epoch != expected_epoch {
            return Err(SourcePublicationFenceError::EpochChanged);
        }
        self.validate_owned_row(&row, false)
            .map_err(SourcePublicationFenceError::Lease)?;
        let now = lease_now().map_err(SourcePublicationFenceError::Lease)?;
        if row.expires_at <= now {
            return Err(SourcePublicationFenceError::Lease(
                SourceLeaseError::Expired {
                    path: self.database_file_binding.path().to_path_buf(),
                },
            ));
        }
        self.validate_control_connection(&control)
            .map_err(SourcePublicationFenceError::Lease)?;
        Ok(())
    }

    /// Mark this owner released using its retained control connection.
    pub(crate) fn release(&self) -> Result<(), SourceLeaseError> {
        self.publication_liveness
            .live
            .store(false, Ordering::Release);
        let mut control = self
            .control_connection
            .lock()
            .map_err(|_| SourceLeaseError::ControlUnavailable)?;
        self.validate_control_connection(&control)?;
        let now = lease_now()?;
        let tx = control
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql_error)?;
        let row = self.read_current_row(&tx)?;
        self.validate_owned_row(&row, true)?;
        if row.state == "released" {
            if row.owner_identity != self.owner_identity || row.lease_epoch != self.lease_epoch {
                return Err(SourceLeaseError::Superseded {
                    path: self.database_file_binding.path().to_path_buf(),
                });
            }
            tx.rollback().map_err(map_sql_error)?;
            return Ok(());
        }
        tx.execute(
            "UPDATE source_lease
             SET state = 'released', released_at = ?1
             WHERE singleton = 1 AND owner_identity = ?2 AND lease_epoch = ?3 AND state = 'active'",
            params![now, self.owner_identity, self.epoch()],
        )
        .map_err(map_sql_error)?;
        if tx.changes() != 1 {
            return Err(SourceLeaseError::Superseded {
                path: self.database_file_binding.path().to_path_buf(),
            });
        }
        self.validate_control_connection(&tx)?;
        tx.commit().map_err(map_sql_error)
    }

    fn validate_current_without_expiry(
        &self,
        connection: &Connection,
    ) -> Result<LeaseRow, SourceLeaseError> {
        self.validate_authority_handles()?;
        self.database_file_binding
            .validate(connection)
            .map_err(map_database_error)?;
        let row = self.read_current_row(connection)?;
        self.validate_owned_row(&row, false)?;
        Ok(row)
    }

    fn validate_control_connection(&self, connection: &Connection) -> Result<(), SourceLeaseError> {
        self.validate_authority_handles()?;
        self.database_file_binding
            .validate(connection)
            .map_err(map_database_error)?;
        Ok(())
    }

    fn validate_authority_handles(&self) -> Result<(), SourceLeaseError> {
        self.profile_guard
            .validate_current()
            .map_err(SourceLeaseError::ProfileOwnership)?;
        validate_database_root(&self.database_root, &self.profile_guard)?;
        self.media_root
            .validate_profile_guard(&self.profile_guard)
            .map_err(map_profile_error)?;
        self.validate_lock_identity()
    }

    fn validate_lock_identity(&self) -> Result<(), SourceLeaseError> {
        lock_file_is_regular(&self.lock_file, &self.lock_path)?;
        let retained_identity = stable_filesystem_identity_from_open_file(&self.lock_file)
            .ok_or_else(|| SourceLeaseError::SidecarReplaced {
                path: self.lock_path.clone(),
            })?;
        if retained_identity != self.lock_identity {
            return Err(SourceLeaseError::SidecarReplaced {
                path: self.lock_path.clone(),
            });
        }
        let current = self
            .database_root
            .open_file_nofollow(&self.lock_name, false)
            .map_err(|source| {
                if source.kind() == io::ErrorKind::NotFound {
                    SourceLeaseError::SidecarReplaced {
                        path: self.lock_path.clone(),
                    }
                } else {
                    map_lock_io(&self.lock_path, source)
                }
            })?;
        lock_file_is_regular(&current, &self.lock_path)?;
        let current_identity =
            stable_filesystem_identity_from_open_file(&current).ok_or_else(|| {
                SourceLeaseError::SidecarReplaced {
                    path: self.lock_path.clone(),
                }
            })?;
        if current_identity != self.lock_identity {
            return Err(SourceLeaseError::SidecarReplaced {
                path: self.lock_path.clone(),
            });
        }
        Ok(())
    }

    fn read_current_row(&self, connection: &Connection) -> Result<LeaseRow, SourceLeaseError> {
        validate_lease_table_schema(connection, self.database_file_binding.path())?;
        read_lease_row(connection, self.database_file_binding.path())?.ok_or_else(|| {
            SourceLeaseError::SchemaUnavailable {
                path: self.database_file_binding.path().to_path_buf(),
                reason: "source lease row is missing",
            }
        })
    }

    fn validate_owned_row(
        &self,
        row: &LeaseRow,
        allow_released: bool,
    ) -> Result<(), SourceLeaseError> {
        validate_lease_row(row, self.database_file_binding.path())?;
        validate_row_source_identities(
            row,
            self.media_root.identity(),
            self.database_root.identity(),
            self.database_file_binding.identity(),
            &self.lock_identity,
            self.media_root.path(),
            self.database_root.path(),
            self.database_file_binding.path(),
            &self.lock_path,
        )?;
        if row.state == "active"
            && (row.profile_root_identity != self.profile_guard.profile_root_identity()
                || row.profile_lock_identity != self.profile_guard.profile_lock_identity())
        {
            return Err(SourceLeaseError::LivenessUnverified {
                path: self.database_file_binding.path().to_path_buf(),
                reason: "active lease row belongs to another profile owner",
            });
        }
        if !allow_released
            && (row.state != "active"
                || row.owner_identity != self.owner_identity
                || row.lease_epoch != self.epoch())
        {
            return Err(SourceLeaseError::Superseded {
                path: self.database_file_binding.path().to_path_buf(),
            });
        }
        Ok(())
    }
}

impl Drop for SourceLease {
    fn drop(&mut self) {
        self.publication_liveness
            .live
            .store(false, Ordering::Release);
        // This uses the retained authorized control connection. It never reopens a pathname.
        // Explicit release remains the fallible shutdown API; a crash or failed cleanup leaves an
        // active durable row for conservative recovery after the sidecar lock is released.
        if let Err(error) = self.release() {
            tracing::warn!(
                path = %self.database_file_binding.path().display(),
                error = %error,
                "Failed to release physical source lease during drop"
            );
        }
    }
}

fn bootstrap_lease_row(
    control: &mut Connection,
    reservation: &SourceLeaseReservation,
    media_root: &BoundDatabaseRoot,
    profile_guard: &WritableProfileGuard,
    database_file_binding: &DatabaseFileBinding,
    created: bool,
    owner_identity: &str,
) -> Result<i64, SourceLeaseError> {
    let path = database_file_binding.path();
    reservation.validate_profile(profile_guard)?;
    database_file_binding
        .validate(control)
        .map_err(map_database_error)?;
    preflight_database_schema(control, path, created)?;
    let table_exists = table_exists(control)?;
    if table_exists {
        validate_lease_table_schema(control, path)?;
    } else {
        validate_no_lease_triggers(control, path)?;
        if !reservation.is_fresh() {
            return Err(SourceLeaseError::LeaseStateUnavailable {
                path: path.to_path_buf(),
                reason: "source lease table is missing behind a preexisting lease sidecar",
            });
        }
    }
    if table_exists {
        if read_lease_row(control, path)?.is_none() {
            return Err(SourceLeaseError::LeaseStateUnavailable {
                path: path.to_path_buf(),
                reason: "source lease table has no singleton row",
            });
        }
    }
    let tx = control
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sql_error)?;
    if !table_exists {
        tx.execute_batch(LEASE_TABLE_SQL).map_err(map_sql_error)?;
    }
    validate_lease_table_schema(&tx, path)?;
    let prior = read_lease_row(&tx, path)?;
    if table_exists && prior.is_none() {
        return Err(SourceLeaseError::LeaseStateUnavailable {
            path: path.to_path_buf(),
            reason: "source lease singleton row disappeared before acquisition",
        });
    }
    let now = lease_now()?;
    let epoch = next_epoch(
        prior.as_ref(),
        reservation,
        media_root,
        profile_guard,
        database_file_binding,
        now,
    )?;
    let expires_at = now
        .checked_add(LEASE_DURATION_SECONDS)
        .ok_or(SourceLeaseError::ClockUnavailable)?;
    let values = [
        media_root.identity(),
        reservation.database_root.identity(),
        database_file_binding.identity(),
        reservation.lock_identity.as_str(),
        profile_guard.profile_root_identity(),
        profile_guard.profile_lock_identity(),
        owner_identity,
    ];
    if let Some(_) = prior {
        tx.execute(
            "UPDATE source_lease
             SET media_root_identity = ?1,
                 database_root_identity = ?2,
                 database_identity = ?3,
                 source_lock_identity = ?4,
                 profile_root_identity = ?5,
                 profile_lock_identity = ?6,
                 owner_identity = ?7,
                 lease_epoch = ?8,
                 acquired_at = ?9,
                 expires_at = ?10,
                 released_at = NULL,
                 state = 'active'
             WHERE singleton = 1",
            params![
                values[0], values[1], values[2], values[3], values[4], values[5], values[6], epoch,
                now, expires_at
            ],
        )
        .map_err(map_sql_error)?;
    } else {
        tx.execute(
            "INSERT INTO source_lease (
                 singleton, lease_schema_version, media_root_identity, database_root_identity,
                 database_identity,
                 source_lock_identity, profile_root_identity, profile_lock_identity,
                 owner_identity, lease_epoch, acquired_at, expires_at, released_at, state
             ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, 'active')",
            params![
                SOURCE_LEASE_TABLE_SCHEMA_VERSION,
                values[0],
                values[1],
                values[2],
                values[3],
                values[4],
                values[5],
                values[6],
                epoch,
                now,
                expires_at
            ],
        )
        .map_err(map_sql_error)?;
    }
    reservation.validate_profile(profile_guard)?;
    database_file_binding
        .validate(&tx)
        .map_err(map_database_error)?;
    validate_lease_row(
        &read_lease_row(&tx, path)?.ok_or(SourceLeaseError::SchemaUnavailable {
            path: path.to_path_buf(),
            reason: "lease row disappeared during bootstrap",
        })?,
        path,
    )?;
    tx.commit().map_err(map_sql_error)?;
    Ok(epoch)
}

fn next_epoch(
    prior: Option<&LeaseRow>,
    reservation: &SourceLeaseReservation,
    media_root: &BoundDatabaseRoot,
    profile_guard: &WritableProfileGuard,
    database_file_binding: &DatabaseFileBinding,
    now: i64,
) -> Result<i64, SourceLeaseError> {
    let Some(row) = prior else {
        return Ok(1);
    };
    validate_lease_row(row, database_file_binding.path())?;
    validate_row_source_identities(
        row,
        media_root.identity(),
        reservation.database_root.identity(),
        database_file_binding.identity(),
        &reservation.lock_identity,
        media_root.path(),
        reservation.database_root.path(),
        database_file_binding.path(),
        &reservation.lock_path,
    )?;
    if row.state == "released" {
        return row
            .lease_epoch
            .checked_add(1)
            .ok_or(SourceLeaseError::InvalidRow {
                path: database_file_binding.path().to_path_buf(),
                reason: "lease epoch overflow",
            });
    }
    if row.profile_root_identity != profile_guard.profile_root_identity()
        || row.profile_lock_identity != profile_guard.profile_lock_identity()
    {
        return Err(SourceLeaseError::LivenessUnverified {
            path: database_file_binding.path().to_path_buf(),
            reason: "expired lease belongs to another profile owner",
        });
    }
    if row.expires_at > now {
        return Err(SourceLeaseError::LeaseNotExpired {
            path: database_file_binding.path().to_path_buf(),
            expires_at: row.expires_at,
        });
    }
    row.lease_epoch
        .checked_add(1)
        .ok_or(SourceLeaseError::InvalidRow {
            path: database_file_binding.path().to_path_buf(),
            reason: "lease epoch overflow",
        })
}

fn validate_row_source_identities(
    row: &LeaseRow,
    media_root_identity: &str,
    database_root_identity: &str,
    database_identity: &str,
    source_lock_identity: &str,
    media_root_path: &Path,
    database_root_path: &Path,
    database_path: &Path,
    source_lock_path: &Path,
) -> Result<(), SourceLeaseError> {
    if row.media_root_identity != media_root_identity {
        return Err(SourceLeaseError::SourceRootReplaced {
            path: media_root_path.to_path_buf(),
        });
    }
    if row.database_root_identity != database_root_identity {
        return Err(SourceLeaseError::DatabaseRootReplaced {
            path: database_root_path.to_path_buf(),
        });
    }
    if row.database_identity != database_identity {
        return Err(SourceLeaseError::DatabaseIdentity(
            SourceDatabaseIdentityError::Replaced {
                path: database_path.to_path_buf(),
            },
        ));
    }
    if row.source_lock_identity != source_lock_identity {
        return Err(SourceLeaseError::SidecarReplaced {
            path: source_lock_path.to_path_buf(),
        });
    }
    Ok(())
}

fn preflight_database_schema(
    connection: &Connection,
    path: &Path,
    created: bool,
) -> Result<(), SourceLeaseError> {
    let user_version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(map_sql_error)?;
    if created {
        if user_version != 0 {
            return Err(SourceLeaseError::MigrationRequired {
                path: path.to_path_buf(),
                user_version,
            });
        }
        let object_count = connection
            .query_row(
                "SELECT COUNT(*) FROM (
                     SELECT 1 FROM sqlite_master
                     WHERE name NOT LIKE 'sqlite_%'
                     LIMIT 2
                 )",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sql_error)?;
        if object_count != 0 {
            return Err(SourceLeaseError::SchemaUnavailable {
                path: path.to_path_buf(),
                reason: "new source database schema was not empty",
            });
        }
        return Ok(());
    }
    if user_version != schema::SOURCE_DB_SCHEMA_VERSION {
        return Err(SourceLeaseError::MigrationRequired {
            path: path.to_path_buf(),
            user_version,
        });
    }
    for required_table in ["metadata", "wav_files"] {
        if !table_exists_named(connection, required_table)? {
            return Err(SourceLeaseError::SchemaUnavailable {
                path: path.to_path_buf(),
                reason: "current source schema is missing a required table",
            });
        }
    }
    validate_required_base_columns(connection, path)?;
    Ok(())
}

#[derive(Clone, Copy)]
struct RequiredBaseColumn {
    name: &'static str,
    declared_type: &'static str,
    not_null: i64,
    primary_key: i64,
}

const MAX_BASE_COLUMN_TYPE_BYTES: i64 = 64;
const REQUIRED_METADATA_COLUMNS: &[RequiredBaseColumn] = &[
    RequiredBaseColumn {
        name: "key",
        declared_type: "TEXT",
        not_null: 0,
        primary_key: 1,
    },
    RequiredBaseColumn {
        name: "value",
        declared_type: "TEXT",
        not_null: 1,
        primary_key: 0,
    },
];
const REQUIRED_WAV_FILES_COLUMNS: &[RequiredBaseColumn] = &[
    RequiredBaseColumn {
        name: "path",
        declared_type: "TEXT",
        not_null: 0,
        primary_key: 1,
    },
    RequiredBaseColumn {
        name: "file_size",
        declared_type: "INTEGER",
        not_null: 1,
        primary_key: 0,
    },
    RequiredBaseColumn {
        name: "modified_ns",
        declared_type: "INTEGER",
        not_null: 1,
        primary_key: 0,
    },
];

fn validate_required_base_columns(
    connection: &Connection,
    path: &Path,
) -> Result<(), SourceLeaseError> {
    for (table, columns) in [
        ("metadata", REQUIRED_METADATA_COLUMNS),
        ("wav_files", REQUIRED_WAV_FILES_COLUMNS),
    ] {
        for expected in columns.iter() {
            let actual = connection
                .query_row(
                    "SELECT CASE
                        WHEN typeof(type) = 'text'
                         AND length(CAST(type AS BLOB)) <= ?1
                        THEN type
                        ELSE NULL
                     END,
                     \"notnull\", pk
                     FROM pragma_table_info(?2)
                     WHERE name = ?3
                     LIMIT 1",
                    params![MAX_BASE_COLUMN_TYPE_BYTES, table, expected.name],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(map_sql_error)?;
            let Some((declared_type, not_null, primary_key)) = actual else {
                return Err(SourceLeaseError::SchemaUnavailable {
                    path: path.to_path_buf(),
                    reason: "current source schema is missing a required base column",
                });
            };
            let Some(declared_type) = declared_type else {
                return Err(SourceLeaseError::SchemaUnavailable {
                    path: path.to_path_buf(),
                    reason: "current source schema has an invalid required base column type",
                });
            };
            if !declared_type.eq_ignore_ascii_case(expected.declared_type)
                || not_null != expected.not_null
                || primary_key != expected.primary_key
            {
                return Err(SourceLeaseError::SchemaUnavailable {
                    path: path.to_path_buf(),
                    reason: "current source schema has an incompatible required base column",
                });
            }
        }
    }
    Ok(())
}

fn table_exists(connection: &Connection) -> Result<bool, SourceLeaseError> {
    table_exists_named(connection, SOURCE_LEASE_TABLE)
}

fn table_exists_named(connection: &Connection, name: &str) -> Result<bool, SourceLeaseError> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
        .map_err(map_sql_error)
}

fn validate_lease_table_schema(
    connection: &Connection,
    path: &Path,
) -> Result<(), SourceLeaseError> {
    const EXPECTED: &[(&str, &str, i64, i64)] = &[
        ("singleton", "INTEGER", 0, 1),
        ("lease_schema_version", "INTEGER", 1, 0),
        ("media_root_identity", "TEXT", 1, 0),
        ("database_root_identity", "TEXT", 1, 0),
        ("database_identity", "TEXT", 1, 0),
        ("source_lock_identity", "TEXT", 1, 0),
        ("profile_root_identity", "TEXT", 1, 0),
        ("profile_lock_identity", "TEXT", 1, 0),
        ("owner_identity", "TEXT", 1, 0),
        ("lease_epoch", "INTEGER", 1, 0),
        ("acquired_at", "INTEGER", 1, 0),
        ("expires_at", "INTEGER", 1, 0),
        ("released_at", "INTEGER", 0, 0),
        ("state", "TEXT", 1, 0),
    ];
    validate_no_lease_triggers(connection, path)?;
    let ddl = connection
        .query_row(
            "SELECT CASE
                WHEN typeof(sql) = 'text'
                 AND length(CAST(sql AS BLOB)) <= ?1
                THEN lower(sql)
                ELSE NULL
             END
             FROM sqlite_master
             WHERE type = 'table' AND name = ?2",
            params![MAX_LEASE_SCHEMA_SQL_BYTES, SOURCE_LEASE_TABLE],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(map_sql_error)?
        .flatten();
    let Some(ddl) = ddl else {
        return Err(SourceLeaseError::SchemaUnavailable {
            path: path.to_path_buf(),
            reason: "source lease table definition is missing or oversized",
        });
    };
    let normalized_ddl: String = ddl.split_whitespace().collect();
    let normalized_expected_ddl: String = LEASE_TABLE_SQL
        .to_ascii_lowercase()
        .split_whitespace()
        .collect();
    if normalized_ddl != normalized_expected_ddl {
        return Err(SourceLeaseError::SchemaUnavailable {
            path: path.to_path_buf(),
            reason: "source lease table definition is not the known shape",
        });
    }
    let invalid_names = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('source_lease')
             WHERE typeof(name) <> 'text'
                OR typeof(type) <> 'text'
                OR length(CAST(name AS BLOB)) > 64
                OR length(CAST(type AS BLOB)) > 64",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(map_sql_error)?;
    if invalid_names != 0 {
        return Err(SourceLeaseError::SchemaUnavailable {
            path: path.to_path_buf(),
            reason: "source lease table metadata contains oversized or invalid names",
        });
    }
    let column_count = connection
        .query_row(
            "SELECT COUNT(*) FROM (
                 SELECT 1 FROM pragma_table_info('source_lease') LIMIT ?1
             )",
            [i64::try_from(EXPECTED.len() + 1).unwrap_or(i64::MAX)],
            |row| row.get::<_, i64>(0),
        )
        .map_err(map_sql_error)?;
    if column_count != i64::try_from(EXPECTED.len()).unwrap_or(i64::MAX) {
        return Err(SourceLeaseError::SchemaUnavailable {
            path: path.to_path_buf(),
            reason: "source lease table has an unexpected number of columns",
        });
    }
    let mut statement = connection
        .prepare("PRAGMA table_info(source_lease)")
        .map_err(map_sql_error)?;
    let columns = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(map_sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sql_error)?;
    if columns.len() != EXPECTED.len()
        || columns.iter().zip(EXPECTED).any(|(actual, expected)| {
            actual.0 != expected.0
                || actual.1 != expected.1
                || actual.2 != expected.2
                || actual.3 != expected.3
        })
    {
        return Err(SourceLeaseError::SchemaUnavailable {
            path: path.to_path_buf(),
            reason: "source lease table columns do not match the known shape",
        });
    }
    Ok(())
}

fn validate_no_lease_triggers(
    connection: &Connection,
    path: &Path,
) -> Result<(), SourceLeaseError> {
    let trigger_count = connection
        .query_row(
            "SELECT COUNT(*) FROM (
                 SELECT 1 FROM sqlite_master
                 WHERE type = 'trigger'
                 LIMIT ?1
             )",
            [MAX_LEASE_TRIGGER_COUNT + 1],
            |row| row.get::<_, i64>(0),
        )
        .map_err(map_sql_error)?;
    if trigger_count > MAX_LEASE_TRIGGER_COUNT {
        return Err(SourceLeaseError::SchemaUnavailable {
            path: path.to_path_buf(),
            reason: "source database has too many triggers to inspect safely",
        });
    }
    let unsafe_triggers = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'trigger'
               AND (
                   tbl_name = ?1
                   OR instr(lower(COALESCE(CAST(sql AS TEXT), '')), ?2) > 0
                   OR typeof(sql) <> 'text'
                   OR length(CAST(sql AS BLOB)) > ?3
               )",
            params![
                SOURCE_LEASE_TABLE,
                SOURCE_LEASE_TABLE,
                MAX_LEASE_SCHEMA_SQL_BYTES
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(map_sql_error)?;
    if unsafe_triggers != 0 {
        return Err(SourceLeaseError::SchemaUnavailable {
            path: path.to_path_buf(),
            reason: "source lease table has an attached or referencing trigger",
        });
    }
    Ok(())
}

fn read_lease_row(
    connection: &Connection,
    path: &Path,
) -> Result<Option<LeaseRow>, SourceLeaseError> {
    let row_count = connection
        .query_row(
            "SELECT COUNT(*) FROM (
                 SELECT singleton FROM source_lease LIMIT 2
             )",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(map_sql_error)?;
    if row_count > 1 {
        return Err(SourceLeaseError::InvalidRow {
            path: path.to_path_buf(),
            reason: "source lease table contains more than one row",
        });
    }
    if row_count == 0 {
        return Ok(None);
    }
    let bounded = connection
        .query_row(
            "SELECT CASE WHEN
                typeof(singleton) = 'integer' AND singleton = 1
                AND typeof(lease_schema_version) = 'integer'
                AND typeof(media_root_identity) = 'text'
                AND length(CAST(media_root_identity AS BLOB)) BETWEEN 1 AND ?1
                AND typeof(database_root_identity) = 'text'
                AND length(CAST(database_root_identity AS BLOB)) BETWEEN 1 AND ?1
                AND typeof(database_identity) = 'text'
                AND length(CAST(database_identity AS BLOB)) BETWEEN 1 AND ?1
                AND typeof(source_lock_identity) = 'text'
                AND length(CAST(source_lock_identity AS BLOB)) BETWEEN 1 AND ?1
                AND typeof(profile_root_identity) = 'text'
                AND length(CAST(profile_root_identity AS BLOB)) BETWEEN 1 AND ?1
                AND typeof(profile_lock_identity) = 'text'
                AND length(CAST(profile_lock_identity AS BLOB)) BETWEEN 1 AND ?1
                AND typeof(owner_identity) = 'text'
                AND length(CAST(owner_identity AS BLOB)) BETWEEN 1 AND ?1
                AND typeof(lease_epoch) = 'integer'
                AND typeof(acquired_at) = 'integer'
                AND typeof(expires_at) = 'integer'
                AND (released_at IS NULL OR typeof(released_at) = 'integer')
                AND typeof(state) = 'text'
                AND length(CAST(state AS BLOB)) BETWEEN 1 AND 16
                THEN 1 ELSE 0 END
             FROM source_lease LIMIT 1",
            [MAX_LEASE_TEXT_BYTES],
            |row| row.get::<_, i64>(0),
        )
        .map_err(map_sql_error)?;
    if bounded != 1 {
        return Err(SourceLeaseError::InvalidRow {
            path: path.to_path_buf(),
            reason: "source lease row contains an invalid or oversized value",
        });
    }
    let result = connection.query_row(
        "SELECT lease_schema_version, media_root_identity, database_root_identity,
                database_identity, source_lock_identity, profile_root_identity,
                profile_lock_identity, owner_identity, lease_epoch, acquired_at, expires_at,
                released_at, state
         FROM source_lease WHERE singleton = 1",
        [],
        |row| {
            Ok(LeaseRow {
                lease_schema_version: row.get(0)?,
                media_root_identity: row.get(1)?,
                database_root_identity: row.get(2)?,
                database_identity: row.get(3)?,
                source_lock_identity: row.get(4)?,
                profile_root_identity: row.get(5)?,
                profile_lock_identity: row.get(6)?,
                owner_identity: row.get(7)?,
                lease_epoch: row.get(8)?,
                acquired_at: row.get(9)?,
                expires_at: row.get(10)?,
                released_at: row.get(11)?,
                state: row.get(12)?,
            })
        },
    );
    match result.optional() {
        Ok(row) => Ok(row),
        Err(error) if is_row_shape_error(&error) => Err(SourceLeaseError::InvalidRow {
            path: path.to_path_buf(),
            reason: "source lease row contains an unexpected SQLite value type",
        }),
        Err(error) => Err(map_sql_error(error)),
    }
}

fn validate_lease_row(row: &LeaseRow, path: &Path) -> Result<(), SourceLeaseError> {
    if row.lease_schema_version != SOURCE_LEASE_TABLE_SCHEMA_VERSION {
        return Err(SourceLeaseError::InvalidRow {
            path: path.to_path_buf(),
            reason: "source lease row uses an unknown schema version",
        });
    }
    for identity in [
        &row.media_root_identity,
        &row.database_root_identity,
        &row.database_identity,
        &row.source_lock_identity,
        &row.profile_root_identity,
        &row.profile_lock_identity,
        &row.owner_identity,
    ] {
        if identity.is_empty()
            || identity.len() > 1024
            || identity.chars().any(|character| character.is_control())
        {
            return Err(SourceLeaseError::InvalidRow {
                path: path.to_path_buf(),
                reason: "source lease identity is empty, oversized, or contains control data",
            });
        }
    }
    if Uuid::parse_str(&row.owner_identity).is_err()
        || row.lease_epoch <= 0
        || row.acquired_at < 0
        || row.expires_at < row.acquired_at
    {
        return Err(SourceLeaseError::InvalidRow {
            path: path.to_path_buf(),
            reason: "source lease row has an invalid identity, epoch, or timestamp",
        });
    }
    match row.state.as_str() {
        "active" if row.released_at.is_none() => {}
        "released"
            if row
                .released_at
                .is_some_and(|released| released >= row.acquired_at) => {}
        _ => {
            return Err(SourceLeaseError::InvalidRow {
                path: path.to_path_buf(),
                reason: "source lease state and release timestamp disagree",
            });
        }
    }
    Ok(())
}

fn open_lock_file(
    root: &BoundDatabaseRoot,
    name: &Path,
    path: &Path,
) -> Result<(File, bool), SourceLeaseError> {
    match root.open_file_nofollow(name, false) {
        Ok(file) => Ok((file, false)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => root
            .open_file_nofollow(name, true)
            .map(|file| (file, true))
            .or_else(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    root.open_file_nofollow(name, false)
                        .map(|file| (file, false))
                } else {
                    Err(error)
                }
            })
            .map_err(|source| map_lock_io(path, source)),
        Err(source) => Err(map_lock_io(path, source)),
    }
}

fn lock_file_is_regular(file: &File, path: &Path) -> Result<(), SourceLeaseError> {
    let metadata = file
        .metadata()
        .map_err(|source| map_lock_io(path, source))?;
    if !metadata.is_file() {
        return Err(SourceLeaseError::SidecarReplaced {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn map_lock_io(path: &Path, source: io::Error) -> SourceLeaseError {
    SourceLeaseError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(unix)]
fn acquire_nonblocking_lock(file: &File, path: &Path) -> Result<(), SourceLeaseError> {
    use std::os::fd::AsRawFd;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(());
    }
    let source = io::Error::last_os_error();
    if source.kind() == io::ErrorKind::WouldBlock {
        return Err(SourceLeaseError::WritableSourceOwnedByAnotherProcess {
            path: path.to_path_buf(),
        });
    }
    Err(map_lock_io(path, source))
}

#[cfg(windows)]
fn acquire_nonblocking_lock(file: &File, path: &Path) -> Result<(), SourceLeaseError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE, WIN32_ERROR};
    use windows::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows::Win32::System::IO::OVERLAPPED;

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
    if result.is_ok() {
        return Ok(());
    }
    let error = result.unwrap_err();
    if WIN32_ERROR::from_error(&error) == Some(ERROR_LOCK_VIOLATION) {
        return Err(SourceLeaseError::WritableSourceOwnedByAnotherProcess {
            path: path.to_path_buf(),
        });
    }
    Err(map_lock_io(path, io::Error::other(error.to_string())))
}

#[cfg(all(not(unix), not(windows)))]
fn acquire_nonblocking_lock(_file: &File, path: &Path) -> Result<(), SourceLeaseError> {
    Err(SourceLeaseError::Unsupported {
        path: path.to_path_buf(),
    })
}

fn lease_now() -> Result<i64, SourceLeaseError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SourceLeaseError::ClockUnavailable)?;
    i64::try_from(duration.as_secs()).map_err(|_| SourceLeaseError::ClockUnavailable)
}

fn is_row_shape_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::FromSqlConversionFailure(..)
            | rusqlite::Error::IntegralValueOutOfRange(..)
            | rusqlite::Error::InvalidColumnType(..)
    )
}

fn map_sql_error(error: rusqlite::Error) -> SourceLeaseError {
    match error.sqlite_error_code() {
        Some(rusqlite::ffi::ErrorCode::DatabaseBusy)
        | Some(rusqlite::ffi::ErrorCode::DatabaseLocked) => SourceLeaseError::Busy,
        _ => SourceLeaseError::Sql(error),
    }
}

fn map_database_error(error: super::sqlite_identity::DatabaseIdentityError) -> SourceLeaseError {
    SourceLeaseError::DatabaseIdentity(super::source_database_identity_error(error))
}

fn map_profile_error(error: ProfileOwnershipError) -> SourceLeaseError {
    match error {
        ProfileOwnershipError::SourceRootReplaced { path } => {
            SourceLeaseError::SourceRootReplaced { path }
        }
        ProfileOwnershipError::ExternalSourceRootUnavailable { path, reason } => {
            SourceLeaseError::SourceRootUnavailable { path, reason }
        }
        ProfileOwnershipError::ExternalSourceRootIdentityUnavailable { path, reason } => {
            SourceLeaseError::SourceRootIdentityUnavailable { path, reason }
        }
        other => SourceLeaseError::ProfileOwnership(other),
    }
}

pub(crate) fn map_database_root_error(error: ProfileOwnershipError) -> SourceLeaseError {
    match error {
        ProfileOwnershipError::SourceRootReplaced { path }
        | ProfileOwnershipError::ProfileChildDatabaseRootReplaced { path } => {
            SourceLeaseError::DatabaseRootReplaced { path }
        }
        ProfileOwnershipError::ExternalSourceRootUnavailable { path, reason }
        | ProfileOwnershipError::ProfileChildDatabaseRootUnavailable { path, reason } => {
            SourceLeaseError::DatabaseRootUnavailable { path, reason }
        }
        ProfileOwnershipError::ExternalSourceRootIdentityUnavailable { path, reason }
        | ProfileOwnershipError::ProfileChildDatabaseRootIdentityUnavailable { path, reason } => {
            SourceLeaseError::DatabaseRootIdentityUnavailable { path, reason }
        }
        other => SourceLeaseError::ProfileOwnership(other),
    }
}

fn validate_database_root(
    database_root: &BoundDatabaseRoot,
    profile_guard: &WritableProfileGuard,
) -> Result<(), SourceLeaseError> {
    // The binding validates the profile root itself and preserves profile-root failures. Do not
    // run a separate outer profile check and then infer a child replacement from its result: a
    // profile can change between those two checks, and that race must remain a profile error.
    database_root
        .validate_profile_guard(profile_guard)
        .map_err(map_database_root_error)
}

const LEASE_TABLE_SQL: &str = "CREATE TABLE source_lease (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    lease_schema_version INTEGER NOT NULL CHECK (lease_schema_version = 1),
    media_root_identity TEXT NOT NULL,
    database_root_identity TEXT NOT NULL,
    database_identity TEXT NOT NULL,
    source_lock_identity TEXT NOT NULL,
    profile_root_identity TEXT NOT NULL,
    profile_lock_identity TEXT NOT NULL,
    owner_identity TEXT NOT NULL,
    lease_epoch INTEGER NOT NULL CHECK (lease_epoch > 0),
    acquired_at INTEGER NOT NULL CHECK (acquired_at >= 0),
    expires_at INTEGER NOT NULL CHECK (expires_at >= acquired_at),
    released_at INTEGER,
    state TEXT NOT NULL CHECK (state IN ('active', 'released')),
    CHECK ((state = 'active' AND released_at IS NULL)
        OR (state = 'released' AND released_at IS NOT NULL))
)";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_row_validation_rejects_nonpositive_epoch_and_bad_state() {
        let path = Path::new("source.db");
        let row = LeaseRow {
            lease_schema_version: SOURCE_LEASE_TABLE_SCHEMA_VERSION,
            media_root_identity: String::from("media-root"),
            database_root_identity: String::from("database-root"),
            database_identity: String::from("database"),
            source_lock_identity: String::from("lock"),
            profile_root_identity: String::from("profile"),
            profile_lock_identity: String::from("profile-lock"),
            owner_identity: Uuid::new_v4().to_string(),
            lease_epoch: 0,
            acquired_at: 1,
            expires_at: 2,
            released_at: None,
            state: String::from("active"),
        };
        assert!(matches!(
            validate_lease_row(&row, path),
            Err(SourceLeaseError::InvalidRow { .. })
        ));
    }
}
