use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wavecrate_source_watcher::{
    SourceWatchHint, SourceWatchSpec, SourceWatcher, WatchHintEvidence,
};

use super::{
    AdmissionState, CatalogTestGate, EventHub, EventReservation, RegisteredSource,
    WritableProfileGuard, publication_is_current, request_audit, source_root_identity,
};

const WATCHER_DEBOUNCE: Duration = Duration::from_millis(400);
const WATCHER_MAX_DELAY: Duration = Duration::from_secs(2);
const WATCHER_RETRY_MIN: Duration = Duration::from_secs(1);
const WATCHER_RETRY_MAX: Duration = Duration::from_secs(60);
const WATCHER_FALLBACK_INTERVAL: Duration = Duration::from_secs(30);
const ROOT_PROBE_ACTIVE: Duration = Duration::from_secs(10);
const ROOT_PROBE_MISSING: Duration = Duration::from_secs(30);

#[allow(dead_code)]
#[derive(Clone, Copy)]
pub(super) enum WatcherMode {
    Native,
    Detached,
}

pub(super) struct WatcherSchedule {
    pending: Option<PendingWatcherHint>,
    initial_audit_pending: bool,
    watcher_retry_after: Option<Instant>,
    watcher_retry_delay: Duration,
    audit_retry_after: Option<Instant>,
    audit_retry_delay: Duration,
    fallback_audit_after: Option<Instant>,
    next_root_probe: Instant,
    root_issue: Option<RootProbeIssue>,
    root_issue_reported: bool,
    watcher_continuity_lost: bool,
}

struct PendingWatcherHint {
    first_observed: Instant,
    last_observed: Instant,
    evidence: WatchHintEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RootProbeIssue {
    Missing,
    Changed(super::RootIdentity),
}

impl WatcherSchedule {
    pub(super) fn new(initial_audit_pending: bool) -> Self {
        Self {
            pending: None,
            initial_audit_pending,
            watcher_retry_after: None,
            watcher_retry_delay: WATCHER_RETRY_MIN,
            audit_retry_after: None,
            audit_retry_delay: WATCHER_RETRY_MIN,
            fallback_audit_after: None,
            next_root_probe: Instant::now(),
            root_issue: None,
            root_issue_reported: false,
            watcher_continuity_lost: false,
        }
    }

    pub(super) fn initial_audit_complete(&mut self) {
        self.initial_audit_pending = false;
    }

    fn root_probe_due(&self, now: Instant) -> bool {
        now >= self.next_root_probe
    }

    fn observe_root(
        &mut self,
        now: Instant,
        current: Option<super::RootIdentity>,
        known: Option<&super::RootIdentity>,
    ) {
        self.next_root_probe = now
            + if current.is_some() {
                ROOT_PROBE_ACTIVE
            } else {
                ROOT_PROBE_MISSING
            };
        let issue = match (known, current) {
            (None, _) => None,
            (Some(known), Some(identity)) if known == &identity => None,
            (Some(_), Some(identity)) => Some(RootProbeIssue::Changed(identity)),
            (Some(_), None) => Some(RootProbeIssue::Missing),
        };
        if issue != self.root_issue {
            let recovered = self.root_issue.is_some() && issue.is_none();
            self.root_issue = issue;
            self.root_issue_reported = false;
            if recovered || self.root_issue.is_some() {
                self.pending = Some(PendingWatcherHint {
                    first_observed: now,
                    last_observed: now,
                    evidence: WatchHintEvidence::FULL_RESCAN,
                });
                if self.watcher_continuity_lost {
                    self.fallback_audit_after = Some(now);
                }
            }
        }
    }

    fn root_issue_blocks_audit(&self) -> bool {
        self.root_issue_reported && self.root_issue.is_some()
    }

    fn root_stale_reported(
        &mut self,
        now: Instant,
        current: Option<super::RootIdentity>,
        known: Option<&super::RootIdentity>,
    ) {
        let current_present = current.is_some();
        let issue = match (known, current) {
            (Some(known), Some(identity)) if known != &identity => {
                RootProbeIssue::Changed(identity)
            }
            _ => RootProbeIssue::Missing,
        };
        self.root_issue = Some(issue);
        self.root_issue_reported = true;
        self.pending = None;
        self.audit_retry_after = None;
        self.fallback_audit_after = None;
        self.next_root_probe = now
            + if current_present {
                ROOT_PROBE_ACTIVE
            } else {
                ROOT_PROBE_MISSING
            };
    }

