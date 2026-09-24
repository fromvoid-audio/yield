#![deny(missing_docs)]
#![deny(warnings)]

//! Bounded, renderer-independent metadata tag input parsing.

/// Maximum UTF-8 byte length accepted for one submitted tag-input value.
pub const MAX_TAG_INPUT_BYTES: usize = 64 * 1024;

/// Maximum UTF-8 byte length of one normalized tag label.
pub const MAX_TAG_LABEL_BYTES: usize = 1024;

/// Maximum number of distinct normalized labels accepted in one submission.
pub const MAX_TAGS_PER_COMMIT: usize = 64;

/// A bounded tag-input parsing failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TagInputError {
    /// The complete raw input exceeded [`MAX_TAG_INPUT_BYTES`].
    InputTooLarge {
        /// Maximum raw input byte length.
        max: usize,
        /// Supplied raw input byte length.
        actual: usize,
    },
    /// A normalized label exceeded [`MAX_TAG_LABEL_BYTES`].
    LabelTooLong {
        /// Zero-based index of the delimited input segment containing the label.
        index: usize,
        /// Maximum normalized label byte length.
        max: usize,
        /// Normalized label byte length at the first byte that exceeded the bound.
        actual: usize,
    },
    /// The submission contained more distinct normalized labels than allowed.
    TooManyLabels {
        /// Maximum number of distinct normalized labels.
        max: usize,
        /// Number of distinct normalized labels discovered when the limit was exceeded.
        actual: usize,
    },
}

impl std::fmt::Display for TagInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InputTooLarge { max, actual } => {
                write!(
                    formatter,
                    "tag input is {actual} UTF-8 bytes; maximum is {max}"
                )
            }
            Self::LabelTooLong { index, max, actual } => write!(
                formatter,
                "normalized tag label at segment {index} is {actual} UTF-8 bytes; maximum is {max}"
            ),
            Self::TooManyLabels { max, actual } => write!(
                formatter,
                "tag input contains {actual} distinct labels; maximum is {max}"
            ),
        }
    }
}

impl std::error::Error for TagInputError {}

/// Parse a bounded tag-input submission into normalized, distinct labels.
///
/// Newlines, commas, and semicolons delimit labels. Each label follows the native vocabulary
/// normalization rules: ASCII letters and numbers are lowercased, underscores are preserved,
/// runs of whitespace, hyphens, and ASCII punctuation become one hyphen, and other characters
/// are ignored. Leading and trailing hyphens are removed. The returned labels preserve the first
/// occurrence order after normalization and duplicate removal.
///
/// The raw input and every normalized label are checked before exceeding their respective byte
/// limits. The label count is checked before adding a new distinct result, and any failure drops
/// the complete in-progress result rather than returning a partial submission.
pub fn parse_tag_input(value: &str) -> Result<Vec<String>, TagInputError> {
    if value.len() > MAX_TAG_INPUT_BYTES {
        return Err(TagInputError::InputTooLarge {
            max: MAX_TAG_INPUT_BYTES,
            actual: value.len(),
        });
    }

    let mut labels = Vec::new();
    for (index, part) in value.split(['\n', ',', ';']).enumerate() {
        let Some(normalized) = normalize_label(part, index)? else {
            continue;
        };
        if labels.iter().any(|label| label == &normalized) {
            continue;
        }
        let actual = labels.len() + 1;
        if actual > MAX_TAGS_PER_COMMIT {
            return Err(TagInputError::TooManyLabels {
                max: MAX_TAGS_PER_COMMIT,
                actual,
            });
        }
        labels.push(normalized);
    }
    Ok(labels)
}

fn normalize_label(value: &str, index: usize) -> Result<Option<String>, TagInputError> {
    let mut normalized = String::with_capacity(value.len().min(MAX_TAG_LABEL_BYTES));
    let mut separator_pending = false;

    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if separator_pending && !normalized.is_empty() {
                push_normalized_byte(&mut normalized, b'-', index)?;
            }
            separator_pending = false;
            push_normalized_byte(
                &mut normalized,
                if ch == '_' {
                    b'_'
                } else {
                    ch.to_ascii_lowercase() as u8
                },
                index,
            )?;
        } else if ch.is_whitespace() || ch == '-' || ch.is_ascii_punctuation() {
            if !normalized.is_empty() {
                separator_pending = true;
            }
        }
    }

    if normalized.is_empty() {
        return Ok(None);
    }

    Ok(Some(normalized))
}

