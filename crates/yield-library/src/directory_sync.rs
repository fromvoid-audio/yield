//! Namespace syncing through retained directory capabilities.

use std::{fs::File, io};

/// Sync a retained directory without resolving its ambient pathname.
pub(crate) fn sync_directory(directory: &File) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use cap_fs_ext::OpenOptionsFollowExt;
        use cap_primitives::fs::{FollowSymlinks, OpenOptions, open};

        // Linux directory capabilities may use O_PATH, which cannot be fsynced. Open the
        // directory itself for read relative to that capability, never its ambient pathname.
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        open(directory, std::path::Path::new("."), &options)?.sync_all()
    }
    #[cfg(not(target_os = "linux"))]
    {
        directory.sync_all()
    }
}
