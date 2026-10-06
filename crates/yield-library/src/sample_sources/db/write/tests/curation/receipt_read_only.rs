//! Historical receipts use the original retained owner, without mutation admission.
use super::*;
use crate::sample_sources::SourceWriterReceipt;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::time::Duration;

fn prepared(database: SourceDatabase) -> (SourceWriter, SourceWriterReceipt) {
    let mut writer = SourceWriter::new(database, SourceId::from_string("receipt-read-only"), 7)
        .expect("original profile-bound writer");
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::new(),
        crate::operation_journal::ParticipantId::new(),
        SourceCommandId::new(),
        writer.current_revision().unwrap(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .unwrap();
    let receipt = writer
        .commit_curation(request)
        .expect("actual committed receipt");
    (writer, receipt)
}

fn authorize_read(context: AuthContext<'_>) -> Authorization {
    match context.action {
        AuthAction::Read { .. }
        | AuthAction::Select
        | AuthAction::Transaction { .. }
        | AuthAction::Function { .. }
        | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Pragma { pragma_name, .. }
            if pragma_name.eq_ignore_ascii_case("table_info") =>
        {
            Authorization::Allow
        }
        _ => Authorization::Deny,
    }
}

fn persistent_bytes(path: &str) -> (Vec<u8>, Vec<u8>) {
    let mut wal = PathBuf::from(path).into_os_string();
    wal.push("-wal");
    (std::fs::read(path).unwrap(), std::fs::read(wal).unwrap())
}

#[test]
fn receipt_read_only_near_expiry_preserves_original_lease_and_persistent_bytes() {
    let fixture = CurationFixture::new_profile_bound();
    let (writer, receipt) = prepared(fixture.database);
    let connection = &writer.database_for_tests().connection;
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    connection
        .execute(
            "UPDATE source_lease SET expires_at=?1 WHERE singleton=1",
            [now + 30],
        )
        .unwrap();
    let path = connection.path().unwrap().to_owned();
    let before = persistent_bytes(&path);
    let changes = connection.total_changes();
    connection.pragma_update(None, "query_only", true).unwrap();
    connection.authorizer(Some(authorize_read));
    assert!(
        connection
            .execute("UPDATE metadata SET value=value", [])
            .is_err()
    );
    assert!(
        connection
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE)")
            .is_err()
    );
    writer
        .current_revision()
        .expect("ordinary original-owner validation is read-only");
    let observed = writer.read_receipt_observation(receipt.identity());
    assert_eq!(observed.unwrap(), Some(receipt));
    let expiry: i64 = connection
        .query_row(
            "SELECT expires_at FROM source_lease WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        expiry,
        now + 30,
        "historical observation cannot renew ownership"
    );
    assert_eq!(connection.total_changes(), changes);
    assert_eq!(persistent_bytes(&path), before);
    connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    connection.pragma_update(None, "query_only", false).unwrap();
    writer.close().unwrap();
}

#[test]
fn receipt_read_only_coexists_with_actual_external_writer_reservation() {
    let fixture = CurationFixture::new_profile_bound();
    let (writer, receipt) = prepared(fixture.database);
    let connection = &writer.database_for_tests().connection;
    connection.busy_timeout(Duration::ZERO).unwrap();
    let external = rusqlite::Connection::open(connection.path().unwrap()).unwrap();
    external.busy_timeout(Duration::ZERO).unwrap();
    external.execute_batch("BEGIN IMMEDIATE").unwrap();
    let observed = writer.read_receipt_observation(receipt.identity());
    external.execute_batch("ROLLBACK").unwrap();
    assert_eq!(observed.unwrap(), Some(receipt));
    assert!(connection.is_autocommit());
    writer.close().unwrap();
}

#[test]
fn receipt_read_only_missing_and_malformed_rows_preserve_original_database() {
    let fixture = CurationFixture::new_profile_bound();
    let (writer, receipt) = prepared(fixture.database);
    let connection = &writer.database_for_tests().connection;
    let path = connection.path().unwrap().to_owned();
    let before = persistent_bytes(&path);
    let changes = connection.total_changes();
    let missing = SourceWriterRequest::new(
        crate::operation_journal::OperationId::new(),
        crate::operation_journal::ParticipantId::new(),
        SourceCommandId::new(),
        writer.current_revision().unwrap(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: false,
        }],
    )
    .unwrap();
    assert_eq!(
        writer.read_receipt_observation(missing.identity()).unwrap(),
        None
    );
    assert_eq!(connection.total_changes(), changes);
    assert_eq!(persistent_bytes(&path), before);
    connection.execute(
        "UPDATE source_participant_curation_receipts SET receipt_bytes=zeroblob(1048577), receipt_len=1048577",
        [],
    ).unwrap();
    let malformed_bytes = persistent_bytes(&path);
    let changes = connection.total_changes();
    assert!(matches!(
        writer.read_receipt_observation(receipt.identity()),
        Err(SourceWriterError::MalformedReceipt)
    ));
    assert!(connection.is_autocommit());
    assert_eq!(connection.total_changes(), changes);
    assert_eq!(persistent_bytes(&path), malformed_bytes);
    writer.close().unwrap();
}

