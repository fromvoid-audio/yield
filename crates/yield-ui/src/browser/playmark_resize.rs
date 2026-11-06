//! Frame-exact keyboard sizing of the focused waveform play selection.

use gpui::{Context, KeyDownEvent, Window};

use super::{Browser, similar_sections, waveform::WaveformPlayRegion};

#[derive(Clone, Copy)]
pub(super) struct ResizeAnchor {
    generation: u64,
    total_frames: usize,
    previous: (usize, usize),
    // Keep the half-frame midpoint through odd-width results so a subsequent
    // doubling can restore an even original range without rounding drift.
    midpoint_twice: u128,
}

#[derive(Clone, Copy)]
enum Resize {
    Half,
    Double,
}

fn resize_key(
    key: &str,
    secondary: bool,
    shift: bool,
    alt: bool,
    function: bool,
) -> Option<Resize> {
    if !secondary || shift || alt || function {
        return None;
    }
    match key {
        "left" | "arrowleft" => Some(Resize::Half),
        "right" | "arrowright" => Some(Resize::Double),
        _ => None,
    }
}

fn frames(region: WaveformPlayRegion, total: usize) -> Option<(usize, usize)> {
    if total == 0 || !region.is_valid() {
        return None;
    }
    let start = (region.start * total as f64)
        .round()
        .clamp(0.0, (total - 1) as f64) as usize;
    let end = (region.end * total as f64)
        .round()
        .clamp((start + 1) as f64, total as f64) as usize;
    Some((start, end))
}

fn resized(
    region: WaveformPlayRegion,
    total: usize,
    generation: u64,
    resize: Resize,
    anchor: Option<ResizeAnchor>,
) -> Option<(WaveformPlayRegion, ResizeAnchor)> {
    let current = frames(region, total)?;
    let width = current.1 - current.0;
    let requested = match resize {
        // An odd frame cannot be divided exactly; round down, bounded at one.
        Resize::Half => (width / 2).max(1),
        Resize::Double => width.saturating_mul(2).min(total),
    };
    if requested == width {
        return None;
    }
    let midpoint_twice = anchor
        .filter(|anchor| {
            anchor.generation == generation
                && anchor.total_frames == total
                && anchor.previous == current
        })
        .map_or(current.0 as u128 + current.1 as u128, |anchor| {
            anchor.midpoint_twice
        });
    let centered_start = midpoint_twice.saturating_sub(requested as u128) / 2;
    let start = centered_start.min((total - requested) as u128) as usize;
    let end = start + requested;
    let constrained =
        midpoint_twice < requested as u128 || centered_start > (total - requested) as u128;
    let region = WaveformPlayRegion::new(start as f64 / total as f64, end as f64 / total as f64)?;
    Some((
        region,
        ResizeAnchor {
            generation,
            total_frames: total,
            previous: (start, end),
            midpoint_twice: if constrained {
                start as u128 + end as u128
            } else {
                midpoint_twice
            },
        },
    ))
}

