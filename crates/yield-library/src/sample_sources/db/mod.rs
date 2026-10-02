use std::collections::BTreeMap;
use std::time::Duration;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use rusqlite::{Connection, Transaction};
use std::fmt;

use crate::app_dirs::{ProfileOwnershipError, WritableProfileGuard};

use super::reconciliation::RootIdentity;
use super::source_entry::SourceTraversalPolicy;

mod content_audit;
mod directories;
mod error;
/// Persistent file operation journal for crash recovery.
pub mod file_ops_journal;
pub(crate) use file_ops_journal::BoundDatabaseRoot;
mod index_entries;
mod index_inspection;
mod open;
mod open_profiles;
/// Private rename-recovery metadata retained after immediate file pruning.
mod pending_renames;
/// Read-only database queries for sample sources.
pub mod read;
mod rename_destinations;
mod rename_metadata;
mod replay_authority;
pub(crate) use replay_authority::ValidatedReplayAuthority;
pub use replay_authority::{DurableReplayUncertainty, DurableReplayUncertaintyState};
/// SQLite schema management for sample source databases.
pub mod schema;
mod source_authority;
mod source_lease;
mod source_reference_fence;
pub(crate) mod sqlite_identity;
mod starmap_layout;
#[allow(unused_imports)]
pub(crate) use source_authority::{
    SourceAuthorityConfirmation, SourceAuthorityDrainMarker, SourceAuthorityGuard,
    SourceAuthorityRequest, SourceAuthorityUseProof, SourceEffectPermit, SourceEffectPin,
    SourceLeaseIdentity, SourceRegistrationId,
};
pub use source_authority::{
    SourceAuthorityError, SourceAuthorityRegistration, SourceAuthorityRegistrationReceipt,
    SourceAuthorityRegistrationRequest, SourceAuthorityRetargetContext,
    SourceAuthorityRetirementAck,
};
pub use source_lease::{SourceLeaseError, SourcePublicationFence, SourcePublicationFenceError};
pub use source_reference_fence::{SourceReferenceFence, SourceReferenceFenceError};
pub use starmap_layout::{
    STARMAP_LAYOUT_MAX_REQUEST_BYTES, STARMAP_LAYOUT_MAX_SAMPLES, StarmapLayoutError,
    StarmapLayoutLoad, StarmapLayoutPoint, StarmapLayoutRequest, StarmapLayoutSample,
};
/// Durable per-source tag catalog and sample assignment helpers.
pub mod tags;
mod telemetry;
mod types;
/// Write-focused database helpers for sample sources.
pub mod write;

/// Database path helpers and normalization utilities.
pub mod util;

mod rating_tests;
#[cfg(test)]
mod role_contract_tests;

pub use content_audit::{
    ContentAuditCheckpoint, ContentAuditEntryState, ContentAuditForwardCandidate,
    ContentAuditReport, ContentAuditRetryCandidate, ContentAuditSkipReason,
};
pub use error::{SourceDatabaseIdentityError, SourceDbError, SourceDirectoryTruthError};
pub use index_entries::{
    SOURCE_INDEX_PAGE_MAX_BYTES, SOURCE_INDEX_PAGE_MAX_ENTRIES, SourceIndexPage,
    SourceIndexPageCursor, SourceIndexPageError,
};
pub use index_inspection::{
    MAX_HELD_INDEX_INSPECTIONS, SourceIndexEntryHandle, SourceIndexInspection,
    SourceIndexInspectionError,
};
pub(crate) use open::SourceDatabaseOpenMode;
#[cfg(test)]
pub(crate) use open::open_source_database;
#[cfg(debug_assertions)]
pub use open::test_source_db_open_total_count;
#[cfg(debug_assertions)]
pub use open::{TestSourceDbOpenCountGuard, test_scope_source_db_open_total_count};
pub use open_profiles::SourceDatabaseConnectionRole;
/// Metadata retained for a pruned row so later scans can recover rename state.
pub use pending_renames::{PendingRenameDiagnostics, PendingRenameEntry, PendingRenamePruneReport};
pub use rename_metadata::RenameMetadataSnapshot;
pub use tags::{
    SOURCE_CURATION_TAG_FACTS_MAX_PATHS, SOURCE_DISPLAY_LABEL_MAX_OUTPUT_BYTES,
    SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES, SOURCE_DISPLAY_LABEL_MAX_PATHS,
    SOURCE_TAG_FILTER_MAX_INPUT_BYTES, SOURCE_TAG_FILTER_MAX_TAGS,
    SOURCE_TAG_FILTER_MAX_TOKEN_BYTES, SourceTagFilterResult,
};
pub use types::{
    BrowserFileMetadata, BrowserMetadataCursor, BrowserMetadataPage, BrowserMetadataSnapshot,
    ExistingFileMetadataUpdate, Rating, SampleCollection, SampleSoundType, SourceCurationTagFacts,
    SourceCurationTagFactsPage, SourceDirectoryEntry, SourceDirectoryTruthCleanup,
    SourceDirectoryTruthCursor, SourceDirectoryTruthPage, SourceDirectoryTruthPublication,
    SourceDirectoryTruthState, SourceDirectoryTruthUnavailableReason, SourceDisplayLabelEntry,
    SourceDisplayLabelResult, SourceIndexClassification, SourceIndexDiagnostic, SourceIndexEntry,
    SourceIndexSnapshot, SourceManifestEntry, SourcePlaybackType, SourceTag,
    SourceTagAssignmentCursor, SourceTagAssignmentPage, SourceTagPage, SourceTagPageCursor,
    SourceTagPageEntry, SourceTagUsage, WavEntry,
};
pub use util::normalize_relative_path;
pub use write::{
    ManifestCommitResult, SOURCE_CURATION_BATCH_LIMIT, SourceCollectionWrite, SourceCommandId,
    SourceContentHashWrite, SourceCurationCommitReceipt, SourceCurationUpdate,
    SourceDirectoryTruthCommitResult, SourceFileWrite, SourceIndexCommitResult, SourceRevision,
    SourceScanFence, SourceSnapshotOwner, SourceSnapshotPage, SourceTagAssignmentMutation,
    SourceTagAssignmentSelection, SourceTagWrite, SourceWriteCommand, SourceWriter,
    SourceWriterCommandIdentity, SourceWriterError, SourceWriterReceipt, SourceWriterRequest,
};

/// Hidden filename used for per-source databases.
pub const DB_FILE_NAME: &str = ".yield.db";
/// Previous hidden filename used for per-source databases.
pub const LEGACY_DB_FILE_NAME: &str = ".yield_samples.db";
/// Metadata key for the last completed scan timestamp.
pub const META_LAST_SCAN_COMPLETED_AT: &str = "last_scan_completed_at";
/// Metadata key storing the last completed periodic source-manifest audit timestamp.
pub const META_LAST_MANIFEST_AUDIT_AT: &str = "last_manifest_audit_at_v1";
/// Metadata key storing the durable macOS filesystem-event coverage checkpoint.
///
/// The value is owned by the native source watcher. It is deliberately separate from manifest
/// revisions: a watcher cursor proves what needs replaying, not that the manifest itself changed.
pub const META_SOURCE_WATCHER_CHECKPOINT: &str = "source_watcher_checkpoint_v1";
/// Bounded source-owned marker requiring a complete audit before replay authority is reused.
pub const META_SOURCE_WATCHER_UNCERTAINTY: &str = "source_watcher_uncertainty_v1";
/// Metadata key for the last similarity-prep scan timestamp.
/// Metadata key storing the last data revision cleaned by deferred maintenance.
pub const META_DEFERRED_MAINTENANCE_REVISION: &str = "deferred_maintenance_revision_v1";
/// Metadata key storing the last deferred-maintenance schema token.
pub const META_DEFERRED_MAINTENANCE_SCHEMA: &str = "deferred_maintenance_schema_v1";
/// Metadata key storing the last revision that changed the ordered wav path set.
pub const META_WAV_PATHS_REVISION: &str = "wav_paths_revision_v1";
/// Metadata key storing the last revision that changed live wav filesystem identities.
pub const META_WAV_IDENTITIES_REVISION: &str = "wav_identities_revision_v1";
/// Metadata key storing a revision-fenced readiness diagnostic for duplicate file identities.
pub const META_READINESS_DUPLICATE_IDENTITY: &str = "readiness_duplicate_identity_v1";
/// Metadata key storing the revision of the index-only unsupported file set.
pub const META_SOURCE_INDEX_REVISION: &str = "source_index_revision_v1";
/// Metadata key storing the physical root identity of the last complete manifest audit.
pub const META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY: &str =
    "last_authoritative_audit_root_identity_v1";
