//! Durable source-local curation commands and their bounded cache patches.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use thiserror::Error;
use wavecrate_library::app_dirs::WritableProfileGuard;
use wavecrate_library::sample_sources::db::BrowserMetadataCursor;
use wavecrate_library::sample_sources::reconciliation::RootIdentity;
use wavecrate_library::sample_sources::{
    BrowserFileMetadata, SampleCollection, SourceCurationCommitReceipt, SourceCurationUpdate,
    SourceDatabase, SourceManifestEntry,
};

use super::{
    AdmissionState, CatalogCommand, CatalogError, CatalogEvent, EventReservation, RegisteredSource,
    SourceId, catalog_error_from_db, source_root_identity,
};

/// Maximum number of updates admitted by one durable curation command.
pub const MAX_CURATION_UPDATES: usize =
    wavecrate_library::sample_sources::SOURCE_CURATION_BATCH_LIMIT;

/// A typed failure raised before a durable curation command enters the worker queue.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CatalogCurationRequestError {
    /// No metadata updates were supplied.
    #[error("curation request is empty")]
    Empty,
    /// The request exceeded the bounded update admission limit.
    #[error("curation request contains {actual} updates; maximum is {max}")]
    TooManyUpdates {
        /// Maximum admitted update count.
        max: usize,
        /// Supplied update count.
        actual: usize,
    },
    /// A rating carried a value outside the supported range.
    #[error("curation request contains invalid rating value {value}")]
    InvalidRating {
        /// Rejected raw rating value.
        value: i8,
    },
    /// A collection carried a slot outside the supported range.
    #[error("curation request contains invalid collection slot {value}")]
    InvalidCollection {
        /// Rejected raw collection value.
        value: u8,
    },
    /// The source has no current desired lifecycle admission.
    #[error("source is not admitted for curation: {0}")]
    NotRegistered(SourceId),
    /// The source was admitted but has been made inactive by a remove.
    #[error("source is inactive for curation: {source_id} generation {lifecycle_generation}")]
    Inactive {
        /// Source whose desired lifecycle is inactive.
        source_id: SourceId,
        /// Lifecycle generation supplied by the request.
        lifecycle_generation: u64,
    },
    /// The request lifecycle does not match the current desired lifecycle.
    #[error(
        "curation lifecycle generation mismatch for {source_id}: expected {expected}, received {received}"
    )]
    StaleLifecycleGeneration {
        /// Source whose lifecycle changed.
        source_id: SourceId,
        /// Current desired lifecycle generation.
        expected: u64,
        /// Lifecycle generation supplied by the request.
        received: u64,
    },
}

/// Return whether a catalog command remains durable during shutdown draining.
pub(crate) fn is_durable(command: &CatalogCommand) -> bool {
    matches!(command, CatalogCommand::ApplyCuration { .. })
}

/// Validate curation input and its current desired lifecycle under the admission mutex.
pub(crate) fn validate_submission(
    command: &CatalogCommand,
    state: &AdmissionState,
) -> Result<(), CatalogCurationRequestError> {
    let CatalogCommand::ApplyCuration {
        source_id,
        lifecycle_generation,
        updates,
        ..
    } = command
    else {
        return Ok(());
    };

    validate_updates(updates)?;
    let Some(desired) = state.sources.get(source_id) else {
        return Err(CatalogCurationRequestError::NotRegistered(
            source_id.clone(),
        ));
    };
    if !desired.active {
        return Err(CatalogCurationRequestError::Inactive {
            source_id: source_id.clone(),
            lifecycle_generation: *lifecycle_generation,
        });
    }
    if desired.lifecycle_generation != *lifecycle_generation {
        return Err(CatalogCurationRequestError::StaleLifecycleGeneration {
            source_id: source_id.clone(),
            expected: desired.lifecycle_generation,
            received: *lifecycle_generation,
        });
    }
    Ok(())
}

