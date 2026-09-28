//! Lossless, bounded handoff from native replay capture to reconciliation admission.

use yield_library::sample_sources::reconciliation::{
    CaptureBoundary, ObservationUncertainty, RawEventKind, RawObservation, RawObservationLimits,
    RawObservationMetadata, RawObservationProvenance, RawObservedPath, RawPathHint, RawPathRole,
    SyntheticObservationBatch, WatcherGeneration,
};

use super::{
    MAX_REPLAY_EVENTS, MAX_REPLAY_PATH_BYTES, ReplayCandidate, ReplayPathKind, ReplayUncertainty,
};

/// A bounded native batch and its explicitly checked historical coverage claim.
///
/// This value is not a proof. The source owner must supply a validated durable prior to
/// `ReconciliationAdapter::admit_replay`; only that adapter can construct continuity proof.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayObservationBatch {
    batch: SyntheticObservationBatch,
    continuous: bool,
}

impl ReplayObservationBatch {
    /// Borrow the ordered observations and source/root/stream/generation provenance.
    pub fn batch(&self) -> &SyntheticObservationBatch {
        &self.batch
    }

    /// Return whether native history completed without a flagged coverage defect.
    pub const fn continuous(&self) -> bool {
        self.continuous
    }

    /// Pass these values together to the existing reconciliation replay adapter.
    pub fn into_admission_parts(self) -> (SyntheticObservationBatch, bool) {
        (self.batch, self.continuous)
    }
}