/// Metadata key storing the exact source revision committed by the last complete manifest audit.
pub const META_LAST_AUTHORITATIVE_AUDIT_REVISION: &str = "last_authoritative_audit_revision_v1";
/// Metadata key for the persisted source traversal/visibility policy.
pub const META_SOURCE_TRAVERSAL_POLICY: &str = "source_traversal_policy_v1";
/// Maximum number of directory entries accepted by one staging batch or read page.
pub const SOURCE_DIRECTORY_TRUTH_BATCH_LIMIT: usize = 256;
/// Maximum number of inactive directory generations removed by one cleanup transaction.
pub const SOURCE_DIRECTORY_TRUTH_CLEANUP_LIMIT: usize = 8;
/// Maximum encoded length of one persisted stable directory identity.
pub const SOURCE_DIRECTORY_TRUTH_MAX_IDENTITY_BYTES: usize = 1024;
/// Maximum UTF-8 byte length of a curation tag label before identity normalization.
pub const SOURCE_CURATION_MAX_TAG_LABEL_BYTES: usize = 1024;
/// Maximum number of selected paths accepted by one bounded source-tag page request.
pub const SOURCE_TAG_PAGE_MAX_PATHS: usize = 65_536;
/// Maximum aggregate encoded byte length of selected paths in one tag-page request.
pub const SOURCE_TAG_PAGE_MAX_PATH_BYTES: usize = 16 * 1024 * 1024;
/// Maximum number of source-tag ids accepted by one bounded selected-tag page request.
pub const SOURCE_TAG_PAGE_MAX_TAG_IDS: usize = 64;
/// Maximum UTF-8 byte length of a source-tag vocabulary prefix.
pub const SOURCE_TAG_PAGE_MAX_PREFIX_BYTES: usize = 1024;
/// Maximum number of entries returned by one bounded source-tag page request.
pub const SOURCE_TAG_PAGE_MAX_PAGE_SIZE: usize = 64;
/// Maximum encoded byte length of one path returned by a tag-assignment page.
pub const SOURCE_TAG_ASSIGNMENT_MAX_PATH_BYTES: usize = 4096;
/// Maximum aggregate path bytes returned by one tag-assignment page.
pub const SOURCE_TAG_ASSIGNMENT_MAX_PAGE_BYTES: usize = 64 * 1024;
/// Env var that enables read-only source DB opening by default.
pub const SOURCE_DB_READ_ONLY_ENV: &str = "YIELD_SOURCE_DB_READ_ONLY";

/// Durable identity and source revision committed by one complete manifest audit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceAuditMetadata {
    /// Physical source-root identity captured by the audit.
    pub root_identity: RootIdentity,
    /// Exact source revision committed by the audit transaction.
    pub source_revision: u64,
}

/// Keeps a verified database-root namespace alive for the lifetime of its connection.
///
/// Recovery and profile-owned writers supply platform-specific implementations when SQLite must
/// open through a descriptor-bound namespace. This is intentionally private to the library's
/// database implementation; ordinary callers continue to use pathname-based opening.
pub(crate) trait DatabaseRootBindingGuard: Send + Sync {
    fn validate_profile_guard(
        &self,
        profile_guard: &WritableProfileGuard,
    ) -> Result<(), ProfileOwnershipError>;
}

pub(super) fn map_database_identity_error(
    error: sqlite_identity::DatabaseIdentityError,
) -> SourceDbError {
    SourceDbError::DatabaseIdentity(source_database_identity_error(error))
}

pub(super) fn source_database_identity_error(
    error: sqlite_identity::DatabaseIdentityError,
) -> SourceDatabaseIdentityError {
    match error {
        sqlite_identity::DatabaseIdentityError::Io { path, source } => {
            SourceDatabaseIdentityError::Io { path, source }
        }
        sqlite_identity::DatabaseIdentityError::NotRegular { path } => {
            SourceDatabaseIdentityError::NotRegular { path }
        }
        sqlite_identity::DatabaseIdentityError::Replaced { path } => {
            SourceDatabaseIdentityError::Replaced { path }
        }
        sqlite_identity::DatabaseIdentityError::Unavailable { path, reason } => {
            SourceDatabaseIdentityError::Unavailable { path, reason }
        }
        sqlite_identity::DatabaseIdentityError::ConnectionMismatch { path, reason } => {
            SourceDatabaseIdentityError::ConnectionMismatch { path, reason }
        }
        sqlite_identity::DatabaseIdentityError::UnsafeSidecar { path, reason } => {
            SourceDatabaseIdentityError::UnsafeSidecar { path, reason }
        }
    }
}

/// Borrowed ownership context for readiness mutations.
///
/// This context deliberately borrows the capabilities retained by a [`SourceDatabase`] rather
/// than cloning or reacquiring them. A raw readiness connection can still serve read queries, but
/// it has no context that can authorize a mutation in a normal build.
pub(crate) struct ReadinessOwnership<'a> {
    profile_guard: Option<&'a WritableProfileGuard>,
    database_root_binding: Option<&'a dyn DatabaseRootBindingGuard>,
    database_file_binding: Option<&'a sqlite_identity::DatabaseFileBinding>,
    source_lease: Option<&'a Arc<source_lease::SourceLease>>,
}

impl<'a> ReadinessOwnership<'a> {
    pub(crate) fn new(
        profile_guard: Option<&'a WritableProfileGuard>,
        database_root_binding: Option<&'a dyn DatabaseRootBindingGuard>,
        database_file_binding: Option<&'a sqlite_identity::DatabaseFileBinding>,
        source_lease: Option<&'a Arc<source_lease::SourceLease>>,
    ) -> Self {
        Self {
            profile_guard,
            database_root_binding,
            database_file_binding,
            source_lease,
        }
    }

    pub(crate) fn unowned() -> Self {
        Self::new(None, None, None, None)
    }

    fn has_owned_capability(&self) -> bool {
        self.profile_guard.is_some()
            || self.database_root_binding.is_some()
            || self.database_file_binding.is_some()
            || self.source_lease.is_some()
    }

    fn has_complete_ownership(&self) -> bool {
        self.profile_guard.is_some()
            && self.database_root_binding.is_some()
            && self.database_file_binding.is_some()
            && self.source_lease.is_some()
    }

    pub(crate) fn publication_fence(
        &self,
    ) -> Result<
        Option<source_lease::SourcePublicationFence>,
        crate::sample_sources::readiness::ReadinessError,
    > {
        match self.source_lease {
            Some(source_lease) if self.has_complete_ownership() => {
                Ok(Some(source_lease.publication_fence()))
            }
            Some(_) => Err(crate::sample_sources::readiness::ReadinessError::OwnershipRequired),
            None if self.has_owned_capability() => Err(
                crate::sample_sources::readiness::ReadinessError::SourceLease(
                    source_lease::SourceLeaseError::Required,
                ),
            ),
            None => {
                #[cfg(any(test, feature = "test-support"))]
                {
                    Ok(None)
                }
                #[cfg(not(any(test, feature = "test-support")))]
                {
                    Err(crate::sample_sources::readiness::ReadinessError::OwnershipRequired)
                }
            }
        }
    }

