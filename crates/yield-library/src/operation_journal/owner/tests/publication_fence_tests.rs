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
    fn original_completion_actor_idle_poll_defers_real_cancellation_without_sql_change() {
        let fixture = fixture("publication-completion-actor");
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
        let fence = runtime
            .publication_fences
            .issue(operation, &shared)
            .unwrap();
        fence.original_worker_started();
        fence.complete_before_effect([3; 32], true);
        assert!(!fence.completion_observed());
        // Saturate the actual original ordinary command queue. Control dispatch must
        // still poll completion first without executing any of these foreign writes.
        let (full, _) =
            crate::operation_journal::work_progress::test_full_progress_store_snapshots();
        let full = StoredFullProgressRecord::new(full).unwrap();
        let mut ordinary = Vec::new();
        for _ in 0..28 {
            ordinary.push(client.persist_full(full.clone(), None).unwrap());
        }
        assert!(client.persist_full(full, None).is_err());
        let before = runtime.store().get(operation).unwrap().unwrap();
        let mut request = client
            .request_cancel_before_publish(
                operation,
                receipt.sequence(),
                CancellationRequestObservation::new(stamp).unwrap(),
            )
            .unwrap();
        let command = receivers
            .recv_with_idle(|| runtime.poll_original_work())
            .unwrap();
        assert!(fence.completion_observed());
        assert!(!execute_command(command, &mut runtime));
        assert!(
            matches!(request.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(OperationJournalOwnerError::PublicationCancellationPending { operation_id }) if operation_id == operation)
        );
        let after = runtime.store().get(operation).unwrap().unwrap();
        assert_eq!(after, before);
        drop(fence);
        drop(client);
        shared.commands.lock().unwrap().take();
        receivers.reject_pending(|| OperationJournalOwnerError::Closed);
        assert!(
            receivers
                .recv_with_idle(|| runtime.poll_original_work())
                .is_none()
        );
        assert!(runtime.publication_fences.unresolved());
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

    fn mutation_request(record: &StoredOperationRecord) -> ChunkClaimRequest {
        let row = &record.capacity_plan.as_ref().unwrap().volumes[0];
        ChunkClaimRequest::new(
            ChunkId::new(),
            record.operation_id,
            AdmissionClaim::new(
                AdmissionClaimId::new(),
                row.identity.clone(),
                row.allocation_class.into(),
                row.allocated_bytes,
            )
            .unwrap(),
            record
                .universal_intent
                .as_ref()
                .unwrap()
                .participants()
                .descriptors()[0]
                .id(),
            "publication-output".into(),
            0,
            row.logical_bytes,
            row.allocation_unit,
            row.protected_free_bytes,
        )
        .unwrap()
    }

    #[test]
    fn publication_mutation_fence_blocks_original_queued_intent_and_chunk_claim() {
        let fixture = fixture("publication-mutation-queue");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let operation = OperationId::new();
        let (record, _) = v5_copy_record(operation);
        store.insert_intent_with_outcome(&record).receipt().unwrap();
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
        let mut fence = Some(
            runtime
                .publication_fences
                .issue(operation, &shared)
                .unwrap(),
        );
        assert!(fence.as_mut().unwrap().enter());
        let before = encode(&runtime.store().get(operation).unwrap().unwrap()).unwrap();
        for unsettled in [false, true] {
            if unsettled {
                drop(fence.take());
            }
            let mut changed = record.clone();
            changed.intent.label = "must not overwrite".into();
            let mut intent = client.persist_intent(changed).unwrap();
            execute_command(receivers.ordinary.try_recv().unwrap(), &mut runtime);
            assert!(
                matches!(intent.recv_timeout(Duration::from_secs(2)).unwrap(),
                Err(OperationJournalOwnerError::PublicationMutationPending { operation_id })
                if operation_id == operation)
            );
            let request = mutation_request(&record);
            let chunk_id = request.chunk_id();
            let mut claim = client.claim_chunk(request).unwrap();
            execute_command(receivers.ordinary.try_recv().unwrap(), &mut runtime);
            assert!(
                matches!(claim.recv_timeout(Duration::from_secs(2)).unwrap(),
                Err(OperationJournalOwnerError::PublicationMutationPending { operation_id })
                if operation_id == operation)
            );
            assert!(
                runtime
                    .store()
                    .read_chunk_claim(chunk_id)
                    .unwrap()
                    .is_none()
            );
            assert!(runtime.capacity_provider.is_none());
            assert_eq!(
                encode(&runtime.store().get(operation).unwrap().unwrap()).unwrap(),
                before
            );
        }
    }

    #[test]
    fn publication_mutation_pending_closes_before_original_chunk_admission() {
        let fixture = fixture("publication-mutation-pending");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let operation = OperationId::new();
        let (record, _) = v5_copy_record(operation);
        store.insert_intent_with_outcome(&record).receipt().unwrap();
        let mut runtime = OwnerRuntime::new(store).unwrap();
        let (commands, _) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(commands));
        runtime.attach_resource_custody(&shared).unwrap();
        let mut fence = runtime
            .publication_fences
            .issue(operation, &shared)
            .unwrap();
        let request = mutation_request(&record);
        let chunk_id = request.chunk_id();
        assert!(matches!(
            runtime.claim_chunk(request),
            Err(OperationJournalOwnerError::ChunkClaim { .. })
        ));
        assert!(!fence.enter());
        assert!(
            runtime
                .store()
                .read_chunk_claim(chunk_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn publication_mutation_fence_preserves_claim_duplicate_and_blocks_issue_and_write_start() {
        let fixture = fixture("publication-mutation-chunk-rows");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let operation = OperationId::new();
        let (record, _) = v5_copy_record(operation);
        store.insert_intent_with_outcome(&record).receipt().unwrap();
        let request = mutation_request(&record);
        let row = &record.capacity_plan.as_ref().unwrap().volumes[0];
        // Arithmetic-only provider creates actual journal rows. It supplies no
        // native witness, source pin or publication permission to the runtime.
        let provider = CapacityProvider::qualified_fixture(BTreeMap::from([(
            row.identity.clone(),
            VolumeFacts {
                identity: row.identity.clone(),
                free_bytes: PROTECTED_FREE_SPACE_FLOOR + 8,
                allocation_unit: 1,
            },
        )]));
        let lease = provider
            .admit(&request.plan().unwrap(), &request.inventory().unwrap())
            .unwrap();
        let claimed = store
            .persist_chunk_claim_with_lease(&request, &provider, lease)
            .unwrap();
        let mut runtime = OwnerRuntime::new(store).unwrap();
        let (commands, _) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(commands));
        runtime.attach_resource_custody(&shared).unwrap();
        let mut fence = runtime
            .publication_fences
            .issue(operation, &shared)
            .unwrap();
        assert!(fence.enter());
        // An exact duplicate is a read, so it remains available during entry.
        let duplicate = runtime.claim_chunk(request.clone()).unwrap();
        assert!(duplicate.matches_request(&request));
        assert!(matches!(runtime.issue_chunk(request.chunk_id(), operation),
            Err(OperationJournalOwnerError::PublicationMutationPending { operation_id })
            if operation_id == operation));
        assert_eq!(
            runtime
                .store()
                .read_chunk_claim(request.chunk_id())
                .unwrap()
                .unwrap()
                .state(),
            claimed.state()
        );
        assert!(runtime.issued_chunk_tickets.is_empty());
        drop(fence);
        assert!(matches!(
            runtime.issue_chunk(request.chunk_id(), operation),
            Err(OperationJournalOwnerError::PublicationMutationPending { .. })
        ));
        // Advance only the actual SQL fixture, then install its original ticket
        // metadata. Entry must reject before consuming that ticket or seeking
        // any native capacity witness; no executable grant is constructed.
        let receipt = runtime
            .store()
            .issue_chunk_claim(request.chunk_id(), operation, Uuid::new_v4())
            .unwrap();
        let permit = ChunkWritePermit::from_issue_receipt(receipt);
        let issue_id = permit.issue_id();
        let identity = permit.identity().clone();
        runtime.issued_chunk_tickets.insert(
            issue_id,
            IssuedChunkTicket {
                unused_release: None,
                chunk_id: permit.chunk_id(),
                operation_id: operation,
                identity: identity.clone(),
                live_output: None,
                live_target: None,
                live_source_fences: Vec::new(),
            },
        );
        assert!(matches!(
            runtime.begin_chunk_write(permit, identity, 1),
            Err(OperationJournalOwnerError::PublicationMutationPending { .. })
        ));
        assert!(runtime.issued_chunk_tickets.contains_key(&issue_id));
        assert_eq!(
            runtime
                .store()
                .read_chunk_claim(request.chunk_id())
                .unwrap()
                .unwrap()
                .state(),
            ChunkClaimState::Issued
        );
        assert_eq!(provider.claim_count(), 1);
        assert!(runtime.capacity_provider.is_none());
    }
}
