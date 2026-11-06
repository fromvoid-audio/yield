//! Bounded, single-use ingress for a native FSEvents replay stream.
//!
//! This buffer preserves native event IDs and source-relative paths without manufacturing a
//! continuity proof. The native stream owner establishes `sinceWhen` and delivery coverage;
//! durable prior authority is required before reconciliation can admit a proof-bearing batch.

use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use yield_library::sample_sources::SourceId;
use yield_library::sample_sources::reconciliation::{
    BackendStreamIdentity, BootstrapReplayEvidence, RootIdentity, WatcherGeneration,
};

use crate::SourceWatchSpec;

#[cfg(target_os = "macos")]
pub mod macos;

mod batch;
mod capture_request;
pub use capture_request::{
    CapturedNativeReplay, CapturedNativeReplayBatch, NativeReplayCaptureError,
    NativeReplayCaptureRequest,
};

pub use batch::ReplayObservationBatch;

/// Maximum number of native paths retained for one replay candidate.
pub const MAX_REPLAY_EVENTS: usize = 256;
/// Maximum native path-spelling bytes retained for one replay candidate.
pub const MAX_REPLAY_PATH_BYTES: usize = 64 * 1024;

/// A native event's conservative delivery classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayEventCondition {
    /// The backend reported a changed path without a loss flag.
    Changed,
    /// The backend requires a subtree rescan; exact changed-path coverage is unavailable.
    MustScanSubdirs,
    /// The backend reported dropped events or an ID wrap.
    DeliveryLost,
    /// The watched source root was replaced or removed.
    RootChanged,
    /// The backend delivered an event shape which is not qualified for replay.
    Unknown,
}

/// Conservative entry type supplied by the native watcher for a changed path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayPathKind {
    /// The native event identifies a regular file.
    File,
    /// The native event identifies a directory, including a removed directory.
    Directory,
    /// The native event identifies a symlink entry.
    Symlink,
    /// The backend did not provide one unambiguous entry type.
    Unknown,
}

/// Bounded reasons why a captured candidate cannot assert complete changed-path coverage.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReplayUncertainty(u16);

impl ReplayUncertainty {
    /// An event had no native cursor.
    pub const MISSING_CURSOR: Self = Self(1 << 0);
    /// A delivered cursor did not advance beyond the last accepted cursor.
    pub const NON_MONOTONIC_CURSOR: Self = Self(1 << 1);
    /// The backend explicitly reported a delivery loss.
    pub const DELIVERY_LOST: Self = Self(1 << 2);
    /// The backend requires an unbounded subtree scan.
    pub const MUST_SCAN_SUBDIRS: Self = Self(1 << 3);
    /// The physical source root changed.
    pub const ROOT_CHANGED: Self = Self(1 << 4);
    /// The event path is outside the captured source root or is not a safe relative path.
    pub const INVALID_PATH: Self = Self(1 << 5);
    /// The number of retained events would exceed the fixed bound.
    pub const EVENT_LIMIT: Self = Self(1 << 6);
    /// Retaining a path would exceed the fixed byte bound.
    pub const PATH_LIMIT: Self = Self(1 << 7);
    /// The backend event shape has not been qualified.
    pub const UNKNOWN_EVENT: Self = Self(1 << 8);
    /// The stream stopped before the native historical-replay completion marker.
    pub const HISTORY_INCOMPLETE: Self = Self(1 << 9);
    /// The root directory's modification time changed during a silent bootstrap stream.
    pub const SILENT_ROOT_CHANGE: Self = Self(1 << 10);

    /// Return whether no uncertainty was recorded.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Return whether all supplied reasons were recorded.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

/// One ordered changed-path event retained from a native callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayPathEvent {
    event_id: u64,
    relative_path: PathBuf,
    kind: ReplayPathKind,
}

impl ReplayPathEvent {
    /// Return the native FSEvents event ID.
    pub const fn event_id(&self) -> u64 {
        self.event_id
    }

