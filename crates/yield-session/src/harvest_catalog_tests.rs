// Actual portable metadata session/catalog/global owners; no qualified reserve or physical effect.
#[test]
fn harvest_session_actual_startup_forwards_facts_and_fences_shutdown_and_restart() {
    let temp = TempDir::new().unwrap();
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).unwrap());
    let profile = profile_root(&temp);
    let source_root = root(&temp, "harvest-source");
    fs::create_dir_all(source_root.as_path()).unwrap();
    write_test_wav(&source_root.as_path().join("one.wav"));
    let source_id = SourceId::from_string("session-harvest");
    let mut service =
        SourceRegistryService::start_portable_registry_for_test(profile.clone()).unwrap();
    wait_for_vocabulary_available(&service);
    wait_for_catalog_ready(&service);
    service
        .submit_catalog(CatalogCommand::Register {
            source_id: source_id.clone(),
            canonical_root: source_root.as_path().to_path_buf(),
            lifecycle_generation: 1,
        })
        .unwrap();
    wait_for_catalog_event_with_budget(
        &service,
        |event| matches!(event, CatalogEvent::Registered { .. }),
        Duration::from_secs(5),
        "Harvest source registration",
    );
    service
        .submit_catalog(CatalogCommand::RequestAudit {
            source_id: source_id.clone(),
            lifecycle_generation: 1,
        })
        .unwrap();
    let snapshot = match wait_for_catalog_event_with_budget(
        &service,
        |event| matches!(event, CatalogEvent::AuditCommitted { .. }),
        Duration::from_secs(5),
        "Harvest manifest audit",
    ) {
        CatalogEvent::AuditCommitted { snapshot } => snapshot,
        event => panic!("{event:?}"),
    };
    let acquisition = service.vocabulary_acquisition_id().unwrap();
    let request = SourceHarvestFactsRequest::new(
        acquisition.clone(),
        source_id.clone(),
        1,
        snapshot.root_identity.clone(),
        snapshot.directory_generation,
        snapshot.committed_revision,
        55,
        &["one.wav".to_owned()],
    )
    .unwrap();
    service
        .submit_catalog(CatalogCommand::RequestHarvestFacts {
            request: request.clone(),
            cancellation: HarvestFactsCancellation::new(),
        })
        .unwrap();
    let facts = match wait_for_catalog_event_with_budget(
        &service,
        |event| matches!(event, CatalogEvent::HarvestFactsCompleted { .. }),
        Duration::from_secs(5),
        "Harvest facts",
    ) {
        CatalogEvent::HarvestFactsCompleted {
            request: captured,
            result: Ok(facts),
            ..
        } => {
            assert_eq!(captured, request);
            facts
        }
        event => panic!("{event:?}"),
    };
    assert_eq!(facts.entries().len(), 1);
    assert_eq!(facts.entries()[0].state(), HarvestState::New);
    assert!(!facts.entries()[0].is_tracked());
    assert_eq!(facts.entries()[0].immediate_derivative_count(), 0);
    assert!(facts.matches_projection(
        &acquisition,
        &source_id,
        1,
        &snapshot.root_identity,
        snapshot.directory_generation,
        snapshot.committed_revision
    ));
    service.shutdown().unwrap();
    assert!(!facts.is_live());
    let mut reopened = SourceRegistryService::start_portable_registry_for_test(profile).unwrap();
    wait_for_vocabulary_available(&reopened);
    let newer = reopened.vocabulary_acquisition_id().unwrap();
    assert_ne!(newer, acquisition);
    assert!(!facts.matches_projection(
        &newer,
        &source_id,
        1,
        &snapshot.root_identity,
        snapshot.directory_generation,
        snapshot.committed_revision
    ));
    reopened.shutdown().unwrap();
}
