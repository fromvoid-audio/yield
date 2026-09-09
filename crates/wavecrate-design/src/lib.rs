#![deny(missing_docs)]
#![deny(warnings)]

//! GPUI-independent design tokens and validated theme-file loading.
//!
//! The theme file is deliberately complete: every semantic token is required so a custom theme
//! cannot silently inherit a stale or unrelated value. The UI consumes [`Theme`] values through
//! its own GPUI adapter, which keeps this crate usable from tests, tools, and future frontends.

use std::{fmt, fs, io::Read, path::Path};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub mod motion;

pub use motion::Motion;

/// The only schema currently understood by Wavecrate.
pub const THEME_SCHEMA_VERSION: u32 = 1;

/// Maximum number of bytes read from an imported theme file.
pub const MAX_THEME_FILE_BYTES: usize = 128 * 1024;

const MAX_THEME_NAME_CHARS: usize = 64;
const MAX_FONT_FAMILY_CHARS: usize = 80;

/// A color represented as straight-alpha RGBA bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Color {
    red: u8,
    green: u8,
    blue: u8,
    alpha: u8,
}

impl Color {
    /// Construct a color from red, green, blue, and alpha bytes.
    pub const fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }

    /// Construct an opaque color from red, green, and blue bytes.
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self::rgba(red, green, blue, u8::MAX)
    }

    /// Return the red component.
    pub const fn red(self) -> u8 {
        self.red
    }

    /// Return the green component.
    pub const fn green(self) -> u8 {
        self.green
    }

    /// Return the blue component.
    pub const fn blue(self) -> u8 {
        self.blue
    }

    /// Return the alpha component.
    pub const fn alpha(self) -> u8 {
        self.alpha
    }

    /// Return this color packed as `0xRRGGBBAA` for a renderer adapter.
    pub const fn to_rgba32(self) -> u32 {
        u32::from_be_bytes([self.red, self.green, self.blue, self.alpha])
    }

    /// Return the canonical uppercase JSON representation.
    pub fn to_hex(self) -> String {
        format!(
            "#{:02X}{:02X}{:02X}{:02X}",
            self.red, self.green, self.blue, self.alpha
        )
    }
}

impl Serialize for Color {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_color(&value).map_err(de::Error::custom)
    }
}

fn parse_color(value: &str) -> Result<Color, String> {
    let Some(hex) = value.strip_prefix('#') else {
        return Err(format!("color '{value}' must start with '#'"));
    };
    if hex.len() != 6 && hex.len() != 8 {
        return Err(format!(
            "color '{value}' must use exactly 6 or 8 hexadecimal digits"
        ));
    }
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("color '{value}' contains a non-hexadecimal digit"));
    }
    let parse_component = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&hex[range], 16).map_err(|_| format!("invalid color '{value}'"))
    };
    Ok(Color::rgba(
        parse_component(0..2)?,
        parse_component(2..4)?,
        parse_component(4..6)?,
        if hex.len() == 8 {
            parse_component(6..8)?
        } else {
            u8::MAX
        },
    ))
}

fn default_waveform_playhead() -> Color {
    Color::rgba(231, 229, 223, 210)
}

fn default_waveform_edit_selection() -> Color {
    Color::rgb(174, 176, 173)
}

fn default_waveform_similar_fill() -> Color {
    Color::rgba(114, 235, 184, 54)
}

fn default_waveform_similar_rail() -> Color {
    Color::rgba(155, 255, 218, 210)
}

fn default_waveform_similar_hover_fill() -> Color {
    Color::rgba(156, 255, 218, 92)
}

fn default_waveform_similar_hover_rail() -> Color {
    Color::rgba(219, 255, 240, 255)
}

fn default_rating_keep() -> Color {
    Color::rgba(122, 226, 96, 235)
}

fn default_rating_trash() -> Color {
    Color::rgba(238, 77, 67, 235)
}

fn default_rating_neutral() -> Color {
    Color::rgba(184, 188, 194, 235)
}

fn default_collection_slots() -> [Color; 6] {
    [
        Color::rgba(242, 100, 79, 232),
        Color::rgba(210, 158, 81, 224),
        Color::rgba(199, 188, 92, 224),
        Color::rgba(112, 172, 111, 224),
        Color::rgba(83, 161, 194, 224),
        Color::rgba(149, 116, 185, 224),
    ]
}

fn default_waveform_scrollbar_height() -> f32 {
    6.0
}

fn default_rating_column_width() -> f32 {
    56.0
}

fn default_collection_column_width() -> f32 {
    72.0
}

fn default_playback_type_column_width() -> f32 {
    76.0
}

fn default_last_played_column_width() -> f32 {
    112.0
}

