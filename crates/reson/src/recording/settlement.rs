//! Instance-bound producer evidence, not journal, filesystem or publication authority.
use super::health::{RecordingHealth, RecordingHealthState};
use super::writer::{CANCELLED, RecordingLimits, RecordingPrefixState, RecordingSink, STOPPED};
use crate::input::AudioInputError;
use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
};
use std::time::Duration;

const CAPTURING: u8 = 0;
const AWAITING_OWNER: u8 = 1;
const FINALIZING: u8 = 2;
const DENIED: u8 = 3;
/// Fixed bound for an owner acknowledgement; capture callbacks never wait on this.
pub const RECORDING_TERMINAL_ACK_TIMEOUT: Duration = Duration::from_secs(2);

struct Instance;

/// One retained sink and its private settlement route, consumed by actual startup.
/// Binding an arbitrary codec transport grants no journal or physical owner authority.
pub struct RecordingSinkBinding {
    pub(super) sink: Box<dyn RecordingSink>,
    pub(super) producer: ProducerSettlement,
}
impl RecordingSinkBinding {
    /// Allocate two one-element channels for this exact sink/serializer instance.
    /// Retain the receiver in the admitted adapter before moving the binding to startup.
    pub fn bind(sink: Box<dyn RecordingSink>) -> (Self, RecordingSettlements) {
        let instance = Arc::new(Instance);
        let (barrier_tx, barrier_rx) = mpsc::sync_channel(1);
        let (joined_tx, joined_rx) = mpsc::sync_channel(1);
        (
            Self {
                sink,
                producer: ProducerSettlement {
                    instance: instance.clone(),
                    barrier_tx,
                    joined_tx,
                },
            },
            RecordingSettlements {
                instance,
                barrier_rx,
                joined_rx,
                barrier_taken: false,
                joined_taken: false,
            },
        )
    }
}

/// Sole consumer of the exact instance's terminal and joined evidence. Not Clone or serializable.
pub struct RecordingSettlements {
    instance: Arc<Instance>,
    barrier_rx: Receiver<RecordingTerminalBarrier>,
    joined_rx: Receiver<RecordingJoinedCompletion>,
    barrier_taken: bool,
    joined_taken: bool,
}
impl RecordingSettlements {
    /// Wait on an owner thread, separately from the synchronous recorder Stop/join thread.
    pub fn recv_barrier(
        &mut self,
        timeout: Duration,
    ) -> Result<RecordingTerminalBarrier, RecvTimeoutError> {
        if self.barrier_taken {
            return Err(RecvTimeoutError::Disconnected);
        }
        let barrier = self.barrier_rx.recv_timeout(timeout)?;
        self.barrier_taken = true;
        debug_assert!(self.owns_barrier(&barrier));
        Ok(barrier)
    }
    /// Only successful actual finalization AND join can send this one-shot value.
    pub fn recv_joined(
        &mut self,
        timeout: Duration,
    ) -> Result<RecordingJoinedCompletion, RecvTimeoutError> {
        if self.joined_taken {
            return Err(RecvTimeoutError::Disconnected);
        }
        let joined = self.joined_rx.recv_timeout(timeout)?;
        self.joined_taken = true;
        debug_assert!(self.owns_completion(&joined));
        Ok(joined)
    }
    /// Compare opaque issuance, never a path, UUID or caller-supplied scalar.
    pub fn owns_barrier(&self, barrier: &RecordingTerminalBarrier) -> bool {
        Arc::ptr_eq(&self.instance, &barrier.instance)
    }
    /// Check that the joined proof came from the sink bound to this consumer.
    pub fn owns_completion(&self, joined: &RecordingJoinedCompletion) -> bool {
        Arc::ptr_eq(&self.instance, &joined.instance)
    }
}

