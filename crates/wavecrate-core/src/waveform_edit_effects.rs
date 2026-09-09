#![deny(missing_docs)]
#![deny(warnings)]

//! Renderer-neutral parameters for destructive waveform edit effects.

use crate::WaveformEditRegion;

/// Parameters for one waveform edit fade.
///
/// Values are finite and normalized to the ranges used by the native edit renderer. The `mute`
/// value is an outward extension measured as a fraction of the selected span, and `outer_gain`
/// is the gain at the far edge of that extension.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveformEditFade {
    length: f32,
    curve: f32,
    mute: f32,
    outer_gain: f32,
}

impl WaveformEditFade {
    /// Construct finite fade parameters using the native parameter ranges.
    ///
    /// `length`, `curve`, and `outer_gain` are clamped to `0.0..=1.0`; negative `mute` values
    /// become zero. Any non-finite input is rejected before a caller can apply the effect.
    pub fn new(length: f32, curve: f32, mute: f32, outer_gain: f32) -> Option<Self> {
        if !length.is_finite() || !curve.is_finite() || !mute.is_finite() || !outer_gain.is_finite()
        {
            return None;
        }
        Some(Self {
            length: length.clamp(0.0, 1.0),
            curve: curve.clamp(0.0, 1.0),
            mute: mute.max(0.0),
            outer_gain: outer_gain.clamp(0.0, 1.0),
        })
    }

    /// Return the fraction of the selected span used by the inner fade.
    pub const fn length(self) -> f32 {
        self.length
    }

    /// Return the fade curve amount.
    pub const fn curve(self) -> f32 {
        self.curve
    }

    /// Return the outward extension fraction.
    pub const fn mute(self) -> f32 {
        self.mute
    }

    /// Return the gain at the far edge of the outward extension.
    pub const fn outer_gain(self) -> f32 {
        self.outer_gain
    }

    /// Update the outer gain from a finite vertical handle ratio.
    ///
    /// A ratio of zero maps to unity, a ratio of `0.5` maps to silence, and the result is
    /// clamped to `0.0..=1.0`. A non-finite ratio restores unity. Length, curve, and mute are
    /// retained unchanged.
    pub fn with_outer_gain_for_vertical_ratio(self, vertical_ratio: f32) -> Self {
        let outer_gain = if !vertical_ratio.is_finite() {
            1.0
        } else {
            (1.0 - (vertical_ratio / 0.5)).clamp(0.0, 1.0)
        };
        Self { outer_gain, ..self }
    }
}

/// Finite gain and optional fades for one waveform edit selection.
///
/// The selection geometry is supplied separately by the media operation, so this value can be
/// reused while a renderer-neutral edit region moves or resizes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveformEditSettings {
    gain: f32,
    fade_in: Option<WaveformEditFade>,
    fade_out: Option<WaveformEditFade>,
}

impl WaveformEditSettings {
    const BOOST_PIXELS: f32 = 120.0;
    const ATTENUATE_PIXELS: f32 = 120.0;

    /// Construct finite edit settings using the native gain range.
    ///
    /// Gain is clamped to `0.0..=4.0`. Non-finite gain is rejected. Fade values must have been
    /// created by [`WaveformEditFade::new`], which rejects non-finite fade parameters before
    /// mutation.
    pub fn new(
        gain: f32,
        fade_in: Option<WaveformEditFade>,
        fade_out: Option<WaveformEditFade>,
    ) -> Option<Self> {
        gain.is_finite().then_some(Self {
            gain: gain.clamp(0.0, 4.0),
            fade_in,
            fade_out,
        })
    }

    /// Return the gain applied inside the selection.
    pub const fn gain(self) -> f32 {
        self.gain
    }

    /// Return the optional fade-in parameters.
    pub const fn fade_in(self) -> Option<WaveformEditFade> {
        self.fade_in
    }

    /// Return the optional fade-out parameters.
    pub const fn fade_out(self) -> Option<WaveformEditFade> {
        self.fade_out
    }

