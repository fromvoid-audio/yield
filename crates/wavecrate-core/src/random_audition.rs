#![deny(missing_docs)]
#![deny(warnings)]

//! Renderer-neutral normalized ranges for random sample audition.

const MIN_RANDOM_AUDITION_SECONDS: f32 = 0.25;

/// Deterministic normalized units used to choose a random audition span.
///
/// The caller supplies independent values for the span's start position and length. The values
/// are clamped by [`random_audition_span_for_units`] so a caller can feed random or otherwise
/// unbounded units without needing to duplicate the range math.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RandomAuditionUnits {
    start: f32,
    length: f32,
}

impl RandomAuditionUnits {
    /// Construct deterministic normalized units for a random audition span.
    pub const fn new(start: f32, length: f32) -> Self {
        Self { start, length }
    }
}

/// Calculate a normalized random audition span for deterministic units.
///
/// Auditions are at least 250 milliseconds long when the duration allows it. Invalid or shorter
/// durations return the whole sample. For valid longer durations, the start unit is clamped to
/// `0.0..=1.0` after reserving the minimum span width, and the length unit is clamped to
/// `0.0..=1.0` before interpolating between that minimum and the end of the file.
///
/// The arithmetic and `f32` handling intentionally match the native random audition helper. In
/// particular, nonfinite units are passed through the float clamp operation, so infinities clamp
/// to the corresponding endpoint while NaN units produce NaN coordinates.
pub fn random_audition_span_for_units(
    duration_seconds: f32,
    units: RandomAuditionUnits,
) -> (f32, f32) {
    if !duration_seconds.is_finite() || duration_seconds <= MIN_RANDOM_AUDITION_SECONDS {
        return (0.0, 1.0);
    }

    let min_width = (MIN_RANDOM_AUDITION_SECONDS / duration_seconds).clamp(0.0, 1.0);
    if min_width >= 1.0 {
        return (0.0, 1.0);
    }

    let max_start = 1.0 - min_width;
    let start = units.start.clamp(0.0, 1.0) * max_start;
    let max_width = 1.0 - start;
    let width = min_width + units.length.clamp(0.0, 1.0) * (max_width - min_width);
    (start, start + width)
}

#[cfg(test)]
mod tests {
    use super::{RandomAuditionUnits, random_audition_span_for_units};

    #[test]
    fn random_audition_span_matches_native_endpoint_math() {
        assert_eq!(
            random_audition_span_for_units(20.0, RandomAuditionUnits::new(0.0, 0.0)),
            (0.0, 0.0125)
        );

        let near_end = random_audition_span_for_units(20.0, RandomAuditionUnits::new(1.0, 0.0));
        assert!((near_end.0 - 0.9875).abs() < 0.001);
        assert!((near_end.1 - 1.0).abs() < 0.001);

        let middle = random_audition_span_for_units(10.0, RandomAuditionUnits::new(0.25, 0.5));
        assert!((middle.0 - 0.24375).abs() < 0.001);
        assert!((middle.1 - 0.634375).abs() < 0.001);
    }

    #[test]
    fn random_audition_span_uses_whole_sample_for_tiny_or_invalid_duration() {
        for duration in [0.0, 0.1, 0.25, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(
                random_audition_span_for_units(duration, RandomAuditionUnits::new(0.75, 0.5)),
                (0.0, 1.0),
                "duration was {duration:?}"
            );
        }
    }

    #[test]
    fn random_audition_span_clamps_finite_units_and_preserves_width_bounds() {
        let duration = 10.0;
        let min_width = 0.25 / duration;
        for start_unit in [-1.0, 0.0, 0.5, 1.0, 2.0] {
            for length_unit in [-1.0, 0.0, 0.5, 1.0, 2.0] {
                let (start, end) = random_audition_span_for_units(
                    duration,
                    RandomAuditionUnits::new(start_unit, length_unit),
                );
                let width = end - start;
                assert!((0.0..=1.0).contains(&start));
                assert!((0.0..=1.0).contains(&end));
                assert!(width + 1.0e-6 >= min_width);
                assert!(width <= 1.0 - start + 1.0e-6);
            }
        }
    }

    #[test]
    fn random_audition_span_preserves_native_nonfinite_unit_handling() {
        let positive_infinity =
            random_audition_span_for_units(10.0, RandomAuditionUnits::new(f32::INFINITY, 0.0));
        assert!((positive_infinity.0 - 0.975).abs() < 0.001);
        assert_eq!(positive_infinity.1, 1.0);

        let negative_infinity =
            random_audition_span_for_units(10.0, RandomAuditionUnits::new(0.0, f32::NEG_INFINITY));
        assert_eq!(negative_infinity, (0.0, 0.025));

        let nan_start =
            random_audition_span_for_units(10.0, RandomAuditionUnits::new(f32::NAN, 0.5));
        assert!(nan_start.0.is_nan());
        assert!(nan_start.1.is_nan());

        let nan_length =
            random_audition_span_for_units(10.0, RandomAuditionUnits::new(0.5, f32::NAN));
        assert!(nan_length.0.is_finite());
        assert!(nan_length.1.is_nan());
    }
}
