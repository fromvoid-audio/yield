use super::*;
use std::sync::Arc;
use yield_library::sample_sources::SampleCollection;

fn setup(
    name: &str,
    count: usize,
) -> (
    Fixture,
    CatalogService,
    SourceId,
    CatalogSnapshot,
    Vec<RootRelativePath>,
) {
    let fixture = Fixture::new(name);
    let paths: Vec<_> = (0..count)
        .map(|i| {
            let path = PathBuf::from(format!("{i:04}.wav"));
            write_test_wav(&fixture.root.path().join(&path), 16000, 4);
            RootRelativePath::try_from_path(path).unwrap()
        })
        .collect();
    let id = SourceId::from_string(name);
    let service = CatalogService::start(&fixture.guard).unwrap();
    let snapshot = register_and_audit(&service, fixture.root.path(), &id);
    (fixture, service, id, snapshot, paths)
}
fn capture(
    service: &CatalogService,
    id: &SourceId,
    snapshot: &CatalogSnapshot,
    paths: &[RootRelativePath],
) -> Arc<CatalogCollectionHistoryGroup> {
    let request = CollectionHistoryGroupRequest::new(
        id.clone(),
        1,
        snapshot.committed_revision,
        100,
        SampleCollection::new(0).unwrap(),
        true,
        paths,
    )
    .unwrap();
    service
        .submit(CatalogCommand::RequestCollectionHistoryGroup {
            request,
            cancellation: CollectionHistoryCancellation::new(),
        })
        .unwrap();
    match receive_until(service, |e| {
        matches!(e, CatalogEvent::CollectionHistoryGroupCompleted { .. })
    }) {
        CatalogEvent::CollectionHistoryGroupCompleted { result, .. } => result.unwrap(),
        _ => unreachable!(),
    }
}
fn prefix(index: usize) {
    let (fixture, service, id, snapshot, paths) =
        setup(&format!("catalog-prepared-prefix-{index}"), 257);
    let group = capture(&service, &id, &snapshot, &paths);
    assert_eq!(group.source_observation().changes().len(), 257);
    assert_eq!(
        group
            .source_observation()
            .batches()
            .map(|b| b.len())
            .collect::<Vec<_>>(),
        [128, 128, 1]
    );
    service
        .arm_compound_batch_failure(&id, 1, 200, index)
        .unwrap();
    service
        .submit(CatalogCommand::ApplyPreparedCollectionOperation {
            operation: PreparedCollectionOperation::new(Arc::clone(&group), 200).unwrap(),
        })
        .unwrap();
    let outcome = receive_curation_operation(&service, 200);
    assert_eq!(outcome.failure().unwrap().batch_index(), index);
    assert_eq!(outcome.committed_batches().len(), index);
    assert_eq!(
        outcome.initial_revision(),
        Some(snapshot.committed_revision)
    );
    assert_eq!(
        outcome.final_revision(),
        Some(snapshot.committed_revision + index as u64)
    );
    assert!(Arc::ptr_eq(
        outcome.prepared_collection_group().unwrap(),
        &group
    ));
    for (ordinal, receipt) in outcome.committed_batches().iter().enumerate() {
        assert_eq!(
            receipt.source_revision(),
            snapshot.committed_revision + ordinal as u64 + 1
        );
        assert_eq!(receipt.root_identity(), &snapshot.root_identity);
        assert_eq!(
            receipt.directory_generation(),
            group.source_observation().directory_generation()
        );
        let expected = group.source_observation().batches().nth(ordinal).unwrap();
        assert_eq!(receipt.files().len(), expected.len());
        for (file, change) in receipt.files().iter().zip(expected) {
            assert_eq!(file.relative_path, change.path().as_path());
            assert_eq!(
                file.collections
                    .contains(&group.source_observation().collection()),
                change.after()
            );
        }
    }
    let db = SourceDatabase::open_for_ui_read_with_database_root(
        fixture.root.path(),
        fixture
            .guard
            .profile_root()
            .join(catalog_profile_child(&id)),
    )
    .unwrap();
    assert_eq!(
        db.get_revision().unwrap(),
        outcome.final_revision().unwrap()
    );
    let connection = rusqlite::Connection::open_with_flags(
        fixture
            .guard
            .profile_root()
            .join(catalog_profile_child(&id))
            .join(".yield.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let actual: usize = connection
        .query_row(
            "SELECT count(*) FROM wav_file_collections WHERE collection=0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(actual, index * 128);
    assert_eq!(
        admission_state(&service.admission, |s| s.curation_payload_bytes),
        0
    );
    assert_eq!(
        admission_state(&service.admission, |s| s.curation_update_count),
        0
    );
    service.shutdown();
}
#[test]
fn prepared_collection_first_batch_failure_has_no_effect() {
    prefix(0);
}
#[test]
fn prepared_collection_middle_failure_retains_only_real_prefix() {
    prefix(1);
}
#[test]
fn prepared_collection_last_failure_retains_only_real_prefix() {
    prefix(2);
}
#[test]
fn prepared_collection_complete_group_preserves_exact_custody_and_revision() {
    let (_fixture, service, id, snapshot, paths) = setup("catalog-prepared-complete", 257);
    let group = capture(&service, &id, &snapshot, &paths);
    service
        .submit(CatalogCommand::ApplyPreparedCollectionOperation {
            operation: PreparedCollectionOperation::new(Arc::clone(&group), 200).unwrap(),
        })
        .unwrap();
    let outcome = receive_curation_operation(&service, 200);
    assert!(outcome.is_success());
    assert_eq!(outcome.committed_batches().len(), 3);
    assert_eq!(
        outcome.final_revision(),
        Some(snapshot.committed_revision + 3)
    );
    assert!(Arc::ptr_eq(
        outcome.prepared_collection_group().unwrap(),
        &group
    ));
    assert!(PreparedCollectionOperation::new(Arc::clone(&group), 201).is_err());
    service.shutdown();
}
#[test]
fn prepared_collection_intervening_revision_retires_capture_before_sql() {
    let (_fixture, service, id, snapshot, paths) = setup("catalog-prepared-intervening", 3);
    let group = capture(&service, &id, &snapshot, &paths);
    let prepared = PreparedCollectionOperation::new(Arc::clone(&group), 201).unwrap();
    service
        .submit(curation_operation(
            &id,
            1,
            &snapshot.root_identity,
            200,
            SourceCurationOperationFence::ExactRevision(snapshot.committed_revision),
            vec![vec![SourceCurationUpdate::SetLocked {
                path: paths[0].clone(),
                locked: true,
            }]],
        ))
        .unwrap();
    let prior = receive_curation_operation(&service, 200);
    assert!(prior.is_success());
    service
        .submit(CatalogCommand::ApplyPreparedCollectionOperation {
            operation: prepared,
        })
        .unwrap();
    let refused = receive_curation_operation(&service, 201);
    assert!(!refused.is_success());
    assert!(refused.committed_batches().is_empty());
    service.shutdown();
}
#[test]
fn prepared_collection_noops_and_bounds_cannot_fabricate_commands() {
    let (_fixture, service, id, snapshot, paths) = setup("catalog-prepared-noop", 3);
    let request = CollectionHistoryGroupRequest::new(
        id.clone(),
        1,
        snapshot.committed_revision,
        100,
        SampleCollection::new(0).unwrap(),
        false,
        &paths,
    )
    .unwrap();
    service
        .submit(CatalogCommand::RequestCollectionHistoryGroup {
            request,
            cancellation: CollectionHistoryCancellation::new(),
        })
        .unwrap();
    let group = match receive_until(&service, |e| {
        matches!(e, CatalogEvent::CollectionHistoryGroupCompleted { .. })
    }) {
        CatalogEvent::CollectionHistoryGroupCompleted { result, .. } => result.unwrap(),
        _ => unreachable!(),
    };
    assert!(group.source_observation().changes().is_empty());
    assert!(PreparedCollectionOperation::new(group, 200).is_err());
    assert!(
        CollectionHistoryGroupRequest::new(
            id.clone(),
            1,
            snapshot.committed_revision,
            101,
            SampleCollection::new(0).unwrap(),
            true,
            &[paths[0].clone(), paths[0].clone()]
        )
        .is_err()
    );
    let many = vec![paths[0].clone(); 65537];
    assert!(
        CollectionHistoryGroupRequest::new(
            id,
            1,
            snapshot.committed_revision,
            102,
            SampleCollection::new(0).unwrap(),
            true,
            &many
        )
        .is_err()
    );
    service.shutdown();
}

fn queued_refusal(case: &str) {
    let fixture = Fixture::new(&format!("catalog-prepared-queued-{case}"));
    write_test_wav(&fixture.root.path().join("one.wav"), 16000, 4);
    let id = SourceId::from_string(format!("prepared-queued-{case}"));
    let (service, gate) = CatalogService::start_with_test_gate(&fixture.guard).unwrap();
    let snapshot = register_and_audit(&service, fixture.root.path(), &id);
    let paths = [RootRelativePath::try_from_path(PathBuf::from("one.wav")).unwrap()];
    let cancellation = CollectionHistoryCancellation::new();
    service
        .submit(CatalogCommand::RequestCollectionHistoryGroup {
            request: CollectionHistoryGroupRequest::new(
                id.clone(),
                1,
                snapshot.committed_revision,
                100,
                SampleCollection::new(0).unwrap(),
                true,
                &paths,
            )
            .unwrap(),
            cancellation: cancellation.clone(),
        })
        .unwrap();
    let group = match receive_until(&service, |e| {
        matches!(e, CatalogEvent::CollectionHistoryGroupCompleted { .. })
    }) {
        CatalogEvent::CollectionHistoryGroupCompleted { result, .. } => result.unwrap(),
        _ => unreachable!(),
    };
    let operation = PreparedCollectionOperation::new(Arc::clone(&group), 200).unwrap();
    let pause = gate.pause_next();
    service
        .submit(CatalogCommand::ApplyPreparedCollectionOperation { operation })
        .unwrap();
    let displaced = fixture.root.path().with_extension("prepared-old");
    let child = fixture
        .guard
        .profile_root()
        .join(catalog_profile_child(&id));
    let moved_child = child.with_extension("prepared-old");
    let main = child.join(".yield.db");
    let moved_main = child.join(".yield-prepared-old.db");
    match case {
        "cancel" => cancellation.cancel(),
        "replace" => {
            service
                .submit(CatalogCommand::Register {
                    source_id: id.clone(),
                    canonical_root: fixture.root.path().to_path_buf(),
                    lifecycle_generation: 2,
                })
                .unwrap();
        }
        "root" => {
            fs::rename(fixture.root.path(), &displaced).unwrap();
            fs::create_dir(fixture.root.path()).unwrap();
        }
        "database-child" => {
            fs::rename(&child, &moved_child).unwrap();
            fs::create_dir(&child).unwrap();
            fs::write(child.join("sentinel"), b"untouched").unwrap();
        }
        "main" => {
            fs::rename(&main, &moved_main).unwrap();
            fs::copy(&moved_main, &main).unwrap();
        }
        "deadline" => {
            let until = Instant::now() + Duration::from_secs(3);
            while group.within_observation_window() && Instant::now() < until {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(!group.within_observation_window());
        }
        _ => panic!("unknown queued fence"),
    }
    pause.release();
    let outcome = receive_curation_operation(&service, 200);
    assert!(!outcome.is_success(), "{case}");
    assert!(outcome.committed_batches().is_empty(), "{case}");
    assert_eq!(outcome.failure().unwrap().batch_index(), 0);
    assert!(Arc::ptr_eq(
        outcome.prepared_collection_group().unwrap(),
        &group
    ));
    assert_eq!(outcome.final_revision(), Some(snapshot.committed_revision));
    if case == "database-child" {
        assert_eq!(fs::read(child.join("sentinel")).unwrap(), b"untouched");
        fs::remove_dir_all(&child).unwrap();
        fs::rename(&moved_child, &child).unwrap();
    }
    if case == "main" {
        fs::remove_file(&main).unwrap();
        fs::rename(&moved_main, &main).unwrap();
    }
    let conn = rusqlite::Connection::open_with_flags(
        fixture
            .guard
            .profile_root()
            .join(catalog_profile_child(&id))
            .join(".yield.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM wav_file_collections", [], |r| r
            .get::<_, usize>(0))
            .unwrap(),
        0
    );
    drop(conn);
    service.shutdown();
    assert_eq!(
        admission_state(&service.admission, |s| s.curation_update_count),
        0
    );
    assert_eq!(
        admission_state(&service.admission, |s| s.curation_payload_bytes),
        0
    );
    if case == "root" {
        fs::remove_dir_all(displaced).unwrap();
    }
}
#[test]
fn prepared_collection_queued_cancellation_refuses_before_sql() {
    queued_refusal("cancel");
}
#[test]
fn prepared_collection_queued_replacement_refuses_before_sql() {
    queued_refusal("replace");
}
#[test]
fn prepared_collection_queued_root_replacement_refuses_before_sql() {
    queued_refusal("root");
}
#[test]
fn prepared_collection_queued_original_window_expires_before_sql() {
    queued_refusal("deadline");
}

#[test]
fn prepared_collection_mixed_targets_preserve_unrelated_fields() {
    let (fixture, service, id, snapshot, paths) = setup("catalog-prepared-mixed", 257);
    let updates = paths
        .iter()
        .enumerate()
        .flat_map(|(i, p)| {
            let mut updates = vec![SourceCurationUpdate::SetLocked {
                path: p.clone(),
                locked: true,
            }];
            if i % 2 == 0 {
                updates.push(SourceCurationUpdate::AddCollection {
                    path: p.clone(),
                    collection: SampleCollection::new(0).unwrap(),
                });
            }
            updates
        })
        .collect::<Vec<_>>();
    service
        .submit(curation_operation(
            &id,
            1,
            &snapshot.root_identity,
            199,
            SourceCurationOperationFence::ExactRevision(snapshot.committed_revision),
            updates.chunks(128).map(|c| c.to_vec()).collect(),
        ))
        .unwrap();
    let prior = receive_curation_operation(&service, 199);
    assert!(prior.is_success());
    let targets = paths
        .iter()
        .enumerate()
        .map(|(i, p)| (p.clone(), i % 2 != 0))
        .collect::<Vec<_>>();
    service
        .submit(CatalogCommand::RequestCollectionHistoryGroup {
            request: CollectionHistoryGroupRequest::new_mixed(
                id.clone(),
                1,
                prior.final_revision().unwrap(),
                100,
                SampleCollection::new(0).unwrap(),
                &targets,
            )
            .unwrap(),
            cancellation: CollectionHistoryCancellation::new(),
        })
        .unwrap();
    let group = match receive_until(&service, |e| {
        matches!(e, CatalogEvent::CollectionHistoryGroupCompleted { .. })
    }) {
        CatalogEvent::CollectionHistoryGroupCompleted { result, .. } => result.unwrap(),
        _ => unreachable!(),
    };
    assert_eq!(group.source_observation().changes().len(), 257);
    service
        .submit(CatalogCommand::ApplyPreparedCollectionOperation {
            operation: PreparedCollectionOperation::new(group, 200).unwrap(),
        })
        .unwrap();
    let outcome = receive_curation_operation(&service, 200);
    assert!(outcome.is_success());
    assert_eq!(outcome.committed_batches().len(), 3);
    let conn = rusqlite::Connection::open_with_flags(
        fixture
            .guard
            .profile_root()
            .join(catalog_profile_child(&id))
            .join(".yield.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let actual = conn
        .prepare("SELECT path FROM wav_file_collections WHERE collection=0 ORDER BY path")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        actual,
        targets
            .iter()
            .filter(|(_, t)| *t)
            .map(|(p, _)| p.as_path().to_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM wav_files WHERE locked=1", [], |r| {
            r.get::<_, usize>(0)
        })
        .unwrap(),
        257
    );
    drop(conn);
    service.shutdown();
}

#[test]
fn prepared_collection_queued_database_child_replacement_refuses_before_sql() {
    queued_refusal("database-child");
}
#[test]
fn prepared_collection_queued_main_identity_replacement_refuses_before_sql() {
    queued_refusal("main");
}

#[test]
fn prepared_collection_capture_queue_consumes_original_deadline_and_releases_credits() {
    let fixture = Fixture::new("catalog-prepared-capture-deadline");
    write_test_wav(&fixture.root.path().join("one.wav"), 16000, 4);
    let id = SourceId::from_string("prepared-capture-deadline");
    let (service, gate) = CatalogService::start_with_test_gate(&fixture.guard).unwrap();
    let snapshot = register_and_audit(&service, fixture.root.path(), &id);
    let pause = gate.pause_next();
    service
        .submit(CatalogCommand::RequestCollectionHistoryGroup {
            request: CollectionHistoryGroupRequest::new(
                id.clone(),
                1,
                snapshot.committed_revision,
                100,
                SampleCollection::new(0).unwrap(),
                true,
                &[RootRelativePath::try_from_path(PathBuf::from("one.wav")).unwrap()],
            )
            .unwrap(),
            cancellation: CollectionHistoryCancellation::new(),
        })
        .unwrap();
    thread::sleep(Duration::from_millis(2050));
    pause.release();
    let event = receive_until(&service, |e| {
        matches!(e, CatalogEvent::CollectionHistoryGroupCompleted { .. })
    });
    assert!(matches!(
        event,
        CatalogEvent::CollectionHistoryGroupCompleted {
            result: Err(CollectionHistoryFailure::BudgetExceeded),
            ..
        }
    ));
    assert_eq!(
        admission_state(&service.admission, |s| s.tag_read_payload_bytes),
        0
    );
    assert_eq!(
        admission_state(&service.admission, |s| s.tag_page_input_bytes),
        0
    );
    service.shutdown();
}