impl ReplayCandidate {
    /// Convert one native candidate into an ordered reconciliation batch.
    ///
    /// The first sequence denotes the start of the covered interval, not the first delivered
    /// changed-path event: FSEvents IDs are shared across the volume and may legitimately skip.
    /// A certain capture with no changed paths has no batch to admit. An uncertain capture always
    /// carries a diagnostic marker and must be admitted as unproven for a source audit.
    pub fn into_observation_batch(
        self,
        expected_source_lifecycle_generation: u64,
        owner_watcher_generation: WatcherGeneration,
    ) -> Option<ReplayObservationBatch> {
        let has_events = !self.events.is_empty();
        let continuous = self.history_complete
            && self.uncertainty.is_empty()
            && self.last_seen.is_some()
            && self.lifecycle_generation == expected_source_lifecycle_generation
            && expected_source_lifecycle_generation != 0
            && owner_watcher_generation.get() != 0;
        if !has_events
            && self.history_complete
            && self.uncertainty.is_empty()
            && self.lifecycle_generation == expected_source_lifecycle_generation
            && expected_source_lifecycle_generation != 0
        {
            return None;
        }
        let mut observations = Vec::with_capacity(self.events.len() + usize::from(!continuous));
        for event in self.events {
            let mut path = RawObservedPath::new(event.relative_path, RawPathRole::Subject);
            path = match event.kind {
                ReplayPathKind::File => path,
                ReplayPathKind::Directory => path.with_hint(RawPathHint::Directory),
                ReplayPathKind::Symlink => path.with_hint(RawPathHint::Symlink),
                ReplayPathKind::Unknown => path.with_hint(RawPathHint::Unknown),
            };
            observations.push(
                RawObservation::new(RawEventKind::Modify, vec![path])
                    .with_metadata(RawObservationMetadata::new().with_event_id(event.event_id)),
            );
        }
        if !continuous {
            let kind = if self.uncertainty.contains(ReplayUncertainty::ROOT_CHANGED) {
                RawEventKind::RootChanged
            } else if self.uncertainty.contains(ReplayUncertainty::DELIVERY_LOST)
                || self.uncertainty.contains(ReplayUncertainty::EVENT_LIMIT)
                || self.uncertainty.contains(ReplayUncertainty::PATH_LIMIT)
            {
                RawEventKind::Overflow
            } else {
                RawEventKind::Unsupported
            };
            let uncertainty = if kind == RawEventKind::Overflow {
                ObservationUncertainty::OVERFLOW | ObservationUncertainty::PATH_COVERAGE
            } else {
                ObservationUncertainty::CONTINUITY | ObservationUncertainty::PATH_COVERAGE
            };
            observations.push(RawObservation::new(kind, Vec::new()).with_uncertainty(uncertainty));
        }
        let (first, last) = match self.last_seen {
            Some(last) if last > self.started_after => {
                (self.started_after.checked_add(1), Some(last))
            }
            _ => (None, None),
        };
        let boundary =
            CaptureBoundary::try_new(self.last_seen.unwrap_or(self.started_after), first, last)
                .expect("replay ingress stores only advancing capture boundaries");
        let provenance = RawObservationProvenance::new(
            self.source_id,
            Some(self.root_identity),
            Some(self.stream_identity),
            owner_watcher_generation,
            boundary,
        );
        let limits = RawObservationLimits::new(
            MAX_REPLAY_EVENTS + 1,
            MAX_REPLAY_PATH_BYTES,
            (MAX_REPLAY_EVENTS + 1) * (std::mem::size_of::<u64>() + 3),
        )
        .expect("nonzero fixed replay bounds");
        Some(ReplayObservationBatch {
            batch: SyntheticObservationBatch::new(provenance, observations, limits),
            continuous,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use yield_library::sample_sources::SourceId;
    use yield_library::sample_sources::reconciliation::{RawObservationEnvelope, RootIdentity};

    use super::*;
    use crate::SourceWatchSpec;
    use crate::replay::{ReplayEventCondition, ReplayIngress};

    fn ingress() -> ReplayIngress {
        ReplayIngress::new(
            SourceWatchSpec::new(
                SourceId::from_string("batch-source"),
                4,
                PathBuf::from("/source"),
                Some(RootIdentity::from_bytes(b"root".to_vec())),
            ),
            17,
            [2; 16],
            100,
        )
        .expect("source identity")
    }

    #[test]
    fn global_id_leaps_keep_covered_interval_and_ordered_paths() {
        let ingress = ingress();
        ingress.record_with_kind(
            Some(105),
            Path::new("/source/kick.wav"),
            ReplayEventCondition::Changed,
            ReplayPathKind::File,
        );
        ingress.record_with_kind(
            Some(113),
            Path::new("/source/drums"),
            ReplayEventCondition::Changed,
            ReplayPathKind::Directory,
        );
        ingress.mark_history_complete();
        let handoff = ingress
            .into_candidate()
            .into_observation_batch(4, WatcherGeneration::new(8))
            .expect("changed paths");
        assert!(handoff.continuous());
        let boundary = handoff.batch().provenance().capture_boundary();
        assert_eq!(boundary.first_sequence(), Some(101));
        assert_eq!(boundary.last_sequence(), Some(113));
        assert_eq!(
            handoff.batch().observations()[0].metadata().event_id(),
            Some(105)
        );
        assert_eq!(handoff.batch().observations()[0].paths()[0].hint(), None);
        assert_eq!(
            handoff.batch().observations()[1].paths()[0].hint(),
            Some(RawPathHint::Directory)
        );
        let (provenance, observations, limits) = handoff.into_admission_parts().0.into_parts();
        RawObservationEnvelope::try_new(provenance, observations, limits)
            .expect("bounded native batch fits reconciliation envelope");
    }

    #[test]
    fn loss_and_unfinished_history_keep_audit_marker() {
        let ingress = ingress();
        ingress.record(
            Some(105),
            Path::new("/source/kick.wav"),
            ReplayEventCondition::Changed,
        );
        ingress.record(
            Some(109),
            Path::new("/source/lost.wav"),
            ReplayEventCondition::DeliveryLost,
        );
        let handoff = ingress
            .into_candidate()
            .into_observation_batch(4, WatcherGeneration::new(8))
            .expect("uncertain batch");
        assert!(!handoff.continuous());
        assert_eq!(
            handoff.batch().observations().last().unwrap().kind(),
            RawEventKind::Overflow
        );
        assert_eq!(
            handoff.batch().observations()[0].paths()[0].path(),
            Path::new("kick.wav")
        );
    }

    #[test]
    fn empty_certain_capture_is_not_presented_as_a_replay_batch() {
        let ingress = ingress();
        ingress.mark_history_complete();
        assert!(
            ingress
                .into_candidate()
                .into_observation_batch(4, WatcherGeneration::new(8))
                .is_none()
        );
    }

    #[test]
    fn stale_lifecycle_never_claims_coverage() {
        let ingress = ingress();
        ingress.record(
            Some(105),
            Path::new("/source/kick.wav"),
            ReplayEventCondition::Changed,
        );
        ingress.mark_history_complete();
        let handoff = ingress
            .into_candidate()
            .into_observation_batch(5, WatcherGeneration::new(8))
            .expect("stale evidence");
        assert!(!handoff.continuous());
        assert_eq!(
            handoff.batch().observations().last().unwrap().kind(),
            RawEventKind::Unsupported
        );
    }
}
