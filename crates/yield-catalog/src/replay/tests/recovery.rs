use super::*;
use yield_library::sample_sources::db::DurableReplayUncertaintyState;

fn event(service: &CatalogService, predicate: impl Fn(&CatalogEvent) -> bool) -> CatalogEvent {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match service.recv_event_timeout(Duration::from_millis(10)) {
            Ok(e) if predicate(&e) => return e,
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => assert!(Instant::now() < deadline),
            Err(e) => panic!("worker closed before audit outcome: {e}"),
        }
    }
}

#[test]
fn correlated_complete_audit_releases_failed_raw_capacity_without_replay_authority() {
    let f = Fixture::new(true);
    let service = f.start();
    let before = f.read().get_revision().unwrap();
    let request = service
        .submit_replay(f.batch(10, &[(None, "one.wav")]), 7, before)
        .unwrap();
    assert!(matches!(
        Fixture::receive(&request),
        CatalogReplayOutcome::AuditRequired { .. }
    ));
    drop(request);
    let permits = (0..MAX_CATALOG_REPLAY_REQUESTS - 1)
        .map(|_| Permit::acquire().unwrap())
        .collect::<Vec<_>>();
    assert!(matches!(
        Permit::acquire(),
        Err(CatalogReplaySubmitError::Busy)
    ));
    service
        .submit(CatalogCommand::RequestAudit {
            source_id: f.source_id(),
            lifecycle_generation: 7,
        })
        .unwrap();
    event(&service, |e| {
        matches!(e, CatalogEvent::AuditCommitted { .. })
    });
    let released = Permit::acquire().expect("covering real audit releases retained raw slot");
    // The complete audit retires its correlated raw boundary, but no live empty FSEvents capture
    // occurred on Linux: the persisted uncertainty must continue to block replay bootstrap.
    assert!(matches!(
        f.read().read_durable_replay_uncertainty().unwrap(),
        DurableReplayUncertaintyState::Retained(_)
    ));
    assert!(
        f.read()
            .read_durable_replay_receipt(
                &f.source_id(),
                &f.identity(),
                WatcherGeneration::new(7),
                WatcherGeneration::new(7)
            )
            .unwrap()
            .is_none()
    );
    drop(permits);
    drop(released);
    service.shutdown();
}

#[test]
fn interrupted_real_audit_retains_raw_marker_and_last_good_without_acknowledgement() {
    let f = Fixture::new(true);
    let (service, gate) = f.start_with_gate();
    let before = f.read().get_revision().unwrap();
    let batch = f.batch(10, &[(None, "one.wav")]);
    let request = service.submit_replay(batch.clone(), 7, before).unwrap();
    assert!(matches!(
        Fixture::receive(&request),
        CatalogReplayOutcome::AuditRequired { .. }
    ));
    drop(request);
    for i in 0..150 {
        f.put(&format!("new-{i}.wav"), b"new files for partial traversal");
    }
    gate.cancel_on_audit_progress.store(true, Ordering::Release);
    service
        .submit(CatalogCommand::RequestAudit {
            source_id: f.source_id(),
            lifecycle_generation: 7,
        })
        .unwrap();
    event(&service, |e| {
        matches!(
            e,
            CatalogEvent::AuditCanceled { .. } | CatalogEvent::Failed { .. }
        )
    });
    let pause = gate.pause_next();
    assert!(
        f.read().get_revision().unwrap() > before,
        "actual interrupted traversal committed a partial audit checkpoint"
    );
    assert!(matches!(
        f.read().read_durable_replay_uncertainty().unwrap(),
        DurableReplayUncertaintyState::Retained(_)
    ));
    let permits = (0..MAX_CATALOG_REPLAY_REQUESTS - 1)
        .map(|_| Permit::acquire().unwrap())
        .collect::<Vec<_>>();
    assert!(
        matches!(Permit::acquire(), Err(CatalogReplaySubmitError::Busy)),
        "interrupted audit cannot release retained raw"
    );
    drop(permits);
    drop(pause);
    let retry = service.submit_replay(batch, 7, before).unwrap();
    assert!(matches!(
        Fixture::receive(&retry),
        CatalogReplayOutcome::AuditRequired { .. }
    ));
    service.shutdown();
}