/// Stable accepted-sample partition at the actual producer barrier. Fields are observations;
/// only the enclosing privately issued barrier or joined proof authenticates their origin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordingTerminalAccounting {
    pub accepted_samples: u64,
    pub encoded_samples: u64,
    pub abandoned_samples: u64,
    pub unconfirmed_samples: u64,
    pub dropped_samples_at_barrier: u64,
    pub overrun_events_at_barrier: u64,
}
impl RecordingTerminalAccounting {
    fn capture(health: RecordingHealth) -> Result<Self, AudioInputError> {
        let accounted = health
            .writer_encoded_samples
            .checked_add(health.writer_abandoned_samples)
            .and_then(|n| n.checked_add(health.writer_unconfirmed_samples));
        let saturated = [
            health.writer_accepted_samples,
            health.writer_encoded_samples,
            health.writer_abandoned_samples,
            health.writer_unconfirmed_samples,
            health.writer_dropped_samples,
            health.writer_overrun_events,
        ]
        .contains(&u64::MAX);
        if saturated
            || !health.writer_accounting_complete
            || health.writer_accounting_overflowed
            || health.writer_failed
            || health.writer_unconfirmed_samples != 0
            || accounted != Some(health.writer_accepted_samples)
        {
            return Err(failed("recording terminal accounting unconfirmed"));
        }
        Ok(Self {
            accepted_samples: health.writer_accepted_samples,
            encoded_samples: health.writer_encoded_samples,
            abandoned_samples: health.writer_abandoned_samples,
            unconfirmed_samples: health.writer_unconfirmed_samples,
            dropped_samples_at_barrier: health.writer_dropped_samples,
            overrun_events_at_barrier: health.writer_overrun_events,
        })
    }
}
#[derive(Clone, Debug)]
pub(super) struct TerminalFacts {
    pub(super) state: RecordingPrefixState,
    pub(super) accounting: RecordingTerminalAccounting,
    pub(super) frames: u64,
    pub(super) limits: RecordingLimits,
    pub(super) inventory: Arc<RecordingTerminalWriteInventory>,
}

/// Exact serializer-owned bytes awaiting finalization, observed before acknowledgement.
/// This value authenticates producer byte custody only through its enclosing private proof;
/// it grants no journal or physical authority. It has no public constructor or decoder.
///
/// ```compile_fail
/// let _ = reson::recording::RecordingTerminalWriteInventory {};
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct RecordingTerminalWriteInventory {
    pub(super) pending_offset: u64,
    pub(super) pending: Vec<u8>,
    pub(super) object_bytes: u64,
    pub(super) riff_length: [u8; 4],
    pub(super) data_length: [u8; 4],
}
impl RecordingTerminalWriteInventory {
    /// Actual retained-transport offset at the start of the still-buffered fragment.
    /// This may fall within a frame; the retained adapter must rechunk complete frames.
    pub fn pending_offset(&self) -> u64 {
        self.pending_offset
    }
    /// Exact pending serializer fragment, bounded by the 16 KiB producer buffer.
    pub fn pending_bytes(&self) -> &[u8] {
        &self.pending
    }
    /// Exact final WAV extent including the 68-byte extensible header.
    pub fn object_bytes(&self) -> u64 {
        self.object_bytes
    }
    /// The pinned hound 3.5.1 final RIFF-length patch at offset 4.
    pub fn riff_length_patch(&self) -> &[u8; 4] {
        &self.riff_length
    }
    /// The pinned hound 3.5.1 final data-length patch at offset 64.
    pub fn data_length_patch(&self) -> &[u8; 4] {
        &self.data_length
    }
}

