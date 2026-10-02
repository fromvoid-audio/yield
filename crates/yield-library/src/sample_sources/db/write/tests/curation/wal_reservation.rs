//! Actual retained-source preparation; these tests issue no physical reserve authority.
use super::*;
use crate::sample_sources::db::write::{SourceWalPreparationError, SourceWalPreparationFailure};
use crate::{WAL_HARD_WATERMARK_BYTES, WalAdmissionError};
use rusqlite::{Connection, ErrorCode};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn fixture() -> (CurationFixture, SourceWriter) {
    let (mut fixture, _) = super::feature_artifacts::prepare();
    let writer = super::feature_artifacts::owner(&mut fixture);
    (fixture, writer)
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}
fn connection(writer: &SourceWriter) -> &Connection {
    &writer.catalog_database().connection
}
fn external(writer: &SourceWriter) -> Connection {
    let other = Connection::open_with_flags(
        &writer.catalog_database().db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();
    other
}
fn busy(error: rusqlite::Error) {
    assert_eq!(error.sqlite_error_code(), Some(ErrorCode::DatabaseBusy));
}
fn timeout(writer: &SourceWriter) -> u64 {
    connection(writer)
        .pragma_query_value(None, "busy_timeout", |r| r.get(0))
        .unwrap()
}
fn lease(connection: &Connection) -> (i64, i64, i64, String) {
    connection
        .query_row(
            "SELECT acquired_at,expires_at,lease_epoch,state FROM source_lease WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}
fn jobs(connection: &Connection) -> Vec<(i64, String, i64, i64, Option<i64>, Option<i64>)> {
    connection.prepare("SELECT id,status,attempts,readiness_claim_generation,lease_expires_at,retry_at FROM analysis_jobs ORDER BY id")
        .unwrap().query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))
        .unwrap().collect::<Result<_,_>>().unwrap()
}

#[test]
fn wal_reservation_excludes_external_writer_and_abort_reuses_exact_owned_connection() {
    let (fixture, writer) = fixture();
    let expected = writer.current_revision().unwrap();
    let before = source_writer_state_snapshot(writer.catalog_database());
    let jobs_before = jobs(connection(&writer));
    let lease_before = lease(connection(&writer));
    let other = external(&writer);
    connection(&writer)
        .busy_timeout(Duration::from_millis(913))
        .unwrap();
    let preparation = writer
        .prepare_wal_reservation(&expected, deadline())
        .unwrap();
    assert_eq!(preparation.revision(), &expected);
    assert!(!preparation.nonce().is_nil());
    busy(other.execute_batch("BEGIN IMMEDIATE").unwrap_err());
    // The fixture's observer is genuinely read-only; the reservation changes no job/lease state.
    assert_eq!(jobs(&fixture.database.connection), jobs_before);
    assert_eq!(lease(&fixture.database.connection), lease_before);
    let writer = preparation.abort().unwrap();
    assert_eq!(timeout(&writer), 913);
    assert!(connection(&writer).is_autocommit());
    assert_eq!(
        source_writer_state_snapshot(writer.catalog_database()),
        before
    );
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    writer
        .commit_catalog_curation_command(
            &expected,
            &fixture.profile_guard,
            &[SourceCurationUpdate::SetLocked {
                path: CurationFixture::path("one.wav"),
                locked: true,
            }],
        )
        .unwrap();
    assert_eq!(
        writer.current_revision().unwrap().revision(),
        expected.revision() + 1
    );
}

#[test]
fn wal_reservation_busy_attempt_never_waits_or_changes_timeout_rows_or_revision() {
    let (_fixture, writer) = fixture();
    let expected = writer.current_revision().unwrap();
    let before = source_writer_state_snapshot(writer.catalog_database());
    connection(&writer)
        .busy_timeout(Duration::from_secs(9))
        .unwrap();
    let other = external(&writer);
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = Instant::now();
    let failure = writer
        .prepare_wal_reservation(&expected, deadline())
        .unwrap_err();
    assert!(
        matches!(failure.error(), SourceWalPreparationError::Sqlite(e) if e.sqlite_error_code()==Some(ErrorCode::DatabaseBusy))
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "nine-second busy timeout must not be used"
    );
    let writer = failure.into_writer().unwrap();
    assert_eq!(timeout(&writer), 9000);
    assert!(connection(&writer).is_autocommit());
    assert_eq!(
        source_writer_state_snapshot(writer.catalog_database()),
        before
    );
    other.execute_batch("ROLLBACK").unwrap();
    writer
        .prepare_wal_reservation(&expected, deadline())
        .unwrap()
        .abort()
        .unwrap();
}

#[test]
fn wal_reservation_real_reader_pressure_rejects_without_effect_then_retries_after_checkpoint() {
    let (_fixture, writer) = fixture();
    let expected = writer.current_revision().unwrap();
    connection(&writer)
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    let reader = Connection::open_with_flags(
        &writer.catalog_database().db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM metadata")
        .unwrap();
    connection(&writer)
        .execute(
            "INSERT INTO metadata(key,value) VALUES ('reservation-pressure',zeroblob(?1))",
            [WAL_HARD_WATERMARK_BYTES as i64],
        )
        .unwrap();
    let before = source_writer_state_snapshot(writer.catalog_database());
    let jobs_before = jobs(connection(&writer));
    let changes = connection(&writer).total_changes();
    let failure = writer
        .prepare_wal_reservation(&expected, deadline())
        .unwrap_err();
    assert!(
        matches!(failure.error(), SourceWalPreparationError::Wal(WalAdmissionError::HardWatermark { observed_bytes,hard_bytes }) if observed_bytes>=hard_bytes && *hard_bytes==WAL_HARD_WATERMARK_BYTES)
    );
    let writer = failure.into_writer().unwrap();
    assert_eq!(connection(&writer).total_changes(), changes);
    assert_eq!(
        source_writer_state_snapshot(writer.catalog_database()),
        before
    );
    assert_eq!(jobs(connection(&writer)), jobs_before);
    assert_eq!(
        reader
            .query_row(
                "SELECT count(*) FROM metadata WHERE key='reservation-pressure'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    connection(&writer)
        .execute("DELETE FROM metadata WHERE key='reservation-pressure'", [])
        .unwrap();
    connection(&writer)
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let preparation = writer
        .prepare_wal_reservation(&expected, deadline())
        .unwrap();
    assert_eq!(preparation.wal_bytes(), 0);
    preparation.abort().unwrap();
}

#[test]
fn wal_reservation_stale_context_and_invalid_deadline_leave_owned_rows_unchanged() {
    let (fixture, writer) = fixture();
    let stale = writer.current_revision().unwrap();
    let before = source_writer_state_snapshot(writer.catalog_database());
    let mut writer = SourceWriter::new(
        writer.into_catalog_database(),
        SourceId::from_string("feature-owner"),
        8,
    )
    .unwrap();
    let failure = writer
        .prepare_wal_reservation(&stale, deadline())
        .unwrap_err();
    assert!(matches!(
        failure.error(),
        SourceWalPreparationError::Owner(SourceWriterError::StaleFence)
    ));
    writer = failure.into_writer().unwrap();
    assert_eq!(
        source_writer_state_snapshot(writer.catalog_database()),
        before
    );
    let stale = writer.current_revision().unwrap();
    writer
        .commit_catalog_curation_command(
            &stale,
            &fixture.profile_guard,
            &[SourceCurationUpdate::SetLocked {
                path: CurationFixture::path("one.wav"),
                locked: true,
            }],
        )
        .unwrap();
    let before = source_writer_state_snapshot(writer.catalog_database());
    let failure = writer
        .prepare_wal_reservation(&stale, deadline())
        .unwrap_err();
    assert!(matches!(
        failure.error(),
        SourceWalPreparationError::Owner(SourceWriterError::StaleRevision { .. })
    ));
    writer = failure.into_writer().unwrap();
    let expected = writer.current_revision().unwrap();
    for bad_deadline in [Instant::now(), Instant::now() + Duration::from_secs(3)] {
        let failure = writer
            .prepare_wal_reservation(&expected, bad_deadline)
            .unwrap_err();
        assert!(matches!(
            failure.error(),
            SourceWalPreparationError::Deadline
        ));
        writer = failure.into_writer().unwrap();
    }
    assert!(connection(&writer).is_autocommit());
    assert_eq!(
        source_writer_state_snapshot(writer.catalog_database()),
        before
    );
}

#[test]
fn wal_reservation_near_expiry_and_expired_lease_never_trigger_hidden_renewal() {
    let (_fixture, mut writer) = fixture();
    let expected = writer.current_revision().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    connection(&writer)
        .execute(
            "UPDATE source_lease SET acquired_at=?1,expires_at=?2 WHERE singleton=1",
            [now - 100, now + 1],
        )
        .unwrap();
    let before = lease(connection(&writer));
    let failure = writer
        .prepare_wal_reservation(&expected, deadline())
        .unwrap_err();
    assert!(matches!(
        failure.error(),
        SourceWalPreparationError::LeaseRenewalRequired
    ));
    writer = failure.into_writer().unwrap();
    assert_eq!(lease(connection(&writer)), before);
    connection(&writer)
        .execute(
            "UPDATE source_lease SET expires_at=?1 WHERE singleton=1",
            [now - 1],
        )
        .unwrap();
    let before = lease(connection(&writer));
    let failure = writer
        .prepare_wal_reservation(&expected, deadline())
        .unwrap_err();
    assert!(matches!(
        failure.error(),
        SourceWalPreparationError::Owner(_)
    ));
    writer = failure.into_writer().unwrap();
    assert_eq!(lease(connection(&writer)), before);
    assert!(connection(&writer).is_autocommit());
}

#[test]
fn wal_reservation_expiry_does_not_unlock_until_abort_and_drop_releases_before_lease_close() {
    let (_fixture, writer) = fixture();
    let expected = writer.current_revision().unwrap();
    let other = external(&writer);
    let preparation = writer
        .prepare_wal_reservation(&expected, Instant::now() + Duration::from_secs(1))
        .unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    assert!(preparation.is_expired());
    busy(other.execute_batch("BEGIN IMMEDIATE").unwrap_err());
    let writer = preparation.abort().unwrap();
    let preparation = writer
        .prepare_wal_reservation(&expected, deadline())
        .unwrap();
    busy(other.execute_batch("BEGIN IMMEDIATE").unwrap_err());
    drop(preparation);
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert_eq!(lease(&other).3, "released");
}

#[test]
fn wal_reservation_failed_rollback_keeps_writer_inaccessible_until_confirmed_retry() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    let (_fixture, writer) = fixture();
    let expected = writer.current_revision().unwrap();
    let other = external(&writer);
    let deny = Arc::new(AtomicBool::new(false));
    let hook_deny = deny.clone();
    connection(&writer).authorizer(Some(move |context: AuthContext<'_>| {
        if matches!(
            context.action,
            AuthAction::Transaction {
                operation: TransactionOperation::Rollback
            }
        ) && hook_deny.load(Ordering::SeqCst)
        {
            Authorization::Deny
        } else {
            Authorization::Allow
        }
    }));
    let preparation = writer
        .prepare_wal_reservation(&expected, deadline())
        .unwrap();
    deny.store(true, Ordering::SeqCst);
    let failure = match preparation.abort() {
        Err(failure) => failure,
        Ok(_) => panic!("rollback must be denied"),
    };
    assert!(matches!(
        failure.error(),
        SourceWalPreparationError::RollbackUnconfirmed
    ));
    let failure: SourceWalPreparationFailure = match failure.into_writer() {
        Err(failure) => failure,
        Ok(_) => panic!("writer must remain unavailable"),
    };
    busy(other.execute_batch("BEGIN IMMEDIATE").unwrap_err());
    deny.store(false, Ordering::SeqCst);
    let writer = failure.into_writer().unwrap();
    assert!(connection(&writer).is_autocommit());
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert_eq!(writer.current_revision().unwrap(), expected);
}

mod vfs;