    /// Borrow the lexically source-relative path.
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    /// Return the conservative native entry type.
    pub const fn kind(&self) -> ReplayPathKind {
        self.kind
    }
}

/// A bounded replay candidate with identity and explicit uncertainty.
///
/// `started_after` is the event ID requested from the native stream. It is not a durable
/// acknowledgement and this type never grants `WatcherContinuityProof`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayCandidate {
    source_id: SourceId,
    lifecycle_generation: u64,
    root_identity: RootIdentity,
    stream_identity: BackendStreamIdentity,
    started_after: u64,
    last_seen: Option<u64>,
    events: Vec<ReplayPathEvent>,
    // Exact, source-local metadata changes which advance the native boundary but do not
    // describe an audio-tree mutation. Kept separate from `events` so its classification
    // cannot be recreated from a caller-supplied path.
    ignored_artifacts: Vec<ReplayPathEvent>,
    unqualified_ignored_artifact: bool,
    history_complete: bool,
    uncertainty: ReplayUncertainty,
}

impl ReplayCandidate {
    /// Convert a live pre-audit capture into transport evidence only when no source path changed.
    /// The retained source writer still needs the matching completed audit before checkpointing.
    pub fn into_bootstrap_evidence(
        self,
        owner_watcher_generation: WatcherGeneration,
    ) -> Option<BootstrapReplayEvidence> {
        let through_sequence = self.last_seen.unwrap_or(self.started_after);
        BootstrapReplayEvidence::from_certain_empty_capture(
            self.source_id,
            self.root_identity,
            self.stream_identity,
            self.lifecycle_generation,
            owner_watcher_generation,
            self.started_after,
            through_sequence,
            self.history_complete,
            !self.uncertainty.is_empty(),
            self.events.len(),
        )
    }

    /// Borrow the captured source ID.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Return the source lifecycle generation captured at stream start.
    pub const fn lifecycle_generation(&self) -> u64 {
        self.lifecycle_generation
    }

    /// Borrow the captured root identity.
    pub fn root_identity(&self) -> &RootIdentity {
        &self.root_identity
    }

    /// Borrow the physical FSEvents device-stream identity.
    pub fn stream_identity(&self) -> &BackendStreamIdentity {
        &self.stream_identity
    }

    /// Return the event ID requested as the native stream's `sinceWhen` value.
    pub const fn started_after(&self) -> u64 {
        self.started_after
    }

    /// Return the greatest native event ID observed, including excluded or uncertain events.
    pub const fn last_seen(&self) -> Option<u64> {
        self.last_seen
    }

    /// Borrow retained events in callback order.
    pub fn events(&self) -> &[ReplayPathEvent] {
        &self.events
    }

    pub(crate) fn ignored_artifacts(&self) -> &[ReplayPathEvent] {
        &self.ignored_artifacts
    }

    /// Return whether the native stream delivered its historical-replay completion marker.
    pub const fn history_complete(&self) -> bool {
        self.history_complete
    }

    /// Return every bounded reason that prevents exact path coverage.
    pub const fn uncertainty(&self) -> ReplayUncertainty {
        self.uncertainty
    }
}

/// A replay ingress cannot start without a physical root and FSEvents device identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayStartError {
    /// The configured root is not a lexical absolute source path.
    InvalidRoot,
    /// The source owner did not supply a physical root identity.
    MissingRootIdentity,
    /// Zero is not a durable FSEvents device identity.
    MissingDeviceIdentity,
    /// The native device stream did not provide a non-nil volume UUID.
    MissingVolumeIdentity,
    /// The prior event ID cannot be advanced without overflow.
    ExhaustedCursor,
}

#[derive(Debug)]
struct ReplayState {
    last_seen: Option<u64>,
    events: Vec<ReplayPathEvent>,
    ignored_artifacts: Vec<ReplayPathEvent>,
    unqualified_ignored_artifact: bool,
    path_bytes: usize,
    history_complete: bool,
    uncertainty: ReplayUncertainty,
    closed: bool,
}

