// All positives are logical attempt observations, on actual owned SQLite. No live source/global
// reservation, physical allocation, transaction ticket or qualified receipt is fabricated.
#[cfg(test)]
mod attempt_integration {
    use super::*;
    use crate::operation_journal::database_attempts::{
        AttemptOutcomeKind, DatabaseAttemptOutcome, DatabaseAttemptStart, MAX_ATTEMPTS,
        MAX_ATTEMPTS_PER_OPERATION,
    };

    #[test]
    fn durable_attempt_ledger_real_sqlite_child_suite() {
        let output=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","operation_journal::store::tests::exact_database_claim_persistence::attempt_integration::durable_attempt_ledger_child","--nocapture"])
            .env("YIELD_ATTEMPT_LEDGER_CHILD","1").output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
    }
    include!("database_attempt_scalar_projection_tests.rs");
    include!("database_attempt_inventory_backing_tests.rs");
    include!("database_attempt_initial_borrow_tests.rs");
    include!("database_attempt_context_funding_tests.rs");
    include!("database_attempt_image_funding_tests.rs");
    include!("database_attempt_audit_funding_tests.rs");

    fn start(
        record: &StoredOperationRecord,
        inventory: &DatabaseClaimInventory,
        index: usize,
    ) -> DatabaseAttemptStart {
        let binding = &inventory.bindings()[index];
        let db = inventory
            .slices()
            .iter()
            .find(|s| {
                s.binding == binding.id
                    && s.class == FullCapacityAllocationClass::SourceDatabaseCommit
            })
            .unwrap();
        let wal = inventory
            .slices()
            .iter()
            .find(|s| {
                s.binding == binding.id && s.class == FullCapacityAllocationClass::SourceWalShm
            })
            .unwrap();
        DatabaseAttemptStart::from_context(
            record,
            inventory,
            Uuid::new_v4(),
            Uuid::new_v4(),
            db.id,
            wal.id,
            1024,
            2048,
            0,
        )
        .unwrap()
    }
    fn outcome(start: &DatabaseAttemptStart, kind: AttemptOutcomeKind) -> DatabaseAttemptOutcome {
        DatabaseAttemptOutcome {
            version: 1,
            observation: Uuid::new_v4(),
            attempt: start.attempt,
            start_digest: start.digest().unwrap(),
            kind,
            // Synthetic receipt identity is an immutable observation ONLY, never an actual owner ack.
            receipt: if kind == AttemptOutcomeKind::CommittedObservation {
                Some(Uuid::new_v4())
            } else {
                None
            },
            before: if kind == AttemptOutcomeKind::Unknown {
                None
            } else {
                Some(start.before)
            },
            after: match kind {
                AttemptOutcomeKind::CommittedObservation => Some(start.after),
                AttemptOutcomeKind::NoCommitObservation => Some(start.before),
                AttemptOutcomeKind::Unknown => None,
            },
            allocated_database: None,
            allocated_wal: None,
            allocated_shm: None,
            budget_observation: None,
        }
    }
    fn retained_image(store: &OperationJournalStore) -> (Vec<String>, [u8; 32]) {
        let original = integration::durable_image(store);
        let mut h = blake3::Hasher::new();
        for table in [
            database_attempts::START_TABLE,
            database_attempts::OUTCOME_TABLE,
        ] {
            let mut statement = store
                .connection
                .prepare(&format!(
                    "SELECT attempt_id,image FROM {table} ORDER BY attempt_id"
                ))
                .unwrap();
            let mut rows = statement.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                h.update(row.get::<_, String>(0).unwrap().as_bytes());
                h.update(&row.get::<_, Vec<u8>>(1).unwrap());
            }
        }
        (original, *h.finalize().as_bytes())
    }
    fn injected() -> OperationJournalStoreError {
        OperationJournalStoreError::InconsistentRecord {
            reason: "logical attempt transaction fault".into(),
        }
    }
    fn persist_fixture(
        store: &OperationJournalStore,
        owner: &WritableProfileGuard,
        count: usize,
    ) -> (
        StoredOperationRecord,
        AdmissionClaimInventory,
        DatabaseClaimInventory,
    ) {
        let (record, claims, inventory) =
            integration::actual_journal_input_count(store, owner, count);
        let provider = logical_provider(&record);
        assert!(
            settle_fixture(store, &record, &claims, &inventory, &provider)
                .outcome()
                .receipt()
                .is_some()
        );
        (record, claims, inventory)
    }
    fn insert_raw_start(store: &OperationJournalStore, start: &DatabaseAttemptStart) {
        store.connection.execute("INSERT INTO operation_journal_database_attempt(attempt_id,operation_id,binding_id,start_nonce,database_slice,sidecar_slice,database_bytes,wal_bytes,shm_bytes,image) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",params![start.attempt.to_string(),start.operation.to_string(),start.binding.to_string(),start.nonce.to_string(),start.database_slice.to_string(),start.sidecar_slice.to_string(),start.database_bytes,start.wal_bytes,start.shm_bytes,start.encode().unwrap()]).unwrap();
    }

    #[test]
    fn durable_attempt_ledger_child() {
        if std::env::var("YIELD_ATTEMPT_LEDGER_CHILD").as_deref() != Ok("1") {
            return;
        }
        atomic_replay_nonce_overlap_and_outcomes();
        mismatched_images_never_create_rows();
        direction_revision_envelopes_are_closed();
        rollback_and_uncertain_boundaries();
        corruption_and_orphans_retain_evidence();
        previous_schema_migrates_without_fabricated_attempts();
        row_caps_retain_all_unsupported_history();
        actual_paged_restart_audit();
    }
    fn atomic_replay_nonce_overlap_and_outcomes() {
        for kind in [
            AttemptOutcomeKind::Unknown,
            AttemptOutcomeKind::NoCommitObservation,
            AttemptOutcomeKind::CommittedObservation,
        ] {
            let f = fixture();
            let store = OperationJournalStore::open(&f.owner).unwrap();
            let (record, claims, inventory) = persist_fixture(&store, &f.owner, 2);
            let a = start(&record, &inventory, 0);
            assert!(!store.persist_database_attempt_start(&a).unwrap().replay);
            let first = retained_image(&store);
            assert!(store.persist_database_attempt_start(&a).unwrap().replay);
            assert_eq!(
                store.read_database_attempt(a.attempt).unwrap().unwrap(),
                (a.clone(), None)
            );
            assert_eq!(
                store
                    .read_database_attempt_for_nonce(a.nonce, a.digest().unwrap())
                    .unwrap()
                    .unwrap()
                    .0,
                a
            );
            assert!(
                store
                    .read_database_attempt_for_nonce(a.nonce, [0; 32])
                    .is_err()
            );
            assert_eq!(retained_image(&store), first);
            let mut duplicate = start(&record, &inventory, 1);
            duplicate.nonce = a.nonce;
            assert!(store.persist_database_attempt_start(&duplicate).is_err());
            assert_eq!(retained_image(&store), first);
            let overlapping = start(&record, &inventory, 0);
            assert!(store.persist_database_attempt_start(&overlapping).is_err());
            assert_eq!(retained_image(&store), first);
            let mut observation = outcome(&a, kind);
            if kind == AttemptOutcomeKind::NoCommitObservation {
                observation.allocated_database = Some(0);
                observation.allocated_wal = Some(17);
            }
            assert!(
                !store
                    .persist_database_attempt_outcome(&observation)
                    .unwrap()
                    .replay
            );
            assert!(
                store
                    .persist_database_attempt_outcome(&observation)
                    .unwrap()
                    .replay
            );
            let read = store.read_database_attempt(a.attempt).unwrap().unwrap();
            assert_eq!(read, (a.clone(), Some(observation.clone())));
            assert_eq!(read.0.database_bytes, 1024);
            assert_eq!(read.0.wal_bytes, 2048);
            assert_eq!(read.0.shm_bytes, 0);
            assert_eq!(read.1.unwrap().allocated_shm, None);
            let before = retained_image(&store);
            let mut changed = observation.clone();
            changed.observation = Uuid::new_v4();
            assert!(store.persist_database_attempt_outcome(&changed).is_err());
            assert!(store.persist_database_attempt_start(&overlapping).is_err());
            for claim in claims.claims() {
                let request = OrdinaryClaimReleaseRequest::new(
                    record.operation_id,
                    claim.claim_id(),
                    Uuid::new_v4(),
                )
                .unwrap();
                assert!(
                    store
                        .release_ordinary_claim(request, &logical_provider(&record))
                        .is_err()
                );
            }
            assert!(store.prove_clean_handoff_durable().is_err());
            assert_eq!(retained_image(&store), before);
            drop(store);
            let reopened = OperationJournalStore::open(&f.owner).unwrap();
            assert_eq!(
                reopened.read_database_attempt(a.attempt).unwrap().unwrap(),
                (a.clone(), Some(observation))
            );
            assert!(
                reopened
                    .persist_database_attempt_start(&overlapping)
                    .is_err()
            );
            assert_eq!(retained_image(&reopened), before);
            let snapshot = reopened.rebuild_ordinary_claim_snapshot().unwrap();
            assert!(!snapshot.proves_durable_zero());
        }
    }
    fn mismatched_images_never_create_rows() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
        let original = start(&record, &inventory, 0);
        let before = retained_image(&store);
        for case in 0..14 {
            let mut bad = original.clone();
            match case {
                0 => bad.command = serde_json::json!({"unsupported":"not the original command"}),
                1 => bad.command_digest = [0; 32],
                2 => bad.operation = Uuid::new_v4(),
                3 => bad.binding = Uuid::new_v4(),
                4 => bad.participant = Uuid::new_v4(),
                5 => bad.database_slice = bad.sidecar_slice,
                6 => bad.revision_domain = RevisionDomain::GlobalMutationGeneration,
                7 => bad.phase = OperationPhase::Terminal,
                8 => bad.before += 1,
                9 => bad.after += 1,
                10 => bad.database_bytes = u64::MAX,
                11 => bad.wal_bytes = u64::MAX,
                12 => bad.inventory[0] ^= 1,
                13 => bad.nonce = Uuid::nil(),
                _ => unreachable!(),
            }
            assert!(
                store.persist_database_attempt_start(&bad).is_err(),
                "bad start case {case}"
            );
            assert_eq!(rows(&store, database_attempts::START_TABLE), 0);
            assert_eq!(retained_image(&store), before);
        }
        assert!(
            DatabaseAttemptStart::decode(
                &[b' '; crate::operation_journal::database_attempts::MAX_START_BYTES + 1]
            )
            .is_err()
        );
        let mut spaced = original.encode().unwrap();
        spaced.push(b' ');
        assert!(DatabaseAttemptStart::decode(&spaced).is_err());
        let mut unknown: serde_json::Value =
            serde_json::from_slice(&original.encode().unwrap()).unwrap();
        unknown["future_field"] = serde_json::json!(1);
        assert!(DatabaseAttemptStart::decode(&serde_json::to_vec(&unknown).unwrap()).is_err());
    }
    fn direction_revision_envelopes_are_closed() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, _, inventory) = integration::actual_journal_input(&store, &f.owner);
        for direction in [
            ClaimDirection::Routine,
            ClaimDirection::RequiredCompensation,
            ClaimDirection::LeaseMaintenance,
        ] {
            let mut slices = inventory.slices().to_vec();
            for slice in &mut slices {
                slice.direction = direction;
            }
            let inventory = DatabaseClaimInventory::new(
                inventory.header().clone(),
                inventory.bindings().to_vec(),
                slices,
            )
            .unwrap();
            let selected = if direction == ClaimDirection::RequiredCompensation {
                record.with_cancellation_request(
                    crate::operation_journal::CancellationRequestObservation::new(1).unwrap(),
                )
            } else {
                record.clone()
            };
            let a = start(&selected, &inventory, 0);
            let p = &inventory.bindings()[0].participants[0];
            match direction {
                ClaimDirection::RequiredCompensation => {
                    assert_eq!(a.before, p.after);
                    assert_eq!(a.after, p.after + 1);
                }
                ClaimDirection::LeaseMaintenance => {
                    assert_eq!((a.before, a.after), (p.before, p.before))
                }
                _ => assert_eq!((a.before, a.after), (p.before, p.after)),
            }
            let wrong = if direction == ClaimDirection::RequiredCompensation {
                record.clone()
            } else {
                record.with_cancellation_request(
                    crate::operation_journal::CancellationRequestObservation::new(1).unwrap(),
                )
            };
            assert!(a.validate_bound(&wrong, &inventory, true).is_err());
        }
        let mut reconciled = record.clone();
        reconciled.phase = OperationPhase::SourceReconciled;
        let db = inventory
            .slices()
            .iter()
            .find(|s| s.class == FullCapacityAllocationClass::SourceDatabaseCommit)
            .unwrap();
        let side = inventory
            .slices()
            .iter()
            .find(|s| s.class == FullCapacityAllocationClass::SourceWalShm)
            .unwrap();
        assert!(
            DatabaseAttemptStart::from_context(
                &reconciled,
                &inventory,
                Uuid::new_v4(),
                Uuid::new_v4(),
                db.id,
                side.id,
                1024,
                2048,
                0
            )
            .is_err()
        );
    }
    fn rollback_and_uncertain_boundaries() {
        for fault in 0..4 {
            let f = fixture();
            let store = OperationJournalStore::open(&f.owner).unwrap();
            let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
            let a = start(&record, &inventory, 0);
            let before = retained_image(&store);
            store.set_insert_test_hook(move |boundary| match (fault, boundary) {
                (0, InsertTestBoundary::BeforePrecommitValidation) => {
                    InsertTestDirective::Fail(injected())
                }
                (1, InsertTestBoundary::BeforePrecommitValidation) => {
                    InsertTestDirective::FailAndLeaveForRollback(injected())
                }
                (1, InsertTestBoundary::Rollback) => InsertTestDirective::Fail(injected()),
                (2, InsertTestBoundary::Commit) => InsertTestDirective::ExecuteThenFail(injected()),
                (3, InsertTestBoundary::AfterCommit) => InsertTestDirective::Fail(injected()),
                _ => InsertTestDirective::Continue,
            });
            let failed = store.persist_database_attempt_start(&a).unwrap_err();
            assert!(failed.source.to_string().contains("logical attempt"));
            match fault {
                0 => {
                    assert_eq!(
                        failed.stage,
                        JournalInsertWriteStage::RolledBackBeforeCommit
                    );
                    assert!(failed.retained_start.is_none());
                    assert!(failed.rollback_error.is_none());
                    assert_eq!(retained_image(&store), before);
                }
                1 => {
                    assert_eq!(failed.stage, JournalInsertWriteStage::RollbackNotConfirmed);
                    assert_eq!(failed.retained_start, Some(a.clone()));
                    assert!(failed.rollback_error.is_some());
                    assert_eq!(rows(&store, database_attempts::START_TABLE), 1);
                    assert!(store.read_database_attempt(a.attempt).is_err());
                    assert!(store.persist_database_attempt_start(&a).is_err());
                    store.connection.execute_batch("ROLLBACK").unwrap();
                    assert_eq!(retained_image(&store), before);
                }
                _ => {
                    assert!(matches!(
                        failed.stage,
                        JournalInsertWriteStage::CommitOutcomeUnknown
                            | JournalInsertWriteStage::CommittedButNotAcknowledged
                    ));
                    assert_eq!(failed.retained_start, Some(a.clone()));
                    assert_eq!(rows(&store, database_attempts::START_TABLE), 1);
                }
            }
            // Quiesce the original SQLite connection before releasing an uncertain scope.
            drop(failed);
            drop(store);
            let reopened = OperationJournalStore::open(&f.owner).unwrap();
            if fault < 2 {
                assert!(reopened.read_database_attempt(a.attempt).unwrap().is_none());
            } else {
                assert_eq!(
                    reopened
                        .read_database_attempt_for_nonce(a.nonce, a.digest().unwrap())
                        .unwrap()
                        .unwrap()
                        .0,
                    a
                );
                let observation = outcome(&a, AttemptOutcomeKind::Unknown);
                reopened.set_insert_test_hook(|boundary| {
                    if boundary == InsertTestBoundary::AfterCommit {
                        InsertTestDirective::Fail(injected())
                    } else {
                        InsertTestDirective::Continue
                    }
                });
                assert_eq!(
                    reopened
                        .persist_database_attempt_outcome(&observation)
                        .unwrap_err()
                        .stage,
                    JournalInsertWriteStage::CommittedButNotAcknowledged
                );
                assert_eq!(
                    reopened
                        .read_database_attempt(a.attempt)
                        .unwrap()
                        .unwrap()
                        .1,
                    Some(observation.clone())
                );
                assert!(
                    reopened
                        .persist_database_attempt_outcome(&observation)
                        .unwrap()
                        .replay
                );
            }
        }
    }
    fn corruption_and_orphans_retain_evidence() {
        for case in 0..6 {
            let f = fixture();
            let store = OperationJournalStore::open(&f.owner).unwrap();
            let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
            let a = start(&record, &inventory, 0);
            store.persist_database_attempt_start(&a).unwrap();
            match case {
                0 => {
                    let mut bad = a.clone();
                    bad.command_digest = [0; 32];
                    store
                        .connection
                        .execute(
                            "UPDATE operation_journal_database_attempt SET image=?1",
                            [bad.encode().unwrap()],
                        )
                        .unwrap();
                }
                1 => {
                    let mut bad = a.clone();
                    bad.version = 2;
                    store
                        .connection
                        .execute(
                            "UPDATE operation_journal_database_attempt SET image=?1",
                            [serde_json::to_vec(&bad).unwrap()],
                        )
                        .unwrap();
                }
                2 => {
                    store.connection.execute("UPDATE operation_journal_database_attempt SET database_bytes=database_bytes+1",[]).unwrap();
                }
                3 => {
                    store
                        .connection
                        .execute_batch("PRAGMA foreign_keys=OFF")
                        .unwrap();
                    store
                        .connection
                        .execute(
                            "UPDATE operation_journal_database_attempt SET operation_id=?1",
                            [Uuid::new_v4().to_string()],
                        )
                        .unwrap();
                    store
                        .connection
                        .execute_batch("PRAGMA foreign_keys=ON")
                        .unwrap();
                }
                4 => {
                    store.connection.execute("UPDATE operation_journal_ordinary_claim SET allocated_bytes=allocated_bytes+1 WHERE allocation_class='JournalRecord'",[]).unwrap();
                }
                5 => {
                    let observation = outcome(&a, AttemptOutcomeKind::Unknown);
                    store
                        .persist_database_attempt_outcome(&observation)
                        .unwrap();
                    store.connection.execute("UPDATE operation_journal_database_attempt_outcome SET observation_id=?1",[Uuid::new_v4().to_string()]).unwrap();
                }
                _ => unreachable!(),
            }
            let before = retained_image(&store);
            assert!(store.read_database_attempt(a.attempt).is_err());
            assert!(store.rebuild_ordinary_claim_snapshot().is_err());
            assert_eq!(retained_image(&store), before);
            drop(store);
            let reopened = OperationJournalStore::open(&f.owner).unwrap();
            assert!(reopened.read_database_attempt(a.attempt).is_err());
            assert_eq!(retained_image(&reopened), before);
            assert_eq!(reopened.get(record.operation_id).unwrap(), Some(record));
        }
    }
    fn previous_schema_migrates_without_fabricated_attempts() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, _, _) = persist_fixture(&store, &f.owner, 1);
        let before = retained_image(&store).0;
        store.connection.execute_batch("DROP TABLE operation_journal_publication_issuance; DROP TABLE operation_journal_database_attempt_outcome; DROP TABLE operation_journal_database_attempt; PRAGMA user_version=11").unwrap();
        drop(store);
        let reopened = OperationJournalStore::open(&f.owner).unwrap();
        assert_eq!(retained_image(&reopened).0, before);
        assert_eq!(reopened.get(record.operation_id).unwrap(), Some(record));
        assert_eq!(rows(&reopened, database_attempts::START_TABLE), 0);
        assert_eq!(rows(&reopened, database_attempts::OUTCOME_TABLE), 0);
        let version: i64 = reopened
            .connection
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, JOURNAL_SCHEMA_VERSION as i64);
    }
    fn row_caps_retain_all_unsupported_history() {
        for (cap, global) in [(MAX_ATTEMPTS_PER_OPERATION, false), (MAX_ATTEMPTS, true)] {
            let f = fixture();
            let store = OperationJournalStore::open(&f.owner).unwrap();
            let (record, _, inventory) = persist_fixture(&store, &f.owner, 1);
            let prototype = start(&record, &inventory, 0);
            // Deliberately unsupported overlap history exercises caps without inventing 8192 live
            // registrations or settling observations. It remains rejected and is never evicted.
            store.connection.execute_batch("BEGIN").unwrap();
            for _ in 0..cap {
                let mut row = prototype.clone();
                row.attempt = Uuid::new_v4();
                row.nonce = Uuid::new_v4();
                insert_raw_start(&store, &row);
            }
            store.connection.execute_batch("COMMIT").unwrap();
            let before = retained_image(&store);
            let failed = store
                .persist_database_attempt_start(&prototype)
                .unwrap_err();
            assert_eq!(failed.stage, JournalInsertWriteStage::NoMutationAttempted);
            assert!(failed.source.to_string().contains("exhausted"));
            assert_eq!(rows(&store, database_attempts::START_TABLE), cap as i64);
            assert_eq!(retained_image(&store), before);
            if global {
                assert!(store.read_database_attempt(prototype.attempt).is_err());
            }
        }
    }
    fn actual_paged_restart_audit() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let mut last = None;
        for n in 0..65 {
            let (mut record, claims, inventory) =
                integration::actual_journal_input(&store, &f.owner);
            let mut bindings = inventory.bindings().to_vec();
            bindings[0].main_file = format!("logical-owned-db-{n}");
            // Distinct frozen source-file observations; no actual SourceWriter registration exists.
            let set = record.universal_intent.as_ref().unwrap().participants();
            let descriptors = set
                .descriptors()
                .iter()
                .map(|d| {
                    let fence = d.source_fence().map(|s| {
                        StoredSourceFence::new(
                            s.source_id().clone(),
                            s.root_identity(),
                            bindings[0].main_file.clone(),
                            s.lifecycle_generation(),
                            s.source_revision(),
                            s.lease_epoch(),
                        )
                        .unwrap()
                    });
                    ParticipantDescriptor::new(d.id(), d.kind(), d.requirement(), fence, None)
                        .unwrap()
                })
                .collect();
            let participants =
                ParticipantSet::new(descriptors, set.checkpoints().to_vec()).unwrap();
            record.universal_intent = Some(
                UniversalIntent::new(
                    None,
                    UniversalCommandKind::Copy,
                    "operation.status.copy",
                    participants,
                )
                .unwrap(),
            );
            let inventory = DatabaseClaimInventory::new(
                inventory.header().clone(),
                bindings,
                inventory.slices().to_vec(),
            )
            .unwrap();
            let provider = logical_provider(&record);
            assert!(
                settle_fixture(&store, &record, &claims, &inventory, &provider)
                    .outcome()
                    .receipt()
                    .is_some()
            );
            let a = start(&record, &inventory, 0);
            store.persist_database_attempt_start(&a).unwrap();
            last = Some(a);
        }
        let last = last.unwrap();
        assert_eq!(rows(&store, database_attempts::START_TABLE), 65);
        assert_eq!(
            store
                .read_database_attempt(last.attempt)
                .unwrap()
                .unwrap()
                .0,
            last
        );
        let before = retained_image(&store);
        drop(store);
        let reopened = OperationJournalStore::open(&f.owner).unwrap();
        assert_eq!(
            reopened
                .read_database_attempt(last.attempt)
                .unwrap()
                .unwrap()
                .0,
            last
        );
        assert_eq!(retained_image(&reopened), before);
        assert!(
            !reopened
                .rebuild_ordinary_claim_snapshot()
                .unwrap()
                .proves_durable_zero()
        );
    }
}
