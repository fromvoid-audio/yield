#![deny(missing_docs)]
#![deny(warnings)]

//! GPUI integration for the frontend's active design theme.

use std::sync::Arc;

use gpui::{App, Global, Hsla, rgba};
use yield_design::{Color, Theme};
use yield_session::ENLARGED_TEXT_SCALE_PERCENT;

/// The active theme stored in GPUI application globals.
#[derive(Clone, Debug)]
pub(crate) struct ActiveTheme(pub(crate) Arc<Theme>);

impl Default for ActiveTheme {
    fn default() -> Self {
        Self(Arc::new(Theme::default()))
    }
}

impl Global for ActiveTheme {}

/// Return the active design tokens for a GPUI render pass.
pub(crate) fn active(cx: &App) -> &Theme {
    &cx.global::<ActiveTheme>().0
}

/// Derive the displayed theme from an authored theme and the in-app text setting.
///
/// The persisted theme stays at its authored sizes. Enlarging the text also enlarges the
/// primary row and control heights so glyphs are not clipped by their default containers.
pub(crate) fn with_text_scale(base: &Theme, percent: u16) -> Theme {
    let factor = if percent == ENLARGED_TEXT_SCALE_PERCENT {
        2.0
    } else {
        1.0
    };
    let mut theme = base.clone();
    theme.typography.body_size *= factor;
    theme.typography.small_size *= factor;
    theme.typography.title_size *= factor;
    theme.typography.waveform_title_size *= factor;
    theme.typography.line_height *= factor;
    theme.spacing.toolbar_height *= factor;
    theme.spacing.status_height *= factor;
    theme.spacing.row_height *= factor;
    theme.spacing.control_height *= factor;
    theme.waveform.title_height *= factor;
    theme.waveform.metadata_height *= factor;
    theme
}

/// Convert a design color to GPUI's renderer color type.
pub(crate) fn color(value: Color) -> Hsla {
    rgba(value.to_rgba32()).into()
}

/// A fully transparent color used by marker and overlay elements.
pub(crate) fn transparent() -> Hsla {
    color(Color::rgba(0, 0, 0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enlarged_text_keeps_authored_theme_and_expands_primary_containers() {
        let base = Theme::default();
        let enlarged = with_text_scale(&base, ENLARGED_TEXT_SCALE_PERCENT);
        assert_eq!(base, Theme::default());
        assert_eq!(
            enlarged.typography.body_size,
            base.typography.body_size * 2.0
        );
        assert_eq!(
            enlarged.typography.small_size,
            base.typography.small_size * 2.0
        );
        assert_eq!(
            enlarged.typography.title_size,
            base.typography.title_size * 2.0
        );
        assert_eq!(enlarged.spacing.row_height, base.spacing.row_height * 2.0);
        assert_eq!(
            enlarged.spacing.control_height,
            base.spacing.control_height * 2.0
        );
        assert_eq!(with_text_scale(&base, 100), base);
    }
}
