#![deny(missing_docs)]
#![deny(warnings)]

//! Metronome state helpers and the compact toolbar glyph.

use std::path::Path;

use gpui::{
    AnyView, App, Bounds, Context, Hsla, PathBuilder, Pixels, Render, Window, canvas, div, point,
    prelude::*, px,
};
use reson::PlaybackMetronomeConfig;

use super::Browser;
use super::waveform::WaveformPlayRegion;

const METRONOME_ICON_SIZE: f32 = 16.0;

/// The toolbar description for the metronome control.
pub(super) const TOOLTIP: &str = "Play a metronome from the beat guide divisions.";

/// A source anchored metronome cycle captured with one admitted playback request.
///
/// Keeping this metadata with the active browser playback prevents a later waveform refresh or
/// focus change from changing the phase of a live retarget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PlaybackMetronomeGrid {
    /// Number of source frames in the repeating cycle.
    pub(super) frames: u64,
    /// Absolute source frame at which the cycle begins.
    pub(super) origin_frame: u64,
}

/// Build the metronome configuration for one desired beat count and captured source grid.
pub(super) fn config_for_count(
    beat_count: Option<u16>,
    grid: Option<PlaybackMetronomeGrid>,
) -> Option<PlaybackMetronomeConfig> {
    beat_count.map(|beat_count| {
        let config = PlaybackMetronomeConfig::new(beat_count);
        grid.map_or(config, |grid| {
            config.with_source_cycle(grid.frames, grid.origin_frame)
        })
    })
}

/// Capture a source anchored grid for an admitted span when the loaded waveform belongs to it.
///
/// The selected play region is the grid when it contains the accepted span. Otherwise the
/// accepted span itself supplies the cycle. Missing or mismatched waveform metadata deliberately
/// returns `None`; Reson then derives its bounded fallback cycle from the playback span.
pub(super) fn source_grid_for_span(
    browser: &Browser,
    path: &Path,
    accepted_span: WaveformPlayRegion,
) -> Option<PlaybackMetronomeGrid> {
    let preview = browser.waveform.as_ref()?;
    let context = preview
        .detail_context
        .as_ref()
        .filter(|context| context.path == path)?;
    source_grid_from_metadata(
        &context.path,
        path,
        accepted_span,
        browser
            .waveform_play_region
            .filter(|selection| selection.is_valid())
            .as_ref(),
        context.total_frames,
    )
}

fn source_grid_from_metadata(
    metadata_path: &Path,
    requested_path: &Path,
    accepted_span: WaveformPlayRegion,
    selection: Option<&WaveformPlayRegion>,
    total_frames: usize,
) -> Option<PlaybackMetronomeGrid> {
    if metadata_path != requested_path {
        return None;
    }
    let total_frames = u64::try_from(total_frames)
        .ok()
        .filter(|frames| *frames > 0)?;
    let grid_span = selection
        .copied()
        .filter(|selection| selection_contains_span(*selection, accepted_span))
        .unwrap_or(accepted_span);
    let origin_frame = ratio_to_frame(grid_span.start, total_frames);
    let end_frame = ratio_to_frame(grid_span.end, total_frames)
        .max(origin_frame.saturating_add(1))
        .min(total_frames);
    let frames = end_frame.saturating_sub(origin_frame).max(1);
    Some(PlaybackMetronomeGrid {
        frames,
        origin_frame,
    })
}

fn selection_contains_span(selection: WaveformPlayRegion, span: WaveformPlayRegion) -> bool {
    const EPSILON: f64 = 0.000_1;
    span.start + EPSILON >= selection.start && span.end <= selection.end + EPSILON
}

fn ratio_to_frame(ratio: f64, total_frames: u64) -> u64 {
    let ratio = if ratio.is_finite() { ratio } else { 0.0 };
    ((ratio.clamp(0.0, 1.0) * total_frames.max(1) as f64).round() as u64).min(total_frames.max(1))
}

fn icon_point(bounds: Bounds<Pixels>, x: f32, y: f32) -> gpui::Point<Pixels> {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let scale = (width / METRONOME_ICON_SIZE)
        .min(height / METRONOME_ICON_SIZE)
        .max(0.0);
    let left = f32::from(bounds.origin.x) + (width - METRONOME_ICON_SIZE * scale) / 2.0;
    let top = f32::from(bounds.origin.y) + (height - METRONOME_ICON_SIZE * scale) / 2.0;
    point(px(left + x * scale), px(top + y * scale))
}

