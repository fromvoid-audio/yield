#![deny(missing_docs)]
#![deny(warnings)]

//! Bounded tag-filter parsing and positional match masks for browser projections.

use std::sync::Arc;

/// Maximum UTF-8 byte length accepted for one tag-filter input value.
pub const MAX_TAG_FILTER_INPUT_BYTES: usize = 64 * 1024;

/// Maximum UTF-8 byte length accepted for one trimmed tag-filter token.
pub const MAX_TAG_FILTER_TOKEN_BYTES: usize = 1024;

/// Maximum number of distinct tag-filter tokens accepted in one input value.
pub const MAX_TAG_FILTER_TOKENS: usize = 64;

/// A bounded tag-filter parsing failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TagFilterError {
    /// The complete raw input exceeded [`MAX_TAG_FILTER_INPUT_BYTES`].
    InputTooLarge {
        /// Maximum raw input byte length.
        max: usize,
        /// Supplied raw input byte length.
        actual: usize,
    },
    /// A trimmed token exceeded [`MAX_TAG_FILTER_TOKEN_BYTES`].
    TokenTooLong {
        /// Zero-based comma-segment index containing the token.
        index: usize,
        /// Maximum token byte length.
        max: usize,
        /// Supplied trimmed token byte length.
        actual: usize,
    },
    /// The input contained more distinct normalized tokens than allowed.
    TooManyTokens {
        /// Maximum number of distinct tokens.
        max: usize,
        /// Number of distinct tokens discovered when the limit was exceeded.
        actual: usize,
    },
}

impl std::fmt::Display for TagFilterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InputTooLarge { max, actual } => write!(
                formatter,
                "tag filter input is {actual} UTF-8 bytes; maximum is {max}"
            ),
            Self::TokenTooLong { index, max, actual } => write!(
                formatter,
                "tag filter token at comma segment {index} is {actual} UTF-8 bytes; maximum is {max}"
            ),
            Self::TooManyTokens { max, actual } => write!(
                formatter,
                "tag filter contains {actual} distinct tokens; maximum is {max}"
            ),
        }
    }
}

impl std::error::Error for TagFilterError {}

/// Distinct, ASCII-lowercased requirements from one bounded tag-filter input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TagFilterQuery {
    tokens: Vec<String>,
}

impl TagFilterQuery {
    /// Return the distinct normalized requirements in first-occurrence order.
    pub fn tokens(&self) -> &[String] {
        &self.tokens
    }

    /// Return whether the input contained no nonempty requirements.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Return the number of distinct requirements.
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Consume the query and return its normalized requirements.
    pub fn into_tokens(self) -> Vec<String> {
        self.tokens
    }
}

/// Parse a bounded comma-separated tag-filter input.
///
/// Commas are the only delimiters. Each segment is trimmed, empty segments are discarded, and
/// the remaining text is lowercased with ASCII case folding. Punctuation, embedded newlines,
/// Unicode characters, and embedded NUL bytes remain part of the token text. Duplicate normalized
/// tokens are removed while preserving first-occurrence order. Any bound failure rejects the
/// complete input instead of returning a partial query.
pub fn parse_tag_filter(value: &str) -> Result<TagFilterQuery, TagFilterError> {
    if value.len() > MAX_TAG_FILTER_INPUT_BYTES {
        return Err(TagFilterError::InputTooLarge {
            max: MAX_TAG_FILTER_INPUT_BYTES,
            actual: value.len(),
        });
    }

    let mut tokens = Vec::new();
    for (index, segment) in value.split(',').enumerate() {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.len() > MAX_TAG_FILTER_TOKEN_BYTES {
            return Err(TagFilterError::TokenTooLong {
                index,
                max: MAX_TAG_FILTER_TOKEN_BYTES,
                actual: trimmed.len(),
            });
        }

        let normalized = trimmed.to_ascii_lowercase();
        if tokens.iter().any(|token| token == &normalized) {
            continue;
        }
        let actual = tokens.len() + 1;
        if actual > MAX_TAG_FILTER_TOKENS {
            return Err(TagFilterError::TooManyTokens {
                max: MAX_TAG_FILTER_TOKENS,
                actual,
            });
        }
        tokens.push(normalized);
    }

    Ok(TagFilterQuery { tokens })
}

/// A compact immutable positional match mask for one exact entry slice.
///
/// Bit `index % 64` in word `index / 64` represents the entry at that position. The mask is
/// intentionally independent of source identity, revision, and display sorting; callers must
/// bind it to the exact entry slice before applying it to a browser projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TagFilterMask {
    entry_count: usize,
    words: Arc<[u64]>,
}

