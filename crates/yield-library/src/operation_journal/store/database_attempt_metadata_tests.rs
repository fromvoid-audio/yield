// Actual SQLite and profile ownership; synthetic provider values establish logical retention only.
#[cfg(test)]
mod integration {
    use super::*;

    #[test]
    fn attempt_metadata_checks_run_in_an_extension_free_process() {
        // Extension loading elsewhere in the suite permanently taints the process. Exercise the
        // actual production exclusion seam in a fresh process, rather than disabling that check.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "operation_journal::store::tests::exact_database_claim_persistence::integration::attempt_metadata_child", "--nocapture"])
            .env("YIELD_ATTEMPT_METADATA_CHILD", "1")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
    }

    pub(super) fn actual_journal_input(
        store: &OperationJournalStore,
        owner: &WritableProfileGuard,
    ) -> (
        StoredOperationRecord,
        AdmissionClaimInventory,
        DatabaseClaimInventory,
    ) {
        actual_journal_input_count(store, owner, 1)
    }
    pub(super) fn actual_journal_input_count(
        store: &OperationJournalStore,
        owner: &WritableProfileGuard,
        count: usize,
    ) -> (
        StoredOperationRecord,
        AdmissionClaimInventory,
        DatabaseClaimInventory,
    ) {
        let (mut record, original, inventory) = input(owner, count);
        let opened = crate::sample_sources::db::sqlite_identity::open_existing_database_file(
            &store.database_root,
            std::path::Path::new(JOURNAL_DATABASE_FILE_NAME),
            &store.database_path,
        )
        .unwrap();
        let volume = crate::durable_capacity::descriptor_capacity_facts(&opened.file)
            .unwrap()
            .identity;
        let allocations = record
            .full_capacity_plan
            .as_ref()
            .unwrap()
            .allocations()
            .iter()
            .map(|a| {
                crate::operation_journal::full_capacity::FullCapacityAllocation::new(
                    if a.class() == FullCapacityAllocationClass::JournalRecord {
                        volume.clone()
                    } else {
                        a.volume().clone()
                    },
                    a.allocation_unit(),
                    a.class(),
                    a.logical_bytes(),
                    a.allocated_bytes(),
                    a.protected_free_bytes(),
                )
                .unwrap()
            })
            .collect();
        record.full_capacity_plan = Some(FullCapacityPlan::new(allocations).unwrap());
        let claims = AdmissionClaimInventory::new(
            original
                .claims()
                .iter()
                .map(|c| {
                    AdmissionClaim::new(
                        c.claim_id(),
                        if c.allocation_class() == FullCapacityAllocationClass::JournalRecord {
                            volume.clone()
                        } else {
                            c.volume().clone()
                        },
                        c.allocation_class(),
                        c.allocated_bytes(),
                    )
                    .unwrap()
                })
                .collect(),
        )
        .unwrap();
        (record, claims, inventory)
    }

    pub(super) fn durable_image(store: &OperationJournalStore) -> Vec<String> {
        // Capture every relevant retained row, including release facts. No rejection may discard
        // unsupported evidence or manufacture an attempt/outcome row.
        let mut image = Vec::new();
        for sql in [
            "SELECT operation_id || ':' || hex(record) FROM operation_journal ORDER BY operation_id",
            "SELECT claim_id || ':' || operation_id || ':' || volume_device || ':' || allocation_class || ':' || allocated_bytes FROM operation_journal_ordinary_claim ORDER BY claim_id",
            "SELECT claim_id || ':' || operation_id || ':' || release_id FROM operation_journal_ordinary_claim_release ORDER BY claim_id",
            "SELECT binding_id || ':' || operation_id || ':' || hex(inventory_digest) || ':' || COALESCE(hex(inventory),'NULL') FROM operation_journal_database_binding ORDER BY binding_id",
            "SELECT slice_id || ':' || operation_id || ':' || claim_id || ':' || ceiling || ':' || wal_ceiling || ':' || shm_ceiling FROM operation_journal_database_claim_slice ORDER BY slice_id",
        ] {
            let mut statement = store.connection.prepare(sql).unwrap();
            image.extend(
                statement
                    .query_map([], |r| r.get::<_, String>(0))
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
            );
        }
        image
    }

