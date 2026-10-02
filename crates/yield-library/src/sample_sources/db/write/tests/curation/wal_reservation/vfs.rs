use super::*;

#[test]
fn wal_reservation_actual_retained_vfs_failure_rejects_without_changing_rows() {
    let (_fixture, writer) = fixture();
    let expected = writer.current_revision().unwrap();
    let before = source_writer_state_snapshot(writer.catalog_database());
    let result = writer.prepare_wal_reservation_unknown_for_test(&expected, deadline());
    let failure = result.unwrap_err();
    assert!(matches!(
        failure.error(),
        SourceWalPreparationError::Wal(WalAdmissionError::ObservationUnavailable { .. })
    ));
    let writer = failure.into_writer().unwrap();
    assert_eq!(
        source_writer_state_snapshot(writer.catalog_database()),
        before
    );
    assert!(connection(&writer).is_autocommit());
    writer
        .prepare_wal_reservation(&expected, deadline())
        .unwrap()
        .abort()
        .unwrap();
}
