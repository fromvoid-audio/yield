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