fn validate_updates(updates: &[SourceCurationUpdate]) -> Result<(), CatalogCurationRequestError> {
    if updates.is_empty() {
        return Err(CatalogCurationRequestError::Empty);
    }
    if updates.len() > MAX_CURATION_UPDATES {
        return Err(CatalogCurationRequestError::TooManyUpdates {
            max: MAX_CURATION_UPDATES,
            actual: updates.len(),
        });
    }
    for update in updates {
        match update {
            SourceCurationUpdate::SetRating { rating, .. } if !(-3..=3).contains(&rating.val()) => {
                return Err(CatalogCurationRequestError::InvalidRating {
                    value: rating.val(),
                });
            }
            SourceCurationUpdate::AddCollection { collection, .. }
            | SourceCurationUpdate::RemoveCollection { collection, .. }
                if usize::from(collection.index()) >= SampleCollection::COUNT =>
            {
                return Err(CatalogCurationRequestError::InvalidCollection {
                    value: collection.index(),
                });
            }
            SourceCurationUpdate::SetLocked { .. } => {}
            _ => {}
        }
    }
    Ok(())
}

/// Load the full browser curation projection through revision-fenced pages.
pub(crate) fn load_snapshot_curation_metadata(
    database: &SourceDatabase,
    revision: u64,
    manifest: &[SourceManifestEntry],
    mut should_cancel: impl FnMut() -> bool,
) -> Result<BTreeMap<PathBuf, BrowserFileMetadata>, CatalogError> {
    let mut metadata = BTreeMap::new();
    let mut cursor: Option<BrowserMetadataCursor> = None;
    loop {
        if should_cancel() {
            return Err(CatalogError::Canceled);
        }
        let page = database
            .browser_metadata_page(revision, cursor.as_ref(), 64)
            .map_err(catalog_error_from_db)?;
        if page.revision != revision {
            return Err(CatalogError::StaleRevision {
                expected: revision,
                actual: page.revision,
            });
        }
        for file in page.files {
            // Browser pages also retain historical missing rows. They are outside the live
            // manifest and therefore do not belong in the curation projection.
            if file.missing {
                continue;
            }
            let Ok(index) =
                manifest.binary_search_by(|entry| entry.relative_path.cmp(&file.relative_path))
            else {
                return Err(CatalogError::DirectoryTruthUnavailable(format!(
                    "browser metadata path {} is absent from manifest revision {revision}",
                    file.relative_path.display()
                )));
            };
            let manifest_entry = &manifest[index];
            if manifest_entry.file_size != file.file_size
                || manifest_entry.modified_ns != file.modified_ns
            {
                return Err(CatalogError::DirectoryTruthUnavailable(format!(
                    "browser metadata facts for {} differ from manifest revision {revision}",
                    file.relative_path.display()
                )));
            }
            if metadata.insert(file.relative_path.clone(), file).is_some() {
                return Err(CatalogError::DirectoryTruthUnavailable(String::from(
                    "browser metadata page returned duplicate paths",
                )));
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    if metadata.len() != manifest.len() {
        return Err(CatalogError::DirectoryTruthUnavailable(format!(
            "browser metadata projection contains {} live rows for {} manifest rows",
            metadata.len(),
            manifest.len()
        )));
    }
    Ok(metadata)
}

/// Execute one accepted durable curation command and publish exactly one terminal event.
pub(crate) fn execute(
    source_id: SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    root_identity: RootIdentity,
    request_id: u64,
    updates: Vec<SourceCurationUpdate>,
    profile_guard: &WritableProfileGuard,
    reservation: &mut EventReservation<'_>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) {
    let result = execute_inner(
        &source_id,
        lifecycle_generation,
        expected_revision,
        &root_identity,
        &updates,
        profile_guard,
        sources,
    );
    match result {
        Ok(receipt) => reservation.publish(CatalogEvent::CurationCommitted {
            source_id,
            lifecycle_generation,
            request_id,
            receipt,
        }),
        Err(error) => reservation.publish(CatalogEvent::CurationFailed {
            source_id,
            lifecycle_generation,
            expected_revision,
            request_id,
            error,
        }),
    }
}

fn execute_inner(
    source_id: &SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    expected_root_identity: &RootIdentity,
    updates: &[SourceCurationUpdate],
    profile_guard: &WritableProfileGuard,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) -> Result<SourceCurationCommitReceipt, CatalogError> {
    let Some(source) = sources.get_mut(source_id) else {
        return Err(CatalogError::NotRegistered(source_id.clone()));
    };
    ensure_generation(source_id, source.lifecycle_generation, lifecycle_generation)?;

    profile_guard
        .validate_current()
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;

    let Some(snapshot) = source.snapshot.as_ref() else {
        return Err(CatalogError::SnapshotUnavailable);
    };
    if snapshot.source_id != *source_id {
        return Err(CatalogError::SnapshotUnavailable);
    }
    ensure_generation(
        source_id,
        snapshot.lifecycle_generation,
        lifecycle_generation,
    )?;
    if snapshot.committed_revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: snapshot.committed_revision,
        });
    }
    if snapshot.root_identity != *expected_root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("curation root identity differs from the retained snapshot"),
        });
    }
    if source
        .known_root_identity
        .as_ref()
        .is_some_and(|identity| identity != expected_root_identity)
    {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("curation root identity differs from the retained source"),
        });
    }

    let before_root_identity = checked_source_root_identity(source_id, &source.root)?;
    if before_root_identity != *expected_root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("source root identity differs from the curation request"),
        });
    }
    let before_revision = source
        .database
        .get_revision()
        .map_err(catalog_error_from_db)?;
    if before_revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: before_revision,
        });
    }

    profile_guard
        .validate_current()
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    let before_commit_root_identity = checked_source_root_identity(source_id, &source.root)?;
    if before_commit_root_identity != *expected_root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("source root identity changed before curation commit"),
        });
    }

    let receipt = source
        .database
        .commit_curation_with_profile_guard(
            profile_guard,
            expected_revision,
            expected_root_identity,
            updates,
        )
        .map_err(catalog_error_from_db)?;

    // The library receipt is durable authority. Any post-commit check failure only invalidates
    // the retained cache; it must never turn a successful database commit into CurationFailed.
    if !post_commit_binding_is_current(
        source_id,
        source,
        profile_guard,
        expected_root_identity,
        receipt.source_revision(),
    ) || !patch_cached_snapshot(source, expected_root_identity, expected_revision, &receipt)
    {
        source.snapshot = None;
    }
    Ok(receipt)
}

