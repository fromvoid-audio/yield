//! Actual catalog owners, isolated from optional extension process taint.
use super::*;
use std::process::{Command, Stdio};
use yield_library::sample_sources::SampleCollection;

#[test]
fn collection_history_catalog_actual_owner_cases() {
    for case in [
        "bits", "noop", "stale", "cancel", "expiry", "replace", "retire", "remap", "write",
        "missing", "pressure", "bounds",
    ] {
        let name = concat!(module_path!(), "::collection_history_catalog_child")
            .strip_prefix("yield_catalog::")
            .unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--ignored", "--nocapture"])
            .env("YIELD_CATALOG_HISTORY_CASE", case)
            .env_remove("YIELD_SQLITE_EXT")
            .env_remove("YIELD_SQLITE_EXT_ENABLE")
            .env_remove("YIELD_SQLITE_EXT_UNSAFE")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                use std::io::Read;
                let mut output = String::new();
                child
                    .stdout
                    .take()
                    .unwrap()
                    .read_to_string(&mut output)
                    .unwrap();
                assert!(
                    status.success() && output.contains("test result: ok. 1 passed;"),
                    "{case}: {output}"
                );
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{case}: child deadline");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}
fn path(p: &str) -> RootRelativePath {
    RootRelativePath::try_from_path(p.into()).unwrap()
}
fn request(
    source: &SourceId,
    revision: u64,
    generation: u64,
    desired: bool,
) -> CollectionHistoryRequest {
    CollectionHistoryRequest::new(
        source.clone(),
        1,
        revision,
        generation,
        SampleCollection::new(2).unwrap(),
        desired,
        &[path("a.wav"), path("b.wav")],
    )
    .unwrap()
}
fn submit(
    service: &CatalogService,
    request: CollectionHistoryRequest,
    cancellation: CollectionHistoryCancellation,
) {
    service
        .submit(CatalogCommand::RequestCollectionHistory {
            request,
            cancellation,
        })
        .unwrap();
}
fn delivery(service: &CatalogService, generation: u64) -> CatalogDelivery {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(d) = service.recv_delivery_timeout(Duration::from_millis(25)) {
            if matches!(&d.event, CatalogEvent::CollectionHistoryCompleted { request, .. } if request.generation() == generation)
            {
                return d;
            }
        }
        assert!(Instant::now() < deadline, "missing history completion");
    }
}
fn drain(service: &CatalogService) {
    wait_for_tag_page_input_release(service);
    while service.try_recv_delivery().is_ok() {}
    wait_for_tag_read_payload(service, 0);
}
#[test]
#[ignore = "executed by isolated actual-owner parent"]
fn collection_history_catalog_child() {
    let case = std::env::var("YIELD_CATALOG_HISTORY_CASE").unwrap();
    if case == "bounds" {
        let source = SourceId::from_string("bounds");
        let make = |paths: &[RootRelativePath], generation| {
            CollectionHistoryRequest::new(
                source.clone(),
                1,
                1,
                generation,
                SampleCollection::new(2).unwrap(),
                true,
                paths,
            )
        };
        assert!(make(&[], 1).is_err());
        assert!(make(&[path("unsupported.txt")], 1).is_err());
        for spelling in ["./a.wav", "a/./b.wav", "a//b.wav"] {
            assert!(
                make(&[path(spelling)], 1).is_err(),
                "accepted ambiguous key {spelling}"
            );
        }
        assert!(make(&[path("a.wav")], 0).is_err());
        assert!(make(&[path("a.wav"), path("a.wav")], 1).is_err());
        assert!(make(&[path(&"x".repeat(4097))], 1).is_err());
        assert!(
            make(
                &(0..129)
                    .map(|i| path(&format!("{i}.wav")))
                    .collect::<Vec<_>>(),
                1
            )
            .is_err()
        );
        assert!(
            make(
                &(0..128)
                    .map(|i| path(&format!("{i}/{}", "x".repeat(4000))))
                    .collect::<Vec<_>>(),
                1
            )
            .is_err()
        );
        assert!(
            make(
                &(0..128)
                    .map(|i| path(&format!("{i}.wav")))
                    .collect::<Vec<_>>(),
                1
            )
            .is_ok()
        );
        #[cfg(unix)]
        assert!(make(&[path("a\\b.wav")], 1).is_err());
        return;
    }
    let fixture = Fixture::new("history-catalog");
    for p in ["a.wav", "b.wav"] {
        write_test_wav(&fixture.root.path().join(p), 16_000, 4000);
    }
    let source = SourceId::from_string("history-catalog");
    let (service, gate) = CatalogService::start_with_test_gate(&fixture.guard).unwrap();
    let snapshot = register_and_audit(&service, fixture.root.path(), &source);
    let directory = fixture
        .guard
        .profile_root()
        .join(catalog_profile_child(&source));
    let sql = rusqlite::Connection::open_with_flags(
        directory.join(".yield.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .unwrap();
    sql.execute(
        "INSERT INTO wav_file_collections(path,collection) VALUES('a.wav',2)",
        [],
    )
    .unwrap();
    let before: Vec<_> = [".yield.db", ".yield.db-wal"]
        .into_iter()
        .filter_map(|name| fs::read(directory.join(name)).ok().map(|data| (name, data)))
        .collect();
    let revision = snapshot.committed_revision;
    match case.as_str() {
        "bits" | "noop" => {
            if case == "noop" {
                sql.execute(
                    "INSERT INTO wav_file_collections(path,collection) VALUES('b.wav',2)",
                    [],
                )
                .unwrap();
            }
            for (generation, desired) in [(1, true), (2, false)] {
                submit(
                    &service,
                    request(&source, revision, generation, desired),
                    CollectionHistoryCancellation::new(),
                );
                let (event, fence) = delivery(&service, generation).into_parts_if_live().unwrap();
                assert!(fence.unwrap().is_live());
                let CatalogEvent::CollectionHistoryCompleted {
                    request: context,
                    result: Ok(selection),
                    ..
                } = event
                else {
                    panic!("{event:?}");
                };
                assert_eq!(context.generation(), generation);
                assert_eq!(selection.revision().revision(), revision);
                assert_eq!(
                    selection.changes().len(),
                    if case == "noop" {
                        if desired { 0 } else { 2 }
                    } else {
                        1
                    }
                );
                for change in selection.changes() {
                    assert_eq!(change.before(), !desired);
                    assert_eq!(change.after(), desired);
                }
            }
        }
        "stale" => {
            submit(
                &service,
                request(&source, revision + 1, 1, true),
                CollectionHistoryCancellation::new(),
            );
            assert!(matches!(
                delivery(&service, 1).into_event_if_live().unwrap(),
                CatalogEvent::CollectionHistoryCompleted {
                    result: Err(CollectionHistoryFailure::StaleSource),
                    ..
                }
            ));
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            sql.execute(
                "UPDATE source_lease SET acquired_at=?1-10, expires_at=?1",
                [now - 1],
            )
            .unwrap();
            submit(
                &service,
                request(&source, revision, 2, true),
                CollectionHistoryCancellation::new(),
            );
            assert!(matches!(
                delivery(&service, 2).into_event_if_live().unwrap(),
                CatalogEvent::CollectionHistoryCompleted {
                    result: Err(CollectionHistoryFailure::StaleSource),
                    ..
                }
            ));
        }
        "cancel" => {
            let token = CollectionHistoryCancellation::new();
            token.cancel();
            submit(&service, request(&source, revision, 1, true), token);
            drain(&service);
            let token = CollectionHistoryCancellation::new();
            submit(&service, request(&source, revision, 2, true), token.clone());
            let queued = delivery(&service, 2);
            token.cancel();
            assert!(queued.into_parts_if_live().is_none());
        }
        "expiry" => {
            submit(
                &service,
                request(&source, revision, 1, true),
                CollectionHistoryCancellation::new(),
            );
            wait_for_tag_page_input_release(&service);
            assert_eq!(
                admission_state(&service.admission, |s| s.tag_read_payload_bytes),
                history_selection::MAX_RESULT_BYTES
            );
            thread::sleep(Duration::from_millis(2100));
            let (event, fence) = delivery(&service, 1).into_parts_if_live().unwrap();
            assert!(fence.is_none());
            assert!(matches!(
                event,
                CatalogEvent::CollectionHistoryCompleted {
                    result: Err(CollectionHistoryFailure::ObservationExpired),
                    ..
                }
            ));
        }
        "replace" | "retire" => {
            submit(
                &service,
                request(&source, revision, 1, true),
                CollectionHistoryCancellation::new(),
            );
            let queued = delivery(&service, 1);
            if case == "replace" {
                submit(
                    &service,
                    request(&source, revision, 2, true),
                    CollectionHistoryCancellation::new(),
                );
                let _ = delivery(&service, 2);
            } else {
                service
                    .submit(CatalogCommand::Remove {
                        source_id: source.clone(),
                        lifecycle_generation: 1,
                    })
                    .unwrap();
            }
            assert!(queued.into_parts_if_live().is_none());
        }
        "remap" => {
            submit(
                &service,
                request(&source, revision, 1, true),
                CollectionHistoryCancellation::new(),
            );
            let queued = delivery(&service, 1);
            service
                .submit(CatalogCommand::Register {
                    source_id: source.clone(),
                    canonical_root: fixture.root.path().to_path_buf(),
                    lifecycle_generation: 2,
                })
                .unwrap();
            assert!(queued.into_parts_if_live().is_none());
        }
        "write" => {
            submit(
                &service,
                request(&source, revision, 1, true),
                CollectionHistoryCancellation::new(),
            );
            let queued = delivery(&service, 1);
            let receipt = apply_curation_updates(
                &service,
                &source,
                &snapshot,
                41,
                vec![SourceCurationUpdate::AddCollection {
                    path: path("b.wav"),
                    collection: SampleCollection::new(2).unwrap(),
                }],
            );
            assert_eq!(receipt.source_revision(), revision + 1);
            assert!(
                queued.into_parts_if_live().is_none(),
                "old revision projected after actual commit"
            );
            submit(
                &service,
                request(&source, receipt.source_revision(), 2, true),
                CollectionHistoryCancellation::new(),
            );
            assert!(
                matches!(delivery(&service, 2).into_event_if_live().unwrap(), CatalogEvent::CollectionHistoryCompleted { result: Ok(selection), .. } if selection.changes().is_empty())
            );
        }
        "missing" => {
            let request = CollectionHistoryRequest::new(
                source.clone(),
                1,
                revision,
                1,
                SampleCollection::new(2).unwrap(),
                true,
                &[path("missing.wav")],
            )
            .unwrap();
            submit(&service, request, CollectionHistoryCancellation::new());
            assert!(matches!(
                delivery(&service, 1).into_event_if_live().unwrap(),
                CatalogEvent::CollectionHistoryCompleted { result: Err(_), .. }
            ));
            let request = CollectionHistoryRequest::new(
                source.clone(),
                2,
                revision,
                2,
                SampleCollection::new(2).unwrap(),
                true,
                &[path("a.wav")],
            )
            .unwrap();
            submit(&service, request, CollectionHistoryCancellation::new());
            drain(&service);
            assert!(service.try_recv_delivery().is_err());
        }
        "pressure" => {
            let pause = gate.pause_next();
            let token = CollectionHistoryCancellation::new();
            token.cancel();
            for generation in 1..=32 {
                submit(
                    &service,
                    request(&source, revision, generation, true),
                    token.clone(),
                );
            }
            assert_eq!(
                admission_state(&service.admission, |s| s.tag_read_payload_bytes),
                32 * history_selection::MAX_RESULT_BYTES
            );
            assert_eq!(
                service.submit(CatalogCommand::RequestCollectionHistory {
                    request: request(&source, revision, 33, true),
                    cancellation: token.clone()
                }),
                Err(CatalogSubmitError::Full)
            );
            drop(pause);
            drain(&service);
        }
        _ => panic!("unknown child"),
    }
    if matches!(
        case.as_str(),
        "bits" | "cancel" | "expiry" | "replace" | "pressure"
    ) {
        for (name, bytes) in before {
            assert_eq!(
                fs::read(directory.join(name)).unwrap(),
                bytes,
                "read transport changed {name}"
            );
        }
    }
    drain(&service);
    service.shutdown();
    assert_eq!(
        service.submit(CatalogCommand::RequestCollectionHistory {
            request: request(&source, revision, 99, true),
            cancellation: CollectionHistoryCancellation::new()
        }),
        Err(CatalogSubmitError::Closed)
    );
}
