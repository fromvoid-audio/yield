//! Actual journal credit/SQLite lifecycle cases. No capacity bootstrap or physical grant.
#[cfg(test)]
mod integration {
    use super::super::super::{
        CONTROL_INPUT_RESERVED_BYTES, InputBudget, InputClass, InputPermit,
        ORDINARY_COMMAND_CAPACITY, ORDINARY_INPUT_BYTES, OwnerCommandReceivers, ResultBudget,
    };
    use super::super::*;
    use crate::operation_journal::store::{InsertTestBoundary, InsertTestDirective};
    fn snapshot(shared: &Arc<OwnerShared>) -> InputBudget {
        *shared.input_bytes.lock().unwrap()
    }
    fn full(shared: &Arc<OwnerShared>) {
        assert_eq!(snapshot(shared), InputBudget::shared_initial());
    }
    fn shared() -> (Arc<OwnerShared>, OwnerCommandReceivers) {
        let (tx, rx) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(tx));
        shared
            .lifecycle
            .store(OwnerLifecycle::Available as u8, Ordering::Release);
        (shared, rx)
    }
    fn exhausted(shared: &Arc<OwnerShared>, class: InputClass) -> InputPermit {
        let remaining = snapshot(shared).remaining[class.index()];
        shared.reserve_input(class, remaining).unwrap()
    }
    struct PausedWorkerRelease(Option<Arc<std::sync::Barrier>>);
    impl PausedWorkerRelease {
        fn release(&mut self) {
            if let Some(barrier) = self.0.take() {
                barrier.wait();
            }
        }
    }
    impl Drop for PausedWorkerRelease {
        fn drop(&mut self) {
            self.release();
        }
    }
    fn retry() -> RetryObservation {
        RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("control-input-retry").unwrap(),
            53,
            RetryErrorClass::TransientContention,
            "owner_busy",
            Some("bounded credit test".into()),
        )
        .unwrap()
    }
    #[test]
    fn control_input_disjoint_exact_total_saturation_never_lends_margin_to_tiny_ordinary() {
        let (s, _rx) = shared();
        assert_eq!(
            ORDINARY_INPUT_BYTES + CONTROL_INPUT_RESERVED_BYTES,
            OPERATION_JOURNAL_OWNER_MAX_QUEUED_INPUT_BYTES
        );
        let ordinary = exhausted(&s, InputClass::Ordinary);
        assert!(matches!(
            s.reserve_input(InputClass::Ordinary, 1),
            Err(OperationJournalOwnerQueueError::InputBytesFull {
                limit: ORDINARY_INPUT_BYTES,
                requested: 1
            })
        ));
        assert_eq!(snapshot(&s).remaining[1], CONTROL_INPUT_RESERVED_BYTES);
        let control = exhausted(&s, InputClass::Control);
        assert_eq!(snapshot(&s).remaining, [0, 0]);
        assert!(matches!(
            s.reserve_input(InputClass::Control, 1),
            Err(OperationJournalOwnerQueueError::InputBytesFull {
                limit: CONTROL_INPUT_RESERVED_BYTES,
                requested: 1
            })
        ));
        drop(control);
        assert!(s.reserve_input(InputClass::Ordinary, 1).is_err());
        drop(ordinary);
        full(&s);
        for class in [InputClass::Ordinary, InputClass::Control] {
            assert!(matches!(
                s.reserve_input(class, 0),
                Err(OperationJournalOwnerQueueError::InvalidInput { .. })
            ));
            assert!(matches!(
                s.reserve_input(class, usize::MAX),
                Err(OperationJournalOwnerQueueError::InputBytesFull { .. })
            ));
        }
        full(&s);
    }
    #[test]
    fn control_input_actual_sql_execution_retains_payload_credit_after_reply_loss_and_allows_cancel()
     {
        let fixture = fixture("journal-control-input-actual-sql");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let ready = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let (rdy, rel) = (ready.clone(), release.clone());
        store.set_insert_test_hook(move |boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                rdy.wait();
                rel.wait();
            }
            InsertTestDirective::Continue
        });
        let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
        wait_until_available(&owner);
        let client = owner.client();
        let id = OperationId::new();
        let ordinary_before = snapshot(&owner.shared).remaining[0];
        let (record, _) = v5_copy_record(id);
        let bytes = bounded_encoded_record(&record).unwrap().len();
        let accepted = client.persist_intent(record).unwrap();
        ready.wait();
        let mut paused = PausedWorkerRelease(Some(release.clone()));
        // We are INSIDE real SQLite's transaction, after decoding; queue arrival is insufficient.
        assert_eq!(
            snapshot(&owner.shared).remaining,
            [ordinary_before - bytes, CONTROL_INPUT_RESERVED_BYTES]
        );
        drop(accepted); // Accepted reply loss returns result credit, never executing input credit.
        assert_eq!(
            snapshot(&owner.shared).remaining[0],
            ordinary_before - bytes
        );
        let backing = owner
            .shared
            .reserve_input(InputClass::Ordinary, ordinary_before - bytes)
            .unwrap();
        assert!(matches!(
            client.get(id),
            Err(OperationJournalOwnerQueueError::InputBytesFull {
                limit: ORDINARY_INPUT_BYTES,
                ..
            })
        ));
        let mut cancel = client
            .request_cancel_before_publish(id, 1, CancellationRequestObservation::new(56).unwrap())
            .unwrap();
        assert!(snapshot(&owner.shared).remaining[1] < CONTROL_INPUT_RESERVED_BYTES);
        paused.release();
        let receipt = cancel
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(receipt.operation_id(), id);
        drop(backing);
        owner.shutdown();
        full(&owner.shared);
        let reopened = OperationJournalStore::open(&fixture.owner).unwrap();
        let persisted = reopened.get(id).unwrap().unwrap();
        assert_eq!(
            persisted.disposition,
            OperationDisposition::CancelRequestedBeforePublish
        );
    }
    #[test]
    fn control_input_real_retry_cancel_validate_store_despite_ordinary_saturation() {
        let (_fixture, mut owner) = open_owner("journal-control-input-retry");
        let client = owner.client();
        let id = OperationId::new();
        let (record, _) = v5_copy_record(id);
        let completion_deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut inserted = client.persist_intent(record).unwrap();
        let sequence = inserted
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .receipt()
            .unwrap()
            .sequence();
        // The reply precedes command-input Drop. A real readonly control command
        // executes only after that Drop, without assuming the live owner's retained
        // resource charges are absent or borrowing ordinary input credit.
        let mut barrier = client
            .submit_control(CONTROL_INPUT_BYTES, |input, result| JournalCommand::Get {
                operation_id: OperationId::new(),
                input,
                result,
            })
            .unwrap();
        assert!(
            barrier
                .recv_timeout(
                    completion_deadline.saturating_duration_since(std::time::Instant::now())
                )
                .unwrap()
                .unwrap()
                .is_none()
        );
        let ordinary = exhausted(&owner.shared, InputClass::Ordinary);
        assert!(matches!(
            client.get(id),
            Err(OperationJournalOwnerQueueError::InputBytesFull { .. })
        ));
        let mut request = client.record_retry_pending(id, sequence, retry()).unwrap();
        let receipt = request
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        let mut stale = client
            .request_cancel_before_publish(
                id,
                sequence,
                CancellationRequestObservation::new(56).unwrap(),
            )
            .unwrap();
        assert!(stale.recv_timeout(Duration::from_secs(2)).unwrap().is_err());
        let mut cancel = client
            .request_cancel_before_publish(
                id,
                receipt.sequence(),
                CancellationRequestObservation::new(56).unwrap(),
            )
            .unwrap();
        cancel
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        drop(ordinary);
        owner.shutdown();
        full(&owner.shared);
    }
    #[test]
    fn control_input_queue_full_restores_exact_control_and_ordinary_permits() {
        let fixture = fixture("journal-control-input-queue");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let (mut owner, ready, release) =
            OperationJournalOwner::start_with_command_gate_for_test(store);
        wait_until_available(&owner);
        let client = owner.client();
        let first = client.get(OperationId::new()).unwrap();
        ready.wait();
        let mut paused = PausedWorkerRelease(Some(release.clone()));
        let mut queued = Vec::new();
        for _ in 0..ORDINARY_COMMAND_CAPACITY {
            queued.push(client.get(OperationId::new()).unwrap());
        }
        for _ in 0..super::super::super::CONTROL_COMMAND_CAPACITY {
            drop(
                client
                    .request_cancel_before_publish(
                        OperationId::new(),
                        1,
                        CancellationRequestObservation::new(56).unwrap(),
                    )
                    .unwrap(),
            );
        }
        let before = snapshot(&owner.shared);
        assert!(matches!(
            client.request_cancel_before_publish(
                OperationId::new(),
                1,
                CancellationRequestObservation::new(56).unwrap()
            ),
            Err(OperationJournalOwnerQueueError::Full)
        ));
        assert!(matches!(
            client.get(OperationId::new()),
            Err(OperationJournalOwnerQueueError::Full)
        ));
        assert_eq!(snapshot(&owner.shared), before);
        assert!(owner.begin_command_drain());
        let before = snapshot(&owner.shared);
        assert!(matches!(
            client.record_retry_pending(OperationId::new(), 1, retry()),
            Err(OperationJournalOwnerQueueError::Closed)
        ));
        assert_eq!(snapshot(&owner.shared), before);
        drop(first);
        drop(queued);
        paused.release();
        owner.shutdown();
        full(&owner.shared);
    }
    #[test]
    fn control_input_result_backpressure_and_control_exhaustion_restore_before_queue_admission() {
        let (_fixture, mut owner) = open_owner("journal-control-input-results");
        let client = owner.client();
        let permits: Vec<_> = [InputClass::Ordinary, InputClass::Control]
            .into_iter()
            .flat_map(|class| (0..class.result_limit()).map(move |_| class))
            .map(|class| owner.shared.reserve_result(class).unwrap())
            .collect();
        let before = snapshot(&owner.shared);
        assert!(matches!(
            client.request_cancel_before_publish(
                OperationId::new(),
                1,
                CancellationRequestObservation::new(56).unwrap()
            ),
            Err(OperationJournalOwnerQueueError::ResultsFull)
        ));
        assert_eq!(snapshot(&owner.shared), before);
        drop(permits);
        let control = exhausted(&owner.shared, InputClass::Control);
        assert!(matches!(
            client.record_retry_pending(OperationId::new(), 1, retry()),
            Err(OperationJournalOwnerQueueError::InputBytesFull {
                limit: CONTROL_INPUT_RESERVED_BYTES,
                ..
            })
        ));
        let mut ordinary = client.get(OperationId::new()).unwrap();
        assert!(
            ordinary
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .is_none()
        );
        drop(control);
        owner.shutdown();
        full(&owner.shared);
    }
    #[test]
    fn control_input_disconnected_sender_returns_both_classes_without_authority() {
        for control in [false, true] {
            let (s, rx) = shared();
            let client = OperationJournalClient { shared: s.clone() };
            drop(rx);
            let rejected = if control {
                client
                    .record_retry_pending(OperationId::new(), 1, retry())
                    .map(|_| ())
            } else {
                client.get(OperationId::new()).map(|_| ())
            };
            assert!(matches!(
                rejected,
                Err(OperationJournalOwnerQueueError::Closed)
            ));
            full(&s);
            assert!(matches!(
                s.status(),
                OperationJournalOwnerStatus::Unavailable { .. }
            ));
            assert_eq!(*s.results.lock().unwrap(), ResultBudget::new());
        }
    }
    #[test]
    fn control_input_actual_worker_panic_retains_then_drops_exact_accepted_queue_backing() {
        let fixture = fixture("journal-control-input-panic");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let (mut owner, ready, release) =
            OperationJournalOwner::start_with_worker_panic_for_test(store);
        wait_until_available(&owner);
        let client = owner.client();
        let accepted = client.get(OperationId::new()).unwrap();
        ready.wait();
        let mut paused = PausedWorkerRelease(Some(release.clone()));
        let pending = client
            .request_cancel_before_publish(
                OperationId::new(),
                1,
                CancellationRequestObservation::new(56).unwrap(),
            )
            .unwrap();
        let queued_ordinary = client.get(OperationId::new()).unwrap();
        let before = snapshot(&owner.shared);
        assert!(before.remaining[0] < ORDINARY_INPUT_BYTES);
        assert!(before.remaining[1] < CONTROL_INPUT_RESERVED_BYTES);
        drop(accepted);
        drop(pending);
        drop(queued_ordinary);
        assert_eq!(snapshot(&owner.shared), before);
        paused.release();
        owner.shutdown();
        full(&owner.shared);
        assert!(matches!(
            owner.status(),
            OperationJournalOwnerStatus::Unavailable { .. }
        ));
    }
    #[test]
    fn control_input_inner_sql_panic_returns_executing_and_rejected_queue_credit_after_rollback() {
        let fixture = fixture("journal-control-input-inner-panic");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let ready = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let (rdy, rel) = (ready.clone(), release.clone());
        store.set_insert_test_hook(move |boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                rdy.wait();
                rel.wait();
                panic!("injected SQL command panic");
            }
            InsertTestDirective::Continue
        });
        let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
        wait_until_available(&owner);
        let client = owner.client();
        let id = OperationId::new();
        let (record, _) = v5_copy_record(id);
        let mut first = client.persist_intent(record).unwrap();
        ready.wait();
        let mut paused = PausedWorkerRelease(Some(release.clone()));
        let mut pending = client
            .request_cancel_before_publish(id, 1, CancellationRequestObservation::new(56).unwrap())
            .unwrap();
        let mut queued_ordinary = client.get(OperationId::new()).unwrap();
        let before = snapshot(&owner.shared);
        assert!(before.remaining[0] < ORDINARY_INPUT_BYTES);
        assert!(before.remaining[1] < CONTROL_INPUT_RESERVED_BYTES);
        paused.release();
        assert!(first.recv_timeout(Duration::from_secs(2)).unwrap().is_err());
        assert!(
            pending
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        assert!(
            queued_ordinary
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        owner.shutdown();
        full(&owner.shared);
        assert!(matches!(
            owner.status(),
            OperationJournalOwnerStatus::Unavailable { .. }
        ));
        let reopened = OperationJournalStore::open(&fixture.owner).unwrap();
        assert!(reopened.get(id).unwrap().is_none());
    }
    #[test]
    fn control_input_poison_and_unconfirmable_refund_fail_closed_without_drop_panic_or_reset() {
        let (s, _rx) = shared();
        let held = s.reserve_input(InputClass::Control, 32).unwrap();
        let worker = s.clone();
        assert!(
            thread::spawn(move || {
                let _guard = worker.input_bytes.lock().unwrap();
                panic!("injected credit mutex poison");
            })
            .join()
            .is_err()
        );
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(held))).is_ok());
        assert!(matches!(
            s.reserve_input(InputClass::Ordinary, 1),
            Err(OperationJournalOwnerQueueError::Unavailable { .. })
        ));
        assert!(matches!(
            s.status(),
            OperationJournalOwnerStatus::Unavailable { .. }
        ));
        let (s, _rx) = shared();
        let held = s.reserve_input(InputClass::Ordinary, 32).unwrap();
        // Fault fixture only: lose debit evidence. Drop must not overrefund/reopen the pool.
        s.input_bytes.lock().unwrap().remaining[0] = ORDINARY_INPUT_BYTES;
        drop(held);
        let image = snapshot(&s);
        assert!(image.unavailable);
        assert_eq!(image.remaining[0], ORDINARY_INPUT_BYTES);
        assert!(matches!(
            s.reserve_input(InputClass::Control, 1),
            Err(OperationJournalOwnerQueueError::Unavailable { .. })
        ));
        assert!(matches!(
            s.status(),
            OperationJournalOwnerStatus::Unavailable { .. }
        ));
    }
}
