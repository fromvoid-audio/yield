use super::*;

#[test]
fn original_service_acquisition_matches_retained_guard_and_survives_client_clones_and_close() {
    let (fixture, mut owner) = open_owner("journal-service-original-acquisition");
    let client = owner.client();
    let original = Uuid::parse_str(fixture.owner.owner_identity()).unwrap();
    assert_eq!(client.original_profile_acquisition(), Some(original));
    let retained = client.clone();
    assert!(owner.shutdown());
    assert_eq!(retained.original_profile_acquisition(), Some(original));
    assert!(matches!(
        retained.status(),
        OperationJournalOwnerStatus::Closed
    ));
    assert!(matches!(
        retained.get(OperationId::new()),
        Err(OperationJournalOwnerQueueError::Closed)
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn refused_actual_production_bootstrap_publishes_no_acquisition() {
    let fixture = fixture("journal-service-refused-production-bootstrap");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let mut owner = OperationJournalOwner::start(store).unwrap();
    let client = owner.client();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match client.status() {
            OperationJournalOwnerStatus::Initializing => {
                assert!(std::time::Instant::now() < deadline);
                thread::yield_now();
            }
            OperationJournalOwnerStatus::Unavailable { .. } => break,
            status => panic!("expected unsupported Linux protected-reserve bootstrap: {status:?}"),
        }
    }
    assert_eq!(client.original_profile_acquisition(), None);
    assert!(matches!(
        client.get(OperationId::new()),
        Err(OperationJournalOwnerQueueError::Unavailable { .. })
    ));
    owner.shutdown();
    assert_eq!(client.original_profile_acquisition(), None);
}
