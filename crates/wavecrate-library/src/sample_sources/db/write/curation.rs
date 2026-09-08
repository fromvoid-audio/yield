use std::{collections::BTreeSet, path::PathBuf};

use rusqlite::{OptionalExtension, Transaction};

use crate::app_dirs::WritableProfileGuard;
use crate::sample_sources::reconciliation::{RootIdentity, RootRelativePath};

use super::super::read::browser_metadata_rows_for_paths;
use super::super::tags::validate_curation_tag_label;
use super::super::util::map_sql_error;
use super::super::{
    META_LAST_AUTHORITATIVE_AUDIT_REVISION, Rating, SampleCollection, SourceDatabase,
    SourceDbError, SourceDirectoryTruthError, SourceDirectoryTruthUnavailableReason,
};

/// Maximum number of curation updates accepted by one metadata-continuity transaction.
pub const SOURCE_CURATION_BATCH_LIMIT: usize = 128;

/// One bounded source-local curation update addressed by a validated root-relative path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceCurationUpdate {
    /// Record or clear playback history for one live source member.
    SetLastPlayedAt {
        /// Source-relative file locator.
        path: RootRelativePath,
        /// Epoch seconds of the accepted playback, or `None` to clear history.
        played_at: Option<i64>,
    },
    /// Set the keep/trash rating for one live source member.
    SetRating {
        /// Source-relative file locator.
        path: RootRelativePath,
        /// Rating to persist.
        rating: Rating,
    },
    /// Set whether the keep rating is locked for one live source member.
    SetLocked {
        /// Source-relative file locator.
        path: RootRelativePath,
        /// Whether the keep rating should be locked.
        locked: bool,
    },
    /// Add one fixed collection membership for one live source member.
    AddCollection {
        /// Source-relative file locator.
        path: RootRelativePath,
        /// Collection slot to add.
        collection: SampleCollection,
    },
    /// Remove one fixed collection membership for one live source member.
    RemoveCollection {
        /// Source-relative file locator.
        path: RootRelativePath,
        /// Collection slot to remove.
        collection: SampleCollection,
    },
    /// Add one normalized tag assignment for one live source member.
    AddTag {
        /// Source-relative file locator.
        path: RootRelativePath,
        /// Tag label to normalize and assign.
        label: String,
    },
    /// Remove one normalized tag assignment for one live source member.
    RemoveTag {
        /// Source-relative file locator.
        path: RootRelativePath,
        /// Tag label whose normalized identity should be removed.
        label: String,
    },
    /// Add one existing source-local tag identity to one live source member.
    AssignExistingTag {
        /// Source-relative file locator.
        path: RootRelativePath,
        /// Positive source-local tag row identifier returned by the source tag catalog.
        tag_id: i64,
    },
    /// Remove one existing source-local tag identity from one live source member.
    RemoveExistingTag {
        /// Source-relative file locator.
        path: RootRelativePath,
        /// Positive source-local tag row identifier returned by the source tag catalog.
        tag_id: i64,
    },
}

impl SourceCurationUpdate {
    fn path(&self) -> &RootRelativePath {
        match self {
            Self::SetLastPlayedAt { path, .. }
            | Self::SetRating { path, .. }
            | Self::SetLocked { path, .. }
            | Self::AddCollection { path, .. }
            | Self::RemoveCollection { path, .. }
            | Self::AddTag { path, .. }
            | Self::RemoveTag { path, .. }
            | Self::AssignExistingTag { path, .. }
            | Self::RemoveExistingTag { path, .. } => path,
        }
    }
}

/// Opaque evidence returned after a bounded metadata-continuity commit.
///
/// The receipt carries only the exact committed revision, the audited root identity, the active
/// directory generation, and metadata rows for the requested paths. It does not grant manifest or
/// source-audit authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceCurationCommitReceipt {
    source_revision: u64,
    root_identity: RootIdentity,
    directory_generation: u64,
    files: Vec<super::super::BrowserFileMetadata>,
}