/// A single-use, bounded ingress for one source and native device stream.
///
/// The caller must pass the actual native event ID and loss classification. This type rejects
/// over-limit paths before copying them, never writes a source database, and does not claim that
/// its caller actually started an FSEvents stream at `started_after`.
#[derive(Debug)]
pub struct ReplayIngress {
    spec: SourceWatchSpec,
    root_identity: RootIdentity,
    stream_identity: BackendStreamIdentity,
    started_after: u64,
    state: Mutex<ReplayState>,
}

impl ReplayIngress {
    /// Create an ingress for a source lifecycle and physical FSEvents device.
    pub fn new(
        spec: SourceWatchSpec,
        device: u64,
        volume_uuid: [u8; 16],
        started_after: u64,
    ) -> Result<Self, ReplayStartError> {
        if !spec.root().is_absolute()
            || spec
                .root()
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(ReplayStartError::InvalidRoot);
        }
        let root_identity = spec
            .root_identity()
            .cloned()
            .ok_or(ReplayStartError::MissingRootIdentity)?;
        if device == 0 {
            return Err(ReplayStartError::MissingDeviceIdentity);
        }
        let stream_identity = BackendStreamIdentity::from_fsevents_volume(device, volume_uuid)
            .ok_or(ReplayStartError::MissingVolumeIdentity)?;
        started_after
            .checked_add(1)
            .ok_or(ReplayStartError::ExhaustedCursor)?;
        Ok(Self {
            spec,
            root_identity,
            stream_identity,
            started_after,
            state: Mutex::new(ReplayState {
                last_seen: None,
                events: Vec::new(),
                ignored_artifacts: Vec::new(),
                unqualified_ignored_artifact: false,
                path_bytes: 0,
                history_complete: false,
                uncertainty: ReplayUncertainty::default(),
                closed: false,
            }),
        })
    }

    /// Record one native callback event without retaining an unbounded path or queue.
    ///
    /// Numeric gaps between delivered FSEvents IDs are normal because IDs come from a global
    /// source. Only missing/regressing IDs or explicit backend loss make this candidate uncertain.
    pub fn record(&self, event_id: Option<u64>, path: &Path, condition: ReplayEventCondition) {
        self.record_with_kind(event_id, path, condition, ReplayPathKind::Unknown);
    }

    /// Record a native event with a conservative entry type for later scan-scope selection.
    pub fn record_with_kind(
        &self,
        event_id: Option<u64>,
        path: &Path,
        condition: ReplayEventCondition,
        kind: ReplayPathKind,
    ) {
        let mut state = lock_unpoisoned(&self.state);
        if state.closed {
            return;
        }
        let condition_uncertainty = match condition {
            ReplayEventCondition::Changed => None,
            ReplayEventCondition::MustScanSubdirs => Some(ReplayUncertainty::MUST_SCAN_SUBDIRS),
            ReplayEventCondition::DeliveryLost => Some(ReplayUncertainty::DELIVERY_LOST),
            ReplayEventCondition::RootChanged => Some(ReplayUncertainty::ROOT_CHANGED),
            ReplayEventCondition::Unknown => Some(ReplayUncertainty::UNKNOWN_EVENT),
        };
        if let Some(reason) = condition_uncertainty {
            mark_uncertain(&mut state, reason);
        }
        let Some(event_id) = event_id else {
            mark_uncertain(&mut state, ReplayUncertainty::MISSING_CURSOR);
            return;
        };
        if event_id <= self.started_after || state.last_seen.is_some_and(|last| event_id <= last) {
            mark_uncertain(&mut state, ReplayUncertainty::NON_MONOTONIC_CURSOR);
            return;
        }
        state.last_seen = Some(event_id);
        if condition_uncertainty.is_some() {
            return;
        }
        if state.events.len() + state.ignored_artifacts.len() >= MAX_REPLAY_EVENTS {
            mark_uncertain(&mut state, ReplayUncertainty::EVENT_LIMIT);
            return;
        }
        let native_bytes = path.as_os_str().as_encoded_bytes().len();
        if native_bytes > MAX_REPLAY_PATH_BYTES.saturating_sub(state.path_bytes) {
            mark_uncertain(&mut state, ReplayUncertainty::PATH_LIMIT);
            return;
        }
        let Ok(relative_path) = path.strip_prefix(self.spec.root()) else {
            mark_uncertain(&mut state, ReplayUncertainty::INVALID_PATH);
            return;
        };
        if !relative_path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
            || relative_path.as_os_str().is_empty()
        {
            mark_uncertain(&mut state, ReplayUncertainty::INVALID_PATH);
            return;
        }
        state.path_bytes += native_bytes;
        state.events.push(ReplayPathEvent {
            event_id,
            relative_path: relative_path.to_path_buf(),
            kind,
        });
    }

    /// Advance the native cursor for a known source-local artifact without treating it as an
    /// audio-tree mutation. The caller must have validated the path and conservative flags.
    #[cfg(any(target_os = "macos", test))]
    fn record_ignored_artifact(&self, event_id: u64) {
        let mut state = lock_unpoisoned(&self.state);
        if state.closed {
            return;
        }
        if event_id <= self.started_after || state.last_seen.is_some_and(|last| event_id <= last) {
            mark_uncertain(&mut state, ReplayUncertainty::NON_MONOTONIC_CURSOR);
            return;
        }
        state.last_seen = Some(event_id);
        state.unqualified_ignored_artifact = true;
    }

    /// Retain an exact `.DS_Store` callback for source-owner cursor admission without treating it
    /// as an audio-tree mutation. The macOS translator is the only caller and validates flags.
    #[cfg(any(target_os = "macos", test))]
    fn record_metadata_artifact(&self, event_id: u64, path: &Path) {
        let mut state = lock_unpoisoned(&self.state);
        if state.closed {
            return;
        }
        if event_id <= self.started_after || state.last_seen.is_some_and(|last| event_id <= last) {
            mark_uncertain(&mut state, ReplayUncertainty::NON_MONOTONIC_CURSOR);
            return;
        }
        state.last_seen = Some(event_id);
        if state.events.len() + state.ignored_artifacts.len() >= MAX_REPLAY_EVENTS {
            mark_uncertain(&mut state, ReplayUncertainty::EVENT_LIMIT);
            return;
        }
        let native_bytes = path.as_os_str().as_encoded_bytes().len();
        if native_bytes > MAX_REPLAY_PATH_BYTES.saturating_sub(state.path_bytes) {
            mark_uncertain(&mut state, ReplayUncertainty::PATH_LIMIT);
            return;
        }
        let Ok(relative_path) = path.strip_prefix(self.spec.root()) else {
            mark_uncertain(&mut state, ReplayUncertainty::INVALID_PATH);
            return;
        };
        if !relative_path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
            || relative_path.as_os_str().is_empty()
            || relative_path.file_name().and_then(|name| name.to_str()) != Some(".DS_Store")
        {
            mark_uncertain(&mut state, ReplayUncertainty::INVALID_PATH);
            return;
        }
        state.path_bytes += native_bytes;
        state.ignored_artifacts.push(ReplayPathEvent {
            event_id,
            relative_path: relative_path.to_path_buf(),
            kind: ReplayPathKind::File,
        });
    }

    /// Stop accepting callbacks. Late events cannot mutate the retained candidate.
    pub fn close(&self) {
        lock_unpoisoned(&self.state).closed = true;
    }

    /// Retain a second conservative native flag without revisiting an event path.
    #[cfg(target_os = "macos")]
    fn mark_reason(&self, reason: ReplayUncertainty) {
        let mut state = lock_unpoisoned(&self.state);
        if !state.closed {
            mark_uncertain(&mut state, reason);
        }
    }

    /// Record the native `HistoryDone` sentinel without interpreting its path as a mutation.
    ///
    /// This marker is necessary but not sufficient for continuity: the native stream owner must
    /// still prove its `sinceWhen` setting, stream identity, and absence of loss flags.
    pub fn mark_history_complete(&self) {
        let mut state = lock_unpoisoned(&self.state);
        if !state.closed {
            state.history_complete = true;
        }
    }

    /// Consume one candidate after the native stream has stopped delivering callbacks.
    pub fn into_candidate(self) -> ReplayCandidate {
        let mut state = match self.state.into_inner() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !state.history_complete {
            state
                .uncertainty
                .insert(ReplayUncertainty::HISTORY_INCOMPLETE);
        }
        ReplayCandidate {
            source_id: self.spec.source_id().clone(),
            lifecycle_generation: self.spec.lifecycle_generation(),
            root_identity: self.root_identity,
            stream_identity: self.stream_identity,
            started_after: self.started_after,
            last_seen: state.last_seen,
            events: state.events,
            ignored_artifacts: state.ignored_artifacts,
            unqualified_ignored_artifact: state.unqualified_ignored_artifact,
            history_complete: state.history_complete,
            uncertainty: state.uncertainty,
        }
    }
}

