use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tempfile::TempDir;

use super::helpers::revision_value;
use crate::app_dirs::{
    ConfigBaseGuard, PersistenceProfileGuard, ProfileOwnershipError, WritableProfileGuard,
};
#[cfg(unix)]
use crate::sample_sources::db::SourceLeaseError;
#[cfg(unix)]
use crate::sample_sources::db::file_ops_journal::{CopyJournalEntryInit, FileOpJournalEntry};
use crate::sample_sources::db::{DatabaseRootBindingGuard, SourceDirectoryTruthState};
use crate::sample_sources::readiness::{
    ArtifactPublishOutcome, ReadinessFailureClassification, ReadinessFailureOutcome,
    ReadinessRetryPolicy, ReadinessStage, ReadinessTarget, ReadinessTargetPublication,
    SourceAvailability,
};
use crate::sample_sources::reconciliation::{
    AdapterDisposition, AdmissionOutcome, BackendStreamIdentity, BootstrapReplayEvidence,
    CaptureBoundary, RawEventKind, RawObservation, RawObservationLimits, RawObservationProvenance,
    RawObservedPath, RawPathRole, ReconciliationAdmissionLimits, ReconciliationAdmissionOwner,
    ReconciliationAdmissionSupervisor, ReconciliationScopeKind, SyntheticObservationBatch,
    WatcherGeneration,
};
use crate::sample_sources::reconciliation::{RootIdentity, RootRelativePath};
use crate::sample_sources::{
    Rating, SOURCE_CURATION_BATCH_LIMIT, SOURCE_CURATION_MAX_TAG_LABEL_BYTES, SampleCollection,
    SourceCommandId, SourceCurationUpdate, SourceDatabase, SourceDbError, SourceId,
    SourcePlaybackType, SourceSnapshotOwner, SourceSnapshotPage, SourceWriter, SourceWriterError,
    SourceWriterRequest,
};

struct CurationFixture {
    _runtime: crate::test_runtime::TestRuntimeGuard,
    _config_base_dir: TempDir,
    _config_base: ConfigBaseGuard,
    _profile: PersistenceProfileGuard,
    _source: TempDir,
    #[cfg(unix)]
    profile_child: Option<PathBuf>,
    database: SourceDatabase,
    profile_guard: WritableProfileGuard,
    root_identity: RootIdentity,
    revision: u64,
}

#[test]
fn replay_manifest_and_checkpoint_commit_once_before_raw_ticket_retirement() {
    let fixture = CurationFixture::new_profile_bound();
    let source = SourceId::from_string("atomic-replay-writer");
    let root = fixture.root_identity.clone();
    let stream =
        BackendStreamIdentity::from_fsevents_volume(99, [2; 16]).expect("native volume identity");
    let root_text = std::str::from_utf8(root.as_bytes()).expect("fixture root identity");
    let seed = serde_json::json!({
        "root_identity": root_text,
        "event_id": 10,
        "format_version": 4,
        "source_id": source.as_str(),
        "lifecycle_generation": 7,
        "source_revision": fixture.revision,
        "cause": "targeted_replay",
        "continuity_proof": {
            "root_identity": root_text,
            "backend": "fsevents",
            "backend_device": 99,
            "backend_volume_uuid": uuid::Uuid::from_bytes([2; 16]).to_string(),
            "watcher_generation": 1,
            "replay_coverage_start_event_id": 10,
            "replay_coverage_end_event_id": 10,
            "acknowledged_end_event_id": 10
        }
    })
    .to_string();
    let mut seed_batch = fixture.database.write_batch().expect("seed batch");
    seed_batch
        .set_metadata(
            crate::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT,
            &seed,
        )
        .expect("seed checkpoint");
    seed_batch.commit_auxiliary_state().expect("seed commit");
    let writer = SourceWriter::new(fixture.database, source.clone(), 7).expect("source writer");
    let expected = writer.current_scan_fence().expect("source revision");
    let lane_limits = RawObservationLimits::new(8, 4096, 4096).expect("lane limits");
    let limits = ReconciliationAdmissionLimits::new(
        1,
        lane_limits,
        RawObservationLimits::new(32, 16384, 16384).expect("global limits"),
        2,
        8,
        8,
    )
    .expect("admission limits");
    let mut owner =
        ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(limits));
    let lane = owner
        .begin_source(source.clone(), root.clone())
        .expect("capture lane");
    let prior = writer
        .catalog_database()
        .read_durable_replay_prior(
            &source,
            &root,
            crate::sample_sources::reconciliation::WatcherGeneration::new(7),
            lane.generation(),
        )
        .expect("read prior")
        .expect("valid prior");
    let batch = SyntheticObservationBatch::new(
        RawObservationProvenance::new(
            source.clone(),
            Some(root.clone()),
            Some(stream.clone()),
            lane.generation(),
            CaptureBoundary::try_new(20, Some(11), Some(12)).expect("capture interval"),
        ),
        vec![RawObservation::new(
            RawEventKind::Modify,
            vec![RawObservedPath::new("one.wav".into(), RawPathRole::Subject)],
        )],
        lane_limits,
    );
    let duplicate_batch = batch.clone();
    let admission = owner
        .admit_replay_with_durable_prior(batch, Some(&prior), true)
        .expect("admit replay");
    assert_eq!(
        admission.admission().disposition(),
        AdapterDisposition::AdmittedWithContinuity
    );
    let duplicate = owner
        .admit_replay_with_durable_prior(duplicate_batch, Some(&prior), true)
        .expect("suppress duplicate delivery");
    assert_eq!(
        duplicate.admission().disposition(),
        AdapterDisposition::DuplicateSuppressed
    );
    assert_eq!(owner.supervisor().in_flight(), 1);
    let ticket = match admission.admission().outcome() {
        AdmissionOutcome::Accepted(ticket) => *ticket,
        outcome => panic!("expected replay ticket, got {outcome:?}"),
    };
    let dispatched = owner.dispatch_next().expect("dispatch replay");
    let proof = dispatched
        .normalized()
        .proof()
        .watcher_continuity()
        .expect("admitted proof")
        .clone();
    assert_eq!(proof.source_id(), &source);
    assert_eq!(proof.root_identity(), &root);
    assert_eq!(proof.backend_stream_identity(), &stream);
    assert_eq!(proof.watcher_generation(), lane.generation());
    assert_eq!(proof.prior_acknowledgement().sequence(), 10);
    assert_eq!(proof.replay_coverage().after_sequence(), 10);
    assert_eq!(proof.replay_coverage().through_sequence(), 12);
    assert!(proof.replay_coverage().is_contiguous());
    owner.mark_dispatched(ticket).expect("replay handed off");
    owner.mark_applied(ticket).expect("replay applied");
    assert!(owner.mark_replay_checkpointed(ticket, &prior).is_err());
    assert_eq!(owner.supervisor().in_flight(), 1);

    let mut partial = writer
        .catalog_database()
        .write_batch()
        .expect("partial replay batch");
    partial
        .upsert_file(Path::new("partial.wav"), 10, 1)
        .expect("partial source fact");
    partial
        .set_metadata(
            crate::sample_sources::db::META_SOURCE_WATCHER_UNCERTAINTY,
            "unresolved",
        )
        .expect("uncertainty in transaction");
    assert!(matches!(
        writer.commit_watcher_replay_manifest_batch(&expected, partial, &prior, &proof),
        Err(SourceWriterError::ReplayCheckpointMismatch)
    ));
    assert!(
        writer
            .catalog_database()
            .entry_for_path(Path::new("partial.wav"))
            .unwrap()
            .is_none()
    );
    assert_eq!(writer.current_scan_fence().unwrap(), expected);
    assert_eq!(owner.supervisor().in_flight(), 1);
    assert!(owner.mark_replay_checkpointed(ticket, &prior).is_err());
    assert_eq!(
        writer
            .catalog_database()
            .read_durable_replay_prior(
                &source,
                &root,
                crate::sample_sources::reconciliation::WatcherGeneration::new(7),
                lane.generation(),
            )
            .unwrap()
            .unwrap()
            .acknowledged_sequence(),
        10,
        "an incomplete replay transaction cannot advance the durable cursor"
    );

    let mut manifest_batch = writer
        .catalog_database()
        .write_batch()
        .expect("prepared replay batch");
    manifest_batch
        .upsert_file(Path::new("one.wav"), 20, 2)
        .expect("prepared source fact");
    let (manifest, committed) = writer
        .commit_watcher_replay_manifest_batch(&expected, manifest_batch, &prior, &proof)
        .expect("atomic replay acknowledgement");
    assert_eq!(manifest.revision, expected.revision() + 1);
    assert_eq!(manifest.touched_path_changes.len(), 1);
    assert_eq!(manifest.touched_path_changes[0].0, Path::new("one.wav"));
    assert_eq!(committed.acknowledged_sequence(), 12);
    assert_eq!(
        writer.current_scan_fence().unwrap().revision(),
        manifest.revision,
        "manifest and checkpoint publish at one covered source revision"
    );
    let stored_checkpoint = writer
        .catalog_database()
        .get_metadata(crate::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT)
        .unwrap()
        .expect("committed replay checkpoint");
    let stored_checkpoint: serde_json::Value =
        serde_json::from_str(&stored_checkpoint).expect("checkpoint JSON");
    assert_eq!(stored_checkpoint["source_id"], source.as_str());
    assert_eq!(stored_checkpoint["root_identity"], root_text);
    assert_eq!(stored_checkpoint["lifecycle_generation"], 7);
    assert_eq!(stored_checkpoint["source_revision"], manifest.revision);
    assert_eq!(stored_checkpoint["event_id"], 12);
    assert_eq!(
        stored_checkpoint["continuity_proof"]["root_identity"],
        root_text
    );
    assert_eq!(stored_checkpoint["continuity_proof"]["backend"], "fsevents");
    assert_eq!(stored_checkpoint["continuity_proof"]["backend_device"], 99);
    assert_eq!(
        stored_checkpoint["continuity_proof"]["backend_volume_uuid"],
        uuid::Uuid::from_bytes([2; 16]).to_string()
    );
    assert_eq!(
        stored_checkpoint["continuity_proof"]["watcher_generation"],
        lane.generation().get()
    );
    assert_eq!(
        stored_checkpoint["continuity_proof"]["replay_coverage_start_event_id"],
        11
    );
    assert_eq!(
        stored_checkpoint["continuity_proof"]["replay_coverage_end_event_id"],
        12
    );
    assert_eq!(
        stored_checkpoint["continuity_proof"]["acknowledged_end_event_id"],
        12
    );
    owner
        .mark_replay_checkpointed(ticket, &committed)
        .expect("retire after committed proof");
    assert_eq!(owner.supervisor().in_flight(), 0);
    assert!(owner.mark_replay_checkpointed(ticket, &committed).is_err());
    assert_eq!(owner.supervisor().in_flight(), 0);

    let mut duplicate = writer
        .catalog_database()
        .write_batch()
        .expect("duplicate replay batch");
    duplicate
        .upsert_file(Path::new("duplicate.wav"), 30, 3)
        .expect("duplicate source fact");
    assert!(matches!(
        writer.commit_watcher_replay_manifest_batch(&expected, duplicate, &prior, &proof),
        Err(SourceWriterError::StaleRevision { .. })
    ));
    assert_eq!(
        writer.current_scan_fence().unwrap().revision(),
        manifest.revision
    );
    assert!(
        writer
            .catalog_database()
            .entry_for_path(Path::new("duplicate.wav"))
            .unwrap()
            .is_none()
    );

    let pending = SyntheticObservationBatch::new(
        RawObservationProvenance::new(
            source.clone(),
            Some(root.clone()),
            Some(stream.clone()),
            lane.generation(),
            CaptureBoundary::try_new(21, Some(13), Some(13)).expect("pending capture"),
        ),
        vec![RawObservation::new(
            RawEventKind::Modify,
            vec![RawObservedPath::new("two.wav".into(), RawPathRole::Subject)],
        )],
        lane_limits,
    );
    let pending = owner
        .admit_replay_with_durable_prior(pending, Some(&committed), true)
        .expect("next replay admitted");
    let pending_ticket = match pending.admission().outcome() {
        AdmissionOutcome::Accepted(ticket) => *ticket,
        outcome => panic!("expected pending replay, got {outcome:?}"),
    };
    owner.dispatch_next().expect("pending dispatch");
    owner
        .mark_dispatched(pending_ticket)
        .expect("pending handoff");
    owner.mark_applied(pending_ticket).expect("pending applied");
    owner
        .stop_source(&source)
        .expect("stop invalidates pending replay");
    assert_eq!(owner.supervisor().in_flight(), 0);
    assert!(
        owner
            .mark_replay_checkpointed(pending_ticket, &committed)
            .is_err()
    );
    drop(writer);

    let reopened = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        Path::new("curation-profile-bound"),
    )
    .expect("reopen source database");
    let writer =
        SourceWriter::new_for_scan(reopened, source.clone(), 7).expect("reopened source owner");
    let reopened_fence = writer.current_scan_fence().expect("reopened fence");
    assert_eq!(reopened_fence.revision(), manifest.revision);
    assert_eq!(
        writer
            .catalog_database()
            .read_durable_replay_prior(
                &source,
                &root,
                crate::sample_sources::reconciliation::WatcherGeneration::new(7),
                lane.generation(),
            )
            .expect("reopened checkpoint")
            .expect("old checkpoint retained"),
        committed
    );
    let restarted = owner
        .begin_source(source.clone(), root.clone())
        .expect("restart watcher lane");
    assert!(restarted.generation() > lane.generation());
    let stale = SyntheticObservationBatch::new(
        RawObservationProvenance::new(
            source.clone(),
            Some(root.clone()),
            Some(stream),
            lane.generation(),
            CaptureBoundary::try_new(22, Some(13), Some(13)).expect("stale capture"),
        ),
        vec![RawObservation::new(
            RawEventKind::Modify,
            vec![RawObservedPath::new("two.wav".into(), RawPathRole::Subject)],
        )],
        lane_limits,
    );
    let invalid = owner
        .admit_replay_with_durable_prior(stale, Some(&committed), true)
        .expect("stale replay remains unproven");
    assert_eq!(
        invalid.admission().disposition(),
        AdapterDisposition::SourceAuditRequired
    );
    let widened = owner
        .durable_audit_marker_for_replay(&invalid)
        .expect("current-lane audit disposition");
    assert_eq!(widened.generation(), Some(restarted.generation()));
    writer
        .retain_watcher_replay_uncertainty(&reopened_fence, &widened)
        .expect("persist restart audit disposition");
    let stored_checkpoint = writer
        .catalog_database()
        .get_metadata(crate::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT)
        .expect("read checkpoint metadata")
        .expect("checkpoint retained under uncertainty");
    let stored_checkpoint: serde_json::Value =
        serde_json::from_str(&stored_checkpoint).expect("checkpoint JSON");
    assert_eq!(stored_checkpoint["event_id"], 12);
    assert!(
        writer
            .catalog_database()
            .read_durable_replay_prior(
                &source,
                &root,
                crate::sample_sources::reconciliation::WatcherGeneration::new(7),
                restarted.generation(),
            )
            .expect("uncertain checkpoint read")
            .is_none()
    );
    let rebound = owner
        .rebind_source(&source, RootIdentity::from_bytes(b"rebound-root".to_vec()))
        .expect("rebound lane");
    assert!(rebound.generation() > restarted.generation());
    assert!(owner.durable_audit_marker_for_replay(&invalid).is_none());
    assert_eq!(
        writer.current_scan_fence().unwrap().revision(),
        manifest.revision
    );
}