/// Semantic colors used by the Wavecrate application.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Colors {
    /// Window and workspace background.
    pub background: Color,
    /// Base panel surface.
    pub surface: Color,
    /// Raised panel or toolbar surface.
    pub surface_raised: Color,
    /// Overlay and active-control surface.
    pub surface_overlay: Color,
    /// Standard divider color.
    pub border: Color,
    /// Emphasized divider color.
    pub border_emphasis: Color,
    /// Primary readable text.
    pub text_primary: Color,
    /// Secondary and metadata text.
    pub text_muted: Color,
    /// Disabled text and controls.
    pub text_disabled: Color,
    /// Primary coral accent.
    pub accent: Color,
    /// Error and destructive-action color.
    pub danger: Color,
    /// Warning and loading color.
    pub warning: Color,
    /// Keep rating indicator color.
    #[serde(default = "default_rating_keep")]
    pub rating_keep: Color,
    /// Trash rating indicator color.
    #[serde(default = "default_rating_trash")]
    pub rating_trash: Color,
    /// Neutral or unrated indicator color.
    #[serde(default = "default_rating_neutral")]
    pub rating_neutral: Color,
    /// Fixed collection swatch colors, in displayed slots one through six.
    #[serde(default = "default_collection_slots")]
    pub collection_slots: [Color; 6],
    /// Faint selected-row fill.
    pub selection_fill: Color,
    /// Selected-row hover fill.
    pub selection_hover_fill: Color,
    /// Disabled control fill.
    pub control_disabled_fill: Color,
    /// Unplayed waveform bars.
    pub waveform_unplayed: Color,
    /// Dark background behind the waveform envelope.
    pub waveform_background: Color,
    /// Blue low-frequency waveform layer.
    pub waveform_low: Color,
    /// Coral mid-frequency waveform layer.
    pub waveform_mid: Color,
    /// Pale high-frequency waveform layer.
    pub waveform_high: Color,
    /// Pale playback cursor drawn over the bounded waveform preview.
    #[serde(default = "default_waveform_playhead")]
    pub waveform_playhead: Color,
    /// Neutral edit-selection color, independent of the playback selection accent.
    #[serde(default = "default_waveform_edit_selection")]
    pub waveform_edit_selection: Color,
    /// Similar-section fill color.
    #[serde(default = "default_waveform_similar_fill")]
    pub waveform_similar_fill: Color,
    /// Similar-section rail color.
    #[serde(default = "default_waveform_similar_rail")]
    pub waveform_similar_rail: Color,
    /// Similar-section hover fill color.
    #[serde(default = "default_waveform_similar_hover_fill")]
    pub waveform_similar_hover_fill: Color,
    /// Similar-section hover rail color.
    #[serde(default = "default_waveform_similar_hover_rail")]
    pub waveform_similar_hover_rail: Color,
    /// Subtle raw waveform carrier layer.
    pub waveform_raw: Color,
    /// Played waveform bars.
    pub waveform_played: Color,
    /// Waveform grid color.
    pub waveform_grid: Color,
    /// Waveform label background.
    pub waveform_label: Color,
}

/// Typography tokens shared by every GPUI surface.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Typography {
    /// Font family registered by the frontend.
    pub font_family: String,
    /// Standard body text size in logical points.
    pub body_size: f32,
    /// Compact metadata text size in logical points.
    pub small_size: f32,
    /// Toolbar and window title size in logical points.
    pub title_size: f32,
    /// Waveform title size in logical points.
    pub waveform_title_size: f32,
    /// Body line height in logical points.
    pub line_height: f32,
}

/// Layout and density tokens shared by every GPUI surface.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spacing {
    /// Top toolbar height.
    pub toolbar_height: f32,
    /// Leading inset reserved for macOS traffic lights in the transparent chrome.
    pub toolbar_leading_inset: f32,
    /// Leading source sidebar width.
    pub sidebar_width: f32,
    /// Waveform panel height.
    pub waveform_height: f32,
    /// Bottom status bar height.
    pub status_height: f32,
    /// Sample browser row height.
    pub row_height: f32,
    /// Horizontal row inset.
    pub row_padding_x: f32,
    /// Gap between compact controls.
    pub control_gap: f32,
    /// Standard toolbar and sidebar inset.
    pub panel_padding: f32,
    /// Control height.
    pub control_height: f32,
    /// Divider thickness.
    pub border_width: f32,
    /// Selected-row leading marker width.
    pub selected_marker_width: f32,
    /// Rating column width in the sample browser.
    #[serde(default = "default_rating_column_width")]
    pub rating_column_width: f32,
    /// Optional playback-type column width in the sample browser.
    #[serde(default = "default_playback_type_column_width")]
    pub playback_type_column_width: f32,
    /// Collection membership column width in the sample browser.
    #[serde(default = "default_collection_column_width")]
    pub collection_column_width: f32,
    /// Extension column width in the sample browser.
    pub extension_column_width: f32,
    /// File-size column width in the sample browser.
    pub size_column_width: f32,
    /// Duration column width in the sample browser.
    pub duration_column_width: f32,
    /// Last-played column width in the sample browser.
    #[serde(default = "default_last_played_column_width")]
    pub last_played_column_width: f32,
    /// Waveform bar gap.
    pub waveform_bar_gap: f32,
    /// Minimum waveform bar height.
    pub waveform_min_bar_height: f32,
}

