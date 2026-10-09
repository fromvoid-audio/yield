use super::super::request_inbox::*;
use super::*;

#[test]
fn inbox_actual_blocked_worker_retains_empty_then_delivers_exact_persistence_receipt_as_stale() {
    let fixture = fixture("journal-inbox-blocked");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let (mut owner, ready, release) =
        OperationJournalOwner::start_with_command_gate_for_test(store);
    wait_until_available(&owner);
    let mut inbox = JournalRequestInbox::new(owner.client()).unwrap();
    let replacement = JournalRequestInbox::<JournalInsertOutcome>::new(owner.client()).unwrap();
    let record = intent_record(OperationId::new());
    let canonical = super::super::super::encode(&record).unwrap();
    let correlation = Uuid::new_v4();
    let identity = *blake3::hash(&canonical).as_bytes();
    let reservation = inbox
        .reserve(
            correlation,
            record.schema_version,
            identity,
            canonical.len(),
        )
        .unwrap();
    let request = reservation.client().persist_intent(record.clone()).unwrap();
    let context = reservation
        .accept(request)
        .unwrap_or_else(|(e, _)| panic!("{e}"));
    ready.wait();
    let start = std::time::Instant::now();
    let polls = inbox.poll(inbox.installation(), |_| panic!("blocked original worker"));
    let elapsed = start.elapsed();
    let pending = inbox.pending();
    release.wait();
    assert_eq!(polls, 1);
    assert_eq!(pending, 1);
    assert!(elapsed < Duration::from_secs(1));
    let mut completion = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while completion.is_none() {
        inbox.poll(replacement.installation(), |result| {
            completion = Some(result)
        });
        assert!(std::time::Instant::now() < deadline);
        thread::yield_now();
    }
    let completion = completion.unwrap();
    assert_eq!(completion.context, context);
    assert_eq!(context.correlation(), correlation);
    assert_eq!(context.payload_identity(), identity);
    assert!(!completion.matches_installation);
    assert!(matches!(
        completion.delivery,
        JournalRequestDelivery::Result(Ok(JournalInsertOutcome::Committed(_)))
    ));
    assert_eq!(inbox.pending(), 0);
    drop(replacement);
    drop(inbox);
    assert!(owner.shutdown());
    assert_eq!(
        OperationJournalStore::open(&fixture.owner)
            .unwrap()
            .get(record.operation_id)
            .unwrap(),
        Some(record)
    );
}