#[test]
fn replay_capture_and_prepared_batch_recover_across_drop_and_pre_ack_restart() {
    let fixture = CurationFixture::new_profile_bound();
    let source = SourceId::from_string("replay-crash-window-writer");
    let root = fixture.root_identity.clone();
    let stream =
        BackendStreamIdentity::from_fsevents_volume(99, [2; 16]).expect("native volume identity");
    let mut writer = SourceWriter::new(fixture.database, source.clone(), 7).expect("source writer");
    let mut audit_batch = writer
        .catalog_database()
        .write_batch()
        .expect("initial audit batch");
    audit_batch
        .complete_manifest_audit(2)
        .expect("initial complete audit");
    let (audit, _) = audit_batch
        .commit_with_manifest_snapshot_and_audit(root.clone(), None)
        .expect("initial audited revision");
    let mut expected = writer.current_scan_fence().expect("audited fence");
    let lane_limits = RawObservationLimits::new(8, 4096, 4096).expect("lane limits");
    let limits = ReconciliationAdmissionLimits::new(
        1,
        lane_limits,
        RawObservationLimits::new(32, 16384, 16384).expect("global limits"),
        2,
        8,
        8,
    )
    .expect("admission limits");
    let mut owner =
        ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(limits));
    let lane = owner
        .begin_source(source.clone(), root.clone())
        .expect("initial watcher lane");
    let bootstrap = BootstrapReplayEvidence::from_certain_empty_capture(
        source.clone(),
        root.clone(),
        stream.clone(),
        7,
        lane.generation(),
        10,
        10,
        true,
        false,
        0,
    )
    .expect("certain initial capture");
    let prior = writer
        .commit_bootstrap_replay_checkpoint(&expected, &audit, &bootstrap)
        .expect("initial durable prior");

    let capture = SyntheticObservationBatch::new(
        RawObservationProvenance::new(
            source.clone(),
            Some(root.clone()),
            Some(stream.clone()),
            lane.generation(),
            CaptureBoundary::try_new(20, Some(11), Some(12)).expect("capture boundary"),
        ),
        vec![RawObservation::new(
            RawEventKind::Modify,
            vec![RawObservedPath::new("one.wav".into(), RawPathRole::Subject)],
        )],
        lane_limits,
    );
    let admission = owner
        .admit_replay_with_durable_prior(capture.clone(), Some(&prior), true)
        .expect("captured replay admitted");
    let ticket = match admission.admission().outcome() {
        AdmissionOutcome::Accepted(ticket) => *ticket,
        outcome => panic!("expected captured replay ticket, got {outcome:?}"),
    };
    owner.dispatch_next().expect("dispatch captured replay");
    owner
        .mark_dispatched(ticket)
        .expect("captured replay handed off");
    owner.mark_applied(ticket).expect("captured replay applied");

    // A prepared but uncommitted manifest and an in-memory capture disappear at this boundary.
    let mut prepared = writer
        .catalog_database()
        .write_batch()
        .expect("prepared but uncommitted manifest");
    prepared
        .upsert_file(Path::new("never-committed.wav"), 20, 2)
        .expect("prepare source fact");
    drop(prepared);
    let before_restart = source_writer_state_snapshot(writer.catalog_database());
    drop(owner);
    drop(writer);

    let reopened = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        Path::new("curation-profile-bound"),
    )
    .expect("reopen after uncommitted batch");
    writer =
        SourceWriter::new_for_scan(reopened, source.clone(), 7).expect("reopened source writer");
    expected = writer
        .current_scan_fence()
        .expect("fence after uncommitted drop");
    assert_eq!(
        source_writer_state_snapshot(writer.catalog_database()),
        before_restart,
        "dropping a prepared batch preserves the last-good projection"
    );
    assert!(
        writer
            .catalog_database()
            .entry_for_path(Path::new("never-committed.wav"))
            .expect("read uncommitted path")
            .is_none()
    );
    let mut owner =
        ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(limits));
    let lane = owner
        .begin_source(source.clone(), root.clone())
        .expect("restarted watcher lane");
    let prior = writer
        .catalog_database()
        .read_durable_replay_prior(&source, &root, WatcherGeneration::new(7), lane.generation())
        .expect("read prior after prepared drop")
        .expect("prior remains available");
    assert_eq!(prior.acknowledged_sequence(), 10);

    let replay = owner
        .admit_replay_with_durable_prior(capture, Some(&prior), true)
        .expect("replay after dropped capture");
    let ticket = match replay.admission().outcome() {
        AdmissionOutcome::Accepted(ticket) => *ticket,
        outcome => panic!("expected replay after restart, got {outcome:?}"),
    };
    let dispatched = owner
        .dispatch_next()
        .expect("dispatch replay after restart");
    let proof = dispatched
        .normalized()
        .proof()
        .watcher_continuity()
        .expect("replayed continuity proof")
        .clone();
    owner.mark_dispatched(ticket).expect("replay handed off");
    owner.mark_applied(ticket).expect("replay applied");
    let mut manifest = writer
        .catalog_database()
        .write_batch()
        .expect("replay manifest");
    manifest
        .upsert_file(Path::new("replayed.wav"), 30, 3)
        .expect("replay source fact");
    let (committed_manifest, committed_prior) = writer
        .commit_watcher_replay_manifest_batch(&expected, manifest, &prior, &proof)
        .expect("atomically commit replay and checkpoint");
    assert_eq!(committed_manifest.revision, expected.revision() + 1);
    assert_eq!(committed_prior.acknowledged_sequence(), 12);
    let last_good = source_writer_state_snapshot(writer.catalog_database());
    assert_eq!(owner.supervisor().in_flight(), 1);

    // Simulate a process exit after SQLite committed the facts and cursor but before the
    // watcher owner acknowledged or retired its raw ticket.
    drop(owner);
    drop(writer);
    let reopened = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        Path::new("curation-profile-bound"),
    )
    .expect("reopen after commit before owner acknowledgement");
    let writer = SourceWriter::new_for_scan(reopened, source.clone(), 7).expect("reopened owner");
    let reopened_fence = writer.current_scan_fence().expect("reopened fence");
    assert_eq!(reopened_fence.revision(), committed_manifest.revision);
    assert_eq!(
        source_writer_state_snapshot(writer.catalog_database()),
        last_good,
        "pre-ack restart retains the exact committed projection"
    );
    assert!(
        writer
            .catalog_database()
            .entry_for_path(Path::new("replayed.wav"))
            .expect("read committed replay row")
            .is_some()
    );
    assert!(
        writer
            .catalog_database()
            .entry_for_path(Path::new("never-committed.wav"))
            .expect("read dropped prepared row")
            .is_none()
    );
    let durable = writer
        .catalog_database()
        .read_durable_replay_prior(&source, &root, WatcherGeneration::new(7), lane.generation())
        .expect("read committed prior after restart")
        .expect("committed prior retained");
    assert_eq!(durable, committed_prior);

    let mut duplicate = writer
        .catalog_database()
        .write_batch()
        .expect("duplicate replay after restart");
    duplicate
        .upsert_file(Path::new("duplicate-after-restart.wav"), 40, 4)
        .expect("duplicate source fact");
    assert!(matches!(
        writer.commit_watcher_replay_manifest_batch(&reopened_fence, duplicate, &prior, &proof),
        Err(SourceWriterError::ReplayCheckpointMismatch)
    ));
    assert_eq!(
        writer.current_scan_fence().unwrap().revision(),
        committed_manifest.revision
    );
    assert!(
        writer
            .catalog_database()
            .entry_for_path(Path::new("duplicate-after-restart.wav"))
            .expect("read duplicate row")
            .is_none()
    );
}

#[test]
fn replay_stream_root_and_generation_replacements_fence_each_held_proof() {
    for (label, replace_stream, replace_root, restart_lane) in [
        ("stream", true, false, false),
        ("root", false, true, false),
        ("generation", false, false, true),
    ] {
        assert_stale_identity_fences_writer(
            label,
            replace_stream,
            replace_root,
            restart_lane,
            false,
        );
    }
    assert_stale_identity_fences_writer("writer lifecycle", false, false, false, true);
}

fn assert_stale_identity_fences_writer(
    label: &str,
    replace_stream: bool,
    replace_root: bool,
    restart_lane: bool,
    replace_writer_lifecycle: bool,
) {
    let fixture = CurationFixture::new_profile_bound();
    let source = SourceId::from_string("stale-identity-replay-writer");
    let root = fixture.root_identity.clone();
    let stream =
        BackendStreamIdentity::from_fsevents_volume(99, [2; 16]).expect("native volume identity");
    let root_text = std::str::from_utf8(root.as_bytes()).expect("fixture root identity");
    let seed = serde_json::json!({
        "root_identity": root_text,
        "event_id": 10,
        "format_version": 4,
        "source_id": source.as_str(),
        "lifecycle_generation": 7,
        "source_revision": fixture.revision,
        "cause": "targeted_replay",
        "continuity_proof": {
            "root_identity": root_text,
            "backend": "fsevents",
            "backend_device": 99,
            "backend_volume_uuid": uuid::Uuid::from_bytes([2; 16]).to_string(),
            "watcher_generation": 1,
            "replay_coverage_start_event_id": 10,
            "replay_coverage_end_event_id": 10,
            "acknowledged_end_event_id": 10
        }
    })
    .to_string();
    let mut seed_batch = fixture.database.write_batch().expect("seed batch");
    seed_batch
        .set_metadata(
            crate::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT,
            &seed,
        )
        .expect("seed checkpoint");
    seed_batch.commit_auxiliary_state().expect("seed commit");

    let mut writer = SourceWriter::new(fixture.database, source.clone(), 7).expect("source writer");
    let mut expected = writer.current_scan_fence().expect("source revision");
    let lane_limits = RawObservationLimits::new(8, 4096, 4096).expect("lane limits");
    let limits = ReconciliationAdmissionLimits::new(
        1,
        lane_limits,
        RawObservationLimits::new(32, 16384, 16384).expect("global limits"),
        2,
        8,
        8,
    )
    .expect("admission limits");
    let mut owner =
        ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(limits));
    let old_lane = owner
        .begin_source(source.clone(), root.clone())
        .expect("initial source lane");
    let prior = writer
        .catalog_database()
        .read_durable_replay_prior(
            &source,
            &root,
            WatcherGeneration::new(7),
            old_lane.generation(),
        )
        .expect("read prior")
        .expect("valid prior");
    let before = source_writer_state_snapshot(writer.catalog_database());
    let checkpoint_before = writer
        .catalog_database()
        .get_metadata(crate::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT)
        .expect("checkpoint metadata")
        .expect("checkpoint exists");

    // Keep the valid proof in a separate lane so identity failures can reuse its contiguous
    // interval without duplicate suppression hiding the stale identity.
    let mut proof_owner =
        ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(limits));
    let proof_lane = proof_owner
        .begin_source(source.clone(), root.clone())
        .expect("proof source lane");
    assert_eq!(proof_lane.generation(), old_lane.generation());

    let valid_batch = SyntheticObservationBatch::new(
        RawObservationProvenance::new(
            source.clone(),
            Some(root.clone()),
            Some(stream.clone()),
            proof_lane.generation(),
            CaptureBoundary::try_new(20, Some(11), Some(12)).expect("valid interval"),
        ),
        vec![RawObservation::new(
            RawEventKind::Modify,
            vec![RawObservedPath::new("one.wav".into(), RawPathRole::Subject)],
        )],
        lane_limits,
    );
    let valid = proof_owner
        .admit_replay_with_durable_prior(valid_batch, Some(&prior), true)
        .expect("admit valid replay proof");
    assert_eq!(
        valid.admission().disposition(),
        AdapterDisposition::AdmittedWithContinuity
    );
    let ticket = match valid.admission().outcome() {
        AdmissionOutcome::Accepted(ticket) => *ticket,
        outcome => panic!("expected valid replay ticket, got {outcome:?}"),
    };
    let dispatched = proof_owner.dispatch_next().expect("dispatch valid proof");
    let proof = dispatched
        .normalized()
        .proof()
        .watcher_continuity()
        .expect("valid continuity proof")
        .clone();
    proof_owner
        .mark_dispatched(ticket)
        .expect("valid replay dispatched");
    proof_owner
        .mark_applied(ticket)
        .expect("valid replay applied");

    let mut current_lane = if restart_lane {
        owner.stop_source(&source).expect("stop old watcher lane");
        owner
            .begin_source(source.clone(), root.clone())
            .expect("restart watcher lane")
    } else {
        old_lane.clone()
    };
    if restart_lane {
        assert!(current_lane.generation() > old_lane.generation());
    }
    if replace_writer_lifecycle {
        proof_owner
            .stop_source(&source)
            .expect("invalidate proof from previous writer lifecycle");
        assert!(
            proof_owner
                .mark_replay_checkpointed(ticket, &prior)
                .is_err()
        );
        assert_eq!(proof_owner.supervisor().in_flight(), 0);
        owner
            .stop_source(&source)
            .expect("stop lane owned by previous writer lifecycle");
        assert_eq!(owner.supervisor().in_flight(), 0);
        writer = SourceWriter::new(writer.into_catalog_database(), source.clone(), 8)
            .expect("replace source writer lifecycle");
        expected = writer.current_scan_fence().expect("new lifecycle fence");
        assert_eq!(expected.lifecycle_generation(), 8);
        owner = ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(limits));
        current_lane = owner
            .begin_source(source.clone(), root.clone())
            .expect("start new lifecycle watcher lane");
        assert_eq!(current_lane.generation(), old_lane.generation());
        assert!(
            writer
                .catalog_database()
                .read_durable_replay_prior(
                    &source,
                    &root,
                    WatcherGeneration::new(expected.lifecycle_generation()),
                    current_lane.generation(),
                )
                .expect("new lifecycle cannot adopt prior checkpoint")
                .is_none()
        );
        assert!(
            writer
                .catalog_database()
                .get_metadata(crate::sample_sources::db::META_SOURCE_WATCHER_UNCERTAINTY)
                .expect("no audit marker before lifecycle fence check")
                .is_none()
        );
        let mut candidate = writer
            .catalog_database()
            .write_batch()
            .expect("old-lifecycle candidate manifest");
        candidate
            .upsert_file(Path::new("old-lifecycle-derived.wav"), 30, 3)
            .expect("candidate from old lifecycle proof");
        assert!(matches!(
            writer.commit_watcher_replay_manifest_batch(&expected, candidate, &prior, &proof),
            Err(SourceWriterError::ReplayCheckpointMismatch)
        ));
        assert_eq!(
            source_writer_state_snapshot(writer.catalog_database()),
            before
        );
        assert!(
            writer
                .catalog_database()
                .entry_for_path(Path::new("old-lifecycle-derived.wav"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            writer
                .catalog_database()
                .get_metadata(crate::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT)
                .unwrap()
                .as_deref(),
            Some(checkpoint_before.as_str())
        );
    }
    let stale_root = if replace_root {
        RootIdentity::from_bytes(b"replacement-root".to_vec())
    } else {
        root.clone()
    };
    let stale_stream = if replace_stream {
        BackendStreamIdentity::from_fsevents_volume(99, [3; 16])
            .expect("replacement volume identity")
    } else {
        stream.clone()
    };
    let stale_generation = if restart_lane {
        old_lane.generation()
    } else {
        current_lane.generation()
    };
    let stale_batch = SyntheticObservationBatch::new(
        RawObservationProvenance::new(
            source.clone(),
            Some(stale_root),
            Some(stale_stream),
            stale_generation,
            CaptureBoundary::try_new(21, Some(11), Some(12)).expect("contiguous stale interval"),
        ),
        vec![RawObservation::new(
            RawEventKind::Modify,
            vec![RawObservedPath::new("one.wav".into(), RawPathRole::Subject)],
        )],
        lane_limits,
    );
    let admission_prior = if replace_writer_lifecycle {
        None
    } else {
        Some(&prior)
    };
    let invalid = owner
        .admit_replay_with_durable_prior(stale_batch, admission_prior, true)
        .unwrap_or_else(|error| panic!("{label} replacement remains auditable: {error:?}"));
    assert_eq!(
        invalid.admission().disposition(),
        AdapterDisposition::SourceAuditRequired,
        "{label} replacement cannot inherit the held proof"
    );
    let request = invalid.audit_request().expect("current-lane audit request");
    assert_eq!(request.source_id(), &source);
    assert_eq!(request.root_identity(), &root);
    assert_eq!(request.generation(), current_lane.generation());
    let marker = owner
        .durable_audit_marker_for_replay(&invalid)
        .expect("current-lane audit marker");
    assert_eq!(marker.root_identity(), Some(&root));
    assert_eq!(marker.generation(), Some(current_lane.generation()));
    assert_eq!(marker.scope(), ReconciliationScopeKind::SourceAudit);

    match (label, invalid.admission().outcome()) {
        (
            "root",
            AdmissionOutcome::Rejected(
                crate::sample_sources::reconciliation::AdmissionRejectReason::WrongRoot,
            ),
        ) => assert_eq!(owner.dispatch_next(), None),
        (
            "generation",
            AdmissionOutcome::Rejected(
                crate::sample_sources::reconciliation::AdmissionRejectReason::StaleGeneration,
            ),
        ) => assert_eq!(owner.dispatch_next(), None),
        ("stream" | "writer lifecycle", AdmissionOutcome::Accepted(invalid_ticket)) => {
            let invalid_ticket = *invalid_ticket;
            let dispatched = owner
                .dispatch_next()
                .expect("dispatch replacement for audit");
            assert_eq!(dispatched.ticket(), invalid_ticket);
            assert!(dispatched.normalized().proof().is_unproven());
            owner
                .mark_dispatched(invalid_ticket)
                .expect("replacement dispatched");
            owner
                .mark_applied(invalid_ticket)
                .expect("replacement applied");
            owner
                .mark_unproven_audit_handed_off(invalid_ticket)
                .expect("replacement handed to audit");
        }
        (_, outcome) => panic!("unexpected {label} disposition: {outcome:?}"),
    }

    let retained = writer
        .retain_watcher_replay_uncertainty(&expected, &marker)
        .unwrap_or_else(|error| panic!("persist {label} audit marker: {error:?}"));
    assert_eq!(retained.root_identity(), &root);
    assert_eq!(
        retained.watcher_generation(),
        Some(current_lane.generation())
    );
    assert_eq!(
        retained.lifecycle_generation(),
        expected.lifecycle_generation()
    );
    assert_eq!(writer.current_scan_fence().unwrap(), expected);
    let mut candidate = writer
        .catalog_database()
        .write_batch()
        .expect("stale proof candidate manifest");
    candidate
        .upsert_file(Path::new("stale-proof-derived.wav"), 30, 3)
        .expect("candidate source fact");
    assert!(matches!(
        writer.commit_watcher_replay_manifest_batch(&expected, candidate, &prior, &proof),
        Err(SourceWriterError::ReplayCheckpointMismatch)
    ));
    assert_eq!(
        source_writer_state_snapshot(writer.catalog_database()),
        before,
        "{label} replacement preserves last-good projection"
    );
    assert!(
        writer
            .catalog_database()
            .entry_for_path(Path::new("stale-proof-derived.wav"))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        writer
            .catalog_database()
            .get_metadata(crate::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT)
            .unwrap()
            .as_deref(),
        Some(checkpoint_before.as_str())
    );
    assert!(
        writer
            .catalog_database()
            .read_durable_replay_prior(
                &source,
                &root,
                WatcherGeneration::new(7),
                current_lane.generation(),
            )
            .unwrap()
            .is_none()
    );
    assert!(
        proof_owner
            .mark_replay_checkpointed(ticket, &prior)
            .is_err()
    );
    assert_eq!(
        proof_owner.supervisor().in_flight(),
        if replace_writer_lifecycle { 0 } else { 1 },
        "stale proof cannot retire its barrier"
    );
}

