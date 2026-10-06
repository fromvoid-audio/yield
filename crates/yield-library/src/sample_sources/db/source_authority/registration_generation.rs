//! Checked original registration serials; copied numbers confer no ownership or grant.
use super::SourceAuthorityError;
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) static NEXT_REGISTRATION_GENERATION: AtomicU64 = AtomicU64::new(1);

/// The serial is process-local and meaningful only with the original UUID/acquisition/custody.
/// The exhausted sentinel never wraps or restarts; failed construction may burn a serial.
pub(super) fn issue(counter: &AtomicU64) -> Result<u64, SourceAuthorityError> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            if value == 0 || value > i64::MAX as u64 {
                None
            } else {
                value.checked_add(1)
            }
        })
        .map_err(|_| SourceAuthorityError::RegistrationGenerationExhausted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_registration_generation_exhaustion_is_sticky_and_storage_bounded() {
        let last = AtomicU64::new(i64::MAX as u64);
        assert_eq!(issue(&last).unwrap(), i64::MAX as u64);
        for _ in 0..3 {
            assert!(matches!(
                issue(&last),
                Err(SourceAuthorityError::RegistrationGenerationExhausted)
            ));
            assert_eq!(last.load(Ordering::Relaxed), i64::MAX as u64 + 1);
        }
        for value in [0, i64::MAX as u64 + 1, u64::MAX] {
            let invalid = AtomicU64::new(value);
            for _ in 0..2 {
                assert!(issue(&invalid).is_err());
                assert_eq!(invalid.load(Ordering::Relaxed), value);
            }
        }
    }

    #[test]
    fn source_registration_generation_concurrent_issuance_never_reuses_a_serial() {
        let counter = AtomicU64::new(1);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        (0..32)
                            .map(|_| issue(&counter).unwrap())
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            let mut serials: Vec<_> = handles
                .into_iter()
                .flat_map(|handle| handle.join().unwrap())
                .collect();
            serials.sort_unstable();
            assert_eq!(serials, (1..=256).collect::<Vec<_>>());
            assert_eq!(counter.load(Ordering::Relaxed), 257);
        });
    }
}
