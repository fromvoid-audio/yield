use std::path::Path;

use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
use crate::sample_sources::db::{DurableReplayUncertaintyState, META_SOURCE_WATCHER_CHECKPOINT};
use crate::sample_sources::reconciliation::{
    BackendStreamIdentity, BootstrapReplayEvidence, CaptureBoundary, RawEventKind, RawObservation,
    RawObservationLimits, RawObservationProvenance, RawObservedPath, RawPathRole,
    ReconciliationAdmissionLimits, ReconciliationAdmissionOwner, ReconciliationAdmissionSupervisor,
    SyntheticObservationBatch, UncertaintyReason, WatcherGeneration,
};
use crate::sample_sources::{SourceDatabase, SourceId, SourceWriter, SourceWriterError};

#[test]
fn complete_audit_and_certain_live_capture_bootstrap_one_durable_prior() {
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let temporary_root = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
    let base = tempfile::tempdir_in(temporary_root).expect("config base");
    let _config_base = ConfigBaseGuard::set(base.path().to_path_buf());
    let _profile = PersistenceProfileGuard::named("watcher-bootstrap-test");
    let profile = WritableProfileGuard::acquire_current().expect("profile guard");
    let source = tempfile::tempdir().expect("source root");
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source.path(),
        &profile,
        Path::new("watcher-bootstrap-source"),
    )
    .expect("profile-bound source");
    let source_id = SourceId::from_string("watcher-bootstrap-source");
    let writer =
        SourceWriter::new_for_scan(database, source_id.clone(), 7).expect("retained source writer");
    let initial = writer.current_scan_fence().expect("initial fence");
    let mut audit_batch = writer
        .catalog_database()
        .write_batch()
        .expect("audit batch");
    audit_batch
        .complete_manifest_audit(1)
        .expect("complete audit");
    let (audit, _) = audit_batch
        .commit_with_manifest_snapshot_and_audit(initial.root_identity().clone(), None)
        .expect("committed audit token");
    let expected = writer.current_scan_fence().expect("post-audit fence");
    assert_eq!(audit.committed_source_revision(), expected.revision());
    let stream =
        BackendStreamIdentity::from_fsevents_volume(17, [2; 16]).expect("device and volume UUID");
    let evidence = BootstrapReplayEvidence::from_certain_empty_capture(
        source_id.clone(),
        expected.root_identity().clone(),
        stream,
        7,
        WatcherGeneration::new(5),
        100,
        100,
        true,
        false,
        0,
    )
    .expect("certain empty live capture");
    let first = writer
        .commit_bootstrap_replay_checkpoint(&expected, &audit, &evidence)
        .expect("first durable prior");
    assert_eq!(first.acknowledged_sequence(), 100);
    let second = writer
        .commit_bootstrap_replay_checkpoint(&expected, &audit, &evidence)
        .expect("duplicate bootstrap is idempotent");
    assert_eq!(second, first);
    let stored = writer
        .catalog_database()
        .get_metadata(META_SOURCE_WATCHER_CHECKPOINT)
        .expect("checkpoint metadata")
        .expect("checkpoint stored");
    assert!(stored.contains("\"cause\":\"bootstrap_audit\""));

    let newer = BootstrapReplayEvidence::from_certain_empty_capture(
        source_id,
        expected.root_identity().clone(),
        BackendStreamIdentity::from_fsevents_volume(17, [2; 16]).unwrap(),
        7,
        WatcherGeneration::new(5),
        100,
        101,
        true,
        false,
        0,
    )
    .unwrap();
    assert!(matches!(
        writer.commit_bootstrap_replay_checkpoint(&expected, &audit, &newer),
        Err(SourceWriterError::ReplayBootstrapMismatch)
    ));
    let readback = writer
        .catalog_database()
        .read_durable_replay_prior(
            expected.source_id(),
            expected.root_identity(),
            WatcherGeneration::new(7),
            WatcherGeneration::new(5),
        )
        .expect("durable read")
        .expect("valid bootstrap prior");
    assert_eq!(readback, first);

    let stale_lifecycle = BootstrapReplayEvidence::from_certain_empty_capture(
        expected.source_id().clone(),
        expected.root_identity().clone(),
        BackendStreamIdentity::from_fsevents_volume(17, [2; 16]).unwrap(),
        8,
        WatcherGeneration::new(5),
        100,
        100,
        true,
        false,
        0,
    )
    .unwrap();
    assert!(matches!(
        writer.commit_bootstrap_replay_checkpoint(&expected, &audit, &stale_lifecycle),
        Err(SourceWriterError::ReplayBootstrapMismatch)
    ));

    let mut next_audit = writer.catalog_database().write_batch().unwrap();
    next_audit.complete_manifest_audit(2).unwrap();
    next_audit
        .commit_with_manifest_snapshot_and_audit(expected.root_identity().clone(), None)
        .unwrap();
    assert!(matches!(
        writer.commit_bootstrap_replay_checkpoint(&expected, &audit, &evidence),
        Err(SourceWriterError::StaleRevision { .. })
    ));
    drop(writer);
    let reopened = SourceDatabase::open_for_catalog_with_profile_guard(
        source.path(),
        &profile,
        Path::new("watcher-bootstrap-source"),
    )
    .expect("reopen after source-writer retirement");
    let durable_after_restart = reopened
        .read_durable_replay_prior(
            expected.source_id(),
            expected.root_identity(),
            WatcherGeneration::new(7),
            WatcherGeneration::new(5),
        )
        .expect("read after restart")
        .expect("bootstrap remains durable");
    assert_eq!(durable_after_restart, first);
}