#[cfg(unix)]
#[test]
fn source_writer_rejects_replay_after_physical_media_root_replacement() {
    let fixture = CurationFixture::new_profile_bound();
    let source = SourceId::from_string("physical-root-replay-writer");
    let root = fixture.root_identity.clone();
    let stream =
        BackendStreamIdentity::from_fsevents_volume(99, [2; 16]).expect("native volume identity");
    let root_text = std::str::from_utf8(root.as_bytes()).expect("fixture root identity");
    let seed = serde_json::json!({
        "root_identity": root_text,
        "event_id": 10,
        "format_version": 4,
        "source_id": source.as_str(),
        "lifecycle_generation": 7,
        "source_revision": fixture.revision,
        "cause": "targeted_replay",
        "continuity_proof": {
            "root_identity": root_text,
            "backend": "fsevents",
            "backend_device": 99,
            "backend_volume_uuid": uuid::Uuid::from_bytes([2; 16]).to_string(),
            "watcher_generation": 1,
            "replay_coverage_start_event_id": 10,
            "replay_coverage_end_event_id": 10,
            "acknowledged_end_event_id": 10
        }
    })
    .to_string();
    let mut seed_batch = fixture.database.write_batch().expect("seed batch");
    seed_batch
        .set_metadata(
            crate::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT,
            &seed,
        )
        .expect("seed checkpoint");
    seed_batch.commit_auxiliary_state().expect("seed commit");

    let writer = SourceWriter::new(fixture.database, source.clone(), 7).expect("source writer");
    let expected = writer.current_scan_fence().expect("source revision");
    let limits = RawObservationLimits::new(8, 4096, 4096).expect("observation limits");
    let mut owner = ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(
        ReconciliationAdmissionLimits::new(
            1,
            limits,
            RawObservationLimits::new(32, 16384, 16384).expect("global limits"),
            2,
            8,
            8,
        )
        .expect("admission limits"),
    ));
    let lane = owner
        .begin_source(source.clone(), root.clone())
        .expect("capture lane");
    let prior = writer
        .catalog_database()
        .read_durable_replay_prior(&source, &root, WatcherGeneration::new(7), lane.generation())
        .expect("read prior")
        .expect("valid prior");
    let batch = SyntheticObservationBatch::new(
        RawObservationProvenance::new(
            source.clone(),
            Some(root),
            Some(stream),
            lane.generation(),
            CaptureBoundary::try_new(20, Some(11), Some(12)).expect("valid interval"),
        ),
        vec![RawObservation::new(
            RawEventKind::Modify,
            vec![RawObservedPath::new("one.wav".into(), RawPathRole::Subject)],
        )],
        limits,
    );
    let admitted = owner
        .admit_replay_with_durable_prior(batch, Some(&prior), true)
        .expect("valid replay admission");
    let ticket = match admitted.admission().outcome() {
        AdmissionOutcome::Accepted(ticket) => *ticket,
        outcome => panic!("expected accepted replay, got {outcome:?}"),
    };
    let dispatched = owner.dispatch_next().expect("dispatch replay");
    let proof = dispatched
        .normalized()
        .proof()
        .watcher_continuity()
        .expect("valid continuity proof")
        .clone();
    owner.mark_dispatched(ticket).expect("replay dispatched");
    owner.mark_applied(ticket).expect("replay applied");
    let before = source_writer_state_snapshot(writer.catalog_database());
    let checkpoint_before = writer
        .catalog_database()
        .get_metadata(crate::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT)
        .expect("checkpoint metadata")
        .expect("checkpoint exists");

    let source_root = fixture._source.path();
    let displaced_root = source_root.with_extension("displaced");
    assert!(!displaced_root.exists());
    let mut candidate = writer
        .catalog_database()
        .write_batch()
        .expect("candidate replay manifest");
    candidate
        .upsert_file(Path::new("physical-root-derived.wav"), 30, 3)
        .expect("candidate source fact");
    std::fs::rename(source_root, &displaced_root).expect("displace physical source root");
    std::fs::create_dir(source_root).expect("create replacement source root");
    let rejected =
        writer.commit_watcher_replay_manifest_batch(&expected, candidate, &prior, &proof);
    std::fs::remove_dir(source_root).expect("remove replacement source root");
    std::fs::rename(&displaced_root, source_root).expect("restore retained source root");

    assert!(matches!(
        rejected,
        Err(SourceWriterError::Database(SourceDbError::SourceLease(
            SourceLeaseError::SourceRootReplaced { .. }
        )))
    ));
    assert_eq!(
        source_writer_state_snapshot(writer.catalog_database()),
        before,
        "physical root replacement preserves last-good projection"
    );
    assert!(
        writer
            .catalog_database()
            .entry_for_path(Path::new("physical-root-derived.wav"))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        writer
            .catalog_database()
            .get_metadata(crate::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT)
            .unwrap()
            .as_deref(),
        Some(checkpoint_before.as_str())
    );
    assert_eq!(owner.supervisor().in_flight(), 1);
}

#[test]
fn replay_cursor_gaps_and_overflow_keep_last_good_state_until_a_complete_audit() {
    let cases = [
        (
            "missing cursor",
            CaptureBoundary::try_new(21, None, None).expect("missing cursor boundary"),
            RawEventKind::Create,
        ),
        (
            "cursor gap",
            CaptureBoundary::try_new(22, Some(12), Some(13)).expect("gap boundary"),
            RawEventKind::Modify,
        ),
        (
            "overflow",
            CaptureBoundary::try_new(23, Some(14), Some(15)).expect("overflow boundary"),
            RawEventKind::Overflow,
        ),
    ];
    for (label, boundary, kind) in cases {
        assert_replay_case_keeps_last_good_state(label, boundary, kind);
    }
}

fn assert_replay_case_keeps_last_good_state(
    label: &str,
    boundary: CaptureBoundary,
    kind: RawEventKind,
) {
    let fixture = CurationFixture::new_profile_bound();
    let source = SourceId::from_string("gap-overflow-replay-writer");
    let root = fixture.root_identity.clone();
    let stream =
        BackendStreamIdentity::from_fsevents_volume(99, [2; 16]).expect("native volume identity");
    let root_text = std::str::from_utf8(root.as_bytes()).expect("fixture root identity");
    let seed = serde_json::json!({
        "root_identity": root_text,
        "event_id": 10,
        "format_version": 4,
        "source_id": source.as_str(),
        "lifecycle_generation": 7,
        "source_revision": fixture.revision,
        "cause": "targeted_replay",
        "continuity_proof": {
            "root_identity": root_text,
            "backend": "fsevents",
            "backend_device": 99,
            "backend_volume_uuid": uuid::Uuid::from_bytes([2; 16]).to_string(),
            "watcher_generation": 1,
            "replay_coverage_start_event_id": 10,
            "replay_coverage_end_event_id": 10,
            "acknowledged_end_event_id": 10
        }
    })
    .to_string();
    let mut seed_batch = fixture.database.write_batch().expect("seed batch");
    seed_batch
        .set_metadata(
            crate::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT,
            &seed,
        )
        .expect("seed checkpoint");
    seed_batch.commit_auxiliary_state().expect("seed commit");

    let writer = SourceWriter::new(fixture.database, source.clone(), 7).expect("source writer");
    let expected = writer.current_scan_fence().expect("source revision");
    let lane_limits = RawObservationLimits::new(8, 4096, 4096).expect("lane limits");
    let limits = ReconciliationAdmissionLimits::new(
        1,
        lane_limits,
        RawObservationLimits::new(32, 16384, 16384).expect("global limits"),
        2,
        8,
        8,
    )
    .expect("admission limits");
    let mut owner =
        ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(limits));
    let lane = owner
        .begin_source(source.clone(), root.clone())
        .expect("capture lane");
    let prior = writer
        .catalog_database()
        .read_durable_replay_prior(&source, &root, WatcherGeneration::new(7), lane.generation())
        .expect("read prior")
        .expect("valid prior");
    let before = source_writer_state_snapshot(writer.catalog_database());
    let checkpoint_before = writer
        .catalog_database()
        .get_metadata(crate::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT)
        .expect("checkpoint metadata")
        .expect("checkpoint exists");

    // Hold a valid-looking replay proof without publishing it. Later evidence that exposes a
    // missing interval must fence this proof before its manifest or checkpoint can commit.
    let valid_batch = SyntheticObservationBatch::new(
        RawObservationProvenance::new(
            source.clone(),
            Some(root.clone()),
            Some(stream.clone()),
            lane.generation(),
            CaptureBoundary::try_new(20, Some(11), Some(12)).expect("valid interval"),
        ),
        vec![RawObservation::new(
            RawEventKind::Modify,
            vec![RawObservedPath::new("one.wav".into(), RawPathRole::Subject)],
        )],
        lane_limits,
    );
    let valid = owner
        .admit_replay_with_durable_prior(valid_batch, Some(&prior), true)
        .expect("admit valid replay proof");
    assert_eq!(
        valid.admission().disposition(),
        AdapterDisposition::AdmittedWithContinuity
    );
    let ticket = match valid.admission().outcome() {
        AdmissionOutcome::Accepted(ticket) => *ticket,
        outcome => panic!("expected valid replay ticket, got {outcome:?}"),
    };
    let dispatched = owner.dispatch_next().expect("dispatch valid replay");
    let proof = dispatched
        .normalized()
        .proof()
        .watcher_continuity()
        .expect("valid continuity proof")
        .clone();
    owner
        .mark_dispatched(ticket)
        .expect("valid replay dispatched");
    owner.mark_applied(ticket).expect("valid replay applied");
    assert_eq!(owner.supervisor().in_flight(), 1);

    let paths = if kind == RawEventKind::Overflow {
        Vec::new()
    } else {
        vec![RawObservedPath::new("one.wav".into(), RawPathRole::Subject)]
    };
    let batch = SyntheticObservationBatch::new(
        RawObservationProvenance::new(
            source.clone(),
            Some(root.clone()),
            Some(stream.clone()),
            lane.generation(),
            boundary,
        ),
        vec![RawObservation::new(kind, paths)],
        lane_limits,
    );
    let admission = owner
        .admit_replay_with_durable_prior(batch, Some(&prior), true)
        .unwrap_or_else(|error| panic!("{label} remains auditable: {error:?}"));
    assert_eq!(
        admission.admission().disposition(),
        AdapterDisposition::SourceAuditRequired,
        "{label} must widen to an audit"
    );
    let request = admission.audit_request().expect("source audit request");
    assert_eq!(request.source_id(), &source);
    assert_eq!(request.root_identity(), &root);
    assert_eq!(request.generation(), lane.generation());
    let marker = owner
        .durable_audit_marker_for_replay(&admission)
        .expect("current-lane durable audit marker");
    assert_eq!(marker.scope(), ReconciliationScopeKind::SourceAudit);
    assert!(marker.boundary().covers(request.boundary()));
    assert!(!marker.reasons().is_empty());

    match (kind, admission.admission().outcome()) {
        (RawEventKind::Overflow, AdmissionOutcome::Rejected(_)) => {
            assert_eq!(owner.dispatch_next(), None);
        }
        (RawEventKind::Create | RawEventKind::Modify, AdmissionOutcome::Accepted(ticket)) => {
            let ticket = *ticket;
            let dispatched = owner.dispatch_next().expect("dispatch unproven audit work");
            assert_eq!(dispatched.ticket(), ticket);
            assert!(dispatched.normalized().proof().is_unproven());
            assert!(!dispatched.normalized().scopes().is_empty());
            assert!(
                dispatched
                    .normalized()
                    .scopes()
                    .iter()
                    .any(|scope| scope.kind() == ReconciliationScopeKind::ExactEntry)
            );
            owner
                .mark_dispatched(ticket)
                .expect("audit work dispatched");
            owner.mark_applied(ticket).expect("audit work applied");
            owner
                .mark_unproven_audit_handed_off(ticket)
                .expect("handoff for complete audit");
        }
        (_, outcome) => panic!("unexpected {label} outcome: {outcome:?}"),
    }
    assert_eq!(owner.supervisor().in_flight(), 1, "valid proof stays held");

    writer
        .retain_watcher_replay_uncertainty(&expected, &marker)
        .unwrap_or_else(|error| panic!("persist {label} uncertainty: {error:?}"));
    assert_eq!(writer.current_scan_fence().unwrap(), expected);

    let audit_request = owner
        .source_audit_request_for_current_lane(&source)
        .expect("current audit request");
    let retained_markers = owner.supervisor().retained_uncertainties().len();
    assert!(retained_markers > 0, "{label} retains an audit barrier");
    assert_eq!(
        owner.acknowledge_source_audit_receipt(&audit_request.incomplete()),
        crate::sample_sources::reconciliation::ReconciliationAcknowledgementOutcome::unchanged(
            retained_markers,
        ),
        "an incomplete audit cannot retire uncertainty"
    );
    assert!(
        writer
            .catalog_database()
            .get_metadata(crate::sample_sources::db::META_SOURCE_WATCHER_UNCERTAINTY)
            .expect("retained uncertainty metadata")
            .is_some()
    );

    let mut candidate = writer
        .catalog_database()
        .write_batch()
        .expect("candidate replay manifest");
    candidate
        .upsert_file(Path::new("unproven-derived.wav"), 30, 3)
        .expect("candidate derived source fact");
    assert!(matches!(
        writer.commit_watcher_replay_manifest_batch(&expected, candidate, &prior, &proof),
        Err(SourceWriterError::ReplayCheckpointMismatch)
    ));
    assert_eq!(
        source_writer_state_snapshot(writer.catalog_database()),
        before,
        "uncertain replay preserves the last-good row and projection metadata"
    );
    assert!(
        writer
            .catalog_database()
            .entry_for_path(Path::new("unproven-derived.wav"))
            .unwrap()
            .is_none()
    );
    assert_eq!(owner.supervisor().in_flight(), 1);
    assert!(owner.mark_replay_checkpointed(ticket, &prior).is_err());
    assert_eq!(owner.supervisor().in_flight(), 1);
    assert_eq!(
        writer
            .catalog_database()
            .get_metadata(crate::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT)
            .unwrap()
            .as_deref(),
        Some(checkpoint_before.as_str()),
        "uncertain replay leaves the persisted checkpoint bytes unchanged"
    );
    assert!(writer
        .catalog_database()
        .read_durable_replay_prior(
            &source,
            &root,
            WatcherGeneration::new(7),
            lane.generation(),
        )
        .unwrap()
        .is_none());
}

