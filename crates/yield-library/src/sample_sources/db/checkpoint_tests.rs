use super::*;
use crate::app_dirs::{AppRootGuard, WritableProfileGuard};

#[test]
fn owned_source_post_commit_hook_uses_retained_wal_and_rejects_active_transaction() {
    let app_root = tempfile::tempdir().unwrap();
    let source_root = tempfile::tempdir().unwrap();
    let _app_root = AppRootGuard::set(std::fs::canonicalize(app_root.path()).unwrap()).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &guard,
        "checkpoint-test",
    )
    .unwrap();
    database
        .connection
        .execute_batch(
            "PRAGMA wal_autocheckpoint=0;
        CREATE TABLE checkpoint_payload(value BLOB);
        INSERT INTO checkpoint_payload VALUES (zeroblob(34603008));",
        )
        .unwrap();
    let bytes = crate::sqlite_wal::retained_wal_bytes(&database.connection).unwrap();
    assert!(bytes >= crate::sqlite_wal::WAL_SOFT_WATERMARK_BYTES);
    database
        .connection
        .execute_batch("BEGIN IMMEDIATE")
        .unwrap();
    database.maybe_checkpoint_owned_wal();
    assert!(!database.checkpoint_policy.has_attempted());
    database.connection.execute_batch("ROLLBACK").unwrap();
    database.maybe_checkpoint_owned_wal();
    assert!(database.checkpoint_policy.has_attempted());
    let frames: (i64, i64, i64) = database
        .connection
        .query_row("PRAGMA main.wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(frames.0, 0);
    assert_eq!(frames.1, frames.2);
    assert!(crate::sqlite_wal::retained_wal_bytes(&database.connection).unwrap() > 0);
    assert_eq!(
        database
            .connection
            .query_row("SELECT length(value) FROM checkpoint_payload", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        34603008
    );
}
