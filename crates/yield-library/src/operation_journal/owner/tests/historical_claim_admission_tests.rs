use super::*;
use crate::operation_journal::FullCapacityAllocationClass;
use std::time::Instant;

#[test]
fn ordinary_rebuild_backing_actual_queued_collection_pressure_precedes_identity_allocation() {
    let fixture = fixture("ordinary-rebuild-queued-pressure");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let old_id = OperationId::new();
    let wire = format!(
        r#"{{"schema_version":1,"operation_id":"{old_id}","intent":{{"actor":"User","kind":"FileHistory","label":"old"}},"phase":"IntentDurable","disposition":"None","payload":null,"created_unix_ms":1,"updated_unix_ms":1}}"#
    );
    store
        .install_historical_claim_fixture(old_id, 1, wire.as_bytes(), Uuid::new_v4())
        .unwrap();
    let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
    wait_until_available(&owner);
    let prepared = prepared_copy_input(
        FullCapacityAllocationClass::DestinationStaging,
        VolumeIdentity { device: 1 },
        1,
    );
    let request_bytes = bounded_prepared_admission_input(&prepared).unwrap();
    let available = owner.shared.input_bytes.lock().unwrap().remaining[0];
    // Fund the genuine command, reply header, and complete terminal consumer;
    // leave exactly 4 KiB for the count/collection phase, not an earlier refusal.
    let held_bytes = available
        - request_bytes
        - HISTORICAL_REPLY_BYTES
        - crate::operation_journal::store::HISTORICAL_DIAGNOSTIC_BYTES
        - 4096;
    let held = owner
        .shared
        .reserve_input(InputClass::Ordinary, held_bytes)
        .unwrap();
    let mut request = owner.client().admit_prepared(prepared).unwrap();
    let error = request
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("ordinary claim rebuild resource envelope exceeded"),
        "{error}"
    );
    assert_eq!(owner.prepared_admission_id_allocations(), 0);
    drop(error);
    drop(request);
    drop(held);
    assert!(owner.shutdown());
    assert_eq!(
        owner.shared.input_bytes.lock().unwrap().remaining[0],
        InputClass::Ordinary.limit()
    );
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    assert_eq!(store.get(old_id).unwrap().unwrap().operation_id, old_id);
    let snapshot = store.rebuild_admission_snapshot().unwrap();
    assert_eq!(snapshot.claims().len(), 1);
}