/// Interaction tokens for selected, focused, hovered, and pressed controls.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Interaction {
    /// Hover fill for compact controls.
    pub hover_fill: Color,
    /// Pressed fill for compact controls.
    pub pressed_fill: Color,
    /// Focus outline color.
    pub focus_outline: Color,
    /// Focus outline width.
    pub focus_outline_width: f32,
}

/// Rendering tokens specific to the waveform preview.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Waveform {
    /// Relative height multiplier for a preview peak.
    pub peak_scale: f32,
    /// Height of the waveform label row.
    pub title_height: f32,
    /// Height of a compact waveform metadata row.
    pub metadata_height: f32,
    /// Height of the horizontal waveform scrollbar in logical points.
    #[serde(default = "default_waveform_scrollbar_height")]
    pub scrollbar_height: f32,
}

/// A complete, versioned Wavecrate theme.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Theme {
    /// Schema version for forward-compatible validation.
    pub schema_version: u32,
    /// Human-readable theme name.
    pub name: String,
    /// Semantic color tokens.
    pub colors: Colors,
    /// Typography tokens.
    pub typography: Typography,
    /// Layout and density tokens.
    pub spacing: Spacing,
    /// Interaction state tokens.
    pub interaction: Interaction,
    /// Timings and frequencies for presentation motion.
    #[serde(default)]
    pub motion: Motion,
    /// Waveform rendering tokens.
    pub waveform: Waveform,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            schema_version: THEME_SCHEMA_VERSION,
            name: "Wavecrate Editorial Terminal".to_string(),
            colors: Colors {
                background: Color::rgb(27, 30, 30),
                surface: Color::rgb(27, 30, 30),
                surface_raised: Color::rgb(27, 30, 30),
                surface_overlay: Color::rgb(42, 45, 45),
                border: Color::rgb(58, 61, 61),
                border_emphasis: Color::rgb(64, 67, 66),
                text_primary: Color::rgb(216, 215, 211),
                text_muted: Color::rgb(153, 155, 154),
                text_disabled: Color::rgb(112, 115, 114),
                accent: Color::rgb(233, 88, 67),
                danger: Color::rgb(239, 76, 61),
                warning: Color::rgb(217, 151, 95),
                rating_keep: default_rating_keep(),
                rating_trash: default_rating_trash(),
                rating_neutral: default_rating_neutral(),
                collection_slots: default_collection_slots(),
                selection_fill: Color::rgba(233, 88, 67, 8),
                selection_hover_fill: Color::rgba(233, 88, 67, 16),
                control_disabled_fill: Color::rgb(36, 40, 41),
                waveform_unplayed: Color::rgb(112, 115, 114),
                waveform_background: Color::rgb(21, 24, 25),
                waveform_low: Color::rgba(61, 72, 184, 240),
                waveform_mid: Color::rgba(224, 71, 47, 235),
                waveform_high: Color::rgba(232, 236, 237, 209),
                waveform_playhead: default_waveform_playhead(),
                waveform_edit_selection: default_waveform_edit_selection(),
                waveform_similar_fill: default_waveform_similar_fill(),
                waveform_similar_rail: default_waveform_similar_rail(),
                waveform_similar_hover_fill: default_waveform_similar_hover_fill(),
                waveform_similar_hover_rail: default_waveform_similar_hover_rail(),
                waveform_raw: Color::rgba(236, 232, 223, 13),
                waveform_played: Color::rgb(98, 102, 106),
                waveform_grid: Color::rgb(54, 57, 57),
                waveform_label: Color::rgb(47, 50, 50),
            },
            typography: Typography {
                font_family: "Ioskeley Mono".to_string(),
                body_size: 13.0,
                small_size: 11.0,
                title_size: 14.0,
                waveform_title_size: 12.0,
                line_height: 18.0,
            },
            spacing: Spacing {
                toolbar_height: 42.0,
                toolbar_leading_inset: 90.0,
                sidebar_width: 260.0,
                waveform_height: 226.0,
                status_height: 30.0,
                row_height: 22.0,
                row_padding_x: 12.0,
                control_gap: 6.0,
                panel_padding: 10.0,
                control_height: 26.0,
                border_width: 1.0,
                selected_marker_width: 2.0,
                rating_column_width: default_rating_column_width(),
                playback_type_column_width: default_playback_type_column_width(),
                collection_column_width: default_collection_column_width(),
                extension_column_width: 64.0,
                size_column_width: 88.0,
                duration_column_width: 88.0,
                last_played_column_width: default_last_played_column_width(),
                waveform_bar_gap: 1.0,
                waveform_min_bar_height: 3.0,
            },
            interaction: Interaction {
                hover_fill: Color::rgba(174, 176, 173, 36),
                pressed_fill: Color::rgba(233, 88, 67, 72),
                focus_outline: Color::rgb(231, 229, 223),
                focus_outline_width: 1.0,
            },
            motion: Motion::default(),
            waveform: Waveform {
                peak_scale: 0.78,
                title_height: 24.0,
                metadata_height: 20.0,
                scrollbar_height: 6.0,
            },
        }
    }
}

