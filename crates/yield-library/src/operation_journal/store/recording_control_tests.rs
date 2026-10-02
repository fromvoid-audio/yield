// Actual SQLite initial transactions/reopen/fault tests with authority-free proposal inputs.
// These do not qualify a protected physical reserve or a terminal/output capability.
#[test]
fn recording_controls_commit_before_bytes_and_reopen_exact_replay_without_releasing() {
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let f = fixture();
    let proposal = recording_proposal_for_test(2, 100);
    let record = finite_recording_record(&proposal);
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let receipt = store.insert_intent(&record).unwrap();
    assert_eq!(receipt.sequence, 1);
    assert_eq!(store.recording_retained_totals().unwrap(), (4, 16384));
    assert!(store.verify_recording_reservation(&proposal, true).unwrap());
    assert!(
        store
            .recording_chunk_page(proposal.operation_id(), None)
            .unwrap()
            .chunks
            .is_empty()
    );
    assert!(store.prove_clean_handoff_durable().is_err());
    assert!(
        store
            .reserve_recording_initial_inside_transaction(&record)
            .is_err()
    );
    store.connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
    store
        .reserve_recording_initial_inside_transaction(&record)
        .unwrap();
    store.connection.execute_batch("COMMIT").unwrap();
    assert_eq!(store.recording_retained_totals().unwrap(), (4, 16384));
    let header = initial_recording_header(&proposal);
    let receipt = store.retain_recording_chunk(&header).unwrap();
    assert_eq!(
        receipt,
        super::recording_chunks::RecordingChunkReceipt::Committed { sequence: 6 }
    );
    drop(store);
    let store = OperationJournalStore::open(&f.owner).unwrap();
    store.verify_recording_initial_readback(&record).unwrap();
    assert_eq!(
        store
            .get_recording_chunk(proposal.operation_id(), header.chunk_id())
            .unwrap(),
        Some(header.clone())
    );
    assert_eq!(
        store
            .recording_chunk_page(proposal.operation_id(), None)
            .unwrap()
            .chunks,
        vec![header]
    );
    assert!(store.insert_intent(&record).is_err()); // Preserve existing initial duplicate policy.
}