impl SourceCurationCommitReceipt {
    /// Return the exact source revision committed by this curation transaction.
    pub const fn source_revision(&self) -> u64 {
        self.source_revision
    }

    /// Borrow the audited physical source-root identity retained by this commit.
    pub fn root_identity(&self) -> &RootIdentity {
        &self.root_identity
    }

    /// Return the active directory-truth generation retained by this commit.
    pub const fn directory_generation(&self) -> u64 {
        self.directory_generation
    }

    /// Borrow browser metadata rows for the requested paths in deterministic path order.
    pub fn files(&self) -> &[super::super::BrowserFileMetadata] {
        &self.files
    }
}

impl SourceDatabase {
    /// Commit a bounded curation batch while preserving authoritative source continuity.
    ///
    /// The held profile guard and the retained database binding are checked before opening the
    /// writer transaction and again immediately before commit. The transaction accepts only live,
    /// supported manifest members and advances the generic source revision, active directory
    /// publication revision, and authoritative-audit revision together.
    pub fn commit_curation_with_profile_guard(
        &self,
        profile_guard: &WritableProfileGuard,
        expected_source_revision: u64,
        expected_root_identity: &RootIdentity,
        updates: &[SourceCurationUpdate],
    ) -> Result<SourceCurationCommitReceipt, SourceDbError> {
        if updates.is_empty() || updates.len() > SOURCE_CURATION_BATCH_LIMIT {
            return Err(SourceDbError::Unexpected);
        }
        validate_update_values(updates)?;
        let next_source_revision = expected_source_revision
            .checked_add(1)
            .ok_or(SourceDbError::Unexpected)?;
        let normalized_paths = normalize_update_paths(updates)?;

        self.validate_profile_guard(profile_guard)?;
        let mut batch = self.write_batch()?;
        let directory_generation = validate_curation_preconditions(
            batch.curation_transaction(),
            expected_source_revision,
            expected_root_identity,
            &normalized_paths,
        )?;

        for update in updates {
            match update {
                SourceCurationUpdate::SetLastPlayedAt { path, played_at } => match played_at {
                    Some(value) => batch.set_last_played_at(path.as_path(), *value)?,
                    None => batch.clear_last_played_at(path.as_path())?,
                },
                SourceCurationUpdate::SetRating { path, rating } => {
                    batch.set_tag(path.as_path(), *rating)?;
                }
                SourceCurationUpdate::SetLocked { path, locked } => {
                    batch.set_locked(path.as_path(), *locked)?;
                }
                SourceCurationUpdate::AddCollection { path, collection } => {
                    batch.add_collection(path.as_path(), *collection)?;
                }
                SourceCurationUpdate::RemoveCollection { path, collection } => {
                    batch.remove_collection(path.as_path(), *collection)?;
                }
                SourceCurationUpdate::AddTag { path, label } => {
                    batch.assign_tag_for_curation(path.as_path(), label)?;
                }
                SourceCurationUpdate::RemoveTag { path, label } => {
                    batch.remove_tag_from_path(path.as_path(), label)?;
                }
                SourceCurationUpdate::AssignExistingTag { path, tag_id } => {
                    batch.assign_existing_tag_for_curation(path.as_path(), *tag_id)?;
                }
                SourceCurationUpdate::RemoveExistingTag { path, tag_id } => {
                    batch.remove_existing_tag_for_curation(path.as_path(), *tag_id)?;
                }
            }
        }

        SourceDatabase::bump_revision(batch.curation_transaction())?;
        let committed_source_revision = source_revision(batch.curation_transaction())?;
        if committed_source_revision != next_source_revision {
            return Err(SourceDbError::Unexpected);
        }

        let next_revision_sql =
            i64::try_from(next_source_revision).map_err(|_| SourceDbError::Unexpected)?;
        let expected_revision_sql =
            i64::try_from(expected_source_revision).map_err(|_| SourceDbError::Unexpected)?;
        let changed = batch
            .curation_transaction()
            .execute(
                "UPDATE source_directory_generations
                 SET published_source_revision = ?1
                 WHERE status = 'active'
                   AND published_source_revision = ?2",
                rusqlite::params![next_revision_sql, expected_revision_sql],
            )
            .map_err(map_sql_error)?;
        if changed != 1 {
            return Err(curation_requires_audit(
                SourceDirectoryTruthUnavailableReason::Malformed,
            ));
        }

        let changed = batch
            .curation_transaction()
            .execute(
                "UPDATE metadata
                 SET value = ?1
                 WHERE key = ?2 AND value = ?3",
                rusqlite::params![
                    next_source_revision.to_string(),
                    META_LAST_AUTHORITATIVE_AUDIT_REVISION,
                    expected_source_revision.to_string()
                ],
            )
            .map_err(map_sql_error)?;
        if changed != 1 {
            return Err(curation_requires_audit(
                SourceDirectoryTruthUnavailableReason::Malformed,
            ));
        }

        let browser_paths = normalized_paths
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        let files = browser_metadata_rows_for_paths(batch.curation_transaction(), &browser_paths)?;
        if files.len() != browser_paths.len() {
            return Err(SourceDbError::Unexpected);
        }

        self.validate_profile_guard(profile_guard)?;
        batch.commit_curation_prepared()?;
        Ok(SourceCurationCommitReceipt {
            source_revision: committed_source_revision,
            root_identity: expected_root_identity.clone(),
            directory_generation,
            files,
        })
    }
}

