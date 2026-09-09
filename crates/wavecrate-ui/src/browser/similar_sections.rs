//! Bounded background similarity scans owned by one exact play selection.

use super::{Browser, waveform::WaveformPlayRegion};
use gpui::{Context, Task};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use wavecrate_media::{
    WavSimilarSectionsRequest, WaveformDetailRange, scan_wav_similar_sections_with_cancel,
};

#[derive(Clone, Debug, PartialEq)]
struct Intent {
    generation: u64,
    anchor: WaveformPlayRegion,
    request: WavSimilarSectionsRequest,
}

/// One active scan, at most one replacement, and revision-fenced display results.
#[derive(Default)]
pub(super) struct SimilarSectionsState {
    wanted: Option<Intent>,
    queued: Option<Intent>,
    cancellation: Option<Arc<AtomicBool>>,
    task: Option<Task<()>>,
    ranges: Vec<WaveformDetailRange>,
    hovered: Option<WaveformDetailRange>,
    flash: Option<(wavecrate_core::WaveformEditRegion, std::time::Instant)>,
}

impl SimilarSectionsState {
    /// Clear display ownership and request cancellation without abandoning the worker lane.
    pub(super) fn cancel(&mut self) {
        if let Some(cancellation) = &self.cancellation {
            cancellation.store(true, Ordering::Release);
        }
        self.wanted = None;
        self.queued = None;
        self.ranges.clear();
        self.hovered = None;
        self.flash = None;
    }

    /// Whether the user still owns an enabled scan or its results.
    pub(super) fn enabled(&self) -> bool {
        self.wanted.is_some()
    }

    /// Whether the current enabled scan is waiting for a result.
    pub(super) fn pending(&self) -> bool {
        self.enabled() && self.task.is_some()
    }
}