#[test]
fn retained_uncertainty_blocks_replay_through_restart_until_complete_audit() {
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let temporary_root = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
    let base = tempfile::tempdir_in(temporary_root).expect("config base");
    let _config_base = ConfigBaseGuard::set(base.path().to_path_buf());
    let _profile = PersistenceProfileGuard::named("watcher-uncertainty-test");
    let profile = WritableProfileGuard::acquire_current().expect("profile guard");
    let source = tempfile::tempdir().expect("source root");
    let child = Path::new("watcher-uncertainty-source");
    let database =
        SourceDatabase::open_for_catalog_with_profile_guard(source.path(), &profile, child)
            .expect("profile-bound source");
    let source_id = SourceId::from_string("watcher-uncertainty-source");
    let writer = SourceWriter::new_for_scan(database, source_id.clone(), 7).expect("source writer");
    let initial = writer.current_scan_fence().expect("initial fence");
    let mut batch = writer
        .catalog_database()
        .write_batch()
        .expect("audit batch");
    batch
        .upsert_file(Path::new("last-good.wav"), 44, 4)
        .expect("last-good source fact");
    batch.complete_manifest_audit(1).expect("complete audit");
    let (audit, _) = batch
        .commit_with_manifest_snapshot_and_audit(initial.root_identity().clone(), None)
        .expect("audited source revision");
    let fence = writer.current_scan_fence().expect("committed fence");
    let stream = BackendStreamIdentity::from_fsevents_volume(17, [2; 16]).expect("stream");
    let evidence = BootstrapReplayEvidence::from_certain_empty_capture(
        source_id.clone(),
        fence.root_identity().clone(),
        stream.clone(),
        7,
        WatcherGeneration::new(1),
        100,
        100,
        true,
        false,
        0,
    )
    .expect("certain capture");
    let prior = writer
        .commit_bootstrap_replay_checkpoint(&fence, &audit, &evidence)
        .expect("bootstrap prior");

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
        .begin_source(source_id.clone(), fence.root_identity().clone())
        .expect("source lane");
    let boundary = CaptureBoundary::try_new(101, Some(101), Some(101)).expect("capture boundary");
    let observed = SyntheticObservationBatch::new(
        RawObservationProvenance::new(
            source_id.clone(),
            Some(fence.root_identity().clone()),
            Some(stream.clone()),
            lane.generation(),
            boundary,
        ),
        vec![RawObservation::new(
            RawEventKind::Modify,
            vec![RawObservedPath::new(
                "kick.wav".into(),
                RawPathRole::Subject,
            )],
        )],
        lane_limits,
    );
    owner
        .admit_replay_with_durable_prior(observed, Some(&prior), false)
        .expect("gapped replay observation");
    let marker = owner
        .supervisor()
        .retained_uncertainties()
        .first()
        .expect("retained marker")
        .clone();
    let retained = writer
        .retain_watcher_replay_uncertainty(&fence, &marker)
        .expect("durable marker");
    assert_eq!(retained.source_revision(), fence.revision());
    assert_eq!(retained.backend_stream_identity(), Some(&stream));
    assert_eq!(retained.watcher_generation(), Some(lane.generation()));
    assert_eq!(retained.capture_boundary(), Some(boundary));
    assert!(
        retained
            .reasons()
            .contains(&UncertaintyReason::ReplayContinuityGap)
    );
    assert_eq!(
        writer
            .retain_watcher_replay_uncertainty(&fence, &marker)
            .unwrap(),
        retained
    );
    let wrong_root = SyntheticObservationBatch::new(
        RawObservationProvenance::new(
            source_id.clone(),
            Some(
                crate::sample_sources::reconciliation::RootIdentity::from_bytes(
                    b"replaced-root".to_vec(),
                ),
            ),
            Some(stream.clone()),
            lane.generation(),
            CaptureBoundary::try_new(102, Some(102), Some(102)).expect("later capture"),
        ),
        vec![RawObservation::new(
            RawEventKind::Modify,
            vec![RawObservedPath::new(
                "kick.wav".into(),
                RawPathRole::Subject,
            )],
        )],
        lane_limits,
    );
    let invalid = owner
        .admit_replay_with_durable_prior(wrong_root, None, false)
        .expect("wrong root remains unproven");
    let widened = owner
        .durable_audit_marker_for_replay(&invalid)
        .expect("current-root source audit marker");
    assert_eq!(widened.root_identity(), Some(fence.root_identity()));
    let retained = writer
        .retain_watcher_replay_uncertainty(&fence, &widened)
        .expect("persist widened audit disposition");
    assert!(retained.reasons().contains(&UncertaintyReason::WrongRoot));
    assert!(
        retained
            .reasons()
            .contains(&UncertaintyReason::ReplayContinuityGap)
    );
    assert!(
        matches!(
            writer.commit_bootstrap_replay_checkpoint(&fence, &audit, &evidence),
            Err(SourceWriterError::ReplayBootstrapMismatch)
        ),
        "an audit completed before the marker cannot clear it"
    );
    assert!(
        writer
            .catalog_database()
            .read_durable_replay_prior(
                &source_id,
                fence.root_identity(),
                WatcherGeneration::new(7),
                lane.generation(),
            )
            .expect("blocked read")
            .is_none()
    );
    let mut incomplete = writer
        .catalog_database()
        .write_batch()
        .expect("incomplete scan batch");
    incomplete
        .upsert_file(Path::new("partial.wav"), 10, 1)
        .expect("safe partial fact");
    writer
        .commit_scan_manifest_batch(&fence, incomplete)
        .expect("incomplete scan may retain a partial delta");
    assert_eq!(
        writer
            .catalog_database()
            .read_durable_replay_uncertainty()
            .expect("uncertainty after incomplete scan"),
        DurableReplayUncertaintyState::Retained(retained.clone())
    );
    assert!(
        writer
            .catalog_database()
            .entry_for_path(Path::new("last-good.wav"))
            .expect("read last-good row after incomplete scan")
            .is_some()
    );
    let checkpoint = writer
        .catalog_database()
        .get_metadata(META_SOURCE_WATCHER_CHECKPOINT)
        .expect("read checkpoint after incomplete scan")
        .expect("prior checkpoint remains stored");
    let checkpoint: serde_json::Value =
        serde_json::from_str(&checkpoint).expect("checkpoint JSON after incomplete scan");
    assert_eq!(checkpoint["event_id"], 100);
    assert!(
        writer
            .catalog_database()
            .read_durable_replay_prior(
                &source_id,
                fence.root_identity(),
                WatcherGeneration::new(7),
                lane.generation(),
            )
            .expect("incomplete scan blocked read")
            .is_none()
    );
    drop(writer);

    let reopened =
        SourceDatabase::open_for_catalog_with_profile_guard(source.path(), &profile, child)
            .expect("reopen source");
    assert_eq!(
        reopened
            .read_durable_replay_uncertainty()
            .expect("reopen uncertainty"),
        DurableReplayUncertaintyState::Retained(retained),
    );
    assert!(
        reopened
            .entry_for_path(Path::new("last-good.wav"))
            .expect("read last-good row after recovery restart")
            .is_some()
    );
    let checkpoint = reopened
        .get_metadata(META_SOURCE_WATCHER_CHECKPOINT)
        .expect("read checkpoint after restart")
        .expect("prior checkpoint remains stored after restart");
    let checkpoint: serde_json::Value =
        serde_json::from_str(&checkpoint).expect("checkpoint JSON after restart");
    assert_eq!(checkpoint["event_id"], 100);
    assert!(
        reopened
            .read_durable_replay_prior(
                &source_id,
                fence.root_identity(),
                WatcherGeneration::new(7),
                lane.generation(),
            )
            .expect("reopen blocked read")
            .is_none()
    );
    let writer =
        SourceWriter::new_for_scan(reopened, source_id.clone(), 7).expect("reopened writer");
    let before_audit = writer.current_scan_fence().expect("pre-audit fence");
    let mut batch = writer
        .catalog_database()
        .write_batch()
        .expect("recovery audit batch");
    batch
        .complete_manifest_audit(2)
        .expect("complete recovery audit");
    let (audit, _) = batch
        .commit_with_manifest_snapshot_and_audit(before_audit.root_identity().clone(), None)
        .expect("committed recovery audit");
    let recovered = writer.current_scan_fence().expect("recovery fence");
    let later = writer
        .retain_watcher_replay_uncertainty(&recovered, &marker)
        .expect("later uncertainty after the audit");
    assert_eq!(later.source_revision(), recovered.revision());
    let new_evidence = BootstrapReplayEvidence::from_certain_empty_capture(
        source_id.clone(),
        recovered.root_identity().clone(),
        stream.clone(),
        7,
        lane.generation(),
        200,
        200,
        true,
        false,
        0,
    )
    .expect("certain recovery capture");
    assert!(
        matches!(
            writer.commit_bootstrap_replay_checkpoint(&recovered, &audit, &new_evidence),
            Err(SourceWriterError::ReplayBootstrapMismatch)
        ),
        "later uncertainty cannot be cleared by the earlier audit"
    );
    let mut batch = writer
        .catalog_database()
        .write_batch()
        .expect("latest audit batch");
    batch
        .complete_manifest_audit(3)
        .expect("complete latest audit");
    let (latest_audit, _) = batch
        .commit_with_manifest_snapshot_and_audit(recovered.root_identity().clone(), None)
        .expect("committed latest audit");
    let latest = writer.current_scan_fence().expect("latest fence");
    let latest_evidence = BootstrapReplayEvidence::from_certain_empty_capture(
        source_id.clone(),
        latest.root_identity().clone(),
        stream,
        7,
        lane.generation(),
        300,
        300,
        true,
        false,
        0,
    )
    .expect("certain latest capture");
    let next_prior = writer
        .commit_bootstrap_replay_checkpoint(&latest, &latest_audit, &latest_evidence)
        .expect("recovered replay authority");
    assert_ne!(next_prior, prior);
    assert_eq!(next_prior.acknowledged_sequence(), 300);
    assert_eq!(
        writer
            .catalog_database()
            .read_durable_replay_uncertainty()
            .expect("cleared marker"),
        DurableReplayUncertaintyState::Absent,
    );
}

