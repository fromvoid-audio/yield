//! Source-bound handoff from the helper to the existing coalesced signal.

use std::io;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Instant;

use super::{
    MacosWatcherProcess, SourceWatchSpec, WatchHintEvidence, WatcherSignal, root_identity,
};
use crate::{DEFAULT_IGNORED_FILE_NAME_PREFIXES, SourceWatcher};

enum Phase {
    Starting {
        bridge: SourceWatcher,
        root: (u64, u64),
        deadline: Instant,
    },
    Active,
    Retiring,
}

/// Progress for the one original source-bound helper.
pub enum WatcherSlotProgress {
    /// Checked readiness is pending.
    Pending,
    /// The original source can install this existing coalesced watcher signal.
    Ready(SourceWatcher),
    /// The installed bridge remains active and may have a new coalesced hint.
    Active,
    /// Admission is closed. The caller keeps the original slot until reap.
    Retiring,
}

/// One helper bound to the source, root and lifecycle generation it started for.
pub struct MacosSourceWatcherSlot {
    spec: SourceWatchSpec,
    signal: WatcherSignal,
    phase: Phase,
    process: MacosWatcherProcess,
}

impl MacosSourceWatcherSlot {
    /// Start the selected helper for the original source and its native root.
    ///
    /// The version 1 executable uses the default artifact filters. Custom filters require
    /// a distinct protocol. Filesystem identity lookup and OS spawn run on the caller's
    /// worker. Native stream readiness is polled separately.
    pub fn start(executable: &Path, spec: SourceWatchSpec, deadline: Instant) -> io::Result<Self> {
        if spec.ignored_file_name_prefixes() != DEFAULT_IGNORED_FILE_NAME_PREFIXES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "macOS watcher helper requires default artifact filters",
            ));
        }
        let root = root_identity(&spec)?;
        let process = MacosWatcherProcess::spawn(executable, spec.root(), deadline)?;
        let bridge = SourceWatcher::detached(spec.clone());
        let signal = bridge.signal();
        Ok(Self {
            spec,
            signal,
            phase: Phase::Starting {
                bridge,
                root,
                deadline,
            },
            process,
        })
    }

    /// Poll against the catalog's currently admitted source, or retire when it is absent.
    ///
    /// A checked readiness reply becomes installable only after rechecking the original
    /// native root. Source removal, supersession or a closed bridge invalidates this slot.
    /// A process failure closes admission and preserves conservative evidence on an active bridge.
    pub fn poll(&mut self, current: Option<&SourceWatchSpec>) -> io::Result<WatcherSlotProgress> {
        if matches!(self.phase, Phase::Retiring)
            || current != Some(&self.spec)
            || self.signal.is_closed()
        {
            self.begin_retirement();
            return Ok(WatcherSlotProgress::Retiring);
        }
        let result = self.poll_current();
        if result.is_err() {
            self.fail_and_retire();
        }
        result
    }

    fn poll_current(&mut self) -> io::Result<WatcherSlotProgress> {
        match self.phase {
            Phase::Starting { root, deadline, .. } => {
                if !self.process.poll_ready()? {
                    return Ok(WatcherSlotProgress::Pending);
                }
                if root_identity(&self.spec)? != root {
                    return Err(io::Error::other(
                        "source root changed before watcher admission",
                    ));
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "source watcher readiness expired before admission",
                    ));
                }
                let Phase::Starting { bridge, .. } =
                    std::mem::replace(&mut self.phase, Phase::Active)
                else {
                    unreachable!("only the original pending bridge can become active");
                };
                Ok(WatcherSlotProgress::Ready(bridge))
            }
            Phase::Active => {
                if let Some(evidence) = self.process.poll_hint()? {
                    self.signal.record_evidence(evidence, Instant::now());
                }
                Ok(WatcherSlotProgress::Active)
            }
            Phase::Retiring => Ok(WatcherSlotProgress::Retiring),
        }
    }

    /// Close the coalesced signal and request graceful retirement while keeping custody.
    pub fn begin_retirement(&mut self) {
        self.signal.close();
        self.phase = Phase::Retiring;
        self.process.begin_retirement();
    }

    /// Signal the original child without waiting. Continue to own the slot until reap.
    pub fn force_termination(&mut self) -> io::Result<()> {
        self.begin_retirement();
        self.process.force_termination()
    }

    /// Observe actual process exit, keeping conservative evidence for unexpected active exit.
    pub fn try_reap(&mut self) -> io::Result<Option<ExitStatus>> {
        let result = self.process.try_reap();
        if !matches!(result, Ok(None)) {
            self.fail_and_retire();
        }
        result
    }

    fn fail_and_retire(&mut self) {
        if matches!(self.phase, Phase::Active) {
            self.signal.record_evidence(
                WatchHintEvidence::FULL_RESCAN.union(WatchHintEvidence::BACKEND_ERROR),
                Instant::now(),
            );
        }
        self.begin_retirement();
    }
}

impl Drop for MacosSourceWatcherSlot {
    fn drop(&mut self) {
        self.begin_retirement();
    }
}