    fn merge_pending(&mut self, pending: PendingWatcherHint) {
        if let Some(existing) = &mut self.pending {
            existing.first_observed = existing.first_observed.min(pending.first_observed);
            existing.last_observed = existing.last_observed.max(pending.last_observed);
            existing.evidence = existing.evidence.union(pending.evidence);
        } else {
            self.pending = Some(pending);
        }
    }

    fn pending_is_due(&self, now: Instant) -> bool {
        let Some(pending) = &self.pending else {
            return false;
        };
        if self
            .audit_retry_after
            .is_some_and(|retry_after| now < retry_after)
        {
            return false;
        }
        now.duration_since(pending.last_observed) >= WATCHER_DEBOUNCE
            || now.duration_since(pending.first_observed) >= WATCHER_MAX_DELAY
    }

    fn watcher_can_retry(&self, now: Instant) -> bool {
        self.watcher_retry_after
            .is_none_or(|retry_after| now >= retry_after)
    }

    fn fallback_audit_due(&self, now: Instant) -> bool {
        self.watcher_continuity_lost
            && self
                .fallback_audit_after
                .is_some_and(|fallback_after| now >= fallback_after)
    }

    fn take_fallback_pending(&mut self, now: Instant) -> Option<PendingWatcherHint> {
        if !self.fallback_audit_due(now) {
            return None;
        }
        self.fallback_audit_after = None;
        Some(self.pending.take().unwrap_or(PendingWatcherHint {
            first_observed: now,
            last_observed: now,
            evidence: WatchHintEvidence::FULL_RESCAN,
        }))
    }

    pub(super) fn watcher_started(&mut self, now: Instant) {
        self.watcher_retry_after = None;
        self.watcher_retry_delay = WATCHER_RETRY_MIN;
        self.fallback_audit_after = None;
        if self.watcher_continuity_lost && self.pending.is_none() {
            self.pending = Some(PendingWatcherHint {
                first_observed: now,
                last_observed: now,
                evidence: WatchHintEvidence::FULL_RESCAN,
            });
        }
    }

    pub(super) fn watcher_start_failed(&mut self, now: Instant) {
        self.watcher_continuity_lost = true;
        if self.watcher_retry_after.is_none() && self.pending.is_none() {
            self.pending = Some(PendingWatcherHint {
                first_observed: now,
                last_observed: now,
                evidence: WatchHintEvidence::FULL_RESCAN,
            });
        }
        self.watcher_retry_after = Some(now + self.watcher_retry_delay);
        if self.fallback_audit_after.is_none() {
            self.fallback_audit_after = Some(now + WATCHER_FALLBACK_INTERVAL);
        }
        self.watcher_retry_delay = self
            .watcher_retry_delay
            .saturating_mul(2)
            .min(WATCHER_RETRY_MAX);
    }

    fn watcher_backend_failed(&mut self, now: Instant) {
        self.watcher_continuity_lost = true;
        self.pending = Some(PendingWatcherHint {
            first_observed: now,
            last_observed: now,
            evidence: WatchHintEvidence::FULL_RESCAN,
        });
        self.watcher_retry_after = Some(now + self.watcher_retry_delay);
        self.fallback_audit_after = Some(now + WATCHER_FALLBACK_INTERVAL);
        self.watcher_retry_delay = self
            .watcher_retry_delay
            .saturating_mul(2)
            .min(WATCHER_RETRY_MAX);
    }

    fn audit_succeeded(&mut self, watcher_active: bool, completed_at: Instant) {
        self.audit_retry_after = None;
        self.audit_retry_delay = WATCHER_RETRY_MIN;
        if watcher_active {
            self.watcher_continuity_lost = false;
            self.fallback_audit_after = None;
        } else if self.watcher_continuity_lost {
            self.fallback_audit_after = Some(completed_at + WATCHER_FALLBACK_INTERVAL);
        }
    }

