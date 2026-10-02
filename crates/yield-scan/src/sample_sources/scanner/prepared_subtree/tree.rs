use super::*;
use cap_std::fs::Dir;
use std::{
    ffi::OsString,
    fs::{File, Metadata},
};
use yield_library::{
    filesystem_identity::stable_filesystem_identity_from_open_file,
    sample_sources::{
        SourceEntryClassification, SourceEntryFileType, SourceTraversalPolicy,
        classify_source_entry_with_policy, db::SourceDirectoryEntry,
    },
};

#[derive(PartialEq, Eq)]
struct Stamp {
    modified: std::time::SystemTime,
    #[cfg(unix)]
    ctime: (i64, i64),
}
fn stamp(metadata: Metadata) -> Result<Stamp, ScanError> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    let modified = metadata
        .modified()
        .map_err(|_| invalid(std::path::Path::new(""), "directory timestamp unavailable"))?;
    Ok(Stamp {
        modified,
        #[cfg(unix)]
        ctime: (metadata.ctime(), metadata.ctime_nsec()),
    })
}
pub(super) struct HeldDirectory {
    pub(super) entry: SourceDirectoryEntry,
    file: File,
    stamp: Stamp,
    members: Vec<(OsString, SourceEntryFileType)>,
}
pub(super) struct HeldTree {
    pub(super) directories: Vec<HeldDirectory>,
    pub(super) files: Vec<RootRelativePath>,
}

fn members(
    file: &File,
    path: &std::path::Path,
    started: Instant,
    cancel: Option<&AtomicBool>,
    budget: &mut usize,
) -> Result<Vec<(OsString, SourceEntryFileType)>, ScanError> {
    let dir = Dir::from_std_file(file.try_clone().map_err(|source| ScanError::Io {
        path: path.to_path_buf(),
        source,
    })?);
    let iter = dir.entries().map_err(|source| ScanError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut result = Vec::new();
    for entry in iter {
        check(started, cancel)?;
        *budget = budget.saturating_add(1);
        if *budget > SUBTREE_SCAN_MAX_ENTRIES {
            return Err(invalid(path, "subtree entry budget exceeded"));
        }
        let entry = entry.map_err(|source| ScanError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let kind = entry.file_type().map_err(|source| ScanError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let kind = SourceEntryFileType::from_no_followed_type(
            kind.is_dir(),
            kind.is_file(),
            kind.is_symlink(),
        );
        result.push((entry.file_name(), kind));
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}
pub(super) fn walk(
    root: &SourceRootCapability,
    scopes: &[RootRelativePath],
    policy: SourceTraversalPolicy,
    started: Instant,
    cancel: Option<&AtomicBool>,
) -> Result<HeldTree, ScanError> {
    let mut pending = scopes
        .iter()
        .map(|s| s.as_path().to_path_buf())
        .collect::<Vec<_>>();
    let mut held = HeldTree {
        directories: Vec::new(),
        files: Vec::new(),
    };
    let mut identities = BTreeSet::new();
    let mut visited = scopes.len();
    let mut bytes = 0usize;
    while let Some(path) = pending.pop() {
        check(started, cancel)?;
        let file = root.open_directory_file(&path)?;
        let identity = stable_filesystem_identity_from_open_file(&file)
            .ok_or_else(|| invalid(&path, "subtree directory identity unavailable"))?;
        if held.directories.len() >= 64 || !identities.insert(identity.clone()) {
            return Err(invalid(
                &path,
                "subtree directory budget or identity repeated",
            ));
        }
        if !matches!(
            classify_source_entry_with_policy(&path, SourceEntryFileType::Directory, policy),
            SourceEntryClassification::Directory { .. }
        ) {
            return Err(invalid(&path, "subtree policy excludes target"));
        }
        let before = stamp(file.metadata().map_err(|source| ScanError::Io {
            path: path.clone(),
            source,
        })?)?;
        let members = members(&file, &path, started, cancel, &mut visited)?;
        for (name, kind) in &members {
            let child = path.join(name);
            bytes = bytes.saturating_add(child.as_os_str().as_encoded_bytes().len());
            if child.as_os_str().as_encoded_bytes().len() > 1024
                || child.components().count() > 32
                || bytes > 32768
            {
                return Err(invalid(&child, "subtree locator/depth budget exceeded"));
            }
            match classify_source_entry_with_policy(&child, *kind, policy) {
                SourceEntryClassification::Directory { .. } => pending.push(child),
                SourceEntryClassification::File { .. } => held.files.push(
                    RootRelativePath::try_from_path(child.clone())
                        .map_err(|_| invalid(&child, "invalid subtree locator"))?,
                ),
                SourceEntryClassification::Rejected(_)
                    if matches!(kind, SourceEntryFileType::Link | SourceEntryFileType::Other) =>
                {
                    return Err(invalid(&child, "subtree structural uncertainty"));
                }
                SourceEntryClassification::Rejected(_) => {}
            }
            if held.files.len() > 64 || pending.len() + held.directories.len() > 64 {
                return Err(invalid(&path, "subtree held resource budget exceeded"));
            }
        }
        if before
            != stamp(file.metadata().map_err(|source| ScanError::Io {
                path: path.clone(),
                source,
            })?)?
        {
            return Err(invalid(
                &path,
                "subtree membership changed during enumeration",
            ));
        }
        held.directories.push(HeldDirectory {
            entry: SourceDirectoryEntry {
                relative_path: path,
                directory_identity: identity,
            },
            file,
            stamp: before,
            members,
        });
    }
    Ok(held)
}
impl HeldTree {
    pub(super) fn revalidate(
        &self,
        root: &SourceRootCapability,
        started: Instant,
        cancel: Option<&AtomicBool>,
        enumerate: bool,
    ) -> Result<(), ScanError> {
        let mut budget = self.directories.len();
        for directory in &self.directories {
            check(started, cancel)?;
            let path = &directory.entry.relative_path;
            let current = root.open_directory_file(path)?;
            if stable_filesystem_identity_from_open_file(&current).as_deref()
                != Some(directory.entry.directory_identity.as_str())
                || stable_filesystem_identity_from_open_file(&directory.file).as_deref()
                    != Some(directory.entry.directory_identity.as_str())
                || stamp(directory.file.metadata().map_err(|source| ScanError::Io {
                    path: path.clone(),
                    source,
                })?)?
                    != directory.stamp
            {
                return Err(invalid(path, "held subtree directory changed"));
            }
            if enumerate
                && members(&directory.file, path, started, cancel, &mut budget)?
                    != directory.members
            {
                return Err(invalid(path, "held subtree membership changed"));
            }
        }
        Ok(())
    }
}
