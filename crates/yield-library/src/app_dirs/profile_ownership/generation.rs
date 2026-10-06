//! Process-local serials for genuine successful native profile-lock acquisitions.

use std::sync::atomic::{AtomicU64, Ordering};

use super::ProfileOwnershipError;

pub(super) static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

pub(super) fn issue_from(counter: &AtomicU64) -> Result<u64, ProfileOwnershipError> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            validate(value).ok()?;
            value.checked_add(1)
        })
        .map_err(|_| ProfileOwnershipError::OwnershipGenerationExhausted)
}

pub(super) fn validate(value: u64) -> Result<(), ProfileOwnershipError> {
    if value == 0 || value > i64::MAX as u64 {
        Err(ProfileOwnershipError::OwnershipGenerationExhausted)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
