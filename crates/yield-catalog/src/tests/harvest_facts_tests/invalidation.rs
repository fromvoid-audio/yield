// Actual owner mutation and lifecycle tests; secondary source seeds are metadata fixtures.
fn authored_secondary(fixture: &Fixture, global: &GlobalLibraryWriter) {
    use yield_library::operation_journal::{OperationId, StoredRelativePath};
    use yield_library::sample_sources::library::{
        HarvestAuthoredRequest, HarvestAuthoredSnapshotRequest,
    };
    let root = tempfile::tempdir().unwrap();
    let db = SourceDatabase::open_for_catalog_with_profile_guard(
        root.path(),
        &fixture.guard,
        Path::new("harvest-secondary"),
    )
    .unwrap();
    let mut batch = db.write_batch().unwrap();
    batch
        .upsert_file_with_hash(Path::new("other.wav"), 10, 1, &"a".repeat(64))
        .unwrap();
    batch
        .set_file_identity(Path::new("other.wav"), Some("metadata-fixture-object"))
        .unwrap();
    batch.commit().unwrap();
    db.begin_source_directory_truth_generation(1, 0).unwrap();
    let writer =
        SourceWriter::new_for_scan(db, SourceId::from_string("harvest-secondary"), 1).unwrap();
    let revision = writer.current_scan_fence().unwrap();
    writer
        .catalog_database()
        .write_batch()
        .unwrap()
        .commit_manifest_audit_with_staged_directory_truth(
            1,
            revision.revision(),
            10,
            revision.root_identity().clone(),
            None,
        )
        .unwrap();
    let observation = writer
        .observe_file_identity(StoredRelativePath::new("other.wav").unwrap())
        .unwrap()
        .into_content_identity()
        .unwrap();
    let snapshot = global
        .harvest_authored_snapshot(
            HarvestAuthoredSnapshotRequest::new(&fixture.guard, observation.clone()).unwrap(),
        )
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    let request = HarvestAuthoredRequest::new(
        &fixture.guard,
        OperationId::new(),
        snapshot,
        observation,
        HarvestState::Touched,
    )
    .unwrap();
    global
        .set_harvest_authored_state(request)
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
}
#[test]
fn harvest_catalog_actual_global_write_invalidates_queued_and_cached_facts() {
    let (fixture, mut global, service, snapshot) = setup();
    submit(
        &service,
        request(&global, &snapshot, 20, &["one.wav"]),
        HarvestFactsCancellation::new(),
    );
    let queued = delivery(&service, 20);
    let cached = match &queued.event {
        CatalogEvent::HarvestFactsCompleted {
            result: Ok(facts), ..
        } => facts.clone(),
        event => panic!("{event:?}"),
    };
    assert!(cached.is_live());
    authored_secondary(&fixture, &global);
    assert!(!cached.is_live());
    assert!(queued.into_event_if_live().is_none());
    submit(
        &service,
        request(&global, &snapshot, 21, &["one.wav"]),
        HarvestFactsCancellation::new(),
    );
    let newer = facts(delivery(&service, 21));
    assert!(newer.is_live());
    global.shutdown();
    assert!(!newer.is_live());
    service.shutdown();
}
#[test]
fn harvest_catalog_source_retirement_immediately_invalidates_queued_context() {
    let (_fixture, mut global, service, snapshot) = setup();
    submit(
        &service,
        request(&global, &snapshot, 22, &["one.wav"]),
        HarvestFactsCancellation::new(),
    );
    let queued = delivery(&service, 22);
    let cached = match &queued.event {
        CatalogEvent::HarvestFactsCompleted {
            result: Ok(facts), ..
        } => facts.clone(),
        event => panic!("{event:?}"),
    };
    service
        .submit(CatalogCommand::Remove {
            source_id: snapshot.source_id.clone(),
            lifecycle_generation: 2,
        })
        .unwrap();
    assert!(!cached.is_live());
    assert!(queued.into_event_if_live().is_none());
    service.shutdown();
    global.shutdown();
}
#[test]
fn harvest_catalog_projection_context_and_foreign_profile_are_never_accepted() {
    let (fixture, mut global, service, snapshot) = setup();
    submit(
        &service,
        request(&global, &snapshot, 23, &["one.wav"]),
        HarvestFactsCancellation::new(),
    );
    let result = facts(delivery(&service, 23));
    let compare = |source: &SourceId, life, directory, revision| {
        result.matches_projection(
            &global.acquisition_id(),
            source,
            life,
            &snapshot.root_identity,
            directory,
            revision,
        )
    };
    assert!(!compare(
        &SourceId::from_string("replacement"),
        1,
        snapshot.directory_generation,
        snapshot.committed_revision
    ));
    assert!(!compare(
        &snapshot.source_id,
        2,
        snapshot.directory_generation,
        snapshot.committed_revision
    ));
    assert!(!compare(
        &snapshot.source_id,
        1,
        snapshot.directory_generation + 1,
        snapshot.committed_revision
    ));
    assert!(!compare(
        &snapshot.source_id,
        1,
        snapshot.directory_generation,
        snapshot.committed_revision + 1
    ));
    let _other_profile = PersistenceProfileGuard::named("harvest-foreign");
    let other = WritableProfileGuard::acquire_current().unwrap();
    assert!(matches!(
        CatalogService::start_with_profile_owners(&other, None, global.harvest_facts_client()),
        Err(CatalogStartError::HarvestOwnerMismatch)
    ));
    fixture.guard.validate_current().unwrap();
    service.shutdown();
    global.shutdown();
}
