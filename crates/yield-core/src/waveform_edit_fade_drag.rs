//! Renderer-neutral fade gestures derived from one captured edit selection.

use crate::{WaveformEditFade, WaveformEditRegion, WaveformEditSettings};

/// One of the six native waveform fade handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaveformEditFadeHandle {
    /// Move the fade-in's inner endpoint.
    InEnd,
    /// Move the selection start while retaining the fade-in's inner endpoint.
    InStart,
    /// Move the fade-in's outer endpoint.
    InOuterStart,
    /// Move the fade-out's inner endpoint.
    OutStart,
    /// Move the selection end while retaining the fade-out's inner endpoint.
    OutEnd,
    /// Move the fade-out's outer endpoint.
    OutOuterEnd,
}

/// Captured baseline for a waveform fade drag.
///
/// Pointer positions use absolute normalized audio coordinates. Gain is preserved, including
/// when a fade handle resizes the region. No playback or filesystem operation is performed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveformEditFadeDrag {
    handle: WaveformEditFadeHandle,
    baseline: WaveformEditRegion,
    settings: WaveformEditSettings,
    fixed_edge: f64,
    curve: f32,
}

impl WaveformEditFadeDrag {
    /// Return the handle captured by this gesture.
    pub const fn handle(self) -> WaveformEditFadeHandle {
        self.handle
    }

    /// Capture the region, effects, selected curve and fixed inner endpoint.
    pub fn new(
        handle: WaveformEditFadeHandle,
        region: WaveformEditRegion,
        settings: WaveformEditSettings,
    ) -> Self {
        let leading = matches!(
            handle,
            WaveformEditFadeHandle::InEnd
                | WaveformEditFadeHandle::InStart
                | WaveformEditFadeHandle::InOuterStart
        );
        let fade = if leading {
            settings.fade_in()
        } else {
            settings.fade_out()
        };
        let length = region.width() * f64::from(fade.map_or(0.0, WaveformEditFade::length));
        Self {
            handle,
            baseline: region,
            settings,
            fixed_edge: if leading {
                region.start() + length
            } else {
                region.end() - length
            },
            curve: fade.map_or(0.5, WaveformEditFade::curve),
        }
    }

    /// Apply a pointer update to the current provisional selection.
    ///
    /// Opposite inner fades restore their baseline length when a collision recedes. Region-edge
    /// handles use their captured baseline and clamp the result to audio, as the native editor
    /// does. Outer handles preserve region geometry. Nonfinite input leaves the current state
    /// unchanged; `None` means an edge cannot form a nonempty valid region and must be ignored.
    pub fn apply(
        self,
        region: WaveformEditRegion,
        settings: WaveformEditSettings,
        ratio: f64,
    ) -> Option<(WaveformEditRegion, WaveformEditSettings)> {
        if !ratio.is_finite() {
            return Some((region, settings));
        }
        let ratio = ratio.clamp(0.0, 1.0);
        match self.handle {
            WaveformEditFadeHandle::InStart => self.resize_edge(ratio, true),
            WaveformEditFadeHandle::OutEnd => self.resize_edge(ratio, false),
            _ if region.width() <= f64::from(f32::EPSILON) => Some((region, settings)),
            WaveformEditFadeHandle::InEnd => self.resize_inner(region, settings, ratio, true),
            WaveformEditFadeHandle::OutStart => self.resize_inner(region, settings, ratio, false),
            WaveformEditFadeHandle::InOuterStart => {
                Some((region, resize_outer(region, settings, ratio, true)))
            }
            WaveformEditFadeHandle::OutOuterEnd => {
                Some((region, resize_outer(region, settings, ratio, false)))
            }
        }
    }