impl Browser {
    pub(super) fn handle_playmark_resize_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let modifiers = event.keystroke.modifiers;
        let Some(resize) = resize_key(
            &event.keystroke.key,
            modifiers.platform || modifiers.control,
            modifiers.shift,
            modifiers.alt,
            modifiers.function,
        ) else {
            return false;
        };
        if !self.waveform_focus_handle.is_focused(window)
            || self.theme_settings_open
            || self.audio_settings_open
            || self.context_menu.is_some()
        {
            return false;
        }
        let Some((region, preview)) = self.waveform_play_region.zip(self.waveform.as_ref()) else {
            // Waveform Command-arrow must not navigate browser playback history.
            cx.stop_propagation();
            return true;
        };
        let next = resized(
            region,
            preview.total_frames,
            self.waveform_generation,
            resize,
            self.playmark_resize_anchor,
        );
        if let Some((region, anchor)) = next {
            self.cancel_waveform_gestures(window, cx);
            self.set_waveform_play_region(Some(region));
            self.playmark_resize_anchor = Some(anchor);
            similar_sections::invalidate_if_stale(self);
            if let Some(path) = self
                .selected
                .and_then(|index| self.entries.get(index))
                .map(|entry| entry.path.clone())
            {
                if let Some(Err(error)) = self.submit_playmark_timing_span(&path, region, cx) {
                    self.set_playback_status(format!(
                        "Playmark resize update unavailable: {error}"
                    ));
                }
            }
            cx.notify();
        }
        cx.stop_propagation();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Keystroke, TestAppContext};
    use yield_media::WaveformPreview;

    #[test]
    fn frame_resize_preserves_midpoint_round_trip_and_shifts_at_boundaries() {
        let original = WaveformPlayRegion::new(0.4, 0.46).unwrap();
        let (half, anchor) = resized(original, 100, 1, Resize::Half, None).unwrap();
        assert_eq!(frames(half, 100), Some((41, 44)));
        let (restored, _) = resized(half, 100, 1, Resize::Double, Some(anchor)).unwrap();
        assert_eq!(restored, original);
        // A different waveform generation must not reuse a half-frame anchor.
        let (fresh, _) = resized(half, 100, 2, Resize::Double, Some(anchor)).unwrap();
        assert_eq!(frames(fresh, 100), Some((39, 45)));
        for (bounds, expected) in [
            ((0.0, 0.2), (0, 40)),
            ((0.8, 1.0), (60, 100)),
            ((0.1, 0.9), (0, 100)),
        ] {
            let (result, _) = resized(
                WaveformPlayRegion::new(bounds.0, bounds.1).unwrap(),
                100,
                1,
                Resize::Double,
                None,
            )
            .unwrap();
            assert_eq!(frames(result, 100), Some(expected));
        }
        let mut region = original;
        let mut anchor = None;
        while let Some((next, next_anchor)) = resized(region, 100, 1, Resize::Half, anchor) {
            region = next;
            anchor = Some(next_anchor);
        }
        let (start, end) = frames(region, 100).unwrap();
        assert_eq!(end - start, 1);
        assert!(resized(region, 0, 1, Resize::Double, anchor).is_none());
    }

    #[gpui::test]
    fn focused_waveform_resizes_but_search_and_unmodified_arrows_keep_their_routes(
        cx: &mut TestAppContext,
    ) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                view.waveform = Some(WaveformPreview {
                    peaks: vec![0.5; 8],
                    bands: Vec::new(),
                    duration_seconds: 1.0,
                    sample_rate: 100,
                    total_frames: 100,
                    detail_context: None,
                });
                view.set_waveform_play_region(WaveformPlayRegion::new(0.2, 0.6));
                let event = KeyDownEvent {
                    keystroke: Keystroke::parse("ctrl-left").unwrap(),
                    is_held: false,
                };
                window.focus(&view.waveform_focus_handle);
                view.handle_key_down(&event, window, cx);
                assert_eq!(
                    frames(view.waveform_play_region.unwrap(), 100),
                    Some((30, 50))
                );
                window.focus(&view.search_input.read(cx).focus_handle());
                view.handle_key_down(&event, window, cx);
                assert_eq!(
                    frames(view.waveform_play_region.unwrap(), 100),
                    Some((30, 50))
                );
                window.focus(&view.waveform_focus_handle);
                let plain = KeyDownEvent {
                    keystroke: Keystroke::parse("left").unwrap(),
                    is_held: false,
                };
                view.handle_key_down(&plain, window, cx);
                assert_eq!(
                    frames(view.waveform_play_region.unwrap(), 100),
                    Some((10, 30))
                );
                view.set_waveform_play_region(None);
                view.handle_key_down(&event, window, cx);
                assert!(view.waveform_play_region.is_none());
            })
        });
    }
}
