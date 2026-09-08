use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use wavecrate_library::app_dirs::WritableProfileGuard;
use wavecrate_library::sample_sources::db::normalize_relative_path;
use wavecrate_library::sample_sources::reconciliation::{RootIdentity, RootRelativePath};
use wavecrate_library::sample_sources::{SourceId, SourceTagPage, SourceTagPageCursor};

use super::{
    AdmissionState, CatalogError, CatalogSnapshot, CatalogTagPageQuery, RegisteredSource,
    publication_is_current, source_root_identity,
};

/// A completed bounded tag read before it is published to the catalog event queue.
pub(crate) struct TagPagePatch {
    pub(crate) root_identity: RootIdentity,
    pub(crate) revision: u64,
    pub(crate) page: SourceTagPage,
}

/// A completed bounded tag-filter read before it is published to the catalog event queue.
pub(crate) struct TagFilterPatch {
    pub(crate) root_identity: RootIdentity,
    pub(crate) directory_generation: u64,
    pub(crate) revision: u64,
    pub(crate) manifest_len: usize,
    pub(crate) mask: Vec<u64>,
}

pub(crate) fn read_page(
    source: &RegisteredSource,
    source_id: &SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    query: &CatalogTagPageQuery,
    after_cursor: Option<&SourceTagPageCursor>,
    page_limit: usize,
    token: &Arc<AtomicBool>,
    admission: &Arc<Mutex<AdmissionState>>,
    profile_guard: &WritableProfileGuard,
) -> Result<TagPagePatch, CatalogError> {
    ensure_current(admission, source_id, lifecycle_generation, token)?;
    profile_guard
        .validate_current()
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;

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
        .map_err(super::catalog_error_from_db)?;
    if before_revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: before_revision,
        });
    }
    ensure_current(admission, source_id, lifecycle_generation, token)?;

    match query {
        CatalogTagPageQuery::SelectedPaths { relative_paths }
        | CatalogTagPageQuery::SelectedTagIds { relative_paths, .. } => {
            validate_live_selected_paths(snapshot, source_id, relative_paths)?
        }
        CatalogTagPageQuery::VocabularyPrefix { .. } => {}
    }

    let page = match query {
        CatalogTagPageQuery::SelectedPaths { relative_paths } => source
            .database
            .tags_for_paths_page_at_revision_with_cancellation(
                expected_revision,
                relative_paths,
                after_cursor,
                page_limit,
                || token.load(Ordering::Acquire),
            )
            .map_err(super::catalog_error_from_db)?,
        CatalogTagPageQuery::SelectedTagIds {
            relative_paths,
            tag_ids,
        } => source
            .database
            .tags_for_paths_and_ids_page_at_revision(
                expected_revision,
                relative_paths,
                tag_ids,
                after_cursor,
                page_limit,
                token,
            )
            .map_err(super::catalog_error_from_db)?,
        CatalogTagPageQuery::VocabularyPrefix { prefix } => source
            .database
            .tag_vocabulary_page_at_revision(expected_revision, prefix, after_cursor, page_limit)
            .map_err(super::catalog_error_from_db)?,
    };
    if page.revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: page.revision,
        });
    }
    if page.entries.len() > super::SOURCE_TAG_PAGE_MAX_PAGE_SIZE {
        return Err(CatalogError::Database(String::from(
            "tag page exceeded its bounded entry limit",
        )));
    }

    ensure_current(admission, source_id, lifecycle_generation, token)?;
    profile_guard
        .validate_current()
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    let after_root_identity = checked_source_root_identity(source_id, &source.root)?;
    if after_root_identity != expected_root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("source root identity changed during tag read"),
        });
    }
    let after_revision = source
        .database
        .get_revision()
        .map_err(super::catalog_error_from_db)?;
    if after_revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: after_revision,
        });
    }
    let retained_revision = source
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.committed_revision)
        .ok_or(CatalogError::SnapshotUnavailable)?;
    if retained_revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: retained_revision,
        });
    }
    ensure_current(admission, source_id, lifecycle_generation, token)?;

    Ok(TagPagePatch {
        root_identity: expected_root_identity,
        revision: expected_revision,
        page,
    })
}