    /// Compare a publication's physical-source fence with the receiving retained lease.
    ///
    /// This check is deliberately cheap and must run before current ownership validation, because
    /// that validation may renew an idle receiving lease. It reads only the weak pointer identity,
    /// immutable owner/epoch values, and the fence's in-memory liveness token.
    pub(crate) fn validate_publication_fence_identity(
        &self,
        publication_fence: Option<&source_lease::SourcePublicationFence>,
    ) -> Result<(), crate::sample_sources::readiness::ReadinessError> {
        match (self.source_lease, publication_fence) {
            (Some(source_lease), Some(publication_fence)) if self.has_complete_ownership() => {
                if !publication_fence.matches_retained_lease(source_lease) {
                    return Err(
                        crate::sample_sources::readiness::ReadinessError::SourcePublicationFence(
                            source_lease::SourcePublicationFenceError::LeaseMismatch,
                        ),
                    );
                }
                if !publication_fence.is_live() {
                    return Err(
                        crate::sample_sources::readiness::ReadinessError::SourcePublicationFence(
                            source_lease::SourcePublicationFenceError::LeaseDropped,
                        ),
                    );
                }
                Ok(())
            }
            (Some(_), None) => Err(
                crate::sample_sources::readiness::ReadinessError::SourceLease(
                    source_lease::SourceLeaseError::Required,
                ),
            ),
            (Some(_), Some(_)) => {
                Err(crate::sample_sources::readiness::ReadinessError::OwnershipRequired)
            }
            (None, Some(_)) => {
                Err(crate::sample_sources::readiness::ReadinessError::OwnershipRequired)
            }
            (None, None) if self.has_owned_capability() => Err(
                crate::sample_sources::readiness::ReadinessError::SourceLease(
                    source_lease::SourceLeaseError::Required,
                ),
            ),
            (None, None) => {
                #[cfg(any(test, feature = "test-support"))]
                {
                    Ok(())
                }
                #[cfg(not(any(test, feature = "test-support")))]
                {
                    Err(crate::sample_sources::readiness::ReadinessError::OwnershipRequired)
                }
            }
        }
    }

    /// Fully validate a publication's physical-source fence after current ownership validation.
    ///
    /// The caller must perform [`Self::validate_publication_fence_identity`] first. This read-only
    /// validation may use the issuing lease's retained control connection and is required again
    /// at each durable mutation boundary.
    pub(crate) fn validate_publication_fence(
        &self,
        publication_fence: Option<&source_lease::SourcePublicationFence>,
    ) -> Result<(), crate::sample_sources::readiness::ReadinessError> {
        match (self.source_lease, publication_fence) {
            (Some(source_lease), Some(publication_fence)) if self.has_complete_ownership() => {
                if !publication_fence.matches_retained_lease(source_lease) {
                    return Err(
                        crate::sample_sources::readiness::ReadinessError::SourcePublicationFence(
                            source_lease::SourcePublicationFenceError::LeaseMismatch,
                        ),
                    );
                }
                publication_fence
                    .validate()
                    .map_err(crate::sample_sources::readiness::ReadinessError::from)
            }
            _ => self.validate_publication_fence_identity(publication_fence),
        }
    }

    pub(crate) fn validate(
        &self,
        connection: &Connection,
    ) -> Result<(), crate::sample_sources::readiness::ReadinessError> {
        match (self.profile_guard, self.database_root_binding) {
            (Some(profile_guard), Some(database_root_binding)) => {
                database_root_binding
                    .validate_profile_guard(profile_guard)
                    .map_err(map_readiness_database_root_error)?;
                match self.database_file_binding {
                    Some(database_file_binding) => {
                        database_file_binding
                            .validate(connection)
                            .map_err(|error| {
                                crate::sample_sources::readiness::ReadinessError::DatabaseIdentity(
                                    source_database_identity_error(error),
                                )
                            })?;
                        match self.source_lease {
                            Some(source_lease) => source_lease
                                .prepare_for_mutation(connection)
                                .map_err(crate::sample_sources::readiness::ReadinessError::from),
                            None => {
                                #[cfg(any(test, feature = "test-support"))]
                                {
                                    Ok(())
                                }
                                #[cfg(not(any(test, feature = "test-support")))]
                                {
                                    Err(crate::sample_sources::readiness::ReadinessError::SourceLease(
                                        source_lease::SourceLeaseError::Required,
                                    ))
                                }
                            }
                        }
                    }
                    None => {
                        #[cfg(any(test, feature = "test-support"))]
                        {
                            Ok(())
                        }
                        #[cfg(not(any(test, feature = "test-support")))]
                        {
                            Err(crate::sample_sources::readiness::ReadinessError::OwnershipRequired)
                        }
                    }
                }
            }
            (None, None) => {
                #[cfg(any(test, feature = "test-support"))]
                {
                    Ok(())
                }
                #[cfg(not(any(test, feature = "test-support")))]
                {
                    Err(crate::sample_sources::readiness::ReadinessError::OwnershipRequired)
                }
            }
            _ => Err(crate::sample_sources::readiness::ReadinessError::OwnershipRequired),
        }
    }

    /// Validate retained ownership without renewing the source lease.
    ///
    /// Read-only source qualifications must use this strict path even in test builds. The
    /// mutation validator above intentionally permits unowned fixtures and may renew a lease;
    /// neither behavior is suitable for minting a source-side read-time qualification.
    #[allow(dead_code)]
    pub(crate) fn validate_read_only(
        &self,
        connection: &Connection,
    ) -> Result<(), crate::sample_sources::readiness::ReadinessError> {
        let (
            Some(profile_guard),
            Some(database_root_binding),
            Some(database_file_binding),
            Some(source_lease),
        ) = (
            self.profile_guard,
            self.database_root_binding,
            self.database_file_binding,
            self.source_lease,
        )
        else {
            return Err(crate::sample_sources::readiness::ReadinessError::OwnershipRequired);
        };
        database_root_binding
            .validate_profile_guard(profile_guard)
            .map_err(map_readiness_database_root_error)?;
        database_file_binding
            .validate(connection)
            .map_err(|error| {
                crate::sample_sources::readiness::ReadinessError::DatabaseIdentity(
                    source_database_identity_error(error),
                )
            })?;
        source_lease
            .validate_current(connection)
            .map_err(crate::sample_sources::readiness::ReadinessError::from)
    }

    /// Return the physical root and database identities held by the retained file binding.
    #[allow(dead_code)]
    pub(crate) fn verified_binding_identities(
        &self,
    ) -> Result<(String, String), crate::sample_sources::readiness::ReadinessError> {
        let Some(database_file_binding) = self.database_file_binding else {
            return Err(crate::sample_sources::readiness::ReadinessError::OwnershipRequired);
        };
        Ok((
            database_file_binding.root().identity().to_owned(),
            database_file_binding.identity().to_owned(),
        ))
    }
}

/// SQLite wrapper that stores wav metadata for a single source folder.
pub struct SourceDatabase {
    /// Declared first so the ordinary SQLite connection closes before lease release.
    connection: Connection,
    db_path: PathBuf,
    root: PathBuf,
    index_page_binding: Arc<()>,
    telemetry_label: &'static str,
    database_root_binding: Option<Arc<dyn DatabaseRootBindingGuard>>,
    database_file_binding: Option<Arc<sqlite_identity::DatabaseFileBinding>>,
    source_lease: Option<Arc<source_lease::SourceLease>>,
    profile_guard: Option<WritableProfileGuard>,
}

/// Owned source-database writer reservation retained across an asynchronous snapshot handoff.
///
/// Dropping the fence releases the SQLite `BEGIN IMMEDIATE` transaction without changing data.
pub struct SourceDatabaseWriteFence {
    /// Declared first so rollback/connection close precedes lease release during implicit drop.
    connection: Connection,
    _database_root_binding: Option<Arc<dyn DatabaseRootBindingGuard>>,
    _database_file_binding: Option<Arc<sqlite_identity::DatabaseFileBinding>>,
    _source_lease: Option<Arc<source_lease::SourceLease>>,
    _profile_guard: Option<WritableProfileGuard>,
}

impl fmt::Debug for SourceDatabaseWriteFence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("SourceDatabaseWriteFence").finish()
    }
}

impl Drop for SourceDatabaseWriteFence {
    fn drop(&mut self) {
        let _ = self.connection.execute_batch("ROLLBACK");
    }
}

