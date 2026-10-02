use crate::operation_journal::recovery_status::{JournalRetryBlockage, JournalUserStatus};

#[test]
fn action_status_persisted_retry_blockages_and_late_observations_survive_reopen() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let mut snapshots = Vec::new();
    for (class, blockage) in [
        (
            RetryErrorClass::TransientContention,
            JournalRetryBlockage::Contention,
        ),
        (
            RetryErrorClass::TransientAvailability,
            JournalRetryBlockage::Availability,
        ),
        (
            RetryErrorClass::CapacityUnavailable,
            JournalRetryBlockage::Capacity,
        ),
    ] {
        let (id, intent, universal, args, plan) = v5_copy_inputs();
        let inserted = store
            .persist_copy_intent(id, intent, universal, args, plan, 50)
            .unwrap();
        let before = store
            .scan_recovery_status_page(None)
            .unwrap()
            .entries()
            .iter()
            .find(|entry| entry.operation_id() == Some(id))
            .unwrap()
            .clone();
        assert_eq!(before.user_status(), JournalUserStatus::InProgress);
        let retry = RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("private-retry-lease").unwrap(),
            53,
            class,
            "bounded_reason",
            Some("private/path/context".to_owned()),
        )
        .unwrap();
        store
            .record_retry_pending(id, inserted.sequence(), retry)
            .unwrap();
        let after = store
            .scan_recovery_status_page(None)
            .unwrap()
            .entries()
            .iter()
            .find(|entry| entry.operation_id() == Some(id))
            .unwrap()
            .clone();
        assert_eq!(
            after.user_status(),
            JournalUserStatus::RetryPending {
                partial: false,
                blockage: Some(blockage)
            }
        );
        assert!(after.is_newer_observation_than(&before));
        assert!(!before.is_newer_observation_than(&after));
        assert!(!after.is_newer_observation_than(&after));
        for older in &snapshots {
            assert!(
                !after.is_newer_observation_than(older),
                "another operation replaced current status"
            );
        }
        let debug = format!("{after:?}");
        assert!(!debug.contains("private-retry-lease"));
        assert!(!debug.contains("private/path/context"));
        assert!(!debug.contains("bounded_reason"));
        snapshots.push(after);
    }
    let page = store.scan_recovery_status_page(None).unwrap();
    drop(store);
    let reopened = OperationJournalStore::open(&fixture.owner).unwrap();
    let after = reopened.scan_recovery_status_page(None).unwrap();
    assert_eq!(page, after);
    assert_eq!(after.entries(), snapshots.as_slice());
}
