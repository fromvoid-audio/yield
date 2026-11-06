//! Editable tempo for the current waveform's new playmark grid.

use gpui::{AppContext, Context, Entity, KeyDownEvent, Subscription, Window};
use yield_core::PlaymarkGridTempo;

use crate::search_input::{MAX_PLAYMARK_GRID_TEMPO_INPUT_BYTES, SearchInput, SearchInputOptions};

use super::{Browser, playmark_tempo};

pub(super) struct BpmSnapEntry {
    pub(super) input: Entity<SearchInput>,
    tempo: Option<PlaymarkGridTempo>,
    displayed_generation: Option<u64>,
    blur: Option<Subscription>,
}

impl BpmSnapEntry {
    pub(super) fn new(cx: &mut Context<Browser>) -> Self {
        Self {
            input: cx.new(|cx| {
                SearchInput::new_with_options(cx, SearchInputOptions::playmark_grid_tempo())
            }),
            tempo: None,
            displayed_generation: None,
            blur: None,
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.tempo = None;
        self.displayed_generation = None;
    }
}

pub(super) fn effective_bpm(browser: &Browser) -> Option<f64> {
    browser
        .bpm_snap_entry
        .tempo
        .map(PlaymarkGridTempo::bpm)
        .or_else(|| playmark_tempo::authored_bpm(browser))
}

fn reset_draft(browser: &mut Browser, cx: &mut Context<Browser>) {
    let text = effective_bpm(browser)
        .map(|bpm| ((bpm * 1_000.0).round() / 1_000.0).to_string())
        .filter(|text| text.len() <= MAX_PLAYMARK_GRID_TEMPO_INPUT_BYTES)
        .unwrap_or_default();
    let input = &browser.bpm_snap_entry.input;
    if input.read(cx).content() != text {
        input.update(cx, |input, cx| input.set_content(text, cx));
    }
}

pub(super) fn sync_input(browser: &mut Browser, window: &mut Window, cx: &mut Context<Browser>) {
    let focus = browser.bpm_snap_entry.input.read(cx).focus_handle();
    if browser.bpm_snap_entry.blur.is_none() {
        browser.bpm_snap_entry.blur = Some(cx.on_blur(&focus, window, |view, _, cx| {
            reset_draft(view, cx);
        }));
    }
    if browser.waveform.is_none() {
        return;
    }
    if browser.bpm_snap_entry.displayed_generation != Some(browser.waveform_generation)
        || !focus.is_focused(window)
    {
        browser.bpm_snap_entry.displayed_generation = Some(browser.waveform_generation);
        reset_draft(browser, cx);
    }
}

pub(super) fn handle_key_down(
    browser: &mut Browser,
    event: &KeyDownEvent,
    window: &mut Window,
    cx: &mut Context<Browser>,
) -> bool {
    let input = browser.bpm_snap_entry.input.read(cx);
    if !input.focus_handle().is_focused(window) {
        return false;
    }
    match event.keystroke.key.as_str() {
        "escape" => {
            reset_draft(browser, cx);
            window.focus(&browser.waveform_focus_handle);
            cx.stop_propagation();
        }
        "enter" => {
            if !event.is_held
                && !input.is_composing()
                && browser.waveform.is_some()
                && browser.bpm_snap_entry.displayed_generation == Some(browser.waveform_generation)
            {
                if let Some(tempo) = PlaymarkGridTempo::parse(input.content()) {
                    browser.bpm_snap_entry.tempo = Some(tempo);
                    reset_draft(browser, cx);
                    browser
                        .set_playback_status(format!("BPM Snap tempo set to {} BPM", tempo.bpm()));
                    window.focus(&browser.waveform_focus_handle);
                } else {
                    browser.set_playback_status("Enter 1–1000 BPM with up to three decimal places");
                }
                cx.notify();
            }
            cx.stop_propagation();
        }
        _ => {}
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, MouseButton, TestAppContext, point, px};

    #[gpui::test]
    fn typed_tempo_owns_focus_and_drives_a_new_playmark(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let preview = || yield_media::WaveformPreview {
            peaks: vec![0.5; 8],
            bands: Vec::new(),
            duration_seconds: 4.0,
            sample_rate: 48_000,
            total_frames: 192_000,
            detail_context: None,
        };
        browser.update(cx, |view, _| view.waveform = Some(preview()));
        cx.update(|window, _| window.activate_window());
        cx.refresh().unwrap();
        let input = browser.read_with(cx, |view, _| view.bpm_snap_entry.input.clone());
        let focus_input = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, app| window.focus(&input.read(app).focus_handle()));
            cx.refresh().unwrap();
        };
        focus_input(cx);
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "b i cmd-f"
        } else {
            "b i ctrl-f"
        });
        browser.read_with(cx, |view, _| {
            assert!(!view.bpm_snap_enabled);
            assert!(!view.transient_markers.enabled());
        });
        cx.update(|window, app| assert!(input.read(app).focus_handle().is_focused(window)));
        input.update(cx, |input, cx| input.set_content("120.5", cx));
        cx.simulate_keystrokes("enter");
        browser.read_with(cx, |view, _| assert_eq!(effective_bpm(view), Some(120.5)));
        browser.update(cx, |view, cx| view.toggle_bpm_snap(cx));

        focus_input(cx);
        input.update(cx, |input, cx| input.set_content("0", cx));
        cx.simulate_keystrokes("enter");
        browser.read_with(cx, |view, _| {
            assert_eq!(effective_bpm(view), Some(120.5));
            assert!(view.playback_status.contains("decimal places"));
        });
        cx.simulate_keystrokes("escape");
        input.read_with(cx, |input, _| assert_eq!(input.content(), "120.5"));

        cx.refresh().unwrap();
        let bounds = cx.debug_bounds("waveform").unwrap();
        let at = |fraction: f32| {
            point(
                bounds.left() + bounds.size.width * fraction,
                bounds.top() + px(2.0),
            )
        };
        let plain = Modifiers::none();
        let left = MouseButton::Left;
        cx.simulate_mouse_down(at(0.25), left, plain);
        cx.simulate_mouse_move(at(0.4), left, plain);
        cx.simulate_mouse_up(at(0.4), left, plain);
        browser.read_with(cx, |view, _| {
            assert_eq!(
                view.waveform_play_region.unwrap().frame_bounds(192_000),
                Some((48_000, 71_900))
            );
        });

        focus_input(cx);
        input.update(cx, |input, cx| input.set_content("134.1", cx));
        browser.update(cx, |view, cx| {
            view.invalidate_waveform_generation();
            view.waveform = Some(preview());
            cx.notify();
        });
        cx.refresh().unwrap();
        input.read_with(cx, |input, _| assert_eq!(input.content(), ""));
        browser.read_with(cx, |view, _| assert_eq!(effective_bpm(view), None));
    }
}