/// Groups multiple database writes into one transaction using cached statements.
pub struct SourceWriteBatch<'conn> {
    tx: Transaction<'conn>,
    /// The source database whose retained profile capability authorizes this transaction.
    ///
    /// Keeping this borrow with the SQLite transaction lets every commit boundary revalidate the
    /// same profile root and database-child binding that were checked before `BEGIN`.
    owner: &'conn SourceDatabase,
    paths_revision_dirty: bool,
    identities_revision_dirty: bool,
    index_revision_dirty: bool,
    manifest_touched_paths: BTreeSet<PathBuf>,
    source_index_changes: BTreeMap<PathBuf, Option<SourceIndexEntry>>,
    manifest_audit_completed: bool,
}

impl SourceDatabase {
    /// Close this source connection and release its physical-source lease.
    ///
    /// The ordinary SQLite connection is dropped before the retained lease row is marked released.
    /// A live snapshot fence keeps the lease alive, so this consuming call closes the connection
    /// and returns [`SourceLeaseError::SnapshotsActive`]; callers must drop the fence and reopen
    /// the source when they need to release it. Drop remains a best-effort fallback that logs a
    /// failed release and leaves the durable row for conservative recovery.
    pub fn close(self) -> Result<(), SourceDbError> {
        let SourceDatabase {
            connection,
            source_lease,
            ..
        } = self;
        let snapshots_active = source_lease
            .as_ref()
            .is_some_and(|lease| Arc::strong_count(lease) > 1);
        drop(connection);
        if snapshots_active {
            drop(source_lease);
            return Err(SourceDbError::SourceLease(
                source_lease::SourceLeaseError::SnapshotsActive,
            ));
        }
        let result = match source_lease.as_deref() {
            Some(lease) => lease.release().map_err(SourceDbError::from),
            None => Ok(()),
        };
        drop(source_lease);
        result
    }

    /// Retarget a profile-owned catalog database to a new, explicitly verified source root.
    ///
    /// This consuming operation is the catalog's higher-generation handoff boundary. It keeps
    /// the profile-child database namespace and its physical-source sidecar lease, advances the
    /// durable lease epoch, and reopens the ordinary connection against the same database file.
    /// External/source-local databases and databases with a live snapshot fence cannot be
    /// retargeted. On failure the database connection has been consumed; the retained SQLite and
    /// filesystem data remain in place and the lease is released conservatively by its owner.
    pub fn retarget_catalog_root(
        self,
        new_root: impl AsRef<Path>,
        profile_guard: &WritableProfileGuard,
    ) -> Result<Self, SourceDbError> {
        self.retarget_catalog_root_inner(new_root, profile_guard, None)
    }

    /// Retarget a profile-child catalog while carrying a drained source-authority generation.
    ///
    /// The opaque context is marked only after the durable owner/epoch transaction commits. If
    /// reopening the new database connection then fails, the caller can still obtain the old
    /// superseded-generation acknowledgement; pre-commit and uncertain failures cannot mint it.
    pub fn retarget_catalog_root_with_source_authority(
        self,
        new_root: impl AsRef<Path>,
        profile_guard: &WritableProfileGuard,
        retarget_context: &mut source_authority::SourceAuthorityRetargetContext,
    ) -> Result<Self, SourceDbError> {
        self.retarget_catalog_root_inner(new_root, profile_guard, Some(retarget_context))
    }

    fn retarget_catalog_root_inner(
        self,
        new_root: impl AsRef<Path>,
        profile_guard: &WritableProfileGuard,
        mut retarget_context: Option<&mut source_authority::SourceAuthorityRetargetContext>,
    ) -> Result<Self, SourceDbError> {
        let new_root = new_root.as_ref();
        let source_lease = self
            .source_lease
            .as_ref()
            .ok_or(SourceDbError::SourceLease(
                source_lease::SourceLeaseError::Required,
            ))?;
        if !source_lease.is_profile_child_namespace() {
            return Err(SourceDbError::SourceLease(
                source_lease::SourceLeaseError::Unsupported {
                    path: source_lease.database_root_binding().path().to_path_buf(),
                },
            ));
        }
        if Arc::strong_count(source_lease) != 1 {
            return Err(SourceDbError::SourceLease(
                source_lease::SourceLeaseError::SnapshotsActive,
            ));
        }
        let expected_database = self
            .database_file_binding
            .as_deref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)?;
        if let Some(retarget_context) = retarget_context.as_deref() {
            retarget_context
                .validate_retarget_subject(source_lease, expected_database)
                .map_err(|_error| {
                    SourceDbError::SourceLease(source_lease::SourceLeaseError::Superseded {
                        path: expected_database.path().to_path_buf(),
                    })
                })?;
        }
        source_lease
            .prepare_for_mutation(&self.connection)
            .map_err(SourceDbError::from)?;
        source_lease
            .validate_before_open(profile_guard, expected_database)
            .map_err(SourceDbError::from)?;
        let new_media_root = Arc::new(
            BoundDatabaseRoot::for_external_source_root(profile_guard, new_root)
                .map_err(SourceDbError::from)?,
        );

