use std::path::Path;

use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
use crate::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT;
use crate::sample_sources::reconciliation::{
    BackendStreamIdentity, BootstrapReplayEvidence, WatcherGeneration,
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
