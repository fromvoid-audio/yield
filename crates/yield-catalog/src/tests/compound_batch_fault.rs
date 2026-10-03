use super::*;
use crate::test_support::{CompoundBatchFaultError as FaultError, MAX_COMPOUND_FAULT_SOURCE_BYTES};
use yield_library::sample_sources::Rating;

fn setup(name: &str) -> (Fixture, CatalogService, SourceId, CatalogSnapshot) {
    let fixture = Fixture::new(name);
    for name in ["one.wav", "two.wav", "three.wav"] {
        write_test_wav(&fixture.root.path().join(name), 16000, 4);
    }
    let id = SourceId::from_string(name);
    let service = CatalogService::start(&fixture.guard).unwrap();
    let snapshot = register_and_audit(&service, fixture.root.path(), &id);
    (fixture, service, id, snapshot)
}
fn batches() -> Vec<Vec<SourceCurationUpdate>> {
    ["one.wav", "two.wav", "three.wav"]
        .into_iter()
        .map(|name| {
            vec![SourceCurationUpdate::SetRating {
                path: RootRelativePath::try_from_path(PathBuf::from(name)).unwrap(),
                rating: Rating::KEEP_1,
            }]
        })
        .collect()
}
fn run(
    service: &CatalogService,
    id: &SourceId,
    snapshot: &CatalogSnapshot,
    request: u64,
    revision: u64,
) -> SourceCurationOperationOutcome {
    service
        .submit(curation_operation(
            id,
            snapshot.lifecycle_generation,
            &snapshot.root_identity,
            request,
            SourceCurationOperationFence::ExactRevision(revision),
            batches(),
        ))
        .unwrap();
    receive_curation_operation(service, request)
}
fn check_credits(service: &CatalogService) {
    let state = service.admission.lock().unwrap();
    assert_eq!(state.curation_payload_bytes, 0);
    assert_eq!(state.curation_update_count, 0);
}
fn prefix(index: usize) {
    let (fixture, service, id, snapshot) = setup(&format!("compound-fault-{index}"));
    service
        .arm_compound_batch_failure(&id, 1, 100, index)
        .unwrap();
    let outcome = run(&service, &id, &snapshot, 100, snapshot.committed_revision);
    assert!(!outcome.is_success());
    assert_eq!(outcome.failure().unwrap().batch_index(), index);
    assert!(
        matches!(outcome.failure().unwrap().error(), CatalogError::Database(message) if message == "test-support: exact compound batch failed before SQL")
    );
    assert_eq!(outcome.committed_batches().len(), index);
    assert_eq!(
        outcome.initial_revision(),
        Some(snapshot.committed_revision)
    );
    assert_eq!(
        outcome.final_revision(),
        Some(snapshot.committed_revision + index as u64)
    );
    for (batch, receipt) in outcome.committed_batches().iter().enumerate() {
        assert_eq!(
            receipt.source_revision(),
            snapshot.committed_revision + batch as u64 + 1
        );
        assert_eq!(receipt.files().len(), 1);
        assert_eq!(receipt.files()[0].rating, Rating::KEEP_1);
    }
    // Actual read-only SQLite readback, not a normalized receipt reducer.
    let path = fixture
        .guard
        .profile_root()
        .join(catalog_profile_child(&id))
        .join(".yield.db");
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    for (batch, name) in ["one.wav", "two.wav", "three.wav"].into_iter().enumerate() {
        let rating: i64 = db
            .query_row("SELECT tag FROM wav_files WHERE path=?1", [name], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            rating,
            i64::from(batch < index),
            "exact persisted prefix {name}"
        );
    }
    let observed = SourceDatabase::open_for_ui_read_with_database_root(
        fixture.root.path(),
        fixture
            .guard
            .profile_root()
            .join(catalog_profile_child(&id)),
    )
    .unwrap();
    assert_eq!(
        observed.get_revision().unwrap(),
        outcome.final_revision().unwrap()
    );
    check_credits(&service);
    assert!(!service.cancel_compound_batch_failure());
    let success = run(
        &service,
        &id,
        &snapshot,
        101,
        outcome.final_revision().unwrap(),
    );
    assert!(success.is_success());
    assert_eq!(success.committed_batches().len(), 3);
    assert_eq!(
        success.final_revision(),
        Some(snapshot.committed_revision + index as u64 + 3)
    );
    check_credits(&service);
    service.shutdown();
}
#[test]
fn compound_fault_first_batch_has_no_effect() {
    prefix(0);
}
#[test]
fn compound_fault_middle_batch_preserves_exact_prefix() {
    prefix(1);
}
#[test]
fn compound_fault_last_batch_preserves_exact_prefix() {
    prefix(2);
}

