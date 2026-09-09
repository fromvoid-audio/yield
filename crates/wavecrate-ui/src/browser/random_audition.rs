#![deny(missing_docs)]
#![deny(warnings)]

//! Random audition selection, lifecycle fencing, and toolbar presentation.

use std::path::{Path, PathBuf};

use gpui::{
    AnyView, App, Bounds, Context, Hsla, PathBuilder, Pixels, Render, Window, canvas, div, point,
    prelude::*, px,
};
use wavecrate_core::{
    AudioFile, FolderScope, RandomAuditionUnits, WaveformViewport, folder_contains_relative_path,
    random_audition_span_for_units,
};

use super::Browser;
use super::waveform::WaveformPlayRegion;

const RANDOM_ICON_SIZE: f32 = 16.0;

/// The toolbar description for the random audition control.
pub(super) const TOOLTIP: &str = "Play random section\nClick: play a random section now.\nShift-click: pick a random listed sample first.\nCommand-click: make Space use random sections.";

/// A random audition waiting for its selected waveform to finish loading.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PendingRandomAudition {
    /// Waveform generation that owns the selected source context.
    pub(super) generation: u64,
    /// Exact source path that must be loaded before the request can play.
    pub(super) path: PathBuf,
    /// Deterministic normalized units used to calculate the audition span.
    pub(super) units: RandomAuditionUnits,
}

fn is_wav(entry: &AudioFile) -> bool {
    entry
        .path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
}

fn folder_candidate_indices(
    entries: &[AudioFile],
    selected_folder: &Path,
    folder_scope: FolderScope,
) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            is_wav(entry)
                && folder_contains_relative_path(
                    selected_folder,
                    &entry.relative_path,
                    folder_scope,
                )
        })
        .map(|(index, _)| index)
        .collect()
}

fn current_folder_candidates(browser: &Browser) -> Vec<usize> {
    let candidates = folder_candidate_indices(
        &browser.entries,
        &browser.selected_folder,
        browser.folder_scope,
    );
    if candidates.is_empty() {
        browser
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| is_wav(entry))
            .map(|(index, _)| index)
            .collect()
    } else {
        candidates
    }
}

fn visible_candidates(browser: &Browser) -> Vec<usize> {
    browser
        .view_model
        .rows()
        .iter()
        .filter_map(|row| {
            browser
                .entries
                .get(row.entry_index)
                .filter(|entry| is_wav(entry))
                .map(|_| row.entry_index)
        })
        .collect()
}

fn selected_candidate(browser: &Browser) -> Option<usize> {
    browser
        .selected
        .filter(|index| browser.entries.get(*index).is_some_and(is_wav))
}

fn loaded_selected_candidate(browser: &Browser) -> Option<usize> {
    selected_candidate(browser).filter(|index| {
        browser
            .entries
            .get(*index)
            .is_some_and(|entry| waveform_matches_path(browser, &entry.path))
    })
}