#[test]
fn historical_claim_actual_admit_prepared_charges_original_pool_and_refuses_pressure_before_effects()
 {
    let fixture = fixture("historical-claim-queued-original-pool");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let old_id = OperationId::new();
    let wire = format!(
        r#"{{"schema_version":1,"operation_id":"{old_id}","intent":{{"actor":"User","kind":"FileHistory","label":"old"}},"phase":"IntentDurable","disposition":"None","payload":[0{}],"created_unix_ms":1,"updated_unix_ms":1}}"#,
        ",0".repeat((MAX_RECORD_BYTES - 512) / 2)
    );
    let database = fixture
        .owner
        .profile_root()
        .join("operation_journal/journal.sqlite3");
    store
        .install_historical_claim_fixture(old_id, 1, wire.as_bytes(), Uuid::new_v4())
        .unwrap();
    let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
    wait_until_available(&owner);
    let pool_before = InputClass::Ordinary.limit();
    let retained = owner
        .shared
        .reserve_input(InputClass::Ordinary, 3 * MAX_RECORD_BYTES)
        .unwrap();
    let request_input = prepared_copy_input(
        FullCapacityAllocationClass::DestinationStaging,
        VolumeIdentity { device: 1 },
        1,
    );
    let mut request = owner.client().admit_prepared(request_input).unwrap();
    let error = request
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("historical claim observation resource envelope exceeded"),
        "{error}"
    );
    drop(request);
    drop(retained);
    let request_input = prepared_copy_input(
        FullCapacityAllocationClass::DestinationStaging,
        VolumeIdentity { device: 1 },
        1,
    );
    let mut request = owner.client().admit_prepared(request_input).unwrap();
    assert!(matches!(
        request.recv_timeout(Duration::from_secs(5)).unwrap(),
        Err(OperationJournalOwnerError::PreparedAdmission(
            PreparedAdmissionError::DescriptorRefreshRejected { .. }
        ))
    ));
    drop(request);
    assert!(
        owner
            .shared
            .historical_scratch_maximum
            .load(Ordering::Relaxed)
            >= 2 * wire.len()
    );
    assert_eq!(owner.prepared_admission_id_allocations(), 0);
    assert!(owner.shutdown());
    assert_eq!(
        owner.shared.input_bytes.lock().unwrap().remaining[0],
        pool_before
    );
    let sql = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(
        sql.query_row("SELECT count(*) FROM operation_journal", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        sql.query_row("SELECT record FROM operation_journal", [], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .unwrap(),
        wire.as_bytes()
    );
    assert_eq!(
        sql.query_row(
            "SELECT count(*) FROM operation_journal_ordinary_claim",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn historical_claim_unconsumed_actor_error_hands_off_to_funded_diagnostics_until_receiver_drop() {
    let fixture = fixture("historical-claim-unconsumed-error");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let database = fixture
        .owner
        .profile_root()
        .join("operation_journal/journal.sqlite3");
    let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
    wait_until_available(&owner);
    let pool_before = InputClass::Ordinary.limit();
    let old_id = OperationId::new();
    // Introduce a malformed historical row after genuine startup. The actor must
    // retain its original typed error rather than allocating an operation identity.
    let wire = format!("{{{}", " ".repeat(MAX_RECORD_BYTES - 1));
    for (sequence, operation) in [(1, old_id), (2, OperationId::new())] {
        let copied = wire.as_bytes().to_vec();
        let mut fixture_request = owner
            .client()
            .submit(
                copied.len() + std::mem::size_of::<JournalCommand>(),
                move |input, result| JournalCommand::InstallHistoricalClaimFixture {
                    operation,
                    sequence,
                    wire: copied,
                    claim: Uuid::new_v4(),
                    input,
                    result,
                },
            )
            .unwrap();
        fixture_request
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
    }
    let request = owner
        .client()
        .admit_prepared(prepared_copy_input(
            FullCapacityAllocationClass::DestinationStaging,
            VolumeIdentity { device: 1 },
            1,
        ))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let retained = request
            .historical_reply
            .as_ref()
            .unwrap()
            .consumer
            .lock()
            .unwrap()
            .is_some();
        if retained {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "actual observer error never retained credit"
        );
        std::thread::yield_now();
    }
    // Join the genuine actor while keeping the result unconsumed. This separates
    // command input cleanup from the returned error backing's actual lifetime.
    assert!(owner.shutdown());
    assert_eq!(
        pool_before - owner.shared.input_bytes.lock().unwrap().remaining[0],
        crate::operation_journal::store::HISTORICAL_DIAGNOSTIC_BYTES + HISTORICAL_REPLY_BYTES
    );
    assert_eq!(owner.prepared_admission_id_allocations(), 0);
    drop(request);
    assert_eq!(
        owner.shared.input_bytes.lock().unwrap().remaining[0],
        pool_before
    );
    let sql = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(
        sql.query_row("SELECT count(*) FROM operation_journal", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        sql.query_row("SELECT record FROM operation_journal LIMIT 1", [], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .unwrap(),
        wire.as_bytes()
    );
}

#[test]
fn historical_claim_late_send_disposes_payload_before_original_reply_credit() {
    let (senders, _receivers) = OwnerCommandReceivers::channel();
    let shared = Arc::new(OwnerShared::new(senders));
    let before = shared.input_bytes.lock().unwrap().remaining[0];
    let header = shared
        .reserve_input(InputClass::Ordinary, HISTORICAL_REPLY_BYTES)
        .unwrap();
    let backing = HistoricalReply::new(HistoricalReplyBacking {
        _header: header,
        consumer: Mutex::new(None),
        observer: Mutex::new(None),
        consumer_refused: std::sync::atomic::AtomicBool::new(false),
    });
    let mut input = shared.reserve_input(InputClass::Ordinary, 1).unwrap();
    input.historical_reply = Some(backing.clone());
    let scratch = input.reserve_historical_scratch(4096).unwrap();
    input.retain_historical_error(scratch, None, None, None);
    let (sender, receiver) = mpsc::sync_channel::<Result<(), OperationJournalOwnerError>>(1);
    let result_permit = shared.reserve_result(InputClass::Ordinary).unwrap();
    let request = OperationJournalRequest {
        receiver: Some(receiver),
        result_permit: Some(result_permit),
        historical_reply: Some(backing.clone()),
    };
    drop(request);
    drop(input);
    assert_eq!(
        before - shared.input_bytes.lock().unwrap().remaining[0],
        4096 + HISTORICAL_REPLY_BYTES
    );
    assert!(!execute_store_result(sender, || Err::<(), _>(
        OperationJournalStoreError::HistoricalClaimObservation {
            reason: "actual late reply".into()
        }
    )));
    assert_eq!(
        before - shared.input_bytes.lock().unwrap().remaining[0],
        4096 + HISTORICAL_REPLY_BYTES
    );
    drop(backing);
    assert_eq!(shared.input_bytes.lock().unwrap().remaining[0], before);
}

#[test]
fn historical_claim_earliest_consumer_refusal_keeps_precharged_reply_through_delivery() {
    let fixture = fixture("historical-claim-earliest-refusal");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
    wait_until_available(&owner);
    let limit = InputClass::Ordinary.limit();
    let retained = owner
        .shared
        .reserve_input(InputClass::Ordinary, limit - 64 * 1024)
        .unwrap();
    let mut request = owner
        .client()
        .admit_prepared(prepared_copy_input(
            FullCapacityAllocationClass::DestinationStaging,
            VolumeIdentity { device: 1 },
            1,
        ))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !request
        .historical_reply
        .as_ref()
        .unwrap()
        .consumer_refused
        .load(Ordering::Acquire)
    {
        assert!(
            Instant::now() < deadline,
            "genuine actor never reached consumer funding refusal"
        );
        std::thread::yield_now();
    }
    assert!(owner.shutdown());
    assert_eq!(
        limit - owner.shared.input_bytes.lock().unwrap().remaining[0],
        retained.bytes + HISTORICAL_REPLY_BYTES
    );
    assert_eq!(owner.prepared_admission_id_allocations(), 0);
    let Err(OperationJournalOwnerError::PreparedAdmission(
        PreparedAdmissionError::RecoveryRequired { reason },
    )) = request.recv_timeout(Duration::from_secs(5)).unwrap()
    else {
        panic!("changed original funding refusal")
    };
    let expected = OperationJournalStoreError::HistoricalClaimResourceBound.to_string();
    assert_eq!(reason, expected);
    assert_eq!(HISTORICAL_REFUSAL_REPLY_BYTES, 2 * expected.len());
    assert_eq!(
        limit - owner.shared.input_bytes.lock().unwrap().remaining[0],
        retained.bytes
    );
    drop(retained);
    assert_eq!(owner.shared.input_bytes.lock().unwrap().remaining[0], limit);
}

#[test]
fn historical_claim_concurrent_last_reply_aliases_refund_exact_original_pool() {
    // Exercise actual concurrent final aliases, not a single-owner try_unwrap path.
    for _ in 0..32 {
        let (senders, _receivers) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(senders));
        let before = shared.input_bytes.lock().unwrap().remaining[0];
        let backing = HistoricalReply::new(HistoricalReplyBacking {
            consumer: Mutex::new(Some(
                shared.reserve_input(InputClass::Ordinary, 1024).unwrap(),
            )),
            observer: Mutex::new(Some(HistoricalObserverCredits {
                _sql: shared.reserve_input(InputClass::Ordinary, 2048).unwrap(),
                _parser: None,
                _envelope: None,
                _sqlite_error: None,
            })),
            consumer_refused: std::sync::atomic::AtomicBool::new(false),
            _header: shared
                .reserve_input(InputClass::Ordinary, HISTORICAL_REPLY_BYTES)
                .unwrap(),
        });
        let barrier = Arc::new(std::sync::Barrier::new(5));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let alias = backing.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    drop(alias);
                })
            })
            .collect();
        drop(backing);
        assert_eq!(
            before - shared.input_bytes.lock().unwrap().remaining[0],
            3072 + HISTORICAL_REPLY_BYTES
        );
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(shared.input_bytes.lock().unwrap().remaining[0], before);
    }
}