fn normalize_update_paths(updates: &[SourceCurationUpdate]) -> Result<Vec<String>, SourceDbError> {
    let mut paths = BTreeSet::new();
    for update in updates {
        paths.insert(super::super::normalize_relative_path(
            update.path().as_path(),
        )?);
    }
    Ok(paths.into_iter().collect())
}

fn validate_update_values(updates: &[SourceCurationUpdate]) -> Result<(), SourceDbError> {
    for update in updates {
        match update {
            SourceCurationUpdate::SetRating { rating, .. } if !(-3..=3).contains(&rating.val()) => {
                return Err(SourceDbError::Unexpected);
            }
            SourceCurationUpdate::AddCollection { collection, .. }
            | SourceCurationUpdate::RemoveCollection { collection, .. }
                if usize::from(collection.index()) >= SampleCollection::COUNT =>
            {
                return Err(SourceDbError::Unexpected);
            }
            SourceCurationUpdate::AddTag { label, .. }
            | SourceCurationUpdate::RemoveTag { label, .. } => {
                validate_curation_tag_label(label)?;
            }
            SourceCurationUpdate::AssignExistingTag { tag_id, .. }
            | SourceCurationUpdate::RemoveExistingTag { tag_id, .. } => {
                if *tag_id <= 0 {
                    return Err(SourceDbError::InvalidTagId { tag_id: *tag_id });
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_curation_preconditions(
    transaction: &Transaction<'_>,
    expected_source_revision: u64,
    expected_root_identity: &RootIdentity,
    normalized_paths: &[String],
) -> Result<u64, SourceDbError> {
    let actual_source_revision = source_revision(transaction)?;
    if actual_source_revision != expected_source_revision {
        return Err(SourceDirectoryTruthError::StaleRevision {
            expected: expected_source_revision,
            actual: actual_source_revision,
        }
        .into());
    }

    let (stored_identity, stored_audit_revision): (Option<String>, Option<String>) = transaction
        .query_row(
            "SELECT
                (SELECT value FROM metadata WHERE key = ?1),
                (SELECT value FROM metadata WHERE key = ?2)",
            rusqlite::params![
                super::super::META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY,
                META_LAST_AUTHORITATIVE_AUDIT_REVISION
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(map_sql_error)?;
    let (Some(stored_identity), Some(stored_audit_revision)) =
        (stored_identity, stored_audit_revision)
    else {
        return Err(curation_requires_audit(
            SourceDirectoryTruthUnavailableReason::AuditRequired,
        ));
    };
    let stored_audit_revision = stored_audit_revision
        .parse::<u64>()
        .map_err(|_| curation_requires_audit(SourceDirectoryTruthUnavailableReason::Malformed))?;
    let stored_identity = serde_json::from_str::<Vec<u8>>(&stored_identity)
        .map(RootIdentity::from_bytes)
        .map_err(|_| curation_requires_audit(SourceDirectoryTruthUnavailableReason::Malformed))?;
    if stored_audit_revision != expected_source_revision
        || stored_identity != *expected_root_identity
    {
        return Err(curation_requires_audit(
            SourceDirectoryTruthUnavailableReason::AuditRequired,
        ));
    }

    if query_exists(
        transaction,
        "SELECT 1 FROM source_manifest_audit_state LIMIT 1",
    )? || query_exists(
        transaction,
        "SELECT 1 FROM source_manifest_audit_seen LIMIT 1",
    )? || query_exists(
        transaction,
        "SELECT 1 FROM source_directory_generations WHERE status = 'staging' LIMIT 1",
    )? {
        return Err(curation_requires_audit(
            SourceDirectoryTruthUnavailableReason::AuditRequired,
        ));
    }

    let mut statement = transaction
        .prepare(
            "SELECT generation, expected_entry_count, staged_entry_count, complete,
                    published_source_revision
             FROM source_directory_generations
             WHERE status = 'active'
             ORDER BY generation ASC
             LIMIT 2",
        )
        .map_err(map_sql_error)?;
    let active_generations = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })
        .map_err(map_sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sql_error)?;
    drop(statement);
    if active_generations.len() != 1 {
        return Err(curation_requires_audit(
            SourceDirectoryTruthUnavailableReason::Malformed,
        ));
    }
    let (generation, expected_count, staged_count, complete, published_revision) =
        active_generations[0];
    let Some(published_revision) = published_revision else {
        return Err(curation_requires_audit(
            SourceDirectoryTruthUnavailableReason::Malformed,
        ));
    };
    if generation <= 0
        || expected_count < 0
        || staged_count < 0
        || complete != 1
        || expected_count != staged_count
        || published_revision <= 0
        || u64::try_from(published_revision).ok() != Some(expected_source_revision)
    {
        return Err(curation_requires_audit(
            SourceDirectoryTruthUnavailableReason::Malformed,
        ));
    }

    validate_live_supported_paths(transaction, normalized_paths)?;
    u64::try_from(generation).map_err(|_| SourceDbError::Unexpected)
}

fn validate_live_supported_paths(
    transaction: &Transaction<'_>,
    normalized_paths: &[String],
) -> Result<(), SourceDbError> {
    if normalized_paths.is_empty() {
        return Err(SourceDbError::Unexpected);
    }
    let placeholders = (1..=normalized_paths.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let supported_filter = crate::sample_sources::supported_audio_where_clause();
    let sql = format!(
        "SELECT path FROM wav_files
         WHERE {supported_filter} AND missing = 0 AND path IN ({placeholders})"
    );
    let mut statement = transaction.prepare(&sql).map_err(map_sql_error)?;
    let found = statement
        .query_map(rusqlite::params_from_iter(normalized_paths.iter()), |row| {
            row.get::<_, String>(0)
        })
        .map_err(map_sql_error)?
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(map_sql_error)?;
    if found != normalized_paths.iter().cloned().collect() {
        return Err(SourceDbError::Unexpected);
    }
    Ok(())
}

fn query_exists(transaction: &Transaction<'_>, sql: &str) -> Result<bool, SourceDbError> {
    transaction
        .query_row(sql, [], |_| Ok(()))
        .optional()
        .map(|value| value.is_some())
        .map_err(map_sql_error)
}

fn source_revision(transaction: &Transaction<'_>) -> Result<u64, SourceDbError> {
    transaction
        .query_row(
            "SELECT value FROM metadata WHERE key = 'revision'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(map_sql_error)?
        .map(|raw| raw.parse::<u64>().map_err(|_| SourceDbError::Unexpected))
        .transpose()
        .map(|revision| revision.unwrap_or_default())
}

fn curation_requires_audit(reason: SourceDirectoryTruthUnavailableReason) -> SourceDbError {
    SourceDirectoryTruthError::RequiresAudit { reason }.into()
}
