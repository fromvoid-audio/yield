use super::*;

#[test]
fn supervised_watcher_does_not_block_catalog_or_overlap_source_generations() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new("catalog-supervised-stalled-watcher");
    let root = fs::canonicalize(fixture.root.path()).expect("canonical source");
    let program_dir = tempfile::tempdir().expect("disposable helper program");
    let executable = program_dir
        .path()
        .canonicalize()
        .unwrap()
        .join("stalled-helper");
    fs::write(
        &executable,
        "#!/bin/sh\nwork=${0%/*}\nif [ -f \"$work/starts\" ] && IFS= read -r original < \"$work/starts\" 2>/dev/null && kill -0 \"$original\" 2>/dev/null; then : > \"$work/overlap\"; fi\nprintf '%s\\n' \"$$\" >> \"$work/starts\"\nkill -STOP \"$$\"\n",
    ).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let gate = Arc::new(CatalogTestGate::new());
    let service = CatalogService::start_configured(
        &fixture.guard,
        Some(gate.clone()),
        None,
        WatcherMode::MacosSupervised,
        None,
        Some(executable),
    )
    .unwrap();
    let source_id = SourceId::from_string("supervised-stalled-source");
    let starts = program_dir.path().join("starts");
    let wait_for_starts = |expected| {
        let deadline = Instant::now() + Duration::from_secs(1);
        while fs::read_to_string(&starts).map_or(0, |text| text.lines().count()) < expected {
            assert!(Instant::now() < deadline, "missing supervised start");
            thread::sleep(Duration::from_millis(5));
        }
    };
    for generation in [1, 2] {
        service
            .submit(CatalogCommand::Register {
                source_id: source_id.clone(),
                canonical_root: root.clone(),
                lifecycle_generation: generation,
            })
            .unwrap();
        assert!(matches!(
            service.recv_event_timeout(Duration::from_secs(1)).unwrap(),
            CatalogEvent::Registered { lifecycle_generation, .. } if lifecycle_generation == generation
        ));
        wait_for_starts(generation as usize);
        assert_eq!(gate.watcher_install_count(&source_id, generation), 0);
        service
            .submit(CatalogCommand::Remove {
                source_id: source_id.clone(),
                lifecycle_generation: generation,
            })
            .unwrap();
        assert!(matches!(
            service.recv_event_timeout(Duration::from_secs(1)).unwrap(),
            CatalogEvent::Removed { lifecycle_generation, .. } if lifecycle_generation == generation
        ));
    }
    assert!(!program_dir.path().join("overlap").exists());
    assert!(service.shutdown_with_report().is_confirmed_join());
}

#[test]
#[ignore = "requires the exact compiled helper path in YIELD_TEST_LIVE_WATCHER_HELPER"]
fn supervised_watcher_publishes_native_change_through_catalog_owner() {
    let executable = std::env::var_os("YIELD_TEST_LIVE_WATCHER_HELPER")
        .map(PathBuf::from)
        .expect("exact helper executable");
    let fixture = Fixture::new("catalog-supervised-native-watcher");
    let root = fs::canonicalize(fixture.root.path()).expect("canonical source root");
    write_test_wav(&root.join("original.wav"), 16_000, 4);
    let source_id = SourceId::from_string("supervised-native-source");
    let gate = Arc::new(CatalogTestGate::new());
    let service = CatalogService::start_configured(
        &fixture.guard,
        Some(gate.clone()),
        None,
        WatcherMode::MacosSupervised,
        None,
        Some(executable),
    )
    .unwrap();
    let initial = register_and_audit(&service, &root, &source_id);
    let wait_for_install = |expected, budget| {
        let deadline = Instant::now() + budget;
        while gate.watcher_install_count(&source_id, 1) < expected {
            assert!(
                Instant::now() < deadline,
                "supervised readiness never arrived"
            );
            thread::sleep(Duration::from_millis(5));
        }
    };
    wait_for_install(1, Duration::from_secs(2));
    assert!(gate.record_watcher_evidence(
        &source_id,
        1,
        WatchHintEvidence::BACKEND_ERROR.union(WatchHintEvidence::FULL_RESCAN),
    ));
    wait_for_install(2, Duration::from_secs(3));
    let changed = Path::new("from-helper.wav");
    write_test_wav(&root.join(changed), 16_000, 4);
    let event = receive_until(&service, |event| match event {
        CatalogEvent::AuditCommitted { snapshot } => snapshot
            .manifest
            .iter()
            .any(|entry| entry.relative_path == changed),
        CatalogEvent::ReplayCommitted { delta, .. } => delta
            .touched_path_changes
            .iter()
            .any(|(path, entry)| path == changed && entry.is_some()),
        _ => false,
    });
    match event {
        CatalogEvent::AuditCommitted { snapshot } => {
            assert_eq!(snapshot.source_id, source_id);
            assert_eq!(snapshot.root_identity, initial.root_identity);
            assert_eq!(snapshot.lifecycle_generation, 1);
            assert!(snapshot.committed_revision > initial.committed_revision);
        }
        CatalogEvent::ReplayCommitted {
            source_id: actual,
            lifecycle_generation,
            root_identity,
            receipt,
            delta,
            ..
        } => {
            assert_eq!(actual, source_id);
            assert_eq!(root_identity, initial.root_identity);
            assert_eq!(lifecycle_generation, 1);
            assert_eq!(receipt.source_revision(), delta.revision);
            assert!(delta.revision > initial.committed_revision);
        }
        _ => unreachable!(),
    }
    assert!(service.shutdown_with_report().is_confirmed_join());
}
