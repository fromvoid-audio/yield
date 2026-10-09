// Genuine CPU producer observations installed as adversarial/recovery SQLite fixture data.
// The fixture's SQL mutation is explicitly not original-owner terminal retention authority.
#[test]
fn recording_finalizing_recovery_fences_new_bytes_but_preserves_exact_replay() {
    use crate::operation_journal::{
        owner::JournalNamespaceFundingProbe,
        recording_commands::{
            RecordingCapturedChunk, RecordingChunkRole,
            controls::{ControlRole, RecordingFinalizingInventory},
            recording_proposal_for_test,
        },
    };
    use reson::recording::{RecordingLimits, RecordingProducer, RecordingSinkBinding};
    use std::io::{self, Cursor, Seek, SeekFrom, Write};
    struct Sink(Cursor<Vec<u8>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Seek for Sink {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.0.seek(position)
        }
    }
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let header = initial_recording_header(&proposal);
    store.retain_recording_chunk(&header).unwrap();
    let (binding, mut original) =
        RecordingSinkBinding::bind(Box::new(Sink(Cursor::new(Vec::new()))));
    let producer = RecordingProducer::start(
        binding,
        RecordingLimits::new(proposal.format().sample_rate(), 2, 100).unwrap(),
    )
    .unwrap();
    let encoded = std::thread::scope(|scope| {
        let stopped = scope.spawn(move || producer.stop());
        let barrier = original
            .recv_barrier(std::time::Duration::from_secs(2))
            .unwrap();
        let inventory = RecordingFinalizingInventory::capture(
            &proposal,
            Uuid::new_v4(),
            Uuid::new_v4(),
            1,
            &original,
            &barrier,
        )
        .unwrap();
        let encoded = inventory.encode(&proposal).unwrap();
        barrier.deny();
        assert!(stopped.join().unwrap().is_err());
        encoded
    });
    let context_funding = JournalNamespaceFundingProbe::new();
    let context_scope = store
        .begin_recording_terminal_write_with_input(&context_funding.input)
        .unwrap();
    let snapshot = store
        .validate_original_recording_terminal_context(&proposal, &context_funding.input)
        .unwrap();
    // A zero-claim fixture is an observation, not admission. It supplies neither the
    // genuine current provider nor the Source pin the original owner also requires.
    assert!(snapshot.claims().is_empty());
    drop(snapshot);
    store
        .rollback_recording_terminal_write(&context_funding.input)
        .unwrap();
    drop(context_scope);
    store.connection.execute("UPDATE operation_journal_recording_chunks SET record=?1 WHERE operation_id=?2 AND slot=?3",
        params![encoded, proposal.operation_id().to_string(), ControlRole::Finalizing.slot()]).unwrap();
    let before = store.recording_retained_totals().unwrap();
    assert!(store.verify_recording_reservation(&proposal, true).unwrap());
    let funding = JournalNamespaceFundingProbe::new();
    let remaining = funding.remaining_bytes();
    store
        .verify_recording_reservation_with_input(&proposal, true, Some(&funding.input))
        .unwrap();
    assert_eq!(funding.remaining_bytes(), remaining);
    assert!(matches!(
        store.retain_recording_chunk(&header).unwrap(),
        super::recording_chunks::RecordingChunkReceipt::Replay { .. }
    ));
    let new = RecordingCapturedChunk::new(
        &proposal,
        Uuid::new_v4(),
        RecordingChunkRole::Pcm { ordinal: 0 },
        68,
        vec![0; 8],
    )
    .unwrap();
    assert!(store.retain_recording_chunk(&new).is_err());
    assert_eq!(store.recording_retained_totals().unwrap(), before);
    assert!(store.begin_recording_prestart_read().is_err());
    assert!(store.connection.is_autocommit());
    drop(store);
    let store = OperationJournalStore::open(&f.owner).unwrap();
    assert!(store.verify_recording_reservation(&proposal, true).unwrap());
    assert!(store.retain_recording_chunk(&new).is_err());
    assert_eq!(
        store
            .get_recording_chunk(proposal.operation_id(), header.chunk_id())
            .unwrap(),
        Some(header)
    );
    assert_eq!(store.recording_retained_totals().unwrap(), before);
    // A valid Finalizing record in a different role never becomes a supported control.
    store.connection.execute("UPDATE operation_journal_recording_chunks SET record=?1 WHERE operation_id=?2 AND slot=?3",
        params![encoded, proposal.operation_id().to_string(), ControlRole::Frozen.slot()]).unwrap();
    assert!(store.verify_recording_reservation(&proposal, true).is_err());
}

