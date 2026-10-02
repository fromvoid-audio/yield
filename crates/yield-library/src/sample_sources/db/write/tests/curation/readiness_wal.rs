//! Genuine retained SourceWriter/SQLite claims; not physical/native qualification.
use super::*;
use crate::sample_sources::readiness::ReadinessError;
use crate::{WAL_HARD_WATERMARK_BYTES, WalAdmissionError};
use rusqlite::Connection;

fn jobs(writer: &SourceWriter) -> Vec<(i64, String, i64, i64, Option<i64>, Option<i64>)> {
    writer
        .catalog_database()
        .connection
        .prepare(
            "SELECT id,status,readiness_claim_generation,attempts,lease_expires_at,retry_at
         FROM analysis_jobs ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn pressure(writer: &SourceWriter) -> Connection {
    let reader = Connection::open_with_flags(
        &writer.catalog_database().db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM analysis_jobs")
        .unwrap();
    // Finite test-only genuine SQLite frames, retained by the real external reader. No size
    // injection or sparse WAL surrogate. This setup creates no capacity/reserve authority.
    writer
        .catalog_database()
        .connection
        .execute(
            "INSERT INTO metadata(key,value) VALUES ('claim-wal-pressure',zeroblob(?1))",
            [WAL_HARD_WATERMARK_BYTES as i64],
        )
        .unwrap();
    reader
}

fn maintain(writer: &SourceWriter, reader: Connection) {
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    writer
        .catalog_database()
        .connection
        .execute("DELETE FROM metadata WHERE key='claim-wal-pressure'", [])
        .unwrap();
    writer
        .catalog_database()
        .connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
}

fn hard<T: std::fmt::Debug>(result: Result<T, SourceWriterError>) {
    assert!(
        matches!(result, Err(SourceWriterError::Readiness(ReadinessError::WalAdmission(
        WalAdmissionError::HardWatermark { observed_bytes, hard_bytes }
    ))) if observed_bytes >= hard_bytes && hard_bytes == WAL_HARD_WATERMARK_BYTES),
        "{result:?}"
    );
}

#[test]
fn readiness_wal_pending_claim_rejects_without_running_or_generation_then_retries() {
    let (mut fixture, target) = super::feature_artifacts::prepare();
    let mut writer = super::feature_artifacts::owner(&mut fixture);
    let old_revision = writer.current_revision().unwrap();
    writer
        .commit_catalog_curation_command(
            &old_revision,
            &fixture.profile_guard,
            &[SourceCurationUpdate::SetRating {
                path: CurationFixture::path("one.wav"),
                rating: Rating::KEEP_3,
            }],
        )
        .unwrap();
    let revision = writer.current_revision().unwrap();
    let reader = pressure(&writer);
    let before = jobs(&writer);
    let changes = writer.catalog_database().connection.total_changes();
    hard(writer.claim_readiness_target(&revision, &target, 10, 100));
    hard(writer.claim_feature_artifact(&revision, &target, 10, 100));
    assert_eq!(jobs(&writer), before);
    assert_eq!(
        writer.catalog_database().connection.total_changes(),
        changes
    );
    assert_eq!(writer.current_revision().unwrap(), revision);
    assert!(matches!(
        writer.claim_readiness_target(&old_revision, &target, 10, 100),
        Err(SourceWriterError::StaleRevision { .. })
    ));
    let mut foreign = target.clone();
    foreign.source_id = "foreign-owner".into();
    assert!(matches!(
        writer.claim_readiness_target(&revision, &foreign, 10, 100),
        Err(SourceWriterError::StaleFence)
    ));
    let old_pressure: i64 = reader
        .query_row(
            "SELECT count(*) FROM metadata WHERE key='claim-wal-pressure'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_pressure, 0, "reader snapshot stays intact");
    maintain(&writer, reader);
    let claim = writer
        .claim_readiness_target(&revision, &target, 10, 100)
        .unwrap()
        .unwrap();
    assert_eq!(claim.claim_generation(), 1);
}

#[test]
fn readiness_wal_live_no_op_and_expired_reclaim_preserve_existing_generation() {
    let (mut fixture, target) = super::feature_artifacts::prepare();
    let mut writer = super::feature_artifacts::owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let first = writer
        .claim_readiness_target(&revision, &target, 10, 10)
        .unwrap()
        .unwrap();
    assert_eq!(first.claim_generation(), 1);
    let reader = pressure(&writer);
    let before = jobs(&writer);
    assert!(
        writer
            .claim_readiness_target(&revision, &target, 15, 10)
            .unwrap()
            .is_none()
    );
    hard(writer.claim_readiness_target(&revision, &target, 20, 10));
    assert_eq!(jobs(&writer), before);
    maintain(&writer, reader);
    let next = writer
        .claim_readiness_target(&revision, &target, 20, 10)
        .unwrap()
        .unwrap();
    assert_eq!(next.claim_generation(), 2);
}

#[test]
fn readiness_wal_due_retry_retains_failure_attempts_and_retry_deadline_on_rejection() {
    let (mut fixture, target) = super::feature_artifacts::prepare();
    let mut writer = super::feature_artifacts::owner(&mut fixture);
    let revision = writer.current_revision().unwrap();
    let first = writer
        .claim_readiness_target(&revision, &target, 10, 100)
        .unwrap()
        .unwrap();
    writer
        .fail_readiness_work(
            &revision,
            &first,
            ReadinessFailureClassification::Retryable,
            "fixture",
            "retry",
            11,
            ReadinessRetryPolicy::new(5, 10, 3).unwrap(),
        )
        .unwrap();
    let reader = pressure(&writer);
    let before = jobs(&writer);
    assert!(
        writer
            .claim_readiness_target(&revision, &target, 12, 100)
            .unwrap()
            .is_none()
    );
    hard(writer.claim_readiness_target(&revision, &target, 16, 100));
    assert_eq!(jobs(&writer), before);
    maintain(&writer, reader);
    let next = writer
        .claim_readiness_target(&revision, &target, 16, 100)
        .unwrap()
        .unwrap();
    assert_eq!(next.claim_generation(), 2);
}

#[test]
fn readiness_wal_replaced_owner_rejects_old_context_before_pressure_changes_jobs() {
    let (mut fixture, target) = super::feature_artifacts::prepare();
    let writer = super::feature_artifacts::owner(&mut fixture);
    let old_revision = writer.current_revision().unwrap();
    let before = jobs(&writer);
    drop(writer);
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        Path::new("curation-profile-bound"),
    )
    .unwrap();
    let mut replacement =
        SourceWriter::new(database, SourceId::from_string("feature-owner"), 8).unwrap();
    let reader = pressure(&replacement);
    assert!(matches!(
        replacement.claim_readiness_target(&old_revision, &target, 10, 100),
        Err(SourceWriterError::StaleFence)
    ));
    assert_eq!(jobs(&replacement), before);
    maintain(&replacement, reader);
    let current = replacement.current_revision().unwrap();
    assert!(
        replacement
            .claim_readiness_target(&current, &target, 10, 100)
            .unwrap()
            .is_some()
    );
}

mod vfs;