    /// Update gain from a pointer-y drag relative to an unchanged baseline.
    ///
    /// Moving upward increases gain by three units per 120 pixels; moving downward decreases it
    /// by one unit per 120 pixels. The result is clamped to the native `0.0..=4.0` range. A
    /// non-finite anchor or pointer returns this baseline unchanged, so each provisional update
    /// can be derived from the same captured settings without accumulating deltas. Fades are
    /// retained unchanged.
    pub fn with_gain_for_pointer_y(self, anchor_y: f32, pointer_y: f32) -> Self {
        let gain = if !pointer_y.is_finite() || !anchor_y.is_finite() {
            self.gain
        } else {
            let delta_y = pointer_y - anchor_y;
            if delta_y < 0.0 {
                self.gain + (-delta_y / Self::BOOST_PIXELS) * 3.0
            } else {
                self.gain - (delta_y / Self::ATTENUATE_PIXELS)
            }
        };
        Self {
            gain: gain.clamp(0.0, 4.0),
            ..self
        }
    }

    /// Preserve edit effects while changing the bounds of a region.
    ///
    /// Inner fade lengths are preserved in absolute units when possible, and outward mute
    /// endpoints are preserved within the native audio-bound limits. The fade-in is applied
    /// before the fade-out, so overlapping inner fades retain the native setter's precedence:
    /// the fade-out is shortened to the remaining span. Each inner length is rounded to six
    /// decimal places, matching the native selection setters. Gain, curves, and outer gains are
    /// retained unchanged. A non-representable intermediate length or mute endpoint falls back
    /// to zero for that field, preserving finite settings. Callers moving a whole region can
    /// retain `self` directly when relative settings should move with the region instead of
    /// preserving absolute endpoints.
    pub fn for_resized_region(
        self,
        baseline: WaveformEditRegion,
        resized: WaveformEditRegion,
    ) -> Self {
        let source_width = baseline.width();
        let resized_width = resized.width();
        let mut settings = Self {
            fade_in: None,
            fade_out: None,
            ..self
        };

        if !source_width.is_finite()
            || source_width <= 0.0
            || !resized_width.is_finite()
            || resized_width <= 0.0
        {
            return settings;
        }

        if let Some(fade_in) = self.fade_in {
            settings = settings.with_resized_fade_in(
                fade_in,
                preserved_inner_fade_length(source_width, fade_in.length, resized_width),
                preserved_fade_in_mute(baseline, resized, fade_in.mute),
            );
        }
        if let Some(fade_out) = self.fade_out {
            settings = settings.with_resized_fade_out(
                fade_out,
                preserved_inner_fade_length(source_width, fade_out.length, resized_width),
                preserved_fade_out_mute(baseline, resized, fade_out.mute),
            );
        }
        settings
    }

    fn with_resized_fade_in(self, source: WaveformEditFade, length: f32, mute: f32) -> Self {
        let length = clamped_fade_length(length, self.fade_out.map_or(0.0, |fade| fade.length));
        let mute = finite_nonnegative(mute);
        let fade_in = (length > 0.0 || mute > 0.0).then_some(WaveformEditFade {
            length,
            curve: source.curve,
            mute,
            outer_gain: source.outer_gain,
        });
        Self { fade_in, ..self }
    }

    fn with_resized_fade_out(self, source: WaveformEditFade, length: f32, mute: f32) -> Self {
        let length = clamped_fade_length(length, self.fade_in.map_or(0.0, |fade| fade.length));
        let mute = finite_nonnegative(mute);
        let fade_out = (length > 0.0 || mute > 0.0).then_some(WaveformEditFade {
            length,
            curve: source.curve,
            mute,
            outer_gain: source.outer_gain,
        });
        Self { fade_out, ..self }
    }
}

fn preserved_inner_fade_length(source_width: f64, length: f32, resized_width: f64) -> f32 {
    let Some(absolute_length) = finite_product(source_width, f64::from(length)) else {
        return 0.0;
    };
    let Some(relative_length) = finite_quotient(absolute_length, resized_width) else {
        return 0.0;
    };
    finite_f32(relative_length.clamp(0.0, 1.0)).unwrap_or(0.0)
}

