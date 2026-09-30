use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use yield_library::app_dirs::WritableProfileGuard;
use yield_library::filesystem_identity::stable_filesystem_identity;
use yield_library::sample_sources::db::{
    DB_FILE_NAME, LEGACY_DB_FILE_NAME, normalize_relative_path,
};
use yield_library::sample_sources::reconciliation::{RootIdentity, RootRelativePath};
use yield_library::sample_sources::{
    SourceDatabase, SourceId, SourceTagAssignmentCursor, SourceTagAssignmentPage, SourceTagPage,
    SourceTagPageCursor,
};

use super::{
    AdmissionState, CatalogError, CatalogSnapshot, CatalogTagAssignmentPreview,
    CatalogTagAssignmentPreviewSource, CatalogTagPageQuery, MAX_TAG_ASSIGNMENT_PREVIEW_BYTES,
    MAX_TAG_ASSIGNMENT_PREVIEW_PATHS, MAX_TAG_ASSIGNMENT_PREVIEW_SOURCES, RegisteredSource,
    begin_source_attempt, publication_is_current, source_root_identity,
};

/// A completed bounded tag read before it is published to the catalog event queue.
pub(crate) struct TagPagePatch {
    pub(crate) root_identity: RootIdentity,
    pub(crate) revision: u64,
    pub(crate) page: SourceTagPage,
}

