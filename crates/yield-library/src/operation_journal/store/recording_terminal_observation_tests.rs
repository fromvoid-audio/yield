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

// The original CPU barrier authenticates data; this store fixture supplies no native Source grant.
#[test]
fn recording_declared_terminal_cas_exact_readback_and_reopen_preserve_reserved_rows() {
    use crate::operation_journal::{
        owner::JournalNamespaceFundingProbe,
        recording_commands::{
            RecordingPcmNormalizer,
            controls::{ControlRole, RecordingDeclaredFinalizingInventory},
            recording_proposal_for_test,
        },
    };
    use reson::recording::{RecordingLimits, RecordingProducer, RecordingSinkBinding};
    use std::io::{self, Cursor, Seek, SeekFrom, Write};
    struct Sink(std::sync::Arc<std::sync::Mutex<Cursor<Vec<u8>>>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Seek for Sink {
        fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
            self.0.lock().unwrap().seek(from)
        }
    }
    for (case, channels) in (1..=8).chain(std::iter::once(1)).enumerate() {
        let frames = if case == 8 { 26000 } else { 17 };
        let f = fixture();
        let proposal = recording_proposal_for_test(channels, frames as u64 + 1);
        let store = OperationJournalStore::open(&f.owner).unwrap();
        store
            .insert_intent(&finite_recording_record(&proposal))
            .unwrap();
        let capturing_funding = JournalNamespaceFundingProbe::new();
        store
            .require_recording_claim_capturing_for_input(&proposal, &capturing_funding.input)
            .unwrap();
        let before_frozen = recording_projection_raw_state(&store.connection);
        let scope = store
            .begin_recording_terminal_write_with_input(&capturing_funding.input)
            .unwrap();
        assert!(
            store
                .start_recording_restart_audit(proposal.operation_id(), &capturing_funding.input)
                .is_err()
        );
        store
            .rollback_recording_terminal_write(&capturing_funding.input)
            .unwrap();
        drop(scope);
        assert_eq!(
            recording_projection_raw_state(&store.connection),
            before_frozen
        );
        let original_keys: Vec<(String, i64, i64)> = store.connection.prepare(
            "SELECT chunk_id,slot,sequence FROM operation_journal_recording_chunks WHERE slot>=20000 ORDER BY slot"
        ).unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap().collect::<Result<_, _>>().unwrap();
        let actual_bytes = std::sync::Arc::new(std::sync::Mutex::new(Cursor::new(Vec::new())));
        let (binding, mut settlements) =
            RecordingSinkBinding::bind(Box::new(Sink(actual_bytes.clone())));
        let mut producer = RecordingProducer::start(
            binding,
            RecordingLimits::new(proposal.format().sample_rate(), channels, frames as u64 + 1)
                .unwrap(),
        )
        .unwrap();
        producer.submit(&vec![0.25; frames * usize::from(channels)]);
        let (inventory, batch, joined, prefix_chunks, header) = std::thread::scope(|scope| {
            let stopped = scope.spawn(move || producer.stop());
            let barrier = settlements
                .recv_barrier(std::time::Duration::from_secs(2))
                .unwrap();
            let mut normalizer = RecordingPcmNormalizer::new(&proposal).unwrap();
            let mut prefix_chunks = Vec::new();
            let written = actual_bytes.lock().unwrap();
            let offset = barrier.write_inventory().pending_offset() as usize;
            assert_eq!(written.get_ref().len(), offset);
            let mut initial =
                crate::operation_journal::recording_commands::prefix::InitialRecordingHeader::new(
                    &proposal,
                )
                .unwrap();
            initial.apply_write(0, &written.get_ref()[..68]).unwrap();
            let header = initial.finish(Uuid::new_v4()).unwrap();
            let mut consumed = 0;
            for part in written.get_ref()[68..offset].chunks(16384) {
                let chunk_bytes = proposal.format().pcm_chunk_bytes() as usize;
                let count = (consumed + part.len()) / chunk_bytes - consumed / chunk_bytes;
                let ids: Vec<_> = (0..count).map(|_| Uuid::new_v4()).collect();
                let prepared = normalizer
                    .prepare(&proposal, 68 + consumed as u64, part, &ids)
                    .unwrap();
                prefix_chunks.extend(prepared.chunks().iter().cloned());
                normalizer.accept(prepared).unwrap();
                consumed += part.len();
            }
            drop(written);
            let result = RecordingDeclaredFinalizingInventory::capture(
                &proposal,
                Uuid::new_v4(),
                Uuid::new_v4(),
                1,
                &settlements,
                &barrier,
                &normalizer,
            )
            .unwrap();
            barrier.acknowledge().unwrap();
            assert!(stopped.join().unwrap().is_ok());
            let joined = settlements
                .recv_joined(std::time::Duration::from_secs(2))
                .unwrap();
            (result.0, result.1, joined, prefix_chunks, header)
        });
        store.retain_recording_chunk(&header).unwrap();
        assert_eq!(batch.chunks().len(), 3);
        let encoded = inventory.encode(&proposal).unwrap();
        assert!(encoded.len() <= 4096);
        let funding = JournalNamespaceFundingProbe::new();
        let scope = store
            .begin_recording_terminal_write_with_input(&funding.input)
            .unwrap();
        store
            .record_declared_finalizing_inside_original_transaction(
                &proposal,
                &inventory,
                &funding.input,
            )
            .unwrap();
        store
            .finish_recording_terminal_write(&funding.input)
            .unwrap();
        store
            .verify_declared_finalizing_readback(&proposal, &inventory, &funding.input)
            .unwrap();
        drop(scope);
        let keys: Vec<(String, i64, i64)> = store.connection.prepare(
            "SELECT chunk_id,slot,sequence FROM operation_journal_recording_chunks WHERE slot>=20000 ORDER BY slot"
        ).unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(keys, original_keys);
        let totals = store.recording_retained_totals().unwrap();
        assert!(store.verify_recording_reservation(&proposal, true).unwrap());
        let after_finalizing = recording_projection_raw_state(&store.connection);
        assert!(
            store
                .require_recording_claim_capturing_for_input(&proposal, &capturing_funding.input)
                .is_err()
        );
        assert!(
            store
                .validate_chunk_operation_for_input(
                    proposal.operation_id(),
                    "issuance",
                    &capturing_funding.input
                )
                .is_err()
        );
        assert_eq!(
            recording_projection_raw_state(&store.connection),
            after_finalizing
        );
        // Exact declarations still require actual retained rows and issued claims. These
        // adversarial SQL observations are not a Source/startup or physical grant fixture.
        let audit_funding = JournalNamespaceFundingProbe::new();
        let audit_scope = store
            .begin_recording_terminal_write_with_input(&audit_funding.input)
            .unwrap();
        assert!(
            store
                .validate_original_declared_terminal_retention(
                    &proposal,
                    &inventory,
                    &audit_funding.input
                )
                .is_err()
        );
        store
            .rollback_recording_terminal_write(&audit_funding.input)
            .unwrap();
        drop(audit_scope);
        for chunk in batch.chunks() {
            inventory.validate_chunk(&proposal, chunk).unwrap();
            // Declaration alone is not permission to bypass the durable append fence.
            assert!(store.retain_recording_chunk(chunk).is_err());
        }
        assert!(matches!(
            store.retain_recording_chunk(&header).unwrap(),
            super::recording_chunks::RecordingChunkReceipt::Replay { .. }
        ));
        drop(store);
        let store = OperationJournalStore::open(&f.owner).unwrap();
        store
            .verify_declared_finalizing_readback(&proposal, &inventory, &funding.input)
            .unwrap();
        let scope = store
            .begin_recording_terminal_write_with_input(&funding.input)
            .unwrap();
        assert!(
            store
                .record_declared_finalizing_inside_original_transaction(
                    &proposal,
                    &inventory,
                    &funding.input
                )
                .is_err()
        );
        store
            .rollback_recording_terminal_write(&funding.input)
            .unwrap();
        drop(scope);
        assert_eq!(store.recording_retained_totals().unwrap(), totals);
        for changed in [
            String::from_utf8(encoded.clone())
                .unwrap()
                .replacen("{", "{\"version\":3,", 1),
            String::from_utf8(encoded.clone())
                .unwrap()
                .replacen("{", "{\"unknown\":null,", 1),
            format!("{} null", String::from_utf8(encoded.clone()).unwrap()),
        ] {
            assert!(
                RecordingDeclaredFinalizingInventory::decode(changed.as_bytes(), &proposal)
                    .is_err()
            );
        }
        let mut changed: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        changed["chunks"][0]["offset"] = serde_json::json!(69);
        assert!(
            RecordingDeclaredFinalizingInventory::decode(
                &serde_json::to_vec(&changed).unwrap(),
                &proposal
            )
            .is_err()
        );
        changed["chunks"][0]["offset"] = serde_json::json!(68);
        changed["chunks"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::Value::Null);
        assert!(
            RecordingDeclaredFinalizingInventory::decode(
                &serde_json::to_vec(&changed).unwrap(),
                &proposal
            )
            .is_err()
        );
        // Install exact immutable bytes and ordinary/chunk rows as data-only SQL fixtures.
        store.connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
        for chunk in prefix_chunks
            .iter()
            .chain(batch.chunks())
            .chain(std::iter::once(&header))
        {
            let slot = match chunk.role() {
                crate::operation_journal::recording_commands::RecordingChunkRole::Pcm {
                    ordinal,
                } => i64::from(ordinal),
                crate::operation_journal::recording_commands::RecordingChunkRole::Header {
                    revision,
                } => 10000 + i64::from(revision),
            };
            if chunk.chunk_id() != header.chunk_id() {
                let sequence = store.next_sequence_inside_transaction().unwrap();
                store.connection.execute("INSERT INTO operation_journal_recording_chunks(chunk_id,operation_id,slot,sequence,record) VALUES(?1,?2,?3,?4,?5)",
                    params![chunk.chunk_id().to_string(), proposal.operation_id().to_string(), slot, sequence, chunk.encode(&proposal).unwrap()]).unwrap();
            }
            let request = ChunkClaimRequest::new(
                ChunkId::from_uuid(chunk.chunk_id()).unwrap(),
                proposal.operation_id(),
                crate::operation_journal::AdmissionClaim::new(
                    AdmissionClaimId::new(),
                    VolumeIdentity { device: 1 },
                    crate::operation_journal::FullCapacityAllocationClass::DestinationStaging,
                    (chunk.bytes().len() as u64).div_ceil(4096) * 4096,
                )
                .unwrap(),
                chunk.work_key().participant_id(),
                "recording-output".into(),
                chunk.offset(),
                chunk.bytes().len() as u64,
                1,
                crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR,
            )
            .unwrap();
            ordinary_claims::insert_rows_inside_transaction(
                &store.connection,
                proposal.operation_id(),
                &request.inventory().unwrap(),
            )
            .unwrap();
            chunk_claims::insert_claimed_inside_transaction(&store.connection, &request).unwrap();
        }
        store.connection.execute_batch("COMMIT").unwrap();
        let complete_funding = JournalNamespaceFundingProbe::new();
        let scope = store
            .begin_recording_terminal_write_with_input(&complete_funding.input)
            .unwrap();
        assert!(
            store
                .validate_original_declared_terminal_retention(
                    &proposal,
                    &inventory,
                    &complete_funding.input
                )
                .is_err()
        );
        // The private terminal append seam does not accept a declaration or exact replay
        // without an already-issued original range claim.
        for chunk in batch.chunks() {
            assert!(
                !store
                    .original_terminal_chunk_is_issued(chunk, &complete_funding.input)
                    .unwrap()
            );
            assert!(
                store
                    .retain_original_declared_terminal_chunk(
                        &proposal,
                        &inventory,
                        chunk,
                        &complete_funding.input
                    )
                    .is_err()
            );
        }
        // A persisted Issued state is a historical observation, not physical completion.
        for chunk in prefix_chunks.iter().chain(batch.chunks()) {
            store.connection.execute("UPDATE operation_journal_chunk_claim SET state='Issued',issue_id=?1 WHERE chunk_id=?2",
                params![Uuid::new_v4().to_string(), chunk.chunk_id().to_string()]).unwrap();
        }
        // Remove just the data fixture's terminal rows, retaining original issued claims.
        // Reinsert through the actual funded terminal append implementation, not the generic path.
        for chunk in batch.chunks() {
            store
                .connection
                .execute(
                    "DELETE FROM operation_journal_recording_chunks WHERE chunk_id=?1",
                    params![chunk.chunk_id().to_string()],
                )
                .unwrap();
        }
        let header_patch = batch
            .chunks()
            .iter()
            .find(|chunk| {
                matches!(
                    chunk.role(),
                    crate::operation_journal::recording_commands::RecordingChunkRole::Header {
                        revision: 1
                    }
                )
            })
            .unwrap();
        assert!(
            store
                .retain_original_declared_terminal_chunk(
                    &proposal,
                    &inventory,
                    header_patch,
                    &complete_funding.input
                )
                .is_err()
        );
        for chunk in batch.chunks() {
            assert!(
                store
                    .original_terminal_chunk_is_issued(chunk, &complete_funding.input)
                    .unwrap()
            );
            assert!(matches!(
                store
                    .retain_original_declared_terminal_chunk(
                        &proposal,
                        &inventory,
                        chunk,
                        &complete_funding.input
                    )
                    .unwrap(),
                super::recording_chunks::RecordingChunkReceipt::Committed { .. }
            ));
            let retained = recording_projection_raw_state(&store.connection);
            assert!(matches!(
                store
                    .retain_original_declared_terminal_chunk(
                        &proposal,
                        &inventory,
                        chunk,
                        &complete_funding.input
                    )
                    .unwrap(),
                super::recording_chunks::RecordingChunkReceipt::Replay { .. }
            ));
            assert_eq!(recording_projection_raw_state(&store.connection), retained);
        }
        store
            .validate_original_declared_terminal_retention(
                &proposal,
                &inventory,
                &complete_funding.input,
            )
            .unwrap();
        let sequence = store
            .original_frozen_control_sequence(&proposal, &complete_funding.input)
            .unwrap();
        let frozen = crate::operation_journal::recording_commands::controls::RecordingFrozenInventory::capture(
            &proposal, &inventory, sequence, &settlements, &joined).unwrap();
        // Precommit failure cannot consume the reserved slot or change retained charges.
        let before_frozen = recording_projection_raw_state(&store.connection);
        store
            .connection
            .execute_batch("SAVEPOINT frozen_fault")
            .unwrap();
        store.set_insert_test_hook(|at| {
            if at == InsertTestBoundary::BeforePrecommitValidation {
                InsertTestDirective::Fail(OperationJournalStoreError::RecordingRetention {
                    reason: "Frozen precommit fault".into(),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        assert!(
            store
                .record_original_frozen_inventory(
                    &proposal,
                    &inventory,
                    &frozen,
                    &complete_funding.input
                )
                .is_err()
        );
        store
            .connection
            .execute_batch("ROLLBACK TO frozen_fault; RELEASE frozen_fault")
            .unwrap();
        assert_eq!(
            recording_projection_raw_state(&store.connection),
            before_frozen
        );
        store.set_insert_test_hook(|_| InsertTestDirective::Continue);
        store
            .record_original_frozen_inventory(
                &proposal,
                &inventory,
                &frozen,
                &complete_funding.input,
            )
            .unwrap();
        store
            .verify_original_frozen_readback(&proposal, &frozen, &complete_funding.input)
            .unwrap();
        assert!(store.verify_recording_reservation(&proposal, true).unwrap());
        if channels == 1 {
            store.set_insert_test_hook(|at| {
                if at == InsertTestBoundary::Commit {
                    InsertTestDirective::ExecuteThenFail(
                        OperationJournalStoreError::RecordingRetention {
                            reason: "Frozen commit acknowledgement lost".into(),
                        },
                    )
                } else {
                    InsertTestDirective::Continue
                }
            });
            assert!(
                store
                    .finish_recording_terminal_write(&complete_funding.input)
                    .is_err()
            );
            assert!(store.connection.is_autocommit());
        } else {
            store
                .finish_recording_terminal_write(&complete_funding.input)
                .unwrap();
        }
        drop(scope);
        let frozen_totals = store.recording_retained_totals().unwrap();
        drop(store);
        let store = OperationJournalStore::open(&f.owner).unwrap();
        store
            .verify_original_frozen_readback(&proposal, &frozen, &complete_funding.input)
            .unwrap();
        assert!(
            store
                .verify_recording_reservation_with_input(
                    &proposal,
                    true,
                    Some(&complete_funding.input)
                )
                .unwrap()
        );
        let scope = store
            .begin_recording_terminal_write_with_input(&complete_funding.input)
            .unwrap();
        let current = store
            .start_original_frozen_verification(
                &proposal,
                &inventory,
                &frozen,
                &complete_funding.input,
            )
            .unwrap();
        assert!(!current.complete().unwrap());
        // Header initialization also needs an original issued claim; no shorter caller prefix.
        assert!(
            store
                .original_frozen_verification_page(
                    &proposal,
                    &frozen,
                    &current,
                    &complete_funding.input
                )
                .is_err()
        );
        assert!(!current.complete().unwrap());
        store.connection.execute("UPDATE operation_journal_chunk_claim SET state='Issued',issue_id=?1 WHERE chunk_id=?2",
            params![Uuid::new_v4().to_string(), header.chunk_id().to_string()]).unwrap();
        let next = store
            .original_frozen_verification_page(
                &proposal,
                &frozen,
                &current,
                &complete_funding.input,
            )
            .unwrap();
        let next = if case == 8 {
            // Seven PCM rows plus three headers require a second turn; the first
            // page must yield with the original state still available for retry.
            assert!(!next.complete().unwrap());
            store
                .original_frozen_verification_page(
                    &proposal,
                    &frozen,
                    &next,
                    &complete_funding.input,
                )
                .unwrap()
        } else {
            next
        };
        assert!(next.complete().unwrap());
        let verified = next.finish(&complete_funding.input).unwrap();
        let before_seal = recording_projection_raw_state(&store.connection);
        store
            .connection
            .execute_batch("SAVEPOINT seal_fault")
            .unwrap();
        store.set_insert_test_hook(|at| {
            if at == InsertTestBoundary::BeforePrecommitValidation {
                InsertTestDirective::Fail(OperationJournalStoreError::RecordingRetention {
                    reason: "Seal precommit fault".into(),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        assert!(
            store
                .retain_original_verified_seal(
                    &proposal,
                    &frozen,
                    &verified,
                    &complete_funding.input
                )
                .is_err()
        );
        store
            .connection
            .execute_batch("ROLLBACK TO seal_fault; RELEASE seal_fault")
            .unwrap();
        assert_eq!(
            recording_projection_raw_state(&store.connection),
            before_seal
        );
        store.set_insert_test_hook(|_| InsertTestDirective::Continue);
        store
            .retain_original_verified_seal(&proposal, &frozen, &verified, &complete_funding.input)
            .unwrap();
        let retained_seal = recording_projection_raw_state(&store.connection);
        store
            .retain_original_verified_seal(&proposal, &frozen, &verified, &complete_funding.input)
            .unwrap();
        assert_eq!(
            recording_projection_raw_state(&store.connection),
            retained_seal
        );
        store
            .verify_original_seal_readback(&proposal, &frozen, &verified, &complete_funding.input)
            .unwrap();
        assert!(
            store
                .verify_recording_reservation_with_input(
                    &proposal,
                    true,
                    Some(&complete_funding.input)
                )
                .unwrap()
        );
        if case == 8 {
            store.set_insert_test_hook(|at| {
                if at == InsertTestBoundary::AfterCommit {
                    InsertTestDirective::Fail(OperationJournalStoreError::RecordingRetention {
                        reason: "Seal commit acknowledgement lost".into(),
                    })
                } else {
                    InsertTestDirective::Continue
                }
            });
            assert!(
                store
                    .finish_recording_terminal_write(&complete_funding.input)
                    .is_err()
            );
            assert!(store.connection.is_autocommit());
        } else {
            store
                .finish_recording_terminal_write(&complete_funding.input)
                .unwrap();
        }
        drop(scope);
        drop(current);
        // Independent actual encoder oracle for the reconstructed durable object hash.
        let mut oracle = Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(
                &mut oracle,
                hound::WavSpec {
                    channels,
                    sample_rate: proposal.format().sample_rate(),
                    bits_per_sample: 32,
                    sample_format: hound::SampleFormat::Float,
                },
            )
            .unwrap();
            for _ in 0..frames * usize::from(channels) {
                writer.write_sample(0.25_f32).unwrap();
            }
            writer.finalize().unwrap();
        }
        let encoded_seal: Vec<u8> = store
            .connection
            .query_row(
                "SELECT record FROM operation_journal_recording_chunks WHERE slot=20002",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let observed: serde_json::Value = serde_json::from_slice(&encoded_seal).unwrap();
        assert_eq!(
            observed["seal"]["content_hash"],
            serde_json::json!(blake3::hash(oracle.get_ref()).as_bytes())
        );
        drop(store);
        let store = OperationJournalStore::open(&f.owner).unwrap();
        store
            .verify_original_declared_terminal_bytes(&proposal, &inventory, &complete_funding.input)
            .unwrap();
        store
            .verify_original_seal_readback(&proposal, &frozen, &verified, &complete_funding.input)
            .unwrap();
        assert!(store.verify_recording_reservation(&proposal, true).unwrap());
        assert_eq!(store.recording_retained_totals().unwrap(), frozen_totals);
        if case == 8 {
            // An unsealed Frozen recording is auditable without filling its reserved Seal slot.
            let scope = store
                .begin_recording_terminal_write_with_input(&complete_funding.input)
                .unwrap();
            let empty_seal = crate::operation_journal::recording_commands::controls::RecordingControlReservation::new(&proposal, ControlRole::Seal).unwrap().encode(&proposal).unwrap();
            store
                .connection
                .execute(
                    "UPDATE operation_journal_recording_chunks SET record=?1 WHERE slot=20002",
                    params![empty_seal],
                )
                .unwrap();
            let empty_snapshot = recording_projection_raw_state(&store.connection);
            let mut unsealed = store
                .start_recording_restart_audit(proposal.operation_id(), &complete_funding.input)
                .unwrap();
            let observed = loop {
                let (next, observed) = store
                    .recording_restart_audit_page(&unsealed, &complete_funding.input)
                    .unwrap();
                if let Some(observed) = observed {
                    break observed;
                }
                unsealed.verifier = next;
            };
            assert_eq!(
                observed.content_hash(),
                *blake3::hash(oracle.get_ref()).as_bytes()
            );
            assert_eq!(
                recording_projection_raw_state(&store.connection),
                empty_snapshot
            );
            drop(unsealed);
            store
                .rollback_recording_terminal_write(&complete_funding.input)
                .unwrap();
            drop(scope);
        }
        // A fresh restart audit is observation only, and never writes a seal or retained row.
        let before_audit = recording_projection_raw_state(&store.connection);
        let scope = store
            .begin_recording_terminal_write_with_input(&complete_funding.input)
            .unwrap();
        let mut audit = store
            .start_recording_restart_audit(proposal.operation_id(), &complete_funding.input)
            .unwrap();
        store
            .finish_recording_terminal_write(&complete_funding.input)
            .unwrap();
        drop(scope);
        let mut audit_pages = 0;
        let recovered = loop {
            let scope = store
                .begin_recording_terminal_write_with_input(&complete_funding.input)
                .unwrap();
            let (next, observed) = store
                .recording_restart_audit_page(&audit, &complete_funding.input)
                .unwrap();
            if case == 8 && audit_pages == 0 {
                // A lost COMMIT acknowledgement cannot consume the old verifier cursor.
                store.set_insert_test_hook(|boundary| {
                    if boundary == InsertTestBoundary::AfterCommit {
                        InsertTestDirective::ExecuteThenFail(
                            OperationJournalStoreError::RecordingRetention {
                                reason: "restart audit lost acknowledgement".into(),
                            },
                        )
                    } else {
                        InsertTestDirective::Continue
                    }
                });
                assert!(
                    store
                        .finish_recording_terminal_write(&complete_funding.input)
                        .is_err()
                );
                assert!(!audit.verifier.as_ref().unwrap().complete().unwrap());
                drop(next);
                drop(observed);
                drop(scope);
                let scope = store
                    .begin_recording_terminal_write_with_input(&complete_funding.input)
                    .unwrap();
                let (next, observed) = store
                    .recording_restart_audit_page(&audit, &complete_funding.input)
                    .unwrap();
                store
                    .finish_recording_terminal_write(&complete_funding.input)
                    .unwrap();
                drop(scope);
                assert!(observed.is_none());
                audit.verifier = next;
            } else {
                store
                    .finish_recording_terminal_write(&complete_funding.input)
                    .unwrap();
                drop(scope);
                if let Some(observed) = observed {
                    break observed;
                }
                audit.verifier = next;
            }
            audit_pages += 1;
            assert!(audit_pages <= 2);
        };
        assert_eq!(
            recovered.content_hash(),
            *blake3::hash(oracle.get_ref()).as_bytes()
        );
        assert_eq!(
            recording_projection_raw_state(&store.connection),
            before_audit
        );
        drop(audit);
        if case == 8 {
            // Exercise the actual owner queue and scheduler without any capacity/Source grant.
            let mut owner = crate::operation_journal::owner::OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while owner.status()
                == crate::operation_journal::owner::OperationJournalOwnerStatus::Initializing
            {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            let result = owner
                .client()
                .audit_recording_frozen_prefix(proposal.operation_id())
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(result.content_hash(), recovered.content_hash());
            assert!(owner.shutdown());
        } else {
            drop(store);
        }
        let store = OperationJournalStore::open(&f.owner).unwrap();
        assert_eq!(
            recording_projection_raw_state(&store.connection),
            before_audit
        );
        let scope = store
            .begin_recording_terminal_write_with_input(&complete_funding.input)
            .unwrap();
        let mut conflicting: serde_json::Value = serde_json::from_slice(&encoded_seal).unwrap();
        let first = conflicting["seal"]["content_hash"][0].as_u64().unwrap();
        conflicting["seal"]["content_hash"][0] = serde_json::json!(first ^ 1);
        store
            .connection
            .execute(
                "UPDATE operation_journal_recording_chunks SET record=?1 WHERE slot=20002",
                params![serde_json::to_vec(&conflicting).unwrap()],
            )
            .unwrap();
        let raw_conflict = recording_projection_raw_state(&store.connection);
        let mut corrupt_audit = store
            .start_recording_restart_audit(proposal.operation_id(), &complete_funding.input)
            .unwrap();
        let mut detected = false;
        for _ in 0..2 {
            match store.recording_restart_audit_page(&corrupt_audit, &complete_funding.input) {
                Ok((next, None)) => corrupt_audit.verifier = next,
                Ok((_, Some(_))) => panic!("corrupt stored hash accepted"),
                Err(_) => {
                    detected = true;
                    break;
                }
            }
        }
        assert!(detected);
        assert_eq!(
            recording_projection_raw_state(&store.connection),
            raw_conflict
        );
        drop(corrupt_audit);
        assert!(
            store
                .retain_original_verified_seal(
                    &proposal,
                    &frozen,
                    &verified,
                    &complete_funding.input
                )
                .is_err()
        );
        assert!(
            store
                .verify_original_seal_readback(
                    &proposal,
                    &frozen,
                    &verified,
                    &complete_funding.input
                )
                .is_err()
        );
        assert_eq!(
            recording_projection_raw_state(&store.connection),
            raw_conflict
        );
        store
            .rollback_recording_terminal_write(&complete_funding.input)
            .unwrap();
        drop(scope);
        let scope = store
            .begin_recording_terminal_write_with_input(&complete_funding.input)
            .unwrap();
        assert!(
            store
                .record_original_frozen_inventory(
                    &proposal,
                    &inventory,
                    &frozen,
                    &complete_funding.input
                )
                .is_err()
        );
        store
            .rollback_recording_terminal_write(&complete_funding.input)
            .unwrap();
        drop(scope);
        assert_eq!(store.recording_retained_totals().unwrap(), frozen_totals);
        let scope = store
            .begin_recording_terminal_write_with_input(&complete_funding.input)
            .unwrap();
        let encoded_frozen = frozen.encode(&proposal).unwrap();
        let wrong: serde_json::Value = serde_json::from_slice(&encoded_frozen).unwrap();
        for mutation in 0..5 {
            let mut wrong = wrong.clone();
            match mutation {
                0 => wrong["prefix"]["freeze_sequence"] = serde_json::json!(sequence + 1),
                1 => wrong["prefix"]["terminal_id"] = serde_json::json!(Uuid::new_v4()),
                2 => wrong["prefix"]["generation"] = serde_json::json!(2),
                3 => {
                    // A fixed 255 can equal the actual hash byte and leave the
                    // supposedly corrupt fixture unchanged. Always flip a bit.
                    let original = wrong["finalizing_hash"][0].as_u64().unwrap();
                    wrong["finalizing_hash"][0] = serde_json::json!(original ^ 1);
                }
                _ => wrong["version"] = serde_json::json!(5),
            }
            store
                .connection
                .execute(
                    "UPDATE operation_journal_recording_chunks SET record=?1 WHERE slot=20001",
                    params![serde_json::to_vec(&wrong).unwrap()],
                )
                .unwrap();
            assert!(store.verify_recording_reservation(&proposal, true).is_err());
            assert!(
                store
                    .verify_recording_reservation_with_input(
                        &proposal,
                        true,
                        Some(&complete_funding.input)
                    )
                    .is_err()
            );
        }
        store
            .connection
            .execute(
                "UPDATE operation_journal_recording_chunks SET record=?1 WHERE slot=20001",
                params![encoded_frozen],
            )
            .unwrap();

        store
            .connection
            .execute(
                "UPDATE operation_journal_chunk_claim SET logical_offset=logical_offset+1",
                [],
            )
            .unwrap();
        assert!(
            store
                .validate_original_declared_terminal_retention(
                    &proposal,
                    &inventory,
                    &complete_funding.input
                )
                .is_err()
        );
        store
            .rollback_recording_terminal_write(&complete_funding.input)
            .unwrap();
        drop(scope);
        store
            .connection
            .execute(
                "UPDATE operation_journal_recording_chunks SET record=?1 WHERE slot=?2",
                params![encoded, ControlRole::Frozen.slot()],
            )
            .unwrap();
        assert!(store.verify_recording_reservation(&proposal, true).is_err());
    }
}