fn push_normalized_byte(
    normalized: &mut String,
    byte: u8,
    index: usize,
) -> Result<(), TagInputError> {
    if normalized.len() >= MAX_TAG_LABEL_BYTES {
        return Err(TagInputError::LabelTooLong {
            index,
            max: MAX_TAG_LABEL_BYTES,
            actual: normalized.len() + 1,
        });
    }
    normalized.push(byte as char);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_TAG_INPUT_BYTES, MAX_TAG_LABEL_BYTES, MAX_TAGS_PER_COMMIT, TagInputError,
        parse_tag_input,
    };

    #[test]
    fn normalization_matches_native_vocabulary_edges() {
        assert_eq!(
            parse_tag_input("  Deep--Kick!! 01  ").unwrap(),
            vec!["deep-kick-01"]
        );
        assert_eq!(
            parse_tag_input("  metal_floor  ").unwrap(),
            vec!["metal_floor"]
        );
        assert_eq!(
            parse_tag_input("a!@#$%^&*()[]{}:<>?/b").unwrap(),
            vec!["a-b"]
        );
        assert_eq!(parse_tag_input("---!!!---").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn unicode_is_ignored_while_unicode_whitespace_still_separates() {
        assert_eq!(parse_tag_input("a😀b").unwrap(), vec!["ab"]);
        assert_eq!(
            parse_tag_input("caf\u{e9}/\u{394}\u{2603}TAG").unwrap(),
            vec!["caf-tag"]
        );
        assert_eq!(
            parse_tag_input("left\u{2003}right").unwrap(),
            vec!["left-right"]
        );
    }

    #[test]
    fn delimiters_deduplicate_and_preserve_first_normalized_order() {
        assert_eq!(
            parse_tag_input("Kick, warm tone;KICK\nwarm-tone, , ;").unwrap(),
            vec!["kick", "warm-tone"]
        );
    }

    #[test]
    fn all_empty_segments_return_an_empty_commit() {
        assert_eq!(
            parse_tag_input(" ;,\n!!!, 😀 ; \t").unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn raw_input_limit_is_checked_before_parsing() {
        let input = "x".repeat(MAX_TAG_INPUT_BYTES + 1);
        assert_eq!(
            parse_tag_input(&input),
            Err(TagInputError::InputTooLarge {
                max: MAX_TAG_INPUT_BYTES,
                actual: MAX_TAG_INPUT_BYTES + 1,
            })
        );
    }

    #[test]
    fn normalized_label_limit_accepts_exact_boundary_and_rejects_oversize() {
        let accepted = "a".repeat(MAX_TAG_LABEL_BYTES);
        assert_eq!(parse_tag_input(&accepted).unwrap(), vec![accepted.clone()]);

        let rejected = "a".repeat(MAX_TAG_LABEL_BYTES + 1);
        assert_eq!(
            parse_tag_input(&rejected),
            Err(TagInputError::LabelTooLong {
                index: 0,
                max: MAX_TAG_LABEL_BYTES,
                actual: MAX_TAG_LABEL_BYTES + 1,
            })
        );
        let trailing_punctuation = format!("{accepted}!");
        assert_eq!(
            parse_tag_input(&trailing_punctuation).unwrap(),
            vec![accepted]
        );
    }

    #[test]
    fn distinct_label_limit_is_atomic_and_allows_duplicates() {
        let labels = (0..MAX_TAGS_PER_COMMIT)
            .map(|index| format!("tag{index}"))
            .collect::<Vec<_>>();
        let input = labels.join(",");
        assert_eq!(parse_tag_input(&input).unwrap(), labels);

        let duplicate_input = format!("{input},TAG0,tag1");
        assert_eq!(
            parse_tag_input(&duplicate_input).unwrap().len(),
            MAX_TAGS_PER_COMMIT
        );

        let over_limit = format!("{input},new-tag");
        assert_eq!(
            parse_tag_input(&over_limit),
            Err(TagInputError::TooManyLabels {
                max: MAX_TAGS_PER_COMMIT,
                actual: MAX_TAGS_PER_COMMIT + 1,
            })
        );
    }
}
