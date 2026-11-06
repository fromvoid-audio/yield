//! Original catalog ownership of macOS watcher children across source replacement.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use yield_source_watcher::macos_live::{MacosSourceWatcherSlot, WatcherSlotProgress};

use super::{install_watcher, watcher_spec, *};
use crate::{SourceId, admission_state, publication_is_current_state};

const MAX_HELPERS: usize = 32;
const START_TIMEOUT: Duration = Duration::from_secs(2);
const RETIRE_GRACE: Duration = Duration::from_millis(250);
const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) struct Supervisor {
    executable: Option<PathBuf>,
    slots: HashMap<SourceId, ChildSlot>,
}

struct ChildSlot {
    spec: SourceWatchSpec,
    child: MacosSourceWatcherSlot,
    retire_after: Option<Instant>,
    active_poll_after: Option<Instant>,
}

impl Supervisor {
    pub(crate) fn new(executable: Option<PathBuf>) -> Self {
        Self {
            executable: executable.or_else(application_helper),
            slots: HashMap::new(),
        }
    }

    pub(crate) fn poll(
        &mut self,
        admission: &Arc<Mutex<AdmissionState>>,
        sources: &mut HashMap<SourceId, RegisteredSource>,
        test_gate: Option<&Arc<CatalogTestGate>>,
        closing: bool,
        allow_start: bool,
    ) {
        let now = Instant::now();
        self.slots.retain(|id, slot| {
            let generation = slot.spec.lifecycle_generation();
            let mut source = sources
                .get_mut(id)
                .filter(|source| source.lifecycle_generation == generation);
            let current = !closing && publication_is_current(admission, id, generation, None);
            if !current {
                slot.retire(now, closing);
            }
            if slot.retire_after.is_none()
                && slot
                    .active_poll_after
                    .is_none_or(|deadline| now >= deadline)
                && let Some(source) = source.as_deref_mut()
            {
                slot.poll_current(id, source, admission, test_gate, now);
            }
            // An otherwise current Source can be parked by its WAL owner. It stays absent
            // from this SQL map temporarily while the original child remains in this slot.
            if let Some(deadline) = slot.retire_after {
                if let Some(source) = source {
                    source.watcher.take();
                }
                if slot.child.try_reap().ok().flatten().is_some() {
                    return false;
                }
                if now >= deadline {
                    let _ = slot.child.force_termination();
                }
            }
            true
        });
        if closing || !allow_start || self.slots.len() >= MAX_HELPERS {
            return;
        }
        let Some((id, source)) = sources.iter_mut().find(|(id, source)| {
            source.watcher.is_none()
                && !self.slots.contains_key(*id)
                && source.watcher_schedule.watcher_can_retry(now)
                && publication_is_current(admission, id, source.lifecycle_generation, None)
        }) else {
            return;
        };
        let spec = watcher_spec(id, source);
        let child = self.executable.as_ref().and_then(|path| {
            MacosSourceWatcherSlot::start(path, spec.clone(), now + START_TIMEOUT).ok()
        });
        if let Some(child) = child {
            self.slots.insert(
                id.clone(),
                ChildSlot {
                    spec,
                    child,
                    retire_after: None,
                    active_poll_after: None,
                },
            );
        } else {
            source.watcher_schedule.watcher_start_failed(now);
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        // Signal every exact child before any fallback Drop can wait for a kernel exit.
        for slot in self.slots.values_mut() {
            let _ = slot.child.force_termination();
        }
    }
}

impl ChildSlot {
    fn retire(&mut self, now: Instant, closing: bool) {
        self.child.begin_retirement();
        if closing {
            self.retire_after = Some(now);
        } else {
            self.retire_after.get_or_insert(now + RETIRE_GRACE);
        }
    }

    fn poll_current(
        &mut self,
        id: &SourceId,
        source: &mut RegisteredSource,
        admission: &Arc<Mutex<AdmissionState>>,
        test_gate: Option<&Arc<CatalogTestGate>>,
        now: Instant,
    ) {
        if self.active_poll_after.is_some() && source.watcher.is_none() {
            // The existing hint drain already closed this bridge and scheduled recovery.
            self.retire(now, false);
            return;
        }
        let exact = self.spec.root() == source.root
            && self.spec.root_identity() == source.known_root_identity.as_ref();
        match self.child.poll(exact.then_some(&self.spec)) {
            Ok(WatcherSlotProgress::Pending) => {}
            Ok(WatcherSlotProgress::Ready(watcher)) => {
                let installed = admission_state(admission, |state| {
                    if !publication_is_current_state(state, id, source.lifecycle_generation, None) {
                        return false;
                    }
                    install_watcher(id, source, watcher, test_gate, now);
                    true
                });
                if installed {
                    self.active_poll_after = Some(now);
                } else {
                    self.retire(now, false);
                }
            }
            Ok(WatcherSlotProgress::Active) => {
                self.active_poll_after = Some(now + ACTIVE_POLL_INTERVAL);
            }
            Ok(WatcherSlotProgress::Retiring) | Err(_) => {
                if source.watcher.is_some() {
                    source.watcher_schedule.watcher_backend_failed(now);
                } else {
                    source.watcher_schedule.watcher_start_failed(now);
                }
                self.retire(now, false);
            }
        }
    }
}

fn application_helper() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let helper = executable.with_file_name("yield-live-watch-helper");
    let metadata = fs::symlink_metadata(&helper).ok()?;
    (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(helper)
}
