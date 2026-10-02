use super::*;
use yield_library::sample_sources::db::DurableReplayUncertaintyState;

fn assert_audit(f: &Fixture, service: &CatalogService, batch: ReplayObservationBatch) {
    let before = f.read().get_revision().unwrap();
    let request = service.submit_replay(batch, 7, before).unwrap();
    assert!(matches!(
        Fixture::receive(&request),
        CatalogReplayOutcome::AuditRequired { .. }
    ));
    assert_eq!(f.read().get_revision().unwrap(), before);
    assert!(matches!(
        f.read().read_durable_replay_uncertainty().unwrap(),
        DurableReplayUncertaintyState::Retained(_)
    ));
}

#[test]
fn missing_prior_and_stale_prior_require_audit_without_replay_commit() {
    for bootstrap in [false, true] {
        let f = Fixture::new(bootstrap);
        let service = f.start();
        assert_audit(
            &f,
            &service,
            f.batch(if bootstrap { 5 } else { 10 }, &[(Some(15), "one.wav")]),
        );
        service.shutdown();
    }
}

#[test]
fn wrong_stream_root_and_generation_cannot_advance_the_checkpoint() {
    for identity in 0..3 {
        let f = Fixture::new(true);
        let service = f.start();
        let root = if identity == 1 {
            RootIdentity::from_bytes(b"another-root".to_vec())
        } else {
            f.identity()
        };
        let ingress = ReplayIngress::new(
            SourceWatchSpec::new(
                f.source_id(),
                7,
                f.base.root.path().to_path_buf(),
                Some(root),
            ),
            if identity == 0 { 100 } else { 99 },
            [2; 16],
            10,
        )
        .unwrap();
        ingress.record_with_kind(
            Some(11),
            &f.base.root.path().join("one.wav"),
            ReplayEventCondition::Changed,
            ReplayPathKind::File,
        );
        ingress.mark_history_complete();
        let batch = ingress
            .into_candidate()
            .into_observation_batch(7, WatcherGeneration::new(if identity == 2 { 8 } else { 7 }))
            .unwrap();
        assert_audit(&f, &service, batch);
        service.shutdown();
    }
}

#[test]
fn actual_producer_overflow_and_unknown_directory_scope_widen_without_ack() {
    for overflow in [false, true] {
        let f = Fixture::new(true);
        let service = f.start();
        let ingress = f.ingress(10);
        for i in 0..if overflow { MAX_REPLAY_EVENTS + 1 } else { 1 } {
            ingress.record_with_kind(
                Some(11 + i as u64),
                &f.base.root.path().join("one.wav"),
                ReplayEventCondition::Changed,
                if overflow {
                    ReplayPathKind::File
                } else {
                    ReplayPathKind::Directory
                },
            );
        }
        ingress.mark_history_complete();
        let batch = ingress
            .into_candidate()
            .into_observation_batch(7, WatcherGeneration::new(7))
            .unwrap();
        assert_audit(&f, &service, batch);
        service.shutdown();
    }
}

#[test]
fn actual_sql_mutation_failure_rolls_back_manifest_index_and_checkpoint() {
    let mut f = Fixture::new(true);
    f.fail_replay_update = true;
    let service = f.start();
    let old = f
        .read()
        .entry_for_path(Path::new("one.wav"))
        .unwrap()
        .unwrap();
    f.put("one.wav", b"must roll back");
    f.put("notes.txt", b"must roll back index");
    assert_audit(
        &f,
        &service,
        f.batch(10, &[(Some(11), "one.wav"), (Some(12), "notes.txt")]),
    );
    let read = f.read();
    assert_eq!(
        read.entry_for_path(Path::new("one.wav"))
            .unwrap()
            .unwrap()
            .content_hash,
        old.content_hash
    );
    assert_eq!(
        read.source_index_entries_for_exact_paths(&[RootRelativePath::try_from_path(
            PathBuf::from("notes.txt")
        )
        .unwrap()])
            .unwrap()
            .entries[0]
            .file_size,
        Some(14)
    );
    service.shutdown();
}

#[test]
fn physical_root_replacement_returns_original_batch_without_ack() {
    let f = Fixture::new(true);
    let service = f.start();
    let before = f.read().get_revision().unwrap();
    let batch = f.batch(10, &[(Some(11), "one.wav")]);
    let moved = f.base.root.path().with_extension("replay-held-root");
    fs::rename(f.base.root.path(), &moved).unwrap();
    fs::create_dir(f.base.root.path()).unwrap();
    let request = service.submit_replay(batch.clone(), 7, before).unwrap();
    let CatalogReplayOutcome::Retained {
        batch: returned, ..
    } = Fixture::receive(&request)
    else {
        panic!("replacement must retain raw input");
    };
    assert_eq!(returned, batch);
    assert!(fs::read_dir(f.base.root.path()).unwrap().next().is_none());
    service.shutdown();
    fs::remove_dir(f.base.root.path()).unwrap();
    fs::rename(moved, f.base.root.path()).unwrap();
    assert_eq!(f.read().get_revision().unwrap(), before);
}