impl Drop for SimilarSectionsState {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn current_intent(browser: &Browser) -> Option<Intent> {
    let anchor = browser
        .waveform_play_region
        .filter(|region| region.is_valid())?;
    let context = browser.waveform.as_ref()?.detail_context.as_ref()?;
    let path = &browser.entries.get(browser.selected?)?.path;
    if &context.path != path {
        return None;
    }
    let frames = context.total_frames as f64;
    let range = WaveformDetailRange {
        start_frame: (anchor.start * frames).floor() as usize,
        end_frame: (anchor.end * frames).ceil().min(frames) as usize,
    };
    Some(Intent {
        generation: browser.waveform_generation,
        anchor,
        request: WavSimilarSectionsRequest::new(context.clone(), range).ok()?,
    })
}

/// Invalidate marks and background ownership when the source or play selection changes.
pub(super) fn invalidate_if_stale(browser: &mut Browser) {
    if browser
        .similar_sections
        .wanted
        .as_ref()
        .is_some_and(|wanted| current_intent(browser).as_ref() != Some(wanted))
    {
        browser.similar_sections.cancel();
    }
}

/// Toggle a scan using only the already loaded source context on the UI thread.
pub(super) fn toggle(browser: &mut Browser, cx: &mut Context<Browser>) {
    invalidate_if_stale(browser);
    if browser.similar_sections.enabled() {
        browser.similar_sections.cancel();
        browser.playback_status = String::from("Similar sections disabled");
    } else if let Some(intent) = current_intent(browser) {
        browser.similar_sections.wanted = Some(intent.clone());
        browser.similar_sections.ranges.clear();
        browser.playback_status = String::from("Finding similar sections…");
        if browser.similar_sections.task.is_some() {
            browser.similar_sections.queued = Some(intent);
        } else {
            launch(browser, intent, cx);
        }
    } else {
        browser.playback_status = String::from("Set a playmark selection first");
    }
    cx.notify();
}

fn launch(browser: &mut Browser, intent: Intent, cx: &mut Context<Browser>) {
    let cancellation = Arc::new(AtomicBool::new(false));
    browser.similar_sections.cancellation = Some(Arc::clone(&cancellation));
    browser.similar_sections.task = Some(cx.spawn(async move |this, cx| {
        let (intent, cancellation, result) = cx
            .background_executor()
            .spawn(async move {
                let result =
                    scan_wav_similar_sections_with_cancel(&intent.request, Some(&cancellation));
                (intent, cancellation, result)
            })
            .await;
        let _ = this.update(cx, |browser, cx| {
            browser.similar_sections.task = None;
            browser.similar_sections.cancellation = None;
            if !cancellation.load(Ordering::Acquire)
                && browser.similar_sections.wanted.as_ref() == Some(&intent)
                && current_intent(browser).as_ref() == Some(&intent)
            {
                match result {
                    Ok(ranges) => {
                        browser.playback_status = format!("{} similar sections", ranges.len());
                        browser.similar_sections.ranges = ranges;
                    }
                    Err(error) => {
                        browser.similar_sections.cancel();
                        browser.playback_status = error;
                    }
                }
            }
            invalidate_if_stale(browser);
            if let Some(next) = browser.similar_sections.queued.take()
                && browser.similar_sections.wanted.as_ref() == Some(&next)
                && current_intent(browser).as_ref() == Some(&next)
            {
                launch(browser, next, cx);
            }
            cx.notify();
        });
    }));
}

/// Return marks only while their exact source and play anchor still match.
pub(super) fn ranges(browser: &Browser) -> &[WaveformDetailRange] {
    if browser
        .similar_sections
        .wanted
        .as_ref()
        .is_some_and(|wanted| current_intent(browser).as_ref() == Some(wanted))
    {
        &browser.similar_sections.ranges
    } else {
        &[]
    }
}

/// Native eight-rectangle similarity glyph.
pub(super) fn render_icon(tint: gpui::Hsla) -> impl gpui::IntoElement {
    use gpui::{Bounds, canvas, fill, point, prelude::*, px, size};
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            for (x, y, w, h) in [
                (2., 3., 3.5, 3.),
                (10.5, 3., 3.5, 3.),
                (2., 10., 3.5, 3.),
                (10.5, 10., 3.5, 3.),
                (6.5, 4.1, 3., 1.2),
                (6.5, 11.1, 3., 1.2),
                (3.15, 7., 1.2, 2.),
                (11.65, 7., 1.2, 2.),
            ] {
                window.paint_quad(fill(
                    Bounds::new(bounds.origin + point(px(x), px(y)), size(px(w), px(h))),
                    tint,
                ));
            }
        },
    )
    .w(px(16.))
    .h(px(16.))
}

/// Build the selection scan toolbar tooltip.
pub(super) fn tooltip(_: &mut gpui::Window, cx: &mut gpui::App) -> gpui::AnyView {
    use gpui::AppContext;
    cx.new(|_| SimilarTooltip).into()
}

struct SimilarTooltip;
impl gpui::Render for SimilarTooltip {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        use gpui::{div, prelude::*, px};
        let theme = crate::theme::active(cx);
        div().px(px(theme.spacing.control_gap)).py(px(theme.spacing.control_gap))
            .bg(crate::theme::color(theme.colors.surface_raised))
            .text_color(crate::theme::color(theme.colors.text_primary))
            .text_size(px(theme.typography.small_size))
            .child("Mark sections similar to the playmark selection.\nSet a playmark first, then toggle this to scan the loaded sample.")
    }
}

fn range_at(browser: &Browser, ratio: f64) -> Option<WaveformDetailRange> {
    if !ratio.is_finite()
        || browser.waveform_region_drag.is_some()
        || browser.waveform_pan_drag.is_some()
        || browser.waveform_scrollbar_drag.is_some()
    {
        return None;
    }
    let frames = browser
        .waveform
        .as_ref()?
        .detail_context
        .as_ref()?
        .total_frames as f64;
    ranges(browser).iter().rev().copied().find(|range| {
        ratio >= range.start_frame as f64 / frames && ratio <= range.end_frame as f64 / frames
    })
}