/// Issued only after actual callback settlement and Stop drain/Cancel abandonment,
/// before any terminal tail/header write. Consume or drop exactly once.
///
/// ```compile_fail
/// fn duplicate(value: reson::recording::RecordingTerminalBarrier) { let _ = value.clone(); }
/// ```
/// ```compile_fail
/// let _ = reson::recording::RecordingTerminalBarrier {};
/// ```
pub struct RecordingTerminalBarrier {
    instance: Arc<Instance>,
    facts: TerminalFacts,
    acknowledgement: SyncSender<()>,
}
impl RecordingTerminalBarrier {
    pub fn state(&self) -> RecordingPrefixState {
        self.facts.state
    }
    pub fn accounting(&self) -> RecordingTerminalAccounting {
        self.facts.accounting
    }
    pub fn frames(&self) -> u64 {
        self.facts.frames
    }
    pub fn limits(&self) -> RecordingLimits {
        self.facts.limits
    }
    /// Exact immutable pending-byte and final-header inventory of this producer instance.
    pub fn write_inventory(&self) -> &RecordingTerminalWriteInventory {
        &self.facts.inventory
    }
    /// Permit this serializer to attempt its final bytes only after the admitted owner accepts
    /// its own bound immutable inventory. This never publishes, validates or releases output.
    /// A returned error means delivery was uncertain; no replacement acknowledgement exists.
    pub fn acknowledge(self) -> Result<(), AudioInputError> {
        self.acknowledgement
            .try_send(())
            .map_err(|_| failed("recording terminal acknowledgement lost"))
    }
    /// Dropping/denying the one-shot acknowledgement keeps the retained prefix unconfirmed.
    pub fn deny(self) {}
}

/// Privately issued after successful finalization and actual successful worker join.
/// This proves producer completion only; it does not prove retained bytes or publication.
///
/// ```compile_fail
/// fn duplicate(value: reson::recording::RecordingJoinedCompletion) { let _ = value.clone(); }
/// ```
/// ```compile_fail
/// let _ = reson::recording::RecordingJoinedCompletion {};
/// ```
pub struct RecordingJoinedCompletion {
    instance: Arc<Instance>,
    facts: TerminalFacts,
}
impl RecordingJoinedCompletion {
    pub fn state(&self) -> RecordingPrefixState {
        self.facts.state
    }
    pub fn accounting(&self) -> RecordingTerminalAccounting {
        self.facts.accounting
    }
    pub fn frames(&self) -> u64 {
        self.facts.frames
    }
    pub fn limits(&self) -> RecordingLimits {
        self.facts.limits
    }
    /// Exact immutable pending-byte and final-header inventory of this producer instance.
    pub fn write_inventory(&self) -> &RecordingTerminalWriteInventory {
        &self.facts.inventory
    }
}

