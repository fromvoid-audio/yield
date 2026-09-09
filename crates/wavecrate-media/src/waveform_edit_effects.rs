//! Renderer-neutral in-place waveform edit effects.

use std::sync::atomic::AtomicBool;

use wavecrate_core::{WaveformEditFade, WaveformEditSettings};

use crate::is_cancelled;

const MIN_MUTE_FADE_SECS: f32 = 0.002;
const CANCELLATION_CHECK_SAMPLES: usize = 4_096;

#[derive(Clone, Copy)]
enum FadeRampDirection {
    Down,
    Up,
}

/// Apply gain and optional native edit fades to an interleaved sample buffer.
///
/// `start_frame..end_frame` is clamped independently to the complete frames in `samples`.
/// Channels default to one when zero is supplied, and a zero sample rate uses one frame per
/// second for the native minimum-mute calculation. The operation changes only the caller-owned
/// buffer; it does not read or write files or publish a result.
pub fn apply_waveform_edit_effects(
    samples: &mut [f32],
    channels: usize,
    sample_rate: u32,
    start_frame: usize,
    end_frame: usize,
    settings: WaveformEditSettings,
) -> Result<(), String> {
    apply_waveform_edit_effects_with_cancel(
        samples,
        channels,
        sample_rate,
        start_frame,
        end_frame,
        settings,
        None,
    )
}

/// Apply gain and optional native edit fades while honoring an optional cancellation flag.
///
/// Cancellation is checked before mutation, at least every 4,096 changed interleaved samples,
/// and after the final operation. Because the transform works in place on a caller-owned buffer,
/// a cancellation error may leave a partial mutation; callers must discard that buffer rather
/// than publish it. This function performs no publication itself.
pub fn apply_waveform_edit_effects_with_cancel(
    samples: &mut [f32],
    channels: usize,
    sample_rate: u32,
    start_frame: usize,
    end_frame: usize,
    settings: WaveformEditSettings,
    cancellation: Option<&AtomicBool>,
) -> Result<(), String> {
    apply_waveform_edit_effects_inner(
        samples,
        channels,
        sample_rate,
        start_frame,
        end_frame,
        settings,
        || is_cancelled(cancellation),
    )
}

fn apply_waveform_edit_effects_inner<F>(
    samples: &mut [f32],
    channels: usize,
    sample_rate: u32,
    start_frame: usize,
    end_frame: usize,
    settings: WaveformEditSettings,
    mut cancellation: F,
) -> Result<(), String>
where
    F: FnMut() -> bool,
{
    if cancellation() {
        return Err(canceled_error());
    }

    let channels = channels.max(1);
    let total_frames = samples.len() / channels;
    let (start_frame, end_frame) = clamped_frame_span(total_frames, start_frame, end_frame);
    if end_frame <= start_frame {
        return Ok(());
    }

    let selection_frames = end_frame - start_frame;
    let min_fade_frames = ((sample_rate.max(1) as f32) * MIN_MUTE_FADE_SECS)
        .round()
        .max(1.0) as usize;
    let mut budget = CancellationBudget::new(&mut cancellation);

    apply_selection_gain(
        samples,
        channels,
        start_frame,
        end_frame,
        settings.gain(),
        &mut budget,
    )?;
    apply_fade_in_effect(
        samples,
        channels,
        start_frame,
        end_frame,
        selection_frames,
        min_fade_frames,
        settings.fade_in(),
        &mut budget,
    )?;
    apply_fade_out_effect(
        samples,
        channels,
        FadeOutEffect {
            start_frame,
            end_frame,
            total_frames,
            selection_frames,
            min_fade_frames,
            fade: settings.fade_out(),
        },
        &mut budget,
    )?;

    if budget.is_canceled() {
        Err(canceled_error())
    } else {
        Ok(())
    }
}

struct CancellationBudget<'a, F> {
    cancellation: &'a mut F,
    samples_since_check: usize,
}

