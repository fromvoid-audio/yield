// Actual journal/SQLite tests with authority-free synthetic proposal observations.
// No fixture below reserves physical capacity or issues a live source/write grant.
#[test]
fn recording_metadata_policy_tightens_default_and_preserves_stricter_owner_cap() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    store.configure_recording_metadata_policy().unwrap();
    let page_size: i64 = store
        .connection
        .query_row("PRAGMA page_size", [], |r| r.get(0))
        .unwrap();
    let ceiling = crate::operation_journal::recording_commands::MAX_RECORDING_JOURNAL_DATABASE_BYTES
        / page_size as u64;
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA max_page_count", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        ceiling as i64
    );
    let current: i64 = store
        .connection
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .unwrap();
    let tighter = current + 64;
    assert!(tighter < ceiling as i64);
    store
        .connection
        .query_row(&format!("PRAGMA max_page_count={tighter}"), [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
    let proposal =
        crate::operation_journal::recording_commands::recording_proposal_for_test(2, 10_000);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    store
        .retain_recording_chunk(&initial_recording_header(&proposal))
        .unwrap();
    for _ in 0..3 {
        store.configure_recording_metadata_policy().unwrap();
        assert_eq!(
            store
                .connection
                .query_row("PRAGMA max_page_count", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            tighter
        );
    }
    assert!(store.verify_recording_reservation(&proposal, true).unwrap());
}

#[test]
fn recording_exhausted_owner_page_cap_refuses_growth_and_preserves_retained_evidence() {
    use crate::operation_journal::recording_commands::{
        RecordingCapturedChunk, RecordingChunkRole, recording_proposal_for_test,
    };
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 10_000);
    let record = finite_recording_record(&proposal);
    let header = initial_recording_header(&proposal);
    store.insert_intent(&record).unwrap();
    store.retain_recording_chunk(&header).unwrap();
    let before = store.recording_retained_totals().unwrap();
    let current: i64 = store
        .connection
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        store
            .connection
            .query_row(&format!("PRAGMA max_page_count={current}"), [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        current
    );
    let pcm = RecordingCapturedChunk::new(
        &proposal,
        Uuid::new_v4(),
        RecordingChunkRole::Pcm { ordinal: 0 },
        68,
        vec![255; 16_384],
    )
    .unwrap();
    let failure = store.retain_recording_chunk(&pcm).unwrap_err();
    assert!(
        matches!(failure, OperationJournalStoreError::Sql(rusqlite::Error::SqliteFailure(ref code, _)) if code.code == rusqlite::ErrorCode::DiskFull)
    );
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA max_page_count", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        current
    );
    assert_eq!(store.recording_retained_totals().unwrap(), before);
    assert_eq!(
        store.get(proposal.operation_id()).unwrap(),
        Some(record.clone())
    );
    assert!(store.verify_recording_reservation(&proposal, true).unwrap());
    assert_eq!(
        store
            .get_recording_chunk(proposal.operation_id(), header.chunk_id())
            .unwrap(),
        Some(header.clone())
    );
    assert!(
        store
            .get_recording_chunk(proposal.operation_id(), pcm.chunk_id())
            .unwrap()
            .is_none()
    );
    drop(store);
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    assert_eq!(store.recording_retained_totals().unwrap(), before);
    assert_eq!(store.get(proposal.operation_id()).unwrap(), Some(record));
    assert!(store.verify_recording_reservation(&proposal, true).unwrap());
    assert_eq!(
        store
            .get_recording_chunk(proposal.operation_id(), header.chunk_id())
            .unwrap(),
        Some(header)
    );
    assert!(
        store
            .get_recording_chunk(proposal.operation_id(), pcm.chunk_id())
            .unwrap()
            .is_none()
    );
}

fn finite_recording_record(
    proposal: &crate::operation_journal::recording_commands::RecordingProposal,
) -> StoredOperationRecord {
    use crate::operation_journal::full_capacity::{
        FullCapacityAllocation, FullCapacityAllocationClass, FullCapacityPlan,
    };
    use crate::operation_journal::model::{UniversalCommandKind, UniversalIntent};
    let volume = VolumeIdentity { device: 1 };
    let allocations = [
        (
            FullCapacityAllocationClass::DestinationStaging,
            proposal.format().max_encoded_bytes().unwrap(),
        ),
        (
            FullCapacityAllocationClass::JournalRecord,
            crate::operation_journal::recording_commands::RECORDING_JOURNAL_PEAK_BYTES,
        ),
    ]
    .into_iter()
    .map(|(class, bytes)| {
        FullCapacityAllocation::new(
            volume.clone(),
            1,
            class,
            bytes,
            bytes,
            crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR,
        )
        .unwrap()
    })
    .collect();
    StoredOperationRecord::new_v17_recording(
        proposal.operation_id(),
        OperationIntent {
            actor: OperationActor::User,
            kind: OperationKind::FileMutation,
            label: "finite capture".into(),
        },
        UniversalIntent::new(
            None,
            UniversalCommandKind::CreateFile,
            "recording.capture",
            proposal.participants().clone(),
        )
        .unwrap(),
        UniversalCommandArgs::Recording(proposal.clone()),
        FullCapacityPlan::new(allocations).unwrap(),
        1,
    )
}
fn initial_recording_header(
    proposal: &crate::operation_journal::recording_commands::RecordingProposal,
) -> crate::operation_journal::recording_commands::RecordingCapturedChunk {
    crate::operation_journal::recording_commands::RecordingCapturedChunk::new(
        proposal,
        Uuid::new_v4(),
        crate::operation_journal::recording_commands::RecordingChunkRole::Header { revision: 0 },
        0,
        vec![0; 68],
    )
    .unwrap()
}
#[test]
fn recording_v17_preserves_frozen_codec_and_rejects_wrong_schema_capacity_identity() {
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let proposal = recording_proposal_for_test(2, 10_000);
    let record = finite_recording_record(&proposal);
    let decoded = decode(&encode(&record).unwrap()).unwrap();
    assert_eq!(decoded, record);
    assert_eq!(
        super::super::validation::classify_record(&decoded),
        super::super::validation::RecordConsistency::Consistent
    );
    let plan = super::super::work_plan::derive_work_plan(
        decoded.universal_command_args().unwrap(),
        proposal.participants(),
    )
    .unwrap();
    assert!(plan.units().iter().any(|u| u.key() == proposal.work_key()));
    let mut changed = record.clone();
    changed.schema_version = 16;
    assert!(encode(&changed).is_err());
    changed = record.clone();
    changed.operation_id = OperationId::new();
    assert!(encode(&changed).is_err());
    changed = record.clone();
    changed.full_capacity_plan = None;
    assert!(encode(&changed).is_err());
    let mut wire: serde_json::Value = serde_json::from_slice(&encode(&record).unwrap()).unwrap();
    wire["full_capacity_plan"]["allocations"][1]["logical_bytes"] = 1.into();
    assert!(decode(&serde_json::to_vec(&wire).unwrap()).is_err());
}
#[test]
fn recording_retains_actual_bytes_and_exact_conflicts_across_sqlite_reopen() {
    use crate::operation_journal::recording_commands::{
        RecordingCapturedChunk, RecordingChunkRole, recording_proposal_for_test,
    };
    let fixture = fixture();
    let proposal = recording_proposal_for_test(2, 10_000);
    let header = initial_recording_header(&proposal);
    let pcm = RecordingCapturedChunk::new(
        &proposal,
        Uuid::new_v4(),
        RecordingChunkRole::Pcm { ordinal: 0 },
        68,
        vec![255; 16_384],
    )
    .unwrap();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let first = store.retain_recording_chunk(&header).unwrap();
    assert!(matches!(
        first,
        super::recording_chunks::RecordingChunkReceipt::Committed { .. }
    ));
    store.retain_recording_chunk(&pcm).unwrap();
    // Lost reply: drop/reopen before consuming a receipt. Actual bytes remain authoritative.
    drop(store);
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    assert_eq!(
        store.get(proposal.operation_id()).unwrap().unwrap(),
        finite_recording_record(&proposal)
    );
    assert!(matches!(
        store.retain_recording_chunk(&pcm).unwrap(),
        super::recording_chunks::RecordingChunkReceipt::Replay { .. }
    ));
    assert_eq!(
        store
            .get_recording_chunk(proposal.operation_id(), pcm.chunk_id())
            .unwrap()
            .unwrap()
            .bytes(),
        pcm.bytes()
    );
    let conflicting = RecordingCapturedChunk::new(
        &proposal,
        pcm.chunk_id(),
        RecordingChunkRole::Pcm { ordinal: 0 },
        68,
        vec![0; 16_384],
    )
    .unwrap();
    assert!(store.retain_recording_chunk(&conflicting).is_err());
    let changed_id = RecordingCapturedChunk::new(
        &proposal,
        Uuid::new_v4(),
        RecordingChunkRole::Pcm { ordinal: 0 },
        68,
        vec![255; 16_384],
    )
    .unwrap();
    assert!(store.retain_recording_chunk(&changed_id).is_err());
    assert_eq!(
        store
            .recording_chunk_page(proposal.operation_id(), None)
            .unwrap()
            .chunks,
        vec![header, pcm]
    );
    let record = store.get(proposal.operation_id()).unwrap().unwrap();
    assert_eq!(record.phase, OperationPhase::IntentDurable);
    assert!(record.published().is_none());
}
#[test]
fn recording_prefix_ordering_partial_final_slot_and_header_barrier_fail_closed() {
    use crate::operation_journal::recording_commands::{
        RecordingCapturedChunk, RecordingChunkRole, recording_proposal_for_test,
    };
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 10_000);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let pcm = RecordingCapturedChunk::new(
        &proposal,
        Uuid::new_v4(),
        RecordingChunkRole::Pcm { ordinal: 0 },
        68,
        vec![0; 8],
    )
    .unwrap();
    assert!(store.retain_recording_chunk(&pcm).is_err());
    store
        .retain_recording_chunk(&initial_recording_header(&proposal))
        .unwrap();
    store.retain_recording_chunk(&pcm).unwrap();
    let next = RecordingCapturedChunk::new(
        &proposal,
        Uuid::new_v4(),
        RecordingChunkRole::Pcm { ordinal: 1 },
        68 + 16_384,
        vec![0; 8],
    )
    .unwrap();
    assert!(store.retain_recording_chunk(&next).is_err());
    let patch = RecordingCapturedChunk::new(
        &proposal,
        Uuid::new_v4(),
        RecordingChunkRole::Header { revision: 1 },
        0,
        vec![0; 4],
    )
    .unwrap();
    store.retain_recording_chunk(&patch).unwrap();
    assert!(store.retain_recording_chunk(&next).is_err());
    assert_eq!(
        store
            .recording_chunk_page(proposal.operation_id(), None)
            .unwrap()
            .chunks
            .len(),
        3
    );
}
#[test]
fn recording_recovery_pages_remain_bounded_and_unknown_bytes_are_retained() {
    use crate::operation_journal::recording_commands::{
        RecordingCapturedChunk, RecordingChunkRole, recording_proposal_for_test,
    };
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 50_000);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    store
        .retain_recording_chunk(&initial_recording_header(&proposal))
        .unwrap();
    let mut chunks = Vec::new();
    for ordinal in 0..10 {
        let chunk = RecordingCapturedChunk::new(
            &proposal,
            Uuid::new_v4(),
            RecordingChunkRole::Pcm { ordinal },
            68 + u64::from(ordinal) * 16_384,
            vec![255; 16_384],
        )
        .unwrap();
        store.retain_recording_chunk(&chunk).unwrap();
        chunks.push(chunk);
    }
    let first = store
        .recording_chunk_page(proposal.operation_id(), None)
        .unwrap();
    assert_eq!(first.chunks.len(), 8);
    let second = store
        .recording_chunk_page(proposal.operation_id(), first.next_sequence)
        .unwrap();
    assert_eq!(second.chunks.len(), 3);
    assert_eq!(second.next_sequence, None);
    let before = chunks[0].encode(&proposal).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&before).unwrap();
    value["version"] = 999.into();
    let bad = serde_json::to_vec(&value).unwrap();
    store
        .connection
        .execute(
            "UPDATE operation_journal_recording_chunks SET record=?1 WHERE chunk_id=?2",
            params![bad, chunks[0].chunk_id().to_string()],
        )
        .unwrap();
    assert!(
        store
            .recording_chunk_page(proposal.operation_id(), None)
            .is_err()
    );
    let after: Vec<u8> = store
        .connection
        .query_row(
            "SELECT record FROM operation_journal_recording_chunks WHERE chunk_id=?1",
            params![chunks[0].chunk_id().to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after, bad);
}