fn preserved_fade_in_mute(
    baseline: WaveformEditRegion,
    resized: WaveformEditRegion,
    mute: f32,
) -> f32 {
    if mute <= 0.0 {
        return 0.0;
    }
    let Some(outer_offset) = finite_product(baseline.width(), f64::from(mute)) else {
        return 0.0;
    };
    let Some(source_outer_start) = finite_difference(baseline.start(), outer_offset) else {
        return 0.0;
    };
    let Some(numerator) = finite_difference(resized.start(), source_outer_start) else {
        return 0.0;
    };
    let Some(relative_mute) = finite_quotient(numerator, resized.width()) else {
        return 0.0;
    };
    bounded_mute(relative_mute, resized.start() / resized.width())
}

fn preserved_fade_out_mute(
    baseline: WaveformEditRegion,
    resized: WaveformEditRegion,
    mute: f32,
) -> f32 {
    if mute <= 0.0 {
        return 0.0;
    }
    let Some(outer_offset) = finite_product(baseline.width(), f64::from(mute)) else {
        return 0.0;
    };
    let Some(source_outer_end) = finite_sum(baseline.end(), outer_offset) else {
        return 0.0;
    };
    let Some(numerator) = finite_difference(source_outer_end, resized.end()) else {
        return 0.0;
    };
    let Some(relative_mute) = finite_quotient(numerator, resized.width()) else {
        return 0.0;
    };
    let Some(distance_to_audio_end) = finite_difference(1.0, resized.end()) else {
        return 0.0;
    };
    bounded_mute(relative_mute, distance_to_audio_end / resized.width())
}

fn bounded_mute(relative_mute: f64, max_mute: f64) -> f32 {
    if !relative_mute.is_finite() {
        return 0.0;
    }
    let relative_mute = relative_mute.max(0.0);
    let clamped_mute = relative_mute.min(max_mute.max(0.0));
    finite_f32(clamped_mute).unwrap_or(0.0)
}

fn clamped_fade_length(length: f32, other_length: f32) -> f32 {
    let clamped = length.clamp(0.0, 1.0);
    let other = f64::from(other_length).clamp(0.0, 1.0);
    let max_allowed = (1.0 - other).max(0.0) as f32;
    round_fade_length(clamped.min(max_allowed))
}

fn round_fade_length(value: f32) -> f32 {
    let scale = 1_000_000.0;
    (value * scale).round() / scale
}

fn finite_nonnegative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn finite_product(left: f64, right: f64) -> Option<f64> {
    let value = left * right;
    value.is_finite().then_some(value)
}

fn finite_sum(left: f64, right: f64) -> Option<f64> {
    let value = left + right;
    value.is_finite().then_some(value)
}

fn finite_difference(left: f64, right: f64) -> Option<f64> {
    let value = left - right;
    value.is_finite().then_some(value)
}

fn finite_quotient(numerator: f64, denominator: f64) -> Option<f64> {
    let value = numerator / denominator;
    value.is_finite().then_some(value)
}

fn finite_f32(value: f64) -> Option<f32> {
    if !value.is_finite() || value < f64::from(f32::MIN) || value > f64::from(f32::MAX) {
        return None;
    }
    let value = value as f32;
    value.is_finite().then_some(value)
}

#[cfg(test)]
mod tests {
    use super::{WaveformEditFade, WaveformEditSettings};
    use crate::WaveformEditRegion;

    #[test]
    fn fade_parameters_clamp_native_ranges() {
        let fade = WaveformEditFade::new(-1.0, 2.0, -3.0, 4.0).unwrap();
        assert_eq!(fade.length(), 0.0);
        assert_eq!(fade.curve(), 1.0);
        assert_eq!(fade.mute(), 0.0);
        assert_eq!(fade.outer_gain(), 1.0);
    }

