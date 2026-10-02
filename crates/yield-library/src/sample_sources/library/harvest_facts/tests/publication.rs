use super::*;
#[test]
fn harvest_facts_owner_exact_untracked_states_and_actual_authored_invalidation() {
    let f = Fixture::new();
    let initial = f.facts().unwrap();
    let e = &initial.entries()[0];
    assert_eq!(e.state(), HarvestState::New);
    assert!(!e.is_tracked());
    assert_eq!(e.immediate_derivative_count(), 0);
    assert!(initial.is_current_global_generation());
    for state in [
        HarvestState::New,
        HarvestState::Seen,
        HarvestState::Touched,
        HarvestState::Done,
        HarvestState::Ignored,
    ] {
        let before = f.facts().unwrap();
        f.apply(f.authored_request(state)).unwrap();
        assert!(!before.is_current_global_generation());
        let after = f.facts().unwrap();
        assert_eq!(after.entries()[0].state(), state);
        assert!(after.entries()[0].is_tracked());
        assert_eq!(
            after.owner_identity().to_string(),
            f.guard().owner_identity()
        );
        assert_ne!(before.generation_identity(), after.generation_identity());
        assert_eq!(
            after.source_revision(),
            content(f.source(), "one.wav").file().source_revision()
        );
    }
    assert!(!initial.is_current_global_generation());
}
#[test]
fn harvest_facts_owner_failed_and_replayed_actual_writes_conservatively_invalidate() {
    let f = Fixture::new();
    let r = f.authored_request(HarvestState::Done);
    let before = f.facts().unwrap();
    // A private stale request is exercised in its own original authored owner tests;
    // use a genuine SQL-aborted write here instead of manufacturing request authority.
    f.db().execute_batch("CREATE TRIGGER abort_authored BEFORE INSERT ON harvest_authored_receipts BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(f.apply(r.clone()).is_err());
    assert!(!before.is_current_global_generation());
    f.db().execute_batch("DROP TRIGGER abort_authored").unwrap();
    f.apply(r.clone()).unwrap();
    let before = f.facts().unwrap();
    assert!(matches!(
        f.apply(r).unwrap(),
        HarvestAuthoredResult::Replayed(_)
    ));
    assert!(!before.is_current_global_generation());
}
#[test]
fn harvest_facts_owner_cancel_before_read_after_read_and_before_delivery() {
    let mut f = Fixture::new();
    let cancel = HarvestFactsCancellation::new();
    let request = f.facts_request(&["one.wav"], cancel.clone()).unwrap();
    cancel.cancel();
    assert!(matches!(
        f.read(request),
        Err(LibraryError::HarvestFactsCanceled)
    ));
    let cancel = HarvestFactsCancellation::new();
    let batch = f
        .read(f.facts_request(&["one.wav"], cancel.clone()).unwrap())
        .unwrap();
    cancel.cancel();
    assert!(!batch.is_current_global_generation());
    let cancel = HarvestFactsCancellation::new();
    let request = f.facts_request(&["one.wav"], cancel.clone()).unwrap();
    let mut old = f.global.take().unwrap();
    old.shutdown();
    drop(old);
    let (g, ready, release) =
        super::super::super::owner::GlobalLibraryWriter::start_with_post_command_gate_for_test(
            f.guard(),
        );
    wait_global(&g);
    f.global = Some(g);
    let reply = f.global().read_harvest_facts(request).unwrap();
    ready.wait();
    cancel.cancel();
    release.wait();
    assert!(matches!(
        reply.recv_timeout(Duration::from_secs(3)).unwrap(),
        Err(LibraryError::HarvestFactsCanceled)
    ));
}
#[test]
fn harvest_facts_owner_shutdown_and_restart_never_revive_prior_generation() {
    let mut f = Fixture::new();
    let batch = f.facts().unwrap();
    f.restart();
    assert!(!batch.is_current_global_generation());
    let fresh = f.facts().unwrap();
    assert!(fresh.is_current_global_generation());
    assert_ne!(batch.generation_identity(), fresh.generation_identity());
    let mut g = f.global.take().unwrap();
    g.shutdown();
    drop(g);
    assert!(!fresh.is_current_global_generation());
}
#[test]
fn harvest_facts_owner_reads_leave_global_and_source_sqlite_revision_wal_unchanged() {
    let f = Fixture::new();
    let request = f
        .facts_request(&["one.wav"], HarvestFactsCancellation::new())
        .unwrap();
    let source = f.source().current_revision().unwrap();
    let db = f.db();
    let version: i64 = db
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    let wal = f.guard().profile_root().join("library.db-wal");
    let before = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    for _ in 0..10 {
        assert!(
            f.read(request.clone())
                .unwrap()
                .is_current_global_generation()
        );
    }
    assert_eq!(f.source().current_revision().unwrap(), source);
    assert_eq!(
        db.query_row::<i64, _, _>("PRAGMA data_version", [], |r| r.get(0))
            .unwrap(),
        version
    );
    assert_eq!(
        std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0),
        before
    );
    assert_eq!(f.count(), 0);
}
#[test]
fn harvest_facts_owner_source_change_before_actual_acknowledgement_rejects_delivery() {
    let mut f = Fixture::new();
    let request = f
        .facts_request(&["one.wav"], HarvestFactsCancellation::new())
        .unwrap();
    let mut old = f.global.take().unwrap();
    old.shutdown();
    drop(old);
    let (g, ready, release) =
        super::super::super::owner::GlobalLibraryWriter::start_with_post_command_gate_for_test(
            f.guard(),
        );
    wait_global(&g);
    f.global = Some(g);
    let reply = f.global().read_harvest_facts(request).unwrap();
    ready.wait();
    f.source_db()
        .execute("UPDATE wav_files SET content_hash=?1", ["b".repeat(64)])
        .unwrap();
    release.wait();
    assert!(matches!(
        reply.recv_timeout(Duration::from_secs(3)).unwrap(),
        Err(LibraryError::HarvestFactsSourceChanged)
    ));
}
#[cfg(unix)]
#[test]
fn harvest_facts_owner_actual_profile_namespace_replacement_ends_generation() {
    let f = Fixture::new();
    let batch = f.facts().unwrap();
    let root = f.guard().profile_root().to_owned();
    let retired = root.with_extension("retired-facts-profile");
    std::fs::rename(&root, &retired).unwrap();
    std::fs::create_dir(&root).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while batch.is_current_global_generation() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    std::fs::remove_dir(&root).unwrap();
    std::fs::rename(&retired, &root).unwrap();
    assert!(!batch.is_current_global_generation());
}

#[test]
fn harvest_facts_owner_cloned_read_client_closes_and_retains_no_profile_ownership() {
    let mut f = Fixture::new();
    let client = f.global().harvest_facts_client();
    let clone = client.clone();
    assert_eq!(client.acquisition_id(), f.global().acquisition_id());
    let request = f
        .facts_request(&["one.wav"], HarvestFactsCancellation::new())
        .unwrap();
    let batch = clone
        .read(request.clone())
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    let mut g = f.global.take().unwrap();
    g.shutdown();
    drop(g);
    assert!(!batch.is_current_global_generation());
    assert!(client.read(request).is_err());
    f.source
        .take()
        .unwrap()
        .into_catalog_database()
        .close()
        .unwrap();
    f.guard.take();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    assert_ne!(client.acquisition_id().as_str(), guard.owner_identity());
}