/// A failure while validating a positional tag-filter mask.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TagFilterMaskError {
    /// The entry count was too large to derive a checked word count.
    EntryCountOverflow {
        /// Supplied entry count.
        entry_count: usize,
    },
    /// The supplied word count does not cover exactly the requested entry count.
    WordCountMismatch {
        /// Supplied entry count.
        entry_count: usize,
        /// Required number of words.
        expected: usize,
        /// Supplied number of words.
        actual: usize,
    },
    /// Bits beyond the final entry were set in the final word.
    UnusedTailBitsSet {
        /// Supplied entry count.
        entry_count: usize,
        /// Zero-based index of the final word.
        word_index: usize,
        /// Bits that must be clear in the final word.
        invalid_bits: u64,
    },
}

impl std::fmt::Display for TagFilterMaskError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EntryCountOverflow { entry_count } => write!(
                formatter,
                "tag filter entry count {entry_count} cannot be represented safely"
            ),
            Self::WordCountMismatch {
                entry_count,
                expected,
                actual,
            } => write!(
                formatter,
                "tag filter mask for {entry_count} entries needs {expected} words, got {actual}"
            ),
            Self::UnusedTailBitsSet {
                entry_count,
                word_index,
                invalid_bits,
            } => write!(
                formatter,
                "tag filter mask for {entry_count} entries sets unused bits {invalid_bits:#018x} in word {word_index}"
            ),
        }
    }
}

impl std::error::Error for TagFilterMaskError {}

impl TagFilterMask {
    /// Validate and retain an immutable positional mask without allocating from `entry_count`.
    pub fn try_from_words(
        entry_count: usize,
        words: Arc<[u64]>,
    ) -> Result<Self, TagFilterMaskError> {
        let expected = required_word_count(entry_count)
            .ok_or(TagFilterMaskError::EntryCountOverflow { entry_count })?;
        if words.len() != expected {
            return Err(TagFilterMaskError::WordCountMismatch {
                entry_count,
                expected,
                actual: words.len(),
            });
        }
        if let Some(last_word) = words.last()
            && let Some(invalid_bits) = unused_tail_bits(entry_count)
            && *last_word & invalid_bits != 0
        {
            return Err(TagFilterMaskError::UnusedTailBitsSet {
                entry_count,
                word_index: expected - 1,
                invalid_bits: *last_word & invalid_bits,
            });
        }
        Ok(Self { entry_count, words })
    }

    /// Validate and retain a vector as an immutable positional mask.
    pub fn try_from_vec(entry_count: usize, words: Vec<u64>) -> Result<Self, TagFilterMaskError> {
        Self::try_from_words(entry_count, Arc::from(words))
    }

    /// Return the number of entries this mask addresses.
    pub const fn entry_count(&self) -> usize {
        self.entry_count
    }

    /// Return the validated immutable words backing this mask.
    pub fn words(&self) -> &[u64] {
        &self.words
    }

    /// Return whether one positional entry is included by this mask.
    pub fn contains(&self, index: usize) -> bool {
        if index >= self.entry_count {
            return false;
        }
        self.words[index / 64] & (1u64 << (index % 64)) != 0
    }
}

fn required_word_count(entry_count: usize) -> Option<usize> {
    entry_count.checked_add(63).map(|count| count / 64)
}

fn unused_tail_bits(entry_count: usize) -> Option<u64> {
    let used_bits = entry_count % 64;
    (used_bits != 0).then(|| !((1u64 << used_bits) - 1))
}

/// The browser's current external tag-filter result state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TagFilterState {
    /// No tag requirements are active.
    Disabled,
    /// Tag requirements are active, but no validated mask is available yet.
    Pending,
    /// Tag requirements are active with a validated positional result mask.
    Ready(TagFilterMask),
}

/// A failure while applying a positional tag-filter result to a browser projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TagFilterApplyError {
    /// A result cannot be applied while tag filtering is disabled.
    Disabled,
    /// The result addresses a different entry slice than the projection being rebuilt.
    EntryCountMismatch {
        /// Number of entries in the projection.
        expected: usize,
        /// Number of entries addressed by the supplied mask.
        actual: usize,
    },
}

impl std::fmt::Display for TagFilterApplyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => formatter.write_str("tag filtering is disabled"),
            Self::EntryCountMismatch { expected, actual } => write!(
                formatter,
                "tag filter result addresses {actual} entries; projection has {expected}"
            ),
        }
    }
}

impl std::error::Error for TagFilterApplyError {}

impl TagFilterState {
    /// Construct a disabled tag-filter state.
    pub const fn disabled() -> Self {
        Self::Disabled
    }