#[test]
fn source_writer_commits_only_exact_replay_proof_and_keeps_duplicate_idempotent() {
    let fixture = CurationFixture::new_profile_bound();
    let source = SourceId::from_string("replay-checkpoint-writer");
    let root = fixture.root_identity.clone();
    let stream =
        BackendStreamIdentity::from_fsevents_volume(99, [2; 16]).expect("native volume identity");
    let root_text = std::str::from_utf8(root.as_bytes()).expect("fixture root identity");
    let seed = serde_json::json!({
        "root_identity": root_text,
        "event_id": 10,
        "format_version": 4,
        "source_id": source.as_str(),
        "lifecycle_generation": 7,
        "source_revision": fixture.revision,
        "cause": "targeted_replay",
        "continuity_proof": {
            "root_identity": root_text,
            "backend": "fsevents",
            "backend_device": 99,
            "backend_volume_uuid": uuid::Uuid::from_bytes([2; 16]).to_string(),
            "watcher_generation": 1,
            "replay_coverage_start_event_id": 10,
            "replay_coverage_end_event_id": 10,
            "acknowledged_end_event_id": 10
        }
    })
    .to_string();
    let mut seed_batch = fixture.database.write_batch().expect("seed batch");
    seed_batch
        .set_metadata(
            crate::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT,
            &seed,
        )
        .expect("seed checkpoint");
    seed_batch.commit_auxiliary_state().expect("seed commit");
    let writer = SourceWriter::new(fixture.database, source.clone(), 7).expect("source writer");
    let before_replay = writer.current_scan_fence().expect("source revision");
    let mut manifest = writer
        .catalog_database()
        .write_batch()
        .expect("targeted batch");
    manifest
        .upsert_file(Path::new("three.wav"), 30, 3)
        .expect("targeted scan fact");
    writer
        .commit_scan_manifest_batch(&before_replay, manifest)
        .expect("targeted manifest commit");
    let expected = writer.current_scan_fence().expect("post-scan revision");
    assert!(
        writer.current_revision().is_err(),
        "targeted scan is not a full audit"
    );

    let lane_limits = RawObservationLimits::new(8, 4096, 4096).expect("lane limits");
    let limits = ReconciliationAdmissionLimits::new(
        1,
        lane_limits,
        RawObservationLimits::new(32, 16384, 16384).expect("global limits"),
        2,
        8,
        8,
    )
    .expect("admission limits");
    let mut owner =
        ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(limits));
    let lane = owner
        .begin_source(source.clone(), root.clone())
        .expect("capture lane");
    let prior = writer
        .catalog_database()
        .read_durable_replay_prior(
            &source,
            &root,
            crate::sample_sources::reconciliation::WatcherGeneration::new(7),
            lane.generation(),
        )
        .expect("read prior")
        .expect("valid prior");
    let batch = SyntheticObservationBatch::new(
        RawObservationProvenance::new(
            source.clone(),
            Some(root.clone()),
            Some(stream.clone()),
            lane.generation(),
            CaptureBoundary::try_new(20, Some(11), Some(12)).expect("capture interval"),
        ),
        vec![RawObservation::new(
            RawEventKind::Modify,
            vec![RawObservedPath::new("one.wav".into(), RawPathRole::Subject)],
        )],
        lane_limits,
    );
    let admission = owner
        .admit_replay_with_durable_prior(batch, Some(&prior), true)
        .expect("admit replay");
    assert_eq!(
        admission.admission().disposition(),
        AdapterDisposition::AdmittedWithContinuity
    );
    let ticket = match admission.admission().outcome() {
        AdmissionOutcome::Accepted(ticket) => *ticket,
        outcome => panic!("expected replay ticket, got {outcome:?}"),
    };
    let dispatched = owner.dispatch_next().expect("dispatch replay");
    assert_eq!(dispatched.ticket(), ticket);
    let proof = dispatched
        .normalized()
        .proof()
        .watcher_continuity()
        .expect("admitted proof")
        .clone();
    owner.mark_dispatched(ticket).expect("replay handed off");
    owner.mark_applied(ticket).expect("replay applied");
    let committed = writer
        .commit_watcher_replay_checkpoint(&expected, &prior, &proof)
        .expect("checkpoint commit");
    assert_eq!(committed.acknowledged_sequence(), 12);
    owner
        .mark_replay_checkpointed(ticket, &committed)
        .expect("retire with persisted checkpoint");
    assert_eq!(
        writer
            .current_scan_fence()
            .expect("revision after checkpoint"),
        expected
    );
    assert_eq!(
        writer
            .commit_watcher_replay_checkpoint(&expected, &prior, &proof)
            .expect("duplicate checkpoint"),
        committed
    );
    assert!(matches!(
        writer.commit_watcher_replay_checkpoint(&expected, &committed, &proof),
        Err(SourceWriterError::ReplayCheckpointMismatch)
    ));
    let mut later = writer
        .catalog_database()
        .write_batch()
        .expect("later scan batch");
    later
        .upsert_file(Path::new("four.wav"), 40, 4)
        .expect("later scan fact");
    writer
        .commit_scan_manifest_batch(&expected, later)
        .expect("advance source revision");
    assert!(matches!(
        writer.commit_watcher_replay_checkpoint(&expected, &prior, &proof),
        Err(SourceWriterError::StaleRevision { .. })
    ));
}

#[test]
fn source_writer_routes_readiness_claim_failure_retry_and_completion() {
    let mut fixture = CurationFixture::new_profile_bound();
    let source_id = SourceId::from_string("readiness-writer-test");
    fixture
        .database
        .connection
        .execute(
            "UPDATE wav_files SET file_identity = CASE path
             WHEN 'one.wav' THEN 'file-one' WHEN 'two.wav' THEN 'file-two' END",
            [],
        )
        .expect("seed manifest identities");
    let source_target = ReadinessTarget::source(
        source_id.as_str(),
        ReadinessStage::SimilarityLayout,
        "layout-v1",
        1,
        "membership-v1",
    );
    let mut targets = Vec::new();
    for (identity, path, content) in [
        ("file-one", "one.wav", "content-one"),
        ("file-two", "two.wav", "content-two"),
    ] {
        for (stage, version) in [
            (ReadinessStage::IndexedIdentity, "identity-v1"),
            (ReadinessStage::AnalysisFeatures, "features-v1"),
            (ReadinessStage::EmbeddingAspects, "embedding-v1"),
        ] {
            targets.push(ReadinessTarget::file(
                source_id.as_str(),
                identity,
                path,
                stage,
                version,
                1,
                content,
            ));
        }
    }
    let target = targets[0].clone();
    targets.push(source_target);
    let deficits = {
        let mut store = fixture
            .database
            .readiness_store()
            .expect("owned readiness store");
        let origin = store
            .capture_publication_origin()
            .expect("publication origin");
        store
            .publish_targets(
                &origin,
                &ReadinessTargetPublication::new(
                    source_id.as_str(),
                    1,
                    1,
                    SourceAvailability::Active,
                    "readiness-contract-v1",
                    &targets,
                    1,
                ),
            )
            .expect("publish target");
        let deficits = store
            .reconcile(source_id.as_str(), 10)
            .expect("reconcile")
            .deficits;
        assert!(deficits.iter().any(|deficit| deficit.target == target));
        deficits
    };
    let mut writer =
        SourceWriter::new(fixture.database, source_id.clone(), 7).expect("source writer");
    let initial = writer.current_revision().expect("initial revision");
    writer
        .commit_catalog_curation_command(
            &initial,
            &fixture.profile_guard,
            &[SourceCurationUpdate::SetRating {
                path: CurationFixture::path("one.wav"),
                rating: Rating::KEEP_3,
            }],
        )
        .expect("advance source revision");
    assert!(matches!(
        writer.claim_readiness_target(&initial, &target, 10, 100),
        Err(SourceWriterError::StaleRevision { .. })
    ));
    let revision = writer.current_revision().expect("current revision");
    assert!(matches!(
        writer.enqueue_readiness_jobs(&initial, &deficits, 10),
        Err(SourceWriterError::StaleRevision { .. })
    ));
    let oversized = vec![deficits[0].clone(); 65];
    assert!(matches!(
        writer.enqueue_readiness_jobs(&revision, &oversized, 10),
        Err(SourceWriterError::InvalidRequest(_))
    ));
    let mut oversized_bytes = deficits[0].clone();
    oversized_bytes.target.content_generation =
        "x".repeat(crate::operation_journal::MAX_RECORD_BYTES);
    assert!(matches!(
        writer.enqueue_readiness_jobs(&revision, &[oversized_bytes], 10),
        Err(SourceWriterError::InvalidRequest(_))
    ));
    let mut mixed_source = deficits.clone();
    mixed_source[0].target.source_id = String::from("another-source");
    assert!(matches!(
        writer.enqueue_readiness_jobs(&revision, &mixed_source, 10),
        Err(SourceWriterError::StaleFence)
    ));
    let jobs_before: i64 = writer
        .catalog_database()
        .connection
        .query_row("SELECT COUNT(*) FROM analysis_jobs", [], |row| row.get(0))
        .expect("work row count before admission");
    assert_eq!(jobs_before, 0);
    assert!(
        writer
            .enqueue_readiness_jobs(&revision, &deficits, 10)
            .expect("enqueue through writer")
            > 0
    );

    let mut wrong_source = target.clone();
    wrong_source.source_id = String::from("another-source");
    assert!(matches!(
        writer.claim_readiness_target(&revision, &wrong_source, 10, 100),
        Err(SourceWriterError::StaleFence)
    ));

    let claim = writer
        .claim_readiness_target(&revision, &target, 10, 100)
        .expect("claim")
        .expect("work available");
    let retry_policy = ReadinessRetryPolicy::new(3, 10, 2).expect("retry policy");
    assert_eq!(
        writer
            .fail_readiness_work(
                &revision,
                &claim,
                ReadinessFailureClassification::Retryable,
                "transient",
                "retry the same target",
                11,
                retry_policy,
            )
            .expect("record failure"),
        ReadinessFailureOutcome::RetryScheduled { retry_at: 14 }
    );
    assert!(
        writer
            .claim_readiness_target(&revision, &target, 13, 100)
            .expect("before retry")
            .is_none()
    );
    let retry = writer
        .claim_readiness_target(&revision, &target, 14, 100)
        .expect("retry")
        .expect("retry due");
    assert_eq!(retry.failure_attempts(), 1);
    assert_eq!(
        writer
            .complete_readiness_work(&revision, &claim, 15, None)
            .expect("stale completion"),
        ArtifactPublishOutcome::RejectedStale
    );
    assert_eq!(
        writer
            .complete_readiness_work(&revision, &retry, 15, None)
            .expect("completion"),
        ArtifactPublishOutcome::Recorded
    );
    let database = writer.into_catalog_database();
    let mut replacement = SourceWriter::new(database, source_id, 8).expect("new lifecycle");
    assert!(matches!(
        replacement.claim_readiness_target(&revision, &target, 16, 100),
        Err(SourceWriterError::StaleFence)
    ));
}

impl CurationFixture {
    fn new() -> Self {
        Self::new_with_profile_child(None)
    }

    fn new_profile_bound() -> Self {
        Self::new_with_profile_child(Some(PathBuf::from("curation-profile-bound")))
    }

    fn new_with_profile_child(profile_child: Option<PathBuf>) -> Self {
        let runtime = crate::test_runtime::TestRuntimeGuard::acquire();
        // macOS exposes the default temporary root through /var -> /private/var. Resolve that
        // ambient alias before exercising the no-follow profile capability.
        let temporary_root =
            std::fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root");
        let config_base_dir = tempfile::tempdir_in(temporary_root).expect("config base");
        let config_base = ConfigBaseGuard::set(config_base_dir.path().to_path_buf());
        let profile = PersistenceProfileGuard::named("source-curation-tests");
        let profile_guard = WritableProfileGuard::acquire_current().expect("profile guard");
        let source = tempfile::tempdir().expect("source root");
        let database = match profile_child.as_ref() {
            Some(profile_child) => SourceDatabase::open_for_catalog_with_profile_guard(
                source.path(),
                &profile_guard,
                profile_child,
            )
            .expect("profile-bound source db"),
            None => SourceDatabase::open_for_source_write(source.path()).expect("source db"),
        };
        database
            .upsert_file(Path::new("one.wav"), 10, 1)
            .expect("first file");
        database
            .upsert_file(Path::new("two.wav"), 20, 2)
            .expect("second file");
        database
            .begin_source_directory_truth_generation(1, 0)
            .expect("directory generation");
        let root_identity = match profile_child.as_ref() {
            Some(_) => RootIdentity::from_bytes(
                database
                    .source_lease
                    .as_ref()
                    .expect("profile-bound source lease")
                    .media_root_binding()
                    .identity()
                    .as_bytes()
                    .to_vec(),
            ),
            None => RootIdentity::from_bytes(b"curation-root".to_vec()),
        };
        database
            .write_batch()
            .expect("audit batch")
            .commit_manifest_audit_with_staged_directory_truth(
                1,
                2,
                10,
                root_identity.clone(),
                None,
            )
            .expect("authoritative audit");
        let revision = database.get_revision().expect("audit revision");
        assert_eq!(revision, 3);

        Self {
            _runtime: runtime,
            _config_base_dir: config_base_dir,
            _config_base: config_base,
            _profile: profile,
            _source: source,
            #[cfg(unix)]
            profile_child,
            database,
            profile_guard,
            root_identity,
            revision,
        }
    }

    fn path(path: &str) -> RootRelativePath {
        RootRelativePath::try_from_path(PathBuf::from(path)).expect("root-relative path")
    }

    fn commit(
        &self,
        updates: &[SourceCurationUpdate],
    ) -> Result<
        crate::sample_sources::SourceCurationCommitReceipt,
        crate::sample_sources::SourceDbError,
    > {
        self.database.commit_curation_with_profile_guard(
            &self.profile_guard,
            self.revision,
            &self.root_identity,
            updates,
        )
    }
}

#[test]
fn catalog_curation_command_renews_live_source_lease_after_idle() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("idle-curation-writer"),
        1,
    )
    .expect("source writer");
    let expected = writer.current_revision().expect("current revision");
    writer
        .database_for_tests()
        .connection
        .execute(
            "UPDATE source_lease SET expires_at = acquired_at WHERE singleton = 1",
            [],
        )
        .expect("expire retained source lease");
    assert!(matches!(
        writer.current_revision(),
        Err(SourceWriterError::Database(SourceDbError::SourceLease(
            crate::sample_sources::db::SourceLeaseError::Expired { .. }
        )))
    ));

    let renewed = writer
        .current_revision_for_command()
        .expect("live owner renews before command qualification");
    assert_eq!(renewed.revision(), expected.revision());
    let receipt = writer
        .commit_catalog_curation_command(
            &renewed,
            &fixture.profile_guard,
            &[SourceCurationUpdate::SetRating {
                path: CurationFixture::path("one.wav"),
                rating: Rating::KEEP_1,
            }],
        )
        .expect("curation after idle");
    assert_eq!(receipt.source_revision(), expected.revision() + 1);
    assert_eq!(
        writer
            .database_for_tests()
            .entry_for_path(Path::new("one.wav"))
            .expect("entry after curation")
            .expect("rated entry")
            .tag,
        Rating::KEEP_1
    );
}

/// Test-only binding whose retained identity can be invalidated on every host.
///
/// Unix tests below exercise actual profile-child replacement. This seam keeps the same
/// profile-bound database path while proving the mutation fence does not depend on a particular
/// platform's rename semantics.
struct ControllableDatabaseRootBinding {
    valid: Arc<AtomicBool>,
}

impl DatabaseRootBindingGuard for ControllableDatabaseRootBinding {
    fn validate_profile_guard(
        &self,
        profile_guard: &WritableProfileGuard,
    ) -> Result<(), ProfileOwnershipError> {
        if self.valid.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(ProfileOwnershipError::ProfileRootReplaced {
                path: profile_guard.profile_root().to_path_buf(),
            })
        }
    }
}