    #[test]
    fn nonfinite_fade_parameters_are_rejected() {
        assert_eq!(WaveformEditFade::new(f32::NAN, 0.0, 0.0, 1.0), None);
        assert_eq!(WaveformEditFade::new(0.0, f32::INFINITY, 0.0, 1.0), None);
        assert_eq!(
            WaveformEditFade::new(0.0, 0.0, f32::NEG_INFINITY, 1.0),
            None
        );
        assert_eq!(WaveformEditFade::new(0.0, 0.0, 0.0, f32::NAN), None);
    }

    #[test]
    fn settings_clamp_gain_and_reject_nonfinite_gain() {
        assert_eq!(
            WaveformEditSettings::new(5.0, None, None).unwrap().gain(),
            4.0
        );
        assert_eq!(WaveformEditSettings::new(f32::NAN, None, None), None);
        assert_eq!(WaveformEditSettings::new(f32::INFINITY, None, None), None);
    }

    #[test]
    fn outer_gain_vertical_ratio_preserves_other_fade_parameters() {
        let fade = WaveformEditFade::new(0.25, 0.75, 0.5, 0.4).unwrap();
        let expected = WaveformEditFade::new(0.25, 0.75, 0.5, 1.0).unwrap();
        assert_eq!(fade.with_outer_gain_for_vertical_ratio(0.0), expected);
        assert_eq!(
            fade.with_outer_gain_for_vertical_ratio(0.25).outer_gain(),
            0.5
        );
        assert_eq!(
            fade.with_outer_gain_for_vertical_ratio(0.5).outer_gain(),
            0.0
        );
        assert_eq!(
            fade.with_outer_gain_for_vertical_ratio(2.0).outer_gain(),
            0.0
        );
        assert_eq!(
            fade.with_outer_gain_for_vertical_ratio(-1.0).outer_gain(),
            1.0
        );
        assert_eq!(fade.with_outer_gain_for_vertical_ratio(f32::NAN), expected);
        assert_eq!(
            fade.with_outer_gain_for_vertical_ratio(f32::INFINITY),
            expected
        );
    }

    #[test]
    fn gain_drag_is_asymmetric_and_uses_one_unchanged_baseline() {
        let fade_in = WaveformEditFade::new(0.2, 0.3, 0.4, 0.5).unwrap();
        let fade_out = WaveformEditFade::new(0.6, 0.7, 0.8, 0.9).unwrap();
        let baseline = WaveformEditSettings::new(1.0, Some(fade_in), Some(fade_out)).unwrap();

        let upward = baseline.with_gain_for_pointer_y(100.0, 60.0);
        assert!((upward.gain() - 2.0).abs() < 1.0e-6);
        assert_eq!(upward.fade_in(), Some(fade_in));
        assert_eq!(upward.fade_out(), Some(fade_out));

        let downward = baseline.with_gain_for_pointer_y(100.0, 140.0);
        assert!((downward.gain() - (1.0 - 40.0 / 120.0)).abs() < 1.0e-6);
        let second_update = baseline.with_gain_for_pointer_y(100.0, 200.0);
        assert!((second_update.gain() - (1.0 - 100.0 / 120.0)).abs() < 1.0e-6);
        assert_eq!(baseline.gain(), 1.0);
    }

    #[test]
    fn gain_drag_clamps_finite_extremes_and_ignores_nonfinite_coordinates() {
        let fade = WaveformEditFade::new(0.2, 0.3, 0.4, 0.5).unwrap();
        let baseline = WaveformEditSettings::new(1.0, Some(fade), None).unwrap();
        assert_eq!(baseline.with_gain_for_pointer_y(0.0, f32::MAX).gain(), 0.0);
        assert_eq!(
            baseline.with_gain_for_pointer_y(0.0, f32::NEG_INFINITY),
            baseline
        );
        assert_eq!(baseline.with_gain_for_pointer_y(f32::NAN, 0.0), baseline);

        let boosted = baseline.with_gain_for_pointer_y(f32::MAX, -f32::MAX);
        assert_eq!(boosted.gain(), 4.0);
        assert_eq!(boosted.fade_in(), Some(fade));
    }

