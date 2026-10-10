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
    // Only minted from the native ingress' private ignored-artifact lane. A path that happens
    // to spell `.DS_Store` in a caller-created batch never gets this classification.
    metadata_only: bool,
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

    pub(crate) const fn metadata_only(&self) -> bool {
        self.metadata_only
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
    /// A truly empty certain capture has no batch to admit. Ignored metadata callbacks are
    /// retained as real, ordered observations so the source owner can durably acknowledge their
    /// native interval without reconciling their path. An uncertain capture carries a diagnostic
    /// marker and must be admitted as unproven for a source audit.
    pub fn into_observation_batch(
        self,
        expected_source_lifecycle_generation: u64,
        owner_watcher_generation: WatcherGeneration,
    ) -> Option<ReplayObservationBatch> {
        let has_events = !self.events.is_empty() || !self.ignored_artifacts().is_empty();
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
        if self.events.is_empty()
            && self.unqualified_ignored_artifact
            && self.history_complete
            && self.uncertainty.is_empty()
            && self.lifecycle_generation == expected_source_lifecycle_generation
            && expected_source_lifecycle_generation != 0
        {
            // Preserve the former empty-capture retry behavior when an excluded sidecar shares
            // this interval. It has no private metadata tag and must not inherit the `.DS_Store`
            // cursor-only route.
            return None;
        }
        let metadata_only = continuous
            && self.events.is_empty()
            && !self.unqualified_ignored_artifact
            && !self.ignored_artifacts().is_empty();
        let mut ordered_events = self.events;
        let mut ignored_artifacts = self.ignored_artifacts;
        ordered_events.append(&mut ignored_artifacts);
        ordered_events.sort_unstable_by_key(|event| event.event_id());
        let mut observations = Vec::with_capacity(ordered_events.len() + usize::from(!continuous));
        for event in ordered_events {
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
            metadata_only,
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
    fn private_metadata_capture_keeps_real_event_and_ordered_cursor_boundary() {
        let ingress = ingress();
        ingress.record_metadata_artifact(103, Path::new("/source/Library/.DS_Store"));
        ingress.mark_history_complete();
        let handoff = ingress
            .into_candidate()
            .into_observation_batch(4, WatcherGeneration::new(8))
            .expect("metadata callback is a durable replay candidate");
        assert!(handoff.continuous());
        assert!(handoff.metadata_only());
        assert_eq!(handoff.batch().observations().len(), 1);
        assert_eq!(
            handoff.batch().observations()[0].kind(),
            RawEventKind::Modify
        );
        assert_eq!(
            handoff.batch().observations()[0].paths()[0].path(),
            Path::new("Library/.DS_Store")
        );
        assert_eq!(
            handoff.batch().observations()[0].metadata().event_id(),
            Some(103)
        );
        let boundary = handoff.batch().provenance().capture_boundary();
        assert_eq!(boundary.first_sequence(), Some(101));
        assert_eq!(boundary.last_sequence(), Some(103));
        let (provenance, observations, limits) = handoff.into_admission_parts().0.into_parts();
        RawObservationEnvelope::try_new(provenance, observations, limits)
            .expect("real metadata observation is a valid bounded owner-admission input");
    }

    #[test]
    fn mixed_metadata_and_audio_capture_is_not_metadata_only_and_keeps_native_order() {
        let ingress = ingress();
        ingress.record_metadata_artifact(103, Path::new("/source/Library/.DS_Store"));
        ingress.record_with_kind(
            Some(105),
            Path::new("/source/Kick.wav"),
            ReplayEventCondition::Changed,
            ReplayPathKind::File,
        );
        ingress.record_metadata_artifact(109, Path::new("/source/Library/.DS_Store"));
        ingress.mark_history_complete();
        let handoff = ingress
            .into_candidate()
            .into_observation_batch(4, WatcherGeneration::new(8))
            .expect("mixed actual events");
        assert!(!handoff.metadata_only());
        let observations = handoff.batch().observations();
        assert_eq!(observations.len(), 3);
        assert_eq!(observations[0].metadata().event_id(), Some(103));
        assert_eq!(observations[1].metadata().event_id(), Some(105));
        assert_eq!(observations[1].paths()[0].path(), Path::new("Kick.wav"));
        assert_eq!(observations[2].metadata().event_id(), Some(109));
    }

    #[test]
    fn other_ignored_sidecar_prevents_ds_store_only_checkpoint_classification() {
        let ingress = ingress();
        ingress.record_ignored_artifact(103);
        ingress.record_metadata_artifact(105, Path::new("/source/.DS_Store"));
        ingress.mark_history_complete();
        assert!(
            ingress
                .into_candidate()
                .into_observation_batch(4, WatcherGeneration::new(8))
                .is_none(),
            "an untagged excluded sidecar must not inherit metadata cursor authority"
        );
    }

    #[test]
    fn metadata_limit_overflow_prevents_a_metadata_only_claim() {
        let ingress = ingress();
        for event_id in 101..=100 + MAX_REPLAY_EVENTS as u64 {
            ingress.record_metadata_artifact(event_id, Path::new("/source/.DS_Store"));
        }
        ingress.record_metadata_artifact(
            101 + MAX_REPLAY_EVENTS as u64,
            Path::new("/source/.DS_Store"),
        );
        ingress.mark_history_complete();
        let candidate = ingress.into_candidate();
        assert_eq!(candidate.ignored_artifacts().len(), MAX_REPLAY_EVENTS);
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::EVENT_LIMIT)
        );
        let handoff = candidate
            .into_observation_batch(4, WatcherGeneration::new(8))
            .expect("overflow retains an uncertainty marker");
        assert!(!handoff.continuous());
        assert!(!handoff.metadata_only());
    }

    #[test]
    fn non_monotonic_metadata_callback_never_claims_cursor_only_coverage() {
        let ingress = ingress();
        ingress.record_metadata_artifact(103, Path::new("/source/.DS_Store"));
        ingress.record_metadata_artifact(102, Path::new("/source/.DS_Store"));
        ingress.mark_history_complete();
        let candidate = ingress.into_candidate();
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::NON_MONOTONIC_CURSOR)
        );
        let handoff = candidate
            .into_observation_batch(4, WatcherGeneration::new(8))
            .expect("non-monotonic capture retains diagnostic evidence");
        assert!(!handoff.continuous());
        assert!(!handoff.metadata_only());
        assert_eq!(
            handoff.batch().observations().last().unwrap().kind(),
            RawEventKind::Unsupported
        );
    }

    #[test]
    fn metadata_path_byte_limit_prevents_a_metadata_only_claim() {
        let ingress = ingress();
        let long_parent = "x".repeat(MAX_REPLAY_PATH_BYTES);
        let path = PathBuf::from("/source").join(long_parent).join(".DS_Store");
        ingress.record_metadata_artifact(101, &path);
        ingress.mark_history_complete();
        let candidate = ingress.into_candidate();
        assert!(candidate.ignored_artifacts().is_empty());
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::PATH_LIMIT)
        );
        let handoff = candidate
            .into_observation_batch(4, WatcherGeneration::new(8))
            .expect("path overflow retains an uncertainty marker");
        assert!(!handoff.continuous());
        assert!(!handoff.metadata_only());
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