#[test]
fn recovery_funded_actual_queued_admission_error_lost_reply_and_teardown() {
    for lost in [false, true] {
        let fixture = fixture("recovery-audit-original-queued-error");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let database = fixture
            .owner
            .profile_root()
            .join("operation_journal/journal.sqlite3");
        let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
        wait_until_available(&owner);
        let operation = OperationId::new();
        let wire = format!(r#"{{"schema_version":1,"operation_id":"{operation}","intent":{{"actor":"User","kind":"FileHistory","label":"recovery-audit"}},"phase":"IntentDurable","disposition":"None","payload":null,"created_unix_ms":1,"updated_unix_ms":1}}"#).into_bytes();
        let mut install = owner
            .client()
            .submit(
                wire.len() + std::mem::size_of::<JournalCommand>(),
                move |input, result| JournalCommand::InstallRecoveryAuditFixture {
                    operation,
                    wire,
                    corrupt_text_bytes: MAX_RECORD_BYTES,
                    input,
                    result,
                },
            )
            .unwrap();
        install
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        drop(install);
        let mut request = owner
            .client()
            .admit_prepared(prepared_copy_input(
                FullCapacityAllocationClass::DestinationStaging,
                VolumeIdentity { device: 1 },
                1,
            ))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while request
            .historical_reply
            .as_ref()
            .unwrap()
            .consumer
            .lock()
            .unwrap()
            .is_none()
        {
            assert!(
                Instant::now() < deadline,
                "original recovery error was never retained"
            );
            std::thread::yield_now();
        }
        assert_eq!(owner.prepared_admission_id_allocations(), 0);
        if lost {
            drop(request);
            assert!(owner.shutdown());
        } else {
            assert!(owner.shutdown());
            let Err(OperationJournalOwnerError::PreparedAdmission(
                PreparedAdmissionError::RecoveryRequired { reason },
            )) = request.recv_timeout(Duration::from_secs(5)).unwrap()
            else {
                panic!("original recovery error changed");
            };
            assert!(
                reason.contains("recovery audit closed field is invalid"),
                "{reason}"
            );
            drop(reason);
            drop(request);
        }
        assert_eq!(
            owner.shared.input_bytes.lock().unwrap().remaining[0],
            InputClass::Ordinary.limit()
        );
        let connection = rusqlite::Connection::open(database).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM operation_journal", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM operation_journal_ordinary_claim",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT length(purpose) FROM operation_journal_recovery_reserve",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            MAX_RECORD_BYTES as i64
        );
    }
}

