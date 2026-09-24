//! Bounded, profile-bound maintenance operations for the session owner.

use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use yield_library::app_dirs::{CacheMaintenanceError, StorageUsageError, WritableProfileGuard};

pub use yield_library::app_dirs::{CacheClearOutcome, GlobalStorageUsage};

/// A typed result handle for one nonblocking maintenance operation.
///
/// The receiver is polled with [`MaintenanceRequest::try_result`]. Dropping the request sets the
/// operation cancellation flag; the session worker joins the maintenance thread during shutdown.
pub struct MaintenanceRequest<T> {
    receiver: Receiver<Result<T, MaintenanceError>>,
    cancel: Arc<AtomicBool>,
    generation: u64,
}

impl<T> MaintenanceRequest<T> {
    pub(crate) fn new(
        receiver: Receiver<Result<T, MaintenanceError>>,
        cancel: Arc<AtomicBool>,
        generation: u64,
    ) -> Self {
        Self {
            receiver,
            cancel,
            generation,
        }
    }

    /// Poll the operation without blocking the caller.
    pub fn try_result(&self) -> Result<Result<T, MaintenanceError>, TryRecvError> {
        self.receiver.try_recv()
    }

    /// Return the maintenance generation assigned when this request was admitted.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl<T> Drop for MaintenanceRequest<T> {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

/// Failure returned by a storage usage query or cache cleanup request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaintenanceError {
    /// No loaded profile/source snapshot was available when the request reached the worker.
    NotReady,
    /// A storage usage query was cancelled before a complete answer was available.
    Cancelled,
    /// A cache cleanup was cancelled after the reported counters were completed.
    CacheCancelled {
        /// Counters completed before cancellation.
        outcome: CacheClearOutcome,
    },
    /// The retained profile root or owner lock changed during maintenance.
    ProfileChanged {
        /// Profile validation detail.
        reason: String,
        /// Cache counters when this was a cleanup operation.
        outcome: Option<CacheClearOutcome>,
    },
    /// The captured cache directory was replaced at its configured path.
    NamespaceReplaced {
        /// Counters completed against the captured directory.
        outcome: CacheClearOutcome,
        /// Replacement detail.
        reason: String,
    },
    /// Cleanup stopped while entries remained in the captured namespace.
    Incomplete {
        /// Counters and bounded remaining-entry observation.
        outcome: CacheClearOutcome,
    },
    /// A bounded maintenance walk could not safely cover its namespace.
    BoundsExceeded {
        /// Bound kind.
        kind: &'static str,
        /// Bound value.
        limit: usize,
        /// Cache counters when this was a cleanup operation.
        outcome: Option<CacheClearOutcome>,
    },
    /// A profile-bound filesystem operation failed.
    Filesystem {
        /// Display path associated with the operation, when available.
        path: Option<PathBuf>,
        /// Renderer-neutral failure detail.
        message: String,
        /// Cache counters when this was a cleanup operation.
        outcome: Option<CacheClearOutcome>,
    },
    /// The current platform has no verified handle-bound cache cleanup primitive.
    Unsupported {
        /// Platform limitation detail.
        reason: String,
        /// Cache counters, normally zero for an unsupported operation.
        outcome: Option<CacheClearOutcome>,
    },
    /// A configured source root overlaps the captured disposable cache namespace.
    SourceRootConflict {
        /// Conflicting configured source root.
        path: PathBuf,
    },
    /// The configured trash folder overlaps the captured disposable cache namespace.
    TrashFolderConflict {
        /// Conflicting configured trash folder.
        path: PathBuf,
    },
    /// The maintenance thread could not be started.
    ThreadSpawn(String),
    /// The maintenance worker panicked; no ambient profile fallback was attempted.
    WorkerPanicked,
    /// A required profile capability was unavailable.
    ProfileUnavailable(String),
}

impl MaintenanceError {
    /// Return cache counters carried by this failure, if the failure came from cache cleanup.
    pub fn cache_clear_outcome(&self) -> Option<CacheClearOutcome> {
        match self {
            Self::ProfileChanged { outcome, .. }
            | Self::BoundsExceeded { outcome, .. }
            | Self::Filesystem { outcome, .. }
            | Self::Unsupported { outcome, .. } => *outcome,
            Self::NamespaceReplaced { outcome, .. } | Self::Incomplete { outcome } => {
                Some(*outcome)
            }
            Self::CacheCancelled { outcome } => Some(*outcome),
            Self::Cancelled
            | Self::NotReady
            | Self::SourceRootConflict { .. }
            | Self::TrashFolderConflict { .. }
            | Self::ThreadSpawn(_)
            | Self::WorkerPanicked
            | Self::ProfileUnavailable(_) => None,
        }
    }
}

