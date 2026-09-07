//! GPUI-independent motion tokens and deterministic animation primitives.
//!
//! Motion values are sampled with caller-provided elapsed [`Duration`] values. The module does
//! not own a clock, spawn work, or request frames; a frontend can use [`OpacityTransition::needs_animation_at`]
//! to decide whether another sample is needed.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The lower alpha bound used by the worker progress pulse.
pub const WORKER_PULSE_MIN_ALPHA: u8 = 80;

/// The upper alpha bound used by the worker progress pulse.
pub const WORKER_PULSE_MAX_ALPHA: u8 = u8::MAX;

const MAX_MOTION_DURATION_MS: u32 = 10_000;
const MIN_MOTION_FREQUENCY_HZ: f32 = 0.01;
const MAX_MOTION_FREQUENCY_HZ: f32 = 60.0;

/// Theme-controlled motion settings.
///
/// The settings intentionally contain only timing and frequency values. Focus styling remains
/// owned by the interaction color tokens, including [`crate::Interaction::focus_outline`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Motion {
    /// Whether timed and periodic motion is enabled.
    pub enabled: bool,
    /// How long keyboard focus remains fully opaque before fading.
    pub keyboard_hold_ms: u32,
    /// Duration of the linear focus fade after its hold period.
    pub focus_fade_ms: u32,
    /// Duration of an overflow opacity transition.
    pub overflow_fade_ms: u32,
    /// Worker progress pulse frequency in cycles per second.
    pub worker_pulse_hz: f32,
    /// Source-processing sweep frequency in cycles per second.
    pub source_sweep_hz: f32,
}

impl Default for Motion {
    fn default() -> Self {
        Self {
            enabled: true,
            keyboard_hold_ms: 800,
            focus_fade_ms: 250,
            overflow_fade_ms: 260,
            worker_pulse_hz: 1.15,
            source_sweep_hz: 0.45,
        }
    }
}

impl Motion {
    /// Validate durations and frequencies for safe deterministic sampling.
    pub fn validate(&self) -> Result<(), String> {
        if self.keyboard_hold_ms > MAX_MOTION_DURATION_MS {
            return Err(format!(
                "motion.keyboard_hold_ms must be between 0 and {MAX_MOTION_DURATION_MS}"
            ));
        }
        if self.focus_fade_ms == 0 || self.focus_fade_ms > MAX_MOTION_DURATION_MS {
            return Err(format!(
                "motion.focus_fade_ms must be between 1 and {MAX_MOTION_DURATION_MS}"
            ));
        }
        if self.overflow_fade_ms == 0 || self.overflow_fade_ms > MAX_MOTION_DURATION_MS {
            return Err(format!(
                "motion.overflow_fade_ms must be between 1 and {MAX_MOTION_DURATION_MS}"
            ));
        }
        finite_frequency("motion.worker_pulse_hz", self.worker_pulse_hz)?;
        finite_frequency("motion.source_sweep_hz", self.source_sweep_hz)?;
        Ok(())
    }

    /// Return the configured keyboard focus hold duration.
    pub fn keyboard_hold(&self) -> Duration {
        Duration::from_millis(u64::from(self.keyboard_hold_ms))
    }

    /// Return the configured focus fade duration.
    pub fn focus_fade(&self) -> Duration {
        Duration::from_millis(u64::from(self.focus_fade_ms))
    }

    /// Return the configured overflow fade duration.
    pub fn overflow_fade(&self) -> Duration {
        Duration::from_millis(u64::from(self.overflow_fade_ms))
    }

    /// Create a focus envelope using this theme's timing and enabled state.
    pub fn focus_envelope(&self) -> FocusEnvelope {
        FocusEnvelope::with_enabled(self.keyboard_hold(), self.focus_fade(), self.enabled)
    }

    /// Create a pointer focus envelope using the configured fade and no hold period.
    pub fn pointer_focus_envelope(&self) -> FocusEnvelope {
        FocusEnvelope::with_enabled(Duration::ZERO, self.focus_fade(), self.enabled)
    }

