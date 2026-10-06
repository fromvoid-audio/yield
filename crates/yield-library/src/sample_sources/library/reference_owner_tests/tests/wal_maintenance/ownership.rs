use super::*;

#[test]
fn reference_wal_actual_worker_postcommit_source_loss_keeps_receipt_and_checkpoints() {
    let mut f = ReferenceFixture::new();
    f.seed();
    let request = f.request(1);
    let bytes = request.proposal_bytes().to_vec();
    stop_global(&mut f);
    let counter = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let global = GlobalLibraryWriter::start_with_database_hook_for_test(
        f.guard.as_ref().unwrap(),
        pressure_hook(counter.clone(), Fault::None),
        Some((ready.clone(), release.clone())),
    )
    .unwrap();
    wait_until_available(&global);
    f.global = Some(global);
    let reply = f.global().apply_reference_rekey(request).unwrap();
    ready.wait();
    assert_eq!(f.count("SELECT COUNT(*) FROM reference_receipts"), 1);
    assert_eq!(f.count("SELECT COUNT(*) FROM harvest_derivations"), 3);
    let old = f.origin.take().unwrap();
    f.origin = Some(
        SourceWriter::new(
            old.into_catalog_database(),
            SourceId::from_string("origin"),
            8,
        )
        .unwrap(),
    );
    release.wait();
    assert!(matches!(
        reply.recv_timeout(Duration::from_secs(5)).unwrap(),
        Err(LibraryError::ReferenceCommitUncertain)
    ));
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    stop_global(&mut f);
    let global = GlobalLibraryWriter::start(f.guard.as_ref().unwrap()).unwrap();
    wait_until_available(&global);
    f.global = Some(global);
    assert!(matches!(
        f.apply({
            let (origin, destination) = f.fences();
            ReferenceRekeyRequest::restore(f.owner(), &bytes, origin, destination).unwrap()
        })
        .unwrap(),
        ReferenceRekeyResult::Replayed(_)
    ));
    assert_eq!(f.count("SELECT COUNT(*) FROM harvest_derivations"), 3);
}

#[cfg(unix)]
#[test]
fn reference_wal_actual_worker_profile_loss_before_or_during_maintenance_closes() {
    for during in [false, true] {
        let mut f = ReferenceFixture::new();
        f.seed();
        let request = f.request(1);
        stop_global(&mut f);
        let counter = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let root = f.guard.as_ref().unwrap().profile_root().to_path_buf();
        let moved = root.with_extension("reference-maintenance-moved");
        let hook = if during {
            let counter = counter.clone();
            let root = root.clone();
            let moved = moved.clone();
            Arc::new(move |connection: &Connection| {
                pressure_hook(Arc::new(AtomicUsize::new(0)), Fault::None)(connection);
                let root = root.clone();
                let moved = moved.clone();
                let counter = counter.clone();
                connection.authorizer(Some(move |context: AuthContext<'_>| {
                    if matches!(
                        context.action,
                        AuthAction::Pragma {
                            pragma_name: "wal_checkpoint",
                            ..
                        }
                    ) {
                        counter.fetch_add(1, Ordering::SeqCst);
                        std::fs::rename(&root, &moved).unwrap();
                        std::fs::create_dir(&root).unwrap();
                    }
                    Authorization::Allow
                }));
            }) as Arc<dyn Fn(&Connection) + Send + Sync>
        } else {
            pressure_hook(counter.clone(), Fault::None)
        };
        let gate = (!during).then(|| (ready.clone(), release.clone()));
        let global = GlobalLibraryWriter::start_with_database_hook_for_test(
            f.guard.as_ref().unwrap(),
            hook,
            gate,
        )
        .unwrap();
        wait_until_available(&global);
        f.global = Some(global);
        let reply = f.global().apply_reference_rekey(request).unwrap();
        if !during {
            ready.wait();
            assert_eq!(f.count("SELECT COUNT(*) FROM reference_receipts"), 1);
            std::fs::rename(&root, &moved).unwrap();
            std::fs::create_dir(&root).unwrap();
            release.wait();
        }
        assert!(reply.recv_timeout(Duration::from_secs(5)).unwrap().is_err());
        let deadline = Instant::now() + Duration::from_secs(5);
        while f.global().status() != GlobalLibraryWriterStatus::Closed {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(f.global().unavailable_reason(), Some(crate::sample_sources::library::GlobalLibraryWriterUnavailable::ProfileOwnershipChanged { .. })));
        assert!(matches!(
            f.global().load_source_registry_snapshot(),
            Err(crate::sample_sources::library::GlobalLibraryWriterQueueError::Closed)
        ));
        assert_eq!(counter.load(Ordering::SeqCst), usize::from(during));
        let retained = Connection::open_with_flags(
            moved.join(crate::sample_sources::library::LIBRARY_DB_FILE_NAME),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(
            retained
                .query_row("SELECT count(*) FROM reference_receipts", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            retained
                .query_row("SELECT count(*) FROM harvest_derivations WHERE parent_source_id='destination' OR child_source_id='destination'", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            3
        );
        drop(retained);
        stop_global(&mut f);
        for source in [f.origin.take().unwrap(), f.destination.take().unwrap()] {
            let _ = source.into_catalog_database().close();
        }
    }
}
