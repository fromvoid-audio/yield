//! Bounded, cancellable transient markers derived from the loaded waveform preview.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui::{
    AnyView, App, Bounds, Context, Render, Task, Window, canvas, div, fill, point, prelude::*, px,
    size,
};
use yield_core::WaveformViewport;
use yield_media::{MAX_WAVEFORM_BAND_POINTS, WaveformFileRevision};

use super::Browser;

const MAX_MARKERS: usize = 512;
const MIN_GAP_SECONDS: f32 = 0.035;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum Sensitivity {
    Low,
    #[default]
    Normal,
    High,
}

impl Sensitivity {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Normal => "Normal",
            Self::High => "High",
        }
    }

    pub(super) fn short_label(self) -> &'static str {
        match self {
            Self::Low => "Lo",
            Self::Normal => "Mid",
            Self::High => "Hi",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Low => Self::Normal,
            Self::Normal => Self::High,
            Self::High => Self::Low,
        }
    }

    fn threshold(self, median: f32) -> f32 {
        let (multiplier, floor) = match self {
            Self::Low => (4.0, 0.075),
            Self::Normal => (2.5, 0.045),
            Self::High => (1.6, 0.025),
        };
        (median * multiplier).max(floor)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Intent {
    generation: u64,
    path: PathBuf,
    revision: WaveformFileRevision,
    sensitivity: Sensitivity,
}

#[derive(Default)]
pub(super) struct TransientMarkersState {
    enabled: bool,
    sensitivity: Sensitivity,
    wanted: Option<Intent>,
    queued: Option<Intent>,
    cancellation: Option<Arc<AtomicBool>>,
    task: Option<Task<()>>,
    positions: Arc<[f32]>,
}

impl TransientMarkersState {
    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn sensitivity(&self) -> Sensitivity {
        self.sensitivity
    }

    pub(super) fn invalidate(&mut self) {
        if let Some(cancellation) = &self.cancellation {
            cancellation.store(true, Ordering::Release);
        }
        self.wanted = None;
        self.queued = None;
        self.positions = Arc::from([]);
    }
}

impl Drop for TransientMarkersState {
    fn drop(&mut self) {
        self.invalidate();
    }
}

fn current_intent(browser: &Browser) -> Option<Intent> {
    let preview = browser.waveform.as_ref()?;
    let path = browser.entries.get(browser.selected?)?.path.clone();
    let context = preview.detail_context.as_ref()?;
    if context.path != path {
        return None;
    }
    Some(Intent {
        generation: browser.waveform_generation,
        path,
        revision: context.file_revision.clone(),
        sensitivity: browser.transient_markers.sensitivity,
    })
}

pub(super) fn toggle(browser: &mut Browser, cx: &mut Context<Browser>) {
    if browser.transient_markers.enabled {
        browser.transient_markers.enabled = false;
        browser.transient_markers.invalidate();
        browser.playback_status = String::from("Transient markers hidden");
    } else if browser.waveform.is_some() {
        browser.transient_markers.enabled = true;
        schedule(browser, cx);
    } else {
        browser.playback_status = String::from("Load a waveform to show transient markers");
    }
    cx.notify();
}

pub(super) fn cycle_sensitivity(browser: &mut Browser, cx: &mut Context<Browser>) {
    if browser.waveform.is_none() {
        return;
    }
    let next = browser.transient_markers.sensitivity.next();
    browser.transient_markers.sensitivity = next;
    if browser.transient_markers.enabled {
        schedule(browser, cx);
    } else {
        browser.playback_status = format!("Transient sensitivity: {}", next.label());
    }
    cx.notify();
}

struct SensitivityTooltip;

impl Render for SensitivityTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::active(cx);
        div()
            .px(px(theme.spacing.control_gap))
            .py(px(theme.spacing.control_gap))
            .bg(crate::theme::color(theme.colors.surface_raised))
            .text_size(px(theme.typography.small_size))
            .text_color(crate::theme::color(theme.colors.text_primary))
            .child("Transient sensitivity. Click to cycle Low, Normal, and High.")
    }
}

pub(super) fn sensitivity_tooltip(_: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| SensitivityTooltip).into()
}

