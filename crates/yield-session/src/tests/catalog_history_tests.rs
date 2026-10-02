//! Actual session forwarding to the retained catalog owner, without reserve qualification.
use super::*;
use yield_library::sample_sources::SampleCollection;
use yield_library::sample_sources::reconciliation::RootRelativePath;

#[test]
fn catalog_history_session_forwards_actual_owner_selection() {
    let name = concat!(module_path!(), "::catalog_history_session_child")
        .strip_prefix("yield_session::")
        .unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--ignored", "--nocapture"])
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
                "{output}"
            );
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("session capture child deadline");
        }
        thread::sleep(Duration::from_millis(10));
    }
}
fn receive(
    slot: &Arc<Mutex<catalog::CatalogSlot>>,
    predicate: impl Fn(&CatalogEvent) -> bool,
) -> CatalogEvent {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(event) = catalog::try_recv_event(slot) {
            if predicate(&event) {
                return event;
            }
        }
        assert!(Instant::now() < deadline, "session catalog event deadline");
        thread::sleep(Duration::from_millis(5));
    }
}
#[test]
#[ignore = "executed by isolated actual-session-forwarding parent"]
fn catalog_history_session_child() {
    let temp = TempDir::new().unwrap();
    let _base = ConfigBaseGuard::set(fs::canonicalize(temp.path()).unwrap());
    let profile = profile_root(&temp);
    let _app = AppRootGuard::set(profile).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let source_directory = TempDir::new().unwrap();
    let root = fs::canonicalize(source_directory.path()).unwrap();
    write_test_wav(&root.join("one.wav"));
    let source = SourceId::from_string("session-history");
    let slot = Arc::new(Mutex::new(catalog::CatalogSlot::loading()));
    let make = |generation| {
        CollectionHistoryRequest::new(
            source.clone(),
            1,
            0,
            generation,
            SampleCollection::new(2).unwrap(),
            true,
            &[RootRelativePath::try_from_path("one.wav".into()).unwrap()],
        )
        .unwrap()
    };
    assert_eq!(
        catalog::submit(
            &slot,
            CatalogCommand::RequestCollectionHistory {
                request: make(1),
                cancellation: CollectionHistoryCancellation::new()
            }
        ),
        Err(CatalogSubmitError::NotReady)
    );
    let owner = Arc::new(CatalogService::start(&guard).unwrap());
    assert!(catalog::install(&slot, Arc::clone(&owner)));
    catalog::submit(
        &slot,
        CatalogCommand::Register {
            source_id: source.clone(),
            canonical_root: fs::canonicalize(&root).unwrap(),
            lifecycle_generation: 1,
        },
    )
    .unwrap();
    receive(&slot, |event| {
        matches!(event, CatalogEvent::Registered { .. })
    });
    catalog::submit(
        &slot,
        CatalogCommand::RequestAudit {
            source_id: source.clone(),
            lifecycle_generation: 1,
        },
    )
    .unwrap();
    let CatalogEvent::AuditCommitted { snapshot, .. } = receive(&slot, |event| {
        matches!(event, CatalogEvent::AuditCommitted { .. })
    }) else {
        unreachable!()
    };
    let request = CollectionHistoryRequest::new(
        source.clone(),
        1,
        snapshot.committed_revision,
        12,
        SampleCollection::new(2).unwrap(),
        true,
        &[RootRelativePath::try_from_path("one.wav".into()).unwrap()],
    )
    .unwrap();
    catalog::submit(
        &slot,
        CatalogCommand::RequestCollectionHistory {
            request,
            cancellation: CollectionHistoryCancellation::new(),
        },
    )
    .unwrap();
    let event = receive(&slot, |event| {
        matches!(event, CatalogEvent::CollectionHistoryCompleted { .. })
    });
    let CatalogEvent::CollectionHistoryCompleted {
        request,
        result: Ok(selection),
        ..
    } = event
    else {
        panic!("{event:?}")
    };
    assert_eq!(request.source_id(), &source);
    assert_eq!(request.generation(), 12);
    assert_eq!(selection.revision().source_id(), &source);
    assert_eq!(selection.revision().revision(), snapshot.committed_revision);
    assert_eq!(selection.changes().len(), 1);
    assert!(!selection.changes()[0].before());
    assert!(selection.changes()[0].after());
    catalog::close(&slot);
    assert_eq!(
        catalog::submit(
            &slot,
            CatalogCommand::RequestCollectionHistory {
                request: make(13),
                cancellation: CollectionHistoryCancellation::new()
            }
        ),
        Err(CatalogSubmitError::Closed)
    );
    owner.shutdown();
}
