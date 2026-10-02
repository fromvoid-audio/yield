//! Confirmed collection before-state on the retained owner, without history admission.
use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::sample_sources::reconciliation::RootRelativePath;
use crate::sample_sources::{SampleCollection, SourceDbError, normalize_relative_path};

use super::{
    SnapshotProgressGuard, SourceRevision, SourceWriter, SourceWriterError, map_sql_error,
};

const MAX_PATHS: usize = 128;
const MAX_PATH_BYTES: usize = 4096;
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

/// Confirmed before and explicit desired after membership for one changed path.
#[derive(Debug, PartialEq, Eq)]
pub struct SourceCollectionHistoryChange {
    path: RootRelativePath,
    before: bool,
    after: bool,
}

impl SourceCollectionHistoryChange {
    /// Borrow the exact validated source-relative path.
    pub fn path(&self) -> &RootRelativePath {
        &self.path
    }
    /// Return the confirmed membership at the captured revision.
    pub const fn before(&self) -> bool {
        self.before
    }
    /// Return the explicit desired membership, never a toggle.
    pub const fn after(&self) -> bool {
        self.after
    }
}

/// Bounded owner-issued observation for later admitted history preparation.
///
/// No intent/receipt identity, persistence, capacity grant or replay authority is represented.
/// Later admission must reread and revalidate these facts inside its owner transition.
#[derive(Debug)]
pub struct SourceCollectionHistorySelection {
    revision: SourceRevision,
    directory_generation: u64,
    collection: SampleCollection,
    changes: Vec<SourceCollectionHistoryChange>,
}

impl SourceCollectionHistorySelection {
    /// Borrow the complete source/root/database/lifecycle/lease/revision observation.
    pub fn revision(&self) -> &SourceRevision {
        &self.revision
    }
    /// Return the audited directory publication generation.
    pub const fn directory_generation(&self) -> u64 {
        self.directory_generation
    }
    /// Return the single targeted collection slot.
    pub const fn collection(&self) -> SampleCollection {
        self.collection
    }
    /// Borrow changed paths in deterministic order; no-op paths are omitted.
    pub fn changes(&self) -> &[SourceCollectionHistoryChange] {
        &self.changes
    }
}

impl SourceWriter {
    /// Observe up to 128 exact confirmed collection memberships on this sole owner thread.
    ///
    /// SQL has a two-second cooperative deadline. Run on an owning worker, never a render
    /// callback: an OS identity check can stall without releasing retained source custody.
    /// This read does not renew a lease, prepare an intent or admit an undo operation.
    pub fn capture_collection_history_selection(
        &self,
        expected: &SourceRevision,
        collection: SampleCollection,
        desired: bool,
        paths: &[RootRelativePath],
    ) -> Result<SourceCollectionHistorySelection, SourceWriterError> {
        self.capture_collection_history_selection_with_deadline(
            expected,
            collection,
            desired,
            paths,
            Instant::now() + Duration::from_secs(2),
        )
    }