fn choose_candidate(
    candidates: &[usize],
    random_unit: f32,
    entries: &[AudioFile],
    avoid_path: Option<&Path>,
) -> Option<usize> {
    let usable = candidates
        .iter()
        .copied()
        .filter(|index| entries.get(*index).is_some_and(is_wav))
        .collect::<Vec<_>>();
    if usable.is_empty() {
        return None;
    }

    let pool = if usable.len() > 1 {
        if let Some(avoid_path) = avoid_path {
            let without_current = usable
                .iter()
                .copied()
                .filter(|index| {
                    entries
                        .get(*index)
                        .is_none_or(|entry| entry.path.as_path() != avoid_path)
                })
                .collect::<Vec<_>>();
            if without_current.is_empty() {
                usable
            } else {
                without_current
            }
        } else {
            usable
        }
    } else {
        usable
    };

    let random_unit = if random_unit.is_finite() {
        random_unit.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let position = ((random_unit * pool.len() as f32).floor() as usize).min(pool.len() - 1);
    pool.get(position).copied()
}

fn random_units() -> RandomAuditionUnits {
    RandomAuditionUnits::new(rand::random::<f32>(), rand::random::<f32>())
}

fn waveform_matches_path(browser: &Browser, path: &Path) -> bool {
    browser
        .waveform
        .as_ref()
        .and_then(|preview| preview.detail_context.as_ref())
        .is_some_and(|context| context.path.as_path() == path)
}

fn pending_matches(browser: &Browser, pending: &PendingRandomAudition) -> bool {
    pending.generation == browser.waveform_generation
        && browser
            .selected
            .and_then(|index| browser.entries.get(index))
            .is_some_and(|entry| entry.path.as_path() == pending.path.as_path())
        && waveform_matches_path(browser, &pending.path)
}

fn pending_selection_matches(browser: &Browser, pending: &PendingRandomAudition) -> bool {
    pending.generation == browser.waveform_generation
        && browser
            .selected
            .and_then(|index| browser.entries.get(index))
            .is_some_and(|entry| entry.path.as_path() == pending.path.as_path())
}

fn viewport_containing_selection(
    viewport: WaveformViewport,
    total_frames: usize,
    region: WaveformPlayRegion,
) -> WaveformViewport {
    let current = viewport.pan_by_visible_fraction(total_frames, 0.0);
    let region_width = region.end - region.start;
    if region.start >= current.start && region.end <= current.end {
        return current;
    }
    if region_width > current.width() {
        return current.zoom_to_region(total_frames, region.start, region.end);
    }
    let center_delta = (region.start + region.end) * 0.5 - current.center();
    current.pan_by_visible_fraction(total_frames, center_delta / current.width())
}

impl Browser {
    /// Request a random audition, optionally choosing the source from visible rows first.
    pub(super) fn request_random_audition(&mut self, listed: bool, cx: &mut Context<Self>) {
        self.random_audition_pending = None;
        let units = random_units();
        let selected_path = self
            .selected
            .and_then(|index| self.entries.get(index))
            .map(|entry| entry.path.as_path());
        let candidate = if listed {
            let visible = visible_candidates(self);
            choose_candidate(
                &visible,
                rand::random::<f32>(),
                &self.entries,
                selected_path,
            )
            .or_else(|| loaded_selected_candidate(self))
        } else {
            selected_candidate(self).or_else(|| {
                choose_candidate(
                    &current_folder_candidates(self),
                    rand::random::<f32>(),
                    &self.entries,
                    None,
                )
            })
        };
        let Some(index) = candidate else {
            self.playback_status = "No WAV samples available for random audition".to_string();
            cx.notify();
            return;
        };
        let Some(path) = self.entries.get(index).map(|entry| entry.path.clone()) else {
            self.playback_status = "Random audition sample is unavailable".to_string();
            cx.notify();
            return;
        };
        let was_selected = self.selected == Some(index);
        if !was_selected {
            self.select_entry(index, None, super::SelectionOrigin::Passive, cx);
        }

        // Selection cancellation owns the old request. Register this request after selection so
        // the load it just started cannot cancel the new random intent.
        self.random_audition_pending = Some(PendingRandomAudition {
            generation: self.waveform_generation,
            path,
            units,
        });
        if waveform_matches_path(
            self,
            self.random_audition_pending
                .as_ref()
                .expect("random request was just registered")
                .path
                .as_path(),
        ) {
            self.finish_pending_random_audition(cx);
        } else if self.waveform_pending || self.waveform_task.is_some() {
            self.playback_status = "Loading random audition preview…".to_string();
            cx.notify();
        } else if was_selected && !self.waveform_pending && self.waveform_task.is_none() {
            self.random_audition_pending = None;
            self.playback_status = "Random audition preview unavailable".to_string();
            cx.notify();
        }
    }

    /// Finish a random audition after its fenced waveform preview is available.
    pub(super) fn finish_pending_random_audition(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.random_audition_pending.take() else {
            return;
        };
        if !pending_selection_matches(self, &pending) {
            return;
        }
        if self.waveform.is_none() {
            self.playback_status = "Random audition preview unavailable".to_string();
            cx.notify();
            return;
        }
        if !pending_matches(self, &pending) {
            self.playback_status = "Random audition preview unavailable".to_string();
            cx.notify();
            return;
        }
        let Some((duration_seconds, total_frames)) = self
            .waveform
            .as_ref()
            .map(|preview| (preview.duration_seconds, preview.total_frames))
        else {
            self.playback_status = "Random audition preview unavailable".to_string();
            cx.notify();
            return;
        };
        let (start, end) = random_audition_span_for_units(duration_seconds, pending.units);
        let Some(region) = WaveformPlayRegion::new(f64::from(start), f64::from(end)) else {
            self.playback_status = "Random audition span unavailable".to_string();
            cx.notify();
            return;
        };
        self.waveform_region_drag = None;
        self.waveform_pan_drag = None;
        self.waveform_scrollbar_drag = None;
        self.waveform_fade_hover = None;
        super::zero_crossing::cancel_for_interruption(self);
        self.set_waveform_play_region(Some(region));
        super::similar_sections::invalidate_if_stale(self);
        self.waveform_viewport =
            viewport_containing_selection(self.waveform_viewport, total_frames, region);
        self.playback_status = "Starting random audition…".to_string();
        let intent = super::transport::PlaybackIntent {
            start: region.start,
            region: Some(region),
            looped: self.playback_policy.enabled(),
            volume: super::volume::quantize_volume(self.volume),
            path: pending.path,
            origin: super::transport::PlaybackIntentOrigin::Explicit,
            resume_desired: None,
        };
        self.request_playback_intent(intent, cx);
    }

    /// Toggle whether the ordinary Space shortcut requests a random section.
    pub(super) fn toggle_sticky_random_audition(&mut self, cx: &mut Context<Self>) {
        self.random_audition_sticky = !self.random_audition_sticky;
        self.playback_status = if self.random_audition_sticky {
            "Random audition enabled for Space".to_string()
        } else {
            "Random audition disabled for Space".to_string()
        };
        cx.notify();
    }
}

fn icon_point(bounds: Bounds<Pixels>, x: f32, y: f32) -> gpui::Point<Pixels> {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let scale = (width / RANDOM_ICON_SIZE)
        .min(height / RANDOM_ICON_SIZE)
        .max(0.0);
    let left = f32::from(bounds.origin.x) + (width - RANDOM_ICON_SIZE * scale) / 2.0;
    let top = f32::from(bounds.origin.y) + (height - RANDOM_ICON_SIZE * scale) / 2.0;
    point(px(left + x * scale), px(top + y * scale))
}

/// Render the fixed random audition glyph used by the compact toolbar control.
pub(super) fn render_icon(icon_color: Hsla) -> impl IntoElement {
    canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            if !width.is_finite() || width <= 0.0 || !height.is_finite() || height <= 0.0 {
                return;
            }
            let mut glyph = PathBuilder::fill();
            glyph.move_to(icon_point(bounds, 2.0, 4.0));
            glyph.line_to(icon_point(bounds, 4.1, 4.0));
            glyph.cubic_bezier_to(
                icon_point(bounds, 5.9, 4.0),
                icon_point(bounds, 7.0, 4.8),
                icon_point(bounds, 8.2, 6.5),
            );
            glyph.line_to(icon_point(bounds, 9.0, 7.6));
            glyph.cubic_bezier_to(
                icon_point(bounds, 9.8, 8.7),
                icon_point(bounds, 10.4, 9.0),
                icon_point(bounds, 11.6, 9.0),
            );
            glyph.line_to(icon_point(bounds, 12.0, 9.0));
            glyph.line_to(icon_point(bounds, 12.0, 7.0));
            glyph.line_to(icon_point(bounds, 15.0, 10.0));
            glyph.line_to(icon_point(bounds, 12.0, 13.0));
            glyph.line_to(icon_point(bounds, 12.0, 11.0));
            glyph.line_to(icon_point(bounds, 11.6, 11.0));
            glyph.cubic_bezier_to(
                icon_point(bounds, 9.7, 11.0),
                icon_point(bounds, 8.5, 10.3),
                icon_point(bounds, 7.4, 8.6),
            );
            glyph.line_to(icon_point(bounds, 6.6, 7.5));
            glyph.cubic_bezier_to(
                icon_point(bounds, 5.8, 6.3),
                icon_point(bounds, 5.2, 6.0),
                icon_point(bounds, 4.1, 6.0),
            );
            glyph.line_to(icon_point(bounds, 2.0, 6.0));
            glyph.close();

            glyph.move_to(icon_point(bounds, 11.6, 4.0));
            glyph.line_to(icon_point(bounds, 12.0, 4.0));
            glyph.line_to(icon_point(bounds, 12.0, 2.0));
            glyph.line_to(icon_point(bounds, 15.0, 5.0));
            glyph.line_to(icon_point(bounds, 12.0, 8.0));
            glyph.line_to(icon_point(bounds, 12.0, 6.0));
            glyph.line_to(icon_point(bounds, 11.6, 6.0));
            glyph.cubic_bezier_to(
                icon_point(bounds, 10.4, 6.0),
                icon_point(bounds, 9.8, 6.3),
                icon_point(bounds, 9.0, 7.4),
            );
            glyph.line_to(icon_point(bounds, 8.8, 7.7));
            glyph.line_to(icon_point(bounds, 7.9, 6.3));
            glyph.line_to(icon_point(bounds, 8.4, 5.6));
            glyph.cubic_bezier_to(
                icon_point(bounds, 9.3, 4.7),
                icon_point(bounds, 10.5, 4.0),
                icon_point(bounds, 11.6, 4.0),
            );
            glyph.close();

            glyph.move_to(icon_point(bounds, 2.0, 10.0));
            glyph.line_to(icon_point(bounds, 4.1, 10.0));
            glyph.cubic_bezier_to(
                icon_point(bounds, 5.2, 10.0),
                icon_point(bounds, 5.8, 9.7),
                icon_point(bounds, 6.6, 8.5),
            );
            glyph.line_to(icon_point(bounds, 7.5, 9.9));
            glyph.cubic_bezier_to(
                icon_point(bounds, 6.5, 11.3),
                icon_point(bounds, 5.5, 12.0),
                icon_point(bounds, 4.1, 12.0),
            );
            glyph.line_to(icon_point(bounds, 2.0, 12.0));
            glyph.close();

            if let Ok(path) = glyph.build() {
                window.paint_path(path, icon_color);
            }
        },
    )
    .w(px(RANDOM_ICON_SIZE))
    .h(px(RANDOM_ICON_SIZE))
}