/// Errors returned while parsing, validating, or reading a theme.
#[derive(Debug)]
pub enum ThemeError {
    /// The theme file exceeded [`MAX_THEME_FILE_BYTES`].
    FileTooLarge {
        /// Maximum accepted file size.
        limit: usize,
    },
    /// The theme file could not be inspected or read.
    Io(std::io::Error),
    /// JSON syntax or shape was invalid.
    Json(serde_json::Error),
    /// A decoded theme violated a semantic validation rule.
    Invalid(String),
}

impl fmt::Display for ThemeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileTooLarge { limit } => {
                write!(formatter, "theme file exceeds the {limit}-byte limit")
            }
            Self::Io(error) => write!(formatter, "could not read theme file: {error}"),
            Self::Json(error) => write!(formatter, "invalid theme JSON: {error}"),
            Self::Invalid(error) => write!(formatter, "invalid theme: {error}"),
        }
    }
}

impl std::error::Error for ThemeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::FileTooLarge { .. } | Self::Invalid(_) => None,
        }
    }
}

/// Parse and validate a complete JSON theme.
pub fn parse_theme_json(json: &str) -> Result<Theme, ThemeError> {
    let theme = serde_json::from_str::<Theme>(json).map_err(ThemeError::Json)?;
    validate_theme(theme)
}

/// Serialize a theme using the canonical pretty-printed JSON representation.
pub fn theme_to_json(theme: &Theme) -> Result<String, ThemeError> {
    serde_json::to_string_pretty(theme).map_err(ThemeError::Json)
}

/// Return the canonical built-in theme as pretty-printed JSON.
pub fn default_theme_json() -> Result<String, ThemeError> {
    theme_to_json(&Theme::default())
}

