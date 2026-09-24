use super::super::{SOURCE_CURATION_MAX_TAG_LABEL_BYTES, SourceDbError};

pub(in crate::sample_sources::db) struct NormalizedTagIdentity {
    pub(in crate::sample_sources::db) display_label: String,
    pub(in crate::sample_sources::db) normalized_text: String,
}

pub(in crate::sample_sources::db) fn normalize_tag_identity(
    label: &str,
) -> Result<NormalizedTagIdentity, SourceDbError> {
    let display_label = label.split_whitespace().collect::<Vec<_>>().join(" ");
    if display_label.is_empty() {
        return Err(SourceDbError::EmptyTagLabel);
    }

    Ok(NormalizedTagIdentity {
        normalized_text: display_label.to_ascii_lowercase(),
        display_label,
    })
}

pub(in crate::sample_sources::db) fn validate_curation_tag_label(
    label: &str,
) -> Result<(), SourceDbError> {
    if label.len() > SOURCE_CURATION_MAX_TAG_LABEL_BYTES {
        return Err(SourceDbError::TagLabelTooLong {
            max: SOURCE_CURATION_MAX_TAG_LABEL_BYTES,
            actual: label.len(),
        });
    }
    if label.contains('\0') {
        return Err(SourceDbError::TagLabelContainsNul);
    }
    if label.split_whitespace().next().is_none() {
        return Err(SourceDbError::EmptyTagLabel);
    }
    Ok(())
}