impl<'a, F> CancellationBudget<'a, F>
where
    F: FnMut() -> bool,
{
    fn new(cancellation: &'a mut F) -> Self {
        Self {
            cancellation,
            samples_since_check: 0,
        }
    }

    fn before_sample(&mut self) -> bool {
        if self.samples_since_check < CANCELLATION_CHECK_SAMPLES {
            return false;
        }
        self.samples_since_check = 0;
        (self.cancellation)()
    }

    fn record_sample(&mut self) {
        self.samples_since_check += 1;
    }

    fn is_canceled(&mut self) -> bool {
        (self.cancellation)()
    }
}

fn apply_selection_gain<F>(
    samples: &mut [f32],
    channels: usize,
    start_frame: usize,
    end_frame: usize,
    gain: f32,
    budget: &mut CancellationBudget<'_, F>,
) -> Result<(), String>
where
    F: FnMut() -> bool,
{
    if (gain - 1.0).abs() <= f32::EPSILON {
        return Ok(());
    }
    for frame in start_frame..end_frame {
        let base = frame * channels;
        for channel in 0..channels {
            if budget.before_sample() {
                return Err(canceled_error());
            }
            if let Some(sample) = samples.get_mut(base + channel) {
                *sample *= gain;
                budget.record_sample();
            }
        }
    }
    Ok(())
}

fn apply_fade_in_effect<F>(
    samples: &mut [f32],
    channels: usize,
    start_frame: usize,
    end_frame: usize,
    selection_frames: usize,
    min_fade_frames: usize,
    fade: Option<WaveformEditFade>,
    budget: &mut CancellationBudget<'_, F>,
) -> Result<(), String>
where
    F: FnMut() -> bool,
{
    let Some(fade) = fade else {
        return Ok(());
    };
    let fade_frames = fade_frame_count(selection_frames, min_fade_frames, fade);
    let mute_frames = ((selection_frames as f32) * fade.mute()).round().max(0.0) as usize;
    if mute_frames > 0 {
        let mute_start = start_frame.saturating_sub(mute_frames);
        if mute_start < start_frame {
            apply_scaled_fade_ramp(
                samples,
                channels,
                mute_start,
                start_frame,
                FadeRampDirection::Down,
                fade.curve(),
                fade.outer_gain(),
                budget,
            )?;
        }
    }
    let fade_end = start_frame.saturating_add(fade_frames).min(end_frame);
    if fade_end > start_frame {
        apply_scaled_fade_ramp(
            samples,
            channels,
            start_frame,
            fade_end,
            FadeRampDirection::Up,
            fade.curve(),
            1.0,
            budget,
        )?;
    }
    Ok(())
}

struct FadeOutEffect {
    start_frame: usize,
    end_frame: usize,
    total_frames: usize,
    selection_frames: usize,
    min_fade_frames: usize,
    fade: Option<WaveformEditFade>,
}

fn apply_fade_out_effect<F>(
    samples: &mut [f32],
    channels: usize,
    effect: FadeOutEffect,
    budget: &mut CancellationBudget<'_, F>,
) -> Result<(), String>
where
    F: FnMut() -> bool,
{
    let Some(fade) = effect.fade else {
        return Ok(());
    };
    let fade_frames = fade_frame_count(effect.selection_frames, effect.min_fade_frames, fade);
    let mute_frames = ((effect.selection_frames as f32) * fade.mute())
        .round()
        .max(0.0) as usize;
    if mute_frames > 0 {
        let mute_end = effect
            .end_frame
            .saturating_add(mute_frames)
            .min(effect.total_frames);
        if effect.end_frame < mute_end {
            apply_scaled_fade_ramp(
                samples,
                channels,
                effect.end_frame,
                mute_end,
                FadeRampDirection::Up,
                fade.curve(),
                fade.outer_gain(),
                budget,
            )?;
        }
    }
    let fade_start = effect
        .end_frame
        .saturating_sub(fade_frames)
        .max(effect.start_frame);
    if fade_start < effect.end_frame {
        apply_scaled_fade_ramp(
            samples,
            channels,
            fade_start,
            effect.end_frame,
            FadeRampDirection::Down,
            fade.curve(),
            1.0,
            budget,
        )?;
    }
    Ok(())
}