    fn resize_inner(
        self,
        region: WaveformEditRegion,
        settings: WaveformEditSettings,
        ratio: f64,
        leading: bool,
    ) -> Option<(WaveformEditRegion, WaveformEditSettings)> {
        let edge = ratio.clamp(region.start(), region.end());
        let opposite = if leading {
            self.settings.fade_out()
        } else {
            self.settings.fade_in()
        };
        let opposite_length =
            self.baseline.width() * f64::from(opposite.map_or(0.0, WaveformEditFade::length));
        let selected_length = if leading {
            edge - region.start()
        } else {
            region.end() - edge
        };
        let remaining = (region.width() - selected_length).max(0.0);
        let opposite_length = opposite_length.min(remaining) / region.width();
        let selected_length = selected_length / region.width();
        let bounded = audio_region(region.start(), region.end())?;
        let selected = if leading {
            settings.fade_in().or(self.settings.fade_in())
        } else {
            settings.fade_out().or(self.settings.fade_out())
        };
        let selected = fade_with_length(selected, selected_length, self.curve);
        let other_current = if leading {
            settings.fade_out()
        } else {
            settings.fade_in()
        };
        // Retain the original outer-gain value when an inner collision temporarily removes a fade.
        let other = opposite.and_then(|fade| {
            fade_with_length(other_current.or(Some(fade)), opposite_length, fade.curve())
        });
        let clean = WaveformEditSettings::new(settings.gain(), None, None)?;
        let result = if leading {
            clean
                .with_fade_in_for_region(bounded, selected)
                .with_fade_out_for_region(bounded, other)
        } else {
            clean
                .with_fade_in_for_region(bounded, other)
                .with_fade_out_for_region(bounded, selected)
        };
        Some((bounded, result))
    }

    fn resize_edge(
        self,
        ratio: f64,
        leading: bool,
    ) -> Option<(WaveformEditRegion, WaveformEditSettings)> {
        let region = if leading {
            if self.baseline.end() < 0.0 {
                return None;
            }
            audio_region(ratio.clamp(0.0, self.baseline.end()), self.baseline.end())?
        } else {
            if self.baseline.start() > 1.0 {
                return None;
            }
            audio_region(
                self.baseline.start(),
                ratio.clamp(self.baseline.start(), 1.0),
            )?
        };
        let clean = WaveformEditSettings::new(self.settings.gain(), None, None)?;
        if region.width() <= f64::from(f32::EPSILON) {
            return Some((region, clean));
        }
        let in_settings =
            WaveformEditSettings::new(self.settings.gain(), self.settings.fade_in(), None)?;
        let out_settings =
            WaveformEditSettings::new(self.settings.gain(), None, self.settings.fade_out())?;
        let preserved_in = in_settings
            .for_resized_region(self.baseline, region)
            .fade_in();
        let preserved_out = out_settings
            .for_resized_region(self.baseline, region)
            .fade_out();
        let preserved_in = snap_outer_endpoint(
            preserved_in,
            self.settings.fade_in(),
            self.baseline,
            region,
            true,
        );
        let preserved_out = snap_outer_endpoint(
            preserved_out,
            self.settings.fade_out(),
            self.baseline,
            region,
            false,
        );
        let fixed = self.fixed_edge.clamp(region.start(), region.end());
        let length = if leading {
            fixed - region.start()
        } else {
            region.end() - fixed
        };
        let preserved = if leading { preserved_in } else { preserved_out };
        let original = if leading {
            self.settings.fade_in()
        } else {
            self.settings.fade_out()
        };
        let template = WaveformEditFade::new(
            0.0,
            self.curve,
            preserved.map_or(0.0, WaveformEditFade::mute),
            original.map_or(1.0, WaveformEditFade::outer_gain),
        );
        let selected = fade_with_length(template, length / region.width(), self.curve);
        let result = if leading {
            // The original start-handle operation gives the opposite fade precedence.
            clean
                .with_fade_out_for_region(region, preserved_out)
                .with_fade_in_for_region(region, selected)
        } else {
            clean
                .with_fade_in_for_region(region, preserved_in)
                .with_fade_out_for_region(region, selected)
        };
        Some((region, result))
    }
}

fn snap_outer_endpoint(
    preserved: Option<WaveformEditFade>,
    original: Option<WaveformEditFade>,
    baseline: WaveformEditRegion,
    region: WaveformEditRegion,
    leading: bool,
) -> Option<WaveformEditFade> {
    let fade = preserved?;
    let original = original?;
    if original.mute() <= 0.0 {
        return Some(fade);
    }
    let offset = baseline.width() * f64::from(original.mute());
    let endpoint = if leading {
        baseline.start() - offset
    } else {
        baseline.end() + offset
    };
    let endpoint = if endpoint <= 1.0e-6 {
        0.0
    } else if endpoint >= 1.0 - 1.0e-6 {
        1.0
    } else {
        endpoint
    };
    let mute = if leading {
        region.start() - endpoint.clamp(0.0, region.start())
    } else {
        endpoint.clamp(region.end(), 1.0) - region.end()
    } / region.width();
    WaveformEditFade::new(fade.length(), fade.curve(), mute as f32, fade.outer_gain())
}