        let SourceDatabase {
            connection,
            database_root_binding,
            database_file_binding,
            source_lease,
            profile_guard: owned_profile_guard,
            ..
        } = self;
        // The ordinary connection must be gone before the retained control connection performs
        // the handoff transaction. This also closes any abandoned source transaction.
        drop(connection);
        drop(database_root_binding);
        drop(owned_profile_guard);
        let expected_database =
            database_file_binding.ok_or(SourceDbError::ProfileOwnershipRequired)?;
        let source_lease = source_lease.ok_or(SourceDbError::SourceLease(
            source_lease::SourceLeaseError::Required,
        ))?;
        let mut source_lease = Arc::try_unwrap(source_lease).map_err(|_| {
            SourceDbError::SourceLease(source_lease::SourceLeaseError::SnapshotsActive)
        })?;
        source_lease
            .retarget_catalog_root(Arc::clone(&new_media_root), profile_guard)
            .map_err(SourceDbError::from)?;
        if let Some(retarget_context) = retarget_context.as_deref_mut() {
            retarget_context.mark_retarget_committed();
        }
        let source_lease = Arc::new(source_lease);
        let database_root = source_lease.database_root_binding();
        open::open_source_database_with_profile_guard_and_lease(
            new_root,
            database_root,
            profile_guard,
            open::SourceDatabaseOpenMode::Full,
            &expected_database,
            source_lease,
        )
    }

    /// Borrow a readiness store authorized by this database's retained profile ownership.
    ///
    /// The returned store borrows both the SQLite connection and the retained profile/database
    /// capabilities. Mutations revalidate those capabilities before beginning work and before
    /// committing it; a raw [`crate::sample_sources::readiness::ReadinessStore::new`] store
    /// remains suitable for read-only queries and fails closed for mutations in normal builds.
    pub fn readiness_store(
        &mut self,
    ) -> Result<crate::sample_sources::readiness::ReadinessStore<'_>, SourceDbError> {
        self.validate_profile_owned_mutation()?;
        let ownership = ReadinessOwnership::new(
            self.profile_guard.as_ref(),
            self.database_root_binding.as_deref(),
            self.database_file_binding.as_deref(),
            self.source_lease.as_ref(),
        );
        Ok(
            crate::sample_sources::readiness::ReadinessStore::from_owned_connection(
                &mut self.connection,
                ownership,
            ),
        )
    }

    /// Borrow an owned readiness store for read-only source qualification.
    ///
    /// Unlike [`Self::readiness_store`], this path performs only retained profile, database-file,
    /// and lease validation. It does not renew the lease or authorize any readiness mutation.
    #[allow(dead_code)]
    pub(crate) fn readiness_store_read_only(
        &mut self,
    ) -> Result<crate::sample_sources::readiness::ReadinessStore<'_>, SourceDbError> {
        let profile_guard = self
            .profile_guard
            .as_ref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)?;
        self.validate_profile_owned_open(profile_guard)?;
        let ownership = ReadinessOwnership::new(
            self.profile_guard.as_ref(),
            self.database_root_binding.as_deref(),
            self.database_file_binding.as_deref(),
            self.source_lease.as_ref(),
        );
        Ok(
            crate::sample_sources::readiness::ReadinessStore::from_owned_connection(
                &mut self.connection,
                ownership,
            ),
        )
    }

    /// Mint a weak source-publication fence from this profile-owned database.
    ///
    /// The fence captures the current physical-source owner and epoch after a read-only live
    /// validation. It does not retain the lease or grant any database/filesystem authority; a
    /// caller must validate it immediately before publishing a source-scoped result.
    pub fn source_publication_fence(
        &self,
    ) -> Result<source_lease::SourcePublicationFence, SourceDbError> {
        let profile_guard = self
            .profile_guard
            .as_ref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)?;
        self.validate_profile_owned_open(profile_guard)?;
        let source_lease = self
            .source_lease
            .as_ref()
            .ok_or(SourceDbError::SourceLease(
                source_lease::SourceLeaseError::Required,
            ))?;
        Ok(source_lease.publication_fence())
    }

    /// Return the verified root and main-file identities used by the source-local journal reader.
    ///
    /// This is observation metadata only.  The retained profile, database-file binding, and
    /// source lease are revalidated before the identities are exposed to a read transaction.
    pub(crate) fn legacy_journal_dedup_binding(&self) -> Result<(String, String), SourceDbError> {
        let profile_guard = self
            .profile_guard
            .as_ref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)?;
        self.validate_profile_owned_open(profile_guard)?;
        let binding = self
            .database_file_binding
            .as_deref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)?;
        Ok((
            binding.root().identity().to_owned(),
            binding.identity().to_owned(),
        ))
    }

    /// Return the retained lease control connection's change count for ownership tests.
    #[cfg(test)]
    pub(crate) fn source_lease_control_total_changes_for_tests(
        &self,
    ) -> Option<Result<u64, SourceLeaseError>> {
        self.source_lease
            .as_deref()
            .map(source_lease::SourceLease::control_total_changes_for_tests)
    }

    /// Read the identity and exact revision of the last complete manifest audit.
    ///
    /// Both values are written by the same audit transaction. A partially present or malformed
    /// pair is rejected so callers cannot publish an identity without its matching revision.
    pub fn last_authoritative_audit_metadata(
        &self,
    ) -> Result<Option<SourceAuditMetadata>, SourceDbError> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(SourceDbError::from)?;
        // Bound returned bytes before rusqlite allocates Strings. LIMIT 3 detects ambiguity
        // even for a corrupt legacy metadata table without its normal unique key constraint.
        let values = {
            let mut statement = transaction.prepare(
                "SELECT key, CASE WHEN typeof(value) = 'text'
                    AND length(CAST(value AS BLOB)) BETWEEN 1 AND
                        CASE WHEN key = ?1 THEN 24576 ELSE 20 END
                    THEN value END
                 FROM metadata WHERE key IN (?1, ?2) LIMIT 3",
            )?;
            statement
                .query_map(
                    rusqlite::params![
                        META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY,
                        META_LAST_AUTHORITATIVE_AUDIT_REVISION
                    ],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                )?
                .collect::<Result<Vec<_>, _>>()?
        };
        transaction.rollback().map_err(SourceDbError::from)?;
        if values.is_empty() {
            return Ok(None);
        }
        if values.len() != 2 {
            return Err(SourceDbError::Unexpected);
        }
        let mut identity = None;
        let mut revision = None;
        for (key, value) in values {
            let slot = match key.as_str() {
                META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY => &mut identity,
                META_LAST_AUTHORITATIVE_AUDIT_REVISION => &mut revision,
                _ => return Err(SourceDbError::Unexpected),
            };
            if slot
                .replace(value.ok_or(SourceDbError::Unexpected)?)
                .is_some()
            {
                return Err(SourceDbError::Unexpected);
            }
        }
        let source_revision = revision
            .ok_or(SourceDbError::Unexpected)?
            .parse::<u64>()
            .map_err(|_| SourceDbError::Unexpected)?;
        let identity = serde_json::from_str::<Vec<u8>>(&identity.ok_or(SourceDbError::Unexpected)?)
            .map_err(|_| SourceDbError::Unexpected)?;
        if identity.is_empty() || identity.len() > 4096 {
            return Err(SourceDbError::Unexpected);
        }
        Ok(Some(SourceAuditMetadata {
            root_identity: RootIdentity::from_bytes(identity),
            source_revision,
        }))
    }

    /// Override this connection's SQLite busy timeout for deterministic contention tests.
    #[cfg(feature = "test-support")]
    pub fn set_busy_timeout_for_tests(&self, timeout: Duration) -> Result<(), SourceDbError> {
        self.connection
            .busy_timeout(timeout)
            .map_err(SourceDbError::from)
    }

    /// Read the source traversal policy, defaulting to recursive inclusion for legacy sources.
    pub fn source_traversal_policy(&self) -> Result<SourceTraversalPolicy, SourceDbError> {
        Ok(self
            .get_metadata(META_SOURCE_TRAVERSAL_POLICY)?
            .as_deref()
            .map(SourceTraversalPolicy::from_stored)
            .unwrap_or_default())
    }

    /// Persist the source traversal policy used by full scans, audits, and targeted sync.
    pub fn set_source_traversal_policy(
        &self,
        policy: SourceTraversalPolicy,
    ) -> Result<(), SourceDbError> {
        self.set_metadata(META_SOURCE_TRAVERSAL_POLICY, policy.as_str())
    }

    /// Open a writable source database for general source-owned mutations.
    ///
    /// This preserves the complete schema and cleanup behavior used by the
    /// legacy `open` entrypoint while making write intent explicit to callers.
    /// Normal builds reject this path-only entrypoint with
    /// [`SourceDbError::ProfileOwnershipRequired`]; use a profile-owned session or catalog
    /// opener for writable access. Fixture builds retain this entrypoint for isolated tests.
    pub fn open_for_source_write(root: impl AsRef<Path>) -> Result<Self, SourceDbError> {
        let root = root.as_ref();
        open::open_source_database(
            root,
            open::should_open_source_db_read_only(),
            open::SourceDatabaseOpenMode::Full,
        )
    }

    /// Open a fully migrated writable database while arranging test fixture state.
    ///
    /// This API is intentionally unavailable in normal builds. Runtime callers
    /// must select the narrow role-specific open that owns their operation.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_for_test_fixture_source_write(
        root: impl AsRef<Path>,
    ) -> Result<Self, SourceDbError> {
        Self::open_for_source_write(root)
    }

    /// Write a transactionally consistent SQLite snapshot to a new database file.
    ///
    /// SQLite's online backup API coordinates with source-local writers and includes committed
    /// WAL-resident pages without requiring a global checkpoint. The destination must not exist.
    /// Acquiring its writable reservation returns [`SourceDbError::ProfileOwnershipRequired`] in
    /// normal builds for path-only databases; profile-owned catalog databases retain their
    /// acquired capability while taking the snapshot.
    pub fn snapshot_to_path(&self, destination: &Path) -> Result<(), SourceDbError> {
        self.snapshot_to_path_with_write_fence(destination)
            .map(drop)
    }

    /// Write a consistent snapshot while retaining the source writer reservation.
    ///
    /// The returned fence must remain alive until the caller either publishes or abandons the
    /// snapshot. This closes the post-snapshot window where a later source mutation could commit
    /// to the old root before an asynchronous remap result is applied.
    /// Path-only databases return [`SourceDbError::ProfileOwnershipRequired`] in normal builds.
    /// Profile-owned catalog databases retain their acquired capability in the returned fence.
    pub fn snapshot_to_path_with_write_fence(
        &self,
        destination: &Path,
    ) -> Result<SourceDatabaseWriteFence, SourceDbError> {
        let fence = self.acquire_snapshot_write_fence()?;
        self.snapshot_to_path_with_held_write_fence(destination)?;
        Ok(fence)
    }

    /// Install the source writer reservation before copying snapshot pages.
    ///
    /// The installer takes ownership of the fence before the backup starts, allowing a caller's
    /// cancellation path to release blocked source writers immediately during a long snapshot.
    pub fn snapshot_to_path_with_write_fence_install(
        &self,
        destination: &Path,
        install: impl FnOnce(SourceDatabaseWriteFence) -> bool,
    ) -> Result<(), SourceDbError> {
        let fence = self.acquire_snapshot_write_fence()?;
        if !install(fence) {
            return Err(SourceDbError::Canceled);
        }
        self.snapshot_to_path_with_held_write_fence(destination)
    }

    fn acquire_snapshot_write_fence(&self) -> Result<SourceDatabaseWriteFence, SourceDbError> {
        let database_root =
            self.db_path
                .parent()
                .ok_or_else(|| SourceDbError::UnsafeSourceDatabasePath {
                    path: self.db_path.clone(),
                    reason: "source database path has no parent directory",
                })?;
        let validated = match (&self.profile_guard, &self.database_root_binding) {
            (Some(profile_guard), Some(_database_root_binding)) => {
                self.validate_profile_owned_mutation()?;
                let expected_database = self
                    .database_file_binding
                    .as_deref()
                    .ok_or(SourceDbError::ProfileOwnershipRequired)?;
                let source_lease = self
                    .source_lease
                    .as_ref()
                    .ok_or(SourceDbError::ProfileOwnershipRequired)?;
                open::open_source_database_with_profile_guard_and_lease(
                    &self.root,
                    Arc::clone(expected_database.root()),
                    profile_guard,
                    open::SourceDatabaseOpenMode::Full,
                    expected_database,
                    Arc::clone(source_lease),
                )?
            }
            (None, None) => open::open_source_database_with_database_root(
                &self.root,
                database_root,
                false,
                open::SourceDatabaseOpenMode::Full,
            )?,
            _ => return Err(SourceDbError::ProfileOwnershipRequired),
        };
        validated.validate_profile_owned_mutation()?;
        let SourceDatabase {
            connection: writer_connection,
            database_root_binding,
            database_file_binding,
            source_lease,
            profile_guard,
            ..
        } = validated;
        writer_connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(SourceDbError::from)?;
        Ok(SourceDatabaseWriteFence {
            connection: writer_connection,
            _database_root_binding: database_root_binding,
            _database_file_binding: database_file_binding,
            _source_lease: source_lease,
            _profile_guard: profile_guard,
        })
    }

    fn snapshot_to_path_with_held_write_fence(
        &self,
        destination: &Path,
    ) -> Result<(), SourceDbError> {
        if let Some(parent) = destination.parent()
            && !parent.is_dir()
        {
            return Err(SourceDbError::CreateDir {
                path: parent.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "snapshot destination directory is unavailable",
                ),
            });
        }
        let reservation = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
            .map_err(|source| SourceDbError::InspectSourceDatabasePath {
                path: destination.to_path_buf(),
                source,
            })?;
        drop(reservation);
        let result = (|| -> Result<(), SourceDbError> {
            let mut destination_connection = rusqlite::Connection::open(destination)?;
            let backup =
                rusqlite::backup::Backup::new(&self.connection, &mut destination_connection)?;
            backup.run_to_completion(128, Duration::from_millis(5), None)?;
            drop(backup);
            destination_connection.close().map_err(|(_, error)| error)?;
            Ok(())
        })();
        if result.is_err() {
            remove_snapshot_artifacts(destination);
        }
        result
    }

    /// Open (or create) a source database stored outside the source root.
    ///
    /// `root` remains the audio root used for relative paths and scans, while
    /// `database_root` owns the `.yield.db` file.
    /// Open a writable external source database for general source-owned mutations.
    /// Normal builds reject this path-only entrypoint with
    /// [`SourceDbError::ProfileOwnershipRequired`].
    pub fn open_for_source_write_with_database_root(
        root: impl AsRef<Path>,
        database_root: impl AsRef<Path>,
    ) -> Result<Self, SourceDbError> {
        open::open_source_database_with_database_root(
            root.as_ref(),
            database_root.as_ref(),
            open::should_open_source_db_read_only(),
            open::SourceDatabaseOpenMode::Full,
        )
    }

    /// Open a writable source database in an explicitly profile-owned external namespace.
    ///
    /// The profile guard authorizes the database-root binding and remains retained by the
    /// returned database. The physical-source lease is acquired before the SQLite connection is
    /// opened, so two profile-owned callers cannot concurrently mutate the same source database.
    pub fn open_for_source_write_with_profile_guard(
        root: impl AsRef<Path>,
        profile_guard: &crate::app_dirs::WritableProfileGuard,
        database_root: impl AsRef<Path>,
    ) -> Result<Self, SourceDbError> {
        let database_root = file_ops_journal::BoundDatabaseRoot::for_external_source_root(
            profile_guard,
            database_root.as_ref(),
        )
        .map_err(open::map_database_root_open_error)?;
        open::open_source_database_with_profile_guard(
            root.as_ref(),
            Arc::new(database_root),
            profile_guard,
            open::SourceDatabaseOpenMode::Full,
            None,
        )
    }

    /// Retain a verified database-root namespace until this connection is closed.
    pub(crate) fn retain_database_root_binding(
        mut self,
        binding: Arc<dyn DatabaseRootBindingGuard>,
    ) -> Self {
        self.database_root_binding = Some(binding);
        self
    }

    pub(crate) fn validate_profile_owned_mutation(&self) -> Result<(), SourceDbError> {
        match (&self.profile_guard, &self.database_root_binding) {
            (Some(profile_guard), Some(_)) => {
                self.validate_profile_guard(profile_guard)?;
                match self.database_file_binding.as_deref() {
                    Some(database_file_binding) => {
                        database_file_binding
                            .validate(&self.connection)
                            .map_err(map_database_identity_error)?;
                        match self.source_lease.as_deref() {
                            Some(source_lease) => source_lease
                                .prepare_for_mutation(&self.connection)
                                .map_err(SourceDbError::from),
                            None => {
                                #[cfg(any(test, feature = "test-support"))]
                                {
                                    Ok(())
                                }
                                #[cfg(not(any(test, feature = "test-support")))]
                                {
                                    Err(SourceDbError::SourceLease(
                                        source_lease::SourceLeaseError::Required,
                                    ))
                                }
                            }
                        }
                    }
                    None => {
                        #[cfg(any(test, feature = "test-support"))]
                        {
                            Ok(())
                        }
                        #[cfg(not(any(test, feature = "test-support")))]
                        {
                            Err(SourceDbError::ProfileOwnershipRequired)
                        }
                    }
                }
            }
            #[cfg(any(test, feature = "test-support"))]
            (None, _) => Ok(()),
            _ => Err(SourceDbError::ProfileOwnershipRequired),
        }
    }

    /// Return whether this source can enter the final lease-release boundary without dropping a
    /// database that still has externally retained snapshot fences.
    pub fn source_authority_close_ready(&self) -> bool {
        self.source_lease
            .as_ref()
            .is_none_or(|lease| Arc::strong_count(lease) <= 2)
    }

    pub(crate) fn validate_profile_owned_open(
        &self,
        profile_guard: &WritableProfileGuard,
    ) -> Result<(), SourceDbError> {
        self.validate_profile_guard(profile_guard)?;
        self.database_file_binding
            .as_deref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)?
            .validate(&self.connection)
            .map_err(map_database_identity_error)?;
        match self.source_lease.as_deref() {
            Some(source_lease) => source_lease
                .validate_current(&self.connection)
                .map_err(SourceDbError::from),
            None => {
                #[cfg(any(test, feature = "test-support"))]
                {
                    Ok(())
                }
                #[cfg(not(any(test, feature = "test-support")))]
                {
                    Err(SourceDbError::SourceLease(
                        source_lease::SourceLeaseError::Required,
                    ))
                }
            }
        }
    }

    /// Revalidate the retained profile-owned database namespace before an operation.
    ///
    /// Profile-owned catalog connections retain both the profile capability and the named child
    /// identity. A successful check means neither the profile root nor that child pathname was
    /// replaced since the connection was opened.
    pub fn validate_profile_guard(
        &self,
        profile_guard: &WritableProfileGuard,
    ) -> Result<(), SourceDbError> {
        if let Some(binding) = self.database_root_binding.as_ref() {
            binding
                .validate_profile_guard(profile_guard)
                .map_err(map_database_root_validation_error)
        } else {
            profile_guard
                .validate_current()
                .map_err(SourceDbError::from)
        }
    }

    /// Open a startup-friendly database for a background job.
    ///
    /// Normal builds reject this path-only writable alias with
    /// [`SourceDbError::ProfileOwnershipRequired`].
    pub fn open_for_background_job(root: impl AsRef<Path>) -> Result<Self, SourceDbError> {
        Self::open_with_role(root, SourceDatabaseConnectionRole::JobWorker)
    }

    /// Open a startup-friendly external database for a background job.
    ///
    /// Normal builds reject this path-only writable alias with
    /// [`SourceDbError::ProfileOwnershipRequired`].
    pub fn open_for_background_job_with_database_root(
        root: impl AsRef<Path>,
        database_root: impl AsRef<Path>,
    ) -> Result<Self, SourceDbError> {
        Self::open_with_role_and_database_root(
            root,
            database_root,
            SourceDatabaseConnectionRole::JobWorker,
        )
    }

    /// Open a startup-friendly database owned by source scanning.
    ///
    /// This path-only alias returns [`SourceDbError::ProfileOwnershipRequired`] in normal builds;
    /// read-only roles remain available through their explicit read entrypoints.
    pub fn open_for_scan(root: impl AsRef<Path>) -> Result<Self, SourceDbError> {
        Self::open_for_background_job(root)
    }

    /// Open a startup-friendly external database owned by source scanning.
    ///
    /// Normal builds reject this path-only writable alias with
    /// [`SourceDbError::ProfileOwnershipRequired`].
    pub fn open_for_scan_with_database_root(
        root: impl AsRef<Path>,
        database_root: impl AsRef<Path>,
    ) -> Result<Self, SourceDbError> {
        Self::open_for_background_job_with_database_root(root, database_root)
    }

    /// Open a scan database in one direct child of an already-owned profile root.
    ///
    /// The child is created and retained through the profile capability, so callers cannot
    /// accidentally create a source-local `.yield.db` while building an app-owned catalog.
    /// The returned database retains the descriptor-bound child capability and a cloned profile
    /// owner for its lifetime.
    pub fn open_for_catalog_with_profile_guard(
        root: impl AsRef<Path>,
        profile_guard: &crate::app_dirs::WritableProfileGuard,
        profile_child: impl AsRef<Path>,
    ) -> Result<Self, SourceDbError> {
        let binding = file_ops_journal::BoundDatabaseRoot::for_profile_child(
            profile_guard,
            profile_child.as_ref(),
        )
        .map_err(open::map_database_root_open_error)?;
        let database = open::open_source_database_with_profile_guard(
            root.as_ref(),
            Arc::new(binding),
            profile_guard,
            open::SourceDatabaseOpenMode::Full,
            None,
        )?;
        Ok(database)
    }

    /// Open an existing database for UI-owned read access.
    pub fn open_for_ui_read(root: impl AsRef<Path>) -> Result<Self, SourceDbError> {
        Self::open_with_role(root, SourceDatabaseConnectionRole::UiRead)
    }

    /// Open an external database for UI-owned read access.
    pub fn open_for_ui_read_with_database_root(
        root: impl AsRef<Path>,
        database_root: impl AsRef<Path>,
    ) -> Result<Self, SourceDbError> {
        Self::open_with_role_and_database_root(
            root,
            database_root,
            SourceDatabaseConnectionRole::UiRead,
        )
    }

    /// Open a source database for deferred schema and cleanup maintenance.
    ///
    /// Normal builds reject this path-only writable alias with
    /// [`SourceDbError::ProfileOwnershipRequired`].
    pub fn open_for_maintenance(root: impl AsRef<Path>) -> Result<Self, SourceDbError> {
        Self::open_with_role(root, SourceDatabaseConnectionRole::Maintenance)
    }

    /// Open a source database using one explicit runtime role profile.
    ///
    /// This keeps the caller's intent visible at the call site so UI reads,
    /// worker writes, and deferred maintenance do not silently share the same
    /// writable open behavior.
    /// Writable roles return [`SourceDbError::ProfileOwnershipRequired`] in normal builds because
    /// this path-only API carries no profile capability; read-only roles remain available.
    pub fn open_with_role(
        root: impl AsRef<Path>,
        role: SourceDatabaseConnectionRole,
    ) -> Result<Self, SourceDbError> {
        open::open_source_database_for_role(root.as_ref(), role)
    }

    /// Open a source database using an explicit role and database root.
    ///
    /// Writable roles return [`SourceDbError::ProfileOwnershipRequired`] in normal builds.
    pub fn open_with_role_and_database_root(
        root: impl AsRef<Path>,
        database_root: impl AsRef<Path>,
        role: SourceDatabaseConnectionRole,
    ) -> Result<Self, SourceDbError> {
        open::open_source_database_for_role_with_database_root(
            root.as_ref(),
            database_root.as_ref(),
            role,
        )
    }

    /// Open a writable source database for an explicit user metadata edit.
    ///
    /// This makes direct user edits visible at the call site without requiring
    /// callers to know the generic writable connection profile.
    /// Normal builds reject this path-only writable alias with
    /// [`SourceDbError::ProfileOwnershipRequired`].
    pub fn open_for_user_metadata_write(root: impl AsRef<Path>) -> Result<Self, SourceDbError> {
        open::open_source_database_for_role(
            root.as_ref(),
            SourceDatabaseConnectionRole::UserMetadataWrite,
        )
    }

    /// Open a writable external source database for an explicit user metadata edit.
    ///
    /// Normal builds reject this path-only writable alias with
    /// [`SourceDbError::ProfileOwnershipRequired`].
    pub fn open_for_user_metadata_write_with_database_root(
        root: impl AsRef<Path>,
        database_root: impl AsRef<Path>,
    ) -> Result<Self, SourceDbError> {
        open::open_source_database_for_role_with_database_root(
            root.as_ref(),
            database_root.as_ref(),
            SourceDatabaseConnectionRole::UserMetadataWrite,
        )
    }

    /// Open a writable external source database for opportunistic playback-history updates.
    ///
    /// This profile uses a short busy timeout so low-value last-played metadata
    /// cannot sit behind source scans or analysis work and delay interactive
    /// sample auditioning.
    /// Normal builds reject this path-only writable alias with
    /// [`SourceDbError::ProfileOwnershipRequired`].
    pub fn open_for_playback_history_write_with_database_root(
        root: impl AsRef<Path>,
        database_root: impl AsRef<Path>,
    ) -> Result<Self, SourceDbError> {
        open::open_source_database_for_role_with_database_root(
            root.as_ref(),
            database_root.as_ref(),
            SourceDatabaseConnectionRole::PlaybackHistoryWrite,
        )
    }

    /// Open a raw startup-friendly connection for a background job.
    ///
    /// Normal builds reject this path-only writable alias with
    /// [`SourceDbError::ProfileOwnershipRequired`].
    pub fn open_connection_for_background_job(
        root: impl AsRef<Path>,
    ) -> Result<Connection, SourceDbError> {
        Self::open_connection_with_role(root, SourceDatabaseConnectionRole::JobWorker)
    }

    /// Open a raw SQLite connection using one explicit runtime role profile.
    ///
    /// Writable roles return [`SourceDbError::ProfileOwnershipRequired`] in normal builds;
    /// read-only roles remain available.
    pub fn open_connection_with_role(
        root: impl AsRef<Path>,
        role: SourceDatabaseConnectionRole,
    ) -> Result<Connection, SourceDbError> {
        let db = Self::open_with_role(root, role)?;
        Ok(db.into_connection())
    }

    /// Open a raw SQLite connection using one explicit role and database root.
    ///
    /// Writable roles return [`SourceDbError::ProfileOwnershipRequired`] in normal builds.
    pub fn open_connection_with_role_and_database_root(
        root: impl AsRef<Path>,
        database_root: impl AsRef<Path>,
        role: SourceDatabaseConnectionRole,
    ) -> Result<Connection, SourceDbError> {
        let db = Self::open_with_role_and_database_root(root, database_root, role)?;
        Ok(db.into_connection())
    }

    /// Open an external metadata database while its audio source root is unavailable.
    ///
    /// This is intentionally limited to callers that already resolved a separate, existing
    /// metadata root. Treating that directory as the operational root avoids recreating or
    /// validating the absent audio directory while still applying the requested connection role.
    /// Writable roles return [`SourceDbError::ProfileOwnershipRequired`] in normal builds.
    pub fn open_unavailable_source_metadata_connection(
        database_root: impl AsRef<Path>,
        role: SourceDatabaseConnectionRole,
    ) -> Result<Connection, SourceDbError> {
        let database_root = database_root.as_ref();
        let db = Self::open_with_role_and_database_root(database_root, database_root, role)?;
        Ok(db.into_connection())
    }

    /// Return the path to the root folder backing this database.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Retain the source-compatible path-only WAL maintenance entrypoint.
    ///
    /// This helper intentionally does nothing for writable roles. A pathname-only
    /// reopen cannot prove the retained profile/database identity and would bypass
    /// the writable ownership boundary. Owned [`SourceDatabase`] writes invoke the
    /// same best-effort passive policy through their retained connection instead.
    #[deprecated(
        note = "WAL maintenance requires a retained SourceDatabase; owned writes checkpoint automatically"
    )]
    pub fn maybe_checkpoint_wal(root: impl AsRef<Path>, role: SourceDatabaseConnectionRole) {
        if role.uses_read_only_connection() {
            return;
        }
        // This compatibility helper predates the profile-owned writer boundary and has no
        // retained connection to which a checkpoint can be safely attached. Mutating a database
        // by reopening this pathname would bypass the retained profile/root identity checks, so
        // all production checkpoint calls now go through `maybe_checkpoint_owned_wal` below.
        tracing::debug!(
            target: "perf::source_db",
            root = %root.as_ref().display(),
            role = role.label(),
            "Skipping WAL checkpoint without an owned source connection"
        );
    }

    /// Attempt the shared WAL maintenance policy through this retained source connection.
    ///
    /// The post-commit owner check keeps a replaced profile/database child from triggering any
    /// maintenance after the write boundary has become invalid. A failed check is deliberately
    /// ignored here: the data transaction has already committed, so the maintenance is simply
    /// skipped rather than reported as a write failure.
    pub(super) fn maybe_checkpoint_owned_wal(&self) {
        if self.validate_profile_owned_mutation().is_ok() {
            crate::sqlite_wal::maybe_checkpoint_database_file(
                &self.connection,
                &self.db_path,
                "source_db",
                self.telemetry_label,
            );
        }
    }

    fn apply_pragmas(&self, busy_timeout_ms: u64) -> Result<(), SourceDbError> {
        let pragmas = format!(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout={busy_timeout_ms};
             PRAGMA temp_store=MEMORY;
             PRAGMA cache_size=-32000;
             PRAGMA mmap_size=134217728;"
        );
        self.connection
            .execute_batch(&pragmas)
            .map_err(util::map_sql_error)?;
        crate::sqlite_wal::apply_workload_wal_pragmas(&self.connection)
            .map_err(util::map_sql_error)?;
        if let Err(err) = crate::sqlite_ext::try_load_optional_extension(&self.connection) {
            tracing::debug!("SQLite extension not loaded: {err}");
        }
        Ok(())
    }

    fn apply_read_only_pragmas(
        &self,
        role: SourceDatabaseConnectionRole,
    ) -> Result<(), SourceDbError> {
        let pragmas = format!(
            "PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout={};
             PRAGMA temp_store=MEMORY;
             PRAGMA cache_size=-32000;
             PRAGMA mmap_size=134217728;",
            role.busy_timeout_ms(),
        );
        self.connection
            .execute_batch(&pragmas)
            .map_err(util::map_sql_error)?;
        if let Err(err) = crate::sqlite_ext::try_load_optional_extension(&self.connection) {
            tracing::debug!("SQLite extension not loaded: {err}");
        }
        Ok(())
    }

    fn apply_schema(&self) -> Result<(), SourceDbError> {
        schema::apply_schema(&self.connection).map(|_| ())
    }

    fn apply_schema_fast(&self) -> Result<(), SourceDbError> {
        schema::apply_schema_fast(&self.connection).map(|_| ())
    }

    fn into_connection(self) -> Connection {
        self.connection
    }
}