#[test]
fn compound_fault_request_and_service_are_exact_and_one_shot() {
    let (fixture, service, id, snapshot) = setup("compound-fault-context");
    let other = CatalogService::start(&fixture.guard).unwrap();
    service.arm_compound_batch_failure(&id, 1, 200, 1).unwrap();
    assert_eq!(
        service.arm_compound_batch_failure(&id, 1, 201, 0),
        Err(FaultError::Occupied)
    );
    assert_eq!(
        other.arm_compound_batch_failure(&id, 1, 200, 1),
        Err(FaultError::StaleLifecycle)
    );
    assert!(!other.cancel_compound_batch_failure());
    let normal = run(&service, &id, &snapshot, 199, snapshot.committed_revision);
    assert!(normal.is_success());
    assert_eq!(
        service.arm_compound_batch_failure(&id, 1, 201, 0),
        Err(FaultError::Occupied)
    );
    let failure = run(
        &service,
        &id,
        &snapshot,
        200,
        normal.final_revision().unwrap(),
    );
    assert_eq!(failure.failure().unwrap().batch_index(), 1);
    assert_eq!(failure.committed_batches().len(), 1);
    let success = run(
        &service,
        &id,
        &snapshot,
        201,
        failure.final_revision().unwrap(),
    );
    assert!(success.is_success());
    check_credits(&service);
    other.shutdown();
    service.shutdown();
}
#[test]
fn compound_fault_wrong_source_does_not_consume_configuration() {
    let (_fixture, service, id, snapshot) = setup("compound-fault-source");
    let root = tempfile::tempdir().unwrap();
    for name in ["one.wav", "two.wav", "three.wav"] {
        write_test_wav(&root.path().join(name), 16000, 4);
    }
    let other = SourceId::from_string("compound-fault-other");
    let other_snapshot = register_and_audit(&service, root.path(), &other);
    service.arm_compound_batch_failure(&id, 1, 200, 1).unwrap();
    assert!(
        run(
            &service,
            &other,
            &other_snapshot,
            200,
            other_snapshot.committed_revision
        )
        .is_success()
    );
    assert_eq!(
        service.arm_compound_batch_failure(&id, 1, 201, 0),
        Err(FaultError::Occupied)
    );
    assert_eq!(
        run(&service, &id, &snapshot, 200, snapshot.committed_revision)
            .failure()
            .unwrap()
            .batch_index(),
        1
    );
    service.shutdown();
}
#[test]
fn compound_fault_original_fences_still_run_before_fault() {
    let (_fixture, service, id, snapshot) = setup("compound-fault-fences");
    service.arm_compound_batch_failure(&id, 1, 200, 0).unwrap();
    let failure = run(
        &service,
        &id,
        &snapshot,
        200,
        snapshot.committed_revision - 1,
    );
    assert_eq!(failure.committed_batches().len(), 0);
    assert!(matches!(
        failure.failure().unwrap().error(),
        CatalogError::StaleRevision { .. }
    ));
    assert!(!service.cancel_compound_batch_failure());
    assert!(run(&service, &id, &snapshot, 201, snapshot.committed_revision).is_success());
    check_credits(&service);
    service.shutdown();
}
#[test]
fn compound_fault_bounds_and_explicit_cancellation_release_only_one_slot() {
    let (_fixture, service, id, snapshot) = setup("compound-fault-bounds");
    for (generation, request, batch) in
        [(0, 200, 0), (1, 0, 0), (1, 200, 512), (1, 200, usize::MAX)]
    {
        assert_eq!(
            service.arm_compound_batch_failure(&id, generation, request, batch),
            Err(FaultError::InvalidContext)
        );
    }
    for source in [
        SourceId::from_string(""),
        SourceId::from_string("with\0nul"),
        SourceId::from_string("x".repeat(MAX_COMPOUND_FAULT_SOURCE_BYTES + 1)),
    ] {
        assert_eq!(
            service.arm_compound_batch_failure(&source, 1, 200, 0),
            Err(FaultError::InvalidContext)
        );
    }
    assert_eq!(
        service.arm_compound_batch_failure(&id, 2, 200, 0),
        Err(FaultError::StaleLifecycle)
    );
    service.arm_compound_batch_failure(&id, 1, 200, 0).unwrap();
    assert!(service.cancel_compound_batch_failure());
    assert!(!service.cancel_compound_batch_failure());
    assert!(run(&service, &id, &snapshot, 200, snapshot.committed_revision).is_success());
    check_credits(&service);
    service.shutdown();
    assert_eq!(
        service.arm_compound_batch_failure(&id, 1, 201, 0),
        Err(FaultError::Closed)
    );
}
#[test]
fn compound_fault_lifecycle_remove_and_shutdown_release_retention() {
    let (fixture, service, id, _snapshot) = setup("compound-fault-retire");
    service.arm_compound_batch_failure(&id, 1, 200, 0).unwrap();
    service
        .submit(CatalogCommand::Register {
            source_id: id.clone(),
            canonical_root: fixture.root.path().to_path_buf(),
            lifecycle_generation: 2,
        })
        .unwrap();
    assert!(!service.cancel_compound_batch_failure());
    receive_until(&service, |e| {
        matches!(
            e,
            CatalogEvent::Registered {
                lifecycle_generation: 2,
                ..
            }
        )
    });
    assert_eq!(
        service.arm_compound_batch_failure(&id, 1, 201, 0),
        Err(FaultError::StaleLifecycle)
    );
    service.arm_compound_batch_failure(&id, 2, 201, 0).unwrap();
    service
        .submit(CatalogCommand::Remove {
            source_id: id.clone(),
            lifecycle_generation: 2,
        })
        .unwrap();
    assert!(!service.cancel_compound_batch_failure());
    assert_eq!(
        service.arm_compound_batch_failure(&id, 2, 202, 0),
        Err(FaultError::StaleLifecycle)
    );
    service.shutdown();
    assert_eq!(
        service.arm_compound_batch_failure(&id, 2, 203, 0),
        Err(FaultError::Closed)
    );
}

