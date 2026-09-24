#![deny(missing_docs)]
#![deny(warnings)]

//! Bounded filesystem change hints for one sample-source lifecycle.
//!
//! This crate owns the operating-system watcher handle, but it does not own source databases,
//! scanners, manifests, or UI publication. Notify callbacks classify only the event envelope and
//! merge it into one constant-size hint. A caller drains that hint and decides when an
//! authoritative audit is appropriate.

use notify::event::ModifyKind;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use yield_library::sample_sources::SourceId;
use yield_library::sample_sources::reconciliation::RootIdentity;

/// Filename prefixes for source-local files which must not feed an audit back into itself.
///
/// These names are compared lexically in the callback. The callback does not inspect metadata or
/// follow symlinks. A caller can replace this list on [`SourceWatchSpec`] when a source database
/// uses different local artifact names.
pub const DEFAULT_IGNORED_FILE_NAME_PREFIXES: &[&str] = &[".yield.db", ".yield_samples.db", "._"];

/// The maximum number of pending hints retained by one watcher handle.
///
/// Event paths are used only while classifying a notify callback and are never copied into a
/// hint. A second callback merges into the existing hint rather than growing a queue.
pub const MAX_PENDING_HINTS_PER_HANDLE: usize = 1;

/// Evidence merged into a [`SourceWatchHint`].
///
/// The flags intentionally separate a descendant mutation from an event concerning only the
/// watched root. A catalog scheduler can compare its retained root identity before turning the
/// latter into a full audit. Backend loss, unknown descendant kinds, overflow, and destructive
/// root events set [`Self::FULL_RESCAN`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WatchHintEvidence(u8);

impl WatchHintEvidence {
    /// No evidence was recorded.
    pub const EMPTY: Self = Self(0);
    /// A supported or otherwise source-relative descendant changed.
    pub const DESCENDANT_CHANGED: Self = Self(1 << 0);
    /// Only the watched root received an ambiguous metadata-like mutation.
    pub const ROOT_METADATA_AMBIGUOUS: Self = Self(1 << 1);
    /// The event stream cannot safely describe the affected paths and requires a full audit.
    pub const FULL_RESCAN: Self = Self(1 << 2);
    /// The backend reported an error; [`Self::FULL_RESCAN`] is set with it.
    pub const BACKEND_ERROR: Self = Self(1 << 3);

    /// Return whether no evidence flags are set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Return whether this evidence includes a descendant mutation.
    pub const fn has_descendant_change(self) -> bool {
        self.0 & Self::DESCENDANT_CHANGED.0 != 0
    }

    /// Return whether this evidence includes an ambiguous root-only mutation.
    pub const fn has_root_metadata_ambiguity(self) -> bool {
        self.0 & Self::ROOT_METADATA_AMBIGUOUS.0 != 0
    }

    /// Return whether conservative full-source reconciliation is required.
    pub const fn requires_full_rescan(self) -> bool {
        self.0 & Self::FULL_RESCAN.0 != 0
    }

    /// Return whether the watcher backend reported an error.
    pub const fn has_backend_error(self) -> bool {
        self.0 & Self::BACKEND_ERROR.0 != 0
    }

    /// Return whether every flag in `required` is present.
    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    /// Merge two evidence sets while retaining every conservative flag.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Immutable source identity captured when a watcher handle is created.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceWatchSpec {
    source_id: SourceId,
    lifecycle_generation: u64,
    root: PathBuf,
    root_identity: Option<RootIdentity>,
    ignored_file_name_prefixes: &'static [&'static str],
}

impl SourceWatchSpec {
    /// Create a watcher specification for one source lifecycle generation.
    ///
    /// The root is expected to be the scanner's canonical absolute path. Validation is deferred
    /// to [`SourceWatcher::start`] so a detached [`WatcherSignal`] remains useful in deterministic
    /// scheduler tests.
    pub fn new(
        source_id: SourceId,
        lifecycle_generation: u64,
        root: PathBuf,
        root_identity: Option<RootIdentity>,
    ) -> Self {
        Self {
            source_id,
            lifecycle_generation,
            root,
            root_identity,
            ignored_file_name_prefixes: DEFAULT_IGNORED_FILE_NAME_PREFIXES,
        }
    }