impl fmt::Display for MaintenanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn counters(
            formatter: &mut fmt::Formatter<'_>,
            outcome: Option<CacheClearOutcome>,
        ) -> fmt::Result {
            if let Some(outcome) = outcome {
                write!(
                    formatter,
                    " (removed {} files / {} bytes; ",
                    outcome.removed_files, outcome.removed_bytes
                )?;
                if outcome.remaining_entries_known {
                    write!(formatter, "{} entries remain; ", outcome.remaining_entries)?;
                } else {
                    formatter.write_str("remaining entries unknown; ")?;
                }
                if outcome.namespace_replaced {
                    formatter.write_str("namespace changed")?;
                } else {
                    formatter.write_str("cleanup incomplete")?;
                }
                formatter.write_str(")")?;
            }
            Ok(())
        }

        match self {
            Self::NotReady => formatter.write_str("maintenance profile/source state is not ready"),
            Self::Cancelled => formatter.write_str("storage usage query cancelled"),
            Self::CacheCancelled { outcome } => {
                formatter.write_str("cache cleanup cancelled")?;
                counters(formatter, Some(*outcome))
            }
            Self::ProfileChanged { reason, outcome } => {
                write!(formatter, "profile changed during maintenance: {reason}")?;
                counters(formatter, *outcome)
            }
            Self::NamespaceReplaced { outcome, reason } => {
                write!(formatter, "cache namespace was replaced: {reason}")?;
                counters(formatter, Some(*outcome))
            }
            Self::Incomplete { outcome } => {
                formatter.write_str("cache cleanup is incomplete")?;
                counters(formatter, Some(*outcome))
            }
            Self::BoundsExceeded {
                kind,
                limit,
                outcome,
            } => {
                write!(formatter, "maintenance exceeded {kind} limit {limit}")?;
                counters(formatter, *outcome)
            }
            Self::Filesystem {
                path,
                message,
                outcome,
            } => {
                if let Some(path) = path {
                    write!(
                        formatter,
                        "maintenance filesystem failure at {}: {message}",
                        path.display()
                    )?;
                } else {
                    write!(formatter, "maintenance filesystem failure: {message}")?;
                }
                counters(formatter, *outcome)
            }
            Self::Unsupported { reason, outcome } => {
                write!(formatter, "cache cleanup unsupported: {reason}")?;
                counters(formatter, *outcome)
            }
            Self::SourceRootConflict { path } => {
                write!(
                    formatter,
                    "source root overlaps disposable cache namespace: {}",
                    path.display()
                )
            }
            Self::TrashFolderConflict { path } => {
                write!(
                    formatter,
                    "trash folder overlaps disposable cache namespace: {}",
                    path.display()
                )
            }
            Self::ThreadSpawn(reason) => {
                write!(formatter, "failed to start maintenance thread: {reason}")
            }
            Self::WorkerPanicked => formatter.write_str("maintenance worker panicked"),
            Self::ProfileUnavailable(reason) => {
                write!(formatter, "maintenance profile unavailable: {reason}")
            }
        }
    }
}

impl std::error::Error for MaintenanceError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MaintenanceKind {
    StorageUsage,
    CacheClear,
}

struct MaintenanceAdmissionState {
    active: Option<(u64, MaintenanceKind)>,
}

/// Shared admission state owned by the session worker and its public service handle.
pub(crate) struct MaintenanceAdmission {
    state: Mutex<MaintenanceAdmissionState>,
    next_id: AtomicU64,
    next_generation: AtomicU64,
}

impl MaintenanceAdmission {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(MaintenanceAdmissionState { active: None }),
            next_id: AtomicU64::new(1),
            next_generation: AtomicU64::new(0),
        }
    }

    pub(crate) fn reserve(
        self: &Arc<Self>,
        kind: MaintenanceKind,
    ) -> Option<MaintenanceReservation> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.active.is_some() {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let generation = self.next_generation.fetch_add(1, Ordering::AcqRel) + 1;
        state.active = Some((id, kind));
        Some(MaintenanceReservation {
            admission: Arc::clone(self),
            id,
            generation,
            cancel: Arc::new(AtomicBool::new(false)),
            released: false,
        })
    }

    pub(crate) fn clear_reserved(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        matches!(state.active, Some((_, MaintenanceKind::CacheClear)))
    }

    pub(crate) fn generation(&self) -> u64 {
        self.next_generation.load(Ordering::Acquire)
    }

    fn release(&self, id: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.active.is_some_and(|(active_id, _)| active_id == id) {
            state.active = None;
        }
    }
}