    fn audit_failed(&mut self, pending: PendingWatcherHint, now: Instant) {
        self.merge_pending(pending);
        self.audit_retry_after = Some(now + self.audit_retry_delay);
        if self.watcher_continuity_lost {
            self.fallback_audit_after = Some(now + WATCHER_FALLBACK_INTERVAL);
        }
        self.audit_retry_delay = self
            .audit_retry_delay
            .saturating_mul(2)
            .min(WATCHER_RETRY_MAX);
    }

    fn audit_canceled(&mut self, pending: PendingWatcherHint, now: Instant) {
        self.merge_pending(pending);
        self.audit_retry_after = Some(now + WATCHER_DEBOUNCE);
        if self.watcher_continuity_lost {
            self.fallback_audit_after = Some(now + WATCHER_FALLBACK_INTERVAL);
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum AuditTrigger {
    Explicit,
    Automatic,
}

#[derive(Clone, Copy)]
pub(super) enum AuditDisposition {
    Committed,
    Failed { retryable: bool, root_stale: bool },
    Canceled,
}

pub(super) fn run_watcher_idle_work(
    profile_guard: &WritableProfileGuard,
    events: &Arc<EventHub>,
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<super::SourceId, RegisteredSource>,
    test_gate: Option<&Arc<CatalogTestGate>>,
    watcher_mode: WatcherMode,
) {
    let now = Instant::now();
    probe_source_roots(sources, now);
    refresh_source_watchers(sources, test_gate, watcher_mode, now);
    drain_source_watcher_hints(sources, now);
    let Some((source_id, lifecycle_generation, pending)) =
        claim_watcher_audit(admission, sources, now)
    else {
        return;
    };

    let mut reservation = EventReservation {
        hub: events,
        admission,
        remaining: 1,
        curation_payload_bytes: 0,
        curation_update_count: 0,
        tag_filter_payload_bytes: 0,
    };
    let disposition = request_audit(
        source_id.clone(),
        lifecycle_generation,
        profile_guard,
        &mut reservation,
        admission,
        sources,
        AuditTrigger::Automatic,
        test_gate,
    );
    reservation.finish();
    let completed_at = Instant::now();

    if let Some(source) = sources.get_mut(&source_id) {
        match disposition {
            AuditDisposition::Committed => {
                source
                    .watcher_schedule
                    .audit_succeeded(source.watcher.is_some(), completed_at);
            }
            AuditDisposition::Failed {
                retryable: true, ..
            } => source.watcher_schedule.audit_failed(pending, completed_at),
            AuditDisposition::Failed {
                retryable: false,
                root_stale: true,
            } => {
                let current = source_root_identity(&source.root).ok();
                let known = source.known_root_identity.clone();
                source
                    .watcher_schedule
                    .root_stale_reported(completed_at, current, known.as_ref());
            }
            AuditDisposition::Failed {
                retryable: false,
                root_stale: false,
            } => {
                source
                    .watcher_schedule
                    .audit_succeeded(source.watcher.is_some(), completed_at);
            }
            AuditDisposition::Canceled => {
                if publication_is_current(admission, &source_id, lifecycle_generation, None) {
                    source
                        .watcher_schedule
                        .audit_canceled(pending, completed_at);
                }
            }
        }
    }
}

fn probe_source_roots(sources: &mut HashMap<super::SourceId, RegisteredSource>, now: Instant) {
    for source in sources.values_mut() {
        if !source.watcher_schedule.root_probe_due(now) {
            continue;
        }
        let known = source.known_root_identity.clone();
        let current = source_root_identity(&source.root).ok();
        source
            .watcher_schedule
            .observe_root(now, current, known.as_ref());
    }
}

fn refresh_source_watchers(
    sources: &mut HashMap<super::SourceId, RegisteredSource>,
    test_gate: Option<&Arc<CatalogTestGate>>,
    watcher_mode: WatcherMode,
    now: Instant,
) {
    for (source_id, source) in sources.iter_mut() {
        if source.watcher.is_some() || !source.watcher_schedule.watcher_can_retry(now) {
            continue;
        }
        let spec = SourceWatchSpec::new(
            source_id.clone(),
            source.lifecycle_generation,
            source.root.clone(),
            source.known_root_identity.clone(),
        );
        let watcher = match watcher_mode {
            WatcherMode::Native => SourceWatcher::start(spec),
            WatcherMode::Detached => Ok(SourceWatcher::detached(spec)),
        };
        match watcher {
            Ok(watcher) => {
                let signal = watcher.signal();
                source.watcher = Some(watcher);
                source.watcher_schedule.watcher_started(now);
                if let Some(test_gate) = test_gate {
                    test_gate.install_watcher_signal(
                        source_id.clone(),
                        source.lifecycle_generation,
                        signal,
                    );
                }
            }
            Err(_) => source.watcher_schedule.watcher_start_failed(now),
        }
    }
}

fn drain_source_watcher_hints(
    sources: &mut HashMap<super::SourceId, RegisteredSource>,
    now: Instant,
) {
    for (source_id, source) in sources.iter_mut() {
        let Some(hint) = source.watcher.as_ref().and_then(SourceWatcher::take_hint) else {
            continue;
        };
        if hint.source_id() != source_id
            || hint.lifecycle_generation() != source.lifecycle_generation
        {
            continue;
        }
        let backend_failed = hint.evidence().has_backend_error();
        merge_source_watcher_hint(source, &hint);
        if backend_failed {
            if let Some(mut watcher) = source.watcher.take() {
                watcher.close();
            }
            source.watcher_schedule.watcher_backend_failed(now);
        }
    }
}

fn merge_source_watcher_hint(source: &mut RegisteredSource, hint: &SourceWatchHint) {
    let evidence = hint.evidence();
    let root_only_ambiguity = evidence.has_root_metadata_ambiguity()
        && !evidence.has_descendant_change()
        && !evidence.requires_full_rescan()
        && !evidence.has_backend_error();
    if root_only_ambiguity {
        let unchanged = source_root_identity(&source.root)
            .ok()
            .is_some_and(|identity| source.known_root_identity.as_ref() == Some(&identity));
        if unchanged {
            return;
        }
    }
    source.watcher_schedule.merge_pending(PendingWatcherHint {
        first_observed: hint.first_observed(),
        last_observed: hint.last_observed(),
        evidence,
    });
}

fn claim_watcher_audit(
    admission: &Arc<Mutex<AdmissionState>>,
    sources: &mut HashMap<super::SourceId, RegisteredSource>,
    now: Instant,
) -> Option<(super::SourceId, u64, PendingWatcherHint)> {
    let candidate = sources.iter().find_map(|(source_id, source)| {
        if source.watcher_schedule.initial_audit_pending
            || source.watcher_schedule.root_issue_blocks_audit()
        {
            return None;
        }
        let watcher_active = source.watcher.is_some();
        let due = if watcher_active {
            source.watcher_schedule.pending_is_due(now)
        } else {
            source.watcher_schedule.fallback_audit_due(now)
        };
        if !due {
            return None;
        }
        Some((source_id.clone(), source.lifecycle_generation))
    })?;
    let (source_id, lifecycle_generation) = candidate;
    let mut state = admission
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.shutdown || state.closed || state.event_budget != 1 {
        return None;
    }
    let Some(desired) = state.sources.get(&source_id) else {
        return None;
    };
    if !desired.active || desired.lifecycle_generation != lifecycle_generation {
        return None;
    }
    let Some(source) = sources.get_mut(&source_id) else {
        return None;
    };
    let pending = if source.watcher.is_some() {
        source.watcher_schedule.pending.take()?
    } else {
        source.watcher_schedule.take_fallback_pending(now)?
    };
    state.event_budget += 1;
    Some((source_id, lifecycle_generation, pending))
}

#[cfg(test)]
mod tests {
    use super::super::RootIdentity;
    use super::*;

    #[test]
    fn schedule_debounces_and_applies_bounded_retry_backoff() {
        let start = Instant::now();
        let mut schedule = WatcherSchedule::new(false);
        schedule.merge_pending(PendingWatcherHint {
            first_observed: start,
            last_observed: start,
            evidence: WatchHintEvidence::DESCENDANT_CHANGED,
        });
        assert!(!schedule.pending_is_due(start + Duration::from_millis(399)));
        assert!(schedule.pending_is_due(start + Duration::from_millis(400)));

        let pending = schedule.pending.take().expect("pending hint");
        let completed = start + Duration::from_secs(10);
        schedule.audit_failed(pending, completed);
        assert_eq!(
            schedule.audit_retry_after,
            Some(completed + WATCHER_RETRY_MIN)
        );
        assert!(!schedule.pending_is_due(completed + WATCHER_RETRY_MIN - Duration::from_nanos(1)));
        assert!(schedule.pending_is_due(completed + WATCHER_RETRY_MIN));
        assert_eq!(schedule.audit_retry_delay, Duration::from_secs(2));
    }

    #[test]
    fn unwatched_gap_survives_audit_until_watcher_restarts() {
        let start = Instant::now();
        let mut schedule = WatcherSchedule::new(false);
        schedule.watcher_start_failed(start);
        assert!(schedule.watcher_continuity_lost);
        assert!(schedule.pending.is_some());

        schedule.pending.take();
        schedule.audit_succeeded(false, start + Duration::from_secs(1));
        assert!(schedule.watcher_continuity_lost);
        assert!(schedule.pending.is_none());

        schedule.watcher_started(start + WATCHER_RETRY_MIN);
        assert!(schedule.watcher_continuity_lost);
        assert!(schedule.pending.is_some());
        schedule.pending.take();
        schedule.audit_succeeded(true, start + WATCHER_RETRY_MIN);
        assert!(!schedule.watcher_continuity_lost);
    }

    #[test]
    fn unavailable_watcher_uses_bounded_periodic_fallback_audits() {
        let start = Instant::now();
        let mut schedule = WatcherSchedule::new(false);
        schedule.watcher_start_failed(start);
        schedule.pending.take();

        let completed = start + Duration::from_secs(1);
        schedule.audit_succeeded(false, completed);
        assert!(
            !schedule.fallback_audit_due(
                completed + WATCHER_FALLBACK_INTERVAL - Duration::from_nanos(1)
            )
        );
        let due = completed + WATCHER_FALLBACK_INTERVAL;
        assert!(schedule.fallback_audit_due(due));
        let pending = schedule
            .take_fallback_pending(due)
            .expect("fallback audit pending");
        assert!(pending.evidence.requires_full_rescan());
        assert!(schedule.fallback_audit_after.is_none());

        schedule.audit_succeeded(false, due + Duration::from_secs(1));
        assert!(!schedule.fallback_audit_due(
            due + Duration::from_secs(1) + WATCHER_FALLBACK_INTERVAL - Duration::from_nanos(1)
        ));
        assert!(
            schedule.fallback_audit_due(due + Duration::from_secs(1) + WATCHER_FALLBACK_INTERVAL)
        );
    }

    #[test]
    fn root_probe_reports_once_and_reopens_after_known_identity_returns() {
        let start = Instant::now();
        let known = RootIdentity::from_bytes(b"known".to_vec());
        let replacement = RootIdentity::from_bytes(b"replacement".to_vec());
        let mut schedule = WatcherSchedule::new(false);

        schedule.observe_root(start, Some(replacement.clone()), Some(&known));
        assert!(schedule.pending.is_some());
        schedule.pending.take();
        schedule.root_stale_reported(start, Some(replacement.clone()), Some(&known));
        assert!(schedule.root_issue_blocks_audit());
        schedule.observe_root(start + ROOT_PROBE_ACTIVE, Some(replacement), Some(&known));
        assert!(schedule.pending.is_none());
        assert!(schedule.root_issue_blocks_audit());

        schedule.observe_root(
            start + ROOT_PROBE_ACTIVE + ROOT_PROBE_ACTIVE,
            Some(known.clone()),
            Some(&known),
        );
        assert!(!schedule.root_issue_blocks_audit());
        assert!(schedule.pending.is_some());

        let mut missing_schedule = WatcherSchedule::new(false);
        missing_schedule.observe_root(start, None, Some(&known));
        assert!(missing_schedule.pending.is_some());
        missing_schedule.pending.take();
        missing_schedule.root_stale_reported(start, None, Some(&known));
        assert!(missing_schedule.root_issue_blocks_audit());
        missing_schedule.observe_root(start + ROOT_PROBE_MISSING, None, Some(&known));
        assert!(missing_schedule.root_issue_blocks_audit());
        let restored = RootIdentity::from_bytes(b"known".to_vec());
        missing_schedule.observe_root(
            start + ROOT_PROBE_MISSING + ROOT_PROBE_ACTIVE,
            Some(restored.clone()),
            Some(&restored),
        );
        assert!(!missing_schedule.root_issue_blocks_audit());
        assert!(missing_schedule.pending.is_some());
    }
}