fn apply_scaled_fade_ramp<F>(
    samples: &mut [f32],
    channels: usize,
    start_frame: usize,
    end_frame: usize,
    direction: FadeRampDirection,
    curve: f32,
    scale: f32,
    budget: &mut CancellationBudget<'_, F>,
) -> Result<(), String>
where
    F: FnMut() -> bool,
{
    let frame_count = end_frame.saturating_sub(start_frame);
    if frame_count == 0 {
        return Ok(());
    }
    let denom = frame_count.saturating_sub(1).max(1) as f32;
    let scale = scale.clamp(0.0, 1.0);
    for i in 0..frame_count {
        let progress = i as f32 / denom;
        let factor = fade_factor(frame_count, progress, direction, curve) * scale;
        let frame = start_frame + i;
        let base = frame * channels;
        for channel in 0..channels {
            if budget.before_sample() {
                return Err(canceled_error());
            }
            if let Some(sample) = samples.get_mut(base + channel) {
                *sample *= factor;
                budget.record_sample();
            }
        }
    }
    Ok(())
}

fn fade_factor(frame_count: usize, progress: f32, direction: FadeRampDirection, curve: f32) -> f32 {
    if frame_count == 1 {
        return 0.0;
    }
    let eased = fade_curve_value(progress, curve);
    let factor = match direction {
        FadeRampDirection::Down => 1.0 - eased,
        FadeRampDirection::Up => eased,
    };
    factor.clamp(0.0, 1.0)
}

fn fade_curve_value(t: f32, curve: f32) -> f32 {
    if curve <= 0.0 {
        return t;
    }
    let t = t.clamp(0.0, 1.0);
    let t2 = t * t;
    let t3 = t2 * t;
    let smootherstep = t3 * (t * (t * 6.0 - 15.0) + 10.0);
    t * (1.0 - curve) + smootherstep * curve
}

fn fade_frame_count(
    selection_frames: usize,
    min_fade_frames: usize,
    fade: WaveformEditFade,
) -> usize {
    let base_fade_frames = ((selection_frames as f32) * fade.length())
        .round()
        .clamp(0.0, selection_frames as f32) as usize;
    if base_fade_frames > 0 {
        base_fade_frames
    } else if fade.mute() > 0.0 {
        min_fade_frames.min(selection_frames)
    } else {
        0
    }
}

fn clamped_frame_span(total_frames: usize, start_frame: usize, end_frame: usize) -> (usize, usize) {
    (start_frame.min(total_frames), end_frame.min(total_frames))
}