pub(crate) struct MaintenanceReservation {
    admission: Arc<MaintenanceAdmission>,
    id: u64,
    generation: u64,
    cancel: Arc<AtomicBool>,
    released: bool,
}

impl MaintenanceReservation {
    pub(crate) fn cancel_token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    fn into_parts(mut self) -> MaintenanceReservationParts {
        self.released = true;
        MaintenanceReservationParts {
            admission: Arc::clone(&self.admission),
            id: self.id,
            cancel: Arc::clone(&self.cancel),
        }
    }
}

impl Drop for MaintenanceReservation {
    fn drop(&mut self) {
        if !self.released {
            self.admission.release(self.id);
        }
    }
}

#[derive(Clone)]
struct MaintenanceReservationParts {
    admission: Arc<MaintenanceAdmission>,
    id: u64,
    cancel: Arc<AtomicBool>,
}

impl MaintenanceReservationParts {
    fn finish(&self) {
        self.admission.release(self.id);
    }
}

struct MaintenanceJob {
    id: u64,
    cancel: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

/// Worker-owned maintenance execution state.
pub(crate) struct MaintenanceService {
    admission: Arc<MaintenanceAdmission>,
    active: Option<MaintenanceJob>,
}

impl MaintenanceService {
    pub(crate) fn new(admission: Arc<MaintenanceAdmission>) -> Self {
        Self {
            admission,
            active: None,
        }
    }

    pub(crate) fn clear_reserved(&self) -> bool {
        self.admission.clear_reserved()
    }

    pub(crate) fn start_storage(
        &mut self,
        profile: Option<&WritableProfileGuard>,
        reservation: MaintenanceReservation,
        result: SyncSender<Result<GlobalStorageUsage, MaintenanceError>>,
    ) {
        self.reap_finished();
        let parts = reservation.into_parts();
        let Some(profile) = profile else {
            let _ = result.send(Err(MaintenanceError::ProfileUnavailable(String::from(
                "writable profile guard is unavailable",
            ))));
            parts.finish();
            return;
        };
        let profile = match profile.try_clone() {
            Ok(profile) => profile,
            Err(error) => {
                let _ = result.send(Err(MaintenanceError::ProfileUnavailable(error.to_string())));
                parts.finish();
                return;
            }
        };
        let cancel = Arc::clone(&parts.cancel);
        let thread_cancel = Arc::clone(&cancel);
        let thread_parts = parts.clone();
        let thread_result = result.clone();
        let id = parts.id;
        let thread = match thread::Builder::new()
            .name(String::from("yield-storage-maintenance"))
            .spawn(move || {
                let operation = panic::catch_unwind(AssertUnwindSafe(|| {
                    yield_library::app_dirs::global_storage_usage_for_profile(&profile, || {
                        thread_cancel.load(Ordering::Acquire)
                    })
                    .map_err(MaintenanceError::from)
                }));
                let operation = match operation {
                    Ok(operation) => operation,
                    Err(_) => Err(MaintenanceError::WorkerPanicked),
                };
                let _ = thread_result.send(operation);
                thread_parts.finish();
            }) {
            Ok(thread) => thread,
            Err(error) => {
                let _ = result.send(Err(MaintenanceError::ThreadSpawn(error.to_string())));
                parts.finish();
                return;
            }
        };
        self.active = Some(MaintenanceJob { id, cancel, thread });
    }

    pub(crate) fn start_clear(
        &mut self,
        profile: Option<&WritableProfileGuard>,
        source_roots: &[PathBuf],
        trash_folder: Option<&Path>,
        reservation: MaintenanceReservation,
        result: SyncSender<Result<CacheClearOutcome, MaintenanceError>>,
    ) {
        self.reap_finished();
        let Some(profile) = profile else {
            let _ = result.send(Err(MaintenanceError::ProfileUnavailable(String::from(
                "writable profile guard is unavailable",
            ))));
            return;
        };
        let namespace = match profile.capture_disposable_cache_namespace() {
            Ok(namespace) => namespace,
            Err(error) => {
                let _ = result.send(Err(map_profile_capture_error(error)));
                return;
            }
        };
        if let Some(path) = source_roots
            .iter()
            .find(|path| namespace.conflicts_with_path(path))
        {
            let _ = result.send(Err(MaintenanceError::SourceRootConflict {
                path: path.clone(),
            }));
            return;
        }
        if let Some(path) = trash_folder.filter(|path| namespace.conflicts_with_path(path)) {
            let _ = result.send(Err(MaintenanceError::TrashFolderConflict {
                path: path.to_path_buf(),
            }));
            return;
        }

        let parts = reservation.into_parts();
        let cancel = Arc::clone(&parts.cancel);
        let thread_cancel = Arc::clone(&cancel);
        let thread_parts = parts.clone();
        let thread_result = result.clone();
        let id = parts.id;
        let thread = match thread::Builder::new()
            .name(String::from("yield-cache-maintenance"))
            .spawn(move || {
                let operation = panic::catch_unwind(AssertUnwindSafe(|| {
                    namespace.clear(|| thread_cancel.load(Ordering::Acquire))
                }));
                let operation = match operation {
                    Ok(operation) => operation.map_err(MaintenanceError::from),
                    Err(_) => Err(MaintenanceError::WorkerPanicked),
                };
                let _ = thread_result.send(operation);
                thread_parts.finish();
            }) {
            Ok(thread) => thread,
            Err(error) => {
                let _ = result.send(Err(MaintenanceError::ThreadSpawn(error.to_string())));
                parts.finish();
                return;
            }
        };
        self.active = Some(MaintenanceJob { id, cancel, thread });
    }