pub(crate) struct AssignmentPagePatch {
    pub(crate) root_identity: RootIdentity,
    pub(crate) revision: u64,
    pub(crate) page: SourceTagAssignmentPage,
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
    profile_guard.validate_current().map_err(|error| {
        super::catalog_error_from_db_for_source(
            source_id,
            yield_library::sample_sources::SourceDbError::ProfileOwnership(error),
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
            .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?,
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
            .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?,
        CatalogTagPageQuery::VocabularyPrefix { prefix } => source
            .database
            .tag_vocabulary_page_at_revision(expected_revision, prefix, after_cursor, page_limit)
            .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?,
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
    profile_guard.validate_current().map_err(|error| {
        super::catalog_error_from_db_for_source(
            source_id,
            yield_library::sample_sources::SourceDbError::ProfileOwnership(error),
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
            detail: String::from("source root identity changed during tag read"),
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

pub(crate) fn read_assignment_page(
    source: &RegisteredSource,
    source_id: &SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    label: &str,
    after_cursor: Option<&SourceTagAssignmentCursor>,
    page_limit: usize,
    token: &Arc<AtomicBool>,
    admission: &Arc<Mutex<AdmissionState>>,
    profile_guard: &WritableProfileGuard,
) -> Result<AssignmentPagePatch, CatalogError> {
    ensure_current(admission, source_id, lifecycle_generation, token)?;
    profile_guard.validate_current().map_err(|error| {
        super::catalog_error_from_db_for_source(
            source_id,
            yield_library::sample_sources::SourceDbError::ProfileOwnership(error),
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
    if checked_source_root_identity(source_id, &source.root)? != expected_root_identity {
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
    let page = source
        .database
        .tag_assignments_page_at_revision(expected_revision, label, after_cursor, page_limit, token)
        .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
    if page.revision != expected_revision || page.paths.len() > super::SOURCE_TAG_PAGE_MAX_PAGE_SIZE
    {
        return Err(CatalogError::Database(String::from(
            "tag assignment page violated its revision or size bound",
        )));
    }
    ensure_current(admission, source_id, lifecycle_generation, token)?;
    profile_guard.validate_current().map_err(|error| {
        super::catalog_error_from_db_for_source(
            source_id,
            yield_library::sample_sources::SourceDbError::ProfileOwnership(error),
        )
    })?;
    source
        .database
        .validate_profile_guard(profile_guard)
        .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
    if checked_source_root_identity(source_id, &source.root)? != expected_root_identity {
        return Err(CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail: String::from("source root identity changed during tag assignment read"),
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
    Ok(AssignmentPagePatch {
        root_identity: expected_root_identity,
        revision: expected_revision,
        page,
    })
}

pub(crate) fn read_assignment_preview(
    label: &str,
    expected_source_ids: &[SourceId],
    profile_guard: &WritableProfileGuard,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<SourceId, RegisteredSource>,
) -> Result<CatalogTagAssignmentPreview, CatalogError> {
    if expected_source_ids.len() > MAX_TAG_ASSIGNMENT_PREVIEW_SOURCES {
        return Err(CatalogError::TagAssignmentPreviewTooManySources {
            max: MAX_TAG_ASSIGNMENT_PREVIEW_SOURCES,
            actual: expected_source_ids.len(),
        });
    }
    if sources.len() > MAX_TAG_ASSIGNMENT_PREVIEW_SOURCES {
        return Err(CatalogError::TagAssignmentPreviewTooManySources {
            max: MAX_TAG_ASSIGNMENT_PREVIEW_SOURCES,
            actual: sources.len(),
        });
    }
    let mut ids = sources.keys().cloned().collect::<Vec<_>>();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut expected_ids = expected_source_ids.to_vec();
    expected_ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    expected_ids.dedup();
    profile_guard
        .validate_current()
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    let registry_path = profile_guard
        .profile_root()
        .join(yield_library::sample_sources::library::LIBRARY_DB_FILE_NAME);
    let registry = yield_library::sample_sources::library::load_source_registry_snapshot_read_only(
        &registry_path,
    )
    .map_err(|error| {
        CatalogError::Database(format!("could not read profile source registry: {error}"))
    })?;
    if registry.len() > MAX_TAG_ASSIGNMENT_PREVIEW_SOURCES {
        return Err(CatalogError::TagAssignmentPreviewTooManySources {
            max: MAX_TAG_ASSIGNMENT_PREVIEW_SOURCES,
            actual: registry.len(),
        });
    }
    let mut registry_ids = registry
        .as_slice()
        .iter()
        .map(|source| source.id.clone())
        .collect::<Vec<_>>();
    registry_ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    if registry_ids != expected_ids
        || ids.iter().any(|id| {
            registry_ids
                .binary_search_by(|candidate| candidate.as_str().cmp(id.as_str()))
                .is_err()
        })
    {
        return Err(CatalogError::TagAssignmentPreviewIncomplete {
            expected: registry_ids.len(),
            available: ids.len(),
        });
    }
    let mut captured = Vec::with_capacity(ids.len());
    let mut inactive = Vec::new();
    let mut affected_sources = Vec::new();
    let mut total_assignments = 0usize;
    let mut total_bytes = 0usize;
    for source_id in &registry_ids {
        let registry_source = registry
            .as_slice()
            .iter()
            .find(|source| &source.id == source_id)
            .expect("registry id was captured");
        let Some(source) = sources.get_mut(source_id) else {
            let root_identity = checked_source_root_identity(source_id, &registry_source.root)?;
            let database_root = profile_guard
                .profile_root()
                .join(super::catalog_profile_child(source_id));
            let database_identity = checked_inactive_database_identity(source_id, &database_root)?;
            let database = SourceDatabase::open_for_ui_read_with_database_root(
                &registry_source.root,
                &database_root,
            )
            .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
            if checked_inactive_database_identity(source_id, &database_root)? != database_identity {
                return Err(CatalogError::DatabaseIntegrityUncertain(format!(
                    "{source_id}: database changed while opening preview"
                )));
            }
            let revision = database
                .get_revision()
                .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
            let cancellation = Arc::new(AtomicBool::new(false));
            let mut cursor = None;
            let mut paths = Vec::new();
            loop {
                let page = database
                    .tag_assignments_page_at_revision(
                        revision,
                        label,
                        cursor.as_ref(),
                        super::SOURCE_TAG_PAGE_MAX_PAGE_SIZE,
                        &cancellation,
                    )
                    .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
                for path in page.paths {
                    total_assignments = total_assignments.saturating_add(1);
                    total_bytes = total_bytes.saturating_add(super::encoded_path_len(&path));
                    if total_assignments > MAX_TAG_ASSIGNMENT_PREVIEW_PATHS {
                        return Err(CatalogError::TagAssignmentPreviewTooLarge {
                            limit: "assignment count",
                            max: MAX_TAG_ASSIGNMENT_PREVIEW_PATHS,
                        });
                    }
                    if total_bytes > MAX_TAG_ASSIGNMENT_PREVIEW_BYTES {
                        return Err(CatalogError::TagAssignmentPreviewTooLarge {
                            limit: "path bytes",
                            max: MAX_TAG_ASSIGNMENT_PREVIEW_BYTES,
                        });
                    }
                    paths.push(path);
                }
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
            if !paths.is_empty() {
                affected_sources.push(CatalogTagAssignmentPreviewSource {
                    source_id: source_id.clone(),
                    lifecycle_generation: 0,
                    root_identity: root_identity.clone(),
                    revision,
                    paths,
                });
            }
            inactive.push((
                source_id.clone(),
                registry_source.root.clone(),
                root_identity,
                revision,
                database_root,
                database_identity,
                database,
            ));
            continue;
        };
        if source.root != registry_source.root {
            return Err(CatalogError::StaleRootGeneration {
                source_id: source_id.clone(),
                detail: String::from("registered source root differs from the profile registry"),
            });
        }
        let lifecycle_generation = source.lifecycle_generation;
        let expected_revision = source
            .snapshot
            .as_ref()
            .ok_or(CatalogError::SnapshotUnavailable)?
            .committed_revision;
        let token = begin_source_attempt(admission, source_id, lifecycle_generation)
            .ok_or(CatalogError::Canceled)?;
        source.attempt_token = Some(Arc::clone(&token));
        let mut cursor = None;
        let mut paths = Vec::new();
        let root_identity = loop {
            let patch = read_assignment_page(
                source,
                source_id,
                lifecycle_generation,
                expected_revision,
                label,
                cursor.as_ref(),
                super::SOURCE_TAG_PAGE_MAX_PAGE_SIZE,
                &token,
                admission,
                profile_guard,
            )?;
            for path in patch.page.paths {
                total_assignments = total_assignments.saturating_add(1);
                total_bytes = total_bytes.saturating_add(super::encoded_path_len(&path));
                if total_assignments > MAX_TAG_ASSIGNMENT_PREVIEW_PATHS {
                    return Err(CatalogError::TagAssignmentPreviewTooLarge {
                        limit: "assignment count",
                        max: MAX_TAG_ASSIGNMENT_PREVIEW_PATHS,
                    });
                }
                if total_bytes > MAX_TAG_ASSIGNMENT_PREVIEW_BYTES {
                    return Err(CatalogError::TagAssignmentPreviewTooLarge {
                        limit: "path bytes",
                        max: MAX_TAG_ASSIGNMENT_PREVIEW_BYTES,
                    });
                }
                paths.push(path);
            }
            cursor = patch.page.next_cursor;
            if cursor.is_none() {
                break patch.root_identity;
            }
        };
        captured.push((
            source_id.clone(),
            lifecycle_generation,
            expected_revision,
            root_identity.clone(),
            token,
        ));
        if !paths.is_empty() {
            affected_sources.push(CatalogTagAssignmentPreviewSource {
                source_id: source_id.clone(),
                lifecycle_generation,
                root_identity,
                revision: expected_revision,
                paths,
            });
        }
    }
    // The preview must represent one complete source set, even when a registry command is
    // admitted concurrently with this worker read.
    let current_ids = super::admission_state(admission, |state| {
        let mut current = state
            .sources
            .iter()
            .filter(|(_, source)| source.active)
            .map(|(source_id, _)| source_id.clone())
            .collect::<Vec<_>>();
        current.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        current
    });
    if current_ids != ids {
        return Err(CatalogError::Canceled);
    }
    profile_guard
        .validate_current()
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    for (source_id, generation, revision, root_identity, token) in &captured {
        ensure_current(admission, source_id, *generation, token)?;
        let source = sources.get(source_id).ok_or(CatalogError::Canceled)?;
        source
            .database
            .validate_profile_guard(profile_guard)
            .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
        if checked_source_root_identity(source_id, &source.root)? != *root_identity {
            return Err(CatalogError::StaleRootGeneration {
                source_id: source_id.clone(),
                detail: String::from("source root changed during assignment preview"),
            });
        }
        let actual = source
            .database
            .get_revision()
            .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
        if actual != *revision {
            return Err(CatalogError::StaleRevision {
                expected: *revision,
                actual,
            });
        }
    }
    for (source_id, root, root_identity, revision, database_root, database_identity, database) in
        &inactive
    {
        if checked_inactive_database_identity(source_id, database_root)? != *database_identity {
            return Err(CatalogError::DatabaseIntegrityUncertain(format!(
                "{source_id}: database changed during assignment preview"
            )));
        }
        if checked_source_root_identity(source_id, root)? != *root_identity {
            return Err(CatalogError::StaleRootGeneration {
                source_id: source_id.clone(),
                detail: String::from("inactive source root changed during assignment preview"),
            });
        }
        let actual = database
            .get_revision()
            .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
        if actual != *revision {
            return Err(CatalogError::StaleRevision {
                expected: *revision,
                actual,
            });
        }
    }
    let after_registry =
        yield_library::sample_sources::library::load_source_registry_snapshot_read_only(
            &registry_path,
        )
        .map_err(|error| {
            CatalogError::Database(format!(
                "could not recheck profile source registry: {error}"
            ))
        })?;
    let mut after_sources = after_registry
        .as_slice()
        .iter()
        .map(|source| (source.id.clone(), source.root.clone()))
        .collect::<Vec<_>>();
    after_sources.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    let mut before_sources = registry
        .as_slice()
        .iter()
        .map(|source| (source.id.clone(), source.root.clone()))
        .collect::<Vec<_>>();
    before_sources.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    if before_sources != after_sources {
        return Err(CatalogError::Canceled);
    }
    profile_guard
        .validate_current()
        .map_err(|error| CatalogError::ProfileUnavailable(error.to_string()))?;
    Ok(CatalogTagAssignmentPreview {
        label: label.to_owned(),
        checked_sources: registry_ids.len(),
        affected_sources,
        total_assignments,
    })
}

fn checked_inactive_database_identity(
    source_id: &SourceId,
    database_root: &Path,
) -> Result<(PathBuf, String), CatalogError> {
    for filename in [DB_FILE_NAME, LEGACY_DB_FILE_NAME] {
        let path = database_root.join(filename);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(CatalogError::DatabaseIntegrityUncertain(format!(
                    "{source_id}: could not inspect {}: {error}",
                    path.display()
                )));
            }
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(CatalogError::DatabaseIntegrityUncertain(format!(
                "{source_id}: source database is not a regular file"
            )));
        }
        let identity = stable_filesystem_identity(&path, &metadata).ok_or_else(|| {
            CatalogError::DatabaseIntegrityUncertain(format!(
                "{source_id}: source database identity unavailable"
            ))
        })?;
        return Ok((path, identity));
    }
    Err(CatalogError::SnapshotUnavailable)
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
    profile_guard.validate_current().map_err(|error| {
        super::catalog_error_from_db_for_source(
            source_id,
            yield_library::sample_sources::SourceDbError::ProfileOwnership(error),
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
        .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
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
                return Err(yield_library::sample_sources::SourceDbError::Unexpected);
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
    let filter_result =
        filter_result.map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
    if filter_result.revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: filter_result.revision,
        });
    }

    ensure_current(admission, source_id, lifecycle_generation, token)?;
    profile_guard.validate_current().map_err(|error| {
        super::catalog_error_from_db_for_source(
            source_id,
            yield_library::sample_sources::SourceDbError::ProfileOwnership(error),
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
            detail: String::from("source root identity changed during tag filter read"),
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
            .map_err(|error| super::catalog_error_from_db_for_source(source_id, error))?;
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