#[test]
fn capacity_exhaustion_marker_survives_failed_write_retry_and_restart() {
    let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
    let temporary_root = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
    let base = tempfile::tempdir_in(temporary_root).expect("config base");
    let _config_base = ConfigBaseGuard::set(base.path().to_path_buf());
    let _profile = PersistenceProfileGuard::named("watcher-capacity-test");
    let profile = WritableProfileGuard::acquire_current().expect("profile guard");
    let source = tempfile::tempdir().expect("source root");
    let child = Path::new("watcher-capacity-source");
    let database =
        SourceDatabase::open_for_catalog_with_profile_guard(source.path(), &profile, child)
            .expect("profile-bound source");
    let source_id = SourceId::from_string("watcher-capacity-source");
    let writer = SourceWriter::new_for_scan(database, source_id.clone(), 7).expect("source writer");
    let initial = writer.current_scan_fence().expect("initial fence");
    let mut batch = writer
        .catalog_database()
        .write_batch()
        .expect("audit batch");
    batch.complete_manifest_audit(1).expect("complete audit");
    let (audit, _) = batch
        .commit_with_manifest_snapshot_and_audit(initial.root_identity().clone(), None)
        .expect("committed audit");
    let fence = writer.current_scan_fence().expect("audited fence");
    let stream = BackendStreamIdentity::from_fsevents_volume(17, [3; 16]).expect("stream");
    let bootstrap = BootstrapReplayEvidence::from_certain_empty_capture(
        source_id.clone(),
        fence.root_identity().clone(),
        stream.clone(),
        7,
        WatcherGeneration::new(1),
        100,
        100,
        true,
        false,
        0,
    )
    .expect("certain capture");
    let prior = writer
        .commit_bootstrap_replay_checkpoint(&fence, &audit, &bootstrap)
        .expect("durable prior");
    assert_eq!(prior.acknowledged_sequence(), 100);

    let lane_limits = RawObservationLimits::new(8, 4096, 4096).expect("lane limits");
    let limits = ReconciliationAdmissionLimits::new(
        1,
        lane_limits,
        RawObservationLimits::new(32, 16384, 16384).expect("global limits"),
        1,
        8,
        1,
    )
    .expect("admission limits");
    let mut owner =
        ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(limits));
    let observation = |source_id: SourceId,
                       root_identity: crate::sample_sources::reconciliation::RootIdentity,
                       generation: WatcherGeneration| {
        SyntheticObservationBatch::new(
            RawObservationProvenance::new(
                source_id,
                Some(root_identity),
                Some(stream.clone()),
                generation,
                CaptureBoundary::try_new(102, Some(102), Some(102)).expect("boundary"),
            ),
            vec![RawObservation::new(
                RawEventKind::Create,
                vec![RawObservedPath::new(
                    "capacity.wav".into(),
                    RawPathRole::Subject,
                )],
            )],
            lane_limits,
        )
    };
    // Fill both ordinary and emergency uncertainty retention with unknown lanes.
    for index in 0..2 {
        owner
            .admit_replay_with_durable_prior(
                observation(
                    SourceId::from_string(format!("unknown-capacity-{index}")),
                    fence.root_identity().clone(),
                    WatcherGeneration::new(1),
                ),
                None,
                true,
            )
            .expect("bounded unknown-lane admission");
    }
    let lane = owner
        .begin_source(source_id.clone(), fence.root_identity().clone())
        .expect("current lane");
    let admission = owner
        .admit_replay_with_durable_prior(
            observation(
                source_id.clone(),
                fence.root_identity().clone(),
                lane.generation(),
            ),
            Some(&prior),
            true,
        )
        .expect("capacity admission");
    assert!(matches!(
        admission.admission().outcome(),
        crate::sample_sources::reconciliation::AdmissionOutcome::UncertaintyCapacityExhausted(_)
    ));
    assert!(admission.audit_request().is_some());
    let marker = owner
        .durable_audit_marker_for_replay(&admission)
        .expect("current-lane source-wide marker despite exhausted retention");
    assert_eq!(marker.source_id(), Some(&source_id));
    assert_eq!(marker.root_identity(), Some(fence.root_identity()));

    // Fill the SQLite database under a fixed page limit so marker persistence sees a real
    // SQLITE_FULL result. The filler is test-only metadata, outside the source writer protocol.
    let connection = &writer.catalog_database().connection;
    let page_count: i64 = connection
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .expect("current page count");
    connection
        .execute_batch(&format!("PRAGMA max_page_count = {page_count}"))
        .expect("bound database capacity");
    let mut saw_full = false;
    for index in 0..4096 {
        match connection.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
            rusqlite::params![format!("_capacity_fill_{index:04}"), "x".repeat(100)],
        ) {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DiskFull =>
            {
                saw_full = true;
                break;
            }
            Err(error) => panic!("unexpected capacity filler error: {error}"),
        }
    }
    assert!(saw_full, "fixture must exhaust SQLite pages");
    let failed_marker_write = writer
        .retain_watcher_replay_uncertainty(&fence, &marker)
        .expect_err("low-space marker write cannot claim durable acknowledgement");
    assert!(
        matches!(
            failed_marker_write,
            SourceWriterError::Database(crate::sample_sources::db::SourceDbError::Sql(
                rusqlite::Error::SqliteFailure(error, _)
            )) if error.code == rusqlite::ErrorCode::DiskFull
        ),
        "expected SQLITE_FULL, got {failed_marker_write:?}"
    );
    assert_eq!(
        writer
            .catalog_database()
            .read_durable_replay_uncertainty()
            .unwrap(),
        DurableReplayUncertaintyState::Absent
    );
    connection
        .execute_batch("PRAGMA max_page_count = 1000000")
        .expect("restore database capacity");
    connection
        .execute("DELETE FROM metadata WHERE key LIKE '_capacity_fill_%'", [])
        .expect("release test-only capacity filler");
    let retained = writer
        .retain_watcher_replay_uncertainty(&fence, &marker)
        .expect("retry retained marker");
    assert_eq!(
        writer
            .retain_watcher_replay_uncertainty(&fence, &marker)
            .unwrap(),
        retained,
        "duplicate retry does not create a new barrier"
    );
    assert!(
        writer
            .commit_bootstrap_replay_checkpoint(&fence, &audit, &bootstrap)
            .is_err(),
        "pre-marker audit cannot retire uncertainty"
    );
    let mut incomplete = writer
        .catalog_database()
        .write_batch()
        .expect("incomplete scan batch");
    incomplete
        .upsert_file(Path::new("partial.wav"), 10, 1)
        .expect("partial scan fact");
    writer
        .commit_scan_manifest_batch(&fence, incomplete)
        .expect("partial scan can publish safe facts");
    assert_eq!(
        writer
            .catalog_database()
            .read_durable_replay_uncertainty()
            .unwrap(),
        DurableReplayUncertaintyState::Retained(retained.clone()),
        "partial scan cannot claim a complete recovery audit"
    );
    drop(writer);

    let reopened =
        SourceDatabase::open_for_catalog_with_profile_guard(source.path(), &profile, child)
            .expect("reopen source");
    assert_eq!(
        reopened.read_durable_replay_uncertainty().unwrap(),
        DurableReplayUncertaintyState::Retained(retained)
    );
    assert!(
        reopened
            .read_durable_replay_prior(
                &source_id,
                fence.root_identity(),
                WatcherGeneration::new(7),
                lane.generation(),
            )
            .expect("reopened replay read")
            .is_none()
    );
    let writer =
        SourceWriter::new_for_scan(reopened, source_id.clone(), 7).expect("reopened writer");
    let mut batch = writer
        .catalog_database()
        .write_batch()
        .expect("recovery batch");
    batch
        .complete_manifest_audit(2)
        .expect("complete recovery audit");
    let (recovery_audit, _) = batch
        .commit_with_manifest_snapshot_and_audit(fence.root_identity().clone(), None)
        .expect("committed recovery audit");
    let recovered = writer.current_scan_fence().expect("recovered fence");
    let recovery_capture = BootstrapReplayEvidence::from_certain_empty_capture(
        source_id,
        recovered.root_identity().clone(),
        stream,
        7,
        lane.generation(),
        200,
        200,
        true,
        false,
        0,
    )
    .expect("certain recovery capture");
    writer
        .commit_bootstrap_replay_checkpoint(&recovered, &recovery_audit, &recovery_capture)
        .expect("only later complete audit restores replay authority");
    assert_eq!(
        writer
            .catalog_database()
            .read_durable_replay_uncertainty()
            .unwrap(),
        DurableReplayUncertaintyState::Absent
    );
}

#[test]
fn bootstrap_evidence_rejects_changed_paths_and_unknown_coverage() {
    let source = SourceId::from_string("source");
    let root = crate::sample_sources::reconciliation::RootIdentity::from_bytes(b"root".to_vec());
    let stream = BackendStreamIdentity::from_fsevents_volume(17, [2; 16]).unwrap();
    let make = |complete, uncertain, changed, through| {
        BootstrapReplayEvidence::from_certain_empty_capture(
            source.clone(),
            root.clone(),
            stream.clone(),
            7,
            WatcherGeneration::new(5),
            100,
            through,
            complete,
            uncertain,
            changed,
        )
    };
    assert!(make(true, false, 0, 100).is_some());
    assert!(make(false, false, 0, 100).is_none());
    assert!(make(true, true, 0, 100).is_none());
    assert!(make(true, false, 1, 101).is_none());
    assert!(make(true, false, 0, 99).is_none());
}
