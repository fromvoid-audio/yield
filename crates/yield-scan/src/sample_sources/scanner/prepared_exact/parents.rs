//! Retained, previously audited parent namespace for exact entries.
use super::*;
use yield_library::filesystem_identity::stable_filesystem_identity_from_open_file;

pub(in crate::sample_sources::scanner) struct ParentDirectory {
    path: std::path::PathBuf,
    identity: String,
    file: File,
}
pub(in crate::sample_sources::scanner) fn hold_parents(
    owner: &SourceWriter,
    paths: &[RootRelativePath],
    root: &SourceRootCapability,
    revision: u64,
) -> Result<Vec<ParentDirectory>, ScanError> {
    let entries = owner
        .catalog_database()
        .exact_scan_parent_directories_at_revision(paths, revision)?;
    let mut parents = Vec::with_capacity(entries.len());
    for entry in entries {
        let file = root.open_directory_file(&entry.relative_path)?;
        if stable_filesystem_identity_from_open_file(&file).as_deref()
            != Some(entry.directory_identity.as_str())
        {
            return Err(invalid(
                &entry.relative_path,
                "exact parent differs from retained directory truth",
            ));
        }
        parents.push(ParentDirectory {
            path: entry.relative_path,
            identity: entry.directory_identity,
            file,
        });
    }
    Ok(parents)
}
pub(in crate::sample_sources::scanner) fn revalidate(
    parents: &[ParentDirectory],
    root: &SourceRootCapability,
    cancel: Option<&AtomicBool>,
) -> Result<(), ScanError> {
    for parent in parents {
        canceled(cancel)?;
        let current = root.open_directory_file(&parent.path)?;
        if stable_filesystem_identity_from_open_file(&current).as_deref()
            != Some(parent.identity.as_str())
            || stable_filesystem_identity_from_open_file(&parent.file).as_deref()
                != Some(parent.identity.as_str())
        {
            return Err(invalid(&parent.path, "held exact parent was replaced"));
        }
    }
    Ok(())
}