// No Source/startup grant: exercise the actual funded SQLite transaction as data-only storage.
#[test]
fn recording_original_finalizing_transaction_rolls_back_or_retains_lost_acknowledgement() {
    use crate::operation_journal::{
        owner::JournalNamespaceFundingProbe,
        recording_commands::{
            RecordingProposal, controls::RecordingFinalizingInventory, recording_proposal_for_test,
        },
    };
    use reson::recording::{RecordingLimits, RecordingProducer, RecordingSinkBinding};
    use std::io::{self, Cursor, Seek, SeekFrom, Write};
    struct Sink(Cursor<Vec<u8>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Seek for Sink {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.0.seek(position)
        }
    }
    fn inventory(proposal: &RecordingProposal) -> RecordingFinalizingInventory {
        let (binding, mut original) =
            RecordingSinkBinding::bind(Box::new(Sink(Cursor::new(Vec::new()))));
        let producer = RecordingProducer::start(
            binding,
            RecordingLimits::new(proposal.format().sample_rate(), 2, 100).unwrap(),
        )
        .unwrap();
        std::thread::scope(|scope| {
            let stopped = scope.spawn(move || producer.stop());
            let barrier = original
                .recv_barrier(std::time::Duration::from_secs(2))
                .unwrap();
            let inventory = RecordingFinalizingInventory::capture(
                proposal,
                Uuid::new_v4(),
                Uuid::new_v4(),
                1,
                &original,
                &barrier,
            )
            .unwrap();
            barrier.deny();
            assert!(stopped.join().unwrap().is_err());
            inventory
        })
    }
    for (boundary, committed) in [
        (InsertTestBoundary::BeforePrecommitValidation, false),
        (InsertTestBoundary::Commit, false),
        (InsertTestBoundary::AfterCommit, true),
    ] {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let proposal = recording_proposal_for_test(2, 100);
        store
            .insert_intent(&finite_recording_record(&proposal))
            .unwrap();
        let inventory = inventory(&proposal);
        let funding = JournalNamespaceFundingProbe::new();
        assert!(
            store
                .record_finalizing_inside_original_transaction(
                    &proposal,
                    &inventory,
                    &funding.input
                )
                .is_err()
        );
        let original = recording_projection_raw_state(&store.connection);
        store.set_insert_test_hook(move |at| {
            if at == boundary {
                InsertTestDirective::Fail(OperationJournalStoreError::RecordingRetention {
                    reason: "original terminal transaction fault".into(),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        let working = store
            .begin_recording_terminal_write_with_input(&funding.input)
            .unwrap()
            .unwrap();
        let written = store.record_finalizing_inside_original_transaction(
            &proposal,
            &inventory,
            &funding.input,
        );
        if boundary == InsertTestBoundary::BeforePrecommitValidation {
            assert!(written.is_err());
        } else {
            written.unwrap();
            assert!(
                store
                    .finish_recording_terminal_write(&funding.input)
                    .is_err()
            );
        }
        if !store.connection.is_autocommit() {
            store
                .rollback_recording_terminal_write(&funding.input)
                .unwrap();
        }
        drop(working);
        if committed {
            store
                .verify_finalizing_readback(&proposal, &inventory, &funding.input)
                .unwrap();
            assert!(store.require_recording_capturing(&proposal).is_err());
            let repeated = store
                .begin_recording_terminal_write_with_input(&funding.input)
                .unwrap();
            assert!(
                store
                    .record_finalizing_inside_original_transaction(
                        &proposal,
                        &inventory,
                        &funding.input
                    )
                    .is_err()
            );
            store
                .rollback_recording_terminal_write(&funding.input)
                .unwrap();
            drop(repeated);
        } else {
            assert_eq!(recording_projection_raw_state(&store.connection), original);
            store.require_recording_capturing(&proposal).unwrap();
        }
        assert!(store.connection.is_autocommit());
        assert_eq!(store.recording_retained_totals().unwrap(), (4, 16384));
        drop(store);
        let store = OperationJournalStore::open(&f.owner).unwrap();
        assert_eq!(store.recording_retained_totals().unwrap(), (4, 16384));
        if committed {
            store
                .verify_finalizing_readback(&proposal, &inventory, &funding.input)
                .unwrap();
        } else {
            assert_eq!(recording_projection_raw_state(&store.connection), original);
        }
    }
}

#[test]
fn recording_funded_terminal_claim_audit_preserves_corrupt_relations_and_bounded_keys() {
    use crate::operation_journal::{
        owner::JournalNamespaceFundingProbe, recording_commands::recording_proposal_for_test,
    };
    for corruption in 0..5 {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let proposal = recording_proposal_for_test(2, 100);
        store
            .insert_intent(&finite_recording_record(&proposal))
            .unwrap();
        // Logical SQLite observations only, with no provider issuance or physical grant.
        let request = ChunkClaimRequest::new(
            ChunkId::new(),
            proposal.operation_id(),
            crate::operation_journal::AdmissionClaim::new(
                AdmissionClaimId::new(),
                VolumeIdentity { device: 1 },
                crate::operation_journal::FullCapacityAllocationClass::DestinationStaging,
                4096,
            )
            .unwrap(),
            proposal.participants().descriptors()[0].id(),
            "recording-output".into(),
            0,
            100,
            1,
            crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR,
        )
        .unwrap();
        store.connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
        ordinary_claims::insert_rows_inside_transaction(
            &store.connection,
            proposal.operation_id(),
            &request.inventory().unwrap(),
        )
        .unwrap();
        chunk_claims::insert_claimed_inside_transaction(&store.connection, &request).unwrap();
        store.connection.execute_batch("COMMIT").unwrap();
        let funding = JournalNamespaceFundingProbe::new();
        let before = funding.remaining_bytes();
        chunk_claims::validate_original_relations(&store, &funding.input).unwrap();
        assert_eq!(funding.remaining_bytes(), before);
        match corruption {
            0 => {
                store
                    .connection
                    .execute(
                        "UPDATE operation_journal_chunk_claim SET participant_id=?1",
                        params![Uuid::new_v4().to_string()],
                    )
                    .unwrap();
            }
            1 => {
                store
                    .connection
                    .execute(
                        "UPDATE operation_journal_chunk_claim SET output_key=?1",
                        params!["🌋".repeat(1024)],
                    )
                    .unwrap();
            }
            2 => {
                store
                    .connection
                    .execute_batch("PRAGMA ignore_check_constraints=ON")
                    .unwrap();
                store
                    .connection
                    .execute(
                        "UPDATE operation_journal_chunk_claim SET chunk_id=?1",
                        params![44],
                    )
                    .unwrap();
            }
            3 => {
                store
                    .connection
                    .execute(
                        "UPDATE operation_journal_ordinary_claim SET allocated_bytes=8192",
                        [],
                    )
                    .unwrap();
            }
            _ => {
                store.connection.execute("INSERT INTO operation_journal_ordinary_claim_release(claim_id,operation_id,release_id) VALUES(?1,?2,?3)",
                    params![request.claim().claim_id().to_string(), proposal.operation_id().to_string(), Uuid::new_v4().to_string()]).unwrap();
            }
        }
        let raw: Vec<(String, i64)> = store
            .connection
            .prepare("SELECT claim_id,allocated_bytes FROM operation_journal_ordinary_claim")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        store.connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
        assert!(
            chunk_claims::validate_original_relations(&store, &funding.input).is_err(),
            "corruption {corruption}"
        );
        store.connection.execute_batch("ROLLBACK").unwrap();
        let after: Vec<(String, i64)> = store
            .connection
            .prepare("SELECT claim_id,allocated_bytes FROM operation_journal_ordinary_claim")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(after, raw);
        assert!(store.connection.is_autocommit());
    }
}

#[test]
fn recording_original_terminal_refusal_fence_precedes_any_durable_phase_change() {
    use crate::operation_journal::recording_commands::{
        RecordingCapturedChunk, RecordingChunkRole, recording_proposal_for_test,
    };
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let header = initial_recording_header(&proposal);
    store.retain_recording_chunk(&header).unwrap();
    let original = recording_projection_raw_state(&store.connection);
    // A refusal-only storage seam; no private producer/barrier or Source permission is issued.
    // The original actor selects this after its genuine private custody becomes invalidated.
    store.require_recording_existing_replay(&header).unwrap();
    let new = RecordingCapturedChunk::new(
        &proposal,
        Uuid::new_v4(),
        RecordingChunkRole::Pcm { ordinal: 0 },
        68,
        vec![0; 8],
    )
    .unwrap();
    assert!(store.require_recording_existing_replay(&new).is_err());
    let mut bytes = header.bytes().to_vec();
    bytes[0] ^= 1;
    let conflicting = RecordingCapturedChunk::new(
        &proposal,
        header.chunk_id(),
        header.role(),
        header.offset(),
        bytes,
    )
    .unwrap();
    assert!(
        store
            .require_recording_existing_replay(&conflicting)
            .is_err()
    );
    assert_eq!(recording_projection_raw_state(&store.connection), original);
    assert!(store.require_recording_capturing(&proposal).is_ok());
    // Thus failure to persist Finalizing cannot reopen the private owner fence.
    drop(store);
    let store = OperationJournalStore::open(&f.owner).unwrap();
    assert_eq!(recording_projection_raw_state(&store.connection), original);
}