fn map_database_root_validation_error(error: ProfileOwnershipError) -> SourceDbError {
    match error {
        ProfileOwnershipError::SourceRootReplaced { path }
        | ProfileOwnershipError::ProfileChildDatabaseRootReplaced { path } => {
            SourceDbError::SourceLease(source_lease::SourceLeaseError::DatabaseRootReplaced {
                path,
            })
        }
        ProfileOwnershipError::ExternalSourceRootUnavailable { path, reason }
        | ProfileOwnershipError::ProfileChildDatabaseRootUnavailable { path, reason } => {
            SourceDbError::SourceLease(source_lease::SourceLeaseError::DatabaseRootUnavailable {
                path,
                reason,
            })
        }
        ProfileOwnershipError::ExternalSourceRootIdentityUnavailable { path, reason }
        | ProfileOwnershipError::ProfileChildDatabaseRootIdentityUnavailable { path, reason } => {
            SourceDbError::SourceLease(
                source_lease::SourceLeaseError::DatabaseRootIdentityUnavailable { path, reason },
            )
        }
        other => SourceDbError::ProfileOwnership(other),
    }
}

fn map_readiness_database_root_error(
    error: ProfileOwnershipError,
) -> crate::sample_sources::readiness::ReadinessError {
    match error {
        ProfileOwnershipError::SourceRootReplaced { path }
        | ProfileOwnershipError::ProfileChildDatabaseRootReplaced { path } => {
            crate::sample_sources::readiness::ReadinessError::SourceLease(
                source_lease::SourceLeaseError::DatabaseRootReplaced { path },
            )
        }
        ProfileOwnershipError::ExternalSourceRootUnavailable { path, reason }
        | ProfileOwnershipError::ProfileChildDatabaseRootUnavailable { path, reason } => {
            crate::sample_sources::readiness::ReadinessError::SourceLease(
                source_lease::SourceLeaseError::DatabaseRootUnavailable { path, reason },
            )
        }
        ProfileOwnershipError::ExternalSourceRootIdentityUnavailable { path, reason }
        | ProfileOwnershipError::ProfileChildDatabaseRootIdentityUnavailable { path, reason } => {
            crate::sample_sources::readiness::ReadinessError::SourceLease(
                source_lease::SourceLeaseError::DatabaseRootIdentityUnavailable { path, reason },
            )
        }
        other => crate::sample_sources::readiness::ReadinessError::ProfileOwnership(other),
    }
}