    pub(in super::super) fn capture_collection_history_selection_with_deadline(
        &self,
        expected: &SourceRevision,
        collection: SampleCollection,
        desired: bool,
        paths: &[RootRelativePath],
        deadline: Instant,
    ) -> Result<SourceCollectionHistorySelection, SourceWriterError> {
        if Instant::now() >= deadline {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        if paths.is_empty()
            || paths.len() > MAX_PATHS
            || usize::from(collection.index()) >= SampleCollection::COUNT
        {
            return Err(SourceWriterError::InvalidRequest(
                "invalid collection history selection",
            ));
        }
        // Count input bytes before retaining normalized keys or cloning revision identities.
        let mut bytes = std::mem::size_of::<SourceCollectionHistorySelection>();
        for len in [
            expected.source_id.as_str().len(),
            expected.root_identity.as_bytes().len(),
            expected.root_binding_identity.len(),
            expected.database_identity.len(),
        ] {
            if len > MAX_PATH_BYTES {
                return Err(SourceWriterError::SnapshotBudgetExceeded);
            }
            bytes = bytes
                .checked_add(len)
                .ok_or(SourceWriterError::SnapshotBudgetExceeded)?;
        }
        for path in paths {
            let len = path.as_path().as_os_str().len();
            if len > MAX_PATH_BYTES {
                return Err(SourceWriterError::SnapshotBudgetExceeded);
            }
            bytes = bytes
                .checked_add(
                    len.checked_mul(2)
                        .ok_or(SourceWriterError::SnapshotBudgetExceeded)?,
                )
                .and_then(|value| {
                    value.checked_add(
                        std::mem::size_of::<SourceCollectionHistoryChange>()
                            + std::mem::size_of::<String>(),
                    )
                })
                .ok_or(SourceWriterError::SnapshotBudgetExceeded)?;
        }
        if bytes > MAX_PAYLOAD_BYTES {
            return Err(SourceWriterError::SnapshotBudgetExceeded);
        }
        let mut normalized = BTreeSet::new();
        for path in paths {
            let key = normalize_relative_path(path.as_path())?;
            // RootRelativePath preserves native spelling. Normalization must not alias a
            // distinct native filename (for example a literal backslash on Unix).
            if Path::new(&key) != path.as_path() {
                return Err(SourceWriterError::InvalidRequest(
                    "collection history path is not an exact database key",
                ));
            }
            if !normalized.insert(key) {
                return Err(SourceWriterError::InvalidRequest(
                    "duplicate collection history path",
                ));
            }
        }
        let paths: Vec<_> = normalized.into_iter().collect();
        let _progress = SnapshotProgressGuard::install(&self.database.connection, deadline);
        let result = (|| {
            let _extension_scope = crate::sqlite_ext::try_extension_free_metadata_scope()
                .ok_or(SourceWriterError::InvalidRequest("unknown SQLite runtime"))?;
            super::super::super::schema::require_known_metadata_schema(&self.database.connection)?;
            let before = self.current_revision()?;
            if expected != &before {
                return Err(SourceWriterError::StaleFence);
            }
            let transaction = self
                .database
                .connection
                .unchecked_transaction()
                .map_err(map_sql_error)?;
            // The existing curation validator reads these strings. Bound them before it allocates,
            // inside this same read snapshot, even if another SQLite client corrupts metadata.
            let invalid: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE
                 (key IN ('revision', ?1) AND (typeof(value)!='text' OR length(CAST(value AS BLOB)) NOT BETWEEN 1 AND 20))
                 OR (key=?2 AND (typeof(value)!='text' OR length(CAST(value AS BLOB)) NOT BETWEEN 1 AND 24576)))",
                rusqlite::params![super::super::super::META_LAST_AUTHORITATIVE_AUDIT_REVISION,
                    super::super::super::META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY], |row| row.get(0),
            ).map_err(map_sql_error)?;
            if invalid {
                return Err(SourceDbError::Unexpected.into());
            }
            let directory_generation = super::super::curation::validate_curation_preconditions(
                &transaction,
                expected.revision(),
                expected.root_identity(),
                &paths,
            )?;
            let mut statement = transaction
                .prepare("SELECT collection FROM wav_file_collections WHERE path=?1 LIMIT 7")
                .map_err(map_sql_error)?;
            let mut changes = Vec::with_capacity(paths.len());
            for path in paths {
                let mut rows = statement.query([&path]).map_err(map_sql_error)?;
                let mut seen = 0_u8;
                while let Some(row) = rows.next().map_err(map_sql_error)? {
                    let slot =
                        SampleCollection::from_i64(row.get::<_, i64>(0).map_err(map_sql_error)?)
                            .ok_or(SourceDbError::Unexpected)?;
                    let bit = 1_u8 << slot.index();
                    if seen & bit != 0 {
                        return Err(SourceDbError::Unexpected.into());
                    }
                    seen |= bit;
                }
                let member = seen & (1_u8 << collection.index()) != 0;
                if member != desired {
                    changes.push(SourceCollectionHistoryChange {
                        path: RootRelativePath::try_from_path(std::path::PathBuf::from(path))
                            .map_err(|_| SourceDbError::Unexpected)?,
                        before: member,
                        after: desired,
                    });
                }
            }
            drop(statement);
            transaction.rollback().map_err(map_sql_error)?;
            if expected != &self.current_revision()? {
                return Err(SourceWriterError::StaleFence);
            }
            Ok(SourceCollectionHistorySelection {
                revision: before,
                directory_generation,
                collection,
                changes,
            })
        })();
        if Instant::now() >= deadline {
            Err(SourceWriterError::SnapshotBudgetExceeded)
        } else {
            result
        }
    }
}