fn metadata_number(database: &SourceDatabase, key: &str) -> u64 {
    database
        .get_metadata(key)
        .expect("metadata")
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

fn continuity_state(database: &SourceDatabase) -> (u64, SourceDirectoryTruthState) {
    (
        database
            .last_authoritative_audit_metadata()
            .expect("audit metadata")
            .expect("audit metadata present")
            .source_revision,
        database
            .source_directory_truth_state()
            .expect("directory truth state"),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceWriterStateSnapshot {
    revision: u64,
    continuity: (u64, SourceDirectoryTruthState),
    paths_revision: u64,
    identities_revision: u64,
    index_revision: u64,
    entry: (i64, i64, Option<i64>, Option<i64>),
    receipt_count: i64,
}

fn source_writer_state_snapshot(database: &SourceDatabase) -> SourceWriterStateSnapshot {
    SourceWriterStateSnapshot {
        revision: database.get_revision().expect("source revision"),
        continuity: continuity_state(database),
        paths_revision: metadata_number(database, "wav_paths_revision_v1"),
        identities_revision: metadata_number(database, "wav_identities_revision_v1"),
        index_revision: metadata_number(database, "source_index_revision_v1"),
        entry: database
            .connection
            .query_row(
                "SELECT tag, locked, last_played_at, last_curated_at
                 FROM wav_files WHERE path = 'one.wav'",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .expect("curation state row"),
        receipt_count: database
            .connection
            .query_row(
                "SELECT count(*) FROM source_participant_curation_receipts",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("receipt count"),
    }
}

#[test]
fn scan_fence_bootstraps_before_audit_without_weakening_curation_fence() {
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let temporary_root =
        std::fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root");
    let config_base_dir = tempfile::tempdir_in(temporary_root).expect("config base");
    let _config_base = ConfigBaseGuard::set(config_base_dir.path().to_path_buf());
    let _profile = PersistenceProfileGuard::named("source-scan-fence-test");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile guard");
    let source = tempfile::tempdir().expect("source root");
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source.path(),
        &profile_guard,
        Path::new("scan-fence-profile-bound"),
    )
    .expect("profile-bound source db");
    assert!(
        database
            .last_authoritative_audit_metadata()
            .expect("audit metadata")
            .is_none()
    );

    let writer =
        SourceWriter::new_for_scan(database, SourceId::from_string("source-scan-fence-test"), 7)
            .expect("scan owner before audit");
    let initial = writer.current_scan_fence().expect("initial scan fence");
    assert_eq!(initial.lifecycle_generation(), 7);
    assert!(initial.lease_epoch() > 0);
    assert_eq!(initial.revision(), 0);
    assert_eq!(
        writer
            .validate_scan_fence(&initial)
            .expect("same owner and revision"),
        initial
    );
    assert!(matches!(
        writer.current_revision(),
        Err(SourceWriterError::InvalidRequest(
            "source writer requires a complete authoritative audit"
        ))
    ));

    let mut scan_batch = writer
        .catalog_database()
        .write_batch()
        .expect("bounded scan batch");
    scan_batch
        .upsert_file(Path::new("one.wav"), 10, 1)
        .expect("prepared scan row");
    let committed = writer
        .commit_scan_manifest_batch(&initial, scan_batch)
        .expect("owner commits bounded scan row");
    assert_eq!(committed.revision, 1);
    assert_eq!(committed.touched_path_changes.len(), 1);
    assert_eq!(committed.touched_path_changes[0].0, Path::new("one.wav"));
    let mut stale_batch = writer
        .catalog_database()
        .write_batch()
        .expect("stale bounded batch");
    stale_batch
        .upsert_file(Path::new("stale.wav"), 20, 2)
        .expect("prepared stale row");
    assert!(matches!(
        writer.commit_scan_manifest_batch(&initial, stale_batch),
        Err(SourceWriterError::StaleRevision {
            expected: 0,
            actual: 1
        })
    ));
    assert!(
        writer
            .catalog_database()
            .entry_for_path(Path::new("stale.wav"))
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        writer.validate_scan_fence(&initial),
        Err(SourceWriterError::StaleRevision {
            expected: 0,
            actual: 1
        })
    ));
    assert!(matches!(
        writer.current_revision(),
        Err(SourceWriterError::InvalidRequest(
            "source writer requires a complete authoritative audit"
        ))
    ));

    writer
        .database_for_tests()
        .begin_source_directory_truth_generation(1, 0)
        .expect("directory generation");
    writer
        .database_for_tests()
        .write_batch()
        .expect("audit batch")
        .commit_manifest_audit_with_staged_directory_truth(
            1,
            1,
            10,
            initial.root_identity().clone(),
            None,
        )
        .expect("complete audit");
    assert_eq!(
        writer
            .current_revision()
            .expect("curation fence after completed audit")
            .revision(),
        2
    );
}

#[test]
fn scan_fence_requires_retained_profile_owned_source() {
    let fixture = CurationFixture::new();
    assert!(matches!(
        SourceWriter::new_for_scan(
            fixture.database,
            SourceId::from_string("unbound-scan-source"),
            1,
        ),
        Err(SourceWriterError::Database(
            SourceDbError::ProfileOwnershipRequired
        ))
    ));
}

#[test]
fn playback_history_commits_preserve_curation_and_reject_stale_writers() {
    let mut fixture = CurationFixture::new();
    let initial = fixture
        .commit(&[
            SourceCurationUpdate::SetRating {
                path: CurationFixture::path("one.wav"),
                rating: Rating::KEEP_3,
            },
            SourceCurationUpdate::SetLocked {
                path: CurationFixture::path("one.wav"),
                locked: true,
            },
            SourceCurationUpdate::AddCollection {
                path: CurationFixture::path("one.wav"),
                collection: SampleCollection::new(2).unwrap(),
            },
        ])
        .unwrap();
    fixture.revision = initial.source_revision();
    let mut expected = initial.files()[0].clone();
    let update = SourceCurationUpdate::SetLastPlayedAt {
        path: CurationFixture::path("one.wav"),
        played_at: Some(1_800_000_000),
    };
    let receipt = fixture.commit(&[update]).unwrap();
    expected.last_played_at = Some(1_800_000_000);
    assert_eq!(receipt.files(), &[expected.clone()]);
    assert_eq!(receipt.source_revision(), fixture.revision + 1);
    assert_eq!(
        receipt.directory_generation(),
        initial.directory_generation()
    );
    assert_eq!(receipt.root_identity(), initial.root_identity());
    let continuity = continuity_state(&fixture.database);
    assert_eq!(continuity.0, receipt.source_revision());

    let clear = SourceCurationUpdate::SetLastPlayedAt {
        path: CurationFixture::path("one.wav"),
        played_at: None,
    };
    assert!(fixture.commit(std::slice::from_ref(&clear)).is_err());
    assert_eq!(continuity_state(&fixture.database), continuity);
    let snapshot = fixture.database.browser_metadata_snapshot().unwrap();
    assert_eq!(snapshot.revision, receipt.source_revision());
    assert_eq!(snapshot.files[0], expected);

    fixture.revision = receipt.source_revision();
    let cleared = fixture.commit(&[clear]).unwrap();
    expected.last_played_at = None;
    assert_eq!(cleared.files(), &[expected]);
    assert_eq!(cleared.source_revision(), fixture.revision + 1);
    assert_eq!(
        continuity_state(&fixture.database).0,
        cleared.source_revision()
    );
}

#[test]
fn valid_curation_commit_preserves_continuity_and_returns_bounded_rows() {
    let fixture = CurationFixture::new();
    let paths_revision = metadata_number(&fixture.database, "wav_paths_revision_v1");
    let identities_revision = metadata_number(&fixture.database, "wav_identities_revision_v1");
    let index_revision = metadata_number(&fixture.database, "source_index_revision_v1");
    let unrelated_before = fixture
        .database
        .entry_for_path(Path::new("two.wav"))
        .expect("unrelated row")
        .expect("unrelated row present");

    let first = SampleCollection::new(0).expect("collection slot");
    let second = SampleCollection::new(1).expect("collection slot");
    let updates = [
        SourceCurationUpdate::SetRating {
            path: CurationFixture::path("one.wav"),
            rating: Rating::KEEP_3,
        },
        SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        },
        SourceCurationUpdate::AddCollection {
            path: CurationFixture::path("one.wav"),
            collection: first,
        },
        SourceCurationUpdate::AddCollection {
            path: CurationFixture::path("one.wav"),
            collection: second,
        },
    ];
    let receipt = fixture.commit(&updates).expect("curation commit");

    assert_eq!(receipt.source_revision(), fixture.revision + 1);
    assert_eq!(receipt.root_identity(), &fixture.root_identity);
    assert_eq!(receipt.directory_generation(), 1);
    assert_eq!(receipt.files().len(), 1);
    assert_eq!(receipt.files()[0].relative_path, PathBuf::from("one.wav"));
    assert_eq!(receipt.files()[0].rating, Rating::KEEP_3);
    assert!(receipt.files()[0].locked);
    assert_eq!(receipt.files()[0].collections, vec![first, second]);
    assert!(receipt.files()[0].last_curated_at.is_some());

    assert_eq!(
        fixture.database.get_revision().unwrap(),
        fixture.revision + 1
    );
    assert_eq!(
        metadata_number(&fixture.database, "wav_paths_revision_v1"),
        paths_revision
    );
    assert_eq!(
        metadata_number(&fixture.database, "wav_identities_revision_v1"),
        identities_revision
    );
    assert_eq!(
        metadata_number(&fixture.database, "source_index_revision_v1"),
        index_revision
    );
    assert_eq!(
        continuity_state(&fixture.database),
        (
            fixture.revision + 1,
            SourceDirectoryTruthState::Active {
                generation: 1,
                published_source_revision: fixture.revision + 1,
            }
        )
    );
    assert_eq!(
        fixture
            .database
            .last_authoritative_audit_metadata()
            .unwrap()
            .unwrap()
            .root_identity,
        fixture.root_identity
    );
    let unrelated_after = fixture
        .database
        .entry_for_path(Path::new("two.wav"))
        .unwrap()
        .unwrap();
    assert_eq!(unrelated_after.tag, unrelated_before.tag);
    assert_eq!(unrelated_after.file_size, unrelated_before.file_size);
    assert_eq!(unrelated_after.modified_ns, unrelated_before.modified_ns);
}

#[test]
fn curation_receipt_matches_browser_playback_type_projection() {
    let fixture = CurationFixture::new();
    let path = Path::new("one.wav");
    let mut tag_batch = fixture.database.write_batch().expect("tag batch");
    tag_batch
        .assign_tag_to_path(path, " One\t-\tShot ")
        .expect("canonical playback tag");
    tag_batch
        .commit_auxiliary_state()
        .expect("auxiliary tag commit");

    let revision = fixture.revision;
    let snapshot = fixture
        .database
        .browser_metadata_snapshot()
        .expect("browser snapshot");
    let page = fixture
        .database
        .browser_metadata_page(revision, None, 64)
        .expect("browser page");
    let exact = fixture
        .database
        .browser_metadata_for_paths_at_revision(revision, &[path.to_path_buf()])
        .expect("exact browser metadata");
    let receipt = fixture
        .commit(&[SourceCurationUpdate::SetRating {
            path: CurationFixture::path("one.wav"),
            rating: Rating::KEEP_1,
        }])
        .expect("curation receipt");

    let expected = Some(SourcePlaybackType::OneShot);
    assert_eq!(
        snapshot
            .files
            .iter()
            .find(|file| file.relative_path == path)
            .expect("snapshot row")
            .playback_type,
        expected
    );
    assert_eq!(
        page.files
            .iter()
            .find(|file| file.relative_path == path)
            .expect("page row")
            .playback_type,
        expected
    );
    assert_eq!(exact.files[0].playback_type, expected);
    assert_eq!(receipt.files()[0].playback_type, expected);
}

#[test]
fn curation_tag_mutations_replace_assigned_playback_types_and_preserve_locked_metadata() {
    let mut fixture = CurationFixture::new();
    let one = Path::new("one.wav");
    let two = Path::new("two.wav");
    let mut seed = fixture.database.write_batch().expect("tag batch");
    for label in [
        "loop",
        "loop-",
        "oneshot",
        "one-shot",
        "unrelated",
        "embedded\0tag",
    ] {
        seed.assign_tag_to_path(one, label).expect("seed tag");
    }
    seed.commit_auxiliary_state().expect("seed tags");

    let locked = fixture
        .commit(&[
            SourceCurationUpdate::SetRating {
                path: CurationFixture::path("two.wav"),
                rating: Rating::KEEP_3,
            },
            SourceCurationUpdate::SetLocked {
                path: CurationFixture::path("two.wav"),
                locked: true,
            },
        ])
        .expect("lock second file");
    fixture.revision = locked.source_revision();

    let receipt = fixture
        .commit(&[
            SourceCurationUpdate::AddTag {
                path: CurationFixture::path("one.wav"),
                label: String::from(" One\t-\tShot "),
            },
            SourceCurationUpdate::AddTag {
                path: CurationFixture::path("two.wav"),
                label: String::from(" LOOP "),
            },
        ])
        .expect("tag curation");
    assert_eq!(receipt.source_revision(), fixture.revision + 1);
    assert_eq!(receipt.files().len(), 2);
    assert_eq!(receipt.files()[0].relative_path, one);
    assert_eq!(
        receipt.files()[0].playback_type,
        Some(SourcePlaybackType::OneShot)
    );
    assert!(receipt.files()[0].last_curated_at.is_some());
    assert_eq!(receipt.files()[1].relative_path, two);
    assert_eq!(
        receipt.files()[1].playback_type,
        Some(SourcePlaybackType::Loop)
    );
    assert_eq!(receipt.files()[1].rating, Rating::KEEP_3);
    assert!(receipt.files()[1].locked);
    assert!(receipt.files()[1].last_curated_at.is_some());

    let normalized_tags = fixture
        .database
        .tags_for_path(one)
        .expect("one tags")
        .into_iter()
        .map(|tag| tag.normalized_text)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        normalized_tags,
        BTreeSet::from([
            String::from("embedded\0tag"),
            String::from("one - shot"),
            String::from("unrelated"),
        ])
    );
    assert_eq!(
        fixture.database.tags_for_path(two).unwrap().len(),
        1,
        "the locked file receives its playback tag without inheriting another file's tags"
    );

    fixture.revision = receipt.source_revision();
    let removed = fixture
        .commit(&[SourceCurationUpdate::RemoveTag {
            path: CurationFixture::path("one.wav"),
            label: String::from(" ONE\t-\tSHOT "),
        }])
        .expect("remove normalized tag");
    assert_eq!(removed.files()[0].playback_type, None);
    assert_eq!(
        fixture
            .database
            .tags_for_path(one)
            .unwrap()
            .into_iter()
            .map(|tag| tag.normalized_text)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([String::from("embedded\0tag"), String::from("unrelated")])
    );
}

#[test]
fn curation_tag_labels_are_bounded_before_any_transaction_mutation() {
    let fixture = CurationFixture::new();
    let valid = SourceCurationUpdate::AddTag {
        path: CurationFixture::path("one.wav"),
        label: String::from("valid"),
    };
    let oversized = SourceCurationUpdate::AddTag {
        path: CurationFixture::path("one.wav"),
        label: "x".repeat(SOURCE_CURATION_MAX_TAG_LABEL_BYTES + 1),
    };
    assert!(matches!(
        fixture.commit(std::slice::from_ref(&oversized)),
        Err(SourceDbError::TagLabelTooLong { .. })
    ));
    assert!(
        fixture
            .database
            .tags_for_path(Path::new("one.wav"))
            .unwrap()
            .is_empty()
    );

    let nul = SourceCurationUpdate::RemoveTag {
        path: CurationFixture::path("one.wav"),
        label: String::from("bad\0tag"),
    };
    assert!(matches!(
        fixture.commit(&[valid.clone(), nul]),
        Err(SourceDbError::TagLabelContainsNul)
    ));
    assert!(
        fixture
            .database
            .tags_for_path(Path::new("one.wav"))
            .unwrap()
            .is_empty()
    );

    let whitespace = SourceCurationUpdate::AddTag {
        path: CurationFixture::path("one.wav"),
        label: String::from(" \t\n"),
    };
    assert!(matches!(
        fixture.commit(std::slice::from_ref(&whitespace)),
        Err(SourceDbError::EmptyTagLabel)
    ));
    assert!(
        fixture
            .database
            .tags_for_path(Path::new("one.wav"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
}

#[test]
fn curation_tag_timestamp_semantics_match_existing_tag_primitives() {
    let mut fixture = CurationFixture::new();
    let path = Path::new("one.wav");
    let mut seed = fixture.database.write_batch().expect("tag batch");
    seed.assign_tag_to_path(path, "Kick").expect("seed tag");
    seed.set_last_curated_at(path, 1).expect("seed timestamp");
    seed.commit_auxiliary_state().expect("seed tag timestamp");

    let duplicate = fixture
        .commit(&[SourceCurationUpdate::AddTag {
            path: CurationFixture::path("one.wav"),
            label: String::from(" kICK "),
        }])
        .expect("duplicate curation tag");
    assert!(duplicate.files()[0].last_curated_at.unwrap() > 1);
    fixture.revision = duplicate.source_revision();

    let mut reset = fixture.database.write_batch().expect("timestamp batch");
    reset.set_last_curated_at(path, 2).expect("reset timestamp");
    reset.commit_auxiliary_state().expect("reset timestamp");

    let absent = fixture
        .commit(&[SourceCurationUpdate::RemoveTag {
            path: CurationFixture::path("one.wav"),
            label: String::from("missing"),
        }])
        .expect("absent curation tag");
    assert_eq!(absent.files()[0].last_curated_at, Some(2));
    assert_eq!(
        fixture.database.last_curated_at_for_path(path).unwrap(),
        Some(2)
    );
}

#[test]
fn stale_revision_root_missing_path_and_invalid_directory_reject_atomically() {
    let fixture = CurationFixture::new();
    let update = [SourceCurationUpdate::SetRating {
        path: CurationFixture::path("one.wav"),
        rating: Rating::KEEP_1,
    }];
    let initial_row = fixture
        .database
        .entry_for_path(Path::new("one.wav"))
        .unwrap()
        .unwrap();
    let initial_continuity = continuity_state(&fixture.database);

    assert!(
        fixture
            .database
            .commit_curation_with_profile_guard(
                &fixture.profile_guard,
                fixture.revision - 1,
                &fixture.root_identity,
                &update,
            )
            .is_err()
    );
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        initial_row.tag
    );
    assert_eq!(continuity_state(&fixture.database), initial_continuity);

    let wrong_root = RootIdentity::from_bytes(b"other-root".to_vec());
    assert!(
        fixture
            .database
            .commit_curation_with_profile_guard(
                &fixture.profile_guard,
                fixture.revision,
                &wrong_root,
                &update,
            )
            .is_err()
    );
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);

    let missing_path = [SourceCurationUpdate::SetRating {
        path: CurationFixture::path("missing.wav"),
        rating: Rating::TRASH_1,
    }];
    assert!(fixture.commit(&missing_path).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    assert_eq!(continuity_state(&fixture.database), initial_continuity);

    fixture
        .database
        .connection
        .execute(
            "INSERT INTO source_manifest_audit_seen (path) VALUES ('pending.wav')",
            [],
        )
        .expect("pending audit checkpoint");
    assert!(fixture.commit(&update).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    fixture
        .database
        .connection
        .execute(
            "DELETE FROM source_manifest_audit_seen WHERE path = 'pending.wav'",
            [],
        )
        .expect("clear pending audit checkpoint");

    fixture
        .database
        .connection
        .execute(
            "UPDATE source_directory_generations
             SET published_source_revision = ?1
             WHERE status = 'active'",
            [fixture.revision as i64 - 1],
        )
        .expect("corrupt directory generation");
    assert!(fixture.commit(&update).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        initial_row.tag
    );
}

#[test]
fn curation_admission_rejects_batches_above_bound_without_mutation() {
    let fixture = CurationFixture::new();
    let updates = (0..=SOURCE_CURATION_BATCH_LIMIT)
        .map(|_| SourceCurationUpdate::SetRating {
            path: CurationFixture::path("one.wav"),
            rating: Rating::KEEP_1,
        })
        .collect::<Vec<_>>();

    assert!(fixture.commit(&updates).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        Rating::NEUTRAL
    );
}

#[test]
fn curation_admission_rejects_invalid_deserialized_rating_without_mutation() {
    let fixture = CurationFixture::new();
    let invalid_rating: Rating = serde_json::from_str("127").expect("deserialized rating");
    let updates = [SourceCurationUpdate::SetRating {
        path: CurationFixture::path("one.wav"),
        rating: invalid_rating,
    }];

    assert!(fixture.commit(&updates).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        Rating::NEUTRAL
    );
}

#[test]
fn curation_admission_rejects_invalid_deserialized_collection_without_mutation() {
    let fixture = CurationFixture::new();
    let invalid_collection: SampleCollection =
        serde_json::from_str("255").expect("deserialized collection");
    let updates = [SourceCurationUpdate::AddCollection {
        path: CurationFixture::path("one.wav"),
        collection: invalid_collection,
    }];

    assert!(fixture.commit(&updates).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    assert!(
        fixture
            .database
            .collections_for_path(Path::new("one.wav"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn curation_admission_rejects_mixed_valid_and_invalid_values_atomically() {
    let fixture = CurationFixture::new();
    let invalid_collection: SampleCollection =
        serde_json::from_str("255").expect("deserialized collection");
    let updates = [
        SourceCurationUpdate::SetRating {
            path: CurationFixture::path("one.wav"),
            rating: Rating::KEEP_3,
        },
        SourceCurationUpdate::AddCollection {
            path: CurationFixture::path("one.wav"),
            collection: invalid_collection,
        },
    ];

    assert!(fixture.commit(&updates).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    let row = fixture
        .database
        .entry_for_path(Path::new("one.wav"))
        .unwrap()
        .unwrap();
    assert_eq!(row.tag, Rating::NEUTRAL);
    assert!(
        fixture
            .database
            .collections_for_path(Path::new("one.wav"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn curation_commit_helper_rejects_manifest_dirty_batches() {
    let fixture = CurationFixture::new();
    let mut batch = fixture.database.write_batch().expect("write batch");
    batch
        .upsert_file(Path::new("new.wav"), 30, 3)
        .expect("manifest mutation");

    assert!(batch.commit_curation_prepared().is_err());
    assert!(
        fixture
            .database
            .entry_for_path(Path::new("new.wav"))
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
}

#[cfg(unix)]
#[test]
fn profile_bound_curation_rejects_replaced_database_child_atomically() {
    let fixture = CurationFixture::new_profile_bound();
    let profile_child = fixture.profile_child.as_ref().expect("profile-bound child");
    let child_path = fixture.profile_guard.profile_root().join(profile_child);
    let moved_path = fixture
        .profile_guard
        .profile_root()
        .join("curation-profile-bound-moved");
    std::fs::rename(&child_path, &moved_path).expect("orphan retained database child");
    std::fs::create_dir(&child_path).expect("replacement database child");
    std::fs::write(child_path.join("sentinel"), b"untouched").expect("replacement sentinel");

    let initial_row = fixture
        .database
        .entry_for_path(Path::new("one.wav"))
        .unwrap()
        .unwrap();
    let initial_continuity = continuity_state(&fixture.database);
    let update = [SourceCurationUpdate::SetRating {
        path: CurationFixture::path("one.wav"),
        rating: Rating::KEEP_1,
    }];

    assert!(fixture.commit(&update).is_err());
    assert_eq!(fixture.database.get_revision().unwrap(), fixture.revision);
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        initial_row.tag
    );
    assert_eq!(continuity_state(&fixture.database), initial_continuity);
    assert!(child_path.join("sentinel").is_file());
}

#[cfg(unix)]
#[test]
fn profile_mutation_guard_rejects_replaced_profile_root_before_batch() {
    let fixture = CurationFixture::new_profile_bound();
    let profile_root = fixture.profile_guard.profile_root().to_path_buf();
    let displaced = profile_root.with_file_name("curation-profile-root-moved");
    std::fs::rename(&profile_root, &displaced).expect("orphan retained profile root");
    std::fs::create_dir(&profile_root).expect("replacement profile root");

    let result = fixture.database.write_batch();
    assert!(matches!(result, Err(SourceDbError::ProfileOwnership(_))));
    assert_eq!(revision_value(&fixture.database), fixture.revision as i64);
}

#[test]
fn profile_mutation_guard_enforces_begin_and_commit_on_all_platforms() {
    let mut fixture = CurationFixture::new_profile_bound();
    let valid = Arc::new(AtomicBool::new(true));
    fixture.database =
        fixture
            .database
            .retain_database_root_binding(Arc::new(ControllableDatabaseRootBinding {
                valid: Arc::clone(&valid),
            }));

    let before_revision = revision_value(&fixture.database);
    let before_tag = fixture
        .database
        .entry_for_path(Path::new("one.wav"))
        .unwrap()
        .unwrap()
        .tag;
    valid.store(false, Ordering::SeqCst);
    let result = fixture.database.write_batch();
    assert!(matches!(result, Err(SourceDbError::ProfileOwnership(_))));
    assert_eq!(revision_value(&fixture.database), before_revision);

    valid.store(true, Ordering::SeqCst);
    let mut batch = fixture.database.write_batch().expect("profile-owned batch");
    batch
        .set_tag(Path::new("one.wav"), Rating::KEEP_1)
        .expect("metadata mutation");
    valid.store(false, Ordering::SeqCst);
    let result = batch.commit();
    assert!(matches!(result, Err(SourceDbError::ProfileOwnership(_))));
    assert_eq!(revision_value(&fixture.database), before_revision);
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        before_tag
    );

    valid.store(true, Ordering::SeqCst);
    fixture
        .database
        .set_tag(Path::new("one.wav"), Rating::KEEP_1)
        .expect("valid profile-owned write");
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        Rating::KEEP_1
    );
}

#[cfg(unix)]
#[test]
fn profile_mutation_guard_rolls_back_batch_after_database_child_replacement() {
    let fixture = CurationFixture::new_profile_bound();
    let profile_child = fixture.profile_child.as_ref().expect("profile-bound child");
    let child_path = fixture.profile_guard.profile_root().join(profile_child);
    let moved_path = fixture
        .profile_guard
        .profile_root()
        .join("curation-profile-mid-batch-moved");
    let before_revision = revision_value(&fixture.database);
    let before_tag = fixture
        .database
        .entry_for_path(Path::new("one.wav"))
        .unwrap()
        .unwrap()
        .tag;

    let mut batch = fixture.database.write_batch().expect("profile-owned batch");
    batch
        .set_tag(Path::new("one.wav"), Rating::KEEP_1)
        .expect("metadata mutation");
    std::fs::rename(&child_path, &moved_path).expect("orphan retained database child");
    std::fs::create_dir(&child_path).expect("replacement database child");

    let result = batch.commit();
    assert!(matches!(
        result,
        Err(SourceDbError::SourceLease(
            SourceLeaseError::DatabaseRootReplaced { .. }
        ))
    ));
    assert_eq!(revision_value(&fixture.database), before_revision);
    assert_eq!(
        fixture
            .database
            .entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .tag,
        before_tag
    );
}

#[cfg(unix)]
#[test]
fn profile_mutation_guard_rejects_direct_journal_insert_after_child_replacement() {
    let fixture = CurationFixture::new_profile_bound();
    let profile_child = fixture.profile_child.as_ref().expect("profile-bound child");
    let child_path = fixture.profile_guard.profile_root().join(profile_child);
    let moved_path = fixture
        .profile_guard
        .profile_root()
        .join("curation-profile-journal-moved");
    std::fs::rename(&child_path, &moved_path).expect("orphan retained database child");
    std::fs::create_dir(&child_path).expect("replacement database child");

    let entry = FileOpJournalEntry::new_copy(
        String::from("profile-guard-journal"),
        CopyJournalEntryInit {
            target_relative: PathBuf::from("target.wav"),
            staged_relative: PathBuf::from(".yield-staged-target.wav"),
            tag: Rating::KEEP_1,
            looped: false,
            locked: false,
            last_played_at: None,
            last_curated_at: None,
        },
    )
    .expect("journal entry");

    let result =
        crate::sample_sources::db::file_ops_journal::insert_entry(&fixture.database, &entry);
    assert!(matches!(
        result,
        Err(SourceDbError::SourceLease(
            SourceLeaseError::DatabaseRootReplaced { .. }
        ))
    ));
    assert!(
        crate::sample_sources::db::file_ops_journal::list_entries(&fixture.database)
            .unwrap()
            .entries
            .is_empty()
    );
}

#[test]
fn curation_tag_assignment_resolves_identity_without_decoding_legacy_display_text() {
    let fixture = CurationFixture::new();
    // A legacy row can contain non-text bytes despite the TEXT column affinity. The bounded
    // curation path only needs its numeric identity and must not decode this unused payload.
    let legacy_display = vec![0xff_u8; 1024 * 1024];
    fixture
        .database
        .connection
        .execute(
            "INSERT INTO source_tags (normalized_text, display_label) VALUES ('loop', ?1)",
            [&legacy_display],
        )
        .expect("legacy tag");
    let receipt = fixture
        .commit(&[SourceCurationUpdate::AddTag {
            path: CurationFixture::path("one.wav"),
            label: String::from("loop"),
        }])
        .expect("assign existing identity without decoding its display label");
    assert_eq!(
        receipt.files()[0].playback_type,
        Some(SourcePlaybackType::Loop)
    );
    let (stored_length, assignments): (usize, usize) = fixture
        .database
        .connection
        .query_row(
            "SELECT length(display_label),
            (SELECT count(*) FROM wav_file_tags WHERE tag_id = source_tags.id)
         FROM source_tags WHERE normalized_text = 'loop'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("stored legacy identity");
    assert_eq!(stored_length, legacy_display.len());
    assert_eq!(assignments, 1);
}

#[test]
fn curation_existing_tag_ids_preserve_legacy_and_unrelated_assignments() {
    let mut fixture = CurationFixture::new();
    let one = Path::new("one.wav");
    let two = Path::new("two.wav");
    let legacy_display = vec![0xff_u8; 1024 * 1024];
    let legacy_normalized = String::from("legacy\0tag");
    fixture
        .database
        .connection
        .execute(
            "INSERT INTO source_tags (normalized_text, display_label) VALUES (?1, ?2)",
            rusqlite::params![&legacy_normalized, &legacy_display],
        )
        .expect("legacy tag");
    let legacy_id: i64 = fixture
        .database
        .connection
        .query_row(
            "SELECT id FROM source_tags WHERE normalized_text = ?1",
            [legacy_normalized.as_str()],
            |row| row.get(0),
        )
        .expect("legacy tag id");
    let loop_id = insert_tag(&fixture.database, "loop", "loop");
    let one_shot_id = insert_tag(&fixture.database, "oneshot", "one-shot");
    let unrelated_id = insert_tag(&fixture.database, "unrelated", "Unrelated");
    fixture
        .database
        .connection
        .execute(
            "INSERT INTO wav_file_tags (path, tag_id) VALUES
                (?1, ?3), (?1, ?4), (?2, ?4)",
            rusqlite::params![
                one.to_string_lossy(),
                two.to_string_lossy(),
                loop_id,
                unrelated_id
            ],
        )
        .expect("seed assignments");
    let mut seed = fixture.database.write_batch().expect("timestamp batch");
    seed.set_last_curated_at(one, 1).expect("seed timestamp");
    seed.commit_auxiliary_state().expect("seed timestamp");

    let assigned = fixture
        .commit(&[
            SourceCurationUpdate::AssignExistingTag {
                path: CurationFixture::path("one.wav"),
                tag_id: legacy_id,
            },
            SourceCurationUpdate::AssignExistingTag {
                path: CurationFixture::path("one.wav"),
                tag_id: one_shot_id,
            },
        ])
        .expect("assign existing source-local tag");
    assert_eq!(
        assigned.files()[0].playback_type,
        Some(SourcePlaybackType::OneShot)
    );
    assert!(assigned.files()[0].last_curated_at.unwrap() > 1);
    let assigned_ids = tag_ids_for_path(&fixture.database, one);
    assert_eq!(
        assigned_ids,
        BTreeSet::from([legacy_id, one_shot_id, unrelated_id])
    );
    assert_eq!(
        tag_ids_for_path(&fixture.database, two),
        BTreeSet::from([unrelated_id])
    );
    let (legacy_type, legacy_length): (String, usize) = fixture
        .database
        .connection
        .query_row(
            "SELECT typeof(display_label), length(CAST(display_label AS BLOB))
             FROM source_tags WHERE id = ?1",
            [legacy_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("legacy display payload");
    assert_eq!(legacy_type, "blob");
    assert_eq!(legacy_length, legacy_display.len());

    fixture.revision = assigned.source_revision();
    let removed = fixture
        .commit(&[SourceCurationUpdate::RemoveExistingTag {
            path: CurationFixture::path("one.wav"),
            tag_id: legacy_id,
        }])
        .expect("remove existing legacy source-local tag");
    assert_eq!(
        removed.files()[0].playback_type,
        Some(SourcePlaybackType::OneShot)
    );
    assert_eq!(
        tag_ids_for_path(&fixture.database, one),
        BTreeSet::from([one_shot_id, unrelated_id])
    );
    assert_eq!(
        tag_ids_for_path(&fixture.database, two),
        BTreeSet::from([unrelated_id])
    );
    fixture.revision = removed.source_revision();
    let removed_playback = fixture
        .commit(&[SourceCurationUpdate::RemoveExistingTag {
            path: CurationFixture::path("one.wav"),
            tag_id: one_shot_id,
        }])
        .expect("remove existing playback source-local tag");
    assert_eq!(removed_playback.files()[0].playback_type, None);
    assert_eq!(
        tag_ids_for_path(&fixture.database, one),
        BTreeSet::from([unrelated_id])
    );
    assert_eq!(
        fixture
            .database
            .connection
            .query_row("SELECT count(*) FROM source_tags", [], |row| row
                .get::<_, usize>(0))
            .expect("tag dictionary count"),
        4
    );
}

#[test]
fn curation_existing_tag_ids_reject_invalid_missing_and_stale_without_writes() {
    let fixture = CurationFixture::new();
    let path = Path::new("one.wav");
    let tag_id = insert_tag(&fixture.database, "keep", "Keep");
    let update = |tag_id| SourceCurationUpdate::AssignExistingTag {
        path: CurationFixture::path("one.wav"),
        tag_id,
    };

    for tag_id in [0, -1] {
        assert!(matches!(
            fixture.commit(&[update(tag_id)]),
            Err(SourceDbError::InvalidTagId { tag_id: actual }) if actual == tag_id
        ));
    }
    assert!(matches!(
        fixture.commit(&[update(tag_id + 1)]),
        Err(SourceDbError::TagIdNotFound { tag_id: actual }) if actual == tag_id + 1
    ));

    let initial_revision = fixture.database.get_revision().expect("revision");
    let initial_ids = tag_ids_for_path(&fixture.database, path);
    assert!(matches!(
        fixture.database.commit_curation_with_profile_guard(
            &fixture.profile_guard,
            fixture.revision - 1,
            &fixture.root_identity,
            &[update(tag_id)],
        ),
        Err(SourceDbError::DirectoryTruth(
            crate::sample_sources::db::SourceDirectoryTruthError::StaleRevision { .. }
        ))
    ));
    assert_eq!(
        fixture.database.get_revision().expect("revision"),
        initial_revision
    );
    assert_eq!(tag_ids_for_path(&fixture.database, path), initial_ids);

    let mut batch = fixture.database.write_batch().expect("invalid id batch");
    assert!(matches!(
        batch.assign_existing_tag_for_curation(path, 0),
        Err(SourceDbError::InvalidTagId { tag_id: 0 })
    ));
    drop(batch);
}

#[test]
fn source_writer_replays_durable_receipts_and_reopens_as_observation_only() {
    let fixture = CurationFixture::new_profile_bound();
    let source_id = SourceId::from_string("source-writer-test");
    let mut writer = SourceWriter::new(fixture.database, source_id.clone(), u64::MAX)
        .expect("source writer authority");
    let initial = writer.current_revision().expect("initial revision");
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(11)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(12)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(13)),
        initial.clone(),
        vec![SourceCurationUpdate::SetRating {
            path: CurationFixture::path("one.wav"),
            rating: Rating::KEEP_3,
        }],
    )
    .expect("request");
    let stale_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(41)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(42)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(43)),
        initial.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("stale request");
    let conflicting_request = SourceWriterRequest::new(
        request.identity().operation_id(),
        request.identity().participant_id(),
        request.identity().command_id(),
        initial.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("conflicting request");
    let first = writer
        .commit_curation(request.clone())
        .expect("first source command");
    assert_eq!(first.source_revision().revision(), initial.revision() + 1);
    let replay = writer
        .commit_curation(request.clone())
        .expect("exact duplicate replay");
    assert_eq!(first, replay);
    let state_after_first = source_writer_state_snapshot(writer.database_for_tests());
    assert_eq!(
        writer.current_revision().unwrap().revision(),
        first.source_revision().revision()
    );
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        state_after_first
    );
    let conflicting_before = source_writer_state_snapshot(writer.database_for_tests());
    assert!(matches!(
        writer.commit_curation(conflicting_request),
        Err(SourceWriterError::ConflictingCommand { .. })
    ));
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        conflicting_before
    );
    let stale_before = source_writer_state_snapshot(writer.database_for_tests());
    assert!(matches!(
        writer.commit_curation(stale_request),
        Err(SourceWriterError::StaleRevision {
            expected: 3,
            actual: 4
        })
    ));
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        stale_before
    );

    writer.inject_fault_for_tests("before_commit");
    let rollback_expected = writer.current_revision().unwrap();
    let rollback_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(21)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(22)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(23)),
        rollback_expected.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("rollback request");
    let rollback_before = source_writer_state_snapshot(writer.database_for_tests());
    assert!(matches!(
        writer.commit_curation(rollback_request.clone()),
        Err(SourceWriterError::Database(SourceDbError::Canceled))
    ));
    assert_eq!(
        writer.current_revision().unwrap().revision(),
        rollback_expected.revision()
    );
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        rollback_before
    );
    let rollback_receipt = writer
        .commit_curation(rollback_request)
        .expect("retry after confirmed rollback");
    assert_eq!(
        rollback_receipt.source_revision().revision(),
        rollback_expected.revision() + 1
    );

    let rollback_uncertain_expected = writer.current_revision().unwrap();
    let rollback_uncertain_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(24)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(25)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(26)),
        rollback_uncertain_expected.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: false,
        }],
    )
    .expect("rollback uncertainty request");
    let rollback_uncertain_before = source_writer_state_snapshot(writer.database_for_tests());
    writer.inject_fault_for_tests("rollback_uncertain");
    assert!(matches!(
        writer.commit_curation(rollback_uncertain_request.clone()),
        Err(SourceWriterError::Uncertain { .. })
    ));
    assert_eq!(
        writer.current_revision().unwrap().revision(),
        rollback_uncertain_expected.revision()
    );
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        rollback_uncertain_before
    );
    let rollback_uncertain_replay = writer
        .commit_curation(rollback_uncertain_request)
        .expect("retry after rollback uncertainty");
    assert_eq!(
        rollback_uncertain_replay.source_revision().revision(),
        rollback_uncertain_expected.revision() + 1
    );

    writer.inject_fault_for_tests("after_commit");
    let uncertain_expected = writer.current_revision().unwrap();
    let uncertain_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(31)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(32)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(33)),
        uncertain_expected,
        vec![SourceCurationUpdate::SetLastPlayedAt {
            path: CurationFixture::path("one.wav"),
            played_at: Some(9),
        }],
    )
    .expect("uncertain request");
    let uncertain_result = writer.commit_curation(uncertain_request.clone());
    match uncertain_result {
        Err(SourceWriterError::Uncertain {
            identity,
            canonical_request,
        }) => {
            assert_eq!(identity, uncertain_request.identity());
            assert_eq!(canonical_request, uncertain_request.canonical_bytes());
        }
        other => panic!("expected uncertain source writer result, got {other:?}"),
    }
    let uncertain_replay = writer
        .commit_curation(uncertain_request.clone())
        .expect("exact retry after uncertain acknowledgement");
    assert_eq!(uncertain_replay.identity(), uncertain_request.identity());
    assert_eq!(
        uncertain_replay.curation().files()[0].relative_path,
        PathBuf::from("one.wav")
    );
    assert_eq!(
        uncertain_replay.curation().files()[0].last_played_at,
        Some(9)
    );

    writer.close().expect("close source writer");
    let reopened_database = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        PathBuf::from("curation-profile-bound"),
    )
    .expect("reopen source database");
    assert_eq!(
        reopened_database
            .connection
            .query_row(
                "SELECT count(*) FROM source_participant_curation_receipts",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        4
    );
    let mut reopened = SourceWriter::new(reopened_database, source_id, u64::MAX - 1)
        .expect("reopened source writer");
    let historical = reopened
        .read_receipt_observation(request.identity())
        .expect("read historical source receipt")
        .expect("historical source receipt exists");
    assert_eq!(historical.identity(), request.identity());
    assert_eq!(
        historical.source_revision().lifecycle_generation(),
        u64::MAX
    );
    assert_eq!(
        historical.source_revision().revision(),
        first.source_revision().revision()
    );
    assert_eq!(
        historical.source_revision().root_identity(),
        first.source_revision().root_identity()
    );
    assert_eq!(
        historical.source_revision().root_binding_identity(),
        first.source_revision().root_binding_identity()
    );
    assert_eq!(
        historical.source_revision().database_identity(),
        first.source_revision().database_identity()
    );
    assert_eq!(
        historical.source_revision().lease_epoch(),
        first.source_revision().lease_epoch()
    );
    assert!(matches!(
        reopened.commit_curation(request.clone()),
        Err(SourceWriterError::SupersededLifecycle { .. })
    ));
    let stale_generation_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(51)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(52)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(53)),
        first.source_revision().clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: false,
        }],
    )
    .expect("stale generation request");
    assert!(matches!(
        reopened.commit_curation(stale_generation_request),
        Err(SourceWriterError::StaleFence)
    ));
    reopened.close().expect("close reopened source writer");
}

