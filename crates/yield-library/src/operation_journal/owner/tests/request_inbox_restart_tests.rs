use super::super::request_inbox::*;
use super::*;

#[test]
fn inbox_owner_restart_under_same_actual_profile_acquisition_never_confirms_old_delivery() {
    let (fixture, mut owner) = open_owner("journal-inbox-same-acquisition-restart");
    let client = owner.client();
    let acquisition = client.original_profile_acquisition().unwrap();
    let mut old = JournalRequestInbox::<Option<StoredOperationRecord>>::new(client).unwrap();
    let reservation = old.reserve(Uuid::new_v4(), 1, [0; 32], 1).unwrap();
    let request = reservation.client().get(OperationId::new()).unwrap();
    reservation
        .accept(request)
        .unwrap_or_else(|(e, _)| panic!("{e}"));
    assert!(owner.shutdown());
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let mut restarted = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
    wait_until_available(&restarted);
    assert_eq!(
        restarted.client().original_profile_acquisition(),
        Some(acquisition)
    );
    let current =
        JournalRequestInbox::<Option<StoredOperationRecord>>::new(restarted.client()).unwrap();
    assert_ne!(old.installation(), current.installation());
    let mut delivered = false;
    assert_eq!(
        old.poll(current.installation(), |completion| {
            assert!(!completion.matches_installation);
            assert!(matches!(
                completion.delivery,
                JournalRequestDelivery::Result(Ok(None))
            ));
            delivered = true;
        }),
        1
    );
    assert!(delivered);
    drop(old);
    drop(current);
    assert!(restarted.shutdown());
}