    /// Replace the lexical source-artifact filename prefixes used by callback filtering.
    ///
    /// The slice is borrowed for the lifetime of the specification, so callers should provide a
    /// static list. Keeping configuration fixed avoids retaining an unbounded callback-owned
    /// collection.
    pub fn with_ignored_file_name_prefixes(mut self, prefixes: &'static [&'static str]) -> Self {
        self.ignored_file_name_prefixes = prefixes;
        self
    }

    /// Borrow the stable source identifier.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Return the source lifecycle generation fenced by this handle.
    pub const fn lifecycle_generation(&self) -> u64 {
        self.lifecycle_generation
    }

    /// Borrow the canonical root path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Borrow the optional physical root identity captured by the source owner.
    pub fn root_identity(&self) -> Option<&RootIdentity> {
        self.root_identity.as_ref()
    }

    /// Borrow the lexical artifact prefixes used by the watcher callback.
    pub fn ignored_file_name_prefixes(&self) -> &'static [&'static str] {
        self.ignored_file_name_prefixes
    }
}

/// One bounded source change hint drained by a catalog scheduler.
///
/// A hint contains no event paths. The source identity, lifecycle generation, and root identity
/// prevent a retained hint from being applied to a replacement handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceWatchHint {
    source_id: SourceId,
    lifecycle_generation: u64,
    root_identity: Option<RootIdentity>,
    first_observed: Instant,
    last_observed: Instant,
    evidence: WatchHintEvidence,
}

impl SourceWatchHint {
    /// Borrow the stable source identifier.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Return the source lifecycle generation captured by the watcher handle.
    pub const fn lifecycle_generation(&self) -> u64 {
        self.lifecycle_generation
    }

    /// Borrow the root identity captured by the watcher handle, when available.
    pub fn root_identity(&self) -> Option<&RootIdentity> {
        self.root_identity.as_ref()
    }

    /// Return the first event time merged into this hint.
    pub fn first_observed(&self) -> Instant {
        self.first_observed
    }

    /// Return the latest event time merged into this hint.
    pub fn last_observed(&self) -> Instant {
        self.last_observed
    }

    /// Return the conservative evidence flags merged into this hint.
    pub const fn evidence(&self) -> WatchHintEvidence {
        self.evidence
    }
}

/// A cloneable, backend-independent ingress and drain signal.
///
/// [`SourceWatcher`] creates one signal per handle and gives its notify callback only that signal.
/// A caller can also create a detached signal from [`WatcherSignal::new`] for deterministic tests
/// or for a future non-notify observation adapter. Signals never submit catalog commands.
#[derive(Clone, Debug)]
pub struct WatcherSignal {
    spec: Arc<SourceWatchSpec>,
    state: Arc<Mutex<SignalState>>,
}

impl WatcherSignal {
    /// Create a detached signal carrying the supplied source identity.
    pub fn new(spec: SourceWatchSpec) -> Self {
        Self {
            spec: Arc::new(spec),
            state: Arc::new(Mutex::new(SignalState {
                accepting: true,
                pending: None,
            })),
        }
    }

    /// Merge bounded evidence into the signal at a supplied monotonic time.
    ///
    /// Returns `true` when the signal accepted the evidence. A closed signal rejects late callback
    /// delivery, while any already-coalesced hint remains drainable for its original lifecycle.
    pub fn record_evidence(&self, evidence: WatchHintEvidence, observed_at: Instant) -> bool {
        if evidence.is_empty() {
            return false;
        }
        let mut state = lock_unpoisoned(&self.state);
        if !state.accepting {
            return false;
        }
        state.record_evidence(&self.spec, evidence, observed_at);
        true
    }