#[test]
fn source_owner_page_is_bounded_read_only_and_rejects_stale_continuation() {
    let fixture = CurationFixture::new_profile_bound();
    let source_id = SourceId::from_string("source-page-test");
    let mut writer = SourceWriter::new(fixture.database, source_id, 7).expect("source owner");
    let expected = writer.current_revision().expect("current revision");
    let changes_before = writer.database_for_tests().connection.total_changes();
    let expired = std::time::Instant::now() - std::time::Duration::from_millis(1);
    assert!(matches!(
        writer.read_browser_metadata_page_with_deadline(&expected, None, 1, expired),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    let first = writer
        .read_browser_metadata_page_with_deadline(
            &expected,
            None,
            1,
            std::time::Instant::now() + std::time::Duration::from_secs(2),
        )
        .expect("first page");
    let SourceSnapshotPage::FreshAsOf { revision, page } = first else {
        panic!("current source page must be fresh")
    };
    assert_eq!(revision, expected);
    assert_eq!(page.files.len(), 1);
    let cursor = page.next_cursor.expect("continuation");
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes_before
    );

    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::new(),
        crate::operation_journal::ParticipantId::new(),
        SourceCommandId::new(),
        expected.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("curation request");
    writer.commit_curation(request).expect("commit");
    assert!(matches!(
        writer.read_browser_metadata_page_with_deadline(&expected, Some(&cursor), 1, expired),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    let stale = writer
        .read_browser_metadata_page_with_deadline(
            &expected,
            Some(&cursor),
            1,
            std::time::Instant::now() + std::time::Duration::from_secs(2),
        )
        .expect("stale page result");
    let SourceSnapshotPage::Stale { current } = stale else {
        panic!("old continuation must be stale")
    };
    assert!(current.revision() > expected.revision());
    let second = writer
        .read_browser_metadata_page_with_deadline(
            &current,
            Some(&cursor),
            1,
            std::time::Instant::now() + std::time::Duration::from_secs(2),
        )
        .expect("new revision page");
    assert!(matches!(second, SourceSnapshotPage::FreshAsOf { .. }));
}

#[test]
fn snapshot_owner_times_out_while_worker_is_blocked_and_retains_writer() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("source-snapshot-worker-test"),
        7,
    )
    .expect("source owner");
    let expected = writer.current_revision().expect("current revision");
    let owner = SourceSnapshotOwner::start(writer).expect("snapshot worker");
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    owner.block_for_tests(entered_tx, release_rx);
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("worker entered blocking operation");

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
    assert!(matches!(
        owner.read_browser_metadata_page_with_deadline(&expected, None, 1, deadline),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    assert!(std::time::Instant::now() < deadline + std::time::Duration::from_millis(250));

    release_tx.send(()).expect("release worker");
    owner.wait_idle_for_tests();
    assert!(matches!(
        owner
            .read_browser_metadata_page(&expected, None, 1)
            .expect("fresh page after blocked operation"),
        SourceSnapshotPage::FreshAsOf { .. }
    ));
    let mut writer = owner
        .begin_retirement()
        .join()
        .expect("retired snapshot worker");
    assert_eq!(
        writer.current_revision().expect("retained authority"),
        expected
    );
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::new(),
        crate::operation_journal::ParticipantId::new(),
        SourceCommandId::new(),
        expected.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("curation request");
    writer.commit_curation(request).expect("commit");
    let owner = SourceSnapshotOwner::start(writer).expect("restarted snapshot worker");
    assert!(matches!(
        owner
            .read_browser_metadata_page(&expected, None, 1)
            .expect("stale page after curation"),
        SourceSnapshotPage::Stale { .. }
    ));
    let writer = owner
        .begin_retirement()
        .join()
        .expect("retired restarted worker");
    writer.close().expect("close source owner");
}

#[test]
fn source_writer_replays_at_full_retained_history_limit() {
    let fixture = CurationFixture::new_profile_bound();
    let source_id = SourceId::from_string("source-writer-history-limit");
    let mut writer =
        SourceWriter::new(fixture.database, source_id.clone(), 1).expect("source writer authority");
    let first_expected = writer.current_revision().expect("initial revision");
    let first_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(1_000)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(2_000)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(3_000)),
        first_expected.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("first history request");
    let first = writer
        .commit_curation(first_request.clone())
        .expect("first history commit");

    for index in 1..256usize {
        let expected = writer.current_revision().expect("history revision");
        let request = SourceWriterRequest::new(
            crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(
                1_000 + index as u128,
            )),
            crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(
                2_000 + index as u128,
            )),
            SourceCommandId::from_uuid(uuid::Uuid::from_u128(3_000 + index as u128)),
            expected.clone(),
            vec![SourceCurationUpdate::SetLocked {
                path: CurationFixture::path("one.wav"),
                locked: index % 2 == 0,
            }],
        )
        .expect("history request");
        let receipt = writer.commit_curation(request).expect("history commit");
        assert_eq!(
            receipt.source_revision().revision(),
            expected.revision() + 1
        );
    }

    let at_capacity = writer.current_revision().expect("full history revision");
    let at_capacity_state = source_writer_state_snapshot(writer.database_for_tests());
    let over_limit_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(9_001)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(9_002)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(9_003)),
        at_capacity.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("over-limit history request");
    assert!(matches!(
        writer.commit_curation(over_limit_request),
        Err(SourceWriterError::CapacityExceeded)
    ));
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        at_capacity_state
    );
    let replay = writer
        .commit_curation(first_request)
        .expect("replay at retained history limit");
    assert_eq!(replay, first);
    assert_eq!(
        writer.current_revision().unwrap().revision(),
        at_capacity.revision()
    );
    writer.close().expect("close history writer");

    let reopened = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        PathBuf::from("curation-profile-bound"),
    )
    .expect("reopen history source database");
    assert_eq!(
        reopened
            .connection
            .query_row(
                "SELECT count(*) FROM source_participant_curation_receipts",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        256
    );
}

