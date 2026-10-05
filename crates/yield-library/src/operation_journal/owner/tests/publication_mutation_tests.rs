//! Actual actor mutations and owned SQLite readback; scalar fences grant no native authority.
#[cfg(test)]
mod integration {
    use super::super::*;

    fn actor(
        store: OperationJournalStore,
    ) -> (OwnerRuntime, OperationJournalClient, OwnerCommandReceivers) {
        let mut runtime = OwnerRuntime::new(store).unwrap();
        let (commands, receivers) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(commands));
        shared
            .lifecycle
            .store(OwnerLifecycle::Available as u8, Ordering::Release);
        runtime.attach_resource_custody(&shared).unwrap();
        (runtime, OperationJournalClient { shared }, receivers)
    }
    fn retry(exhaustion: bool) -> RetryObservation {
        RetryObservation::new(
            1,
            51,
            52,
            RetryLeaseId::new("mutation-retry").unwrap(),
            53,
            if exhaustion {
                RetryErrorClass::CapacityUnavailable
            } else {
                RetryErrorClass::TransientContention
            },
            "owner_busy",
            Some("owner retry".into()),
        )
        .unwrap()
    }
    fn pending<T>(result: Result<T, OperationJournalOwnerError>, operation: OperationId) {
        assert!(
            matches!(result, Err(OperationJournalOwnerError::PublicationMutationPending { operation_id }) if operation_id == operation)
        );
    }
    #[test]
    fn publication_mutation_full_progress_pending_stale_cas_and_unsettled_readback() {
        for entered in [false, true] {
            let fixture = fixture("publication-mutation-full");
            let store = OperationJournalStore::open(&fixture.owner).unwrap();
            let (initial, progressed) =
                crate::operation_journal::work_progress::test_full_progress_store_snapshots();
            let initial = StoredFullProgressRecord::new(initial).unwrap();
            let progressed = StoredFullProgressRecord::new(progressed).unwrap();
            let operation = initial.snapshot().intent().operation_id();
            store.persist_full(&initial, None).unwrap();
            let (mut runtime, client, receivers) = actor(store);
            let mut fence = runtime
                .publication_fences
                .issue(operation, &client.shared)
                .unwrap();
            if entered {
                assert!(fence.enter());
            }
            let before = runtime.store().get_full(operation).unwrap().unwrap();
            let mut request = client.persist_full(progressed.clone(), Some(99)).unwrap();
            assert!(!execute_command(
                receivers.ordinary.try_recv().unwrap(),
                &mut runtime
            ));
            let result = request.recv_timeout(Duration::from_secs(2)).unwrap();
            if entered {
                pending(result, operation);
            } else {
                assert!(result.is_err());
                assert!(!fence.enter());
            }
            let after = runtime.store().get_full(operation).unwrap().unwrap();
            assert_eq!(after.sequence(), before.sequence());
            assert_eq!(after.record(), before.record());
            drop(fence);
            let mut request = client
                .persist_full(progressed, Some(before.sequence()))
                .unwrap();
            assert!(!execute_command(
                receivers.ordinary.try_recv().unwrap(),
                &mut runtime
            ));
            let result = request.recv_timeout(Duration::from_secs(2)).unwrap();
            if entered {
                pending(result, operation);
                let after = runtime.store().get_full(operation).unwrap().unwrap();
                assert_eq!(after.sequence(), before.sequence());
                assert_eq!(after.record(), before.record());
            } else {
                assert!(matches!(
                    result,
                    Ok(FullProgressWriteOutcome::Applied { .. })
                ));
            }
        }
    }
    #[test]
    fn publication_mutation_retry_and_exhaustion_fence_original_operation_only() {
        for exhaustion in [false, true] {
            for entered in [false, true] {
                let fixture = fixture("publication-mutation-retry");
                let store = OperationJournalStore::open(&fixture.owner).unwrap();
                let operation = OperationId::new();
                let (record, args) = v5_copy_record(operation);
                let receipt = store
                    .persist_copy_intent(
                        operation,
                        record.intent,
                        record.universal_intent.unwrap(),
                        args,
                        record.capacity_plan.unwrap(),
                        record.created_unix_ms,
                    )
                    .unwrap();
                let (mut runtime, client, receivers) = actor(store);
                let mut fence = runtime
                    .publication_fences
                    .issue(operation, &client.shared)
                    .unwrap();
                if entered {
                    assert!(fence.enter());
                }
                let before = encode(&runtime.store().get(operation).unwrap().unwrap()).unwrap();
                for sequence in [99, receipt.sequence()] {
                    let result = if exhaustion {
                        let mut request = client
                            .record_post_intent_chunk_exhaustion(
                                operation,
                                sequence,
                                retry(exhaustion),
                            )
                            .unwrap();
                        assert!(!execute_command(
                            receivers.control.try_recv().unwrap(),
                            &mut runtime
                        ));
                        request
                            .recv_timeout(Duration::from_secs(2))
                            .unwrap()
                            .map(|_| ())
                    } else {
                        let mut request = client
                            .record_retry_pending(operation, sequence, retry(exhaustion))
                            .unwrap();
                        assert!(!execute_command(
                            receivers.control.try_recv().unwrap(),
                            &mut runtime
                        ));
                        request
                            .recv_timeout(Duration::from_secs(2))
                            .unwrap()
                            .map(|_| ())
                    };
                    if entered {
                        pending(result, operation);
                    } else if sequence == 99 {
                        assert!(result.is_err());
                        assert!(!fence.enter());
                    } else {
                        assert!(result.is_ok(), "{result:?}");
                    }
                    if entered || sequence == 99 {
                        assert_eq!(
                            encode(&runtime.store().get(operation).unwrap().unwrap()).unwrap(),
                            before
                        );
                    }
                }
                drop(fence);
                if entered {
                    let result = if exhaustion {
                        let mut request = client
                            .record_post_intent_chunk_exhaustion(
                                operation,
                                receipt.sequence(),
                                retry(true),
                            )
                            .unwrap();
                        assert!(!execute_command(
                            receivers.control.try_recv().unwrap(),
                            &mut runtime
                        ));
                        request
                            .recv_timeout(Duration::from_secs(2))
                            .unwrap()
                            .map(|_| ())
                    } else {
                        let mut request = client
                            .record_retry_pending(operation, receipt.sequence(), retry(false))
                            .unwrap();
                        assert!(!execute_command(
                            receivers.control.try_recv().unwrap(),
                            &mut runtime
                        ));
                        request
                            .recv_timeout(Duration::from_secs(2))
                            .unwrap()
                            .map(|_| ())
                    };
                    pending(result, operation);
                    assert_eq!(
                        encode(&runtime.store().get(operation).unwrap().unwrap()).unwrap(),
                        before
                    );
                }
                // A different operation reaches the ordinary missing-row store error, not this fence.
                let mut request = client
                    .record_retry_pending(OperationId::new(), 1, retry(false))
                    .unwrap();
                assert!(!execute_command(
                    receivers.control.try_recv().unwrap(),
                    &mut runtime
                ));
                let result = request.recv_timeout(Duration::from_secs(2)).unwrap();
                assert!(result.is_err());
                assert!(!matches!(
                    result,
                    Err(OperationJournalOwnerError::PublicationMutationPending { .. })
                ));
            }
        }
    }
}