fn remove_snapshot_artifacts(database: &Path) {
    for path in [
        database.to_path_buf(),
        snapshot_sidecar_path(database, "-wal"),
        snapshot_sidecar_path(database, "-shm"),
        snapshot_sidecar_path(database, "-journal"),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                path = %path.display(),
                error = %error,
                "Failed to clean incomplete SQLite snapshot artifact"
            ),
        }
    }
}

fn snapshot_sidecar_path(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

/// Read a numeric metadata field without allocating an unbounded corrupt SQLite value.
fn read_bounded_numeric_metadata(connection: &Connection, key: &str) -> Result<u64, SourceDbError> {
    // A numeric revision is at most 20 decimal bytes. Do not materialize oversized
    // corrupt values; a second row rejects ambiguous legacy tables rather than picking one.
    let mut statement = connection
        .prepare(
            "SELECT CASE WHEN typeof(value) = 'text'
                AND length(CAST(value AS BLOB)) BETWEEN 1 AND 20 THEN value END
             FROM metadata WHERE key = ?1 LIMIT 2",
        )
        .map_err(SourceDbError::from)?;
    let values = statement
        .query_map([key], |row| row.get::<_, Option<String>>(0))
        .map_err(SourceDbError::from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(SourceDbError::from)?;
    match values.as_slice() {
        [] => Ok(0),
        [Some(raw)] => raw.parse::<u64>().map_err(|_| SourceDbError::Unexpected),
        _ => Err(SourceDbError::Unexpected),
    }
}

/// Unit tests for source-database open, migration, and metadata invariants.
#[cfg(test)]
mod source_db_mod_tests;

#[cfg(test)]
mod source_db_migration_tests;

#[cfg(test)]
mod bounded_metadata_tests;