    #[test]
    fn resize_preserves_absolute_fades_and_edit_parameters() {
        let fade_in = WaveformEditFade::new(0.25, 0.3, 0.1, 0.4).unwrap();
        let fade_out = WaveformEditFade::new(0.4, 0.7, 0.1, 0.6).unwrap();
        let settings = WaveformEditSettings::new(2.5, Some(fade_in), Some(fade_out)).unwrap();
        let baseline = WaveformEditRegion::new(0.3, 0.7).unwrap();
        let resized = WaveformEditRegion::new(0.28, 0.72).unwrap();

        let preserved = settings.for_resized_region(baseline, resized);
        let preserved_in = preserved.fade_in().unwrap();
        let preserved_out = preserved.fade_out().unwrap();

        assert_eq!(preserved.gain(), 2.5);
        assert_eq!(preserved_in.curve(), fade_in.curve());
        assert_eq!(preserved_in.outer_gain(), fade_in.outer_gain());
        assert_eq!(preserved_out.curve(), fade_out.curve());
        assert_eq!(preserved_out.outer_gain(), fade_out.outer_gain());
        assert_close(preserved_in.length(), 0.22727273);
        assert_close(preserved_out.length(), 0.36363637);
        assert_close(preserved_in.mute(), 0.045454547);
        assert_close(preserved_out.mute(), 0.045454547);

        let source_outer_start = baseline.start() - baseline.width() * 0.1;
        let source_outer_end = baseline.end() + baseline.width() * 0.1;
        let preserved_outer_start =
            resized.start() - resized.width() * f64::from(preserved_in.mute());
        let preserved_outer_end = resized.end() + resized.width() * f64::from(preserved_out.mute());
        assert!((preserved_outer_start - source_outer_start).abs() < 1.0e-6);
        assert!((preserved_outer_end - source_outer_end).abs() < 1.0e-6);
        assert_eq!(settings.fade_in(), Some(fade_in));
        assert_eq!(settings.fade_out(), Some(fade_out));
    }

    #[test]
    fn resize_clips_outer_endpoints_to_native_audio_bounds() {
        let fade_in = WaveformEditFade::new(0.2, 0.3, 0.5, 0.4).unwrap();
        let fade_out = WaveformEditFade::new(0.2, 0.7, 0.5, 0.6).unwrap();
        let settings = WaveformEditSettings::new(1.0, Some(fade_in), Some(fade_out)).unwrap();

        let resized = settings.for_resized_region(
            WaveformEditRegion::new(0.2, 0.8).unwrap(),
            WaveformEditRegion::new(0.1, 0.9).unwrap(),
        );
        assert_close(resized.fade_in().unwrap().mute(), 0.125);
        assert_close(resized.fade_out().unwrap().mute(), 0.125);

        let resized = settings.for_resized_region(
            WaveformEditRegion::new(0.2, 0.8).unwrap(),
            WaveformEditRegion::new(-0.05, 0.9).unwrap(),
        );
        assert_eq!(resized.fade_in().unwrap().mute(), 0.0);

        let resized = settings.for_resized_region(
            WaveformEditRegion::new(0.2, 0.8).unwrap(),
            WaveformEditRegion::new(0.1, 1.05).unwrap(),
        );
        assert_eq!(resized.fade_out().unwrap().mute(), 0.0);
    }

    #[test]
    fn resize_shrinking_overlapping_fades_uses_native_order_and_rounding() {
        let fade_in = WaveformEditFade::new(0.5, 0.2, 0.0, 0.3).unwrap();
        let fade_out = WaveformEditFade::new(0.5, 0.8, 0.0, 0.7).unwrap();
        let settings = WaveformEditSettings::new(1.5, Some(fade_in), Some(fade_out)).unwrap();
        let baseline = WaveformEditRegion::new(0.0, 1.0).unwrap();
        let resized = WaveformEditRegion::new(0.125, 0.875).unwrap();

        let preserved = settings.for_resized_region(baseline, resized);
        assert_eq!(preserved.fade_in().unwrap().length(), 0.666667);
        assert_eq!(preserved.fade_out().unwrap().length(), 0.333333);
        assert_eq!(preserved.fade_in().unwrap().curve(), fade_in.curve());
        assert_eq!(preserved.fade_out().unwrap().curve(), fade_out.curve());
        assert_eq!(
            preserved.fade_in().unwrap().outer_gain(),
            fade_in.outer_gain()
        );
        assert_eq!(
            preserved.fade_out().unwrap().outer_gain(),
            fade_out.outer_gain()
        );
    }

