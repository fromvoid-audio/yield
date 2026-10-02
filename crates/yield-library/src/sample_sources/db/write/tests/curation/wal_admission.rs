use super::*;
use crate::{WAL_HARD_WATERMARK_BYTES, WalAdmissionError};

fn request(writer: &SourceWriter, sequence: u128, rating: Rating) -> SourceWriterRequest {
    SourceWriterRequest::new(
        crate::operation_journal::OperationId::from_uuid(uuid::Uuid::from_u128(sequence)),
        crate::operation_journal::ParticipantId::from_uuid(uuid::Uuid::from_u128(2077)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(sequence)),
        writer.current_revision().unwrap(),
        vec![SourceCurationUpdate::SetRating {
            path: CurationFixture::path("one.wav"),
            rating,
        }],
    )
    .unwrap()
}

#[test]
fn source_wal_admission_rejects_real_pressure_without_delta_and_preserves_exact_replay() {
    let fixture = CurationFixture::new_profile_bound();
    let mut writer =
        SourceWriter::new(fixture.database, SourceId::from_string("wal-source"), 1).unwrap();
    let admitted = request(&writer, 1, Rating::KEEP_3);
    let receipt = writer.commit_curation(admitted.clone()).unwrap();
    let revision = writer.current_revision().unwrap();
    let unseen = request(&writer, 2, Rating::NEUTRAL);
    let reader = rusqlite::Connection::open_with_flags(
        &writer.database_for_tests().db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    reader
        .execute_batch("BEGIN; SELECT value FROM metadata WHERE key='revision'")
        .unwrap();
    writer
        .database_for_tests()
        .connection
        .execute_batch("PRAGMA wal_autocheckpoint=0")
        .unwrap();
    // Actual SQLite frames retained by an external read snapshot, rather than a caller byte
    // scalar or a sparse pathname surrogate. This fixture does not grant capacity authority.
    writer
        .database_for_tests()
        .connection
        .execute(
            "INSERT INTO metadata(key,value) VALUES ('wal-test-pressure',zeroblob(?1))",
            [WAL_HARD_WATERMARK_BYTES as i64],
        )
        .unwrap();
    let before_changes = writer.database_for_tests().connection.total_changes();
    assert!(matches!(writer.commit_curation(unseen.clone()),
        Err(crate::sample_sources::SourceWriterError::Database(SourceDbError::WalAdmission(
            WalAdmissionError::HardWatermark { observed_bytes, .. }
        ))) if observed_bytes >= WAL_HARD_WATERMARK_BYTES));
    assert!(matches!(
        writer.commit_catalog_curation_command(
            &revision,
            &fixture.profile_guard,
            &[SourceCurationUpdate::SetRating {
                path: CurationFixture::path("one.wav"),
                rating: Rating::NEUTRAL
            }],
        ),
        Err(crate::sample_sources::SourceWriterError::Database(
            SourceDbError::WalAdmission(WalAdmissionError::HardWatermark { .. })
        ))
    ));
    assert_eq!(writer.current_revision().unwrap(), revision);
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        before_changes
    );
    let rows: i64 = writer
        .database_for_tests()
        .connection
        .query_row(
            "SELECT count(*) FROM source_participant_curation_receipts",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(
        writer
            .read_receipt_observation(admitted.identity())
            .unwrap()
            .unwrap(),
        receipt
    );
    assert_eq!(writer.commit_curation(admitted).unwrap(), receipt);
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        before_changes
    );
    let old_view: i64 = reader
        .query_row(
            "SELECT count(*) FROM metadata WHERE key='wal-test-pressure'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        old_view, 0,
        "admission must not kill or change the reader snapshot"
    );
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    writer
        .database_for_tests()
        .connection
        .execute("DELETE FROM metadata WHERE key='wal-test-pressure'", [])
        .unwrap();
    writer
        .database_for_tests()
        .connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let next = writer.commit_curation(unseen).unwrap();
    assert_eq!(next.source_revision().revision(), revision.revision() + 1);
}

#[test]
fn source_wal_admission_unknown_mode_rejects_both_production_curation_paths() {
    let fixture = CurationFixture::new_profile_bound();
    let mut writer =
        SourceWriter::new(fixture.database, SourceId::from_string("wal-mode"), 2).unwrap();
    let expected = writer.current_revision().unwrap();
    let unseen = request(&writer, 3, Rating::KEEP_3);
    writer
        .database_for_tests()
        .connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE")
        .unwrap();
    let before_changes = writer.database_for_tests().connection.total_changes();
    assert!(matches!(
        writer.commit_curation(unseen.clone()),
        Err(crate::sample_sources::SourceWriterError::Database(
            SourceDbError::WalAdmission(WalAdmissionError::ObservationUnavailable { .. })
        ))
    ));
    assert!(matches!(
        writer.commit_catalog_curation_command(
            &expected,
            &fixture.profile_guard,
            &[SourceCurationUpdate::SetRating {
                path: CurationFixture::path("one.wav"),
                rating: Rating::KEEP_3
            }],
        ),
        Err(crate::sample_sources::SourceWriterError::Database(
            SourceDbError::WalAdmission(WalAdmissionError::ObservationUnavailable { .. })
        ))
    ));
    assert_eq!(writer.current_revision().unwrap(), expected);
    assert_eq!(
        writer.database_for_tests().connection.total_changes(),
        before_changes
    );
    writer
        .database_for_tests()
        .connection
        .execute_batch("PRAGMA journal_mode=WAL")
        .unwrap();
    writer.commit_curation(unseen).unwrap();
}
