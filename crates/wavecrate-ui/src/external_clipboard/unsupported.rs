use std::path::PathBuf;

use super::FileClipboardError;

pub(super) fn copy_file_paths(_paths: &[PathBuf]) -> Result<(), FileClipboardError> {
    Err(FileClipboardError::UnsupportedPlatform)
}
