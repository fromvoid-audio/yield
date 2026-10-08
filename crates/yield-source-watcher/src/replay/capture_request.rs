//! Nonblocking original native capture; its sole slot follows the worker and owned result.
use super::{ReplayCandidate, ReplayObservationBatch};
use crate::SourceWatchSpec;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, TryRecvError},
};
use std::time::Duration;
use yield_library::sample_sources::reconciliation::WatcherGeneration;

static CAPTURE_SLOT: AtomicBool = AtomicBool::new(false);
struct CaptureSlot;
#[cfg(any(target_os = "macos", test))]
impl CaptureSlot {
    fn acquire() -> Result<Arc<Self>, NativeReplayCaptureError> {
        CAPTURE_SLOT
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| NativeReplayCaptureError::Busy)?;
        Ok(Arc::new(Self))
    }
}
impl Drop for CaptureSlot {
    fn drop(&mut self) {
        CAPTURE_SLOT.store(false, Ordering::Release);
    }
}

/// Fixed capture refusal; none of these outcomes establishes replay continuity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeReplayCaptureError {
    /// This host has no original FSEvents producer.
    UnsupportedPlatform,
    /// The worker or an original undecided result still retains the sole slot.
    Busy,
    /// Capture context or the positive, at-most-two-second deadline is invalid.
    InvalidInput,
    /// The actual retained source root changed.
    RootChanged,
    /// Native device history or stream could not be established.
    HistoryUnavailable,
    /// The bounded worker could not start or disconnected.
    WorkerUnavailable,
    /// Native delivery did not finish within its original deadline.
    TimedOut,
    /// Cancellation won before delivery; it does not establish worker quiescence.
    Canceled,
    /// This original single-use reply has already been consumed.
    Consumed,
}

/// Original passive candidate and slot custody; decoding bytes cannot recreate this value.
pub struct CapturedNativeReplay {
    candidate: ReplayCandidate,
    slot: Arc<CaptureSlot>,
}
impl CapturedNativeReplay {
    /// Convert the actual candidate using the retained owner's current lifecycle/generation.
    /// The result remains an observation; only durable-prior owner admission can grant replay.
    /// The original slot follows a nonempty batch until this wrapper is actually dropped.
    pub fn into_observation_batch(
        self,
        lifecycle: u64,
        generation: WatcherGeneration,
    ) -> Option<CapturedNativeReplayBatch> {
        self.candidate
            .into_observation_batch(lifecycle, generation)
            .map(|batch| CapturedNativeReplayBatch {
                batch,
                _slot: self.slot,
            })
    }
}

/// Bounded original envelope with its capture custody, never a continuity or mutation grant.
///
/// ```compile_fail
/// fn duplicate(v: yield_source_watcher::replay::CapturedNativeReplayBatch) { let _ = v.clone(); }
/// ```
pub struct CapturedNativeReplayBatch {
    batch: ReplayObservationBatch,
    _slot: Arc<CaptureSlot>,
}
impl CapturedNativeReplayBatch {
    /// Borrow passive input for the existing catalog admission. Retain this wrapper through
    /// admission/reply so dropping a capture request cannot prematurely release its backing.
    pub fn observation(&self) -> &ReplayObservationBatch {
        &self.batch
    }
}