struct RandomTooltip;

impl Render for RandomTooltip {
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

/// Build the random audition tooltip for the compact toolbar control.
pub(super) fn tooltip(_: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| RandomTooltip).into()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use gpui::TestAppContext;
    use wavecrate_core::{
        AudioFile, FolderScope, RandomAuditionUnits, WaveformViewport,
        random_audition_span_for_units,
    };
    use wavecrate_media::{WaveformDetailContext, WaveformPreview, load_waveform_preview};

    use super::super::waveform::WaveformPlayRegion;
    use super::{
        Browser, PendingRandomAudition, choose_candidate, folder_candidate_indices, is_wav,
        viewport_containing_selection,
    };

    fn file(path: &str, relative_path: &str) -> AudioFile {
        AudioFile {
            path: PathBuf::from(path),
            relative_path: PathBuf::from(relative_path),
            byte_len: 0,
            duration_seconds: Some(1.0),
            sample_rate: Some(48_000),
            playback_type: None,
            curation: None,
        }
    }

    #[test]
    fn random_candidate_uses_endpoints_and_avoids_current_when_possible() {
        let entries = vec![
            file("/source/a.wav", "a.wav"),
            file("/source/b.wav", "b.wav"),
            file("/source/c.wav", "c.wav"),
        ];
        let candidates = [0, 1, 2];
        assert_eq!(choose_candidate(&candidates, 0.0, &entries, None), Some(0));
        assert_eq!(choose_candidate(&candidates, 1.0, &entries, None), Some(2));
        assert_eq!(
            choose_candidate(
                &candidates,
                0.0,
                &entries,
                Some(std::path::Path::new("/source/a.wav")),
            ),
            Some(1)
        );
        assert_eq!(
            choose_candidate(
                &[0],
                1.0,
                &entries,
                Some(std::path::Path::new("/source/a.wav")),
            ),
            Some(0)
        );
    }

