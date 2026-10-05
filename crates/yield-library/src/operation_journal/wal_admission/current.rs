//! Original parked-worker validation replies; these never fence cleanup or admit settlement.
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

pub(crate) struct CurrentRound {
    deadline: Instant,
    expected: u64,
    acknowledged: Mutex<u64>,
    _credit: super::HandshakeCredit,
}
pub(crate) struct CurrentRequest {
    round: Arc<CurrentRound>,
    bit: u64,
}
impl CurrentRound {
    pub(super) fn new(
        count: usize,
        deadline: Instant,
        credit: super::HandshakeCredit,
    ) -> Option<Arc<Self>> {
        if count == 0 || count > 64 {
            return None;
        }
        Some(Arc::new(Self {
            deadline,
            expected: u64::MAX >> (64 - count),
            acknowledged: Mutex::new(0),
            _credit: credit,
        }))
    }
    pub(super) fn request(self: &Arc<Self>, index: usize) -> CurrentRequest {
        let bit = 1u64
            .checked_shl(index as u32)
            .expect("bounded original inventory");
        assert_ne!(self.expected & bit, 0);
        CurrentRequest {
            round: Arc::clone(self),
            bit,
        }
    }
    pub(super) fn complete(&self) -> bool {
        Instant::now() < self.deadline
            && self
                .acknowledged
                .lock()
                .is_ok_and(|mask| *mask == self.expected)
    }
}
impl CurrentRequest {
    /// Called only after the actual original worker validates its retained native custody.
    pub(crate) fn acknowledge(self) {
        if Instant::now() < self.round.deadline
            && let Ok(mut mask) = self.round.acknowledged.lock()
        {
            *mask |= self.bit;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn current_round_requires_every_original_slot_and_keeps_credit_with_worker_request() {
        let budget = super::super::HandshakeBudget::new();
        let mut credit = budget.credit();
        credit.resize(128).unwrap();
        let round = CurrentRound::new(64, Instant::now() + Duration::from_secs(2), credit).unwrap();
        for index in 0..63 {
            round.request(index).acknowledge();
        }
        round.request(0).acknowledge();
        assert!(
            !round.complete(),
            "duplicate cannot replace missing last owner"
        );
        let last = round.request(63);
        drop(round);
        assert_eq!(budget.used(), 128, "worker retains original round debit");
        last.acknowledge();
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn current_round_expiry_and_poison_never_publish_complete() {
        let budget = super::super::HandshakeBudget::new();
        let expired = CurrentRound::new(1, Instant::now(), budget.credit()).unwrap();
        expired.request(0).acknowledge();
        assert!(!expired.complete());
        let round =
            CurrentRound::new(1, Instant::now() + Duration::from_secs(2), budget.credit()).unwrap();
        let cloned = Arc::clone(&round);
        assert!(
            std::thread::spawn(move || {
                let _guard = cloned.acknowledged.lock().unwrap();
                panic!("negative poison fault");
            })
            .join()
            .is_err()
        );
        round.request(0).acknowledge();
        assert!(!round.complete());
        assert!(CurrentRound::new(65, Instant::now(), budget.credit()).is_none());
    }
}