#[test]
fn compound_fault_wrong_lifecycle_admission_preserves_slot() {
    let (_fixture, service, id, snapshot) = setup("compound-fault-wrong-generation");
    service.arm_compound_batch_failure(&id, 1, 200, 0).unwrap();
    assert!(matches!(
        service.submit(curation_operation(
            &id,
            2,
            &snapshot.root_identity,
            199,
            SourceCurationOperationFence::ExactRevision(snapshot.committed_revision),
            batches()
        )),
        Err(CatalogSubmitError::InvalidCurationRequest(
            CatalogCurationRequestError::StaleLifecycleGeneration { .. }
        ))
    ));
    assert_eq!(
        service.arm_compound_batch_failure(&id, 1, 201, 0),
        Err(FaultError::Occupied)
    );
    assert_eq!(
        run(&service, &id, &snapshot, 200, snapshot.committed_revision)
            .failure()
            .unwrap()
            .batch_index(),
        0
    );
    check_credits(&service);
    service.shutdown();
}
#[test]
fn compound_fault_shutdown_clears_pending_slot_and_terminal_short_plan_clears_it() {
    let (_fixture, service, id, snapshot) = setup("compound-fault-shutdown");
    service.arm_compound_batch_failure(&id, 1, 200, 3).unwrap();
    assert!(run(&service, &id, &snapshot, 200, snapshot.committed_revision).is_success());
    assert!(!service.cancel_compound_batch_failure());
    service.arm_compound_batch_failure(&id, 1, 201, 0).unwrap();
    service.shutdown();
    assert!(!service.cancel_compound_batch_failure());
    assert_eq!(
        service.arm_compound_batch_failure(&id, 1, 201, 0),
        Err(FaultError::Closed)
    );
}