    #[test]
    fn current_folder_candidates_ignore_non_audio_and_preserve_scope() {
        let entries = vec![
            file("/source/root.wav", "root.wav"),
            file("/source/kick/hit.wav", "kick/hit.wav"),
            file("/source/kick/readme.txt", "kick/readme.txt"),
            file("/source/snare/hit.wav", "snare/hit.wav"),
        ];
        assert_eq!(
            folder_candidate_indices(
                &entries,
                std::path::Path::new("kick"),
                FolderScope::DirectOnly
            ),
            vec![1]
        );
        assert_eq!(
            folder_candidate_indices(
                &entries,
                std::path::Path::new("."),
                FolderScope::AllDescendants,
            ),
            vec![0, 1, 3]
        );
        assert!(is_wav(&entries[0]));
        assert!(!is_wav(&entries[2]));
    }

    #[test]
    fn viewport_keeps_a_selection_that_is_already_covered() {
        let viewport = WaveformViewport {
            start: 0.2,
            end: 0.8,
        };
        let region = WaveformPlayRegion::new(0.3, 0.7).expect("selection");
        assert_eq!(
            viewport_containing_selection(viewport, 48_000, region),
            viewport
        );
    }

    #[test]
    fn viewport_pans_to_center_an_offscreen_selection() {
        let viewport = WaveformViewport {
            start: 0.2,
            end: 0.4,
        };
        let region = WaveformPlayRegion::new(0.7, 0.8).expect("selection");
        let framed = viewport_containing_selection(viewport, 48_000, region);
        assert!(framed.start <= region.start);
        assert!(framed.end >= region.end);
        assert!((framed.center() - 0.75).abs() < 1.0e-9);
    }

