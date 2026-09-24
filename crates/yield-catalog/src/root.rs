use std::fs;
use std::path::{Path, PathBuf};

use yield_library::sample_sources::SourceId;
use yield_library::sample_sources::reconciliation::RootIdentity;

use super::CatalogError;

/// A source root whose canonical target and physical identity were checked together.
///
/// The lexical path is retained privately so a caller can hand the proof back to the catalog
/// after asynchronous work without losing the ability to detect a symlink-target replacement.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct VerifiedSourceRoot {
    canonical_root: PathBuf,
    lexical_root: PathBuf,
    root_identity: RootIdentity,
}

pub(crate) struct RegistrationRoot {
    canonical_target: PathBuf,
    identity: RootIdentity,
}

impl RegistrationRoot {
    pub(crate) fn identity(&self) -> &RootIdentity {
        &self.identity
    }
}

impl VerifiedSourceRoot {
    /// Borrow the canonical source-root path.
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    /// Borrow the physical identity captured for the canonical source-root path.
    pub fn root_identity(&self) -> &RootIdentity {
        &self.root_identity
    }

    pub(crate) fn lexical_root(&self) -> &Path {
        &self.lexical_root
    }
}

/// Resolve and verify a source root for a later catalog registration.
///
/// This performs blocking filesystem reads and should be called from a background worker. It
/// strictly canonicalizes the supplied path, verifies that the target is a directory with a
/// stable identity, then repeats both checks so a symlink-target or physical-root change during
/// resolution fails closed. The returned proof does not retain any database or scanner state.
pub fn resolve_source_root(path: &Path) -> Result<VerifiedSourceRoot, CatalogError> {
    let (canonical_root, root_identity) = resolve_stable(path)?;
    Ok(VerifiedSourceRoot {
        canonical_root,
        lexical_root: path.to_path_buf(),
        root_identity,
    })
}

/// Capture the identity required by a registration before its prior handle is released.
pub(crate) fn capture_registration_root(
    source_id: &SourceId,
    canonical_root: &Path,
    verified_root: Option<&VerifiedSourceRoot>,
) -> Result<RegistrationRoot, CatalogError> {
    let (canonical_target, identity) = match verified_root {
        Some(verified_root) => {
            if canonical_root != verified_root.canonical_root() {
                return Err(stale_for_source(
                    source_id,
                    "verified source root path does not match the registration path",
                ));
            }
            resolve_stable_for_source(source_id, verified_root.lexical_root())?
        }
        None => resolve_stable_for_source(source_id, canonical_root)?,
    };
    if verified_root.is_some() && &canonical_target != canonical_root {
        return Err(stale_for_source(
            source_id,
            "source root canonical target changed",
        ));
    }
    if let Some(verified_root) = verified_root
        && &identity != verified_root.root_identity()
    {
        return Err(stale_for_source(
            source_id,
            "verified source root physical identity changed",
        ));
    }
    Ok(RegistrationRoot {
        canonical_target,
        identity,
    })
}

/// Revalidate a registration root against its optional caller-supplied proof.
pub(crate) fn revalidate_registration_root(
    source_id: &SourceId,
    canonical_root: &Path,
    verified_root: Option<&VerifiedSourceRoot>,
    expected_root: Option<&RegistrationRoot>,
) -> Result<RootIdentity, CatalogError> {
    let (current_root, current_identity) = match verified_root {
        Some(verified_root) => {
            if canonical_root != verified_root.canonical_root() {
                return Err(stale_for_source(
                    source_id,
                    "verified source root path does not match the registration path",
                ));
            }
            resolve_stable_for_source(source_id, verified_root.lexical_root())?
        }
        None => resolve_stable_for_source(source_id, canonical_root)?,
    };

    if &current_root != canonical_root && verified_root.is_some() {
        return Err(stale_for_source(
            source_id,
            "source root canonical target changed",
        ));
    }
    if let Some(expected_root) = expected_root
        && (current_root != expected_root.canonical_target
            || current_identity != expected_root.identity)
    {
        return Err(stale_for_source(
            source_id,
            "source root canonical target or physical identity changed",
        ));
    }
    if let Some(verified_root) = verified_root
        && &current_identity != verified_root.root_identity()
    {
        return Err(stale_for_source(
            source_id,
            "verified source root physical identity changed",
        ));
    }
    Ok(current_identity)
}

fn resolve_stable(path: &Path) -> Result<(PathBuf, RootIdentity), CatalogError> {
    resolve_stable_for_source(&SourceId::from_string("unknown"), path)
}

fn resolve_stable_for_source(
    source_id: &SourceId,
    path: &Path,
) -> Result<(PathBuf, RootIdentity), CatalogError> {
    let first = resolve_once_for_source(source_id, path)?;
    let second = resolve_once_for_source(source_id, path)?;
    if first != second {
        return Err(stale_for_source(
            source_id,
            "source root changed while its verified path was being resolved",
        ));
    }
    Ok(first)
}

fn resolve_once_for_source(
    source_id: &SourceId,
    path: &Path,
) -> Result<(PathBuf, RootIdentity), CatalogError> {
    let canonical_root = fs::canonicalize(path).map_err(|error| {
        stale_for_source(
            source_id,
            &format!(
                "could not canonicalize source root {}: {error}",
                path.display()
            ),
        )
    })?;
    let root_identity = checked_source_root_identity(source_id, &canonical_root)?;
    Ok((canonical_root, root_identity))
}

fn checked_source_root_identity(
    source_id: &SourceId,
    root: &Path,
) -> Result<RootIdentity, CatalogError> {
    super::source_root_identity(root).map_err(|error| match error {
        CatalogError::StaleRootGeneration { detail, .. } => CatalogError::StaleRootGeneration {
            source_id: source_id.clone(),
            detail,
        },
        error => error,
    })
}

fn stale_for_source(source_id: &SourceId, detail: impl Into<String>) -> CatalogError {
    CatalogError::StaleRootGeneration {
        source_id: source_id.clone(),
        detail: detail.into(),
    }
}