    /// Take the current coalesced hint, if one is pending.
    pub fn take_hint(&self) -> Option<SourceWatchHint> {
        lock_unpoisoned(&self.state).pending.take()
    }

    /// Alias for [`Self::take_hint`] for scheduler code that calls its ingress a drain.
    pub fn drain_hint(&self) -> Option<SourceWatchHint> {
        self.take_hint()
    }

    /// Disable callback acceptance while retaining any already-coalesced hint.
    pub fn close(&self) {
        lock_unpoisoned(&self.state).accepting = false;
    }

    /// Return whether this signal has stopped accepting callback evidence.
    pub fn is_closed(&self) -> bool {
        !lock_unpoisoned(&self.state).accepting
    }

    fn record_notify_result(&self, result: notify::Result<Event>, observed_at: Instant) {
        let evidence = match result {
            Ok(event) => classify_event(&self.spec, &event),
            // Do not inspect, format, or retain backend errors. The error may contain an
            // arbitrary path list; the bounded signal needs only its conservative consequence.
            Err(_) => WatchHintEvidence::FULL_RESCAN.union(WatchHintEvidence::BACKEND_ERROR),
        };
        if evidence.is_empty() {
            return;
        }
        let mut state = lock_unpoisoned(&self.state);
        if !state.accepting {
            return;
        }
        state.record_evidence(&self.spec, evidence, observed_at);
    }
}

#[derive(Debug)]
struct SignalState {
    accepting: bool,
    pending: Option<SourceWatchHint>,
}

impl SignalState {
    fn record_evidence(
        &mut self,
        spec: &SourceWatchSpec,
        evidence: WatchHintEvidence,
        observed_at: Instant,
    ) {
        if let Some(pending) = &mut self.pending {
            if observed_at < pending.first_observed {
                pending.first_observed = observed_at;
            }
            if observed_at > pending.last_observed {
                pending.last_observed = observed_at;
            }
            pending.evidence = pending.evidence.union(evidence);
            return;
        }
        self.pending = Some(SourceWatchHint {
            source_id: spec.source_id.clone(),
            lifecycle_generation: spec.lifecycle_generation,
            root_identity: spec.root_identity.clone(),
            first_observed: observed_at,
            last_observed: observed_at,
            evidence,
        });
    }
}

/// An RAII operating-system watcher for one source lifecycle generation.
///
/// The watcher recursively observes the source root and nonrecursively observes its parent. The
/// parent watch lets a future scheduler notice root replacement or removal; parent events for
/// unrelated siblings are discarded lexically before they reach the signal.
pub struct SourceWatcher {
    signal: WatcherSignal,
    backend: Option<notify::RecommendedWatcher>,
}

/// Owned source watcher handle retained by a lifecycle owner.
pub type SourceWatcherHandle = SourceWatcher;

impl SourceWatcher {
    /// Create a detached watcher handle with no operating-system backend.
    ///
    /// This keeps scheduler tests deterministic while preserving the same coalescing and lifecycle
    /// signal used by [`Self::start`]. The detached handle accepts evidence through its signal but
    /// performs no filesystem observation.
    pub fn detached(spec: SourceWatchSpec) -> Self {
        Self {
            signal: WatcherSignal::new(spec),
            backend: None,
        }
    }