    #[test]
    fn viewport_zooms_to_a_selection_wider_than_the_current_domain() {
        let viewport = WaveformViewport {
            start: 0.4,
            end: 0.6,
        };
        let region = WaveformPlayRegion::new(0.2, 0.8).expect("selection");
        let framed = viewport_containing_selection(viewport, 48_000, region);
        assert!(framed.start <= region.start);
        assert!(framed.end >= region.end);
        assert!((framed.width() - (region.end - region.start)).abs() < 1.0e-9);
    }

    fn detail_context_for(path: &Path) -> WaveformDetailContext {
        let directory = tempfile::tempdir().expect("temporary directory");
        let fixture = directory.path().join("context.wav");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&40_u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&48_000_u32.to_le_bytes());
        bytes.extend_from_slice(&96_000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&4_u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        fs::write(&fixture, bytes).expect("write WAV context fixture");
        let mut context = load_waveform_preview(&fixture)
            .expect("load WAV context fixture")
            .detail_context
            .expect("fixture detail context");
        context.path = path.to_path_buf();
        context
    }

    fn preview_for(path: &Path) -> WaveformPreview {
        WaveformPreview {
            peaks: vec![0.5; 8],
            bands: Vec::new(),
            duration_seconds: 10.0,
            sample_rate: 48_000,
            total_frames: 480_000,
            detail_context: Some(detail_context_for(path)),
        }
    }