#[test]
fn inbox_original_queue_enforces_slots_bytes_duplicate_and_sixteen_handle_poll_limit() {
    let (_fixture, mut owner) = open_owner("journal-inbox-bounds");
    let client = owner.client();
    let baseline = *client.shared.input_bytes.lock().unwrap();
    let mut inbox =
        JournalRequestInbox::<Option<StoredOperationRecord>>::new(client.clone()).unwrap();
    assert!(client.shared.input_bytes.lock().unwrap().remaining[0] < baseline.remaining[0]);
    let installation = inbox.installation();
    {
        let reservation = inbox
            .reserve(
                Uuid::new_v4(),
                1,
                [1; 32],
                OPERATION_JOURNAL_OWNER_MAX_QUEUED_INPUT_BYTES,
            )
            .unwrap();
        let request = reservation.client().get(OperationId::new()).unwrap();
        reservation
            .accept(request)
            .unwrap_or_else(|(e, _)| panic!("{e}"));
    }
    assert!(matches!(
        inbox.reserve(Uuid::new_v4(), 1, [2; 32], 1),
        Err(JournalRequestInboxError::InputBytesFull)
    ));
    let correlation = inbox.first_context_for_test().unwrap().correlation();
    assert!(matches!(
        inbox.reserve(correlation, 1, [3; 32], 1),
        Err(JournalRequestInboxError::InvalidContext)
    ));
    assert_eq!(
        inbox.detach(correlation).unwrap().correlation(),
        correlation
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    // Detached accepted work may still hold a result permit until the worker runs it.
    while client.shared.results.lock().unwrap().remaining
        != [ORDINARY_RESULT_CAPACITY, CONTROL_RESULT_CAPACITY]
    {
        assert!(std::time::Instant::now() < deadline);
        thread::yield_now();
    }
    for index in 0..64 {
        loop {
            let reservation = inbox.reserve(Uuid::new_v4(), 1, [4; 32], 1).unwrap();
            let result = if index < ORDINARY_RESULT_CAPACITY {
                reservation.client().get(OperationId::new())
            } else {
                // Real control-class queue/result permits; no effect authority is issued.
                reservation
                    .client()
                    .submit_control(CONTROL_INPUT_BYTES, |input, result| JournalCommand::Get {
                        operation_id: OperationId::new(),
                        input,
                        result,
                    })
            };
            match result {
                Ok(request) => {
                    reservation
                        .accept(request)
                        .unwrap_or_else(|(e, _)| panic!("{e}"));
                    break;
                }
                Err(OperationJournalOwnerQueueError::Full) => {
                    drop(reservation);
                    thread::yield_now();
                }
                Err(e) => panic!("{e}"),
            }
            assert!(std::time::Instant::now() < deadline);
        }
    }
    assert!(matches!(
        inbox.reserve(Uuid::new_v4(), 1, [5; 32], 1),
        Err(JournalRequestInboxError::Full)
    ));
    let mut delivered = 0;
    while inbox.pending() != 0 {
        let previous = delivered;
        assert!(
            inbox.poll(installation, |completion| {
                assert!(completion.matches_installation);
                assert!(matches!(
                    completion.delivery,
                    JournalRequestDelivery::Result(Ok(None))
                ));
                delivered += 1;
            }) <= 16
        );
        assert!(delivered - previous <= 16);
        assert!(std::time::Instant::now() < deadline);
        thread::yield_now();
    }
    assert_eq!(delivered, 64);
    drop(inbox);
    while *client.shared.input_bytes.lock().unwrap() != baseline {
        assert!(std::time::Instant::now() < deadline);
        thread::yield_now();
    }
    assert_eq!(*client.shared.input_bytes.lock().unwrap(), baseline);
    assert!(owner.shutdown());
}

#[test]
fn inbox_original_generation_exhaustion_and_queue_rejection_refund_without_reusing_context() {
    let (fixture, mut owner) = open_owner("journal-inbox-generation");
    let client = owner.client();
    let mut inbox =
        JournalRequestInbox::<Option<StoredOperationRecord>>::new(client.clone()).unwrap();
    let original = inbox.installation();
    let replacement =
        JournalRequestInbox::<Option<StoredOperationRecord>>::new(client.clone()).unwrap();
    assert_ne!(original, replacement.installation());
    drop(replacement);
    let baseline = *client.shared.input_bytes.lock().unwrap();
    client
        .shared
        .service_generation
        .store(u64::MAX, Ordering::Relaxed);
    assert!(matches!(
        JournalRequestInbox::<Option<StoredOperationRecord>>::new(client.clone()),
        Err(JournalRequestInboxError::GenerationExhausted)
    ));
    assert_eq!(*client.shared.input_bytes.lock().unwrap(), baseline);
    let reservation = inbox.reserve(Uuid::new_v4(), 1, [0; 32], 100).unwrap();
    assert!(owner.begin_command_drain());
    assert!(matches!(
        reservation.client().get(OperationId::new()),
        Err(OperationJournalOwnerQueueError::Closed)
    ));
    drop(reservation);
    assert_eq!(inbox.pending(), 0);
    assert_eq!(inbox.encoded_bytes_for_test(), 0);
    drop(inbox);
    assert!(owner.shutdown());
    assert!(
        OperationJournalStore::open(&fixture.owner)
            .unwrap()
            .scan_recovery_page(None)
            .unwrap()
            .entries()
            .is_empty()
    );
}

#[test]
fn inbox_refunds_rejection_and_retains_actual_pre_intent_refusal_without_a_success_identity() {
    let (fixture, mut owner) = open_owner("journal-inbox-refusal");
    let client = owner.client();
    let mut inbox = JournalRequestInbox::<OperationId>::new(client).unwrap();
    let (args, participants) = valid_full_admission_request();
    let canonical = serde_json::to_vec(&(&args, &participants)).unwrap();
    let reservation = inbox
        .reserve(
            Uuid::new_v4(),
            1,
            *blake3::hash(&canonical).as_bytes(),
            canonical.len(),
        )
        .unwrap();
    let request = reservation.client().admit_full(args, participants).unwrap();
    reservation
        .accept(request)
        .unwrap_or_else(|(e, _)| panic!("{e}"));
    let installed = inbox.installation();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut refused = false;
    while !refused {
        inbox.poll(installed, |completion| {
            assert!(matches!(
                completion.delivery,
                JournalRequestDelivery::Result(Err(
                    OperationJournalOwnerError::CapacityProviderUnavailable
                ))
            ));
            refused = true;
        });
        assert!(std::time::Instant::now() < deadline);
        thread::yield_now();
    }
    assert_eq!(inbox.encoded_bytes_for_test(), 0);
    assert!(owner.begin_command_drain());
    assert!(matches!(
        inbox.reserve(Uuid::new_v4(), 1, [0; 32], 1),
        Err(JournalRequestInboxError::Owner(
            OperationJournalOwnerQueueError::Closed
        ))
    ));
    assert_eq!(inbox.pending(), 0);
    drop(inbox);
    assert!(owner.shutdown());
    assert!(
        OperationJournalStore::open(&fixture.owner)
            .unwrap()
            .scan_recovery_page(None)
            .unwrap()
            .entries()
            .is_empty()
    );
}

#[test]
fn inbox_foreign_actual_owner_handle_is_returned_untouched_and_worker_loss_is_unconfirmed() {
    let (_fixture, mut owner) = open_owner("journal-inbox-original");
    let (_other_fixture, mut other) = open_owner("journal-inbox-foreign");
    let mut inbox =
        JournalRequestInbox::<Option<StoredOperationRecord>>::new(owner.client()).unwrap();
    let reservation = inbox.reserve(Uuid::new_v4(), 1, [0; 32], 1).unwrap();
    let foreign = other.client().get(OperationId::new()).unwrap();
    let (error, mut returned) = reservation.accept(foreign).err().unwrap();
    assert!(matches!(error, JournalRequestInboxError::ForeignOrConsumed));
    assert!(
        returned
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .is_none()
    );
    assert_eq!(inbox.pending(), 0);
    assert_eq!(inbox.encoded_bytes_for_test(), 0);
    drop(inbox);
    assert!(owner.shutdown());
    assert!(other.shutdown());

    let fixture = fixture("journal-inbox-panic");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let (mut owner, ready, release) =
        OperationJournalOwner::start_with_worker_panic_for_test(store);
    wait_until_available(&owner);
    let mut inbox = JournalRequestInbox::new(owner.client()).unwrap();
    let reservation = inbox.reserve(Uuid::new_v4(), 1, [0; 32], 1).unwrap();
    let request = reservation.client().get(OperationId::new()).unwrap();
    reservation
        .accept(request)
        .unwrap_or_else(|(e, _)| panic!("{e}"));
    ready.wait();
    release.wait();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut unconfirmed = false;
    while !unconfirmed {
        inbox.poll(inbox.installation(), |completion| {
            assert!(matches!(
                completion.delivery,
                JournalRequestDelivery::Unconfirmed
            ));
            unconfirmed = true;
        });
        assert!(std::time::Instant::now() < deadline);
        thread::yield_now();
    }
    drop(inbox);
    assert!(!owner.shutdown());
}
