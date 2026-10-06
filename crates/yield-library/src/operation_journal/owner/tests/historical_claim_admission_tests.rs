use super::*;
use crate::operation_journal::FullCapacityAllocationClass;

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
    let sql = rusqlite::Connection::open(&database).unwrap();
    sql.execute(
        "INSERT INTO operation_journal VALUES (?1,1,?2)",
        rusqlite::params![old_id.to_string(), wire.as_bytes()],
    )
    .unwrap();
    sql.execute(
        "INSERT INTO operation_journal_ordinary_claim VALUES (?1,?2,'1','DestinationStaging',1)",
        rusqlite::params![Uuid::new_v4().to_string(), old_id.to_string()],
    )
    .unwrap();
    drop(sql);
    let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
    wait_until_available(&owner);
    let pool_before = owner.shared.input_bytes.lock().unwrap().remaining[0];
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
    assert_eq!(
        owner.shared.input_bytes.lock().unwrap().remaining[0],
        pool_before
    );
    assert!(owner.shutdown());
    let sql = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(
        sql.query_row("SELECT count(*) FROM operation_journal", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        sql.query_row("SELECT record FROM operation_journal", [], |row| row
            .get::<_, Vec<u8>>(0))
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
fn historical_claim_unconsumed_actor_error_keeps_original_scratch_until_receiver_drop() {
    let fixture = fixture("historical-claim-unconsumed-error");
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let database = fixture
        .owner
        .profile_root()
        .join("operation_journal/journal.sqlite3");
    let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
    wait_until_available(&owner);
    let pool_before = owner.shared.input_bytes.lock().unwrap().remaining[0];
    let old_id = OperationId::new();
    // Introduce a malformed historical row after genuine startup. The actor must
    // retain its original typed error rather than allocating an operation identity.
    let sql = rusqlite::Connection::open(&database).unwrap();
    let wire = format!("{{{}", " ".repeat(MAX_RECORD_BYTES - 1));
    sql.execute(
        "INSERT INTO operation_journal VALUES (?1,1,?2)",
        rusqlite::params![old_id.to_string(), wire.as_bytes()],
    )
    .unwrap();
    sql.execute(
        "INSERT INTO operation_journal_ordinary_claim VALUES (?1,?2,'1','DestinationStaging',1)",
        rusqlite::params![Uuid::new_v4().to_string(), old_id.to_string()],
    )
    .unwrap();
    drop(sql);
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
            .terminal
            .lock()
            .unwrap()
            .iter()
            .any(Option::is_some);
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
    assert!(pool_before - owner.shared.input_bytes.lock().unwrap().remaining[0] >= 2 * wire.len());
    assert_eq!(owner.prepared_admission_id_allocations(), 0);
    drop(request);
    assert_eq!(
        owner.shared.input_bytes.lock().unwrap().remaining[0],
        pool_before
    );
    let sql = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(
        sql.query_row("SELECT record FROM operation_journal", [], |row| row
            .get::<_, Vec<u8>>(0))
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
        .reserve_input(
            InputClass::Ordinary,
            std::mem::size_of::<HistoricalReplyBacking>() + 2 * std::mem::size_of::<usize>(),
        )
        .unwrap();
    let backing = Arc::new(HistoricalReplyBacking {
        _header: header,
        terminal: Mutex::new([None, None, None]),
    });
    let mut input = shared.reserve_input(InputClass::Ordinary, 1).unwrap();
    input.historical_reply = Some(Arc::clone(&backing));
    let scratch = input.reserve_historical_scratch(4096).unwrap();
    input.retain_historical_error([Some(scratch), None, None]);
    let (sender, receiver) = mpsc::sync_channel::<Result<(), OperationJournalOwnerError>>(1);
    let result_permit = shared.reserve_result(InputClass::Ordinary).unwrap();
    let request = OperationJournalRequest {
        receiver: Some(receiver),
        result_permit: Some(result_permit),
        historical_reply: Some(Arc::clone(&backing)),
    };
    drop(request);
    drop(input);
    assert!(before - shared.input_bytes.lock().unwrap().remaining[0] >= 4096);
    assert!(!execute_store_result(sender, || Err::<(), _>(
        OperationJournalStoreError::HistoricalClaimObservation {
            reason: "actual late reply".into()
        }
    )));
    assert!(before - shared.input_bytes.lock().unwrap().remaining[0] >= 4096);
    drop(backing);
    assert_eq!(shared.input_bytes.lock().unwrap().remaining[0], before);
}