    fn browser_with_selected_preview<'a>(
        cx: &'a mut TestAppContext,
        path: &Path,
    ) -> (gpui::Entity<Browser>, &'a mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _| {
            view.entries = vec![file(path.to_string_lossy().as_ref(), "sample.wav")];
            view.selected = Some(0);
            view.waveform_generation = 7;
            view.waveform = Some(preview_for(path));
        });
        (browser, cx)
    }

    #[gpui::test]
    fn pending_random_audition_rejects_stale_generation_and_path(cx: &mut TestAppContext) {
        let path = PathBuf::from("/tmp/random-fence.wav");
        let (browser, cx) = browser_with_selected_preview(cx, &path);
        browser.update(cx, |view, _| {
            view.random_audition_pending = Some(PendingRandomAudition {
                generation: view.waveform_generation - 1,
                path: path.clone(),
                units: RandomAuditionUnits::new(0.25, 0.5),
            });
        });
        browser.update(cx, |view, cx| view.finish_pending_random_audition(cx));
        browser.update(cx, |view, _| {
            assert!(view.random_audition_pending.is_none());
            assert!(view.playback_init_intent.is_none());
            view.random_audition_pending = Some(PendingRandomAudition {
                generation: view.waveform_generation,
                path: PathBuf::from("/tmp/other-random.wav"),
                units: RandomAuditionUnits::new(0.25, 0.5),
            });
        });
        browser.update(cx, |view, cx| view.finish_pending_random_audition(cx));
        browser.update(cx, |view, _| {
            assert!(view.random_audition_pending.is_none());
            assert!(view.playback_init_intent.is_none());
        });
    }

    #[gpui::test]
    fn matching_random_load_failure_clears_pending_and_reports_status(cx: &mut TestAppContext) {
        let path = PathBuf::from("/tmp/random-load-failure.wav");
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _| {
            view.entries = vec![file(path.to_string_lossy().as_ref(), "sample.wav")];
            view.selected = Some(0);
            view.waveform_generation = 4;
            view.waveform = None;
            view.random_audition_pending = Some(PendingRandomAudition {
                generation: 4,
                path: path.clone(),
                units: RandomAuditionUnits::new(0.0, 0.0),
            });
        });
        browser.update(cx, |view, cx| view.finish_pending_random_audition(cx));
        browser.update(cx, |view, _| {
            assert!(view.random_audition_pending.is_none());
            assert_eq!(view.playback_status, "Random audition preview unavailable");
            assert!(view.playback_init_intent.is_none());
        });
    }

    #[gpui::test]
    fn matching_random_load_starts_once_with_chosen_region(cx: &mut TestAppContext) {
        let path = PathBuf::from("/tmp/random-load-success.wav");
        let (browser, cx) = browser_with_selected_preview(cx, &path);
        let units = RandomAuditionUnits::new(0.5, 0.25);
        let expected = random_audition_span_for_units(10.0, units);
        let expected_region = super::super::waveform::WaveformPlayRegion::new(
            f64::from(expected.0),
            f64::from(expected.1),
        )
        .expect("random region");
        browser.update(cx, |view, _| {
            view.random_audition_pending = Some(PendingRandomAudition {
                generation: view.waveform_generation,
                path: path.clone(),
                units,
            });
        });
        browser.update(cx, |view, cx| view.finish_pending_random_audition(cx));
        browser.update(cx, |view, _| {
            assert!(view.random_audition_pending.is_none());
            assert_eq!(view.waveform_play_region, Some(expected_region));
            let intent = view
                .playback_init_intent
                .as_ref()
                .expect("random playback waits for preferences");
            assert_eq!(intent.path, path);
            assert_eq!(intent.region, Some(expected_region));
            assert_eq!(intent.start, expected_region.start);
        });
        browser.update(cx, |view, cx| view.finish_pending_random_audition(cx));
        browser.update(cx, |view, _| {
            assert!(view.random_audition_pending.is_none());
            assert_eq!(view.waveform_play_region, Some(expected_region));
        });
    }

    #[gpui::test]
    fn listed_random_uses_selected_only_when_its_waveform_is_loaded(cx: &mut TestAppContext) {
        let path = PathBuf::from("/tmp/random-listed-fallback.wav");
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _| {
            view.entries = vec![file(path.to_string_lossy().as_ref(), "sample.wav")];
            view.selected = Some(0);
            view.waveform_generation = 3;
        });
        browser.update(cx, |view, cx| view.request_random_audition(true, cx));
        browser.update(cx, |view, _| {
            assert!(view.random_audition_pending.is_none());
            assert_eq!(
                view.playback_status,
                "No WAV samples available for random audition"
            );
            view.waveform = Some(preview_for(&path));
        });
        browser.update(cx, |view, cx| view.request_random_audition(true, cx));
        browser.update(cx, |view, _| {
            assert!(view.random_audition_pending.is_none());
            assert_eq!(
                view.playback_init_intent
                    .as_ref()
                    .map(|intent| intent.path.as_path()),
                Some(path.as_path())
            );
        });
    }
}
