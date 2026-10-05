//! One original owner round. Journal decisions and native cleanup compete under a short lock.
use std::{
    sync::{Arc, Condvar, Mutex},
    time::Instant,
};

pub(crate) struct CurrentRound {
    deadline: Instant,
    expected: u64,
    acknowledged: Mutex<RoundState>,
    resolved: Condvar,
    nonce: uuid::Uuid,
    _credit: super::HandshakeCredit,
}
struct RoundState {
    slots: u64,
    invalidated: bool,
    decision: Decision,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Decision {
    Preparing,
    Deciding,
    Resolved,
}
/// Sole journal-side decision custody. Drop before SQLite starts is a definite refusal;
/// Drop after start leaves the original native owners unresolved until exact readback.
pub(in crate::operation_journal) struct OriginalDecision {
    round: Arc<CurrentRound>,
    sqlite_started: bool,
}
impl OriginalDecision {
    pub(in crate::operation_journal) fn sqlite_started(&self) -> bool {
        self.sqlite_started
    }
    pub(in crate::operation_journal) fn nonce(&self) -> uuid::Uuid {
        self.round.nonce
    }
    pub(in crate::operation_journal) fn acknowledged(&self) -> bool {
        self.round.acknowledged.lock().is_ok_and(|state| {
            state.decision == Decision::Deciding
                && !state.invalidated
                && state.slots == self.round.expected
                && Instant::now() < self.round.deadline
        })
    }
    #[cfg(test)]
    pub(in crate::operation_journal) fn start_sqlite(&mut self) -> bool {
        let Ok(state) = self.round.acknowledged.lock() else {
            return false;
        };
        if state.decision != Decision::Deciding
            || state.invalidated
            || state.slots != self.round.expected
            || Instant::now() >= self.round.deadline
        {
            return false;
        }
        self.sqlite_started = true;
        true
    }
    /// Caller must have a definite initial transaction outcome or exact durable readback.
    pub(in crate::operation_journal) fn resolve(&self) {
        let mut state = self
            .round
            .acknowledged
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        state.decision = Decision::Resolved;
        self.round.resolved.notify_all();
    }
}
impl Drop for OriginalDecision {
    fn drop(&mut self) {
        if !self.sqlite_started {
            self.resolve();
        }
    }
}
/// Only an actual parked worker retains this non-Clone acknowledgement custody.
pub(crate) struct CurrentCustody(Arc<CurrentRound>);
#[cfg(all(test, target_os = "linux"))]
impl CurrentCustody {
    pub(crate) fn original_round_for_tests(&self) -> Arc<CurrentRound> {
        Arc::clone(&self.0)
    }
}
#[cfg(all(test, target_os = "linux"))]
impl CurrentRound {
    pub(crate) fn invalidated_for_tests(&self) -> bool {
        self.invalidated()
    }
}
impl Drop for CurrentCustody {
    fn drop(&mut self) {
        let mut state = self
            .0
            .acknowledged
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Ordinary owner loops poll without waiting. This destruction fence is only the
        // last-resort unwind boundary: Native fields must not drop beneath in-flight SQL.
        while matches!(state.decision, Decision::Deciding) {
            state = self
                .0
                .resolved
                .wait(state)
                .unwrap_or_else(|p| p.into_inner());
        }
        state.invalidated = true;
    }
}
impl CurrentCustody {
    pub(crate) fn settlement_held(&self) -> bool {
        self.0
            .acknowledged
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .decision
            .is_unresolved()
    }
    /// Claim cleanup before touching SQLite, competing atomically with journal Start.
    pub(crate) fn begin_cleanup(&self) -> bool {
        let mut state = self
            .0
            .acknowledged
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if state.decision.is_unresolved() {
            return false;
        }
        state.invalidated = true;
        true
    }
}
impl Decision {
    fn is_unresolved(self) -> bool {
        matches!(self, Self::Deciding)
    }
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
            acknowledged: Mutex::new(RoundState {
                slots: 0,
                invalidated: false,
                decision: Decision::Preparing,
            }),
            resolved: Condvar::new(),
            nonce: uuid::Uuid::new_v4(),
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
    pub(super) fn invalidated(&self) -> bool {
        self.acknowledged
            .lock()
            .map_or(true, |state| state.invalidated)
    }
    pub(super) fn complete(&self) -> bool {
        self.with_complete(|| ()).is_some()
    }
    /// Start a new sealed commit-acknowledgement round. The sole journal-side guard
    /// resolves NoAttempt on every pre-SQL refusal, including partial acknowledgement.
    pub(in crate::operation_journal) fn decision_round(
        count: usize,
        deadline: Instant,
        credit: super::HandshakeCredit,
    ) -> Option<(Arc<Self>, OriginalDecision)> {
        let round = Self::new(count, deadline, credit)?;
        round.acknowledged.lock().ok()?.decision = Decision::Deciding;
        let decision = OriginalDecision {
            round: Arc::clone(&round),
            sqlite_started: false,
        };
        Some((round, decision))
    }
    /// Scalar publication competes with original worker cleanup under this same mutex.
    /// Never run native I/O, SQLite or owner waits inside this closure.
    pub(super) fn with_complete<T>(&self, publish: impl FnOnce() -> T) -> Option<T> {
        let state = self.acknowledged.lock().ok()?;
        if Instant::now() >= self.deadline
            || state.invalidated
            || state.slots != self.expected
            || state.decision != Decision::Preparing
        {
            return None;
        }
        Some(publish())
    }
}
impl CurrentRequest {
    /// Original worker installs returned custody before it can clean up its native writer.
    pub(crate) fn acknowledge(self) -> CurrentCustody {
        if Instant::now() < self.round.deadline
            && let Ok(mut state) = self.round.acknowledged.lock()
            && !state.invalidated
            && matches!(state.decision, Decision::Preparing | Decision::Deciding)
        {
            state.slots |= self.bit;
        }
        CurrentCustody(self.round)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn current_round_no_attempt_resolves_partial_ack_without_late_revival() {
        let budget = super::super::HandshakeBudget::new();
        let mut credit = budget.credit();
        credit.resize(128).unwrap();
        let (round, decision) =
            CurrentRound::decision_round(2, Instant::now() + Duration::from_secs(2), credit)
                .unwrap();
        let first = round.request(0).acknowledge();
        let late = round.request(1);
        assert!(!decision.acknowledged());
        assert!(first.settlement_held());
        assert!(!first.begin_cleanup());
        drop(decision); // Definite NoAttempt, before any journal transaction.
        assert!(first.begin_cleanup());
        let late = late.acknowledge();
        assert!(!late.settlement_held());
        assert!(!round.complete());
        drop(round);
        assert_eq!(budget.used(), 128);
        drop(first);
        drop(late);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn current_round_commit_ack_is_separate_from_historical_ready() {
        let budget = super::super::HandshakeBudget::new();
        let (round, decision) = CurrentRound::decision_round(
            1,
            Instant::now() + Duration::from_secs(2),
            budget.credit(),
        )
        .unwrap();
        let custody = round.request(0).acknowledge();
        assert!(decision.acknowledged());
        assert!(
            !round.complete(),
            "a commit ACK must not republish historical Ready"
        );
        assert!(!custody.begin_cleanup());
        assert!(!round.invalidated());
        drop(decision);
        assert!(custody.begin_cleanup());
    }

    #[test]
    fn current_round_original_global_worker_holds_and_invalidates_before_release_delivery() {
        use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
        use crate::sample_sources::library::{
            GlobalLibraryWriter, GlobalLibraryWriterStatus, GlobalPreparationRequest,
            LIBRARY_DB_FILE_NAME,
        };
        let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
        let temp = tempfile::tempdir().unwrap();
        let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
        let _profile = PersistenceProfileGuard::named("current-global-custody");
        let profile = WritableProfileGuard::acquire_current().unwrap();
        let mut writer = GlobalLibraryWriter::start(&profile).unwrap();
        let startup = Instant::now() + Duration::from_secs(5);
        while writer.status() != GlobalLibraryWriterStatus::Available {
            assert!(Instant::now() < startup);
            std::thread::sleep(Duration::from_millis(1));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let request =
            GlobalPreparationRequest::new(writer.preparation_owner(), [9; 32], deadline, 1, 128)
                .unwrap();
        let holder = writer
            .submit_preparation(request)
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        let budget = super::super::HandshakeBudget::new();
        let mut credit = budget.credit();
        credit.resize(128).unwrap();
        let round = CurrentRound::new(1, deadline, credit).unwrap();
        holder.request_current(round.request(0)).unwrap();
        while !round.complete() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let outside =
            rusqlite::Connection::open(profile.profile_root().join(LIBRARY_DB_FILE_NAME)).unwrap();
        outside.busy_timeout(Duration::ZERO).unwrap();
        assert_eq!(
            outside
                .execute_batch("BEGIN IMMEDIATE")
                .unwrap_err()
                .sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy)
        );
        let release = holder.abort();
        release
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        assert!(round.invalidated());
        assert!(!round.complete());
        let late = round.request(0).acknowledge();
        assert!(
            !round.complete(),
            "late reply cannot revive original cleanup"
        );
        outside.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        drop(late);
        drop(round);
        assert_eq!(budget.used(), 0);
        assert!(writer.shutdown());
    }

    #[test]
    fn current_round_requires_every_original_slot_and_keeps_credit_with_worker_request() {
        let budget = super::super::HandshakeBudget::new();
        let mut credit = budget.credit();
        credit.resize(128).unwrap();
        let round = CurrentRound::new(64, Instant::now() + Duration::from_secs(2), credit).unwrap();
        let mut workers = Vec::new();
        for index in 0..63 {
            workers.push(round.request(index).acknowledge());
        }
        workers.push(round.request(0).acknowledge());
        assert!(
            !round.complete(),
            "duplicate cannot replace missing last owner"
        );
        let last = round.request(63);
        drop(round);
        assert_eq!(budget.used(), 128, "worker retains original round debit");
        let custody = last.acknowledge();
        assert!(custody.0.complete());
        drop(workers);
        assert!(
            !custody.0.complete(),
            "original worker departure invalidates the full round"
        );
        assert_eq!(budget.used(), 128);
        drop(custody);
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
