use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use wavecrate_library::app_dirs::WritableProfileGuard;
use wavecrate_library::sample_sources::db::normalize_relative_path;
use wavecrate_library::sample_sources::reconciliation::{RootIdentity, RootRelativePath};
use wavecrate_library::sample_sources::{SourceDisplayLabelEntry, SourceId};

use super::{
    AdmissionState, CatalogError, CatalogSnapshot, RegisteredSource, publication_is_current,
    source_root_identity,
};

/// A completed bounded display-label read before it is published to the catalog event queue.
pub(crate) struct DisplayLabelsPatch {
    pub(crate) root_identity: RootIdentity,
    pub(crate) directory_generation: u64,
    pub(crate) revision: u64,
    pub(crate) requested_paths: Vec<PathBuf>,
    pub(crate) entries: Vec<SourceDisplayLabelEntry>,
}

pub(crate) fn read(
    source: &RegisteredSource,
    source_id: &SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    requested_paths: Vec<PathBuf>,
    token: &Arc<AtomicBool>,
    admission: &Arc<Mutex<AdmissionState>>,
    profile_guard: &WritableProfileGuard,
) -> Result<DisplayLabelsPatch, CatalogError> {
    ensure_current(admission, source_id, lifecycle_generation, token)?;
    profile_guard.validate_current().map_err(|error| {
        super::catalog_error_from_db_for_source(
            source_id,
            wavecrate_library::sample_sources::SourceDbError::ProfileOwnership(error),
        )
    })?;
    source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;

    let snapshot = source
        .snapshot
        .as_ref()
        .ok_or(CatalogError::SnapshotUnavailable)?;
    ensure_snapshot_identity(snapshot, source, source_id, lifecycle_generation)?;
    if snapshot.committed_revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: snapshot.committed_revision,
        });
    }

    let expected_root_identity = source
        .known_root_identity
        .clone()
        .unwrap_or_else(|| snapshot.root_identity.clone());
    if expected_root_identity != snapshot.root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("retained source root identity differs from snapshot identity"),
        });
    }

    let before_root_identity = checked_source_root_identity(source_id, &source.root)?;
    if before_root_identity != expected_root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("source root identity differs from the retained snapshot"),
        });
    }
    let before_revision = source
        .database
        .get_revision()
        .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
    if before_revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: before_revision,
        });
    }
    ensure_current(admission, source_id, lifecycle_generation, token)?;
    validate_live_selected_paths(snapshot, source_id, &requested_paths)?;

    let result = source
        .database
        .display_labels_for_paths_at_revision(expected_revision, &requested_paths, token)
        .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
    if result.revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: result.revision,
        });
    }
    if result.entries.len() > wavecrate_library::sample_sources::SOURCE_DISPLAY_LABEL_MAX_PATHS {
        return Err(CatalogError::Database(String::from(
            "display-label result exceeded its bounded entry limit",
        )));
    }

    ensure_current(admission, source_id, lifecycle_generation, token)?;
    profile_guard.validate_current().map_err(|error| {
        super::catalog_error_from_db_for_source(
            source_id,
            wavecrate_library::sample_sources::SourceDbError::ProfileOwnership(error),
        )
    })?;
    source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
    let after_root_identity = checked_source_root_identity(source_id, &source.root)?;
    if after_root_identity != expected_root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("source root identity changed during display-label read"),
        });
    }
    let after_revision = source
        .database
        .get_revision()
        .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
    if after_revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: after_revision,
        });
    }
    let retained_snapshot = source
        .snapshot
        .as_ref()
        .ok_or(CatalogError::SnapshotUnavailable)?;
    if retained_snapshot.committed_revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: retained_snapshot.committed_revision,
        });
    }
    if retained_snapshot.root_identity != expected_root_identity
        || retained_snapshot.directory_generation != snapshot.directory_generation
    {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("retained display-label snapshot changed during read"),
        });
    }
    ensure_current(admission, source_id, lifecycle_generation, token)?;

    Ok(DisplayLabelsPatch {
        root_identity: expected_root_identity,
        directory_generation: snapshot.directory_generation,
        revision: expected_revision,
        requested_paths,
        entries: result.entries,
    })
}

fn validate_live_selected_paths(
    snapshot: &CatalogSnapshot,
    source_id: &SourceId,
    relative_paths: &[PathBuf],
) -> Result<(), CatalogError> {
    for path in relative_paths {
        let validated = RootRelativePath::try_from_path(path.clone()).map_err(|error| {
            CatalogError::Database(format!(
                "{source_id}: invalid display-label path {path:?}: {error}"
            ))
        })?;
        let normalized = normalize_relative_path(validated.as_path())
            .map(PathBuf::from)
            .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
        if snapshot
            .manifest
            .binary_search_by(|entry| entry.relative_path.cmp(&normalized))
            .is_err()
        {
            return Err(CatalogError::DisplayLabelPathMissing {
                source_id: source_id.clone(),
                path: normalized,
            });
        }
    }
    Ok(())
}

fn ensure_current(
    admission: &Arc<Mutex<AdmissionState>>,
    source_id: &SourceId,
    lifecycle_generation: u64,
    token: &Arc<AtomicBool>,
) -> Result<(), CatalogError> {
    if token.load(Ordering::Acquire)
        || !publication_is_current(admission, source_id, lifecycle_generation, Some(token))
    {
        Err(CatalogError::Canceled)
    } else {
        Ok(())
    }
}

fn ensure_snapshot_identity(
    snapshot: &CatalogSnapshot,
    source: &RegisteredSource,
    source_id: &SourceId,
    lifecycle_generation: u64,
) -> Result<(), CatalogError> {
    if snapshot.source_id != *source_id {
        return Err(CatalogError::Audit(String::from(
            "retained snapshot source identity does not match request",
        )));
    }
    if snapshot.lifecycle_generation != lifecycle_generation
        || source.lifecycle_generation != lifecycle_generation
    {
        let expected = source.lifecycle_generation;
        return Err(CatalogError::StaleLifecycleGeneration {
            source_id: source_id.clone(),
            expected,
            received: lifecycle_generation,
        });
    }
    Ok(())
}

fn checked_source_root_identity(
    source_id: &SourceId,
    root: &Path,
) -> Result<RootIdentity, CatalogError> {
    source_root_identity(root).map_err(|error| match error {
        CatalogError::StaleRootGeneration { detail, .. } => CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail,
        },
        error => error,
    })
}
