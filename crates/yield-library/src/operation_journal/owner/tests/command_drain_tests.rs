#[cfg(test)]
mod integration {
    use super::super::*;

    #[test]
    fn command_drain_returns_before_blocked_worker_and_preserves_real_store_custody() {
        let fixture = fixture("journal-command-drain-blocked");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let (owner, ready, release) =
            OperationJournalOwner::start_with_command_gate_for_test(store);
        wait_until_available(&owner);
        let client = owner.client();
        let operation_id = OperationId::new();
        let mut accepted = client.persist_intent(intent_record(operation_id)).unwrap();
        ready.wait();
        let (tx, rx) = mpsc::sync_channel(1);
        let control = thread::spawn(move || {
            assert!(owner.begin_command_drain());
            tx.send(owner).unwrap();
        });
        let returned = rx.recv_timeout(Duration::from_secs(2));
        if returned.is_err() {
            // Release even a broken implementation before reporting a blocking join regression.
            release.wait();
            control.join().unwrap();
            panic!("command drain waited on the accepted worker command");
        }
        let mut owner = returned.unwrap();
        control.join().unwrap();
        assert_eq!(owner.status(), OperationJournalOwnerStatus::Closing);
        assert!(!owner.begin_command_drain());
        assert!(!owner.worker.as_ref().unwrap().is_finished());
        assert!(OperationJournalStore::open(&fixture.owner).is_err());
        let input = *owner.shared.input_bytes.lock().unwrap();
        let results = *owner.shared.results.lock().unwrap();
        for _ in 0..4 {
            assert!(matches!(
                client.get(OperationId::new()),
                Err(OperationJournalOwnerQueueError::Closed)
            ));
            assert!(matches!(
                client.persist_intent(intent_record(OperationId::new())),
                Err(OperationJournalOwnerQueueError::Closed)
            ));
        }
        assert_eq!(*owner.shared.input_bytes.lock().unwrap(), input);
        assert_eq!(*owner.shared.results.lock().unwrap(), results);
        release.wait();
        accepted
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        // Even an empty accepted queue does not disconnect the original worker or release SQLite.
        assert_eq!(owner.status(), OperationJournalOwnerStatus::Closing);
        assert!(!owner.worker.as_ref().unwrap().is_finished());
        assert!(OperationJournalStore::open(&fixture.owner).is_err());
        assert!(owner.shutdown());
        assert!(!owner.begin_command_drain());
        assert!(!owner.shutdown());
        let reopened = OperationJournalStore::open(&fixture.owner).unwrap();
        assert!(reopened.get(operation_id).unwrap().is_some());
    }

    #[test]
    fn command_drain_idle_owner_keeps_worker_and_resource_lease_until_external_join() {
        let (fixture, mut owner) = open_owner("journal-command-drain-idle");
        let client = owner.client();
        assert!(owner.begin_command_drain());
        assert_eq!(client.status(), OperationJournalOwnerStatus::Closing);
        assert!(!owner.begin_command_drain());
        assert!(owner.shared.commands.lock().unwrap().is_some());
        assert!(!owner.worker.as_ref().unwrap().is_finished());
        assert!(OperationJournalStore::open(&fixture.owner).is_err());
        assert!(matches!(
            client.get(OperationId::new()),
            Err(OperationJournalOwnerQueueError::Closed)
        ));
        assert!(owner.shutdown());
        assert!(owner.worker.is_none());
        assert!(OperationJournalStore::open(&fixture.owner).is_ok());
    }

    #[test]
    fn command_drain_actual_worker_panic_stays_unavailable_and_cannot_be_reopened() {
        let fixture = fixture("journal-command-drain-panic");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let (mut owner, ready, release) =
            OperationJournalOwner::start_with_worker_panic_for_test(store);
        wait_until_available(&owner);
        let client = owner.client();
        let mut accepted = client.get(OperationId::new()).unwrap();
        ready.wait();
        assert!(owner.begin_command_drain());
        assert!(OperationJournalStore::open(&fixture.owner).is_err());
        release.wait();
        assert!(accepted.recv_timeout(Duration::from_secs(2)).is_err());
        assert!(!owner.shutdown());
        assert!(matches!(
            owner.status(),
            OperationJournalOwnerStatus::Unavailable { .. }
        ));
        assert!(!owner.begin_command_drain());
        assert!(matches!(
            client.get(OperationId::new()),
            Err(OperationJournalOwnerQueueError::Unavailable { .. })
        ));
        assert!(OperationJournalStore::open(&fixture.owner).is_ok());
    }

    #[test]
    fn command_drain_can_start_immediately_without_waiting_for_bootstrap() {
        let fixture = fixture("journal-command-drain-startup");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
        // Deliberately do not wait for Available. Both actual startup/available races close admission.
        assert!(owner.begin_command_drain());
        assert_eq!(owner.status(), OperationJournalOwnerStatus::Closing);
        assert!(!owner.begin_command_drain());
        assert!(matches!(
            owner.client().get(OperationId::new()),
            Err(OperationJournalOwnerQueueError::Closed)
        ));
        assert!(owner.shutdown());
        assert_eq!(owner.status(), OperationJournalOwnerStatus::Closed);
        assert!(OperationJournalStore::open(&fixture.owner).is_ok());
    }
}
