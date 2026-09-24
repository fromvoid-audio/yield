//! Durable source tag catalog queries and assignment mutations.

mod display_labels;
mod identity;
mod mutations;
mod queries;

#[cfg(test)]
pub(super) fn matching_tag_filter_paths_sql_for_test() -> String {
    queries::matching_tag_filter_paths_sql_for_test()
}

#[cfg(test)]
pub(super) fn display_label_rows_sql_for_test() -> String {
    display_labels::display_label_rows_sql_for_test()
}

/// Maximum number of required tags accepted by one source tag filter.
pub const SOURCE_TAG_FILTER_MAX_TAGS: usize = 64;
/// Maximum UTF-8 byte length of one required tag filter token.
pub const SOURCE_TAG_FILTER_MAX_TOKEN_BYTES: usize = 1024;
/// Maximum aggregate UTF-8 byte length of required tag filter input.
pub const SOURCE_TAG_FILTER_MAX_INPUT_BYTES: usize = 64 * 1024;

/// Maximum number of paths accepted by one bounded display-label request.
pub const SOURCE_DISPLAY_LABEL_MAX_PATHS: usize = 64;
/// Maximum aggregate raw plus normalized path bytes accepted by one display-label request.
pub const SOURCE_DISPLAY_LABEL_MAX_PATH_BYTES: usize = 256 * 1024;
/// Maximum UTF-8 byte length of one assembled display label.
pub const SOURCE_DISPLAY_LABEL_MAX_OUTPUT_BYTES: usize = 4096;

/// Counts returned by one revision-fenced source tag filter read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceTagFilterResult {
    /// Exact source revision observed by the read transaction.
    pub revision: u64,
    /// Number of matching live supported paths passed to the visitor.
    pub matched_paths: u64,
}

pub(in crate::sample_sources::db) use identity::{
    normalize_tag_identity, validate_curation_tag_label,
};

/// SQL expression returning 1 for Loop, 2 for OneShot, and NULL for an unrecognized tag.
///
/// Keep this expression shared by browser projection and curation mutation so both paths retain
/// the same punctuation, edge-separator, and embedded-NUL behavior.
pub(in crate::sample_sources::db) const PLAYBACK_TYPE_CLASSIFICATION: &str = "CASE
    WHEN instr(st.normalized_text, char(0)) > 0 THEN NULL
    WHEN trim(st.normalized_text, ' -_') = 'loop' THEN 1
    WHEN trim(st.normalized_text, ' -_') = 'oneshot' THEN 2
    WHEN length(trim(st.normalized_text, ' -_')) > 7
         AND substr(trim(st.normalized_text, ' -_'), 1, 3) = 'one'
         AND substr(trim(st.normalized_text, ' -_'), -4) = 'shot'
         AND trim(
                 substr(
                     trim(st.normalized_text, ' -_'),
                     4,
                     length(trim(st.normalized_text, ' -_')) - 7
                 ),
                 ' -_'
             ) = ''
        THEN 2
END";

#[cfg(test)]
mod tests;
