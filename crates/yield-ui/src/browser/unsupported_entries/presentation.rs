//! Bounded, lossless-path presentation and inspection eligibility observations.
use super::*;

pub(super) fn row_description(entry: &SourceIndexEntry) -> String {
    use yield_library::sample_sources::SourceIndexDiagnostic as D;
    let kind = match entry.classification {
        SourceIndexClassification::UnsupportedAudio => "Unsupported audio format",
        SourceIndexClassification::UnsupportedNonAudio => "Not an audio file",
        SourceIndexClassification::Inaccessible => "Cannot access file",
        SourceIndexClassification::PracticallyUnsupportedAudio => "Audio exceeds supported limits",
    };
    let diagnostic = match entry.diagnostic {
        Some(D::EntryTypeUnavailable) => "File type could not be checked",
        Some(D::MetadataUnavailable) => "File identity or metadata unavailable",
        Some(D::OpenUnavailable) => "File could not be opened safely",
        Some(D::PracticalSupportLimit) => "Audio exceeds supported limits",
        Some(D::NonUnicodePath) => "Filename shown as raw bytes",
        None => "Saved entry",
    };
    format!(
        "{kind} · {diagnostic} · {}",
        if eligible(entry) {
            "Enter to check identity; reveal unavailable"
        } else {
            "Identity check and reveal unavailable"
        }
    )
}

pub(super) fn display_path(path: &Path) -> String {
    if let Some(text) = path.to_str() {
        let mut end = text.len().min(DISPLAY_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < text.len() {
            format!("{}… (display clipped)", &text[..end])
        } else {
            text.to_owned()
        }
    } else {
        let bytes = path.as_os_str().as_encoded_bytes();
        let hex = bytes
            .iter()
            .take(256)
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "Raw path bytes: {hex}{}",
            if bytes.len() > 256 {
                "… (display clipped)"
            } else {
                ""
            }
        )
    }
}
