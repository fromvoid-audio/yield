#![deny(missing_docs)]
#![deny(warnings)]

//! Renderer-neutral parameters for destructive waveform edit effects.

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
}

#[cfg(test)]
mod tests {
    use super::{WaveformEditFade, WaveformEditSettings};

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
}
