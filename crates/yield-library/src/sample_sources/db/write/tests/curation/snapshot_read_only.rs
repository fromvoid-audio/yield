//! Actual retained-owner snapshot checks with writes and checkpoint SQL forbidden.

use super::*;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::time::{Duration, Instant};

/// Reject mutations and maintenance without interfering with read transactions/schema reads.
fn authorize_snapshot(context: AuthContext<'_>) -> Authorization {
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

/// Capture persisted database/WAL bytes while the existing owner remains open.
fn persisted_bytes(path: &Path) -> (Vec<u8>, Vec<u8>) {
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    (std::fs::read(path).unwrap(), std::fs::read(wal).unwrap())
}

#[test]
fn snapshot_read_only_wal_pages_forbid_sql_writes_and_checkpoint() {
    let fixture = CurationFixture::new_profile_bound();
    let db_path = fixture.database.connection.path().unwrap().to_owned();
    let mut writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("snapshot-read-only-wal"),
        7,
    )
    .unwrap();
    writer
        .database_for_tests()
        .connection
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    let original = writer.current_revision().unwrap();
    let request = SourceWriterRequest::new(
        crate::operation_journal::OperationId::new(),
        crate::operation_journal::ParticipantId::new(),
        SourceCommandId::new(),
        original.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: CurationFixture::path("one.wav"),
            locked: true,
        }],
    )
    .unwrap();
    writer.commit_curation(request).unwrap();
    let expected = writer.current_revision().unwrap();
    let database = writer.database_for_tests();
    let before = persisted_bytes(Path::new(&db_path));
    assert!(!before.1.is_empty(), "committed state is still in WAL");
    let changes = database.connection.total_changes();
    database
        .connection
        .pragma_update(None, "query_only", true)
        .unwrap();
    database.connection.authorizer(Some(authorize_snapshot));
    // Negative controls prove the same connection cannot mutate or checkpoint.
    assert!(
        database
            .connection
            .execute("UPDATE metadata SET value = value", [])
            .is_err()
    );
    assert!(
        database
            .connection
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE)")
            .is_err()
    );
    assert!(
        database
            .connection
            .execute_batch("CREATE TABLE snapshot_bypass(id)")
            .is_err()
    );
    let owner = SourceSnapshotOwner::start(writer).unwrap();
    let first = owner
        .read_browser_metadata_page(&expected, None, 1)
        .unwrap();
    let SourceSnapshotPage::FreshAsOf { revision, page } = first else {
        panic!("WAL-resident page must be fresh")
    };
    assert_eq!(revision, expected);
    assert_eq!(page.files.len(), 1);
    assert_eq!(page.files[0].relative_path, Path::new("one.wav"));
    assert!(page.files[0].locked);
    let cursor = page.next_cursor.unwrap();
    let next = owner
        .read_browser_metadata_page(&expected, Some(&cursor), usize::MAX)
        .unwrap();
    let SourceSnapshotPage::FreshAsOf { page, .. } = next else {
        panic!("continuation must be fresh")
    };
    assert_eq!(page.files.len(), 1);
    assert_eq!(page.files[0].relative_path, Path::new("two.wav"));
    assert!(matches!(
        owner.read_browser_metadata_page(&original, Some(&cursor), 1).unwrap(),
        SourceSnapshotPage::Stale { current } if current == expected
    ));
    let writer = owner.begin_retirement().join().unwrap();
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        changes
    );
    assert_eq!(persisted_bytes(Path::new(&db_path)), before);
    assert!(writer.database_for_tests().connection.is_autocommit());
    writer
        .database_for_tests()
        .connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    writer
        .database_for_tests()
        .connection
        .pragma_update(None, "query_only", false)
        .unwrap();
    assert_eq!(writer.current_revision().unwrap(), expected);
    writer.close().unwrap();
}

#[test]
fn snapshot_read_only_expired_request_does_not_take_pending_worker_slot() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("snapshot-expired-slot"),
        7,
    )
    .unwrap();
    let expected = writer.current_revision().unwrap();
    let owner = SourceSnapshotOwner::start(writer).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    owner.block_for_tests(entered_tx, release_rx);
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(matches!(
        owner.read_browser_metadata_page_with_deadline(
            &expected,
            None,
            1,
            Instant::now() - Duration::from_secs(1)
        ),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    // A valid request still has the pending slot and waits for its own deadline.
    let started = Instant::now();
    assert!(matches!(
        owner.read_browser_metadata_page_with_deadline(
            &expected,
            None,
            1,
            started + Duration::from_millis(100)
        ),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    assert!(started.elapsed() >= Duration::from_millis(90));
    release_tx.send(()).unwrap();
    owner.wait_idle_for_tests();
    assert!(matches!(
        owner
            .read_browser_metadata_page(&expected, None, 1)
            .unwrap(),
        SourceSnapshotPage::FreshAsOf { .. }
    ));
    owner.begin_retirement().join().unwrap().close().unwrap();
}

#[test]
fn snapshot_read_only_rejects_expired_lease_without_renewing_or_writing() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("snapshot-expired-lease"),
        7,
    )
    .unwrap();
    let expected = writer.current_revision().unwrap();
    let database = writer.database_for_tests();
    database
        .connection
        .execute(
            "UPDATE source_lease SET expires_at = acquired_at WHERE singleton = 1",
            [],
        )
        .unwrap();
    let changes = database.connection.total_changes();
    let before = persisted_bytes(Path::new(database.connection.path().unwrap()));
    database
        .connection
        .pragma_update(None, "query_only", true)
        .unwrap();
    database.connection.authorizer(Some(authorize_snapshot));
    let owner = SourceSnapshotOwner::start(writer).unwrap();
    assert!(matches!(
        owner.read_browser_metadata_page(&expected, None, 64),
        Err(SourceWriterError::Database(SourceDbError::SourceLease(
            crate::sample_sources::db::SourceLeaseError::Expired { .. }
        )))
    ));
    let writer = owner.begin_retirement().join().unwrap();
    let database = writer.database_for_tests();
    assert_eq!(database.connection.total_changes(), changes);
    assert_eq!(
        persisted_bytes(Path::new(database.connection.path().unwrap())),
        before
    );
    database
        .connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    database
        .connection
        .pragma_update(None, "query_only", false)
        .unwrap();
    writer.close().unwrap();
}
