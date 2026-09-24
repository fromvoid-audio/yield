//! Display-only sample names, independent of filesystem rename operations.

use std::{borrow::Cow, path::Path};

/// Source of the text shown in the browser's Name column.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SampleNameViewMode {
    /// Show the filename stem, excluding its directory and final extension.
    #[default]
    DiskFilename,
    /// Show an available metadata label, falling back to the filename stem.
    MetadataLabel,
}

impl SampleNameViewMode {
    /// Return the other display mode without changing file identity or metadata.
    pub fn toggled(self) -> Self {
        match self {
            Self::DiskFilename => Self::MetadataLabel,
            Self::MetadataLabel => Self::DiskFilename,
        }
    }

    /// Return the compact reference header label for this mode.
    pub fn label(self) -> &'static str {
        match self {
            Self::DiskFilename => "Disk",
            Self::MetadataLabel => "Label",
        }
    }
}

/// Project a display name without changing the path used for selection or playback.
///
/// `metadata_label` is an already ordered, bounded projection supplied by the metadata owner.
/// Missing or empty metadata falls back to the disk stem. This helper performs no I/O and
/// borrows valid UTF-8 text whenever possible.
pub fn sample_display_name<'a>(
    relative_path: &'a Path,
    metadata_label: Option<&'a str>,
    mode: SampleNameViewMode,
) -> Cow<'a, str> {
    if mode == SampleNameViewMode::MetadataLabel
        && let Some(label) = metadata_label.filter(|label| !label.is_empty())
    {
        return Cow::Borrowed(label);
    }
    relative_path
        .file_stem()
        .or_else(|| relative_path.file_name())
        .unwrap_or(relative_path.as_os_str())
        .to_string_lossy()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_names_remove_directories_and_only_the_final_extension() {
        let path = Path::new("pack/音色.v2.WAV");
        assert_eq!(
            sample_display_name(path, Some("warm_loop"), SampleNameViewMode::DiskFilename),
            "音色.v2"
        );
        assert_eq!(path, Path::new("pack/音色.v2.WAV"));
    }

    #[test]
    fn metadata_names_preserve_owner_order_and_fall_back_when_unavailable() {
        let path = Path::new("pack/kick.wav");
        let mode = SampleNameViewMode::MetadataLabel;
        assert_eq!(
            sample_display_name(path, Some("warm_bass"), mode),
            "warm_bass"
        );
        assert_eq!(sample_display_name(path, None, mode), "kick");
        assert_eq!(sample_display_name(path, Some(""), mode), "kick");
    }
}
