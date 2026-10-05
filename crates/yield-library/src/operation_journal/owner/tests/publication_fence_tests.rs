//! Actual original-runtime cancellation and SQLite evidence; scalar fences confer no effect rights.
#[cfg(test)]
mod integration {
    use super::super::*;
    fn queued_cancel(
        runtime: &mut OwnerRuntime,
        client: &OperationJournalClient,
        receivers: &OwnerCommandReceivers,
        operation: OperationId,
        sequence: i64,
        stamp: i64,
    ) -> Result<JournalCancelRequestReceipt, OperationJournalOwnerError> {
        let mut request = client
            .request_cancel_before_publish(
                operation,
                sequence,
                CancellationRequestObservation::new(stamp).unwrap(),
            )
            .unwrap();
        assert!(!execute_command(
            receivers.control.try_recv().unwrap(),
            runtime
        ));
        request.recv_timeout(Duration::from_secs(2)).unwrap()
    }

    #[test]
    fn publication_fence_original_runtime_cancel_closes_pending_before_actual_store_write() {
        let fixture = fixture("publication-fence-cancel-pending");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let operation = OperationId::new();
        let (record, args) = v5_copy_record(operation);
        let stamp = record.updated_unix_ms + 1;
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
        let mut runtime = OwnerRuntime::new(store).unwrap();
        let (commands, receivers) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(commands));
        shared
            .lifecycle
            .store(OwnerLifecycle::Available as u8, Ordering::Release);
        runtime.attach_resource_custody(&shared).unwrap();
        let client = OperationJournalClient {
            shared: Arc::clone(&shared),
        };
        let mut pending = runtime
            .publication_fences
            .issue(operation, &shared)
            .unwrap();
        let cancelled = queued_cancel(
            &mut runtime,
            &client,
            &receivers,
            operation,
            receipt.sequence(),
            stamp,
        )
        .unwrap();
        assert!(!pending.enter());
        assert!(cancelled.sequence() > receipt.sequence());
        let retained = runtime.store().get(operation).unwrap().unwrap();
        assert_eq!(
            retained.disposition,
            OperationDisposition::CancelRequestedBeforePublish
        );
    }
    #[test]
    fn publication_fence_original_runtime_entered_and_unsettled_cancel_do_not_touch_sqlite() {
        let fixture = fixture("publication-fence-cancel-entered");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let operation = OperationId::new();
        let (record, args) = v5_copy_record(operation);
        let stamp = record.updated_unix_ms + 1;
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
        let mut runtime = OwnerRuntime::new(store).unwrap();
        let (commands, receivers) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(commands));
        shared
            .lifecycle
            .store(OwnerLifecycle::Available as u8, Ordering::Release);
        runtime.attach_resource_custody(&shared).unwrap();
        let client = OperationJournalClient {
            shared: Arc::clone(&shared),
        };
        let mut pending = runtime
            .publication_fences
            .issue(operation, &shared)
            .unwrap();
        assert!(pending.enter());
        let before = runtime.store().get(operation).unwrap().unwrap();
        assert!(
            matches!(queued_cancel(&mut runtime, &client, &receivers, operation, receipt.sequence(), stamp), Err(OperationJournalOwnerError::PublicationCancellationPending { operation_id }) if operation_id == operation)
        );
        drop(pending);
        assert!(matches!(
            queued_cancel(
                &mut runtime,
                &client,
                &receivers,
                operation,
                receipt.sequence(),
                stamp
            ),
            Err(OperationJournalOwnerError::PublicationCancellationPending { .. })
        ));
        let after = runtime.store().get(operation).unwrap().unwrap();
        assert_eq!(encode(&after).unwrap(), encode(&before).unwrap());
    }
}
