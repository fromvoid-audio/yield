//! Native file clipboard handoff for browser-selected audio files.

use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use yield_library::sample_sources::is_rejected_source_file_path;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod unsupported;
#[cfg(target_os = "windows")]
mod windows;

/// Paths that were validated for a native file clipboard handoff.
///
/// Construct this with [`prepare_file_paths`] on a worker thread. Keeping
/// filesystem validation out of [`copy_file_paths`] makes the UI-thread
/// publication boundary explicit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedFilePaths {
    paths: Vec<PathBuf>,
}

impl PreparedFilePaths {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths }
    }

    /// Return the validated absolute file paths in their input order.
    pub(crate) fn as_slice(&self) -> &[PathBuf] {
        &self.paths
    }

    fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// Failure reported while preparing or publishing a native file handoff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FileClipboardError {
    /// No paths were supplied by the browser command.
    NoPathsProvided,
    /// The supplied paths contained no eligible existing regular absolute files.
    NoExistingFiles,
    /// The current platform has no native file clipboard implementation.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    UnsupportedPlatform,
    /// The native clipboard rejected the handoff.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    Platform(String),
}

impl Display for FileClipboardError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPathsProvided => formatter.write_str("No files selected for clipboard handoff"),
            Self::NoExistingFiles => {
                formatter.write_str("No selected files are available for clipboard handoff")
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            Self::UnsupportedPlatform => {
                formatter.write_str("Native file clipboard handoff is unsupported on this platform")
            }
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Self::Platform(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for FileClipboardError {}

/// Validate and filter already-resolved absolute paths for a file handoff.
///
/// This performs filesystem checks and therefore belongs on a worker thread.
/// The returned paths retain input order and are not canonicalized or
/// otherwise rewritten.
pub(crate) fn prepare_file_paths(
    paths: &[PathBuf],
) -> Result<PreparedFilePaths, FileClipboardError> {
    if paths.is_empty() {
        return Err(FileClipboardError::NoPathsProvided);
    }

    let valid_paths = paths
        .iter()
        .filter(|path| path.is_absolute() && !is_rejected_source_file_path(path) && path.is_file())
        .cloned()
        .collect::<Vec<_>>();
    if valid_paths.is_empty() {
        return Err(FileClipboardError::NoExistingFiles);
    }

    Ok(PreparedFilePaths::new(valid_paths))
}

/// Publish prepared paths as native file clipboard data on the UI thread.
///
/// This function does not inspect the filesystem. The caller must pass a
/// value returned by [`prepare_file_paths`] and invoke it from the GPUI UI
/// thread so AppKit or Windows clipboard APIs run in the foreground window's
/// thread.
pub(crate) fn copy_file_paths(paths: &PreparedFilePaths) -> Result<(), FileClipboardError> {
    if paths.is_empty() {
        return Err(FileClipboardError::NoPathsProvided);
    }

    platform::copy_file_paths(paths.as_slice())
}

/// Copy plain text through the native pasteboard and report publication failures.
///
/// The status-log shortcut uses this instead of GPUI's infallible clipboard
/// convenience API so it never claims a failed write succeeded.
pub(crate) fn copy_text(text: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        macos::copy_text(text)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = text;
        Err(String::from(
            "Text clipboard handoff is unavailable on this platform",
        ))
    }
}

#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use unsupported as platform;
#[cfg(target_os = "windows")]
use windows as platform;

#[cfg(test)]
mod tests {
    use super::{FileClipboardError, prepare_file_paths};
    use std::path::PathBuf;

    #[test]
    fn prepare_rejects_empty_input_without_filesystem_work() {
        assert_eq!(
            prepare_file_paths(&[]),
            Err(FileClipboardError::NoPathsProvided)
        );
    }

    #[test]
    fn prepare_filters_ineligible_paths() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let existing = directory.path().join("existing sample.wav");
        std::fs::write(&existing, b"sample").expect("write sample");
        let nested_directory = directory.path().join("folder");
        std::fs::create_dir(&nested_directory).expect("create nested directory");
        let metadata = directory.path().join(".wavecrate.db");
        std::fs::write(&metadata, b"private fixture").expect("write source metadata");

        let paths = vec![
            PathBuf::from("relative.wav"),
            directory.path().join("missing.wav"),
            nested_directory,
            metadata,
            existing.clone(),
        ];

        let prepared = prepare_file_paths(&paths).expect("existing file should survive filtering");
        assert_eq!(prepared.as_slice(), &[existing]);
    }

    #[test]
    fn prepare_preserves_unicode_spaces_and_input_order() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first = directory.path().join("é first sample.wav");
        let second = directory.path().join("second sample.wav");
        std::fs::write(&first, b"first").expect("write first sample");
        std::fs::write(&second, b"second").expect("write second sample");

        let prepared = prepare_file_paths(&[second.clone(), first.clone()])
            .expect("existing files should be prepared");
        assert_eq!(prepared.as_slice(), &[second, first]);
    }

    #[test]
    fn copy_rejects_an_empty_prepared_value_before_platform_access() {
        let empty = super::PreparedFilePaths::new(Vec::new());
        assert_eq!(
            super::copy_file_paths(&empty),
            Err(FileClipboardError::NoPathsProvided)
        );
    }
}