    /// Start a recursive source-root watcher and a nonrecursive parent watch.
    ///
    /// The source root must be an absolute, scanner-canonical path. If the root is unavailable or
    /// either backend watch cannot be installed, construction fails so the caller can retain a
    /// bounded retry obligation. The parent watch is skipped for a filesystem root whose parent
    /// is itself.
    pub fn start(spec: SourceWatchSpec) -> Result<Self, WatcherStartError> {
        validate_root(spec.root())?;
        let signal = WatcherSignal::new(spec.clone());
        let callback_signal = signal.clone();
        let mut backend = notify::recommended_watcher(move |result| {
            callback_signal.record_notify_result(result, Instant::now());
        })
        .map_err(WatcherStartError::BackendInitialization)?;

        if let Err(error) = backend.watch(spec.root(), RecursiveMode::Recursive) {
            signal.close();
            drop(backend);
            return Err(WatcherStartError::RootWatch {
                root: spec.root.clone(),
                error,
            });
        }

        if let Some(parent) = parent_to_watch(spec.root()) {
            if let Err(error) = backend.watch(&parent, RecursiveMode::NonRecursive) {
                signal.close();
                drop(backend);
                return Err(WatcherStartError::ParentWatch { parent, error });
            }
        }

        Ok(Self {
            signal,
            backend: Some(backend),
        })
    }

    /// Borrow a cloneable signal for scheduler or deterministic test integration.
    pub fn signal(&self) -> WatcherSignal {
        self.signal.clone()
    }

    /// Take the current coalesced hint from this watcher.
    pub fn take_hint(&self) -> Option<SourceWatchHint> {
        self.signal.take_hint()
    }

    /// Drain the current coalesced hint from this watcher.
    pub fn drain_hint(&self) -> Option<SourceWatchHint> {
        self.signal.drain_hint()
    }

    /// Return whether the watcher has stopped accepting backend callbacks.
    pub fn is_closed(&self) -> bool {
        self.signal.is_closed()
    }

    /// Close the signal before dropping the backend watcher.
    ///
    /// Closing first ensures a callback racing with backend teardown cannot append evidence after
    /// the lifecycle owner has retired this handle.
    pub fn close(&mut self) {
        self.signal.close();
        let backend = self.backend.take();
        drop(backend);
    }
}

impl Drop for SourceWatcher {
    fn drop(&mut self) {
        self.close();
    }
}

/// Why a source watcher could not be started.
#[derive(Debug)]
pub enum WatcherStartError {
    /// The source root was not an absolute canonical path.
    InvalidRoot(PathBuf),
    /// The notify backend could not be constructed.
    BackendInitialization(notify::Error),
    /// The recursive source-root watch could not be installed.
    RootWatch {
        /// Root path passed to notify.
        root: PathBuf,
        /// Backend error returned by notify.
        error: notify::Error,
    },
    /// The nonrecursive parent watch could not be installed.
    ParentWatch {
        /// Parent path passed to notify.
        parent: PathBuf,
        /// Backend error returned by notify.
        error: notify::Error,
    },
}

impl fmt::Display for WatcherStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoot(root) => {
                write!(
                    formatter,
                    "source watcher root is not canonical: {}",
                    root.display()
                )
            }
            Self::BackendInitialization(error) => {
                write!(
                    formatter,
                    "source watcher backend initialization failed: {error}"
                )
            }
            Self::RootWatch { root, error } => {
                write!(
                    formatter,
                    "source watcher could not watch root {}: {error}",
                    root.display()
                )
            }
            Self::ParentWatch { parent, error } => write!(
                formatter,
                "source watcher could not watch parent {}: {error}",
                parent.display()
            ),
        }
    }
}

impl std::error::Error for WatcherStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BackendInitialization(error)
            | Self::RootWatch { error, .. }
            | Self::ParentWatch { error, .. } => Some(error),
            Self::InvalidRoot(_) => None,
        }
    }
}

fn lock_unpoisoned(state: &Mutex<SignalState>) -> MutexGuard<'_, SignalState> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn validate_root(root: &Path) -> Result<(), WatcherStartError> {
    if !root.is_absolute()
        || root
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(WatcherStartError::InvalidRoot(root.to_path_buf()));
    }
    Ok(())
}

fn parent_to_watch(root: &Path) -> Option<PathBuf> {
    let parent = root.parent()?;
    (parent != root).then(|| parent.to_path_buf())
}