fn mark_uncertain(state: &mut ReplayState, reason: ReplayUncertainty) {
    state.uncertainty.insert(reason);
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VOLUME_UUID: [u8; 16] = [1; 16];

    fn ingress() -> ReplayIngress {
        let spec = SourceWatchSpec::new(
            SourceId::from_string("replay-source"),
            7,
            PathBuf::from("/source"),
            Some(RootIdentity::from_bytes(b"root".to_vec())),
        );
        ReplayIngress::new(spec, 42, VOLUME_UUID, 100).expect("qualified identities")
    }

    #[test]
    fn retains_ordered_paths_with_global_id_leaps_without_minting_proof() {
        let ingress = ingress();
        ingress.record(
            Some(105),
            Path::new("/source/a.wav"),
            ReplayEventCondition::Changed,
        );
        ingress.record(
            Some(113),
            Path::new("/source/b.wav"),
            ReplayEventCondition::Changed,
        );
        ingress.mark_history_complete();
        let candidate = ingress.into_candidate();
        assert_eq!(candidate.started_after(), 100);
        assert_eq!(candidate.last_seen(), Some(113));
        assert!(candidate.uncertainty().is_empty());
        assert_eq!(candidate.events().len(), 2);
        assert_eq!(candidate.events()[0].relative_path(), Path::new("a.wav"));
        assert_eq!(candidate.events()[1].event_id(), 113);
        assert!(candidate.history_complete());
    }

    #[test]
    fn missing_or_regressing_cursor_retains_uncertainty() {
        let ingress = ingress();
        ingress.record(
            None,
            Path::new("/source/a.wav"),
            ReplayEventCondition::Changed,
        );
        ingress.record(
            Some(105),
            Path::new("/source/a.wav"),
            ReplayEventCondition::Changed,
        );
        ingress.record(
            Some(104),
            Path::new("/source/b.wav"),
            ReplayEventCondition::Changed,
        );
        let candidate = ingress.into_candidate();
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::MISSING_CURSOR)
        );
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::NON_MONOTONIC_CURSOR)
        );
        assert_eq!(candidate.events().len(), 1);
    }

    #[test]
    fn overflow_and_root_change_never_become_path_evidence() {
        for condition in [
            ReplayEventCondition::DeliveryLost,
            ReplayEventCondition::RootChanged,
        ] {
            let ingress = ingress();
            ingress.record(Some(105), Path::new("/source/a.wav"), condition);
            let candidate = ingress.into_candidate();
            assert!(candidate.events().is_empty());
            assert!(!candidate.uncertainty().is_empty());
            assert_eq!(candidate.last_seen(), Some(105));
        }
    }

    #[test]
    fn delivery_loss_is_retained_even_when_cursor_is_missing() {
        let ingress = ingress();
        ingress.record(
            None,
            Path::new("/source/a.wav"),
            ReplayEventCondition::DeliveryLost,
        );
        let candidate = ingress.into_candidate();
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::DELIVERY_LOST)
        );
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::MISSING_CURSOR)
        );
        assert!(candidate.events().is_empty());
    }

    #[test]
    fn rejects_outside_paths_and_caps_retained_event_count() {
        let ingress = ingress();
        ingress.record(
            Some(105),
            Path::new("/other/a.wav"),
            ReplayEventCondition::Changed,
        );
        for index in 0..=MAX_REPLAY_EVENTS {
            ingress.record(
                Some(106 + index as u64),
                Path::new("/source/a.wav"),
                ReplayEventCondition::Changed,
            );
        }
        let candidate = ingress.into_candidate();
        assert_eq!(candidate.events().len(), MAX_REPLAY_EVENTS);
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::INVALID_PATH)
        );
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::EVENT_LIMIT)
        );
    }

    #[test]
    fn oversized_callback_path_is_not_copied_into_a_candidate() {
        let ingress = ingress();
        let oversized = format!("/source/{}", "a".repeat(MAX_REPLAY_PATH_BYTES));
        ingress.record(
            Some(105),
            Path::new(&oversized),
            ReplayEventCondition::Changed,
        );
        let candidate = ingress.into_candidate();
        assert!(candidate.events().is_empty());
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::PATH_LIMIT)
        );
        assert_eq!(candidate.last_seen(), Some(105));
    }

    #[test]
    fn physical_root_and_device_are_required_at_stream_start() {
        let relative_root = SourceWatchSpec::new(
            SourceId::from_string("replay-source"),
            7,
            PathBuf::from("source"),
            Some(RootIdentity::from_bytes(b"root".to_vec())),
        );
        assert!(matches!(
            ReplayIngress::new(relative_root, 42, VOLUME_UUID, 100),
            Err(ReplayStartError::InvalidRoot)
        ));
        let without_root = SourceWatchSpec::new(
            SourceId::from_string("replay-source"),
            7,
            PathBuf::from("/source"),
            None,
        );
        assert!(matches!(
            ReplayIngress::new(without_root, 42, VOLUME_UUID, 100),
            Err(ReplayStartError::MissingRootIdentity)
        ));
        let valid = SourceWatchSpec::new(
            SourceId::from_string("replay-source"),
            7,
            PathBuf::from("/source"),
            Some(RootIdentity::from_bytes(b"root".to_vec())),
        );
        assert!(matches!(
            ReplayIngress::new(valid.clone(), 0, VOLUME_UUID, 100),
            Err(ReplayStartError::MissingDeviceIdentity)
        ));
        assert!(matches!(
            ReplayIngress::new(valid.clone(), 42, [0; 16], 100),
            Err(ReplayStartError::MissingVolumeIdentity)
        ));
        assert!(matches!(
            ReplayIngress::new(valid, 42, VOLUME_UUID, u64::MAX),
            Err(ReplayStartError::ExhaustedCursor)
        ));
    }

    #[test]
    fn closed_ingress_ignores_late_callbacks() {
        let ingress = ingress();
        ingress.close();
        ingress.record(
            Some(105),
            Path::new("/source/a.wav"),
            ReplayEventCondition::Changed,
        );
        let candidate = ingress.into_candidate();
        assert!(candidate.events().is_empty());
        assert_eq!(candidate.last_seen(), None);
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::HISTORY_INCOMPLETE)
        );
    }

    #[test]
    fn late_history_marker_cannot_clear_stopped_lane_uncertainty() {
        let ingress = ingress();
        ingress.close();
        ingress.mark_history_complete();
        let candidate = ingress.into_candidate();
        assert!(!candidate.history_complete());
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::HISTORY_INCOMPLETE)
        );
    }
}
