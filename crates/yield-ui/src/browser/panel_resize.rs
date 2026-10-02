//! Shared presentation-only resize calculations for bounded panel gestures.

/// Clamp a requested size, retaining a usable panel when pointer input is invalid.
pub(super) fn bounded_size(requested: f32, minimum: f32, maximum: f32) -> f32 {
    if requested.is_finite() {
        requested.clamp(minimum, maximum)
    } else {
        minimum
    }
}

/// Axis-independent drag origin and exact pre-drag preference for cancellation.
#[derive(Clone, Copy, Debug)]
pub(super) struct ResizeDrag {
    origin: f32,
    size: f32,
    previous_preference: Option<f32>,
}

impl ResizeDrag {
    pub(super) fn begin(origin: f32, size: f32, previous_preference: Option<f32>) -> Self {
        Self {
            origin,
            size,
            previous_preference,
        }
    }

    pub(super) fn requested_size(self, coordinate: f32) -> f32 {
        let requested = self.size + coordinate - self.origin;
        if requested.is_finite() {
            requested
        } else {
            self.size
        }
    }

    pub(super) fn previous_preference(self) -> Option<f32> {
        self.previous_preference
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overshoot_reversal_and_invalid_input_keep_a_bounded_panel() {
        let drag = ResizeDrag::begin(200.0, 240.0, Some(300.0));
        assert_eq!(
            bounded_size(drag.requested_size(900.0), 180.0, 420.0),
            420.0
        );
        // Reversal uses the drag origin, not the previous clamped sample.
        assert_eq!(
            bounded_size(drag.requested_size(190.0), 180.0, 420.0),
            230.0
        );
        assert_eq!(
            bounded_size(drag.requested_size(-900.0), 180.0, 420.0),
            180.0
        );
        assert_eq!(drag.requested_size(f32::NAN), 240.0);
        assert_eq!(drag.previous_preference(), Some(300.0));
        assert_eq!(bounded_size(f32::INFINITY, 96.0, 120.0), 96.0);
    }
}