#[test]
fn recording_transaction_faults_roll_back_or_retain_unconfirmed_exact_bytes() {
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let header = initial_recording_header(&proposal);
    for boundary in [
        InsertTestBoundary::BeforePrecommitValidation,
        InsertTestBoundary::Commit,
    ] {
        store.set_insert_test_hook(move |observed| {
            if observed == boundary {
                InsertTestDirective::Fail(OperationJournalStoreError::RecordingRetention {
                    reason: "injected transactional failure".into(),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        assert!(store.retain_recording_chunk(&header).is_err());
        assert!(
            store
                .get_recording_chunk(proposal.operation_id(), header.chunk_id())
                .unwrap()
                .is_none()
        );
    }
    store.set_insert_test_hook(|boundary| {
        if boundary == InsertTestBoundary::Commit {
            InsertTestDirective::ExecuteThenFail(OperationJournalStoreError::RecordingRetention {
                reason: "lost COMMIT reply".into(),
            })
        } else {
            InsertTestDirective::Continue
        }
    });
    assert!(store.retain_recording_chunk(&header).is_err());
    assert_eq!(
        store
            .get_recording_chunk(proposal.operation_id(), header.chunk_id())
            .unwrap(),
        Some(header.clone())
    );
    assert!(matches!(
        store.retain_recording_chunk(&header).unwrap(),
        super::recording_chunks::RecordingChunkReceipt::Replay { .. }
    ));
}
#[test]
fn recording_schema9_migrates_preserving_existing_intent_and_capped_policy_rejects_trigger() {
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let legacy = intent_record(OperationId::new());
    store.insert_intent(&legacy).unwrap();
    store
        .connection
        .execute_batch("DROP TABLE operation_journal_database_attempt_outcome; DROP TABLE operation_journal_database_attempt; DROP TABLE operation_journal_database_claim_slice; DROP TABLE operation_journal_database_binding; DROP TABLE operation_journal_recording_chunks; PRAGMA user_version=9;")
        .unwrap();
    drop(store);
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    assert_eq!(store.get(legacy.operation_id).unwrap(), Some(legacy));
    let proposal = recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    store.configure_recording_metadata_policy().unwrap();
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA cache_spill", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    store.connection.execute_batch("CREATE TRIGGER recording_trigger AFTER INSERT ON operation_journal_recording_chunks BEGIN SELECT 1; END;").unwrap();
    assert!(
        store
            .retain_recording_chunk(&initial_recording_header(&proposal))
            .is_err()
    );
    store
        .connection
        .execute_batch("DROP TRIGGER recording_trigger;")
        .unwrap();
    assert!(
        store
            .recording_chunk_page(proposal.operation_id(), None)
            .unwrap()
            .chunks
            .is_empty()
    );
}
#[cfg(unix)]
#[test]
fn recording_replaced_journal_rejects_before_retention_effect() {
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let moved = store.database_path.with_extension("retained-old");
    std::fs::rename(&store.database_path, &moved).unwrap();
    std::fs::write(&store.database_path, b"replacement must remain unchanged").unwrap();
    assert!(
        store
            .retain_recording_chunk(&initial_recording_header(&proposal))
            .is_err()
    );
    assert_eq!(
        std::fs::read(&store.database_path).unwrap(),
        b"replacement must remain unchanged"
    );
}
#[test]
fn recording_owner_recovers_actual_bytes_without_minting_capacity_or_write_authority() {
    use crate::operation_journal::owner::OperationJournalOwner;
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let header = initial_recording_header(&proposal);
    store.retain_recording_chunk(&header).unwrap();
    let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
    let client = owner.client();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut request = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "owner initialization timeout"
        );
        match client.recording_chunk_page(proposal.operation_id(), None) {
            Ok(request) => break request,
            Err(crate::operation_journal::owner::OperationJournalOwnerQueueError::Initializing) => {
                std::thread::yield_now()
            }
            Err(e) => panic!("recovery queue: {e}"),
        }
    };
    let page = request
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap()
        .unwrap();
    assert_eq!(page.chunks, vec![header]);
    assert!(owner.shutdown());
    assert!(matches!(
        client.recording_chunk_page(proposal.operation_id(), None),
        Err(crate::operation_journal::owner::OperationJournalOwnerQueueError::Closed)
    ));
}
#[test]
fn recording_retention_capacity_exhaustion_preserves_existing_rows_and_rejects_before_insert() {
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    store.connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
    for slot in 0..8193 {
        store.connection.execute("INSERT INTO operation_journal_recording_chunks(chunk_id,operation_id,slot,sequence,record) VALUES(?1,?2,?3,?4,?5)",params![Uuid::new_v4().to_string(),OperationId::new().to_string(),slot,slot+10,b"retained unknown bytes".as_slice()]).unwrap();
    }
    store.connection.execute_batch("COMMIT").unwrap();
    assert!(
        store
            .retain_recording_chunk(&initial_recording_header(&proposal))
            .is_err()
    );
    let count: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM operation_journal_recording_chunks",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 8193 + 4); // Previously reserved controls remain intact too.
    assert!(
        store
            .recording_chunk_page(proposal.operation_id(), None)
            .unwrap()
            .chunks
            .is_empty()
    );
}
#[test]
fn recording_budget_binding_requires_actual_journal_descriptor_volume() {
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let proposal = recording_proposal_for_test(2, 100);
    let record = finite_recording_record(&proposal);
    let opened = crate::sample_sources::db::sqlite_identity::open_existing_database_file(
        &store.database_root,
        std::path::Path::new(JOURNAL_DATABASE_FILE_NAME),
        &store.database_path,
    )
    .unwrap();
    let actual = crate::durable_capacity::descriptor_capacity_facts(&opened.file).unwrap();
    assert_ne!(
        actual.identity,
        record.full_capacity_plan.as_ref().unwrap().allocations()[1]
            .volume()
            .clone(),
        "fixture must use a different volume"
    );
    // Synthetic scalar/volume in a codec fixture is not live metadata admission.
    assert!(
        store
            .validate_recording_capacity_binding(
                &proposal,
                record.full_capacity_plan.as_ref().unwrap()
            )
            .is_err()
    );
}

#[test]
fn recording_retained_bytes_block_clean_handoff_even_without_a_claim_fixture() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    store.prove_clean_handoff_durable().unwrap();
    let proposal =
        crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let header = initial_recording_header(&proposal);
    store.retain_recording_chunk(&header).unwrap();
    assert!(store.prove_clean_handoff_durable().is_err());
    assert_eq!(
        store
            .get_recording_chunk(proposal.operation_id(), header.chunk_id())
            .unwrap(),
        Some(header)
    );
}
#[test]
fn recording_full_progress_observation_retains_the_same_operation_identity() {
    use crate::operation_journal::{
        AdmissionClaim, AdmissionClaimId, AdmissionClaimInventory, FullProgressIntentObservation,
    };
    let proposal =
        crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
    let record = finite_recording_record(&proposal);
    let capacity = record.full_capacity_plan.unwrap();
    let claims = AdmissionClaimInventory::new(
        capacity
            .allocations()
            .iter()
            .map(|a| {
                AdmissionClaim::new(
                    AdmissionClaimId::from_uuid(Uuid::new_v4()).unwrap(),
                    a.volume().clone(),
                    a.class(),
                    a.allocated_bytes(),
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
    )
    .unwrap();
    let args = UniversalCommandArgs::Recording(proposal.clone());
    let plan = super::super::work_plan::derive_work_plan(&args, proposal.participants()).unwrap();
    let observation = FullProgressIntentObservation::new(
        proposal.operation_id(),
        args.clone(),
        proposal.participants().clone(),
        plan.clone(),
        capacity.clone(),
        claims.clone(),
    )
    .unwrap();
    let decoded: FullProgressIntentObservation =
        serde_json::from_slice(&serde_json::to_vec(&observation).unwrap()).unwrap();
    assert_eq!(decoded, observation);
    assert!(
        FullProgressIntentObservation::new(
            OperationId::new(),
            args,
            proposal.participants().clone(),
            plan,
            capacity,
            claims
        )
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn recording_replaced_profile_rejects_before_retention_effect() {
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let proposal =
        crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let root = fixture.owner.profile_root().to_path_buf();
    let displaced = root.with_file_name("recording-displaced-root");
    fs::rename(&root, &displaced).unwrap();
    fs::create_dir(&root).unwrap();
    assert!(
        store
            .retain_recording_chunk(&initial_recording_header(&proposal))
            .is_err()
    );
    let rows: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM operation_journal_recording_chunks",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 4); // Initial reservations survived; no captured header was inserted.
    drop(store);
    fs::remove_dir_all(&root).unwrap();
    fs::rename(displaced, root).unwrap();
}
#[test]
fn recording_retention_blocks_ordinary_charge_release_before_retirement_implementation() {
    use crate::operation_journal::{
        AdmissionClaim, AdmissionClaimId, AdmissionClaimInventory, FullCapacityAllocationClass,
    };
    let fixture = fixture();
    let store = OperationJournalStore::open(&fixture.owner).unwrap();
    let proposal =
        crate::operation_journal::recording_commands::recording_proposal_for_test(2, 100);
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let claim_id = AdmissionClaimId::new();
    let claims = AdmissionClaimInventory::new(vec![
        AdmissionClaim::new(
            claim_id,
            VolumeIdentity { device: 1 },
            FullCapacityAllocationClass::JournalRecord,
            4096,
        )
        .unwrap(),
    ])
    .unwrap();
    // Actual durable claim observations, not a physical reservation/provider fixture.
    ordinary_claims::insert_rows_inside_transaction(
        &store.connection,
        proposal.operation_id(),
        &claims,
    )
    .unwrap();
    // Reserved-only metadata already prevents release before any captured bytes exist.
    assert!(
        store
            .persist_ordinary_claim_release(release_request(
                proposal.operation_id(),
                claim_id,
                Uuid::new_v4()
            ))
            .is_err()
    );
    assert!(
        ordinary_claims::read_release(&store.connection, claim_id)
            .unwrap()
            .is_none()
    );
    store
        .retain_recording_chunk(&initial_recording_header(&proposal))
        .unwrap();
    assert!(
        store
            .persist_ordinary_claim_release(release_request(
                proposal.operation_id(),
                claim_id,
                Uuid::new_v4()
            ))
            .is_err()
    );
    assert!(
        ordinary_claims::read_release(&store.connection, claim_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn recording_rebuild_refuses_unknown_retained_control_after_reopen_without_releasing() {
    use crate::operation_journal::recording_commands::{
        controls::{ControlRole, RecordingControlReservation},
        recording_proposal_for_test,
    };
    let f = fixture();
    let proposal = recording_proposal_for_test(2, 100);
    let store = OperationJournalStore::open(&f.owner).unwrap();
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    store
        .retain_recording_chunk(&initial_recording_header(&proposal))
        .unwrap();
    assert!(store.rebuild_admission_snapshot().is_ok());
    let control = RecordingControlReservation::new(&proposal, ControlRole::Frozen).unwrap();
    store
        .connection
        .execute(
            "UPDATE operation_journal_recording_chunks SET record=?1 WHERE chunk_id=?2",
            params![
                b"unknown retained terminal control".as_slice(),
                control.id().to_string()
            ],
        )
        .unwrap();
    let before = store.recording_retained_totals().unwrap();
    drop(store);
    let store = OperationJournalStore::open(&f.owner).unwrap();
    assert!(
        store.rebuild_admission_snapshot().is_err(),
        "unknown retained terminal control must refuse rebuilt admission"
    );
    assert!(store.rebuild_ordinary_claim_snapshot().is_err());
    assert_eq!(store.recording_retained_totals().unwrap(), before);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT record FROM operation_journal_recording_chunks WHERE chunk_id=?1",
                [control.id().to_string()],
                |r| r.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
        b"unknown retained terminal control"
    );
}

#[test]
fn recording_rebuild_audits_paged_actual_prefix_and_readonly_historical_inventory() {
    use crate::operation_journal::recording_commands::{
        RecordingCapturedChunk, RecordingChunkRole, recording_proposal_for_test,
    };
    let f = fixture();
    let proposal = recording_proposal_for_test(2, 100_000);
    let store = OperationJournalStore::open(&f.owner).unwrap();
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    // A reservation alone is valid retained evidence, not an empty/released operation.
    assert!(store.rebuild_admission_snapshot().is_ok());
    store
        .retain_recording_chunk(&initial_recording_header(&proposal))
        .unwrap();
    for ordinal in 0..9 {
        let pcm = RecordingCapturedChunk::new(
            &proposal,
            Uuid::new_v4(),
            RecordingChunkRole::Pcm { ordinal },
            68 + u64::from(ordinal) * 16_384,
            vec![0; 16_384],
        )
        .unwrap();
        store.retain_recording_chunk(&pcm).unwrap();
    }
    let patch = RecordingCapturedChunk::new(
        &proposal,
        Uuid::new_v4(),
        RecordingChunkRole::Header { revision: 1 },
        4,
        vec![0; 4],
    )
    .unwrap();
    store.retain_recording_chunk(&patch).unwrap();
    let totals = store.recording_retained_totals().unwrap();
    drop(store);
    let store = OperationJournalStore::open(&f.owner).unwrap();
    assert!(store.rebuild_admission_snapshot().is_ok());
    assert!(store.rebuild_ordinary_claim_snapshot().is_ok());
    assert_eq!(store.recording_retained_totals().unwrap(), totals);
    assert!(store.prove_clean_handoff_durable().is_err());
    // Historical zero-control rows remain observable. Rebuild cannot retrofit capture authority.
    store
        .connection
        .execute(
            "DELETE FROM operation_journal_recording_chunks WHERE slot BETWEEN 20000 AND 20003",
            [],
        )
        .unwrap();
    assert!(store.rebuild_admission_snapshot().is_ok());
    assert!(store.retain_recording_chunk(&patch).is_err());
    assert!(store.prove_clean_handoff_durable().is_err());
}

#[test]
fn recording_rebuild_refuses_orphan_binding_order_and_sequence_corruption_without_mutation() {
    use crate::operation_journal::recording_commands::{
        RecordingCapturedChunk, RecordingChunkRole, recording_proposal_for_test,
    };
    for case in 0..8 {
        let f = fixture();
        let proposal = recording_proposal_for_test(2, 100_000);
        let store = OperationJournalStore::open(&f.owner).unwrap();
        store
            .insert_intent(&finite_recording_record(&proposal))
            .unwrap();
        let header = initial_recording_header(&proposal);
        store.retain_recording_chunk(&header).unwrap();
        let pcm: Vec<_> = (0..2)
            .map(|ordinal| {
                RecordingCapturedChunk::new(
                    &proposal,
                    Uuid::new_v4(),
                    RecordingChunkRole::Pcm { ordinal },
                    68 + u64::from(ordinal) * 16_384,
                    vec![0; 16_384],
                )
                .unwrap()
            })
            .collect();
        for chunk in &pcm {
            store.retain_recording_chunk(chunk).unwrap();
        }
        let patch = RecordingCapturedChunk::new(
            &proposal,
            Uuid::new_v4(),
            RecordingChunkRole::Header { revision: 1 },
            4,
            vec![0; 4],
        )
        .unwrap();
        store.retain_recording_chunk(&patch).unwrap();
        assert!(store.rebuild_admission_snapshot().is_ok());
        match case {
            0 => {
                store.connection.execute("UPDATE operation_journal_recording_chunks SET operation_id=?1 WHERE chunk_id=?2",
                params![OperationId::new().to_string(),header.chunk_id().to_string()]).unwrap();
            }
            1 => {
                let other = recording_proposal_for_test(2, 100_000);
                let foreign = RecordingCapturedChunk::new(
                    &other,
                    header.chunk_id(),
                    RecordingChunkRole::Header { revision: 0 },
                    0,
                    vec![0; 68],
                )
                .unwrap();
                store
                    .connection
                    .execute(
                        "UPDATE operation_journal_recording_chunks SET record=?1 WHERE chunk_id=?2",
                        params![
                            foreign.encode(&other).unwrap(),
                            header.chunk_id().to_string()
                        ],
                    )
                    .unwrap();
            }
            2 => {
                store.connection.execute("UPDATE operation_journal_recording_chunks SET chunk_id=?1 WHERE chunk_id=?2",
                params![format!("{{{}}}", header.chunk_id()),header.chunk_id().to_string()]).unwrap();
            }
            3 => {
                store.connection.execute("UPDATE operation_journal_recording_chunks SET sequence=1 WHERE chunk_id=?1",
                [header.chunk_id().to_string()]).unwrap();
            }
            4 => {
                store
                    .connection
                    .execute(
                        "DELETE FROM operation_journal_recording_chunks WHERE chunk_id=?1",
                        [pcm[0].chunk_id().to_string()],
                    )
                    .unwrap();
            }
            5 => {
                store.connection.execute("UPDATE operation_journal_recording_chunks SET sequence=1999 WHERE chunk_id=?1",
                [pcm[1].chunk_id().to_string()]).unwrap();
            }
            6 => {
                let short = RecordingCapturedChunk::new(
                    &proposal,
                    pcm[0].chunk_id(),
                    RecordingChunkRole::Pcm { ordinal: 0 },
                    68,
                    vec![0; 8],
                )
                .unwrap();
                store
                    .connection
                    .execute(
                        "UPDATE operation_journal_recording_chunks SET record=?1 WHERE chunk_id=?2",
                        params![
                            short.encode(&proposal).unwrap(),
                            pcm[0].chunk_id().to_string()
                        ],
                    )
                    .unwrap();
            }
            _ => {
                let gap = RecordingCapturedChunk::new(
                    &proposal,
                    patch.chunk_id(),
                    RecordingChunkRole::Header { revision: 2 },
                    4,
                    vec![0; 4],
                )
                .unwrap();
                store.connection.execute("UPDATE operation_journal_recording_chunks SET slot=10002,record=?1 WHERE chunk_id=?2",
                    params![gap.encode(&proposal).unwrap(),patch.chunk_id().to_string()]).unwrap();
            }
        }
        let read_rows = |store: &OperationJournalStore| {
            store.connection.prepare("SELECT chunk_id,operation_id,slot,sequence,record FROM operation_journal_recording_chunks ORDER BY slot").unwrap()
                .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?,r.get::<_,Vec<u8>>(4)?))).unwrap()
                .collect::<Result<Vec<_>,_>>().unwrap()
        };
        let before = read_rows(&store);
        let totals = store.recording_retained_totals();
        drop(store);
        let store = OperationJournalStore::open(&f.owner).unwrap();
        assert!(store.rebuild_admission_snapshot().is_err(), "case {case}");
        assert!(
            store.rebuild_ordinary_claim_snapshot().is_err(),
            "case {case}"
        );
        assert_eq!(read_rows(&store), before, "case {case}");
        if let Ok(totals) = totals {
            assert_eq!(store.recording_retained_totals().unwrap(), totals);
        }
        assert!(store.connection.is_autocommit());
    }
}

#[test]
fn recording_prestart_reads_exact_empty_reservation_and_refuses_capture_without_mutation() {
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let f = fixture();
    let proposal = recording_proposal_for_test(2, 100);
    let store = OperationJournalStore::open(&f.owner).unwrap();
    store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    let before = store.recording_retained_totals().unwrap();
    assert_eq!(
        store
            .recording_prestart_proposal(proposal.operation_id())
            .unwrap(),
        proposal
    );
    assert_eq!(store.recording_retained_totals().unwrap(), before);
    drop(store);
    let store = OperationJournalStore::open(&f.owner).unwrap();
    assert_eq!(
        store
            .recording_prestart_proposal(proposal.operation_id())
            .unwrap(),
        proposal
    );
    store
        .retain_recording_chunk(&initial_recording_header(&proposal))
        .unwrap();
    let captured = store.recording_retained_totals().unwrap();
    assert!(
        store
            .recording_prestart_proposal(proposal.operation_id())
            .is_err()
    );
    assert_eq!(store.recording_retained_totals().unwrap(), captured);
}

#[test]
fn recording_prestart_refuses_missing_or_unknown_controls_without_retrofit() {
    use crate::operation_journal::recording_commands::{
        controls::{ControlRole, RecordingControlReservation},
        recording_proposal_for_test,
    };
    for missing in [true, false] {
        let f = fixture();
        let proposal = recording_proposal_for_test(2, 100);
        let store = OperationJournalStore::open(&f.owner).unwrap();
        store
            .insert_intent(&finite_recording_record(&proposal))
            .unwrap();
        let control = RecordingControlReservation::new(&proposal, ControlRole::Frozen).unwrap();
        if missing {
            store
                .connection
                .execute(
                    "DELETE FROM operation_journal_recording_chunks WHERE chunk_id=?1",
                    [control.id().to_string()],
                )
                .unwrap();
        } else {
            store
                .connection
                .execute(
                    "UPDATE operation_journal_recording_chunks SET record=?1 WHERE chunk_id=?2",
                    params![b"unknown control".as_slice(), control.id().to_string()],
                )
                .unwrap();
        }
        let before = store.recording_retained_totals().unwrap();
        let rows: Vec<(String, Vec<u8>)> = store
            .connection
            .prepare(
                "SELECT chunk_id,record FROM operation_journal_recording_chunks ORDER BY chunk_id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            store
                .recording_prestart_proposal(proposal.operation_id())
                .is_err()
        );
        assert_eq!(store.recording_retained_totals().unwrap(), before);
        let after: Vec<(String, Vec<u8>)> = store
            .connection
            .prepare(
                "SELECT chunk_id,record FROM operation_journal_recording_chunks ORDER BY chunk_id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(after, rows);
    }
}

#[test]
fn recording_prestart_refuses_actual_cancel_disposition_and_preserves_reservation() {
    use crate::operation_journal::recording_commands::recording_proposal_for_test;
    let f = fixture();
    let proposal = recording_proposal_for_test(2, 100);
    let store = OperationJournalStore::open(&f.owner).unwrap();
    let inserted = store
        .insert_intent(&finite_recording_record(&proposal))
        .unwrap();
    store
        .request_cancel_before_publish(
            proposal.operation_id(),
            inserted.sequence(),
            CancellationRequestObservation::new(2).unwrap(),
        )
        .unwrap();
    let before = store.recording_retained_totals().unwrap();
    assert!(
        store
            .recording_prestart_proposal(proposal.operation_id())
            .is_err()
    );
    assert_eq!(store.recording_retained_totals().unwrap(), before);
}