pub(crate) fn read_filter(
    source: &RegisteredSource,
    source_id: &SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    expected_manifest_len: usize,
    required_tags: &[String],
    token: &Arc<AtomicBool>,
    admission: &Arc<Mutex<AdmissionState>>,
    profile_guard: &WritableProfileGuard,
) -> Result<TagFilterPatch, CatalogError> {
    ensure_current(admission, source_id, lifecycle_generation, token)?;
    profile_guard
        .validate_current()
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;

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
    if snapshot.manifest.len() != expected_manifest_len {
        return Err(CatalogError::TagFilterManifestLengthMismatch {
            expected: expected_manifest_len,
            actual: snapshot.manifest.len(),
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
        .map_err(super::catalog_error_from_db)?;
    if before_revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: before_revision,
        });
    }
    ensure_current(admission, source_id, lifecycle_generation, token)?;

    let mask_words = expected_manifest_len
        .checked_add(63)
        .map(|rounded| rounded / 64)
        .ok_or(CatalogError::TagFilterManifestLengthMismatch {
            expected: expected_manifest_len,
            actual: snapshot.manifest.len(),
        })?;
    let mask_bytes = mask_words.checked_mul(std::mem::size_of::<u64>()).ok_or(
        CatalogError::TagFilterManifestLengthMismatch {
            expected: expected_manifest_len,
            actual: snapshot.manifest.len(),
        },
    )?;
    if mask_bytes > super::MAX_TAG_FILTER_MASK_BYTES {
        return Err(CatalogError::TagFilterManifestLengthMismatch {
            expected: expected_manifest_len,
            actual: snapshot.manifest.len(),
        });
    }
    let mut mask = vec![0_u64; mask_words];
    let mut snapshot_mismatch = false;
    let filter_result = source.database.visit_paths_matching_tags_at_revision(
        expected_revision,
        required_tags,
        token,
        |path| {
            let path = Path::new(path);
            let Some(entry_index) = snapshot
                .manifest
                .binary_search_by(|entry| entry.relative_path.as_path().cmp(path))
                .ok()
            else {
                snapshot_mismatch = true;
                return Err(wavecrate_library::sample_sources::SourceDbError::Unexpected);
            };
            mask[entry_index / 64] |= 1_u64 << (entry_index % 64);
            Ok(())
        },
    );
    if snapshot_mismatch {
        return Err(CatalogError::TagFilterSnapshotMismatch {
            source_id: source_id.clone(),
            expected_manifest_len,
        });
    }
    let filter_result = filter_result.map_err(super::catalog_error_from_db)?;
    if filter_result.revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: filter_result.revision,
        });
    }

    ensure_current(admission, source_id, lifecycle_generation, token)?;
    profile_guard
        .validate_current()
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    let after_root_identity = checked_source_root_identity(source_id, &source.root)?;
    if after_root_identity != expected_root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("source root identity changed during tag filter read"),
        });
    }
    let after_revision = source
        .database
        .get_revision()
        .map_err(super::catalog_error_from_db)?;
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
    if retained_snapshot.manifest.len() != expected_manifest_len {
        return Err(CatalogError::TagFilterManifestLengthMismatch {
            expected: expected_manifest_len,
            actual: retained_snapshot.manifest.len(),
        });
    }
    if retained_snapshot.root_identity != expected_root_identity
        || retained_snapshot.directory_generation != snapshot.directory_generation
    {
        return Err(CatalogError::TagFilterSnapshotMismatch {
            source_id: source_id.clone(),
            expected_manifest_len,
        });
    }
    ensure_current(admission, source_id, lifecycle_generation, token)?;

    Ok(TagFilterPatch {
        root_identity: expected_root_identity,
        directory_generation: snapshot.directory_generation,
        revision: expected_revision,
        manifest_len: expected_manifest_len,
        mask,
    })
}

fn validate_live_selected_paths(
    snapshot: &CatalogSnapshot,
    source_id: &SourceId,
    relative_paths: &[PathBuf],
) -> Result<(), CatalogError> {
    for path in relative_paths {
        let validated = RootRelativePath::try_from_path(path.clone()).map_err(|error| {
            CatalogError::Audit(format!("{source_id}: invalid tag-page path: {error}"))
        })?;
        let normalized = normalize_relative_path(validated.as_path())
            .map(PathBuf::from)
            .map_err(super::catalog_error_from_db)?;
        if snapshot
            .manifest
            .binary_search_by(|entry| entry.relative_path.cmp(&normalized))
            .is_err()
        {
            return Err(CatalogError::TagPagePathMissing {
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