    /// Return whether tag requirements are enabled, including while their result is pending.
    pub const fn is_enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Return whether a result mask is still pending.
    pub const fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }

    /// Borrow the validated mask when the filter is ready.
    pub fn mask(&self) -> Option<&TagFilterMask> {
        match self {
            Self::Ready(mask) => Some(mask),
            Self::Disabled | Self::Pending => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        MAX_TAG_FILTER_INPUT_BYTES, MAX_TAG_FILTER_TOKEN_BYTES, MAX_TAG_FILTER_TOKENS,
        TagFilterError, TagFilterMask, TagFilterMaskError, parse_tag_filter,
    };

    #[test]
    fn parser_splits_only_on_commas_and_deduplicates_ascii_case() {
        let query = parse_tag_filter("  Kick, warm\nTone, KICK, , ,Snare!  ").unwrap();
        assert_eq!(
            query.tokens(),
            ["kick", "warm\ntone", "snare!"].map(String::from)
        );
    }

    #[test]
    fn parser_preserves_internal_punctuation_newline_unicode_and_nul() {
        let query = parse_tag_filter("A-B, line\nBreak, Café, nul\0tag").unwrap();
        assert_eq!(
            query.tokens(),
            ["a-b", "line\nbreak", "café", "nul\0tag"].map(String::from)
        );
    }

    #[test]
    fn parser_rejects_whole_input_at_each_bound() {
        let exact_token = "a".repeat(MAX_TAG_FILTER_TOKEN_BYTES);
        assert_eq!(parse_tag_filter(&exact_token).unwrap().len(), 1);
        let oversized_token = "a".repeat(MAX_TAG_FILTER_TOKEN_BYTES + 1);
        assert_eq!(
            parse_tag_filter(&oversized_token),
            Err(TagFilterError::TokenTooLong {
                index: 0,
                max: MAX_TAG_FILTER_TOKEN_BYTES,
                actual: MAX_TAG_FILTER_TOKEN_BYTES + 1,
            })
        );

        let too_many = (0..=MAX_TAG_FILTER_TOKENS)
            .map(|index| format!("tag-{index}"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            parse_tag_filter(&too_many),
            Err(TagFilterError::TooManyTokens {
                max: MAX_TAG_FILTER_TOKENS,
                actual: MAX_TAG_FILTER_TOKENS + 1,
            })
        );

        let oversized_input = "x".repeat(MAX_TAG_FILTER_INPUT_BYTES + 1);
        assert_eq!(
            parse_tag_filter(&oversized_input),
            Err(TagFilterError::InputTooLarge {
                max: MAX_TAG_FILTER_INPUT_BYTES,
                actual: MAX_TAG_FILTER_INPUT_BYTES + 1,
            })
        );
    }

    #[test]
    fn empty_segments_produce_no_requirements() {
        let query = parse_tag_filter(" , ,, \t,").unwrap();
        assert!(query.is_empty());
    }

    #[test]
    fn mask_validates_boundaries_and_tail_bits() {
        for entry_count in [0usize, 1, 63, 64, 65, 127, 128] {
            let word_count = entry_count.div_ceil(64);
            let mask = TagFilterMask::try_from_vec(entry_count, vec![0; word_count])
                .expect("zero mask is valid");
            assert_eq!(mask.entry_count(), entry_count);
            assert!(!mask.contains(entry_count));
        }

        let mask = TagFilterMask::try_from_vec(64, vec![1u64 << 63]).unwrap();
        assert!(mask.contains(63));
        assert!(!mask.contains(64));

        let mask = TagFilterMask::try_from_vec(65, vec![0, 1]).unwrap();
        assert!(mask.contains(64));
        assert!(!mask.contains(63));

        let mask = TagFilterMask::try_from_vec(63, vec![(1u64 << 62)]).unwrap();
        assert!(mask.contains(62));
        assert_eq!(
            TagFilterMask::try_from_vec(63, vec![1u64 << 63]),
            Err(TagFilterMaskError::UnusedTailBitsSet {
                entry_count: 63,
                word_index: 0,
                invalid_bits: 1u64 << 63,
            })
        );
        assert_eq!(mask.words(), &[1u64 << 62]);
    }

    #[test]
    fn mask_rejects_mismatched_word_lengths_without_padding() {
        assert_eq!(
            TagFilterMask::try_from_words(64, Arc::from([])),
            Err(TagFilterMaskError::WordCountMismatch {
                entry_count: 64,
                expected: 1,
                actual: 0,
            })
        );
        assert_eq!(
            TagFilterMask::try_from_words(0, Arc::from([0u64])),
            Err(TagFilterMaskError::WordCountMismatch {
                entry_count: 0,
                expected: 0,
                actual: 1,
            })
        );
    }
}