pub(super) fn schedule(browser: &mut Browser, cx: &mut Context<Browser>) {
    if !browser.transient_markers.enabled {
        return;
    }
    let Some(intent) = current_intent(browser) else {
        browser.transient_markers.enabled = false;
        browser.transient_markers.invalidate();
        browser.playback_status = String::from("Transient markers unavailable for this waveform");
        return;
    };
    if browser.transient_markers.wanted.as_ref() == Some(&intent) {
        return;
    }
    browser.transient_markers.wanted = Some(intent.clone());
    browser.transient_markers.positions = Arc::from([]);
    browser.playback_status = String::from("Finding transient markers…");
    if browser.transient_markers.task.is_some() {
        if let Some(cancellation) = &browser.transient_markers.cancellation {
            cancellation.store(true, Ordering::Release);
        }
        browser.transient_markers.queued = Some(intent);
    } else {
        launch(browser, intent, cx);
    }
}

fn launch(browser: &mut Browser, intent: Intent, cx: &mut Context<Browser>) {
    let Some(preview) = browser.waveform.as_ref() else {
        return;
    };
    let bands = preview.bands.clone();
    let duration = preview.duration_seconds;
    let cancellation = Arc::new(AtomicBool::new(false));
    browser.transient_markers.cancellation = Some(Arc::clone(&cancellation));
    browser.transient_markers.task = Some(cx.spawn(async move |this, cx| {
        let (intent, cancellation, result) = cx
            .background_executor()
            .spawn(async move {
                let result = detect(&bands, duration, intent.sensitivity, &cancellation);
                (intent, cancellation, result)
            })
            .await;
        let _ = this.update(cx, |browser, cx| {
            browser.transient_markers.task = None;
            browser.transient_markers.cancellation = None;
            accept_result(
                browser,
                &intent,
                cancellation.load(Ordering::Acquire),
                result,
            );
            if let Some(next) = browser.transient_markers.queued.take()
                && browser.transient_markers.enabled
                && browser.transient_markers.wanted.as_ref() == Some(&next)
                && current_intent(browser).as_ref() == Some(&next)
            {
                launch(browser, next, cx);
            }
            cx.notify();
        });
    }));
}

fn accept_result(
    browser: &mut Browser,
    intent: &Intent,
    canceled: bool,
    result: Result<Vec<f32>, &'static str>,
) {
    if canceled
        || !browser.transient_markers.enabled
        || browser.transient_markers.wanted.as_ref() != Some(intent)
        || current_intent(browser).as_ref() != Some(intent)
    {
        return;
    }
    match result {
        Ok(positions) => {
            browser.playback_status = format!("{} transient markers", positions.len());
            browser.transient_markers.positions = positions.into();
        }
        Err(error) => {
            browser.transient_markers.enabled = false;
            browser.transient_markers.invalidate();
            browser.playback_status = format!("Transient markers unavailable: {error}");
        }
    }
}

fn detect(
    bands: &[[f32; 4]],
    duration: f32,
    sensitivity: Sensitivity,
    cancellation: &AtomicBool,
) -> Result<Vec<f32>, &'static str> {
    if bands.len() < 3 || bands.len() > MAX_WAVEFORM_BAND_POINTS {
        return Err("invalid preview band count");
    }
    if !duration.is_finite() || duration <= 0.0 {
        return Err("invalid waveform duration");
    }
    let mut novelty = Vec::with_capacity(bands.len());
    novelty.push(0.0);
    for (index, pair) in bands.windows(2).enumerate() {
        if index % 128 == 0 && cancellation.load(Ordering::Acquire) {
            return Err("canceled");
        }
        if pair.iter().flatten().any(|value| !value.is_finite()) {
            return Err("invalid preview bands");
        }
        let flux = (0..3)
            .map(|lane| (pair[1][lane] - pair[0][lane]).max(0.0))
            .sum::<f32>()
            + (pair[1][3] - pair[0][3]).max(0.0) * 0.5;
        novelty.push(flux);
    }
    let mut sorted = novelty.clone();
    sorted.sort_by(f32::total_cmp);
    let median = sorted[sorted.len() / 2];
    let threshold = sensitivity.threshold(median);
    let min_gap = ((MIN_GAP_SECONDS / duration) * bands.len() as f32)
        .ceil()
        .max((bands.len() as f32 / MAX_MARKERS as f32).ceil())
        .max(1.0) as usize;
    let mut positions = Vec::new();
    let mut last_index = None;
    for index in 1..novelty.len() - 1 {
        if index % 128 == 0 && cancellation.load(Ordering::Acquire) {
            return Err("canceled");
        }
        let value = novelty[index];
        if value < threshold || value < novelty[index - 1] || value < novelty[index + 1] {
            continue;
        }
        if last_index.is_some_and(|previous| index - previous < min_gap) {
            continue;
        }
        positions.push(index as f32 / bands.len() as f32);
        last_index = Some(index);
        if positions.len() >= MAX_MARKERS {
            break;
        }
    }
    Ok(positions)
}

