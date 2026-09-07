#![deny(missing_docs)]
#![deny(warnings)]

//! GPUI integration for the frontend's active design theme.

use std::sync::Arc;

use gpui::{App, Global, Hsla, rgba};
use wavecrate_design::{Color, Theme};

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

/// Convert a design color to GPUI's renderer color type.
pub(crate) fn color(value: Color) -> Hsla {
    rgba(value.to_rgba32()).into()
}

/// A fully transparent color used by marker and overlay elements.
pub(crate) fn transparent() -> Hsla {
    color(Color::rgba(0, 0, 0, 0))
}