#[test]
fn source_writer_rolls_back_oversized_receipt_after_curation_mutation() {
    let fixture = CurationFixture::new_profile_bound();
    let source_id = SourceId::from_string("source-writer-oversized-receipt");
    let mut writer =
        SourceWriter::new(fixture.database, source_id, 1).expect("source writer authority");
    let expected = writer.current_revision().expect("initial revision");
    let before = source_writer_state_snapshot(writer.database_for_tests());
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(5_001)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(5_002)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(5_003)),
        expected.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("oversized receipt request");

    writer.inject_fault_for_tests("oversized_receipt");
    assert!(matches!(
        writer.commit_curation(request.clone()),
        Err(SourceWriterError::CapacityExceeded)
    ));
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        before
    );

    let receipt = writer
        .commit_curation(request)
        .expect("retry after oversized receipt rollback");
    assert_eq!(
        receipt.source_revision().revision(),
        expected.revision() + 1
    );
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()).receipt_count,
        before.receipt_count + 1
    );
}

#[test]
fn source_writer_rejects_combined_receipt_budget_without_state_change() {
    let fixture = CurationFixture::new_profile_bound();
    let source_id = SourceId::from_string("source-writer-receipt-budget");
    let mut writer =
        SourceWriter::new(fixture.database, source_id, 1).expect("source writer authority");

    for index in 0..15u128 {
        let expected = writer.current_revision().expect("budget revision");
        let request = SourceWriterRequest::new(
            crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(10_000 + index)),
            crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(
                20_000 + index,
            )),
            SourceCommandId::from_uuid(uuid::Uuid::from_u128(30_000 + index)),
            expected,
            vec![SourceCurationUpdate::SetLocked {
                path: CurationFixture::path("one.wav"),
                locked: index % 2 == 0,
            }],
        )
        .expect("budget request");
        writer.inject_fault_for_tests("receipt_at_record_limit");
        writer
            .commit_curation(request)
            .expect("padded receipt within combined budget");
    }

    let expected = writer.current_revision().expect("budget boundary revision");
    let before = source_writer_state_snapshot(writer.database_for_tests());
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(40_001)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(40_002)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(40_003)),
        expected.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("budget boundary request");
    writer.inject_fault_for_tests("receipt_at_record_limit");
    assert!(matches!(
        writer.commit_curation(request),
        Err(SourceWriterError::CapacityExceeded)
    ));
    assert_eq!(
        source_writer_state_snapshot(writer.database_for_tests()),
        before
    );
}

#[test]
fn source_writer_rejects_oversized_request_before_transaction_mutation() {
    let fixture = CurationFixture::new_profile_bound();
    let source_id = SourceId::from_string("source-writer-oversized");
    let writer =
        SourceWriter::new(fixture.database, source_id, 1).expect("source writer authority");
    let expected = writer.current_revision().expect("initial revision");
    let long_path = CurationFixture::path(&format!("{}.wav", "x".repeat(10_000)));
    let updates = (0..SOURCE_CURATION_BATCH_LIMIT)
        .map(|_| SourceCurationUpdate::SetLocked {
            path: long_path.clone(),
            locked: true,
        })
        .collect();
    let oversized = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(4_001)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(4_002)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(4_003)),
        expected.clone(),
        updates,
    );
    assert!(matches!(
        oversized,
        Err(SourceWriterError::InvalidRequest(_))
    ));
    assert_eq!(writer.current_revision().unwrap(), expected);
    writer.close().expect("close oversized writer");
    let reopened = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        PathBuf::from("curation-profile-bound"),
    )
    .expect("reopen oversized source database");
    assert_eq!(
        reopened
            .connection
            .query_row(
                "SELECT count(*) FROM source_participant_curation_receipts",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
}

#[test]
fn source_writer_rejects_revision_beyond_sqlite_integer_before_mutation() {
    let fixture = CurationFixture::new_profile_bound();
    let max_revision = i64::MAX.to_string();
    fixture
        .database
        .connection
        .execute(
            "UPDATE metadata SET value = ?1 WHERE key = 'revision'",
            [&max_revision],
        )
        .expect("set max source revision");
    fixture
        .database
        .connection
        .execute(
            "UPDATE metadata SET value = ?1 WHERE key = ?2",
            rusqlite::params![
                &max_revision,
                crate::sample_sources::db::META_LAST_AUTHORITATIVE_AUDIT_REVISION
            ],
        )
        .expect("set max audit revision");
    fixture
        .database
        .connection
        .execute(
            "UPDATE source_directory_generations
             SET published_source_revision = ?1
             WHERE status = 'active'",
            [i64::MAX],
        )
        .expect("set max directory revision");

    let mut writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("source-writer-max-revision"),
        1,
    )
    .expect("source writer authority");
    let expected = writer.current_revision().expect("max revision");
    assert_eq!(expected.revision(), i64::MAX as u64);
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(5_001)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(5_002)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(5_003)),
        expected.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("max revision request");
    assert!(matches!(
        writer.commit_curation(request),
        Err(SourceWriterError::InvalidRequest(_))
    ));
    assert_eq!(writer.current_revision().unwrap(), expected);
    writer.close().expect("close max revision writer");
}