    /// Create an overflow opacity transition using this theme's timing and enabled state.
    pub fn overflow_transition(&self, initial_opacity: f32) -> OpacityTransition {
        OpacityTransition::new(initial_opacity, self.overflow_fade(), self.enabled)
    }

    /// Sample the worker progress pulse at elapsed time.
    pub fn worker_pulse_alpha_at(&self, elapsed: Duration) -> u8 {
        if !self.enabled {
            WORKER_PULSE_MAX_ALPHA
        } else {
            pulse_alpha(
                elapsed,
                self.worker_pulse_hz,
                WORKER_PULSE_MIN_ALPHA,
                WORKER_PULSE_MAX_ALPHA,
            )
        }
    }

    /// Sample the source-processing sweep phase at elapsed time.
    pub fn source_sweep_phase_at(&self, elapsed: Duration) -> f32 {
        if self.enabled {
            sweep_phase(elapsed, self.source_sweep_hz)
        } else {
            0.0
        }
    }
}

fn finite_frequency(name: &str, value: f32) -> Result<(), String> {
    if !value.is_finite() || !(MIN_MOTION_FREQUENCY_HZ..=MAX_MOTION_FREQUENCY_HZ).contains(&value) {
        return Err(format!(
            "{name} must be finite and between {MIN_MOTION_FREQUENCY_HZ} and {MAX_MOTION_FREQUENCY_HZ}"
        ));
    }
    Ok(())
}

/// A focus indicator envelope with a full-opacity hold followed by a linear fade.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FocusEnvelope {
    hold: Duration,
    fade: Duration,
    enabled: bool,
}

/// Alias emphasizing that [`FocusEnvelope`] is sampled over elapsed time.
pub type TimedFocusEnvelope = FocusEnvelope;

impl FocusEnvelope {
    /// Create an enabled focus envelope.
    pub fn new(hold: Duration, fade: Duration) -> Self {
        Self::with_enabled(hold, fade, true)
    }

    /// Create a focus envelope with explicit reduced-motion behavior.
    pub fn with_enabled(hold: Duration, fade: Duration, enabled: bool) -> Self {
        Self {
            hold,
            fade,
            enabled,
        }
    }

    /// Sample focus alpha at elapsed time since the focus event.
    pub fn alpha_at(&self, elapsed: Duration) -> u8 {
        unit_to_alpha(self.value_at(elapsed))
    }

    /// Sample focus opacity in the inclusive `0.0..=1.0` range.
    pub fn value_at(&self, elapsed: Duration) -> f32 {
        if !self.enabled || elapsed <= self.hold {
            return 1.0;
        }
        if self.fade.is_zero() {
            return 0.0;
        }
        let fade_elapsed = elapsed.saturating_sub(self.hold);
        if fade_elapsed >= self.fade {
            return 0.0;
        }
        1.0 - fade_elapsed.as_secs_f32() / self.fade.as_secs_f32()
    }

    /// Whether this envelope needs another sample at elapsed time.
    ///
    /// The hold period is included so a caller that only requests frames while this returns
    /// `true` can naturally wake the fade without a separate timer. Once the full hold-and-fade
    /// lifetime has elapsed, this returns `false` and the caller can shut down its frame cadence.
    pub fn needs_animation_at(&self, elapsed: Duration) -> bool {
        self.enabled && !self.fade.is_zero() && elapsed < self.hold.saturating_add(self.fade)
    }

    /// Whether the envelope has reached its stable endpoint at elapsed time.
    pub fn is_settled_at(&self, elapsed: Duration) -> bool {
        !self.enabled || elapsed >= self.hold.saturating_add(self.fade)
    }
}

/// A retargetable opacity transition sampled with caller-provided monotonic time.
///
/// Retargeting samples the current value at the supplied time and uses that value as the next
/// transition's start. This preserves visual continuity when an overflow direction reverses
/// before the previous transition has settled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OpacityTransition {
    start: f32,
    target: f32,
    started_at: Duration,
    duration: Duration,
    enabled: bool,
}

impl OpacityTransition {
    /// Create a stable transition whose initial value is `initial_opacity`.
    pub fn new(initial_opacity: f32, duration: Duration, enabled: bool) -> Self {
        let initial_opacity = sanitize_opacity(initial_opacity);
        Self {
            start: initial_opacity,
            target: initial_opacity,
            started_at: Duration::ZERO,
            duration,
            enabled,
        }
    }

