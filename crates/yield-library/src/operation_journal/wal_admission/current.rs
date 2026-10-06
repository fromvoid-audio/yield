//! One original owner round. Journal decisions and native cleanup compete under a short lock.
use std::{
    sync::{Arc, Condvar, Mutex},
    time::Instant,
};

#[derive(Clone, Copy)]
enum BoundaryRefusal {
    Changed,
    Busy,
    Revoked,
}

type SourceBoundaries = [Option<crate::sample_sources::db::RegisteredDecisionBoundary>; 64];

pub(crate) struct CurrentRound {
    deadline: Instant,
    expected: u64,
    acknowledged: Mutex<RoundState>,
    resolved: Condvar,
    nonce: uuid::Uuid,
    #[cfg(all(test, target_os = "linux"))]
    snapshot_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    _credit: super::HandshakeCredit,
}
/// Exact Ready publication provenance; never current effect or SQL authority.
pub(in crate::operation_journal) struct OriginalReadyRound {
    round: Arc<CurrentRound>,
    cohort: u64,
}
const _: () = assert!(std::mem::size_of::<OriginalReadyRound>() <= 16);
impl OriginalReadyRound {
    pub(super) fn new(round: Arc<CurrentRound>, cohort: u64) -> Self {
        Self { round, cohort }
    }
    fn matches_ready(&self, original: &CurrentRound, state: &RoundState) -> bool {
        state.cohort == self.cohort
            && !state.invalidated
            && state.slots == original.expected
            && original.sources_present(state)
            && state.decision == Decision::Preparing
            && Instant::now() < original.deadline
    }
    /// Provenance permits frozen-policy comparison, never current native admission.
    pub(super) fn require_provenance(
        &self,
        original: &Arc<CurrentRound>,
    ) -> Result<(), super::WalPreparationError> {
        if !Arc::ptr_eq(&self.round, original) {
            return Err(super::WalPreparationError::InvalidInput);
        }
        let state = original
            .acknowledged
            .lock()
            .map_err(|_| super::WalPreparationError::InvalidInput)?;
        if !self.matches_ready(original, &state) {
            return Err(super::WalPreparationError::InvalidInput);
        }
        Ok(())
    }
    pub(super) fn require_current(
        &self,
        original: &Arc<CurrentRound>,
    ) -> Result<(), super::WalPreparationError> {
        use super::WalPreparationError;
        // The scalar round lock is released before trying any original native gate.
        self.require_provenance(original)?;
        match original.with_boundaries(|state| self.matches_ready(original, state)) {
            Ok(true) => Ok(()),
            Err(BoundaryRefusal::Busy) => Err(WalPreparationError::Busy),
            _ => Err(WalPreparationError::InvalidInput),
        }
    }
}
struct RoundState {
    slots: u64,
    invalidated: bool,
    decision: Decision,
    sources: [Option<uuid::Uuid>; 64],
    boundaries: SourceBoundaries,
    cohort: u64,
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
        self.round
            .with_boundaries(|state| {
                state.decision == Decision::Deciding
                    && !state.invalidated
                    && state.slots == self.round.expected
                    && self.round.sources_present(state)
                    && Instant::now() < self.round.deadline
            })
            .unwrap_or(false)
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(in crate::operation_journal) fn start_sqlite(&mut self) -> bool {
        let round = Arc::clone(&self.round);
        round
            .with_boundaries(|state| {
                if state.decision != Decision::Deciding
                    || state.invalidated
                    || state.slots != round.expected
                    || !round.sources_present(state)
                    || Instant::now() >= round.deadline
                {
                    return false;
                }
                self.sqlite_started = true;
                true
            })
            .unwrap_or(false)
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
    pub(crate) fn foreign_ack_for_tests(
        self: &Arc<Self>,
        boundary: crate::sample_sources::db::RegisteredDecisionBoundary,
    ) -> CurrentCustody {
        let index = self
            .acknowledged
            .lock()
            .unwrap()
            .sources
            .iter()
            .position(Option::is_some)
            .unwrap();
        self.request(index).acknowledge_registered(boundary)
    }
    pub(crate) fn snapshot_hook_for_tests(&self, hook: Box<dyn FnOnce() + Send>) {
        *self.snapshot_hook.lock().unwrap() = Some(hook);
    }
    pub(crate) fn all_ack_slots_for_tests(&self) -> bool {
        let state = self.acknowledged.lock().unwrap();
        self.sources_present(&state) && state.slots == self.expected
    }
    pub(crate) fn cleanup_fenced_for_tests(&self) -> bool {
        self.acknowledged.lock().unwrap().decision == Decision::Deciding
    }
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
    source: Option<uuid::Uuid>,
}
impl CurrentRound {
    pub(crate) const READY_PHASE_BYTES: usize = 64;
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
                sources: [None; 64],
                boundaries: std::array::from_fn(|_| None),
                cohort: 0,
            }),
            resolved: Condvar::new(),
            nonce: uuid::Uuid::new_v4(),
            #[cfg(all(test, target_os = "linux"))]
            snapshot_hook: Mutex::new(None),
            _credit: credit,
        }))
    }
    pub(super) fn require_source(&self, index: usize, registration: uuid::Uuid) -> bool {
        let Ok(mut state) = self.acknowledged.lock() else {
            return false;
        };
        if index >= 64
            || state.slots != 0
            || self.expected & (1u64 << index) == 0
            || registration.is_nil()
            || state.sources.iter().flatten().any(|id| *id == registration)
        {
            return false;
        }
        let Some(next) = state.cohort.checked_add(1) else {
            state.invalidated = true;
            return false;
        };
        state.cohort = next;
        state.sources[index] = Some(registration);
        true
    }
    fn sources_present(&self, state: &RoundState) -> bool {
        state
            .sources
            .iter()
            .zip(&state.boundaries)
            .all(|(source, boundary)| match source {
                Some(expected) => boundary
                    .as_ref()
                    .is_some_and(|b| b.registration() == *expected),
                None => boundary.is_none(),
            })
    }
    fn with_boundaries<T>(
        &self,
        compare: impl FnOnce(&RoundState) -> T,
    ) -> Result<T, BoundaryRefusal> {
        // Snapshot fixed Arc references; gate -> round is the producer lock order.
        let (boundaries, cohort) = {
            let state = self
                .acknowledged
                .lock()
                .map_err(|_| BoundaryRefusal::Revoked)?;
            (state.boundaries.clone(), state.cohort)
        };
        #[cfg(all(test, target_os = "linux"))]
        {
            let hook = self.snapshot_hook.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        fn visit<T>(
            boundaries: &[Option<crate::sample_sources::db::RegisteredDecisionBoundary>],
            compare: impl FnOnce() -> Result<T, BoundaryRefusal>,
        ) -> Result<T, BoundaryRefusal> {
            let Some((first, rest)) = boundaries.split_first() else {
                return compare();
            };
            match first {
                Some(boundary) => {
                    boundary
                        .try_current(|| visit(rest, compare))
                        .map_err(|error| match error {
                            crate::sample_sources::db::DecisionBoundaryRefusal::Busy => {
                                BoundaryRefusal::Busy
                            }
                            crate::sample_sources::db::DecisionBoundaryRefusal::Revoked => {
                                BoundaryRefusal::Revoked
                            }
                        })?
                }
                None => visit(rest, compare),
            }
        }
        visit(&boundaries, || {
            let state = self
                .acknowledged
                .lock()
                .map_err(|_| BoundaryRefusal::Revoked)?;
            if state.cohort != cohort {
                return Err(BoundaryRefusal::Changed);
            }
            Ok(compare(&state))
        })
    }
    pub(super) fn retained_bytes(source_count: usize) -> usize {
        std::mem::size_of::<Self>()
            + std::mem::size_of::<SourceBoundaries>()
            + source_count
                * crate::sample_sources::db::RegisteredDecisionBoundary::MAX_BACKING_BYTES
            + 64
            + Self::READY_PHASE_BYTES
    }
    pub(super) fn request(self: &Arc<Self>, index: usize) -> CurrentRequest {
        let bit = 1u64
            .checked_shl(index as u32)
            .expect("bounded original inventory");
        assert_ne!(self.expected & bit, 0);
        CurrentRequest {
            round: Arc::clone(self),
            bit,
            source: self
                .acknowledged
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .sources[index],
        }
    }
    pub(super) fn invalidated(&self) -> bool {
        match self.with_boundaries(|state| state.invalidated) {
            Ok(invalidated) => invalidated,
            Err(BoundaryRefusal::Changed | BoundaryRefusal::Busy) => false, // Pending ACK, not current validity.
            Err(BoundaryRefusal::Revoked) => true,
        }
    }
    pub(super) fn complete(&self) -> bool {
        self.with_complete(|_| ()).is_some()
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
    pub(super) fn with_complete<T>(&self, publish: impl FnOnce(u64) -> T) -> Option<T> {
        self.with_boundaries(|state| {
            if Instant::now() >= self.deadline
                || state.invalidated
                || state.slots != self.expected
                || !self.sources_present(state)
                || state.decision != Decision::Preparing
            {
                return None;
            }
            Some(publish(state.cohort))
        })
        .ok()
        .flatten()
    }
}
impl CurrentRequest {
    pub(crate) fn acknowledge(self) -> CurrentCustody {
        self.acknowledge_inner(None)
    }
    pub(crate) fn acknowledge_registered(
        self,
        boundary: crate::sample_sources::db::RegisteredDecisionBoundary,
    ) -> CurrentCustody {
        let round = Arc::clone(&self.round);
        boundary
            .with_current(|| self.acknowledge_inner(Some(boundary.clone())))
            .unwrap_or_else(|| CurrentCustody(round))
    }
    fn acknowledge_inner(
        self,
        boundary: Option<crate::sample_sources::db::RegisteredDecisionBoundary>,
    ) -> CurrentCustody {
        if Instant::now() < self.round.deadline
            && self.source == boundary.as_ref().map(|b| b.registration())
            && let Ok(mut state) = self.round.acknowledged.lock()
            && !state.invalidated
            && matches!(state.decision, Decision::Preparing | Decision::Deciding)
        {
            if let Some(boundary) = boundary.as_ref()
                && state
                    .boundaries
                    .iter()
                    .flatten()
                    .any(|old| old.same_gate(boundary))
            {
                state.invalidated = true;
                return CurrentCustody(Arc::clone(&self.round));
            }
            let Some(next) = state.cohort.checked_add(1) else {
                state.invalidated = true;
                return CurrentCustody(Arc::clone(&self.round));
            };
            state.cohort = next;
            state.boundaries[self.bit.trailing_zeros() as usize] = boundary;
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
    fn current_round_checked_cohort_overflow_refuses_without_ack_or_authority() {
        // Logical bookkeeping fixture; no original owner token or physical authority.
        let budget = super::super::HandshakeBudget::new();
        let mut credit = budget.credit();
        credit.resize(CurrentRound::retained_bytes(0)).unwrap();
        let round = CurrentRound::new(1, Instant::now() + Duration::from_secs(1), credit).unwrap();
        assert!(!round.require_source(1, uuid::Uuid::new_v4()));
        round.acknowledged.lock().unwrap().cohort = u64::MAX;
        let custody = round.request(0).acknowledge();
        assert!(!round.complete());
        assert!(round.invalidated());
        assert_eq!(round.acknowledged.lock().unwrap().slots, 0);
        drop(custody);
        drop(round);
        assert_eq!(budget.used(), 0);
    }

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