#[test]
fn receipt_read_only_expired_original_lease_cannot_renew() {
    let fixture = CurationFixture::new_profile_bound();
    let (writer, receipt) = prepared(fixture.database);
    let connection = &writer.database_for_tests().connection;
    connection
        .execute(
            "UPDATE source_lease SET expires_at=acquired_at WHERE singleton=1",
            [],
        )
        .unwrap();
    let path = connection.path().unwrap().to_owned();
    let before = persistent_bytes(&path);
    let changes = connection.total_changes();
    assert!(matches!(
        writer.read_receipt_observation(receipt.identity()),
        Err(SourceWriterError::Database(SourceDbError::SourceLease(
            crate::sample_sources::db::SourceLeaseError::Expired { .. }
        )))
    ));
    assert_eq!(connection.total_changes(), changes);
    assert_eq!(persistent_bytes(&path), before);
    assert!(connection.is_autocommit());
    writer.close().unwrap();
}

#[test]
fn receipt_read_only_deadline_interrupts_actual_sql_and_restores_connection() {
    let fixture = CurationFixture::new_profile_bound();
    let (writer, receipt) = prepared(fixture.database);
    let connection = &writer.database_for_tests().connection;
    connection.execute_batch(
        "ALTER TABLE source_participant_curation_receipts RENAME TO retained_receipts;
         CREATE VIEW source_participant_curation_receipts AS
         SELECT r.* FROM retained_receipts r WHERE
             (WITH RECURSIVE work(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM work WHERE n<1000000000)
              SELECT sum(n) FROM work)>0;"
    ).unwrap();
    assert!(matches!(
        writer.read_receipt_observation_with_deadline(
            receipt.identity(),
            std::time::Instant::now() - Duration::from_secs(1)
        ),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    let changes = connection.total_changes();
    assert!(matches!(
        writer.read_receipt_observation_with_deadline(
            receipt.identity(),
            std::time::Instant::now() + Duration::from_millis(50)
        ),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    assert!(connection.is_autocommit());
    assert_eq!(connection.total_changes(), changes);
    let sum: i64 = connection.query_row(
        "WITH RECURSIVE work(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM work WHERE n<10000) SELECT sum(n) FROM work",
        [], |row| row.get(0)
    ).expect("expired progress handler was removed");
    assert_eq!(sum, 50_005_000);
    writer.current_revision().unwrap();
    connection
        .execute_batch(
            "DROP VIEW source_participant_curation_receipts;
         ALTER TABLE retained_receipts RENAME TO source_participant_curation_receipts;",
        )
        .unwrap();
    assert_eq!(
        writer.read_receipt_observation(receipt.identity()).unwrap(),
        Some(receipt)
    );
    writer.close().unwrap();
}

#[test]
fn receipt_read_only_revalidates_revision_after_releasing_snapshot() {
    use rusqlite::hooks::TransactionOperation;
    use std::sync::atomic::AtomicUsize;
    let fixture = CurationFixture::new_profile_bound();
    let (writer, receipt) = prepared(fixture.database);
    let connection = &writer.database_for_tests().connection;
    let before = writer.current_revision().unwrap();
    let external = rusqlite::Connection::open(connection.path().unwrap()).unwrap();
    external.busy_timeout(Duration::ZERO).unwrap();
    let rollbacks = Arc::new(AtomicUsize::new(0));
    let observed_rollbacks = rollbacks.clone();
    connection.authorizer(Some(move |context: AuthContext<'_>| {
        if matches!(context.action, AuthAction::Transaction { operation: TransactionOperation::Rollback })
            && observed_rollbacks.fetch_add(1, Ordering::SeqCst) == 1
        {
            external.execute(
                "UPDATE metadata SET value=?1 WHERE key IN ('revision', 'last_authoritative_audit_revision_v1')",
                [(before.revision()+1).to_string()],
            ).expect("real external WAL commit at original snapshot release");
        }
        Authorization::Allow
    }));
    assert!(matches!(
        writer.read_receipt_observation(receipt.identity()),
        Err(SourceWriterError::StaleFence)
    ));
    assert!(
        rollbacks.load(Ordering::SeqCst) >= 2,
        "original audit and receipt snapshot released"
    );
    connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    assert!(connection.is_autocommit());
    writer.current_revision().unwrap();
    writer.close().unwrap();
}