fn post_commit_binding_is_current(
    source_id: &SourceId,
    source: &RegisteredSource,
    profile_guard: &WritableProfileGuard,
    expected_root_identity: &RootIdentity,
    committed_revision: u64,
) -> bool {
    profile_guard.validate_current().is_ok()
        && source
            .database
            .validate_profile_guard(profile_guard)
            .is_ok()
        && checked_source_root_identity(source_id, &source.root)
            .is_ok_and(|identity| identity == *expected_root_identity)
        && source
            .database
            .get_revision()
            .is_ok_and(|revision| revision == committed_revision)
}

fn patch_cached_snapshot(
    source: &mut RegisteredSource,
    expected_root_identity: &RootIdentity,
    expected_revision: u64,
    receipt: &SourceCurationCommitReceipt,
) -> bool {
    let Some(snapshot) = source.snapshot.as_mut() else {
        return false;
    };
    let Some(next_revision) = expected_revision.checked_add(1) else {
        return false;
    };
    if receipt.source_revision() != next_revision
        || receipt.root_identity() != expected_root_identity
        || receipt.directory_generation() != snapshot.directory_generation
    {
        return false;
    }
    for file in receipt.files() {
        if file.missing {
            return false;
        }
        let Ok(index) = snapshot
            .manifest
            .binary_search_by(|entry| entry.relative_path.cmp(&file.relative_path))
        else {
            return false;
        };
        let manifest_entry = &snapshot.manifest[index];
        if manifest_entry.file_size != file.file_size
            || manifest_entry.modified_ns != file.modified_ns
        {
            return false;
        }
    }
    snapshot.committed_revision = receipt.source_revision();
    for file in receipt.files() {
        snapshot
            .curation_metadata
            .insert(file.relative_path.clone(), file.clone());
    }
    true
}

fn ensure_generation(
    source_id: &SourceId,
    expected: u64,
    received: u64,
) -> Result<(), CatalogError> {
    if expected == received {
        Ok(())
    } else {
        Err(CatalogError::StaleLifecycleGeneration {
            source_id: source_id.clone(),
            expected,
            received,
        })
    }
}

fn checked_source_root_identity(
    source_id: &SourceId,
    root: &std::path::Path,
) -> Result<RootIdentity, CatalogError> {
    source_root_identity(root).map_err(|error| match error {
        CatalogError::StaleRootGeneration { detail, .. } => CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail,
        },
        error => error,
    })
}
