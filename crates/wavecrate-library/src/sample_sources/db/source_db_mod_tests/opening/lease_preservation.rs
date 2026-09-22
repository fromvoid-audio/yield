use super::*;
use crate::app_dirs::{AppRootGuard, WritableProfileGuard};
use rusqlite::types::Value;

#[test]
fn source_database_identity_first_lease_preserves_user_and_recovery_state() {
    let app_root = tempdir().expect("app root");
    let media_root = tempdir().expect("media root");
    let database_root = tempdir().expect("database root");
    let _app_root =
        AppRootGuard::set(std::fs::canonicalize(app_root.path()).expect("canonical app root"))
            .expect("app root override");
    let profile = WritableProfileGuard::acquire_current().expect("profile owner");
    let legacy = SourceDatabase::open_for_source_write(database_root.path())
        .expect("legacy source fixture without a physical lease");
    legacy.connection.execute_batch(
        "INSERT INTO wav_files
            (path, file_size, modified_ns, tag, locked, last_played_at, last_curated_at, user_tag)
            VALUES ('keep.wav', 42, 123, 3, 1, 456, 789, 'Keep label');
         INSERT INTO source_tags (id, normalized_text, display_label)
            VALUES (17, 'keep tag', 'Keep Tag');
         INSERT INTO wav_file_tags VALUES ('keep.wav', 17);
         INSERT INTO wav_file_collections VALUES ('keep.wav', 4);
         INSERT INTO source_readiness_sources
            (source_id, source_generation, readiness_revision, availability, updated_at)
            VALUES ('source', 8, 12, 'active', 100);
         INSERT INTO source_readiness_targets
            (source_id, scope_kind, scope_id, relative_path, stage, required_version,
             source_generation, content_generation, eligibility, updated_at)
            VALUES ('source', 'file', 'sample', 'keep.wav', 'analysis_features', 'v1',
                    8, 'hash', 'eligible', 100);
         INSERT INTO source_readiness_artifacts
            (source_id, scope_kind, scope_id, relative_path, stage, artifact_version,
             source_generation, content_generation, artifact_ref, completed_at)
            VALUES ('source', 'file', 'sample', 'keep.wav', 'analysis_features', 'v1',
                    8, 'hash', 'retained-artifact', 99);
         INSERT INTO analysis_jobs
            (sample_id, source_id, relative_path, job_type, status, created_at,
             readiness_managed, readiness_claim_generation, lease_expires_at,
             readiness_scope_kind, readiness_scope_id, readiness_stage, artifact_version,
             source_generation, content_generation, running_at)
            VALUES ('sample', 'source', 'keep.wav', 'readiness_analysis_features_v1', 'running',
                    90, 1, 6, 120, 'file', 'sample', 'analysis_features', 'v1', 8, 'hash', 100);
         INSERT INTO pending_wav_renames
            (path, file_size, modified_ns, content_hash, tag, looped, locked,
             normal_tags, collections, staged_generation, staged_at)
            VALUES ('old.wav', 42, 123, 'hash', 3, 1, 1, '[\"Keep Tag\"]', '[4]', 8, 100);
         INSERT INTO pending_wav_rename_destinations VALUES ('new.wav', 8, 'hash');
         INSERT INTO file_ops_journal
            (id, op_type, stage, source_relative, target_relative, staged_relative, created_at)
            VALUES ('pending-operation', 'move', 'staged', 'old.wav', 'new.wav', 'staged.wav', 100);"
    ).expect("representative user, readiness and recovery state");
    let tables = [
        "wav_files",
        "source_tags",
        "wav_file_tags",
        "wav_file_collections",
        "source_readiness_sources",
        "source_readiness_targets",
        "source_readiness_artifacts",
        "analysis_jobs",
        "pending_wav_renames",
        "pending_wav_rename_destinations",
        "file_ops_journal",
    ];
    let snapshot = |connection: &Connection| {
        tables
            .iter()
            .map(|table| {
                let mut statement = connection
                    .prepare(&format!("SELECT * FROM {table}"))
                    .expect("retained table");
                let columns = statement.column_count();
                statement
                    .query_map([], |row| {
                        (0..columns)
                            .map(|index| row.get::<_, Value>(index))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .expect("read state")
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .expect("state rows")
            })
            .collect::<Vec<_>>()
    };
    let before = snapshot(&legacy.connection);
    assert!(before.iter().all(|rows| rows.len() == 1));
    let pending_before = legacy
        .list_pending_renames()
        .expect("read pending recovery");
    assert_eq!(pending_before[0].metadata.normal_tags, ["Keep Tag"]);
    drop(legacy);

    let path = database_root.path().join(DB_FILE_NAME);
    let read_only = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("old read-only source remains usable");
    assert_eq!(snapshot(&read_only), before);
    assert_eq!(
        read_only
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'source_lease'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("lease absent before ownership"),
        0
    );
    drop(read_only);

    let owned = SourceDatabase::open_for_source_maintenance_with_profile_guard(
        media_root.path(),
        &profile,
        database_root.path(),
    )
    .expect("first physical source lease");
    assert_eq!(snapshot(&owned.connection), before);
    let pending_after = owned
        .list_pending_renames()
        .expect("retained pending recovery");
    assert_eq!(pending_after[0].metadata.normal_tags, ["Keep Tag"]);
    assert_eq!(
        owned
            .connection
            .query_row(
                "SELECT lease_epoch FROM source_lease WHERE singleton = 1 AND state = 'active'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("new lease"),
        1
    );
    drop(owned);
    let read_only = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("read-only source after release");
    assert_eq!(snapshot(&read_only), before);
}
