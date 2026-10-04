//! Real never-audited source producers on the original catalog worker and shared FIFO.
use super::*;
use yield_library::sample_sources::db::{
    SourceRegistryAuditObservation, SourceRegistryRegistrationPhase,
};
fn held(request: &mut CatalogSourceWalRequest) -> CatalogSourceWalPreparation {
    wait(request, |s| {
        matches!(s, CatalogSourceWalStatus::PreparedLease(_))
    });
    request.try_prepared().unwrap()
}
fn source(
    input: &yield_library::sample_sources::db::write::SourceLeaseMaintenanceAllocation,
) -> SourceId {
    SourceId::from_string(input.subject().source_id().to_owned())
}
#[test]
fn lease_two_never_audited_sources_use_original_worker_without_renewal_or_fake_revision() {
    let f = Fixture::new();
    let (service, inputs, _) = f.start_lease(None, None);
    let a = f.external_id(&source(&inputs[0]));
    let b = f.external_id(&source(&inputs[1]));
    let before = [(lease(&a), metadata(&a)), (lease(&b), metadata(&b))];
    let original_deadline = inputs[0].deadline();
    let mut first = service
        .prepare_source_lease_wal_with_binding(
            inputs[0].clone(),
            correlation(),
            original_deadline + Duration::from_secs(1),
        )
        .unwrap();
    let mut second = service
        .prepare_source_lease_wal_with_binding(inputs[1].clone(), correlation(), deadline())
        .unwrap();
    let holder_a = held(&mut first);
    let holder_b = held(&mut second);
    busy(&a);
    busy(&b);
    let CatalogSourceWalStatus::PreparedLease(history) = holder_a.status() else {
        panic!("genuine lease")
    };
    assert_eq!(
        history.original_binding().stored_fence().subject().audit(),
        &SourceRegistryAuditObservation::Unavailable {}
    );
    assert_eq!(
        history
            .original_binding()
            .stored_fence()
            .subject()
            .registration()
            .unwrap()
            .phase(),
        SourceRegistryRegistrationPhase::Prepared
    );
    assert_eq!(
        holder_a.acknowledge_settlement(),
        Err(CatalogSourceWalRejection::SettlementUnsupported)
    );
    let copy = history.clone();
    assert_eq!(
        copy.scope_descriptor_bytes().as_ptr(),
        history.scope_descriptor_bytes().as_ptr()
    );
    assert_eq!(
        [(lease(&a), metadata(&a)), (lease(&b), metadata(&b))],
        before
    );
    assert_eq!(credits(&service).0, 2);
    let duplicate = service
        .prepare_source_lease_wal_with_binding(inputs[0].clone(), correlation(), deadline())
        .unwrap();
    wait(&duplicate, |s| {
        *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Busy)
    });
    drop(holder_a);
    drop(holder_b);
    wait(&first, |s| *s == CatalogSourceWalStatus::Aborted);
    wait(&second, |s| *s == CatalogSourceWalStatus::Aborted);
    a.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    b.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert_eq!(
        [(lease(&a), metadata(&a)), (lease(&b), metadata(&b))],
        before
    );
    assert_eq!(credits(&service).0, 0);
    assert!(credits(&service).1 > 0);
    service.shutdown();
    drop(history);
    assert!(credits(&service).1 > 0);
    drop(copy);
    assert_eq!(credits(&service), (0, 0));
}
#[test]
fn lease_pending_cancel_and_queue_deadline_leave_sqlite_unchanged() {
    let f = Fixture::new();
    let (service, inputs, gate) = f.start_lease(None, None);
    let pause = gate.pause_next();
    let db = f.external_id(&source(&inputs[0]));
    let before = (lease(&db), metadata(&db));
    let request = service
        .prepare_source_lease_wal_with_binding(inputs[0].clone(), correlation(), deadline())
        .unwrap();
    let witness = request.outcome_probe_for_tests();
    drop(request);
    let expired = service
        .prepare_source_lease_wal_with_binding(
            inputs[1].clone(),
            correlation(),
            Instant::now() + Duration::from_millis(30),
        )
        .unwrap();
    thread::sleep(Duration::from_millis(50));
    assert_eq!(credits(&service).0, 2);
    db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert_eq!((lease(&db), metadata(&db)), before);
    drop(pause);
    wait(&expired, |s| {
        *s == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Deadline)
    });
    let end = Instant::now() + Duration::from_secs(1);
    while witness.status() != CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Cancelled)
    {
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(credits(&service), (0, 0));
    service.shutdown();
}
#[test]
fn lease_real_busy_and_unknown_retained_wal_reject_without_prepared_or_renewal() {
    for unknown in [false, true] {
        let f = Fixture::new();
        let (service, inputs, gate) = f.start_lease(None, None);
        let pause = gate.pause_next();
        let db = f.external_id(&source(&inputs[0]));
        let before = (lease(&db), metadata(&db));
        if unknown {
            service
                .source_wal_transport()
                .set_unknown_wal_for_tests(true);
        } else {
            db.execute_batch("BEGIN IMMEDIATE").unwrap();
        }
        let request = service
            .prepare_source_lease_wal_with_binding(inputs[0].clone(), correlation(), deadline())
            .unwrap();
        drop(pause);
        let rejected = wait(&request, |s| {
            matches!(s, CatalogSourceWalStatus::Rejected(_))
        });
        // Full native allocation validation precedes watermark admission; an unreadable
        // retained WAL can fail that exact scope check before the WAL-specific check.
        assert!(
            matches!(
                rejected,
                CatalogSourceWalStatus::Rejected(
                    CatalogSourceWalRejection::Scope | CatalogSourceWalRejection::WalUnavailable
                )
            ),
            "unknown={unknown}: {rejected:?}"
        );
        if !unknown {
            db.execute_batch("ROLLBACK").unwrap();
        }
        assert_eq!((lease(&db), metadata(&db)), before);
        assert_eq!(credits(&service), (0, 0));
        service.shutdown();
    }
}
#[test]
fn lease_real_rollback_and_timeout_failures_retain_original_custody_until_confirmed_cleanup() {
    for timeout in [false, true] {
        let fault = Arc::new(AtomicBool::new(false));
        let f = Fixture::new();
        let (service, inputs, _) = if timeout {
            f.start_lease(None, Some(fault.clone()))
        } else {
            f.start_lease(Some(fault.clone()), None)
        };
        let db = f.external_id(&source(&inputs[0]));
        let original = metadata(&db);
        let mut request = service
            .prepare_source_lease_wal_with_binding(inputs[0].clone(), correlation(), deadline())
            .unwrap();
        let holder = held(&mut request);
        fault.store(true, Ordering::Release);
        holder.abort();
        wait(&request, |s| {
            matches!(s, CatalogSourceWalStatus::Unavailable(_))
        });
        assert_eq!(credits(&service).0, 1);
        assert_eq!(metadata(&db), original);
        if !timeout {
            busy(&db);
        } else {
            db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        }
        assert!(!matches!(
            request.status(),
            CatalogSourceWalStatus::Aborted | CatalogSourceWalStatus::Retired
        ));
        fault.store(false, Ordering::Release);
        wait(&request, |s| *s == CatalogSourceWalStatus::Aborted);
        assert_eq!(credits(&service), (0, 0));
        db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        service.shutdown();
    }
}
#[test]
fn lease_lifecycle_replacement_cannot_acquire_a_second_owner_while_failed_original_is_held() {
    let deny = Arc::new(AtomicBool::new(false));
    let f = Fixture::new();
    let (service, inputs, _) = f.start_lease(Some(deny.clone()), None);
    let id = source(&inputs[0]);
    let db = f.external_id(&id);
    let mut request = service
        .prepare_source_lease_wal_with_binding(inputs[0].clone(), correlation(), deadline())
        .unwrap();
    let holder = held(&mut request);
    deny.store(true, Ordering::Release);
    holder.abort();
    wait(&request, |s| {
        matches!(s, CatalogSourceWalStatus::Unavailable(_))
    });
    service
        .submit(CatalogCommand::Register {
            source_id: id,
            canonical_root: f.base.root.path().to_path_buf(),
            lifecycle_generation: 2,
        })
        .unwrap();
    let end = Instant::now() + Duration::from_secs(1);
    loop {
        if let Ok(CatalogEvent::Failed {
            operation: CatalogOperation::Register,
            error: CatalogError::SourceWriterConflict(_),
            ..
        }) = service.try_recv_event()
        {
            break;
        }
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(2));
    }
    busy(&db);
    deny.store(false, Ordering::Release);
    wait(&request, |s| {
        matches!(
            s,
            CatalogSourceWalStatus::Aborted
                | CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Scope)
        )
    });
    db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    service.shutdown();
}
#[test]
fn lease_and_audited_commands_exhaust_the_original_combined_pool_without_extra_worker_or_budget() {
    let mut f = Fixture::new();
    f.mixed = true;
    let (service, revisions, inputs, gate) = f.start_mode(None, None, None, true);
    assert_eq!(revisions.len(), 1);
    let pause = gate.pause_next();
    let mut requests = Vec::new();
    loop {
        let next = if requests.len() % 2 == 0 {
            service.prepare_source_lease_wal_with_binding(
                inputs[0].clone(),
                correlation(),
                deadline(),
            )
        } else {
            service.prepare_source_wal_with_binding(revisions[0].clone(), correlation(), deadline())
        };
        match next {
            Ok(r) => requests.push(r),
            Err(CatalogSourceWalRejection::Full) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(credits(&service).0, requests.len());
    assert!(requests.len() > 2 && requests.len() <= 64 && credits(&service).1 <= 64 * 1024);
    service.source_wal_transport().close();
    assert_eq!(credits(&service), (0, 0));
    assert!(
        requests
            .iter()
            .all(|r| r.status()
                == CatalogSourceWalStatus::Rejected(CatalogSourceWalRejection::Closed))
    );
    drop(pause);
    service.shutdown();
}
#[test]
fn lease_normal_deferral_preserves_32_command_queue_and_unrelated_audited_read_progress() {
    let mut f = Fixture::new();
    f.mixed = true;
    let (service, revisions, inputs, _) = f.start_mode(None, None, None, true);
    let mut request = service
        .prepare_source_lease_wal_with_binding(inputs[0].clone(), correlation(), deadline())
        .unwrap();
    let holder = held(&mut request);
    let id = source(&inputs[0]);
    service
        .submit(CatalogCommand::ReadSnapshot {
            source_id: id.clone(),
            expected_revision: None,
        })
        .unwrap();
    service
        .submit(CatalogCommand::RequestTagPage {
            source_id: revisions[0].source_id().clone(),
            lifecycle_generation: 1,
            expected_revision: revisions[0].revision(),
            request_id: 2260,
            query: CatalogTagPageQuery::SelectedPaths {
                relative_paths: vec![PathBuf::from("one.wav")],
            },
            after_cursor: None,
            page_limit: 1,
        })
        .unwrap();
    let end = Instant::now() + Duration::from_secs(1);
    loop {
        if let Ok(CatalogEvent::TagPageReady {
            request_id: 2260, ..
        }) = service.try_recv_event()
        {
            break;
        }
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(2));
    }
    for _ in 1..COMMAND_CAPACITY {
        service
            .submit(CatalogCommand::ReadSnapshot {
                source_id: id.clone(),
                expected_revision: None,
            })
            .unwrap();
    }
    assert_eq!(
        service.submit(CatalogCommand::ReadSnapshot {
            source_id: id,
            expected_revision: None
        }),
        Err(CatalogSubmitError::Full)
    );
    assert_eq!(service.try_command_metrics().unwrap().pending_dispatch, 32);
    drop(holder);
    wait(&request, |s| *s == CatalogSourceWalStatus::Aborted);
    service.shutdown();
    assert_eq!(credits(&service), (0, 0));
}
#[test]
fn lease_lost_request_delivery_and_shutdown_require_actual_original_owner_cleanup() {
    let f = Fixture::new();
    let (service, inputs, _) = f.start_lease(None, None);
    let db = f.external_id(&source(&inputs[0]));
    let mut request = service
        .prepare_source_lease_wal_with_binding(inputs[0].clone(), correlation(), deadline())
        .unwrap();
    let holder = held(&mut request);
    let witness = request.outcome_probe_for_tests();
    drop(holder);
    drop(request);
    let end = Instant::now() + Duration::from_secs(1);
    while witness.status() != CatalogSourceWalStatus::Aborted {
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(2));
    }
    db.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert_eq!(credits(&service), (0, 0));
    let mut second = service
        .prepare_source_lease_wal_with_binding(inputs[1].clone(), correlation(), deadline())
        .unwrap();
    let held_b = held(&mut second);
    service.shutdown();
    assert_eq!(second.status(), CatalogSourceWalStatus::Aborted);
    assert_eq!(credits(&service), (0, 0));
    drop(held_b);
}