fn classify_event(spec: &SourceWatchSpec, event: &Event) -> WatchHintEvidence {
    // Notify uses this flag when event coverage is no longer trustworthy. It takes precedence
    // over path and kind filtering, including an Access or source-artifact envelope: the backend
    // has explicitly told us that a conservative source audit is required.
    if event.need_rescan() {
        return WatchHintEvidence::FULL_RESCAN;
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return WatchHintEvidence::EMPTY;
    }

    let mut evidence = WatchHintEvidence::EMPTY;
    for path in &event.paths {
        let relation = if path == spec.root() {
            PathRelation::Root
        } else if path
            .strip_prefix(spec.root())
            .is_ok_and(|relative| !relative.as_os_str().is_empty())
        {
            PathRelation::Descendant
        } else {
            PathRelation::Unrelated
        };

        match relation {
            PathRelation::Unrelated => {}
            PathRelation::Root => {
                evidence = evidence.union(classify_root_event(event.kind));
            }
            PathRelation::Descendant => {
                if is_ignored_file_name(path, spec.ignored_file_name_prefixes) {
                    continue;
                }
                evidence = evidence.union(classify_descendant_event(event.kind));
            }
        }
    }

    if event.paths.is_empty() && evidence.is_empty() {
        evidence = WatchHintEvidence::FULL_RESCAN;
    }
    evidence
}

#[derive(Clone, Copy)]
enum PathRelation {
    Root,
    Descendant,
    Unrelated,
}

fn classify_root_event(kind: EventKind) -> WatchHintEvidence {
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => WatchHintEvidence::FULL_RESCAN,
        EventKind::Modify(ModifyKind::Name(_)) => WatchHintEvidence::FULL_RESCAN,
        EventKind::Any | EventKind::Other | EventKind::Modify(_) => {
            WatchHintEvidence::ROOT_METADATA_AMBIGUOUS
        }
        EventKind::Access(_) => WatchHintEvidence::EMPTY,
    }
}

fn classify_descendant_event(kind: EventKind) -> WatchHintEvidence {
    match kind {
        EventKind::Create(_)
        | EventKind::Remove(_)
        | EventKind::Modify(ModifyKind::Data(_))
        | EventKind::Modify(ModifyKind::Metadata(_))
        | EventKind::Modify(ModifyKind::Name(_)) => WatchHintEvidence::DESCENDANT_CHANGED,
        EventKind::Modify(ModifyKind::Any) | EventKind::Modify(ModifyKind::Other) => {
            WatchHintEvidence::FULL_RESCAN
        }
        EventKind::Any | EventKind::Other => WatchHintEvidence::FULL_RESCAN,
        EventKind::Access(_) => WatchHintEvidence::EMPTY,
    }
}

