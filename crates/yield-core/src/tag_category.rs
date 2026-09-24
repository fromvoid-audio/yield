#![deny(missing_docs)]
#![deny(warnings)]

//! Fixed metadata tag categories and bounded native-compatible inference.

use crate::MAX_TAG_LABEL_BYTES;

/// Maximum UTF-8 byte length accepted by [`infer_tag_category`].
pub const MAX_TAG_CATEGORY_INPUT_BYTES: usize = MAX_TAG_LABEL_BYTES;

/// The fixed categories used by metadata tag projections.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TagCategory {
    /// Playback behavior tags such as `loop` and `one-shot`.
    PlaybackType,
    /// Sound source or role tags such as `kick` and `vocal`.
    SoundType,
    /// Character tags such as `warm` and `bright`.
    Character,
    /// Prefix and source naming tags such as `artist` and `pack`.
    Prefix,
    /// Tuning and scale tags such as `major` and `dorian`.
    TuningScale,
}

/// Categories in their stable display order.
pub const TAG_CATEGORIES: [TagCategory; 5] = [
    TagCategory::PlaybackType,
    TagCategory::SoundType,
    TagCategory::Character,
    TagCategory::Prefix,
    TagCategory::TuningScale,
];

/// Built-in metadata labels available before any source tags are loaded.
pub const DEFAULT_METADATA_TAGS: [&str; 2] = ["one-shot", "loop"];

impl TagCategory {
    /// Return the stable persisted category identifier.
    pub const fn id(self) -> &'static str {
        match self {
            Self::PlaybackType => "playback-type",
            Self::SoundType => "sound-type",
            Self::Character => "character",
            Self::Prefix => "prefix",
            Self::TuningScale => "tuning-scale",
        }
    }

    /// Return the stable user-facing category label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::PlaybackType => "Playback Type",
            Self::SoundType => "Sound Type",
            Self::Character => "Character",
            Self::Prefix => "Prefix",
            Self::TuningScale => "Tuning/Scale",
        }
    }

    /// Return the stable zero-based display order.
    pub const fn order(self) -> usize {
        match self {
            Self::PlaybackType => 0,
            Self::SoundType => 1,
            Self::Character => 2,
            Self::Prefix => 3,
            Self::TuningScale => 4,
        }
    }

    /// Return whether users may extend this category with new labels.
    pub const fn is_user_extensible(self) -> bool {
        !matches!(self, Self::PlaybackType)
    }

    /// Resolve one exact persisted category identifier.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "playback-type" => Some(Self::PlaybackType),
            "sound-type" => Some(Self::SoundType),
            "character" => Some(Self::Character),
            "prefix" => Some(Self::Prefix),
            "tuning-scale" => Some(Self::TuningScale),
            _ => None,
        }
    }
}

/// A bounded tag-category inference failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TagCategoryError {
    /// The input exceeded [`MAX_TAG_CATEGORY_INPUT_BYTES`].
    InputTooLarge {
        /// Maximum accepted input byte length.
        max: usize,
        /// Supplied input byte length.
        actual: usize,
    },
}

impl std::fmt::Display for TagCategoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InputTooLarge { max, actual } => write!(
                formatter,
                "tag category input is {actual} UTF-8 bytes; maximum is {max}"
            ),
        }
    }
}

impl std::error::Error for TagCategoryError {}

/// Infer the native category for one bounded tag label.
///
/// Category matching lowercases ASCII characters and treats hyphens, underscores, and Unicode
/// whitespace as one space. Other characters remain part of the match text. Exact native
/// vocabulary entries map to Playback Type, Sound Type, Character, or Tuning/Scale. Labels with
/// a `prefix`, `artist`, `pack`, or `project` prefix followed by `:` or a space map to Prefix;
/// every other label falls back to Character.
pub fn infer_tag_category(value: &str) -> Result<TagCategory, TagCategoryError> {
    if value.len() > MAX_TAG_CATEGORY_INPUT_BYTES {
        return Err(TagCategoryError::InputTooLarge {
            max: MAX_TAG_CATEGORY_INPUT_BYTES,
            actual: value.len(),
        });
    }

    let normalized = normalize_category_match(value);
    Ok(if PLAYBACK_TYPE_TAGS.contains(&normalized.as_str()) {
        TagCategory::PlaybackType
    } else if SOUND_TYPE_TAGS.contains(&normalized.as_str()) {
        TagCategory::SoundType
    } else if CHARACTER_TAGS.contains(&normalized.as_str()) {
        TagCategory::Character
    } else if TUNING_SCALE_TAGS.contains(&normalized.as_str()) {
        TagCategory::TuningScale
    } else if has_category_prefix(&normalized, "prefix")
        || has_category_prefix(&normalized, "artist")
        || has_category_prefix(&normalized, "pack")
        || has_category_prefix(&normalized, "project")
    {
        TagCategory::Prefix
    } else {
        TagCategory::Character
    })
}