fn audio_region(start: f64, end: f64) -> Option<WaveformEditRegion> {
    WaveformEditRegion::new(start.clamp(0.0, 1.0), end.clamp(0.0, 1.0))
}

fn fade_with_length(
    source: Option<WaveformEditFade>,
    length: f64,
    curve: f32,
) -> Option<WaveformEditFade> {
    WaveformEditFade::new(
        length.clamp(0.0, 1.0) as f32,
        curve,
        source.map_or(0.0, WaveformEditFade::mute),
        source.map_or(1.0, WaveformEditFade::outer_gain),
    )
}

fn resize_outer(
    region: WaveformEditRegion,
    settings: WaveformEditSettings,
    ratio: f64,
    leading: bool,
) -> WaveformEditSettings {
    let Some(fade) = (if leading {
        settings.fade_in()
    } else {
        settings.fade_out()
    }) else {
        return settings;
    };
    let mute = if leading {
        if region.start() <= 0.0 {
            0.0
        } else {
            (region.start() - ratio.clamp(0.0, region.start())) / region.width()
        }
    } else if region.end() >= 1.0 {
        0.0
    } else {
        (ratio.clamp(region.end(), 1.0) - region.end()) / region.width()
    };
    let mute = mute as f32;
    let fade = WaveformEditFade::new(
        fade.length(),
        fade.curve(),
        if mute.is_finite() { mute } else { 0.0 },
        fade.outer_gain(),
    );
    if leading {
        settings.with_fade_in_for_region(region, fade)
    } else {
        settings.with_fade_out_for_region(region, fade)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(start: f64, end: f64) -> WaveformEditRegion {
        WaveformEditRegion::new(start, end).unwrap()
    }

    fn settings() -> WaveformEditSettings {
        WaveformEditSettings::new(
            2.0,
            Some(WaveformEditFade::new(0.25, 0.3, 0.0, 0.4).unwrap()),
            Some(WaveformEditFade::new(0.25, 0.7, 0.0, 0.6).unwrap()),
        )
        .unwrap()
    }

    fn near(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1.0e-6, "{actual} != {expected}");
    }

    #[test]
    fn inner_collisions_restore_baseline_effects_in_both_directions() {
        let range = region(0.2, 0.8);
        let original = settings();
        for (handle, extreme, restored) in [
            (WaveformEditFadeHandle::InEnd, 0.8, 0.35),
            (WaveformEditFadeHandle::OutStart, 0.2, 0.65),
        ] {
            let drag = WaveformEditFadeDrag::new(handle, range, original);
            let (range, collided) = drag.apply(range, original, extreme).unwrap();
            match handle {
                WaveformEditFadeHandle::InEnd => {
                    assert_eq!(collided.fade_in().unwrap().length(), 1.0);
                    assert!(collided.fade_out().is_none());
                }
                _ => {
                    assert_eq!(collided.fade_out().unwrap().length(), 1.0);
                    assert!(collided.fade_in().is_none());
                }
            }
            let (_, result) = drag.apply(range, collided, restored).unwrap();
            assert_eq!(result, original);
        }
    }

    #[test]
    fn selection_edge_handles_retain_inner_endpoint_gain_and_opposite_fade() {
        let range = region(0.25, 0.75);
        let original = settings();
        for (handle, pointer, expected_start, expected_end) in [
            (WaveformEditFadeHandle::InStart, 0.125, 0.125, 0.75),
            (WaveformEditFadeHandle::OutEnd, 0.875, 0.25, 0.875),
        ] {
            let drag = WaveformEditFadeDrag::new(handle, range, original);
            let (resized, result) = drag.apply(range, original, pointer).unwrap();
            assert_eq!(resized, region(expected_start, expected_end));
            assert_eq!(result.gain(), 2.0);
            let (selected, other) = if handle == WaveformEditFadeHandle::InStart {
                (result.fade_in().unwrap(), result.fade_out().unwrap())
            } else {
                (result.fade_out().unwrap(), result.fade_in().unwrap())
            };
            near(selected.length(), 0.4);
            near(other.length(), 0.2);
            assert_eq!(result.fade_in().unwrap().outer_gain(), 0.4);
            assert_eq!(result.fade_out().unwrap().outer_gain(), 0.6);
            // Repeated updates use the captured edge, not the previous resized result.
            assert_eq!(
                drag.apply(resized, result, pointer),
                Some((resized, result))
            );
        }
    }

    #[test]
    fn outer_handles_keep_geometry_and_clamp_to_audio() {
        let range = region(0.25, 0.75);
        let original = settings();
        for (handle, pointer) in [
            (WaveformEditFadeHandle::InOuterStart, -2.0),
            (WaveformEditFadeHandle::OutOuterEnd, 2.0),
        ] {
            let (same, result) = WaveformEditFadeDrag::new(handle, range, original)
                .apply(range, original, pointer)
                .unwrap();
            assert_eq!(same, range);
            assert_eq!(result.gain(), original.gain());
            let fade = if handle == WaveformEditFadeHandle::InOuterStart {
                result.fade_in().unwrap()
            } else {
                result.fade_out().unwrap()
            };
            assert_eq!(fade.mute(), 0.5);
            assert_eq!(fade.length(), 0.25);
        }
    }

    #[test]
    fn creation_uses_default_curve_and_outer_only_fades_survive() {
        let range = region(0.25, 0.75);
        let empty = WaveformEditSettings::new(1.5, None, None).unwrap();
        let (_, created) = WaveformEditFadeDrag::new(WaveformEditFadeHandle::InEnd, range, empty)
            .apply(range, empty, 0.5)
            .unwrap();
        assert_eq!(created.fade_in().unwrap().curve(), 0.5);
        assert_eq!(created.fade_in().unwrap().length(), 0.5);
        let outer = WaveformEditSettings::new(
            1.5,
            Some(WaveformEditFade::new(0.0, 0.6, 0.25, 0.8).unwrap()),
            None,
        )
        .unwrap();
        let (_, result) =
            WaveformEditFadeDrag::new(WaveformEditFadeHandle::InOuterStart, range, outer)
                .apply(range, outer, 0.0)
                .unwrap();
        assert_eq!(result.fade_in().unwrap().length(), 0.0);
        assert_eq!(result.fade_in().unwrap().mute(), 0.5);
        assert_eq!(result.fade_in().unwrap().outer_gain(), 0.8);
    }

    #[test]
    fn outer_endpoint_edge_snap_matches_native_resize() {
        let range = region(0.25, 0.75);
        let original = WaveformEditSettings::new(
            1.0,
            Some(WaveformEditFade::new(0.25, 0.5, 0.499999, 0.8).unwrap()),
            None,
        )
        .unwrap();
        let (resized, result) =
            WaveformEditFadeDrag::new(WaveformEditFadeHandle::InStart, range, original)
                .apply(range, original, 0.125)
                .unwrap();
        let endpoint =
            resized.start() - resized.width() * f64::from(result.fade_in().unwrap().mute());
        assert!(endpoint.abs() < 1.0e-8);
    }

    #[test]
    fn all_handles_reject_nonfinite_input_and_never_panic_outside_audio() {
        let handles = [
            WaveformEditFadeHandle::InEnd,
            WaveformEditFadeHandle::InStart,
            WaveformEditFadeHandle::InOuterStart,
            WaveformEditFadeHandle::OutStart,
            WaveformEditFadeHandle::OutEnd,
            WaveformEditFadeHandle::OutOuterEnd,
        ];
        for range in [
            region(-2.0, -1.0),
            region(2.0, 3.0),
            region(-0.5, 1.5),
            region(0.0, 1.0e-100),
            region(-f64::MAX / 2.0, f64::MAX / 2.0),
        ] {
            for handle in handles {
                let original = settings();
                let drag = WaveformEditFadeDrag::new(handle, range, original);
                assert_eq!(
                    drag.apply(range, original, f64::NAN),
                    Some((range, original))
                );
                assert_eq!(
                    drag.apply(range, original, f64::INFINITY),
                    Some((range, original))
                );
                for pointer in [-f64::MAX, 0.0, 0.5, 1.0, f64::MAX] {
                    if let Some((result_region, result)) = drag.apply(range, original, pointer) {
                        assert!(result_region.width().is_finite() && result_region.width() > 0.0);
                        assert_eq!(result.gain(), original.gain());
                        for fade in [result.fade_in(), result.fade_out()].into_iter().flatten() {
                            assert!(fade.length().is_finite() && fade.mute().is_finite());
                        }
                    }
                }
            }
        }
    }
}