    #[test]
    fn attempt_metadata_child() {
        if std::env::var("YIELD_ATTEMPT_METADATA_CHILD").as_deref() != Ok("1") {
            return;
        }
        for case in 0..11 {
            let f = fixture();
            let store = OperationJournalStore::open(&f.owner).unwrap();
            let (mut record, mut claims, inventory) = actual_journal_input(&store, &f.owner);
            if case == 10 {
                // Matching proposal and durable bytes still cannot cover the full rollback peak.
                record.full_capacity_plan = Some(FullCapacityPlan::new(record.full_capacity_plan.as_ref().unwrap().allocations().iter().map(|a| {
                    crate::operation_journal::full_capacity::FullCapacityAllocation::new(a.volume().clone(), a.allocation_unit(), a.class(), if a.class()==FullCapacityAllocationClass::JournalRecord {1} else {a.logical_bytes()}, if a.class()==FullCapacityAllocationClass::JournalRecord {1} else {a.allocated_bytes()}, a.protected_free_bytes()).unwrap()
                }).collect()).unwrap());
                claims = AdmissionClaimInventory::new(
                    claims
                        .claims()
                        .iter()
                        .map(|c| {
                            AdmissionClaim::new(
                                c.claim_id(),
                                c.volume().clone(),
                                c.allocation_class(),
                                if c.allocation_class()
                                    == FullCapacityAllocationClass::JournalRecord
                                {
                                    1
                                } else {
                                    c.allocated_bytes()
                                },
                            )
                            .unwrap()
                        })
                        .collect(),
                )
                .unwrap();
            }
            let provider = logical_provider(&record);
            let settled = settle_fixture(&store, &record, &claims, &inventory, &provider);
            assert!(settled.outcome().receipt().is_some());
            let journal = claims
                .claims()
                .iter()
                .find(|c| c.allocation_class() == FullCapacityAllocationClass::JournalRecord)
                .unwrap();
            match case {
                1 => {
                    store.connection.execute("UPDATE operation_journal_ordinary_claim SET allocated_bytes=allocated_bytes-1 WHERE claim_id=?1", [journal.claim_id().to_string()]).unwrap();
                }
                2 => {
                    store
                        .connection
                        .execute(
                            "DELETE FROM operation_journal_ordinary_claim WHERE claim_id=?1",
                            [journal.claim_id().to_string()],
                        )
                        .unwrap();
                }
                3 => {
                    store.connection.execute("INSERT INTO operation_journal_ordinary_claim_release(claim_id,operation_id,release_id) VALUES(?1,?2,?3)", params![journal.claim_id().to_string(),record.operation_id.to_string(),Uuid::new_v4().to_string()]).unwrap();
                }
                4 => {
                    store.connection.execute("UPDATE operation_journal_ordinary_claim SET volume_device=?1 WHERE claim_id=?2", params![journal.volume().device.wrapping_add(1).to_string(),journal.claim_id().to_string()]).unwrap();
                }
                5 => {
                    store
                        .connection
                        .execute_batch("CREATE TEMP TABLE unrelated(x)")
                        .unwrap();
                }
                6 => {
                    store
                        .connection
                        .execute_batch("CREATE TABLE unexpected_attempt_policy(x)")
                        .unwrap();
                }
                7 => {
                    store.connection.execute_batch("BEGIN").unwrap();
                }
                8 => {
                    store.connection.execute("UPDATE operation_journal_database_binding SET inventory_digest=zeroblob(32)", []).unwrap();
                }
                9 => {
                    store.connection.execute("INSERT INTO operation_journal_ordinary_claim(claim_id,operation_id,volume_device,allocation_class,allocated_bytes) VALUES(?1,?2,?3,'JournalRecord',1)", params![Uuid::new_v4().to_string(),record.operation_id.to_string(),journal.volume().device.wrapping_add(1).to_string()]).unwrap();
                }
                _ => {}
            }
            let before = durable_image(&store);
            let checked = store.prepare_database_attempt_metadata(record.operation_id);
            if case == 0 {
                let scope = checked.unwrap();
                assert!(crate::sqlite_ext::try_extension_free_metadata_scope().is_some());
                let spill: i64 = store
                    .connection
                    .query_row("PRAGMA cache_spill", [], |r| r.get(0))
                    .unwrap();
                let temp: i64 = store
                    .connection
                    .query_row("PRAGMA temp_store", [], |r| r.get(0))
                    .unwrap();
                let pages: i64 = store
                    .connection
                    .query_row("PRAGMA max_page_count", [], |r| r.get(0))
                    .unwrap();
                let size: i64 = store
                    .connection
                    .query_row("PRAGMA page_size", [], |r| r.get(0))
                    .unwrap();
                assert_eq!((spill, temp), (0, 2));
                assert_eq!(pages as u64 * size as u64, crate::operation_journal::recording_commands::MAX_RECORDING_JOURNAL_DATABASE_BYTES);
                drop(scope);
                assert_eq!(durable_image(&store), before);
                drop(store);
                let reopened = OperationJournalStore::open(&f.owner).unwrap();
                assert!(
                    reopened
                        .prepare_database_attempt_metadata(record.operation_id)
                        .is_ok()
                );
                assert_eq!(durable_image(&reopened), before);
            } else {
                assert!(checked.is_err(), "case {case} accepted");
                assert_eq!(
                    durable_image(&store),
                    before,
                    "case {case} modified retained evidence"
                );
            }
        }
        // Actual initial binding fixtures intentionally use a synthetic journal volume. A valid
        // durable binding image cannot turn that proposal into actual-descriptor coverage.
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let (record, claims, inventory) = input(&f.owner, 1);
        let provider = logical_provider(&record);
        assert!(
            settle_fixture(&store, &record, &claims, &inventory, &provider)
                .outcome()
                .receipt()
                .is_some()
        );
        let before = durable_image(&store);
        assert!(
            store
                .prepare_database_attempt_metadata(record.operation_id)
                .is_err()
        );
        assert_eq!(durable_image(&store), before);
        assert!(
            store
                .prepare_database_attempt_metadata(OperationId::new())
                .is_err()
        );
    }
}