    #[test]
    fn resize_crossing_bounds_uses_ordered_region_geometry() {
        let fade_in = WaveformEditFade::new(0.25, 0.1, 0.0, 0.8).unwrap();
        let fade_out = WaveformEditFade::new(0.25, 0.9, 0.0, 0.2).unwrap();
        let settings = WaveformEditSettings::new(0.75, Some(fade_in), Some(fade_out)).unwrap();
        let baseline = WaveformEditRegion::new(0.25, 0.75).unwrap();
        let resized = WaveformEditRegion::new(0.9, 0.1).unwrap();

        let preserved = settings.for_resized_region(baseline, resized);
        assert_eq!(resized.start(), 0.1);
        assert_eq!(resized.end(), 0.9);
        assert_close(preserved.fade_in().unwrap().length(), 0.15625);
        assert_close(preserved.fade_out().unwrap().length(), 0.15625);
    }

    #[test]
    fn resize_removes_empty_fades_but_preserves_outer_only_fades() {
        let empty = WaveformEditFade::new(0.0, 0.2, 0.0, 0.4).unwrap();
        let outer_only = WaveformEditFade::new(0.0, 0.8, 0.1, 0.3).unwrap();
        let settings = WaveformEditSettings::new(1.25, Some(empty), Some(outer_only)).unwrap();
        let baseline = WaveformEditRegion::new(0.2, 0.8).unwrap();
        let resized = WaveformEditRegion::new(0.3, 0.7).unwrap();

        let preserved = settings.for_resized_region(baseline, resized);
        assert_eq!(preserved.fade_in(), None);
        let fade_out = preserved.fade_out().unwrap();
        assert_eq!(fade_out.length(), 0.0);
        assert_close(fade_out.mute(), 0.4);
        assert_eq!(fade_out.curve(), outer_only.curve());
        assert_eq!(fade_out.outer_gain(), outer_only.outer_gain());
    }

    #[test]
    fn resize_extremes_zero_only_nonrepresentable_fields() {
        let baseline = WaveformEditRegion::new(0.0, f64::MAX).unwrap();
        let fade = WaveformEditFade::new(0.5, 0.4, f32::MAX, 0.6).unwrap();
        let settings = WaveformEditSettings::new(3.0, Some(fade), None).unwrap();

        let preserved = settings.for_resized_region(baseline, baseline);
        let preserved_fade = preserved.fade_in().unwrap();
        assert_eq!(preserved_fade.length(), 0.5);
        assert_eq!(preserved_fade.mute(), 0.0);
        assert!(preserved_fade.length().is_finite());
        assert!(preserved_fade.mute().is_finite());

        let tiny_region = WaveformEditRegion::new(0.0, 1.0e-100).unwrap();
        let length_overflow = WaveformEditSettings::new(
            3.0,
            Some(WaveformEditFade::new(0.5, 0.4, 0.0, 0.6).unwrap()),
            None,
        )
        .unwrap();
        assert_eq!(
            length_overflow
                .for_resized_region(baseline, tiny_region)
                .fade_in(),
            None
        );

        let mute_unrepresentable = WaveformEditSettings::new(
            3.0,
            Some(WaveformEditFade::new(0.25, 0.4, 1.0, 0.6).unwrap()),
            None,
        )
        .unwrap();
        let mute_result = mute_unrepresentable
            .for_resized_region(WaveformEditRegion::new(0.0, 1.0).unwrap(), tiny_region);
        let mute_fade = mute_result.fade_in().unwrap();
        assert_eq!(mute_fade.length(), 1.0);
        assert_eq!(mute_fade.mute(), 0.0);
        assert!(mute_fade.length().is_finite());
        assert!(mute_fade.mute().is_finite());
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1.0e-6);
    }
}
