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
}
