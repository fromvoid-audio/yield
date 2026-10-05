//! Real actor queue and original SQLite refusal, with scalar fences only; no native grants.
#[cfg(test)]
mod integration {
    use super::super::*;
    #[test]
    fn publication_chunk_release_queue_retains_entered_and_unsettled_original_evidence() {
        let fixture = fixture("publication-chunk-release-fence");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let operation = OperationId::new();
        let (record, args) = v5_copy_record(operation);
        store
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
        let mut fence = runtime
            .publication_fences
            .issue(operation, &shared)
            .unwrap();
        assert!(fence.enter());
        let chunk = ChunkId::new();
        let release = Uuid::new_v4();
        runtime.pending_chunk_releases.insert(
            release,
            PendingChunkRelease {
                chunk_id: chunk,
                operation_id: operation,
                issue_id: Uuid::new_v4(),
                release_id: release,
            },
        );
        let before = encode(&runtime.store().get(operation).unwrap().unwrap()).unwrap();
        for entered in [true, false] {
            if !entered {
                drop(fence);
                fence = runtime
                    .publication_fences
                    .issue(OperationId::new(), &shared)
                    .unwrap();
            }
            let mut cancel = client.cancel_chunk(chunk, operation, release).unwrap();
            assert!(!execute_command(
                receivers.control.try_recv().unwrap(),
                &mut runtime
            ));
            assert!(
                matches!(cancel.recv_timeout(Duration::from_secs(2)).unwrap(), Err(OperationJournalOwnerError::PublicationChargeReleasePending { operation_id }) if operation_id == operation)
            );
            let mut retry = client
                .retry_chunk_release(chunk, operation, release)
                .unwrap();
            assert!(!execute_command(
                receivers.control.try_recv().unwrap(),
                &mut runtime
            ));
            assert!(matches!(
                retry.recv_timeout(Duration::from_secs(2)).unwrap(),
                Err(OperationJournalOwnerError::PublicationChargeReleasePending { .. })
            ));
            assert_eq!(runtime.pending_chunk_releases.len(), 1);
            assert_eq!(runtime.pending_chunk_releases[&release].chunk_id, chunk);
            assert_eq!(
                encode(&runtime.store().get(operation).unwrap().unwrap()).unwrap(),
                before
            );
            assert!(runtime.store().read_chunk_claim(chunk).unwrap().is_none());
        }
    }
    #[test]
    fn publication_chunk_release_pending_fence_closes_even_when_release_identity_is_stale() {
        let fixture = fixture("publication-chunk-release-stale");
        let mut runtime =
            OwnerRuntime::new(OperationJournalStore::open(&fixture.owner).unwrap()).unwrap();
        let (commands, _receivers) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(commands));
        let operation = OperationId::new();
        let mut fence = runtime
            .publication_fences
            .issue(operation, &shared)
            .unwrap();
        assert!(matches!(
            runtime.cancel_chunk(ChunkId::new(), operation, Uuid::new_v4()),
            Err(OperationJournalOwnerError::ChunkClaim { .. })
        ));
        assert!(!fence.enter());
        assert!(!runtime.publication_fences.unresolved());
    }
}