/// Update the match hover only on pointer changes, without a perpetual animation.
pub(super) fn hover(browser: &mut Browser, ratio: Option<f64>) -> bool {
    let next = ratio.and_then(|ratio| range_at(browser, ratio));
    let changed = browser.similar_sections.hovered != next;
    browser.similar_sections.hovered = next;
    changed
}

/// Select a matching edit region after the waveform's existing handles have declined the press.
pub(super) fn select_at(
    browser: &mut Browser,
    ratio: f64,
    window: &mut gpui::Window,
    cx: &mut Context<Browser>,
) -> bool {
    let Some(range) = range_at(browser, ratio) else {
        return false;
    };
    let frames = browser
        .waveform
        .as_ref()
        .and_then(|preview| preview.detail_context.as_ref())
        .map_or(0, |context| context.total_frames) as f64;
    let Some(region) = wavecrate_core::WaveformEditRegion::new(
        range.start_frame as f64 / frames,
        range.end_frame as f64 / frames,
    ) else {
        return false;
    };
    browser.cancel_waveform_gestures(window, cx);
    browser.waveform_edit_region = Some(region);
    browser.waveform_edit_settings = super::waveform::default_edit_settings();
    browser.waveform_fade_hover = None;
    browser.similar_sections.flash = Some((region, std::time::Instant::now()));
    cx.notify();
    true
}

