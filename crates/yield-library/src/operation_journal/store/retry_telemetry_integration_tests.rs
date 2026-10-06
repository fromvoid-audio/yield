mod retry_telemetry_integration {
    use super::*;
    use crate::operation_journal::retry_telemetry::{
        JournalRetryCall, JournalRetryReturn, tests::Capture,
    };

    fn observation(attempt: u32, class: RetryErrorClass) -> RetryObservation {
        RetryObservation::new(
            attempt,
            51,
            52,
            RetryLeaseId::new("original-retry-telemetry").expect("retry lease"),
            53,
            class,
            "original_retry_observation",
            None,
        )
        .expect("retry observation")
    }

    fn inserted_copy(store: &OperationJournalStore) -> (OperationId, i64) {
        let (id, intent, universal, args, plan) = v5_copy_inputs();
        let receipt = store
            .persist_copy_intent(id, intent, universal, args, plan, 50)
            .expect("persist genuine original Store intent");
        // A bounded persisted claim observation fixture; no live capacity authority.
        store.connection.execute(
            "INSERT INTO operation_journal_ordinary_claim (claim_id, operation_id, volume_device, allocation_class, allocated_bytes) VALUES (?1, ?2, '1', 'SourceWalShm', 8192)",
            params![Uuid::new_v4().to_string(), id.to_string()],
        ).expect("retained logical claim fixture");
        (id, receipt.sequence())
    }

    fn row(store: &OperationJournalStore, id: OperationId) -> (i64, Vec<u8>) {
        store
            .connection
            .query_row(
                "SELECT sequence, record FROM operation_journal WHERE operation_id=?1",
                params![id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("original row bytes and sequence")
    }

    #[test]
    fn retry_telemetry_original_update_replay_conflict_and_invalid_sequence() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("original Store");
        let (id, sequence) = inserted_copy(&store);
        let claims = store
            .read_ordinary_claim_page(None, 64)
            .expect("claims before retry");
        let capture = Capture::new();
        let retry = observation(1, RetryErrorClass::TransientContention);
        let receipt = store
            .record_retry_pending(id, sequence, retry.clone())
            .expect("retry update");
        let updated = row(&store, id);
        assert_eq!(
            store
                .record_retry_pending(id, sequence, retry)
                .expect("readonly exact replay"),
            receipt
        );
        assert!(
            store
                .record_retry_pending(
                    id,
                    sequence,
                    observation(2, RetryErrorClass::TransientContention)
                )
                .is_err()
        );
        assert!(matches!(
            store.record_retry_pending(id, 0, observation(1, RetryErrorClass::TransientContention)),
            Err(OperationJournalStoreError::InvalidExpectedSequence { expected: 0 })
        ));
        assert_eq!(row(&store, id), updated);
        assert_eq!(
            store
                .read_ordinary_claim_page(None, 64)
                .expect("retained claims"),
            claims
        );
        assert!(store.connection.is_autocommit());
        let snapshot = capture.snapshot();
        assert_eq!(
            snapshot.count(
                JournalRetryCall::RetryPending,
                JournalRetryReturn::ConfirmedReturn
            ),
            2
        );
        assert_eq!(
            snapshot.count(
                JournalRetryCall::RetryPending,
                JournalRetryReturn::OtherError
            ),
            2
        );
        assert_eq!(snapshot.elapsed_buckets.iter().sum::<u64>(), 4);
    }

    #[test]
    fn retry_telemetry_original_contention_preserves_bytes_and_then_confirms() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("original Store");
        let (id, sequence) = inserted_copy(&store);
        let before = row(&store, id);
        let claims = store
            .read_ordinary_claim_page(None, 64)
            .expect("claims before contention");
        // Fixture preparation releases an already retained exclusive lock, then restores
        // the ORIGINAL connection mode before entering the original retry method.
        store.connection.execute_batch("PRAGMA locking_mode=NORMAL; SELECT count(*) FROM operation_journal; PRAGMA locking_mode=EXCLUSIVE;").expect("release fixture lock and restore exact mode");
        let mode: String = store
            .connection
            .pragma_query_value(None, "locking_mode", |row| row.get(0))
            .expect("actual connection mode");
        assert_eq!(mode.to_ascii_lowercase(), "exclusive");
        store
            .connection
            .busy_timeout(Duration::ZERO)
            .expect("bounded fixture timeout");
        let blocker =
            Connection::open_with_flags(&store.database_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
                .expect("existing fixture database");
        blocker
            .execute_batch("BEGIN IMMEDIATE")
            .expect("real competing SQLite reservation");
        let capture = Capture::new();
        let retry = observation(1, RetryErrorClass::TransientContention);
        let error = store
            .record_retry_pending(id, sequence, retry.clone())
            .expect_err("actual original contention return");
        assert!(
            matches!(error, OperationJournalStoreError::Sql(rusqlite::Error::SqliteFailure(ref error, _)) if error.code == rusqlite::ErrorCode::DatabaseBusy)
        );
        // Observe the row on the actual blocker while its reservation still lives.
        let blocked_row: (i64, Vec<u8>) = blocker
            .query_row(
                "SELECT sequence, record FROM operation_journal WHERE operation_id=?1",
                params![id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("unchanged original row under contention");
        assert_eq!(blocked_row, before);
        blocker
            .execute_batch("ROLLBACK")
            .expect("release genuine blocker");
        drop(blocker);
        assert_eq!(row(&store, id), before);
        assert_eq!(
            store
                .read_ordinary_claim_page(None, 64)
                .expect("unchanged claims"),
            claims
        );
        store
            .record_retry_pending(id, sequence, retry)
            .expect("subsequent original confirmed retry");
        let snapshot = capture.snapshot();
        assert_eq!(
            snapshot.count(
                JournalRetryCall::RetryPending,
                JournalRetryReturn::ContentionError
            ),
            1
        );
        assert_eq!(
            snapshot.count(
                JournalRetryCall::RetryPending,
                JournalRetryReturn::ConfirmedReturn
            ),
            1
        );
        assert!(store.connection.is_autocommit());
    }

    #[test]
    fn retry_telemetry_original_malformed_row_remains_retained() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("original Store");
        let (id, sequence) = inserted_copy(&store);
        let malformed = b"{".to_vec();
        store
            .connection
            .execute(
                "UPDATE operation_journal SET record=?1 WHERE operation_id=?2",
                params![&malformed, id.to_string()],
            )
            .expect("malformed retained fixture");
        let before = row(&store, id);
        let claims = store
            .read_ordinary_claim_page(None, 64)
            .expect("claims before malformed retry");
        let capture = Capture::new();
        assert!(matches!(
            store.record_retry_pending(
                id,
                sequence,
                observation(1, RetryErrorClass::TransientContention)
            ),
            Err(OperationJournalStoreError::RetainedRecord { .. })
        ));
        assert_eq!(row(&store, id), before);
        assert_eq!(before.1, malformed);
        assert_eq!(
            store
                .read_ordinary_claim_page(None, 64)
                .expect("retained malformed claim evidence"),
            claims
        );
        assert!(store.connection.is_autocommit());
        assert_eq!(
            capture.snapshot().count(
                JournalRetryCall::RetryPending,
                JournalRetryReturn::RetainedAuditError
            ),
            1
        );
    }

    #[test]
    fn retry_telemetry_original_chunk_exhaustion_counts_nested_returns() {
        let fixture = fixture();
        let store = OperationJournalStore::open(&fixture.owner).expect("original Store");
        let (id, intent, universal, args, plan) = v5_copy_inputs();
        let operation = StoredOperationRecord::new_v16_command(
            id,
            intent,
            universal,
            UniversalCommandArgs::Copy(args),
            FullCapacityPlan::from_legacy(&plan).expect("full plan"),
            50,
        );
        let inserted = store
            .insert_intent(&operation)
            .expect("original full intent");
        let capture = Capture::new();
        let retry = observation(1, RetryErrorClass::CapacityUnavailable);
        let returned = store
            .record_post_intent_chunk_exhaustion(id, inserted.sequence(), retry.clone())
            .expect("original exhausted disposition");
        assert_eq!(returned.1, OperationDisposition::RetryPending);
        let after = row(&store, id);
        assert_eq!(
            store
                .record_post_intent_chunk_exhaustion(id, inserted.sequence(), retry)
                .expect("original exhaustion replay"),
            returned
        );
        assert!(matches!(
            store.record_post_intent_chunk_exhaustion(
                id,
                inserted.sequence(),
                observation(1, RetryErrorClass::TransientContention)
            ),
            Err(OperationJournalStoreError::InvalidRetryObservation { .. })
        ));
        assert_eq!(row(&store, id), after);
        let snapshot = capture.snapshot();
        assert_eq!(
            snapshot.count(
                JournalRetryCall::RetryPending,
                JournalRetryReturn::ConfirmedReturn
            ),
            2
        );
        assert_eq!(
            snapshot.count(
                JournalRetryCall::ChunkExhaustion,
                JournalRetryReturn::ConfirmedReturn
            ),
            2
        );
        assert_eq!(
            snapshot.count(
                JournalRetryCall::ChunkExhaustion,
                JournalRetryReturn::OtherError
            ),
            1
        );
        assert_eq!(snapshot.elapsed_buckets.iter().sum::<u64>(), 5);
    }
}