    /// Create a transition from a theme's overflow settings.
    pub fn from_motion(initial_opacity: f32, motion: &Motion) -> Self {
        motion.overflow_transition(initial_opacity)
    }

    /// Retarget the transition at monotonic time, preserving the sampled current value.
    pub fn retarget(&mut self, target_opacity: f32, now: Duration) {
        let target_opacity = sanitize_opacity(target_opacity);
        if self.target == target_opacity {
            return;
        }
        if !self.enabled || self.duration.is_zero() {
            self.start = target_opacity;
            self.target = target_opacity;
            self.started_at = now;
            return;
        }
        self.start = self.value_at(now);
        self.target = target_opacity;
        self.started_at = now;
    }

    /// Sample the interpolated opacity in the inclusive `0.0..=1.0` range.
    pub fn value_at(&self, now: Duration) -> f32 {
        if !self.enabled || self.start == self.target || self.duration.is_zero() {
            return self.target;
        }
        let elapsed = now.saturating_sub(self.started_at);
        if elapsed >= self.duration {
            return self.target;
        }
        let progress = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        self.start + (self.target - self.start) * smoothstep(progress)
    }

    /// Sample the transition as an 8-bit alpha value.
    pub fn alpha_at(&self, now: Duration) -> u8 {
        unit_to_alpha(self.value_at(now))
    }

    /// Return whether a caller should request another animation sample.
    pub fn needs_animation_at(&self, now: Duration) -> bool {
        self.enabled
            && self.start != self.target
            && !self.duration.is_zero()
            && now.saturating_sub(self.started_at) < self.duration
    }

    /// Return whether the transition has reached its stable endpoint.
    pub fn is_settled_at(&self, now: Duration) -> bool {
        !self.needs_animation_at(now)
    }

    /// Return the current target opacity.
    pub fn target(&self) -> f32 {
        self.target
    }
}

/// Return a clamped smoothstep interpolation value `t*t*(3-2*t)`.
pub fn smoothstep(progress: f32) -> f32 {
    let progress = if progress.is_finite() {
        progress.clamp(0.0, 1.0)
    } else if progress.is_sign_positive() {
        1.0
    } else {
        0.0
    };
    progress * progress * (3.0 - 2.0 * progress)
}

/// Return a bounded sinusoidal alpha pulse for elapsed time and frequency.
pub fn pulse_alpha(
    elapsed: Duration,
    frequency_hz: f32,
    minimum_alpha: u8,
    maximum_alpha: u8,
) -> u8 {
    let lower_alpha = minimum_alpha.min(maximum_alpha);
    let upper_alpha = minimum_alpha.max(maximum_alpha);
    let phase = sweep_phase(elapsed, frequency_hz);
    let normalized = (phase * std::f32::consts::TAU).sin() * 0.5 + 0.5;
    let span = f32::from(upper_alpha - lower_alpha);
    (f32::from(lower_alpha) + span * normalized)
        .round()
        .clamp(f32::from(lower_alpha), f32::from(upper_alpha)) as u8
}

/// Return a bounded periodic phase in the inclusive `0.0..1.0` range.
pub fn sweep_phase(elapsed: Duration, frequency_hz: f32) -> f32 {
    let frequency_hz = if frequency_hz.is_finite() && frequency_hz > 0.0 {
        f64::from(frequency_hz)
    } else {
        0.0
    };
    if frequency_hz == 0.0 {
        return 0.0;
    }
    let seconds = elapsed.as_secs() as f64;
    let fractional_seconds = f64::from(elapsed.subsec_nanos()) / 1_000_000_000.0;
    let cycles = (seconds.rem_euclid(1_000_000.0) + fractional_seconds) * frequency_hz;
    cycles.rem_euclid(1.0) as f32
}

fn sanitize_opacity(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else if value.is_sign_positive() {
        1.0
    } else {
        0.0
    }
}