#[test]
fn recording_controls_initial_faults_are_atomic_and_lost_commit_reply_retains_all_rows() {
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    for boundary in [
        InsertTestBoundary::BeforePrecommitValidation,
        InsertTestBoundary::Commit,
    ] {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let proposal = recording_proposal_for_test(2, 100);
        store.set_insert_test_hook(move |at| {
            if at == boundary {
                InsertTestDirective::Fail(OperationJournalStoreError::RecordingRetention {
                    reason: "atomic fault".into(),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        assert!(
            store
                .insert_intent(&finite_recording_record(&proposal))
                .is_err()
        );
        assert!(store.get(proposal.operation_id()).unwrap().is_none());
        assert_eq!(store.recording_retained_totals().unwrap(), (0, 0));
    }
    for boundary in [InsertTestBoundary::Commit, InsertTestBoundary::AfterCommit] {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let proposal = recording_proposal_for_test(2, 100);
        let record = finite_recording_record(&proposal);
        store.set_insert_test_hook(move |at| {
            if at == boundary {
                InsertTestDirective::ExecuteThenFail(
                    OperationJournalStoreError::RecordingRetention {
                        reason: "lost reply".into(),
                    },
                )
            } else {
                InsertTestDirective::Continue
            }
        });
        // The existing initial AfterCommit hook recognizes Fail, not ExecuteThenFail.
        if boundary == InsertTestBoundary::AfterCommit {
            store.set_insert_test_hook(move |at| {
                if at == boundary {
                    InsertTestDirective::Fail(OperationJournalStoreError::RecordingRetention {
                        reason: "lost readback reply".into(),
                    })
                } else {
                    InsertTestDirective::Continue
                }
            });
        }
        assert!(store.insert_intent(&record).is_err());
        drop(store);
        let store = OperationJournalStore::open(&f.owner).unwrap();
        assert_eq!(
            store.get(proposal.operation_id()).unwrap(),
            Some(record.clone())
        );
        store.verify_recording_initial_readback(&record).unwrap();
        assert_eq!(store.recording_retained_totals().unwrap(), (4, 16384));
    }
}

#[test]
fn recording_controls_unknown_conflicting_and_partial_rows_are_retained_not_filtered() {
    use crate::operation_journal::recording_commands::{
        controls::{ControlRole, RecordingControlReservation},
        recording_proposal_for_test,
    };
    for case in 0..8 {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let proposal = recording_proposal_for_test(2, 100);
        store
            .insert_intent(&finite_recording_record(&proposal))
            .unwrap();
        let header = initial_recording_header(&proposal);
        store.retain_recording_chunk(&header).unwrap();
        let control = RecordingControlReservation::new(&proposal, ControlRole::Frozen).unwrap();
        match case {
            0 => {
                store
                    .connection
                    .execute(
                        "DELETE FROM operation_journal_recording_chunks WHERE chunk_id=?1",
                        params![control.id().to_string()],
                    )
                    .unwrap();
            }
            1 => {
                store
                    .connection
                    .execute(
                        "UPDATE operation_journal_recording_chunks SET record=?1 WHERE chunk_id=?2",
                        params![b"{\"version\":2}".as_slice(), control.id().to_string()],
                    )
                    .unwrap();
            }
            2 => {
                store.connection.execute("UPDATE operation_journal_recording_chunks SET chunk_id=?1 WHERE chunk_id=?2", params![Uuid::new_v4().to_string(), control.id().to_string()]).unwrap();
            }
            3 => {
                store.connection.execute("UPDATE operation_journal_recording_chunks SET slot=20004 WHERE chunk_id=?1", params![control.id().to_string()]).unwrap();
            }
            4 => {
                store
                    .connection
                    .execute(
                        "UPDATE operation_journal_recording_chunks SET record=?1 WHERE chunk_id=?2",
                        params![vec![0u8; 4097], control.id().to_string()],
                    )
                    .unwrap();
            }
            6 => {
                store.connection.execute("UPDATE operation_journal_recording_chunks SET sequence=1000 WHERE chunk_id=?1", params![control.id().to_string()]).unwrap();
            }
            7 => {
                // Keep four controls contiguous but put them after already captured bytes.
                store.connection.execute("UPDATE operation_journal_recording_chunks SET sequence=sequence+1000 WHERE slot BETWEEN 20000 AND 20003", []).unwrap();
            }
            _ => {
                let other = recording_proposal_for_test(2, 100);
                let wrong = RecordingControlReservation::new(&other, ControlRole::Frozen).unwrap();
                store
                    .connection
                    .execute(
                        "UPDATE operation_journal_recording_chunks SET record=?1 WHERE chunk_id=?2",
                        params![wrong.encode(&other).unwrap(), control.id().to_string()],
                    )
                    .unwrap();
            }
        }
        let before: Vec<(String, String, i64, i64, Vec<u8>)> = store
            .connection
            .prepare(
                "SELECT chunk_id,operation_id,slot,sequence,record FROM operation_journal_recording_chunks ORDER BY chunk_id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            store
                .recording_chunk_page(proposal.operation_id(), None)
                .is_err(),
            "case{case}"
        );
        assert!(
            store
                .get_recording_chunk(proposal.operation_id(), header.chunk_id())
                .is_err()
        );
        assert!(store.retain_recording_chunk(&header).is_err());
        assert!(store.prove_clean_handoff_durable().is_err());
        let after: Vec<(String, String, i64, i64, Vec<u8>)> = store
            .connection
            .prepare(
                "SELECT chunk_id,operation_id,slot,sequence,record FROM operation_journal_recording_chunks ORDER BY chunk_id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(before, after);
    }
}

#[test]
fn recording_controls_reserve_full_rows_and_bytes_before_any_initial_commit() {
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    // Adversarial retained historical blobs still occupy real SQLite rows/bytes. They are
    // deliberately not a model of physical reserve authority or valid completed recordings.
    for byte_limit in [false, true] {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let proposal = recording_proposal_for_test(2, 100);
        let operation = OperationId::new();
        let second_operation = OperationId::new();
        store.connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let target_bytes =
            crate::operation_journal::recording_commands::MAX_RECORDING_RETAINED_ENCODED_BYTES
                as i64
                - 16384
                + 1;
        let rows = if byte_limit { 1024 } else { 8189 };
        let mut charged = 0;
        {
            let mut insert = store.connection.prepare("INSERT INTO operation_journal_recording_chunks(chunk_id,operation_id,slot,sequence,record) VALUES(?1,?2,?3,?4,?5)").unwrap();
            for index in 0..rows {
                let size = if byte_limit {
                    (target_bytes - charged).min(131072) as usize
                } else {
                    1
                };
                let blob = vec![b'?'; size];
                charged += size as i64;
                let op = if index < 8176 {
                    operation
                } else {
                    second_operation
                };
                insert
                    .execute(params![
                        Uuid::new_v4().to_string(),
                        op.to_string(),
                        index % 8176,
                        index + 100,
                        blob
                    ])
                    .unwrap();
            }
        }
        store.connection.execute_batch("COMMIT").unwrap();
        assert_eq!(store.recording_retained_totals().unwrap(), (rows, charged));
        assert!(
            store
                .insert_intent(&finite_recording_record(&proposal))
                .is_err()
        );
        assert!(store.get(proposal.operation_id()).unwrap().is_none());
        assert_eq!(store.recording_retained_totals().unwrap(), (rows, charged));
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM operation_journal_recording_chunks WHERE operation_id=?1",
                    params![proposal.operation_id().to_string()],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
}

#[test]
fn recording_controls_corrupted_after_commit_are_unacknowledged_and_retained() {
    use crate::operation_journal::recording_commands::{
        controls::{ControlRole, RecordingControlReservation},
        recording_proposal_for_test,
    };
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    let record = finite_recording_record(&proposal);
    let id = RecordingControlReservation::new(&proposal, ControlRole::Frozen)
        .unwrap()
        .id();
    // Inject corruption through the retained owner after actual COMMIT and before readback.
    *store.recording_readback_corruption.lock().unwrap() = Some(id);
    let outcome = store.insert_intent_with_outcome(&record);
    match outcome {
        JournalInsertOutcome::Failed(failure) => assert_eq!(
            failure.stage(),
            JournalInsertWriteStage::CommittedButNotAcknowledged
        ),
        _ => panic!("corrupt committed controls cannot be acknowledged"),
    }
    assert_eq!(store.get(proposal.operation_id()).unwrap(), Some(record));
    assert_eq!(store.recording_retained_totals().unwrap(), (4, 16384));
    assert!(
        store
            .recording_chunk_page(proposal.operation_id(), None)
            .is_err()
    );
    assert!(store.prove_clean_handoff_durable().is_err());
}

#[test]
fn recording_controls_historical_capture_bytes_remain_readable_but_cannot_resume() {
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let header = initial_recording_header(&proposal);
    store.retain_recording_chunk(&header).unwrap();
    // A genuine old-schema capture has no terminal reservation rows. Model that persisted
    // legacy image; observation remains available, and no retrofit or live byte replay occurs.
    store
        .connection
        .execute(
            "DELETE FROM operation_journal_recording_chunks WHERE slot>=20000",
            [],
        )
        .unwrap();
    assert_eq!(
        store
            .get_recording_chunk(proposal.operation_id(), header.chunk_id())
            .unwrap(),
        Some(header.clone())
    );
    assert_eq!(
        store
            .recording_chunk_page(proposal.operation_id(), None)
            .unwrap()
            .chunks,
        vec![header.clone()]
    );
    assert!(store.retain_recording_chunk(&header).is_err());
    store.connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
    assert!(
        store
            .reserve_recording_initial_inside_transaction(&finite_recording_record(&proposal))
            .is_err()
    );
    store.connection.execute_batch("ROLLBACK").unwrap();
    assert!(store.prove_clean_handoff_durable().is_err());
}

#[test]
fn recording_controls_mid_insert_identity_collision_rolls_back_only_the_new_intent() {
    use crate::operation_journal::recording_commands::{
        controls::{ControlRole, RecordingControlReservation},
        recording_proposal_for_test,
    };
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    let collision = RecordingControlReservation::new(&proposal, ControlRole::Frozen).unwrap();
    let foreign = OperationId::new();
    // First reservation row would insert, then the second collides with retained evidence.
    store.connection.execute("INSERT INTO operation_journal_recording_chunks(chunk_id,operation_id,slot,sequence,record) VALUES(?1,?2,0,100,?3)",
        params![collision.id().to_string(), foreign.to_string(), b"retained foreign evidence".as_slice()]).unwrap();
    let before = store.recording_retained_totals().unwrap();
    assert!(
        store
            .insert_intent(&finite_recording_record(&proposal))
            .is_err()
    );
    assert!(store.get(proposal.operation_id()).unwrap().is_none());
    assert_eq!(store.recording_retained_totals().unwrap(), before);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT record FROM operation_journal_recording_chunks WHERE chunk_id=?1",
                params![collision.id().to_string()],
                |r| r.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
        b"retained foreign evidence"
    );
}

#[test]
fn recording_controls_reserved_only_rows_block_chunk_charge_release() {
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    let record = finite_recording_record(&proposal);
    store.insert_intent(&record).unwrap();
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
    // Durable logical claim observations only; no provider lease or physical grant is issued.
    store.connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
    ordinary_claims::insert_rows_inside_transaction(
        &store.connection,
        proposal.operation_id(),
        &request.inventory().unwrap(),
    )
    .unwrap();
    chunk_claims::insert_claimed_inside_transaction(&store.connection, &request).unwrap();
    store.connection.execute_batch("COMMIT").unwrap();
    assert!(
        store
            .release_chunk_claim(
                request.chunk_id(),
                proposal.operation_id(),
                None,
                Uuid::new_v4()
            )
            .is_err()
    );
    assert_eq!(
        store
            .read_chunk_claim(request.chunk_id())
            .unwrap()
            .unwrap()
            .state(),
        ChunkClaimState::Claimed
    );
    assert!(
        ordinary_claims::read_release(&store.connection, request.claim().claim_id())
            .unwrap()
            .is_none()
    );
    assert_eq!(store.recording_retained_totals().unwrap(), (4, 16384));
}

#[test]
fn recording_controls_oversized_capture_inventory_rejects_point_observation_without_repair() {
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let f = fixture();
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let header = initial_recording_header(&proposal);
    store.retain_recording_chunk(&header).unwrap();
    // Adversarial persisted corruption, not valid captured bytes or reserve qualification.
    store.connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
    {
        let mut insert = store.connection.prepare("INSERT INTO operation_journal_recording_chunks(chunk_id,operation_id,slot,sequence,record) VALUES(?1,?2,?3,?4,?5)").unwrap();
        for ordinal in 0..8188 {
            insert
                .execute(params![
                    Uuid::new_v4().to_string(),
                    proposal.operation_id().to_string(),
                    ordinal,
                    ordinal + 100,
                    b"?".as_slice()
                ])
                .unwrap();
        }
    }
    store.connection.execute_batch("COMMIT").unwrap();
    assert!(
        store
            .get_recording_chunk(proposal.operation_id(), header.chunk_id())
            .is_err()
    );
    assert!(
        store
            .recording_chunk_page(proposal.operation_id(), None)
            .is_err()
    );
    assert!(store.retain_recording_chunk(&header).is_err());
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM operation_journal_recording_chunks",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        8193
    );
    let retained: Vec<u8> = store
        .connection
        .query_row(
            "SELECT record FROM operation_journal_recording_chunks WHERE chunk_id=?1",
            params![header.chunk_id().to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, header.encode(&proposal).unwrap());
    assert!(store.prove_clean_handoff_durable().is_err());
}