#[test]
fn source_writer_rejects_audit_root_mismatch_before_issuing_a_revision() {
    let fixture = CurationFixture::new_profile_bound();
    fixture
        .database
        .connection
        .execute(
            "UPDATE metadata SET value = ?1 WHERE key = ?2",
            rusqlite::params![
                "[99]",
                crate::sample_sources::db::META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY
            ],
        )
        .expect("tamper audit root");
    assert!(matches!(
        SourceWriter::new(
            fixture.database,
            SourceId::from_string("source-writer-root-mismatch"),
            1,
        ),
        Err(SourceWriterError::StaleFence)
    ));
}

#[test]
fn source_writer_rejects_tampered_receipt_lengths_before_loading_payloads() {
    let fixture = CurationFixture::new_profile_bound();
    let source_id = SourceId::from_string("source-writer-malformed");
    let mut writer =
        SourceWriter::new(fixture.database, source_id.clone(), 1).expect("source writer authority");
    let expected = writer.current_revision().expect("revision");
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(61)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(62)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(63)),
        expected,
        vec![SourceCurationUpdate::SetRating {
            path: CurationFixture::path("one.wav"),
            rating: Rating::KEEP_1,
        }],
    )
    .expect("request");
    writer.commit_curation(request.clone()).expect("commit");
    writer.close().expect("close source writer");

    let reopened_database = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        PathBuf::from("curation-profile-bound"),
    )
    .expect("reopen source database");
    reopened_database
        .connection
        .execute_batch("PRAGMA ignore_check_constraints = ON;")
        .expect("allow malformed fixture row");
    reopened_database
        .connection
        .execute(
            "UPDATE source_participant_curation_receipts
             SET request_len = request_len + 1",
            [],
        )
        .expect("tamper request length");
    let mut reopened =
        SourceWriter::new(reopened_database, source_id, 1).expect("reopen source writer");
    assert!(matches!(
        reopened.commit_curation(request),
        Err(SourceWriterError::MalformedReceipt)
    ));
    let expected = reopened.current_revision().expect("revision after tamper");
    let new_request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(64)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(65)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(66)),
        expected,
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .expect("new request after tamper");
    assert!(matches!(
        reopened.commit_curation(new_request),
        Err(SourceWriterError::MalformedReceipt)
    ));
    reopened.close().expect("close reopened source writer");
}

fn insert_tag(database: &SourceDatabase, normalized_text: &str, display_label: &str) -> i64 {
    database
        .connection
        .execute(
            "INSERT INTO source_tags (normalized_text, display_label) VALUES (?1, ?2)",
            rusqlite::params![normalized_text, display_label],
        )
        .expect("tag");
    database
        .connection
        .query_row(
            "SELECT id FROM source_tags WHERE normalized_text = ?1",
            [normalized_text],
            |row| row.get(0),
        )
        .expect("tag id")
}

fn tag_ids_for_path(database: &SourceDatabase, path: &Path) -> BTreeSet<i64> {
    let raw_path = path.to_string_lossy();
    let mut statement = database
        .connection
        .prepare("SELECT tag_id FROM wav_file_tags WHERE path = ?1 ORDER BY tag_id")
        .expect("tag ids");
    statement
        .query_map([raw_path.as_ref()], |row| row.get(0))
        .expect("tag ids query")
        .collect::<Result<BTreeSet<_>, _>>()
        .expect("tag ids rows")
}

#[test]
fn tag_assignment_selection_captures_real_complete_scope_without_writes() {
    use crate::sample_sources::SourceTagAssignmentMutation;
    let mut fixture = CurationFixture::new_profile_bound();
    let old = insert_tag(&fixture.database, "old tag", "Old Tag");
    let empty = insert_tag(&fixture.database, "empty", "Empty");
    let receipt = fixture
        .commit(&[
            SourceCurationUpdate::AssignExistingTag {
                path: CurationFixture::path("two.wav"),
                tag_id: old,
            },
            SourceCurationUpdate::AssignExistingTag {
                path: CurationFixture::path("one.wav"),
                tag_id: old,
            },
        ])
        .unwrap();
    fixture.revision = receipt.source_revision();
    let writer =
        SourceWriter::new(fixture.database, SourceId::from_string("tag-selection"), 7).unwrap();
    let revision = writer.current_revision().unwrap();
    let selected = writer
        .capture_tag_assignment_selection(
            &revision,
            "  OLD   tag ",
            SourceTagAssignmentMutation::Rename("  NEW  tag ".into()),
        )
        .unwrap();
    assert_eq!(selected.revision(), &revision);
    assert_eq!(selected.old_tag_id(), old);
    assert_eq!(selected.old_identity(), "old tag");
    assert_eq!(
        selected.mutation(),
        &SourceTagAssignmentMutation::Rename("new tag".into())
    );
    assert_eq!(
        selected.paths(),
        &[
            CurationFixture::path("one.wav"),
            CurationFixture::path("two.wav")
        ]
    );
    let empty_selection = writer
        .capture_tag_assignment_selection(&revision, "empty", SourceTagAssignmentMutation::Delete)
        .unwrap();
    assert_eq!(empty_selection.old_tag_id(), empty);
    assert!(empty_selection.paths().is_empty());
    assert_eq!(writer.current_revision().unwrap(), revision);
    assert_eq!(
        tag_ids_for_path(writer.database_for_tests(), Path::new("one.wav")),
        BTreeSet::from([old])
    );
    let owner = SourceSnapshotOwner::start(writer).unwrap();
    let repeated = owner
        .capture_tag_assignment_selection(
            &revision,
            "old tag",
            SourceTagAssignmentMutation::Rename("new tag".into()),
        )
        .unwrap();
    assert_eq!(repeated, selected);
    owner.begin_retirement().join().unwrap().close().unwrap();
}

#[test]
fn tag_assignment_selection_rejects_missing_classification_and_colliding_labels() {
    use crate::sample_sources::SourceTagAssignmentMutation as Mutation;
    let fixture = CurationFixture::new_profile_bound();
    insert_tag(&fixture.database, "old", "Old");
    insert_tag(&fixture.database, "other", "Other");
    insert_tag(&fixture.database, "loop", "Loop");
    let writer =
        SourceWriter::new(fixture.database, SourceId::from_string("tag-reject"), 7).unwrap();
    let revision = writer.current_revision().unwrap();
    for (old, mutation) in [
        ("absent", Mutation::Delete),
        ("loop", Mutation::Delete),
        ("old", Mutation::Rename(" OLD ".into())),
        ("old", Mutation::Rename(" OTHER ".into())),
        ("old", Mutation::Rename("one - shot".into())),
        ("old", Mutation::Rename("\0".into())),
    ] {
        assert!(
            writer
                .capture_tag_assignment_selection(&revision, old, mutation)
                .is_err()
        );
        assert_eq!(writer.current_revision().unwrap(), revision);
    }
    assert!(
        writer
            .capture_tag_assignment_selection(&revision, &"x".repeat(1025), Mutation::Delete)
            .is_err()
    );
    assert!(
        writer
            .capture_tag_assignment_selection(&revision, "old", Mutation::Rename("x".repeat(1025)))
            .is_err()
    );
    let count: usize = writer
        .database_for_tests()
        .connection
        .query_row("SELECT COUNT(*) FROM source_tags", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 3);
}

#[test]
fn tag_assignment_selection_rejects_overflow_without_truncation() {
    use crate::sample_sources::SourceTagAssignmentMutation as Mutation;
    let fixture = CurationFixture::new_profile_bound();
    let tag = insert_tag(&fixture.database, "old", "Old");
    // Fixture construction populates the real schema before the retained-owner read.
    for n in 0..129 {
        let path = format!("{n:03}.wav");
        fixture
            .database
            .connection
            .execute(
                "INSERT INTO wav_files(path,file_size,modified_ns,extension) VALUES (?1,1,1,'wav')",
                [&path],
            )
            .unwrap();
        fixture
            .database
            .connection
            .execute(
                "INSERT INTO wav_file_tags(path,tag_id) VALUES (?1,?2)",
                rusqlite::params![path, tag],
            )
            .unwrap();
    }
    let writer =
        SourceWriter::new(fixture.database, SourceId::from_string("tag-overflow"), 7).unwrap();
    let revision = writer.current_revision().unwrap();
    assert!(matches!(
        writer.capture_tag_assignment_selection(&revision, "old", Mutation::Delete),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    writer
        .database_for_tests()
        .connection
        .execute("DELETE FROM wav_file_tags WHERE path='128.wav'", [])
        .unwrap();
    assert_eq!(
        writer
            .capture_tag_assignment_selection(&revision, "old", Mutation::Delete)
            .unwrap()
            .paths()
            .len(),
        128
    );
    assert!(matches!(
        writer.capture_tag_assignment_selection(&revision, "old", Mutation::Rename("new".into())),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    writer
        .database_for_tests()
        .connection
        .execute("DELETE FROM wav_file_tags WHERE path >= '064.wav'", [])
        .unwrap();
    assert_eq!(
        writer
            .capture_tag_assignment_selection(&revision, "old", Mutation::Rename("new".into()))
            .unwrap()
            .paths()
            .len(),
        64
    );
    assert_eq!(writer.current_revision().unwrap(), revision);
}

#[test]
fn tag_assignment_selection_rejects_long_missing_unsupported_and_malformed_members() {
    use crate::sample_sources::SourceTagAssignmentMutation as Mutation;
    let fixture = CurationFixture::new_profile_bound();
    let tag = insert_tag(&fixture.database, "old", "Old");
    let writer =
        SourceWriter::new(fixture.database, SourceId::from_string("tag-members"), 7).unwrap();
    let revision = writer.current_revision().unwrap();
    for (path, extension, missing) in [
        (format!("{}.wav", "x".repeat(1024)), "wav", 0),
        ("absent.wav".into(), "wav", 1),
        ("unsupported.mp3".into(), "mp3", 0),
        ("../escape.wav".into(), "wav", 0),
        ("folder/./alias.wav".into(), "wav", 0),
    ] {
        let conn = &writer.database_for_tests().connection;
        conn.execute("INSERT INTO wav_files(path,file_size,modified_ns,extension,missing) VALUES (?1,1,1,?2,?3)", rusqlite::params![path,extension,missing]).unwrap();
        conn.execute(
            "INSERT INTO wav_file_tags(path,tag_id) VALUES (?1,?2)",
            rusqlite::params![path, tag],
        )
        .unwrap();
        assert!(
            writer
                .capture_tag_assignment_selection(&revision, "old", Mutation::Delete)
                .is_err()
        );
        assert_eq!(writer.current_revision().unwrap(), revision);
        assert_eq!(
            tag_ids_for_path(writer.database_for_tests(), Path::new(&path)),
            BTreeSet::from([tag])
        );
        conn.execute("DELETE FROM wav_file_tags", []).unwrap();
    }
}

#[test]
fn tag_assignment_selection_rejects_stale_revision_and_lifecycle_and_expired_deadline() {
    use crate::sample_sources::SourceTagAssignmentMutation as Mutation;
    let fixture = CurationFixture::new_profile_bound();
    insert_tag(&fixture.database, "old", "Old");
    let mut writer =
        SourceWriter::new(fixture.database, SourceId::from_string("tag-fence"), 7).unwrap();
    let initial = writer.current_revision().unwrap();
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::new(),
        crate::operation_journal::ParticipantId::new(),
        SourceCommandId::new(),
        initial.clone(),
        vec![SourceCurationUpdate::SetRating {
            path: CurationFixture::path("one.wav"),
            rating: Rating::KEEP_3,
        }],
    )
    .unwrap();
    writer.commit_curation(request).unwrap();
    assert!(matches!(
        writer.capture_tag_assignment_selection(&initial, "old", Mutation::Delete),
        Err(SourceWriterError::StaleFence)
    ));
    let current = writer.current_revision().unwrap();
    assert!(matches!(
        writer.capture_tag_assignment_selection_with_deadline(
            &current,
            "old",
            Mutation::Delete,
            std::time::Instant::now()
        ),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    let replacement = SourceWriter::new(
        writer.into_catalog_database(),
        SourceId::from_string("tag-fence"),
        8,
    )
    .unwrap();
    assert!(matches!(
        replacement.capture_tag_assignment_selection(&current, "old", Mutation::Delete),
        Err(SourceWriterError::StaleFence)
    ));
}

#[test]
fn tag_assignment_selection_worker_timeout_retains_custody_and_read_is_discarded() {
    use crate::sample_sources::SourceTagAssignmentMutation as Mutation;
    let fixture = CurationFixture::new_profile_bound();
    insert_tag(&fixture.database, "old", "Old");
    let writer =
        SourceWriter::new(fixture.database, SourceId::from_string("tag-worker"), 7).unwrap();
    let revision = writer.current_revision().unwrap();
    let owner = SourceSnapshotOwner::start(writer).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    owner.block_for_tests(entered_tx, release_rx);
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(
        owner.capture_tag_assignment_selection_with_deadline(
            &revision,
            "old",
            Mutation::Delete,
            started + std::time::Duration::from_millis(30)
        ),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    release_tx.send(()).unwrap();
    owner.wait_idle_for_tests();
    assert!(
        owner
            .capture_tag_assignment_selection(&revision, "old", Mutation::Delete)
            .unwrap()
            .paths()
            .is_empty()
    );
    let writer = owner.begin_retirement().join().unwrap();
    assert_eq!(writer.current_revision().unwrap(), revision);
}

#[cfg(unix)]
#[test]
fn tag_assignment_selection_rejects_replaced_media_root_and_expired_lease() {
    use crate::sample_sources::SourceTagAssignmentMutation as Mutation;
    let fixture = CurationFixture::new_profile_bound();
    insert_tag(&fixture.database, "old", "Old");
    let writer = SourceWriter::new(fixture.database, SourceId::from_string("tag-root"), 7).unwrap();
    let revision = writer.current_revision().unwrap();
    let before = source_writer_state_snapshot(writer.catalog_database());
    let root = fixture._source.path();
    let displaced = root.with_extension("tag-selection-displaced");
    std::fs::rename(root, &displaced).unwrap();
    std::fs::create_dir(root).unwrap();
    assert!(
        writer
            .capture_tag_assignment_selection(&revision, "old", Mutation::Delete)
            .is_err()
    );
    std::fs::remove_dir(root).unwrap();
    std::fs::rename(&displaced, root).unwrap();
    assert_eq!(
        source_writer_state_snapshot(writer.catalog_database()),
        before
    );
    writer
        .database_for_tests()
        .connection
        .execute("UPDATE source_lease SET expires_at=acquired_at", [])
        .unwrap();
    assert!(matches!(
        writer.capture_tag_assignment_selection(&revision, "old", Mutation::Delete),
        Err(SourceWriterError::Database(SourceDbError::SourceLease(
            SourceLeaseError::Expired { .. }
        )))
    ));
}
mod feature_artifacts;

#[test]
fn bounded_metadata_corruption_rejects_actual_writer_revision_and_tag_selection_without_edits() {
    use crate::sample_sources::SourceTagAssignmentMutation;
    let fixture = CurationFixture::new_profile_bound();
    insert_tag(&fixture.database, "old", "Old");
    let writer =
        SourceWriter::new(fixture.database, SourceId::from_string("bounded-audit"), 7).unwrap();
    let revision = writer.current_revision().unwrap();
    for (key, invalid) in [
        (
            crate::sample_sources::db::META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY,
            " ".repeat(24577),
        ),
        (
            crate::sample_sources::db::META_LAST_AUTHORITATIVE_AUDIT_REVISION,
            "0".repeat(21),
        ),
        ("revision", "0".repeat(21)),
    ] {
        let db = writer.database_for_tests();
        let original = db.get_metadata(key).unwrap().unwrap();
        db.connection
            .execute(
                "UPDATE metadata SET value=?1 WHERE key=?2",
                rusqlite::params![invalid, key],
            )
            .unwrap();
        let before = db.connection.total_changes();
        assert!(writer.current_revision().is_err());
        assert!(
            writer
                .capture_tag_assignment_selection(
                    &revision,
                    "old",
                    SourceTagAssignmentMutation::Delete
                )
                .is_err()
        );
        assert_eq!(db.connection.total_changes(), before);
        db.connection
            .execute(
                "UPDATE metadata SET value=?1 WHERE key=?2",
                rusqlite::params![original, key],
            )
            .unwrap();
        assert_eq!(writer.current_revision().unwrap(), revision);
    }
}
