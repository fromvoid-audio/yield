use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use wavecrate_library::app_dirs::WritableProfileGuard;
use wavecrate_library::sample_sources::SourceId;
use wavecrate_library::sample_sources::db::SourceManifestEntry;
use wavecrate_library::sample_sources::reconciliation::{RootIdentity, RootRelativePath};
use wavecrate_scan::sample_sources::scanner::read_wav_display_metadata_for_manifest;

use super::{
    AdmissionState, CatalogError, CatalogSnapshot, CatalogWavMetadata, RegisteredSource,
    publication_is_current, source_root_identity,
};

/// A completed bounded metadata read before it is applied to the retained snapshot.
pub(crate) struct WavMetadataPatch {
    pub(crate) root_identity: RootIdentity,
    pub(crate) revision: u64,
    pub(crate) requested_paths: Vec<PathBuf>,
    pub(crate) manifest_paths: Vec<PathBuf>,
    pub(crate) metadata: BTreeMap<PathBuf, CatalogWavMetadata>,
}

pub(crate) fn read_patch(
    source: &RegisteredSource,
    source_id: &SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    requested_paths: Vec<PathBuf>,
    token: &Arc<AtomicBool>,
    admission: &Arc<Mutex<AdmissionState>>,
    profile_guard: &WritableProfileGuard,
) -> Result<WavMetadataPatch, CatalogError> {
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

    let exact_paths = requested_paths
        .iter()
        .cloned()
        .map(RootRelativePath::try_from_path)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            CatalogError::Audit(format!(
                "{source_id}: invalid metadata request path: {error}"
            ))
        })?;
    let (subset_revision, manifest) = source
        .database
        .manifest_snapshot_with_revision_for_exact_paths(&exact_paths)
        .map_err(super::catalog_error_from_db)?;
    if subset_revision != expected_revision {
        return Err(CatalogError::StaleRevision {
            expected: expected_revision,
            actual: subset_revision,
        });
    }
    ensure_current(admission, source_id, lifecycle_generation, token)?;
    ensure_manifest_matches_snapshot(snapshot, &manifest, source_id)?;

    let mut should_cancel = || {
        token.load(Ordering::Acquire)
            || !publication_is_current(admission, source_id, lifecycle_generation, Some(token))
    };
    let scanner_metadata = read_wav_display_metadata_for_manifest(
        &source.root,
        &expected_root_identity,
        &manifest,
        &mut should_cancel,
    )
    .map_err(|error| super::scan_wav_metadata_error(source_id, error))?;

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
            detail: String::from("source root identity changed during metadata read"),
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

    let metadata = scanner_metadata
        .into_iter()
        .map(|(path, metadata)| {
            (
                path,
                CatalogWavMetadata {
                    frames: metadata.frames,
                    sample_rate: metadata.sample_rate,
                },
            )
        })
        .collect();
    Ok(WavMetadataPatch {
        root_identity: expected_root_identity,
        revision: expected_revision,
        requested_paths,
        manifest_paths: manifest
            .into_iter()
            .map(|entry| entry.relative_path)
            .collect(),
        metadata,
    })
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

fn ensure_manifest_matches_snapshot(
    snapshot: &CatalogSnapshot,
    subset: &[SourceManifestEntry],
    source_id: &SourceId,
) -> Result<(), CatalogError> {
    for entry in subset {
        let Ok(index) = snapshot
            .manifest
            .binary_search_by(|candidate| candidate.relative_path.cmp(&entry.relative_path))
        else {
            return Err(CatalogError::Audit(format!(
                "{source_id}: exact metadata path {} is absent from the retained snapshot",
                entry.relative_path.display()
            )));
        };
        if snapshot.manifest[index] != *entry {
            return Err(CatalogError::Audit(format!(
                "{source_id}: exact metadata path {} changed within revision {}",
                entry.relative_path.display(),
                snapshot.committed_revision
            )));
        }
    }
    Ok(())
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
