//! Real worker and profile-owned SQLite tests; recovery-only fixtures issue no physical grant.
#[cfg(test)]
mod integration {
    use super::super::super::{
        CONTROL_COMMAND_CAPACITY, CONTROL_RECEIVE_POLL, CONTROL_RESULT_CAPACITY, InputBudget,
        InputClass, ORDINARY_COMMAND_CAPACITY, ORDINARY_RESULT_CAPACITY, OwnerCommandReceivers,
        ResultBudget,
    };
    use super::super::*;
    use crate::operation_journal::store::{InsertTestBoundary, InsertTestDirective};

    fn retry() -> RetryObservation {
        RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("control-priority-retry").unwrap(),
            53,
            RetryErrorClass::TransientContention,
            "owner_busy",
            None,
        )
        .unwrap()
    }
    fn full(shared: &Arc<OwnerShared>) {
        assert_eq!(*shared.input_bytes.lock().unwrap(), InputBudget::new());
        assert_eq!(*shared.results.lock().unwrap(), ResultBudget::new());
    }
    fn available_shared() -> (Arc<OwnerShared>, OwnerCommandReceivers) {
        let (tx, rx) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(tx));
        shared
            .lifecycle
            .store(OwnerLifecycle::Available as u8, Ordering::Release);
        (shared, rx)
    }

    #[test]
    fn control_priority_real_sql_sequences_fifo_and_missing_intent_refusal_drain_both_queues() {
        let fixture = fixture("journal-control-priority");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let ready = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let (rdy, rel) = (ready.clone(), release.clone());
        let first_hook = std::sync::atomic::AtomicBool::new(true);
        store.set_insert_test_hook(move |boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation
                && first_hook.swap(false, Ordering::AcqRel)
            {
                rdy.wait();
                rel.wait();
            }
            InsertTestDirective::Continue
        });
        let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
        wait_until_available(&owner);
        let client = owner.client();
        let first_id = OperationId::new();
        let mut first = client.persist_intent(v5_copy_record(first_id).0).unwrap();
        ready.wait(); // Inside actual SQL, not just a queue gate.
        let mut ordinary = Vec::new();
        for _ in 0..ORDINARY_COMMAND_CAPACITY {
            let id = OperationId::new();
            ordinary.push((id, client.persist_intent(v5_copy_record(id).0).unwrap()));
        }
        assert!(matches!(
            client.get(first_id),
            Err(OperationJournalOwnerQueueError::Full)
        ));
        // Both legal control commands must overtake all 28 already-queued ordinary intents.
        let mut retried = client.record_retry_pending(first_id, 1, retry()).unwrap();
        let mut cancelled = client
            .request_cancel_before_publish(
                first_id,
                2,
                CancellationRequestObservation::new(56).unwrap(),
            )
            .unwrap();
        let mut stale = client
            .request_cancel_before_publish(
                first_id,
                1,
                CancellationRequestObservation::new(57).unwrap(),
            )
            .unwrap();
        let queued_id = ordinary[0].0;
        let mut missing = client
            .request_cancel_before_publish(
                queued_id,
                1,
                CancellationRequestObservation::new(56).unwrap(),
            )
            .unwrap();
        assert!(matches!(
            client.record_retry_pending(first_id, 1, retry()),
            Err(OperationJournalOwnerQueueError::ResultsFull)
        ));
        assert!(owner.begin_command_drain());
        assert!(OperationJournalStore::open(&fixture.owner).is_err());
        release.wait();
        assert_eq!(
            first
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap()
                .receipt()
                .unwrap()
                .sequence(),
            1
        );
        assert_eq!(
            retried
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap()
                .sequence(),
            2
        );
        assert_eq!(
            cancelled
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap()
                .sequence(),
            3
        );
        assert!(stale.recv_timeout(Duration::from_secs(3)).unwrap().is_err());
        assert!(
            missing
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .is_err()
        );
        for (index, (_, request)) in ordinary.iter_mut().enumerate() {
            assert_eq!(
                request
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap()
                    .receipt()
                    .unwrap()
                    .sequence(),
                index as i64 + 4
            );
        }
        assert_eq!(owner.status(), OperationJournalOwnerStatus::Closing);
        assert!(!owner.worker.as_ref().unwrap().is_finished());
        assert!(OperationJournalStore::open(&fixture.owner).is_err());
        assert!(owner.shutdown());
        full(&owner.shared);
        let reopened = OperationJournalStore::open(&fixture.owner).unwrap();
        assert_eq!(
            reopened.get(first_id).unwrap().unwrap().disposition,
            OperationDisposition::CancelRequestedBeforePublish
        );
        assert_eq!(
            reopened.get(queued_id).unwrap().unwrap().disposition,
            OperationDisposition::None
        );
    }

    #[test]
    fn control_results_real_cancel_survives_sixty_ordinary_handles_and_refunds_only_its_class() {
        let (_fixture, mut owner) = open_owner("journal-control-result-margin");
        let client = owner.client();
        let mut ids = Vec::new();
        for _ in 0..CONTROL_RESULT_CAPACITY {
            let id = OperationId::new();
            let mut request = client.persist_intent(v5_copy_record(id).0).unwrap();
            let seq = request
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .receipt()
                .unwrap()
                .sequence();
            ids.push((id, seq));
        }
        let mut ordinary = Vec::new();
        for _ in 0..ORDINARY_RESULT_CAPACITY {
            let request = client.get(OperationId::new()).unwrap();
            // Observe actual delivered storage reply while retaining the private outstanding handle.
            assert!(
                request
                    .receiver
                    .as_ref()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap()
                    .is_none()
            );
            ordinary.push(request);
        }
        assert!(matches!(
            client.get(OperationId::new()),
            Err(OperationJournalOwnerQueueError::ResultsFull)
        ));
        let mut control = Vec::new();
        for (id, seq) in ids {
            let request = client
                .request_cancel_before_publish(
                    id,
                    seq,
                    CancellationRequestObservation::new(56).unwrap(),
                )
                .unwrap();
            assert_eq!(
                request
                    .receiver
                    .as_ref()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap()
                    .operation_id(),
                id
            );
            control.push(request);
        }
        assert_eq!(owner.shared.results.lock().unwrap().remaining, [0, 0]);
        assert!(matches!(
            client.record_retry_pending(OperationId::new(), 1, retry()),
            Err(OperationJournalOwnerQueueError::ResultsFull)
        ));
        drop(control.pop());
        assert!(matches!(
            client.get(OperationId::new()),
            Err(OperationJournalOwnerQueueError::ResultsFull)
        ));
        let mut replacement = client
            .record_retry_pending(OperationId::new(), 1, retry())
            .unwrap();
        assert!(
            replacement
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        drop(control);
        drop(ordinary);
        owner.shutdown();
        full(&owner.shared);
    }

    #[test]
    fn control_idle_wake_executes_real_cancel_and_half_disconnected_queues_drain() {
        assert!(CONTROL_RECEIVE_POLL <= Duration::from_millis(50));
        let (_fixture, mut owner) = open_owner("journal-control-idle-wake");
        let client = owner.client();
        let id = OperationId::new();
        let mut inserted = client.persist_intent(v5_copy_record(id).0).unwrap();
        let seq = inserted
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .receipt()
            .unwrap()
            .sequence();
        let mut cancel = client
            .request_cancel_before_publish(
                id,
                seq,
                CancellationRequestObservation::new(56).unwrap(),
            )
            .unwrap();
        assert_eq!(
            cancel
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .operation_id(),
            id
        );
        owner.shutdown();
        full(&owner.shared);
        for disconnected in [InputClass::Ordinary, InputClass::Control] {
            let (s, rx) = available_shared();
            let senders = s.commands.lock().unwrap().take().unwrap();
            let (live, dead) = match disconnected {
                InputClass::Ordinary => (senders.control, senders.ordinary),
                InputClass::Control => (senders.ordinary, senders.control),
            };
            drop(dead);
            let class = match disconnected {
                InputClass::Ordinary => InputClass::Control,
                InputClass::Control => InputClass::Ordinary,
            };
            let input = s.reserve_input(class, 1).unwrap();
            let (result, reply) = mpsc::sync_channel(1);
            live.try_send(JournalCommand::Get {
                operation_id: OperationId::new(),
                input,
                result,
            })
            .unwrap();
            drop(live);
            reject_command(
                rx.recv().expect("live half drains accepted command"),
                OperationJournalOwnerError::Closed,
            );
            assert!(reply.recv_timeout(Duration::from_secs(1)).unwrap().is_err());
            assert!(rx.recv().is_none());
            full(&s);
        }
    }

    #[test]
    fn control_result_poison_and_unconfirmable_refund_fail_closed_without_drop_panic() {
        let (s, _rx) = available_shared();
        let held = s.reserve_result(InputClass::Control).unwrap();
        let worker = s.clone();
        assert!(
            thread::spawn(move || {
                let _lock = worker.results.lock().unwrap();
                panic!("injected result credit poison");
            })
            .join()
            .is_err()
        );
        assert!(panic::catch_unwind(AssertUnwindSafe(|| drop(held))).is_ok());
        assert!(matches!(
            s.reserve_result(InputClass::Ordinary),
            Err(OperationJournalOwnerQueueError::Unavailable { .. })
        ));
        assert!(matches!(
            s.status(),
            OperationJournalOwnerStatus::Unavailable { .. }
        ));
        let (s, _rx) = available_shared();
        let held = s.reserve_result(InputClass::Ordinary).unwrap();
        s.results.lock().unwrap().remaining[0] = ORDINARY_RESULT_CAPACITY;
        assert!(panic::catch_unwind(AssertUnwindSafe(|| drop(held))).is_ok());
        assert!(s.results.lock().unwrap().unavailable);
        assert!(matches!(
            s.reserve_result(InputClass::Control),
            Err(OperationJournalOwnerQueueError::Unavailable { .. })
        ));
    }

    #[test]
    fn control_queue_and_result_partitions_preserve_exact_original_aggregate_limits() {
        assert_eq!(
            ORDINARY_COMMAND_CAPACITY + CONTROL_COMMAND_CAPACITY,
            OPERATION_JOURNAL_OWNER_COMMAND_CAPACITY
        );
        assert_eq!(
            ORDINARY_RESULT_CAPACITY + CONTROL_RESULT_CAPACITY,
            OPERATION_JOURNAL_OWNER_RESULT_CAPACITY
        );
        let (s, _rx) = available_shared();
        let mut held = Vec::new();
        for class in [InputClass::Ordinary, InputClass::Control] {
            for _ in 0..class.result_limit() {
                held.push(s.reserve_result(class).unwrap());
            }
            assert!(matches!(
                s.reserve_result(class),
                Err(OperationJournalOwnerQueueError::ResultsFull)
            ));
        }
        assert_eq!(s.results.lock().unwrap().remaining, [0, 0]);
        drop(held);
        full(&s);
    }
}