fn unit_to_alpha(value: f32) -> u8 {
    (sanitize_opacity(value) * f32::from(u8::MAX))
        .round()
        .clamp(0.0, f32::from(u8::MAX)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_holds_then_fades_deterministically() {
        let envelope = FocusEnvelope::new(Duration::from_millis(800), Duration::from_millis(250));

        assert_eq!(envelope.alpha_at(Duration::from_millis(800)), u8::MAX);
        assert_eq!(envelope.alpha_at(Duration::from_millis(925)), 128);
        assert_eq!(envelope.alpha_at(Duration::from_millis(1_050)), 0);
        assert!(envelope.needs_animation_at(Duration::ZERO));
        assert!(envelope.needs_animation_at(Duration::from_millis(800)));
        assert!(envelope.needs_animation_at(Duration::from_millis(900)));
        assert!(!envelope.needs_animation_at(Duration::from_millis(1_050)));
        assert!(!envelope.is_settled_at(Duration::ZERO));
        assert!(envelope.is_settled_at(Duration::from_millis(1_050)));
    }

    #[test]
    fn pointer_focus_uses_zero_hold() {
        let envelope = FocusEnvelope::new(Duration::ZERO, Duration::from_millis(250));

        assert_eq!(envelope.alpha_at(Duration::ZERO), u8::MAX);
        assert_eq!(envelope.alpha_at(Duration::from_millis(125)), 128);
    }

    #[test]
    fn rapid_retargeting_starts_from_the_current_value() {
        let mut transition = OpacityTransition::new(0.0, Duration::from_millis(260), true);
        transition.retarget(1.0, Duration::ZERO);
        let before_reversal = transition.value_at(Duration::from_millis(130));
        assert!((before_reversal - 0.5).abs() < 0.0001);

        transition.retarget(0.0, Duration::from_millis(130));
        assert!((transition.value_at(Duration::from_millis(130)) - before_reversal).abs() < 0.0001);
        assert_eq!(transition.value_at(Duration::from_millis(390)), 0.0);
        assert!(!transition.needs_animation_at(Duration::from_millis(390)));
    }

    #[test]
    fn disabled_motion_is_stable_and_needs_no_animation() {
        let envelope = FocusEnvelope::with_enabled(
            Duration::from_millis(800),
            Duration::from_millis(250),
            false,
        );
        assert_eq!(envelope.alpha_at(Duration::ZERO), u8::MAX);
        assert_eq!(envelope.alpha_at(Duration::from_secs(10)), u8::MAX);
        assert!(!envelope.needs_animation_at(Duration::from_secs(10)));

        let mut transition = OpacityTransition::new(0.0, Duration::from_millis(260), false);
        transition.retarget(1.0, Duration::ZERO);
        assert_eq!(transition.value_at(Duration::ZERO), 1.0);
        assert!(!transition.needs_animation_at(Duration::ZERO));
        assert_eq!(Motion::default().source_sweep_phase_at(Duration::ZERO), 0.0);
    }

    #[test]
    fn pulse_and_sweep_are_bounded_and_periodic() {
        let motion = Motion::default();
        let pulse = motion.worker_pulse_alpha_at(Duration::from_millis(173));
        assert!((WORKER_PULSE_MIN_ALPHA..=WORKER_PULSE_MAX_ALPHA).contains(&pulse));
        let period = Duration::from_secs_f32(1.0 / motion.source_sweep_hz);
        let first = motion.source_sweep_phase_at(Duration::from_millis(17));
        let after_period = motion.source_sweep_phase_at(Duration::from_millis(17) + period);
        assert!((first - after_period).abs() < 0.0001);
        assert!((0.0..1.0).contains(&first));
    }

    #[test]
    fn motion_validation_rejects_invalid_duration_and_frequency() {
        let motion = Motion {
            overflow_fade_ms: 0,
            ..Motion::default()
        };
        assert!(motion.validate().is_err());
        let motion = Motion {
            worker_pulse_hz: f32::NAN,
            ..Motion::default()
        };
        assert!(motion.validate().is_err());
        let motion = Motion {
            source_sweep_hz: 0.0,
            ..Motion::default()
        };
        assert!(motion.validate().is_err());
    }
}