/// Read, parse, and validate a theme file with a strict byte limit.
pub fn load_theme_file(path: &Path) -> Result<Theme, ThemeError> {
    let metadata = fs::metadata(path).map_err(ThemeError::Io)?;
    if !metadata.is_file() {
        return Err(ThemeError::Invalid(
            "theme path must refer to a regular file".to_string(),
        ));
    }
    if metadata.len() > MAX_THEME_FILE_BYTES as u64 {
        return Err(ThemeError::FileTooLarge {
            limit: MAX_THEME_FILE_BYTES,
        });
    }
    let mut file = fs::File::open(path).map_err(ThemeError::Io)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize + 1);
    file.by_ref()
        .take((MAX_THEME_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(ThemeError::Io)?;
    if bytes.len() > MAX_THEME_FILE_BYTES {
        return Err(ThemeError::FileTooLarge {
            limit: MAX_THEME_FILE_BYTES,
        });
    }
    let json = String::from_utf8(bytes)
        .map_err(|error| ThemeError::Invalid(format!("theme file is not UTF-8: {error}")))?;
    parse_theme_json(&json)
}

/// Validate a complete theme value before publishing or persisting it.
pub fn validate_theme(mut theme: Theme) -> Result<Theme, ThemeError> {
    if theme.schema_version != THEME_SCHEMA_VERSION {
        return Err(ThemeError::Invalid(format!(
            "schema_version {} is unsupported; expected {}",
            theme.schema_version, THEME_SCHEMA_VERSION
        )));
    }
    if theme.name.trim().is_empty() || theme.name.chars().count() > MAX_THEME_NAME_CHARS {
        return Err(ThemeError::Invalid(format!(
            "name must contain 1-{MAX_THEME_NAME_CHARS} characters"
        )));
    }
    if theme.name.chars().any(char::is_control) {
        return Err(ThemeError::Invalid(
            "name must not contain control characters".to_string(),
        ));
    }
    if theme.typography.font_family.trim().is_empty()
        || theme.typography.font_family.chars().count() > MAX_FONT_FAMILY_CHARS
        || theme.typography.font_family.chars().any(char::is_control)
    {
        return Err(ThemeError::Invalid(format!(
            "typography.font_family must contain 1-{MAX_FONT_FAMILY_CHARS} visible characters"
        )));
    }
    finite_range(
        "typography.body_size",
        theme.typography.body_size,
        8.0,
        32.0,
    )?;
    finite_range(
        "typography.small_size",
        theme.typography.small_size,
        7.0,
        24.0,
    )?;
    finite_range(
        "typography.title_size",
        theme.typography.title_size,
        8.0,
        48.0,
    )?;
    finite_range(
        "typography.waveform_title_size",
        theme.typography.waveform_title_size,
        8.0,
        32.0,
    )?;
    finite_range(
        "typography.line_height",
        theme.typography.line_height,
        10.0,
        72.0,
    )?;
    finite_range(
        "spacing.toolbar_height",
        theme.spacing.toolbar_height,
        24.0,
        96.0,
    )?;
    finite_range(
        "spacing.toolbar_leading_inset",
        theme.spacing.toolbar_leading_inset,
        0.0,
        240.0,
    )?;
    finite_range(
        "spacing.sidebar_width",
        theme.spacing.sidebar_width,
        120.0,
        640.0,
    )?;
    finite_range(
        "spacing.waveform_height",
        theme.spacing.waveform_height,
        120.0,
        640.0,
    )?;
    finite_range(
        "spacing.status_height",
        theme.spacing.status_height,
        20.0,
        72.0,
    )?;
    finite_range("spacing.row_height", theme.spacing.row_height, 16.0, 96.0)?;
    finite_range(
        "spacing.row_padding_x",
        theme.spacing.row_padding_x,
        0.0,
        48.0,
    )?;
    finite_range("spacing.control_gap", theme.spacing.control_gap, 0.0, 32.0)?;
    finite_range(
        "spacing.panel_padding",
        theme.spacing.panel_padding,
        0.0,
        48.0,
    )?;
    finite_range(
        "spacing.control_height",
        theme.spacing.control_height,
        18.0,
        96.0,
    )?;
    finite_range("spacing.border_width", theme.spacing.border_width, 0.0, 8.0)?;
    finite_range(
        "spacing.selected_marker_width",
        theme.spacing.selected_marker_width,
        1.0,
        16.0,
    )?;
    finite_range(
        "spacing.rating_column_width",
        theme.spacing.rating_column_width,
        32.0,
        160.0,
    )?;
    finite_range(
        "spacing.collection_column_width",
        theme.spacing.collection_column_width,
        32.0,
        240.0,
    )?;
    finite_range(
        "spacing.extension_column_width",
        theme.spacing.extension_column_width,
        32.0,
        240.0,
    )?;
    finite_range(
        "spacing.size_column_width",
        theme.spacing.size_column_width,
        32.0,
        240.0,
    )?;
    finite_range(
        "spacing.duration_column_width",
        theme.spacing.duration_column_width,
        32.0,
        240.0,
    )?;
    finite_range(
        "spacing.playback_type_column_width",
        theme.spacing.playback_type_column_width,
        32.0,
        240.0,
    )?;
    finite_range(
        "spacing.last_played_column_width",
        theme.spacing.last_played_column_width,
        32.0,
        240.0,
    )?;
    finite_range(
        "spacing.waveform_bar_gap",
        theme.spacing.waveform_bar_gap,
        0.0,
        16.0,
    )?;
    finite_range(
        "spacing.waveform_min_bar_height",
        theme.spacing.waveform_min_bar_height,
        1.0,
        32.0,
    )?;
    finite_range(
        "interaction.focus_outline_width",
        theme.interaction.focus_outline_width,
        0.0,
        8.0,
    )?;
    theme.motion.validate().map_err(ThemeError::Invalid)?;
    finite_range("waveform.peak_scale", theme.waveform.peak_scale, 0.1, 2.0)?;
    finite_range(
        "waveform.title_height",
        theme.waveform.title_height,
        16.0,
        64.0,
    )?;
    finite_range(
        "waveform.metadata_height",
        theme.waveform.metadata_height,
        16.0,
        64.0,
    )?;
    finite_range(
        "waveform.scrollbar_height",
        theme.waveform.scrollbar_height,
        2.0,
        12.0,
    )?;
    if theme.spacing.row_height < theme.typography.body_size * 1.15 {
        return Err(ThemeError::Invalid(
            "spacing.row_height must leave room for body text".to_string(),
        ));
    }
    if theme.spacing.control_height < theme.typography.body_size * 1.1 {
        return Err(ThemeError::Invalid(
            "spacing.control_height must leave room for body text".to_string(),
        ));
    }
    if theme.spacing.row_height < theme.typography.line_height {
        return Err(ThemeError::Invalid(
            "spacing.row_height must be at least typography.line_height".to_string(),
        ));
    }
    if theme.spacing.control_height < theme.typography.line_height {
        return Err(ThemeError::Invalid(
            "spacing.control_height must be at least typography.line_height".to_string(),
        ));
    }
    if theme.spacing.toolbar_height < theme.typography.line_height + 4.0 {
        return Err(ThemeError::Invalid(
            "spacing.toolbar_height must leave room for line height and toolbar padding"
                .to_string(),
        ));
    }
    if theme.waveform.title_height < theme.typography.waveform_title_size * 1.25 {
        return Err(ThemeError::Invalid(
            "waveform.title_height must leave room for waveform title text".to_string(),
        ));
    }
    if theme.waveform.metadata_height < theme.typography.small_size * 1.25 {
        return Err(ThemeError::Invalid(
            "waveform.metadata_height must leave room for metadata text".to_string(),
        ));
    }
    if theme.spacing.waveform_height
        < theme.waveform.title_height
            + theme.waveform.metadata_height
            + theme.waveform.scrollbar_height
            + 4.0
    {
        return Err(ThemeError::Invalid(
            "spacing.waveform_height must contain waveform title, scrollbar, and metadata rows"
                .to_string(),
        ));
    }
    const MIN_WINDOW_WIDTH: f32 = 960.0;
    // The two curation columns are additive to the original fixed-column budget. Keep the
    // legacy budget of 640px valid at the 960px minimum window by reserving the 192px name
    // column that remains after the default 56px rating and 72px collection columns.
    const MIN_MAIN_CONTENT_WIDTH: f32 = 192.0;
    let fixed_browser_width = theme.spacing.sidebar_width
        + theme.spacing.selected_marker_width
        + theme.spacing.rating_column_width
        + theme.spacing.collection_column_width
        + theme.spacing.extension_column_width
        + theme.spacing.size_column_width
        + theme.spacing.duration_column_width;
    if fixed_browser_width > MIN_WINDOW_WIDTH - MIN_MAIN_CONTENT_WIDTH {
        return Err(ThemeError::Invalid(
            "sidebar and fixed sample columns leave less than 192px for the sample names"
                .to_string(),
        ));
    }
    const MIN_WINDOW_HEIGHT: f32 = 560.0;
    let minimum_chrome_height = theme.spacing.toolbar_height
        + theme.spacing.waveform_height
        + theme.spacing.status_height
        + theme.spacing.control_height
        + theme.spacing.panel_padding * 2.0
        + theme.spacing.row_height * 4.0;
    if minimum_chrome_height > MIN_WINDOW_HEIGHT {
        return Err(ThemeError::Invalid(
            "toolbar, waveform, status, and theme controls must fit the 560px minimum window"
                .to_string(),
        ));
    }
    theme.name = theme.name.trim().to_string();
    theme.typography.font_family = theme.typography.font_family.trim().to_string();
    Ok(theme)
}

fn finite_range(name: &str, value: f32, minimum: f32, maximum: f32) -> Result<(), ThemeError> {
    if !value.is_finite() || value < minimum || value > maximum {
        return Err(ThemeError::Invalid(format!(
            "{name} must be finite and between {minimum} and {maximum}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn valid_json() -> String {
        default_theme_json().expect("default theme serializes")
    }

    #[test]
    fn default_matches_original_editorial_terminal_palette() {
        let theme = Theme::default();
        assert_eq!(theme.colors.background, Color::rgb(27, 30, 30));
        assert_eq!(theme.colors.accent, Color::rgb(233, 88, 67));
        assert_eq!(theme.colors.text_primary, Color::rgb(216, 215, 211));
        assert_eq!(theme.colors.text_muted, Color::rgb(153, 155, 154));
        assert_eq!(theme.colors.rating_keep, default_rating_keep());
        assert_eq!(theme.colors.rating_trash, default_rating_trash());
        assert_eq!(theme.colors.rating_neutral, default_rating_neutral());
        assert_eq!(theme.colors.collection_slots, default_collection_slots());
        assert_eq!(theme.spacing.sidebar_width, 260.0);
        assert_eq!(theme.spacing.waveform_height, 226.0);
        assert_eq!(theme.spacing.row_height, 22.0);
        assert_eq!(
            theme.spacing.rating_column_width,
            default_rating_column_width()
        );
        assert_eq!(
            theme.spacing.collection_column_width,
            default_collection_column_width()
        );
        assert_eq!(
            theme.spacing.last_played_column_width,
            default_last_played_column_width()
        );
    }

    #[test]
    fn json_round_trip_is_complete_and_canonical() {
        let json = valid_json();
        let theme = parse_theme_json(&json).expect("default JSON validates");
        assert_eq!(theme, Theme::default());
        assert!(json.contains("\"schema_version\": 1"));
        assert!(json.contains("\"accent\": \"#E95843FF\""));
    }

    #[test]
    fn legacy_theme_defaults_additive_waveform_and_curation_tokens() {
        let mut legacy: serde_json::Value =
            serde_json::from_str(&valid_json()).expect("default theme is JSON");
        let object = legacy.as_object_mut().expect("theme object");
        let colors = object
            .get_mut("colors")
            .and_then(serde_json::Value::as_object_mut)
            .expect("colors object");
        for field in [
            "rating_keep",
            "rating_trash",
            "rating_neutral",
            "collection_slots",
            "waveform_playhead",
            "waveform_edit_selection",
            "waveform_similar_fill",
            "waveform_similar_rail",
            "waveform_similar_hover_fill",
            "waveform_similar_hover_rail",
        ] {
            colors.remove(field);
        }
        let waveform = object
            .get_mut("waveform")
            .and_then(serde_json::Value::as_object_mut)
            .expect("waveform object");
        waveform.remove("scrollbar_height");
        let spacing = object
            .get_mut("spacing")
            .and_then(serde_json::Value::as_object_mut)
            .expect("spacing object");
        spacing.remove("rating_column_width");
        spacing.remove("collection_column_width");
        spacing.remove("last_played_column_width");
        spacing.remove("playback_type_column_width");
        let legacy = serde_json::to_string(&legacy).expect("legacy theme serializes");

        let theme = parse_theme_json(&legacy).expect("legacy theme validates");

        assert_eq!(theme.colors.rating_keep, default_rating_keep());
        assert_eq!(theme.colors.rating_trash, default_rating_trash());
        assert_eq!(theme.colors.rating_neutral, default_rating_neutral());
        assert_eq!(theme.colors.collection_slots, default_collection_slots());
        assert_eq!(theme.colors.waveform_playhead, default_waveform_playhead());
        assert_eq!(
            theme.colors.waveform_edit_selection,
            default_waveform_edit_selection()
        );
        assert_eq!(
            theme.colors.waveform_similar_fill,
            default_waveform_similar_fill()
        );
        assert_eq!(
            theme.colors.waveform_similar_rail,
            default_waveform_similar_rail()
        );
        assert_eq!(
            theme.colors.waveform_similar_hover_fill,
            default_waveform_similar_hover_fill()
        );
        assert_eq!(
            theme.colors.waveform_similar_hover_rail,
            default_waveform_similar_hover_rail()
        );
        assert_eq!(theme.waveform.scrollbar_height, 6.0);
        assert_eq!(
            theme.spacing.rating_column_width,
            default_rating_column_width()
        );
        assert_eq!(
            theme.spacing.collection_column_width,
            default_collection_column_width()
        );
        assert_eq!(
            theme.spacing.last_played_column_width,
            default_last_played_column_width()
        );
    }

    #[test]
    fn legacy_fixed_column_budget_remains_valid_with_curation_defaults() {
        let mut legacy: serde_json::Value =
            serde_json::from_str(&valid_json()).expect("default theme is JSON");
        let object = legacy.as_object_mut().expect("theme object");
        let spacing = object
            .get_mut("spacing")
            .and_then(serde_json::Value::as_object_mut)
            .expect("spacing object");
        spacing.remove("rating_column_width");
        spacing.remove("collection_column_width");
        spacing.remove("last_played_column_width");
        spacing.remove("playback_type_column_width");
        // The pre-curation fixed columns total exactly 640px. The additive defaults therefore
        // leave the compatibility 192px name-column budget at the 960px minimum window.
        spacing.insert("sidebar_width".to_string(), serde_json::Value::from(398.0));
        let legacy = serde_json::to_string(&legacy).expect("legacy theme serializes");

        let theme = parse_theme_json(&legacy).expect("old fixed-column budget remains valid");

        assert_eq!(theme.spacing.rating_column_width, 56.0);
        assert_eq!(theme.spacing.collection_column_width, 72.0);
        assert_eq!(theme.spacing.last_played_column_width, 112.0);
        assert_eq!(theme.spacing.playback_type_column_width, 76.0);
    }

    #[test]
    fn playback_type_width_round_trips_and_rejects_invalid_dimensions() {
        for width in [32.0, 76.0, 240.0] {
            let mut theme = Theme::default();
            theme.spacing.playback_type_column_width = width;
            let json = serde_json::to_string(&theme).expect("theme serializes");
            let parsed = parse_theme_json(&json).expect("bounded width validates");
            assert_eq!(parsed.spacing.playback_type_column_width, width);
        }
        for width in [31.0, 241.0, f32::NAN, f32::INFINITY] {
            let mut theme = Theme::default();
            theme.spacing.playback_type_column_width = width;
            assert!(validate_theme(theme).is_err());
        }
    }

    #[test]
    fn explicit_rating_and_collection_tokens_survive_theme_loading() {
        let mut custom: serde_json::Value =
            serde_json::from_str(&valid_json()).expect("default theme is JSON");
        let colors = custom
            .get_mut("colors")
            .and_then(serde_json::Value::as_object_mut)
            .expect("colors object");
        colors.insert(
            "rating_keep".to_string(),
            serde_json::Value::String("#01020304".to_string()),
        );
        colors.insert(
            "rating_trash".to_string(),
            serde_json::Value::String("#05060708".to_string()),
        );
        colors.insert(
            "rating_neutral".to_string(),
            serde_json::Value::String("#090A0B0C".to_string()),
        );
        colors.insert(
            "collection_slots".to_string(),
            serde_json::Value::Array(
                [
                    "#10111213",
                    "#14151617",
                    "#18191A1B",
                    "#1C1D1E1F",
                    "#20212223",
                    "#24252627",
                ]
                .into_iter()
                .map(|color| serde_json::Value::String(color.to_string()))
                .collect(),
            ),
        );

        let parsed =
            parse_theme_json(&serde_json::to_string(&custom).expect("custom theme serializes"))
                .expect("custom theme validates");

        assert_eq!(parsed.colors.rating_keep, Color::rgba(1, 2, 3, 4));
        assert_eq!(parsed.colors.rating_trash, Color::rgba(5, 6, 7, 8));
        assert_eq!(parsed.colors.rating_neutral, Color::rgba(9, 10, 11, 12));
        assert_eq!(
            parsed.colors.collection_slots,
            [
                Color::rgba(16, 17, 18, 19),
                Color::rgba(20, 21, 22, 23),
                Color::rgba(24, 25, 26, 27),
                Color::rgba(28, 29, 30, 31),
                Color::rgba(32, 33, 34, 35),
                Color::rgba(36, 37, 38, 39),
            ]
        );
    }

    #[test]
    fn checked_in_example_matches_the_embedded_default() {
        let example = parse_theme_json(include_str!("../../../themes/wavecrate.json"))
            .expect("canonical theme example validates");
        assert_eq!(example, Theme::default());
    }

    #[test]
    fn rejects_unknown_missing_and_unsupported_schema_fields() {
        let unknown = valid_json().replace("\"name\":", "\"unexpected\": 1,\n    \"name\":");
        assert!(parse_theme_json(&unknown).is_err());
        let missing = valid_json().replace("  \"name\": \"Wavecrate Editorial Terminal\",\n", "");
        assert!(parse_theme_json(&missing).is_err());
        let unsupported = valid_json().replace("\"schema_version\": 1", "\"schema_version\": 2");
        let error = parse_theme_json(&unsupported).expect_err("unsupported schema");
        assert!(error.to_string().contains("unsupported"));
    }

    #[test]
    fn legacy_theme_without_motion_uses_motion_defaults() {
        let legacy = valid_json()
            .replace(
                "  \"motion\": {\n    \"enabled\": true,\n    \"keyboard_hold_ms\": 800,\n    \"focus_fade_ms\": 250,\n    \"overflow_fade_ms\": 260,\n    \"worker_pulse_hz\": 1.15,\n    \"source_sweep_hz\": 0.45\n  },\n",
                "",
            );
        let parsed = parse_theme_json(&legacy).expect("legacy theme remains accepted");
        assert_eq!(parsed.motion, Motion::default());
    }

    #[test]
    fn rejects_unknown_motion_fields_and_invalid_motion_values() {
        let unknown = valid_json().replace(
            "    \"enabled\": true,",
            "    \"unexpected\": true,\n    \"enabled\": true,",
        );
        assert!(parse_theme_json(&unknown).is_err());

        let invalid_duration =
            valid_json().replace("\"overflow_fade_ms\": 260", "\"overflow_fade_ms\": 0");
        assert!(parse_theme_json(&invalid_duration).is_err());

        let invalid_frequency =
            valid_json().replace("\"source_sweep_hz\": 0.45", "\"source_sweep_hz\": 0.0");
        assert!(parse_theme_json(&invalid_frequency).is_err());
    }

    #[test]
    fn rejects_non_strict_colors_and_invalid_dimensions() {
        let short_color = valid_json().replace("#1B1E1EFF", "#12345");
        assert!(parse_theme_json(&short_color).is_err());
        let bad_color = valid_json().replace("#E95843FF", "#E9584GFF");
        assert!(parse_theme_json(&bad_color).is_err());
        let negative = valid_json().replace("\"row_height\": 22.0", "\"row_height\": -1.0");
        assert!(parse_theme_json(&negative).is_err());
        let too_small = valid_json().replace("\"row_height\": 22.0", "\"row_height\": 10.0");
        assert!(parse_theme_json(&too_small).is_err());
        let overflowing = valid_json().replace("\"row_height\": 22.0", "\"row_height\": 1e100");
        assert!(parse_theme_json(&overflowing).is_err());
    }

    #[test]
    fn rejects_fixed_layout_columns_that_cannot_fit_the_minimum_window() {
        let too_wide = valid_json()
            .replace("\"sidebar_width\": 260.0", "\"sidebar_width\": 640.0")
            .replace(
                "\"extension_column_width\": 64.0",
                "\"extension_column_width\": 240.0",
            )
            .replace(
                "\"size_column_width\": 88.0",
                "\"size_column_width\": 240.0",
            )
            .replace(
                "\"duration_column_width\": 88.0",
                "\"duration_column_width\": 240.0",
            );
        let error = parse_theme_json(&too_wide).expect_err("fixed columns exceed minimum width");
        assert!(error.to_string().contains("192px"));

        let too_tall =
            valid_json().replace("\"waveform_height\": 226.0", "\"waveform_height\": 480.0");
        let error = parse_theme_json(&too_tall).expect_err("chrome exceeds minimum height");
        assert!(error.to_string().contains("560px"));
    }

    #[test]
    fn rejects_waveform_scrollbar_heights_outside_the_bounded_range() {
        let too_small =
            valid_json().replace("\"scrollbar_height\": 6.0", "\"scrollbar_height\": 1.0");
        assert!(parse_theme_json(&too_small).is_err());
        let too_large =
            valid_json().replace("\"scrollbar_height\": 6.0", "\"scrollbar_height\": 13.0");
        assert!(parse_theme_json(&too_large).is_err());
    }

    #[test]
    fn rejects_invalid_names_and_nonregular_paths() {
        let empty_name = valid_json().replace(
            "\"name\": \"Wavecrate Editorial Terminal\"",
            "\"name\": \"   \"",
        );
        assert!(parse_theme_json(&empty_name).is_err());
        let directory = tempfile::tempdir().expect("temp directory");
        let error = load_theme_file(directory.path()).expect_err("directory is not a theme file");
        assert!(error.to_string().contains("regular file"));
    }

    #[test]
    fn bounded_file_loading_rejects_oversized_content() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("oversized.json");
        fs::write(&path, vec![b' '; MAX_THEME_FILE_BYTES + 1]).expect("write oversized file");
        let error = load_theme_file(&path).expect_err("oversized theme");
        assert!(matches!(error, ThemeError::FileTooLarge { .. }));
    }
}