/// Paint clipped match fills and rails beneath the existing selection controls.
pub(super) fn overlay(browser: &Browser, theme: wavecrate_design::Theme) -> impl gpui::IntoElement {
    use gpui::{Bounds, canvas, fill, point, prelude::*, px, size};
    let idle = browser.waveform_region_drag.is_none()
        && browser.waveform_pan_drag.is_none()
        && browser.waveform_scrollbar_drag.is_none();
    let frames = browser
        .waveform
        .as_ref()
        .and_then(|preview| preview.detail_context.as_ref())
        .map_or(0, |context| context.total_frames) as f64;
    let viewport = browser.waveform_viewport;
    let marks: Vec<_> = ranges(browser)
        .iter()
        .filter_map(|range| {
            let (start, end) = viewport.visible_range_from_absolute(
                range.start_frame as f64 / frames,
                range.end_frame as f64 / frames,
            )?;
            Some((start, end, browser.similar_sections.hovered == Some(*range)))
        })
        .collect();
    let flash = browser
        .similar_sections
        .flash
        .filter(|(region, _)| browser.waveform_edit_region == Some(*region));
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            if !idle {
                return;
            }
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            for (start, end, hovered) in &marks {
                let left = bounds.origin.x + px(*start as f32 * width);
                let span = px((end - start) as f32 * width);
                let (background, rail) = if *hovered {
                    (
                        theme.colors.waveform_similar_hover_fill,
                        theme.colors.waveform_similar_hover_rail,
                    )
                } else {
                    (
                        theme.colors.waveform_similar_fill,
                        theme.colors.waveform_similar_rail,
                    )
                };
                window.paint_quad(fill(
                    Bounds::new(point(left, bounds.origin.y), size(span, bounds.size.height)),
                    crate::theme::color(background),
                ));
                for offset in [0., (height - 2.).max(0.)] {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(left, bounds.origin.y + px(offset)),
                            size(span, px(height.min(2.))),
                        ),
                        crate::theme::color(rail),
                    ));
                }
            }
            if let Some((region, started)) = flash {
                let envelope = theme.motion.pointer_focus_envelope();
                let elapsed = started.elapsed();
                if envelope.needs_animation_at(elapsed) {
                    window.request_animation_frame();
                }
                if let Some((start, end)) =
                    viewport.visible_range_from_absolute(region.start(), region.end())
                {
                    let alpha = envelope.value_at(elapsed);
                    window.paint_quad(fill(
                        Bounds::new(
                            bounds.origin + point(px(start as f32 * width), px(0.)),
                            size(px((end - start) as f32 * width), bounds.size.height),
                        ),
                        crate::theme::color(theme.colors.waveform_edit_selection)
                            .opacity(alpha * 0.35),
                    ));
                }
            }
        },
    )
    .absolute()
    .top_0()
    .bottom_0()
    .left_0()
    .right_0()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, MouseButton, TestAppContext, point};
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    use wavecrate_core::AudioFile;
    use wavecrate_media::{WaveformPreview, load_waveform_preview};

    use super::super::{transport, waveform};

    fn write_wave_fixture() -> (tempfile::TempDir, PathBuf, WaveformPreview) {
        let directory = tempfile::tempdir().expect("create Similar Sections fixture directory");
        let path = directory.path().join("similar-sections.wav");
        let frames = 1_024_u32;
        let data_bytes = frames * 4;
        let mut bytes = Vec::with_capacity(44 + data_bytes as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&3_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&48_000_u32.to_le_bytes());
        bytes.extend_from_slice(&192_000_u32.to_le_bytes());
        bytes.extend_from_slice(&4_u16.to_le_bytes());
        bytes.extend_from_slice(&32_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_bytes.to_le_bytes());
        for frame in 0..frames {
            let phase = frame as f32 / frames as f32;
            let sample = (std::f32::consts::TAU * 5.0 * phase).sin() * 0.25;
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        fs::write(&path, bytes).expect("write Similar Sections fixture");
        let preview = load_waveform_preview(&path).expect("load Similar Sections fixture");
        (directory, path, preview)
    }

    fn entry_for(path: &Path) -> AudioFile {
        AudioFile {
            path: path.to_path_buf(),
            relative_path: PathBuf::from("similar-sections.wav"),
            byte_len: fs::metadata(path).expect("fixture metadata").len(),
            duration_seconds: Some(1_024.0 / 48_000.0),
            sample_rate: Some(48_000),
            playback_type: None,
            curation: None,
        }
    }

    fn install_browser<'a>(
        cx: &'a mut TestAppContext,
        path: &Path,
        preview: WaveformPreview,
    ) -> (gpui::Entity<Browser>, &'a mut gpui::VisualTestContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _| {
            view.entries = vec![entry_for(path)];
            view.selected = Some(0);
            view.waveform_generation = 9;
            view.waveform = Some(preview);
            view.waveform_play_region =
                Some(waveform::WaveformPlayRegion::new(0.10, 0.30).expect("play selection"));
        });
        (browser, cx)
    }

    #[gpui::test]
    fn similar_results_are_fenced_by_generation_path_and_raw_anchor(cx: &mut TestAppContext) {
        let (_directory, path, preview) = write_wave_fixture();
        let (browser, cx) = install_browser(cx, &path, preview);
        let mark = WaveformDetailRange {
            start_frame: 600,
            end_frame: 700,
        };

        browser.update(cx, |view, _| {
            let intent = current_intent(view).expect("matching Similar Sections intent");
            view.similar_sections.wanted = Some(intent);
            view.similar_sections.ranges = vec![mark];

            view.waveform_generation += 1;
            assert!(ranges(view).is_empty(), "generation change must hide marks");
            view.waveform_generation -= 1;

            view.entries[0].path = PathBuf::from("/different-source.wav");
            assert!(ranges(view).is_empty(), "path change must hide marks");
            view.entries[0].path = path.clone();

            view.waveform_play_region = Some(
                waveform::WaveformPlayRegion::new(0.100001, 0.30).expect("changed play selection"),
            );
            assert_eq!(
                current_intent(view).unwrap().request.anchor,
                view.similar_sections
                    .wanted
                    .as_ref()
                    .unwrap()
                    .request
                    .anchor,
                "subframe changes must still invalidate the raw anchor"
            );
            assert!(ranges(view).is_empty(), "raw anchor change must hide marks");
        });
    }

    #[gpui::test]
    fn cancel_marks_cancellation_and_clears_active_and_queued_work(cx: &mut TestAppContext) {
        let (_directory, path, preview) = write_wave_fixture();
        let (browser, cx) = install_browser(cx, &path, preview);
        let cancellation = Arc::new(AtomicBool::new(false));
        let mark = WaveformDetailRange {
            start_frame: 600,
            end_frame: 700,
        };

        browser.update(cx, |view, _| {
            let intent = current_intent(view).expect("matching Similar Sections intent");
            view.similar_sections.wanted = Some(intent.clone());
            view.similar_sections.queued = Some(intent);
            view.similar_sections.cancellation = Some(Arc::clone(&cancellation));
            view.similar_sections.ranges = vec![mark];
            view.similar_sections.cancel();

            assert!(!view.similar_sections.enabled());
            assert!(view.similar_sections.wanted.is_none());
            assert!(view.similar_sections.queued.is_none());
            assert!(view.similar_sections.ranges.is_empty());
        });
        assert!(cancellation.load(Ordering::Acquire));
    }

    #[gpui::test]
    fn matching_left_and_right_mark_clicks_select_edit_only_and_preserve_playback(
        cx: &mut TestAppContext,
    ) {
        let (_directory, path, preview) = write_wave_fixture();
        let (browser, cx) = install_browser(cx, &path, preview);
        let play_region = waveform::WaveformPlayRegion::new(0.10, 0.30).expect("play selection");
        let accepted = transport::PlaybackIntent {
            start: 0.12,
            region: Some(play_region),
            looped: true,
            volume: 0.75,
            path: path.clone(),
            origin: transport::PlaybackIntentOrigin::Explicit,
            resume_desired: None,
        };
        let left = WaveformDetailRange {
            start_frame: 500,
            end_frame: 600,
        };
        let right = WaveformDetailRange {
            start_frame: 750,
            end_frame: 850,
        };
        browser.update(cx, |view, _| {
            view.waveform_play_region = Some(play_region);
            view.playing_path = Some(path.clone());
            view.accepted_playback_intent = Some(accepted.clone());
            view.playback_epoch = 41;
            view.waveform_edit_settings =
                wavecrate_core::WaveformEditSettings::new(2.0, None, None).unwrap();
            let intent = current_intent(view).expect("matching Similar Sections intent");
            view.similar_sections.wanted = Some(intent);
            view.similar_sections.ranges = vec![left, right];
        });
        cx.refresh().expect("refresh Similar Sections waveform");
        cx.run_until_parked();
        let bounds = cx.debug_bounds("waveform").expect("waveform bounds");
        let at = |ratio: f64| {
            point(
                bounds.origin.x + bounds.size.width * ratio as f32,
                bounds.origin.y + bounds.size.height / 2.0,
            )
        };
        let modifiers = Modifiers::none();

        cx.simulate_mouse_down(at(0.537), MouseButton::Left, modifiers);
        cx.simulate_mouse_up(at(0.537), MouseButton::Left, modifiers);
        browser.update(cx, |view, _| {
            let edit = view.waveform_edit_region.expect("left mark edit selection");
            assert!((edit.start() - left.start_frame as f64 / 1_024.0).abs() < 0.002);
            assert!((edit.end() - left.end_frame as f64 / 1_024.0).abs() < 0.002);
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 41);
            assert_eq!(
                view.waveform_edit_settings,
                waveform::default_edit_settings()
            );
        });

        cx.simulate_mouse_down(at(0.781), MouseButton::Right, modifiers);
        cx.simulate_mouse_up(at(0.781), MouseButton::Right, modifiers);
        browser.update(cx, |view, _| {
            let edit = view
                .waveform_edit_region
                .expect("right mark edit selection");
            assert!((edit.start() - right.start_frame as f64 / 1_024.0).abs() < 0.002);
            assert!((edit.end() - right.end_frame as f64 / 1_024.0).abs() < 0.002);
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert_eq!(view.accepted_playback_intent.as_ref(), Some(&accepted));
            assert_eq!(view.playback_epoch, 41);
            assert_eq!(
                view.waveform_edit_settings,
                waveform::default_edit_settings()
            );
        });
    }
}