fn positions(browser: &Browser) -> Arc<[f32]> {
    if browser.transient_markers.enabled
        && browser.transient_markers.wanted.as_ref() == current_intent(browser).as_ref()
    {
        Arc::clone(&browser.transient_markers.positions)
    } else {
        Arc::from([])
    }
}

fn visible_positions(
    positions: &[f32],
    viewport: WaveformViewport,
) -> impl Iterator<Item = f32> + '_ {
    positions.iter().filter_map(move |ratio| {
        viewport
            .visible_ratio_from_absolute(f64::from(*ratio))
            .map(|visible| visible as f32)
    })
}

pub(super) fn render_overlay(
    browser: &Browser,
    theme: &yield_design::Theme,
) -> impl gpui::IntoElement {
    let positions = positions(browser);
    let viewport = browser.waveform_viewport;
    let tint = crate::theme::color(theme.colors.waveform_playhead).opacity(0.5);
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0()
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    for visible in visible_positions(&positions, viewport) {
                        let x = bounds.origin.x + bounds.size.width * visible;
                        window.paint_quad(fill(
                            Bounds::new(
                                point(x, bounds.origin.y),
                                size(px(1.0), bounds.size.height),
                            ),
                            tint,
                        ));
                    }
                },
            )
            .size_full(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{KeyDownEvent, Keystroke, TestAppContext};
    use std::{fs, path::Path};
    use yield_core::AudioFile;

    fn fixture() -> (tempfile::TempDir, PathBuf, yield_media::WaveformPreview) {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory.path().join("transients.wav");
        let frames = 256_u32;
        let mut bytes = Vec::with_capacity(44 + frames as usize * 4);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + frames * 4).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&3_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&48_000_u32.to_le_bytes());
        bytes.extend_from_slice(&192_000_u32.to_le_bytes());
        bytes.extend_from_slice(&4_u16.to_le_bytes());
        bytes.extend_from_slice(&32_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(frames * 4).to_le_bytes());
        bytes.resize(44 + frames as usize * 4, 0);
        fs::write(&path, bytes).expect("fixture WAV");
        let mut preview = yield_media::load_waveform_preview(&path).expect("load preview");
        preview.bands = vec![[0.0; 4]; 100];
        preview.bands[20] = [1.0, 0.8, 0.7, 1.0];
        preview.bands[70] = [0.8, 1.0, 0.9, 1.0];
        preview.duration_seconds = 1.0;
        (directory, path, preview)
    }

    fn install<'a>(
        cx: &'a mut TestAppContext,
        path: &Path,
        preview: yield_media::WaveformPreview,
    ) -> (gpui::Entity<Browser>, &'a mut gpui::VisualTestContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _| {
            view.entries = vec![AudioFile {
                path: path.to_path_buf(),
                relative_path: PathBuf::from("transients.wav"),
                byte_len: fs::metadata(path).expect("metadata").len(),
                duration_seconds: Some(1.0),
                sample_rate: Some(48_000),
                playback_type: None,
                curation: None,
            }];
            view.selected = Some(0);
            view.waveform_generation = 7;
            view.waveform = Some(preview);
        });
        (browser, cx)
    }

    #[test]
    fn detects_bounded_positions_for_two_spectral_attacks() {
        let mut bands = vec![[0.0; 4]; 100];
        bands[20] = [1.0, 0.8, 0.7, 1.0];
        bands[70] = [0.8, 1.0, 0.9, 1.0];
        let positions =
            detect(&bands, 1.0, Sensitivity::Normal, &AtomicBool::new(false)).expect("transients");
        assert_eq!(positions, [0.2, 0.7]);
    }

    #[test]
    fn sensitivity_selects_attacks_by_strength() {
        let mut bands = vec![[0.0; 4]; 100];
        bands[20][0] = 0.03;
        bands[40][0] = 0.055;
        bands[60][0] = 0.10;
        for (sensitivity, expected) in [
            (Sensitivity::Low, &[0.6][..]),
            (Sensitivity::Normal, &[0.4, 0.6]),
            (Sensitivity::High, &[0.2, 0.4, 0.6]),
        ] {
            let positions =
                detect(&bands, 1.0, sensitivity, &AtomicBool::new(false)).expect("transients");
            assert_eq!(positions, expected);
        }
    }

    #[test]
    fn loaded_wav_preview_produces_markers_near_audible_attacks() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory.path().join("attacks.wav");
        let frames = 48_000_u32;
        let mut bytes = Vec::with_capacity(44 + frames as usize * 4);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + frames * 4).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&3_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&48_000_u32.to_le_bytes());
        bytes.extend_from_slice(&192_000_u32.to_le_bytes());
        bytes.extend_from_slice(&4_u16.to_le_bytes());
        bytes.extend_from_slice(&32_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(frames * 4).to_le_bytes());
        for frame in 0..frames {
            let sample: f32 =
                if (9_600..10_112).contains(&frame) || (33_600..34_112).contains(&frame) {
                    if frame % 2 == 0 { 0.8 } else { -0.8 }
                } else {
                    0.0
                };
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        fs::write(&path, bytes).expect("fixture WAV");
        let preview = yield_media::load_waveform_preview(&path).expect("load preview");
        let positions = detect(
            &preview.bands,
            preview.duration_seconds,
            Sensitivity::Normal,
            &AtomicBool::new(false),
        )
        .expect("transients");
        assert!(
            positions
                .iter()
                .any(|position| (position - 0.2).abs() < 0.05)
        );
        assert!(
            positions
                .iter()
                .any(|position| (position - 0.7).abs() < 0.05)
        );
    }

    #[test]
    fn rejects_invalid_input_and_cancellation() {
        assert!(
            detect(
                &[[0.0; 4]; 2],
                1.0,
                Sensitivity::Normal,
                &AtomicBool::new(false)
            )
            .is_err()
        );
        let bands = vec![[0.0; 4]; 256];
        assert_eq!(
            detect(&bands, 1.0, Sensitivity::Normal, &AtomicBool::new(true)),
            Err("canceled")
        );
    }

    #[test]
    fn dense_preview_keeps_marker_count_and_order_bounded() {
        let mut bands = vec![[0.0; 4]; MAX_WAVEFORM_BAND_POINTS];
        for index in (2..bands.len()).step_by(3) {
            bands[index] = [1.0; 4];
        }
        let positions = detect(&bands, 20.0, Sensitivity::Normal, &AtomicBool::new(false))
            .expect("dense preview");
        assert!(!positions.is_empty());
        assert!(positions.len() <= MAX_MARKERS);
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn marker_projection_tracks_zoomed_viewport() {
        let viewport = WaveformViewport {
            start: 0.25,
            end: 0.75,
        };
        let projected = visible_positions(&[0.1, 0.375, 0.625, 0.9], viewport).collect::<Vec<_>>();
        assert_eq!(projected, [0.25, 0.75]);
    }

    #[gpui::test]
    fn toggle_reload_and_stale_generation_hide_old_markers(cx: &mut TestAppContext) {
        let (_directory, path, preview) = fixture();
        let (browser, cx) = install(cx, &path, preview);
        browser.update(cx, |view, cx| toggle(view, cx));
        cx.run_until_parked();
        browser.update(cx, |view, cx| {
            assert_eq!(positions(view).as_ref(), [0.2, 0.7]);
            toggle(view, cx);
            assert!(positions(view).is_empty());
            toggle(view, cx);
        });
        cx.run_until_parked();
        browser.update(cx, |view, _| {
            assert_eq!(positions(view).as_ref(), [0.2, 0.7]);
            view.waveform_generation += 1;
            assert!(
                positions(view).is_empty(),
                "stale generation must not paint"
            );
            view.waveform_generation -= 1;
            view.entries[0].path = PathBuf::from("/different-source.wav");
            assert!(positions(view).is_empty(), "stale source must not paint");
            view.entries[0].path = path.clone();
            view.waveform
                .as_mut()
                .expect("preview")
                .detail_context
                .as_mut()
                .expect("revision context")
                .file_revision
                .change_marker
                .push_str("-changed");
            assert!(positions(view).is_empty(), "stale revision must not paint");
        });
    }

    #[gpui::test]
    fn sensitivity_button_updates_markers_and_ignores_old_results(cx: &mut TestAppContext) {
        let (_directory, path, mut preview) = fixture();
        preview.bands[40][0] = 0.03;
        let (browser, cx) = install(cx, &path, preview);
        browser.update(cx, |view, cx| toggle(view, cx));
        cx.run_until_parked();
        let old = browser.update(cx, |view, _| {
            assert_eq!(positions(view).as_ref(), [0.2, 0.7]);
            current_intent(view).expect("normal sensitivity intent")
        });
        cx.refresh().expect("loaded waveform layout");
        let toolbar = cx.debug_bounds("main-toolbar").expect("toolbar");
        let button = cx
            .debug_bounds("transient-sensitivity-toolbar")
            .expect("sensitivity button");
        assert!(button.left() >= toolbar.left() && button.right() <= toolbar.right());
        assert!(button.top() >= toolbar.top() && button.bottom() <= toolbar.bottom());
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        browser.update(cx, |view, _| {
            assert_eq!(view.transient_markers.sensitivity(), Sensitivity::High);
            accept_result(view, &old, false, Err("old failure"));
            assert!(view.transient_markers.enabled());
            assert_eq!(positions(view).as_ref(), [0.2, 0.4, 0.7]);
        });
    }

    #[gpui::test]
    fn hiding_during_preparation_cancels_results(cx: &mut TestAppContext) {
        let (_directory, path, preview) = fixture();
        let (browser, cx) = install(cx, &path, preview);
        browser.update(cx, |view, cx| {
            toggle(view, cx);
            toggle(view, cx);
        });
        cx.run_until_parked();
        browser.update(cx, |view, _| {
            assert!(!view.transient_markers.enabled());
            assert!(positions(view).is_empty());
            assert_eq!(view.playback_status, "Transient markers hidden");
        });
    }

    #[gpui::test]
    fn stale_worker_completion_cannot_replace_current_markers(cx: &mut TestAppContext) {
        let (_directory, path, preview) = fixture();
        let (browser, cx) = install(cx, &path, preview);
        browser.update(cx, |view, _| {
            let old = current_intent(view).expect("old intent");
            view.transient_markers.enabled = true;
            view.transient_markers.wanted = Some(old.clone());
            view.waveform_generation += 1;
            let current = current_intent(view).expect("current intent");
            view.transient_markers.wanted = Some(current.clone());
            accept_result(view, &old, false, Ok(vec![0.1]));
            assert!(positions(view).is_empty());
            accept_result(view, &current, false, Ok(vec![0.2, 0.7]));
            assert_eq!(positions(view).as_ref(), [0.2, 0.7]);
            accept_result(view, &old, false, Err("old failure"));
            assert!(view.transient_markers.enabled());
            assert_eq!(positions(view).as_ref(), [0.2, 0.7]);
        });
    }

    #[gpui::test]
    fn i_shortcut_only_toggles_with_waveform_focus_and_ignores_repeat(cx: &mut TestAppContext) {
        let (_directory, path, preview) = fixture();
        let (browser, cx) = install(cx, &path, preview);
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                let press = |view: &mut Browser,
                             held,
                             window: &mut gpui::Window,
                             cx: &mut Context<Browser>| {
                    view.handle_key_down(
                        &KeyDownEvent {
                            keystroke: Keystroke::parse("i").expect("I key"),
                            is_held: held,
                        },
                        window,
                        cx,
                    );
                };
                window.focus(&view.search_input.read(cx).focus_handle());
                press(view, false, window, cx);
                assert!(!view.transient_markers.enabled());
                window.focus(&view.waveform_focus_handle);
                press(view, false, window, cx);
                assert!(view.transient_markers.enabled());
                press(view, true, window, cx);
                assert!(view.transient_markers.enabled());
            });
        });
    }
}