fn canceled_error() -> String {
    String::from("waveform edit effects canceled")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use wavecrate_core::{WaveformEditFade, WaveformEditSettings};

    use super::{
        apply_waveform_edit_effects, apply_waveform_edit_effects_inner,
        apply_waveform_edit_effects_with_cancel,
    };

    fn fade(length: f32, curve: f32, mute: f32, outer_gain: f32) -> WaveformEditFade {
        WaveformEditFade::new(length, curve, mute, outer_gain).unwrap()
    }

    fn settings(
        gain: f32,
        fade_in: Option<WaveformEditFade>,
        fade_out: Option<WaveformEditFade>,
    ) -> WaveformEditSettings {
        WaveformEditSettings::new(gain, fade_in, fade_out).unwrap()
    }

    #[test]
    fn stereo_gain_only_changes_selected_complete_frames() {
        let mut samples = [0.25, -0.5, 1.0, -1.0, 0.5, -0.25, 2.0, -2.0];
        apply_waveform_edit_effects(&mut samples, 2, 48_000, 1, 3, settings(2.0, None, None))
            .unwrap();
        assert_eq!(samples, [0.25, -0.5, 2.0, -2.0, 1.0, -0.5, 2.0, -2.0]);
    }

    #[test]
    fn gain_is_applied_before_the_inner_fade() {
        let mut samples = [0.25; 4];
        apply_waveform_edit_effects(
            &mut samples,
            1,
            48_000,
            0,
            4,
            settings(2.0, Some(fade(1.0, 0.0, 0.0, 1.0)), None),
        )
        .unwrap();
        assert_eq!(samples[0], 0.0);
        assert_eq!(samples[1], (0.25_f32 * 2.0) * (1.0 / 3.0));
        assert_eq!(samples[2], (0.25_f32 * 2.0) * (2.0 / 3.0));
        assert_eq!(samples[3], 0.5);
    }

    #[test]
    fn linear_fade_in_and_out_match_native_frame_ramps() {
        let mut samples = [1.0; 10];
        let linear = fade(0.3, 0.0, 0.0, 1.0);
        apply_waveform_edit_effects(
            &mut samples,
            1,
            48_000,
            0,
            10,
            settings(1.0, Some(linear), Some(linear)),
        )
        .unwrap();
        assert_eq!(samples, [0.0, 0.5, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.5, 0.0]);
    }

    #[test]
    fn s_curve_fade_matches_native_smootherstep() {
        let mut samples = [1.0; 5];
        apply_waveform_edit_effects(
            &mut samples,
            1,
            48_000,
            0,
            5,
            settings(1.0, Some(fade(1.0, 1.0, 0.0, 1.0)), None),
        )
        .unwrap();
        assert_eq!(samples, [0.0, 0.103515625, 0.5, 0.8964844, 1.0]);
    }

    #[test]
    fn outer_mute_uses_the_native_two_millisecond_minimum_fade() {
        let mut samples = [1.0; 8];
        let fade = fade(0.0, 0.0, 0.5, 0.5);
        apply_waveform_edit_effects(
            &mut samples,
            1,
            1_000,
            2,
            6,
            settings(1.0, Some(fade), Some(fade)),
        )
        .unwrap();
        assert_eq!(samples, [0.5, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.5]);
    }

    #[test]
    fn frame_span_clamps_without_synthesizing_or_touching_trailing_samples() {
        let mut samples = [1.0, -1.0, 0.5, -0.5, 0.25];
        apply_waveform_edit_effects(
            &mut samples,
            2,
            48_000,
            1,
            usize::MAX,
            settings(2.0, None, None),
        )
        .unwrap();
        assert_eq!(samples, [1.0, -1.0, 1.0, -1.0, 0.25]);

        apply_waveform_edit_effects(
            &mut samples,
            2,
            48_000,
            usize::MAX,
            usize::MAX,
            settings(0.0, None, None),
        )
        .unwrap();
        assert_eq!(samples, [1.0, -1.0, 1.0, -1.0, 0.25]);
    }

    #[test]
    fn single_frame_fades_zero_every_channel() {
        let mut samples = [0.5, -0.5];
        let linear = fade(1.0, 0.0, 0.0, 1.0);
        apply_waveform_edit_effects(
            &mut samples,
            2,
            48_000,
            0,
            1,
            settings(1.0, Some(linear), Some(linear)),
        )
        .unwrap();
        assert_eq!(samples, [0.0, -0.0]);
    }

    #[test]
    fn invalid_parameters_are_rejected_before_buffer_mutation() {
        let samples_before = [0.25, -0.5];
        let samples = samples_before;
        assert!(WaveformEditFade::new(f32::NAN, 0.0, 0.0, 1.0).is_none());
        assert!(WaveformEditFade::new(0.0, f32::INFINITY, 0.0, 1.0).is_none());
        assert!(WaveformEditSettings::new(f32::NEG_INFINITY, None, None).is_none());
        assert_eq!(samples, samples_before);
    }

    #[test]
    fn already_canceled_operation_is_a_noop() {
        let cancellation = AtomicBool::new(true);
        let mut samples = [0.25, -0.5, 1.0, -1.0];
        let before = samples;
        let result = apply_waveform_edit_effects_with_cancel(
            &mut samples,
            2,
            48_000,
            0,
            2,
            settings(2.0, None, None),
            Some(&cancellation),
        );
        assert_eq!(result, Err(String::from("waveform edit effects canceled")));
        assert_eq!(samples, before);
    }

    #[test]
    fn cancellation_can_leave_only_a_bounded_partial_mutation() {
        let mut samples = vec![1.0; 8_192];
        let settings = settings(2.0, None, None);
        let end_frame = samples.len();
        let mut checks = 0;
        let result = apply_waveform_edit_effects_inner(
            &mut samples,
            1,
            48_000,
            0,
            end_frame,
            settings,
            || {
                checks += 1;
                checks >= 2
            },
        );
        assert_eq!(result, Err(String::from("waveform edit effects canceled")));
        assert!(samples[..4_096].iter().all(|sample| *sample == 2.0));
        assert!(samples[4_096..].iter().all(|sample| *sample == 1.0));
    }
}