/// Single-use, nonblocking reply from the original bounded native worker.
///
/// ```compile_fail
/// let _ = yield_source_watcher::replay::NativeReplayCaptureRequest {};
/// ```
pub struct NativeReplayCaptureRequest {
    result: Receiver<Result<CapturedNativeReplay, NativeReplayCaptureError>>,
    cancellation: Arc<AtomicBool>,
    consumed: bool,
    _slot: Arc<CaptureSlot>,
}
impl NativeReplayCaptureRequest {
    /// Start original FSEvents history off the calling thread. No source SQL, audit,
    /// publication or receipt occurs here. Unsupported hosts reject before spawning.
    pub fn start(
        spec: SourceWatchSpec,
        after_event: u64,
        timeout: Duration,
    ) -> Result<Self, NativeReplayCaptureError> {
        if spec.lifecycle_generation() == 0
            || spec.source_id().as_str().is_empty()
            || !spec.root().is_absolute()
            || spec.source_id().as_str().len() > 256
            || spec.root().as_os_str().as_encoded_bytes().len() > 4096
            || spec
                .root_identity()
                .is_none_or(|identity| identity.as_bytes().len() > 4096)
            || timeout.is_zero()
            || timeout > Duration::from_secs(2)
        {
            return Err(NativeReplayCaptureError::InvalidInput);
        }
        #[cfg(target_os = "macos")]
        {
            let deadline = std::time::Instant::now()
                .checked_add(timeout)
                .ok_or(NativeReplayCaptureError::InvalidInput)?;
            Self::spawn(move |cancel| {
                if cancel.load(Ordering::Acquire) {
                    return Err(NativeReplayCaptureError::Canceled);
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err(NativeReplayCaptureError::TimedOut);
                }
                super::macos::capture_history(spec, after_event, remaining).map_err(|error| {
                    use super::macos::NativeReplayError as E;
                    match error {
                        E::Busy => NativeReplayCaptureError::Busy,
                        E::InvalidTimeout | E::InvalidIdentity(_) => {
                            NativeReplayCaptureError::InvalidInput
                        }
                        E::RootChanged => NativeReplayCaptureError::RootChanged,
                        E::WorkerUnavailable => NativeReplayCaptureError::WorkerUnavailable,
                        E::TimedOut => NativeReplayCaptureError::TimedOut,
                        E::VolumeHistoryUnavailable
                        | E::StreamUnavailable
                        | E::CursorUnavailable => NativeReplayCaptureError::HistoryUnavailable,
                    }
                })
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = after_event;
            Err(NativeReplayCaptureError::UnsupportedPlatform)
        }
    }

    #[cfg(any(target_os = "macos", test))]
    fn spawn(
        capture: impl FnOnce(Arc<AtomicBool>) -> Result<ReplayCandidate, NativeReplayCaptureError>
        + Send
        + 'static,
    ) -> Result<Self, NativeReplayCaptureError> {
        let slot = CaptureSlot::acquire()?;
        let worker_slot = slot.clone();
        let cancellation = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancellation.clone();
        let (sender, result) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("yield-native-replay-request".into())
            .spawn(move || {
                let result = capture(worker_cancel.clone());
                let result = if worker_cancel.load(Ordering::Acquire) {
                    Err(NativeReplayCaptureError::Canceled)
                } else {
                    result
                };
                let _ = sender.send(result.map(|candidate| CapturedNativeReplay {
                    candidate,
                    slot: worker_slot.clone(),
                }));
                // Failed/lost delivery drops its real candidate before the worker's final slot.
                drop(worker_slot);
            })
            .map_err(|_| NativeReplayCaptureError::WorkerUnavailable)?;
        Ok(Self {
            result,
            cancellation,
            consumed: false,
            _slot: slot,
        })
    }

    /// Poll once without blocking. Empty leaves the same reply untouched; consumption is final.
    pub fn try_result(&mut self) -> Result<Option<CapturedNativeReplay>, NativeReplayCaptureError> {
        if self.consumed {
            return Err(NativeReplayCaptureError::Consumed);
        }
        match self.result.try_recv() {
            Ok(result) => {
                self.consumed = true;
                if self.cancellation.load(Ordering::Acquire) {
                    drop(result);
                    Err(NativeReplayCaptureError::Canceled)
                } else {
                    result.map(Some)
                }
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.consumed = true;
                Err(NativeReplayCaptureError::WorkerUnavailable)
            }
        }
    }
    /// Signal cancellation. Slot release waits for actual worker/result/request destruction.
    /// The underlying producer retains its separate active-stream guard until native stop.
    /// A canceled or timed-out reply supplies no native-quiescence or cleanup authority.
    pub fn cancel(&self) {
        self.cancellation.store(true, Ordering::Release);
    }
}
impl Drop for NativeReplayCaptureRequest {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl std::fmt::Display for NativeReplayCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::UnsupportedPlatform => "native replay capture unsupported",
            Self::Busy => "native replay capture busy",
            Self::InvalidInput => "native replay capture input invalid",
            Self::RootChanged => "native replay capture root changed",
            Self::HistoryUnavailable => "native replay capture history unavailable",
            Self::WorkerUnavailable => "native replay capture worker unavailable",
            Self::TimedOut => "native replay capture timed out",
            Self::Canceled => "native replay capture canceled",
            Self::Consumed => "native replay capture reply consumed",
        })
    }
}
impl std::error::Error for NativeReplayCaptureError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Mutex, mpsc},
        thread,
        time::Instant,
    };
    use yield_library::sample_sources::{SourceId, reconciliation::RootIdentity};
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    fn spec() -> SourceWatchSpec {
        #[cfg(windows)]
        let root = std::path::PathBuf::from(r"C:\source");
        #[cfg(not(windows))]
        let root = std::path::PathBuf::from("/source");
        SourceWatchSpec::new(
            SourceId::from_string("capture-fixture"),
            7,
            root,
            Some(RootIdentity::from_bytes(b"root".to_vec())),
        )
    }
    fn wait_released() {
        let deadline = Instant::now() + Duration::from_secs(2);
        while CAPTURE_SLOT.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn capture_request_empty_cancel_lost_reply_keeps_actual_worker_slot() {
        let _serial = TEST_LOCK.lock().unwrap();
        let (entered, seen) = mpsc::sync_channel(1);
        let (release, gate) = mpsc::sync_channel(1);
        let mut request = NativeReplayCaptureRequest::spawn(move |cancel| {
            entered.send(()).unwrap();
            gate.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(cancel.load(Ordering::Acquire));
            Err(NativeReplayCaptureError::RootChanged)
        })
        .unwrap();
        seen.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(request.try_result().unwrap().is_none());
        request.cancel();
        drop(request);
        assert!(matches!(
            NativeReplayCaptureRequest::spawn(|_| Err(NativeReplayCaptureError::Canceled)),
            Err(NativeReplayCaptureError::Busy)
        ));
        release.send(()).unwrap();
        wait_released();
    }
    #[test]
    fn capture_request_original_result_and_batch_retain_slot_until_final_drop() {
        let _serial = TEST_LOCK.lock().unwrap();
        // Passive ingress data fixture only; no native producer, continuity proof or receipt.
        let mut request = NativeReplayCaptureRequest::spawn(|_| {
            Ok(super::super::ReplayIngress::new(spec(), 42, [1; 16], 100)
                .unwrap()
                .into_candidate())
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let captured = loop {
            if let Some(captured) = request.try_result().unwrap() {
                break captured;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        };
        assert!(matches!(
            request.try_result(),
            Err(NativeReplayCaptureError::Consumed)
        ));
        drop(request);
        assert!(CAPTURE_SLOT.load(Ordering::Acquire));
        let batch = captured
            .into_observation_batch(7, WatcherGeneration::new(7))
            .unwrap();
        assert!(!batch.observation().continuous());
        assert!(matches!(
            NativeReplayCaptureRequest::spawn(|_| Err(NativeReplayCaptureError::Canceled)),
            Err(NativeReplayCaptureError::Busy)
        ));
        drop(batch);
        wait_released();
    }
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn capture_request_unsupported_host_creates_no_worker_or_slot() {
        let _serial = TEST_LOCK.lock().unwrap();
        assert!(matches!(
            NativeReplayCaptureRequest::start(spec(), 100, Duration::from_secs(1)),
            Err(NativeReplayCaptureError::UnsupportedPlatform)
        ));
        assert!(!CAPTURE_SLOT.load(Ordering::Acquire));
        assert!(matches!(
            NativeReplayCaptureRequest::start(spec(), 100, Duration::ZERO),
            Err(NativeReplayCaptureError::InvalidInput)
        ));
        assert!(!CAPTURE_SLOT.load(Ordering::Acquire));
    }
}