    pub(crate) fn shutdown(&mut self) {
        if let Some(job) = self.active.take() {
            job.cancel.store(true, Ordering::Release);
            let _ = job.thread.join();
            self.admission.release(job.id);
        }
    }

    fn reap_finished(&mut self) {
        let finished = self
            .active
            .as_ref()
            .is_some_and(|job| job.thread.is_finished());
        if !finished {
            return;
        }
        if let Some(job) = self.active.take() {
            let _ = job.thread.join();
            self.admission.release(job.id);
        }
    }
}

impl Drop for MaintenanceService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn map_profile_capture_error(
    error: yield_library::app_dirs::ProfileOwnershipError,
) -> MaintenanceError {
    match error {
        yield_library::app_dirs::ProfileOwnershipError::Unsupported { path } => {
            MaintenanceError::Unsupported {
                reason: format!(
                    "Windows has no verified handle-bound disposable-cache cleanup primitive at {}",
                    path.display()
                ),
                outcome: Some(CacheClearOutcome {
                    incomplete: true,
                    ..CacheClearOutcome::default()
                }),
            }
        }
        error => MaintenanceError::ProfileUnavailable(error.to_string()),
    }
}

impl From<StorageUsageError> for MaintenanceError {
    fn from(error: StorageUsageError) -> Self {
        match error {
            StorageUsageError::Cancelled => Self::Cancelled,
            StorageUsageError::ProfileChanged(reason) => Self::ProfileChanged {
                reason,
                outcome: None,
            },
            StorageUsageError::BoundsExceeded { kind, limit } => Self::BoundsExceeded {
                kind,
                limit,
                outcome: None,
            },
            StorageUsageError::Io(message) => Self::Filesystem {
                path: None,
                message,
                outcome: None,
            },
        }
    }
}

impl From<CacheMaintenanceError> for MaintenanceError {
    fn from(error: CacheMaintenanceError) -> Self {
        match error {
            CacheMaintenanceError::Cancelled { outcome } => Self::CacheCancelled { outcome },
            CacheMaintenanceError::ProfileChanged { outcome, reason } => Self::ProfileChanged {
                reason,
                outcome: Some(outcome),
            },
            CacheMaintenanceError::NamespaceReplaced { outcome, reason } => {
                Self::NamespaceReplaced { outcome, reason }
            }
            CacheMaintenanceError::Incomplete { outcome, .. } => Self::Incomplete { outcome },
            CacheMaintenanceError::BoundsExceeded {
                outcome,
                kind,
                limit,
            } => Self::BoundsExceeded {
                kind,
                limit,
                outcome: Some(outcome),
            },
            CacheMaintenanceError::Io {
                outcome,
                path,
                message,
            } => Self::Filesystem {
                path: Some(path),
                message,
                outcome: Some(outcome),
            },
            CacheMaintenanceError::Unsupported { outcome, reason } => Self::Unsupported {
                reason,
                outcome: Some(outcome),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn admission_allows_one_operation_and_releases_after_drop() {
        let admission = Arc::new(MaintenanceAdmission::new());
        let first = admission
            .reserve(MaintenanceKind::StorageUsage)
            .expect("first reservation");
        assert!(admission.reserve(MaintenanceKind::CacheClear).is_none());
        drop(first);
        assert!(admission.reserve(MaintenanceKind::CacheClear).is_some());
    }

    #[test]
    fn request_drop_sets_cancellation_and_try_result_is_nonblocking() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let request = MaintenanceRequest::new(receiver, Arc::clone(&cancel), 1);
        assert!(matches!(request.try_result(), Err(TryRecvError::Empty)));
        drop(request);
        assert!(cancel.load(Ordering::Acquire));
        let _ = sender.send(Ok(GlobalStorageUsage::default()));
    }
}
