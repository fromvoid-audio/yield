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