pub(super) struct ProducerSettlement {
    instance: Arc<Instance>,
    barrier_tx: SyncSender<RecordingTerminalBarrier>,
    joined_tx: SyncSender<RecordingJoinedCompletion>,
}
pub(super) struct BarrierIssuer {
    instance: Arc<Instance>,
    sender: SyncSender<RecordingTerminalBarrier>,
    pub(super) guard: Arc<TerminalWriteFence>,
}
pub(super) struct JoinedIssuer {
    instance: Arc<Instance>,
    sender: SyncSender<RecordingJoinedCompletion>,
    guard: Arc<TerminalWriteFence>,
    health: Arc<RecordingHealthState>,
}
impl ProducerSettlement {
    pub(super) fn start(
        self,
        mode: Arc<AtomicU8>,
        health: Arc<RecordingHealthState>,
    ) -> (BarrierIssuer, JoinedIssuer) {
        let guard = Arc::new(TerminalWriteFence {
            phase: AtomicU8::new(CAPTURING),
            expected_mode: AtomicU8::new(0),
            mode,
        });
        (
            BarrierIssuer {
                instance: self.instance.clone(),
                sender: self.barrier_tx,
                guard: guard.clone(),
            },
            JoinedIssuer {
                instance: self.instance,
                sender: self.joined_tx,
                guard,
                health,
            },
        )
    }
}
impl BarrierIssuer {
    pub(super) fn acknowledge(
        &self,
        facts: TerminalFacts,
        health: &RecordingHealthState,
    ) -> Result<(), AudioInputError> {
        RecordingTerminalAccounting::capture(health.snapshot())?;
        let expected = terminal_mode(facts.state);
        if self.guard.mode.load(Ordering::SeqCst) != expected
            || self
                .guard
                .phase
                .compare_exchange(
                    CAPTURING,
                    AWAITING_OWNER,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_err()
        {
            self.guard.deny();
            return Err(failed("recording terminal disposition changed"));
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let barrier = RecordingTerminalBarrier {
            instance: self.instance.clone(),
            facts,
            acknowledgement: tx,
        };
        if self.sender.try_send(barrier).is_err()
            || rx.recv_timeout(RECORDING_TERMINAL_ACK_TIMEOUT).is_err()
            || self.guard.mode.load(Ordering::SeqCst) != expected
        {
            self.guard.deny();
            return Err(failed(
                "recording terminal owner acknowledgement unconfirmed",
            ));
        }
        self.guard.expected_mode.store(expected, Ordering::SeqCst);
        if self
            .guard
            .phase
            .compare_exchange(
                AWAITING_OWNER,
                FINALIZING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            self.guard.deny();
            return Err(failed("recording terminal fence already closed"));
        }
        Ok(())
    }
    pub(super) fn facts(
        state: RecordingPrefixState,
        frames: u64,
        limits: RecordingLimits,
        health: &RecordingHealthState,
        inventory: Arc<RecordingTerminalWriteInventory>,
    ) -> Result<TerminalFacts, AudioInputError> {
        let accounting = RecordingTerminalAccounting::capture(health.snapshot())?;
        if frames.checked_mul(u64::from(limits.channels())) != Some(accounting.encoded_samples)
            || accounting
                .encoded_samples
                .checked_mul(4)
                .and_then(|n| n.checked_add(68))
                != Some(inventory.object_bytes)
        {
            return Err(failed(
                "recording terminal inventory/accounting coverage differs",
            ));
        }
        Ok(TerminalFacts {
            state,
            frames,
            limits,
            accounting,
            inventory,
        })
    }
}
impl JoinedIssuer {
    pub(super) fn publish(self, facts: TerminalFacts) -> Result<(), AudioInputError> {
        let accounting = RecordingTerminalAccounting::capture(self.health.snapshot())?;
        if (
            accounting.accepted_samples,
            accounting.encoded_samples,
            accounting.abandoned_samples,
            accounting.unconfirmed_samples,
        ) != (
            facts.accounting.accepted_samples,
            facts.accounting.encoded_samples,
            facts.accounting.abandoned_samples,
            facts.accounting.unconfirmed_samples,
        ) || self.guard.mode.load(Ordering::SeqCst) != terminal_mode(facts.state)
        {
            return Err(failed("recording joined settlement changed"));
        }
        self.sender
            .try_send(RecordingJoinedCompletion {
                instance: self.instance.clone(),
                facts,
            })
            .map_err(|_| failed("recording joined completion delivery unconfirmed"))
    }
    pub(super) fn deny(&self) {
        self.guard.deny();
    }
}
impl Drop for JoinedIssuer {
    fn drop(&mut self) {
        self.guard.deny();
    }
}

pub(super) struct TerminalWriteFence {
    phase: AtomicU8,
    expected_mode: AtomicU8,
    mode: Arc<AtomicU8>,
}
impl TerminalWriteFence {
    pub(super) fn deny(&self) {
        self.phase.store(DENIED, Ordering::SeqCst);
    }
    pub(super) fn check(&self) -> io::Result<()> {
        let phase = self.phase.load(Ordering::SeqCst);
        if std::thread::panicking()
            || (phase == FINALIZING
                && self.mode.load(Ordering::SeqCst) != self.expected_mode.load(Ordering::SeqCst))
        {
            self.deny();
            return Err(io::Error::other("recording terminal transport closed"));
        }
        if matches!(phase, CAPTURING | FINALIZING) {
            Ok(())
        } else {
            Err(io::Error::other("recording terminal transport closed"))
        }
    }
}
fn terminal_mode(state: RecordingPrefixState) -> u8 {
    match state {
        RecordingPrefixState::StoppedUnpublished => STOPPED,
        RecordingPrefixState::CancelledRetained => CANCELLED,
    }
}
fn failed(detail: &str) -> AudioInputError {
    AudioInputError::RecordingFailed {
        detail: detail.into(),
    }
}