/// Render the fixed 16×16 metronome glyph used by the compact toolbar control.
pub(super) fn render_icon(icon_color: Hsla) -> impl IntoElement {
    canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            if !width.is_finite() || width <= 0.0 || !height.is_finite() || height <= 0.0 {
                return;
            }
            for leading in [true, false] {
                let mut glyph = PathBuilder::fill();
                if leading {
                    glyph.move_to(icon_point(bounds, 3.0, 3.0));
                    glyph.line_to(icon_point(bounds, 3.5, 3.0));
                    glyph.cubic_bezier_to(
                        icon_point(bounds, 5.2, 3.0),
                        icon_point(bounds, 6.5, 5.2),
                        icon_point(bounds, 6.5, 8.0),
                    );
                    glyph.cubic_bezier_to(
                        icon_point(bounds, 6.5, 10.8),
                        icon_point(bounds, 5.2, 13.0),
                        icon_point(bounds, 3.5, 13.0),
                    );
                    glyph.line_to(icon_point(bounds, 3.0, 13.0));
                } else {
                    glyph.move_to(icon_point(bounds, 13.0, 3.0));
                    glyph.line_to(icon_point(bounds, 12.5, 3.0));
                    glyph.cubic_bezier_to(
                        icon_point(bounds, 10.8, 3.0),
                        icon_point(bounds, 9.5, 5.2),
                        icon_point(bounds, 9.5, 8.0),
                    );
                    glyph.cubic_bezier_to(
                        icon_point(bounds, 9.5, 10.8),
                        icon_point(bounds, 10.8, 13.0),
                        icon_point(bounds, 12.5, 13.0),
                    );
                    glyph.line_to(icon_point(bounds, 13.0, 13.0));
                }
                glyph.close();
                if let Ok(path) = glyph.build() {
                    window.paint_path(path, icon_color);
                }
            }
        },
    )
    .w(px(METRONOME_ICON_SIZE))
    .h(px(METRONOME_ICON_SIZE))
}

struct MetronomeTooltip;

impl Render for MetronomeTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::active(cx);
        div()
            .px(px(theme.spacing.control_gap))
            .py(px(theme.spacing.control_gap))
            .bg(crate::theme::color(theme.colors.surface_raised))
            .border(px(theme.spacing.border_width))
            .border_color(crate::theme::color(theme.colors.border_emphasis))
            .text_size(px(theme.typography.small_size))
            .text_color(crate::theme::color(theme.colors.text_primary))
            .child(TOOLTIP)
    }
}

/// Build the metronome tooltip for the compact toolbar control.
pub(super) fn tooltip(_: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| MetronomeTooltip).into()
}

#[cfg(test)]
mod tests {
    use super::super::waveform::WaveformPlayRegion;
    use super::{
        PlaybackMetronomeGrid, config_for_count, selection_contains_span, source_grid_from_metadata,
    };
    use std::path::Path;

    #[test]
    fn config_can_disable_or_preserve_a_source_grid() {
        assert_eq!(config_for_count(None, None), None);
        assert_eq!(
            config_for_count(Some(4), None),
            Some(reson::PlaybackMetronomeConfig::new(4))
        );
        assert_eq!(
            config_for_count(
                Some(4),
                Some(PlaybackMetronomeGrid {
                    frames: 24_000,
                    origin_frame: 12_000,
                }),
            ),
            Some(reson::PlaybackMetronomeConfig::new(4).with_source_cycle(24_000, 12_000),)
        );
    }

    #[test]
    fn selected_grid_must_contain_the_accepted_span() {
        let selection = WaveformPlayRegion::new(0.25, 0.75).expect("valid selection");
        let contained = WaveformPlayRegion::new(0.375, 0.625).expect("valid span");
        let outside = WaveformPlayRegion::new(0.2, 0.8).expect("valid span");
        assert!(selection_contains_span(selection, contained));
        assert!(!selection_contains_span(selection, outside));
    }

    #[test]
    fn source_grid_uses_selection_only_when_it_contains_the_accepted_span() {
        let selection = WaveformPlayRegion::new(0.25, 0.75).expect("valid selection");
        let contained = WaveformPlayRegion::new(0.375, 0.625).expect("valid span");
        let grid = source_grid_from_metadata(
            Path::new("sample.wav"),
            Path::new("sample.wav"),
            contained,
            Some(&selection),
            96_000,
        )
        .expect("source grid");
        assert_eq!(
            grid,
            PlaybackMetronomeGrid {
                frames: 48_000,
                origin_frame: 24_000,
            }
        );

        let outside = WaveformPlayRegion::new(0.2, 0.8).expect("valid span");
        let fallback = source_grid_from_metadata(
            Path::new("sample.wav"),
            Path::new("sample.wav"),
            outside,
            Some(&selection),
            96_000,
        )
        .expect("source grid");
        assert_eq!(
            fallback,
            PlaybackMetronomeGrid {
                frames: 57_600,
                origin_frame: 19_200,
            }
        );
    }

    #[test]
    fn source_grid_requires_exact_positive_waveform_metadata() {
        let span = WaveformPlayRegion::new(0.0, 1.0).expect("valid span");
        assert_eq!(
            source_grid_from_metadata(
                Path::new("sample.wav"),
                Path::new("sample.wav"),
                span,
                None,
                0,
            ),
            None
        );
        assert_eq!(
            source_grid_from_metadata(
                Path::new("other.wav"),
                Path::new("sample.wav"),
                span,
                None,
                48_000,
            ),
            None
        );
    }
}