const PLAYBACK_TYPE_TAGS: &[&str] = &["loop", "one shot", "oneshot"];
const SOUND_TYPE_TAGS: &[&str] = &[
    "kick",
    "snare",
    "clap",
    "hat",
    "bass",
    "stab",
    "texture",
    "vocal",
    "percussion",
    "ambience",
    "effect",
    "fx",
    "drum loop",
    "synth loop",
];
const CHARACTER_TAGS: &[&str] = &[
    "warm",
    "harsh",
    "clean",
    "noisy",
    "distorted",
    "punchy",
    "soft",
    "metallic",
    "dark",
    "bright",
    "wide",
    "dry",
    "wet",
    "raw",
    "polished",
];
const TUNING_SCALE_TAGS: &[&str] = &[
    "major",
    "minor",
    "dorian",
    "phrygian",
    "lydian",
    "mixolydian",
    "locrian",
    "pentatonic",
    "chromatic",
    "microtonal",
];

fn has_category_prefix(value: &str, prefix: &str) -> bool {
    value == prefix
        || value
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(':') || rest.starts_with(' '))
}

fn normalize_category_match(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut separator_pending = false;
    for ch in value.chars() {
        if ch == '-' || ch == '_' || ch.is_whitespace() {
            if !normalized.is_empty() {
                separator_pending = true;
            }
            continue;
        }
        if separator_pending {
            normalized.push(' ');
        }
        separator_pending = false;
        normalized.push(ch.to_ascii_lowercase());
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_METADATA_TAGS, MAX_TAG_CATEGORY_INPUT_BYTES, TAG_CATEGORIES, TagCategory,
        TagCategoryError, infer_tag_category,
    };

    #[test]
    fn category_ids_labels_order_and_extensibility_are_stable() {
        let expected = [
            (
                TagCategory::PlaybackType,
                "playback-type",
                "Playback Type",
                false,
            ),
            (TagCategory::SoundType, "sound-type", "Sound Type", true),
            (TagCategory::Character, "character", "Character", true),
            (TagCategory::Prefix, "prefix", "Prefix", true),
            (
                TagCategory::TuningScale,
                "tuning-scale",
                "Tuning/Scale",
                true,
            ),
        ];
        assert_eq!(TAG_CATEGORIES.len(), expected.len());
        for (order, (category, id, label, extensible)) in expected.into_iter().enumerate() {
            assert_eq!(TAG_CATEGORIES[order], category);
            assert_eq!(category.id(), id);
            assert_eq!(category.label(), label);
            assert_eq!(category.order(), order);
            assert_eq!(category.is_user_extensible(), extensible);
            assert_eq!(TagCategory::from_id(id), Some(category));
        }
        assert_eq!(DEFAULT_METADATA_TAGS, ["one-shot", "loop"]);
    }

    #[test]
    fn unknown_category_ids_are_rejected() {
        for id in ["", "Playback Type", "playback_type", "unknown", "tag"] {
            assert_eq!(TagCategory::from_id(id), None);
        }
    }

    #[test]
    fn inference_matches_native_vocabularies_and_prefixes() {
        let cases = [
            ("loop", TagCategory::PlaybackType),
            ("ONE SHOT", TagCategory::PlaybackType),
            ("one_shot", TagCategory::PlaybackType),
            ("oneshot", TagCategory::PlaybackType),
            ("DRUM--LOOP", TagCategory::SoundType),
            ("kick", TagCategory::SoundType),
            ("warm", TagCategory::Character),
            ("major", TagCategory::TuningScale),
            ("PREFIX:artist", TagCategory::Prefix),
            ("artist-name", TagCategory::Prefix),
            ("pack_name", TagCategory::Prefix),
            ("project\u{2003}bass", TagCategory::Prefix),
            ("unknown", TagCategory::Character),
        ];
        for (value, expected) in cases {
            assert_eq!(infer_tag_category(value), Ok(expected), "{value}");
        }
    }

    #[test]
    fn punctuation_remains_significant_to_category_matching() {
        assert_eq!(infer_tag_category("one!shot"), Ok(TagCategory::Character));
        assert_eq!(infer_tag_category("prefix.foo"), Ok(TagCategory::Character));
        assert_eq!(infer_tag_category("artist.foo"), Ok(TagCategory::Character));
        assert_eq!(infer_tag_category("prefix:foo"), Ok(TagCategory::Prefix));
    }

    #[test]
    fn category_input_bound_is_explicit_and_checked_before_normalization() {
        let accepted = "x".repeat(MAX_TAG_CATEGORY_INPUT_BYTES);
        assert_eq!(infer_tag_category(&accepted), Ok(TagCategory::Character));

        let rejected = "x".repeat(MAX_TAG_CATEGORY_INPUT_BYTES + 1);
        assert_eq!(
            infer_tag_category(&rejected),
            Err(TagCategoryError::InputTooLarge {
                max: MAX_TAG_CATEGORY_INPUT_BYTES,
                actual: MAX_TAG_CATEGORY_INPUT_BYTES + 1,
            })
        );
    }
}