fn is_ignored_file_name(path: &Path, prefixes: &[&str]) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| prefixes.iter().any(|prefix| name.starts_with(prefix)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{EventAttributes, Flag, MetadataKind};
    use std::fs;
    use std::thread;
    use std::time::Duration;

    const SOURCE_ID: &str = "source-watcher-tests";
    const CUSTOM_IGNORED_PREFIXES: &[&str] = &["ignored."];

    fn spec(root: &Path) -> SourceWatchSpec {
        SourceWatchSpec::new(
            SourceId::from_string(SOURCE_ID),
            7,
            root.to_path_buf(),
            Some(RootIdentity::from_bytes(b"root-identity".to_vec())),
        )
    }

    fn event(kind: EventKind, paths: &[&Path]) -> Event {
        Event {
            kind,
            paths: paths.iter().map(|path| (*path).to_path_buf()).collect(),
            attrs: EventAttributes::default(),
        }
    }

    #[test]
    fn detached_signal_coalesces_evidence_and_preserves_bounds() {
        let root = PathBuf::from("/tmp/source-watcher-test-root");
        let signal = WatcherSignal::new(spec(&root));
        let first = Instant::now();
        let last = first + Duration::from_millis(23);

        assert!(signal.record_evidence(WatchHintEvidence::DESCENDANT_CHANGED, first));
        assert!(signal.record_evidence(WatchHintEvidence::ROOT_METADATA_AMBIGUOUS, last));

        let hint = signal.take_hint().expect("coalesced hint");
        assert_eq!(hint.source_id().as_str(), SOURCE_ID);
        assert_eq!(hint.lifecycle_generation(), 7);
        assert_eq!(hint.first_observed(), first);
        assert_eq!(hint.last_observed(), last);
        assert!(hint.evidence().has_descendant_change());
        assert!(hint.evidence().has_root_metadata_ambiguity());
        assert_eq!(MAX_PENDING_HINTS_PER_HANDLE, 1);
        assert!(signal.take_hint().is_none());
    }

    #[test]
    fn access_and_source_artifact_events_are_filtered_lexically() {
        let root = PathBuf::from("/tmp/source-watcher-test-root");
        let signal = WatcherSignal::new(spec(&root));
        let access_path = root.join("kick.wav");
        signal.record_notify_result(
            Ok(event(
                EventKind::Access(notify::event::AccessKind::Read),
                &[&access_path],
            )),
            Instant::now(),
        );
        assert!(signal.take_hint().is_none());

        for name in [
            ".yield.db",
            ".yield.db-wal",
            ".yield_samples.db-shm",
            "._kick.wav",
        ] {
            let path = root.join(name);
            signal.record_notify_result(
                Ok(event(
                    EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
                    &[&path],
                )),
                Instant::now(),
            );
        }
        assert!(signal.take_hint().is_none());

        let audio_path = root.join("kick.wav");
        signal.record_notify_result(
            Ok(event(
                EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
                &[&audio_path],
            )),
            Instant::now(),
        );
        assert!(
            signal
                .take_hint()
                .expect("audio change hint")
                .evidence()
                .has_descendant_change()
        );
    }

    #[test]
    fn parent_siblings_are_ignored_and_root_evidence_is_separate() {
        let root = PathBuf::from("/tmp/source-watcher-test-root");
        let signal = WatcherSignal::new(spec(&root));
        let sibling = PathBuf::from("/tmp/other-source/kick.wav");
        signal.record_notify_result(
            Ok(event(
                EventKind::Create(notify::event::CreateKind::File),
                &[&sibling],
            )),
            Instant::now(),
        );
        assert!(signal.take_hint().is_none());

        signal.record_notify_result(
            Ok(event(
                EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
                &[&root],
            )),
            Instant::now(),
        );
        let hint = signal.take_hint().expect("root ambiguity hint");
        assert!(hint.evidence().has_root_metadata_ambiguity());
        assert!(!hint.evidence().has_descendant_change());
        assert!(!hint.evidence().requires_full_rescan());

        signal.record_notify_result(Ok(event(EventKind::Any, &[&root])), Instant::now());
        let hint = signal.take_hint().expect("ambiguous root kind hint");
        assert!(hint.evidence().has_root_metadata_ambiguity());
        assert!(!hint.evidence().requires_full_rescan());

        signal.record_notify_result(
            Ok(event(
                EventKind::Remove(notify::event::RemoveKind::Folder),
                &[&root],
            )),
            Instant::now(),
        );
        assert!(
            signal
                .take_hint()
                .expect("root replacement hint")
                .evidence()
                .requires_full_rescan()
        );
    }

    #[test]
    fn notify_rescan_and_backend_errors_are_conservative_without_retaining_paths() {
        let root = PathBuf::from("/tmp/source-watcher-test-root");
        let signal = WatcherSignal::new(spec(&root));
        let path = root.join("kick.wav");
        let mut rescan = event(EventKind::Modify(ModifyKind::Any), &[&path]);
        rescan.attrs = EventAttributes::default();
        rescan.attrs.set_flag(Flag::Rescan);
        signal.record_notify_result(Ok(rescan), Instant::now());
        assert!(
            signal
                .take_hint()
                .expect("unknown event hint")
                .evidence()
                .requires_full_rescan()
        );

        signal.record_notify_result(
            Err(notify::Error::generic("test backend error").add_path(root.join("unbounded"))),
            Instant::now(),
        );
        let hint = signal.take_hint().expect("backend error hint");
        assert!(hint.evidence().has_backend_error());
        assert!(hint.evidence().requires_full_rescan());
    }

    #[test]
    fn rescan_flag_overrides_access_artifact_and_sibling_filtering() {
        let root = PathBuf::from("/tmp/source-watcher-test-root");
        let signal = WatcherSignal::new(spec(&root));
        let mut access = event(
            EventKind::Access(notify::event::AccessKind::Read),
            &[&root.join("kick.wav")],
        );
        access.attrs.set_flag(Flag::Rescan);
        signal.record_notify_result(Ok(access), Instant::now());
        assert!(
            signal
                .take_hint()
                .expect("access rescan hint")
                .evidence()
                .requires_full_rescan()
        );

        let mut artifact = event(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            &[&root.join(".yield.db-wal")],
        );
        artifact.attrs.set_flag(Flag::Rescan);
        signal.record_notify_result(Ok(artifact), Instant::now());
        assert!(
            signal
                .take_hint()
                .expect("artifact rescan hint")
                .evidence()
                .requires_full_rescan()
        );

        let mut sibling = event(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            &[&PathBuf::from("/tmp/other-source/kick.wav")],
        );
        sibling.attrs.set_flag(Flag::Rescan);
        signal.record_notify_result(Ok(sibling), Instant::now());
        assert!(
            signal
                .take_hint()
                .expect("sibling rescan hint")
                .evidence()
                .requires_full_rescan()
        );
    }

    #[test]
    fn custom_artifact_prefixes_are_accepted() {
        let root = PathBuf::from("/tmp/source-watcher-test-root");
        let spec = spec(&root).with_ignored_file_name_prefixes(CUSTOM_IGNORED_PREFIXES);
        let signal = WatcherSignal::new(spec);
        let ignored = root.join("ignored.wav");
        signal.record_notify_result(
            Ok(event(
                EventKind::Create(notify::event::CreateKind::File),
                &[&ignored],
            )),
            Instant::now(),
        );
        assert!(signal.take_hint().is_none());
    }

    #[test]
    fn closed_signal_rejects_late_callback_and_keeps_prior_hint() {
        let root = PathBuf::from("/tmp/source-watcher-test-root");
        let signal = WatcherSignal::new(spec(&root));
        let path = root.join("kick.wav");
        signal.record_notify_result(
            Ok(event(
                EventKind::Create(notify::event::CreateKind::File),
                &[&path],
            )),
            Instant::now(),
        );
        signal.close();
        assert!(signal.is_closed());
        assert!(!signal.record_evidence(WatchHintEvidence::FULL_RESCAN, Instant::now()));
        assert!(signal.take_hint().is_some());
    }

    #[test]
    fn native_temp_root_reports_descendant_change() {
        let root = tempfile::tempdir().expect("temporary source root");
        let canonical_root = root.path().canonicalize().expect("canonical source root");
        let watcher = SourceWatcher::start(spec(&canonical_root)).expect("native watcher");
        fs::write(canonical_root.join("kick.wav"), b"yield watcher smoke").expect("write file");

        let deadline = Instant::now() + Duration::from_secs(20);
        let accepted_evidence = loop {
            if let Some(hint) = watcher.take_hint() {
                let evidence = hint.evidence();
                // Some native backends report a root-only metadata event before the more precise
                // child event. Drain that coalesced lifecycle signal and accept a conservative
                // full-rescan fallback when the backend does not preserve child paths.
                if evidence.has_descendant_change() || evidence.requires_full_rescan() {
                    break true;
                }
            }
            assert!(Instant::now() < deadline, "native watcher produced no hint");
            thread::sleep(Duration::from_millis(25));
        };
        assert!(accepted_evidence);
    }
}