#[test]
fn recovery_funded_actual_queued_exact_audit_pressure_refuses_without_effects() {
    let fixture = fixture("recovery-audit-exact-pressure");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let database = fixture
        .owner
        .profile_root()
        .join("operation_journal/journal.sqlite3");
    let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
    wait_until_available(&owner);
    let operation = OperationId::new();
    let wire=format!(r#"{{"schema_version":1,"operation_id":"{operation}","intent":{{"actor":"User","kind":"FileHistory","label":"recovery-audit"}},"phase":"IntentDurable","disposition":"None","payload":null,"created_unix_ms":1,"updated_unix_ms":1}}"#).into_bytes();
    let mut install = owner
        .client()
        .submit(
            wire.len() + std::mem::size_of::<JournalCommand>(),
            move |input, result| JournalCommand::InstallRecoveryAuditFixture {
                operation,
                wire,
                corrupt_text_bytes: 0,
                input,
                result,
            },
        )
        .unwrap();
    install
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    drop(install);
    owner
        .shared
        .recovery_audit_pause
        .store(true, Ordering::Release);
    let mut request = owner
        .client()
        .admit_prepared(prepared_copy_input(
            FullCapacityAllocationClass::DestinationStaging,
            VolumeIdentity { device: 1 },
            1,
        ))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !owner.shared.recovery_audit_entered.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "original admission never reached recovery audit"
        );
        std::thread::yield_now();
    }
    // Previous ordinary reconstruction and consumer headers are already live.
    // Exhaust the SAME original ordinary pool only at this audit boundary.
    let remaining = owner.shared.input_bytes.lock().unwrap().remaining[0];
    let retained = owner
        .shared
        .reserve_input(InputClass::Ordinary, remaining - 1)
        .unwrap();
    owner
        .shared
        .recovery_audit_pause
        .store(false, Ordering::Release);
    let Err(OperationJournalOwnerError::PreparedAdmission(
        PreparedAdmissionError::RecoveryRequired { reason },
    )) = request.recv_timeout(Duration::from_secs(5)).unwrap()
    else {
        panic!("audit pressure did not refuse");
    };
    assert!(
        reason.contains("historical claim observation resource envelope exceeded"),
        "{reason}"
    );
    drop(reason);
    drop(request);
    drop(retained);
    assert!(owner.shutdown());
    assert_eq!(owner.prepared_admission_id_allocations(), 0);
    assert_eq!(
        owner.shared.input_bytes.lock().unwrap().remaining[0],
        InputClass::Ordinary.limit()
    );
    let connection = rusqlite::Connection::open(database).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM operation_journal", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM operation_journal_ordinary_claim",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM operation_journal_recovery_reserve_transition",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        4
    );
}
