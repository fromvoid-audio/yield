//! Durable source tag catalog queries and assignment mutations.

mod identity;
mod mutations;
mod queries;

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
