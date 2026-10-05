//! Genuine worker/SQLite tests. Empty reservations and historical scope issue no physical grant.
#[cfg(target_os = "linux")]
use super::super::tests::{empty, external};
use super::super::tests::{fixture, release, request};
use super::*;

fn prepare(client: &GlobalWalPreparationClient, strict: bool) -> GlobalPreparationReservation {
    let request = GlobalPreparationRequest::new(
        client.preparation_owner(),
        [71; 32],
        Instant::now() + Duration::from_secs(2),
        1,
        4096,
    )
    .unwrap();
    let request = if strict {
        request.with_wal_binding()
    } else {
        request
    };
    client
        .submit_preparation(request)
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap()
}

#[cfg(target_os = "linux")]
#[test]
fn global_requester_actual_original_scope_and_writer_exclusion_through_limited_clone() {
    fixture(|writer, guard, _| {
        let client = writer.wal_preparation_client();
        let clone = client.clone();
        assert_eq!(client.preparation_owner(), writer.preparation_owner());
        let held = prepare(&clone, true);
        let view = held.observation();
        assert_eq!(view.owner(), &writer.preparation_owner());
        assert_eq!(view.acquisition(), &writer.acquisition_id());
        assert_eq!(view.command_digest(), &[71; 32]);
        assert!(!view.nonce().is_nil());
        let scope = view.wal_scope().unwrap();
        assert_eq!(scope.main().identity(), view.database_identity());
        assert_ne!(scope.main().identity(), scope.wal().identity());
        assert_ne!(scope.main().identity(), scope.shm().identity());
        let outside = external(guard);
        assert!(matches!(outside.execute_batch("BEGIN IMMEDIATE"),
            Err(rusqlite::Error::SqliteFailure(e,_)) if e.code == rusqlite::ErrorCode::DatabaseBusy));
        let queued = writer
            .replace_ui_preferences_payload("after-real-rollback".into())
            .unwrap();
        assert!(matches!(queued.try_recv(), Err(mpsc::TryRecvError::Empty)));
        release(held);
        queued
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        outside.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    });
}

#[cfg(target_os = "linux")]
#[test]
fn global_requester_clones_and_writer_share_original_strict_credit_across_historical_copies() {
    fixture(|writer, _, _| {
        let client = writer.wal_preparation_client();
        let clone = client.clone();
        let mut views = Vec::new();
        for index in 0..4 {
            let held = if index == 0 {
                writer
                    .submit_preparation(request(writer, Duration::from_secs(2)).with_wal_binding())
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap()
            } else {
                prepare(if index % 2 == 0 { &client } else { &clone }, true)
            };
            views.push(held.observation().clone());
            release(held);
        }
        let retained_copy = views[0].clone();
        for requester in [&client, &clone] {
            assert!(matches!(
                requester
                    .submit_preparation(request(writer, Duration::from_secs(1)).with_wal_binding()),
                Err(GlobalLibraryWriterQueueError::Full)
            ));
        }
        assert!(matches!(
            writer.submit_preparation(request(writer, Duration::from_secs(1)).with_wal_binding()),
            Err(GlobalLibraryWriterQueueError::Full)
        ));
        drop(views.remove(0));
        assert!(matches!(
            clone.submit_preparation(request(writer, Duration::from_secs(1)).with_wal_binding()),
            Err(GlobalLibraryWriterQueueError::Full)
        ));
        drop(retained_copy);
        release(prepare(&clone, true));
        drop(views);
        empty(writer);
    });
}

#[test]
fn global_requester_shutdown_disconnects_all_clones_and_restart_rejects_original_registration() {
    fixture(|writer, guard, _| {
        let client = writer.wal_preparation_client();
        let clone = client.clone();
        let old = client.preparation_owner();
        let held = prepare(&client, false);
        assert!(writer.shutdown()); // Actual owner shutdown aborts held empty reservation.
        drop(held);
        for requester in [&client, &clone] {
            assert_eq!(requester.status(), GlobalLibraryWriterStatus::Closed);
            assert!(matches!(
                requester.submit_preparation(
                    GlobalPreparationRequest::new(
                        old.clone(),
                        [71; 32],
                        Instant::now() + Duration::from_secs(1),
                        1,
                        128
                    )
                    .unwrap()
                ),
                Err(GlobalLibraryWriterQueueError::Closed)
            ));
        }
        let mut replacement = GlobalLibraryWriter::start(guard).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while replacement.status() != GlobalLibraryWriterStatus::Available {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let current = replacement.wal_preparation_client();
        assert_eq!(old.acquisition(), current.preparation_owner().acquisition());
        assert_ne!(
            old.registration(),
            current.preparation_owner().registration()
        );
        let stale = GlobalPreparationRequest::new(
            old,
            [71; 32],
            Instant::now() + Duration::from_secs(1),
            1,
            128,
        )
        .unwrap();
        assert_eq!(
            current
                .submit_preparation(stale)
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .err(),
            Some(GlobalPreparationError::StaleOwner)
        );
        release(prepare(&current, false));
        replacement.shutdown();
    });
}

#[cfg(target_os = "linux")]
#[test]
fn global_requester_queue_failure_dropped_pending_and_deadline_preserve_real_owner() {
    fixture(|writer, guard, _| {
        let client = writer.wal_preparation_client();
        let held = prepare(&client, false);
        let pending = client
            .submit_preparation(request(writer, Duration::from_secs(1)).with_wal_binding())
            .unwrap();
        drop(pending); // Queued command still belongs to original owner; no caller cleanup ack.
        let mut queued = Vec::new();
        for _ in 1..GLOBAL_LIBRARY_WRITER_COMMAND_CAPACITY {
            queued.push(writer.load_ui_preferences_payload().unwrap());
        }
        for _ in 0..5 {
            assert!(matches!(
                client
                    .submit_preparation(request(writer, Duration::from_secs(1)).with_wal_binding()),
                Err(GlobalLibraryWriterQueueError::Full)
            ));
        }
        let outside = external(guard);
        assert!(outside.execute_batch("BEGIN IMMEDIATE").is_err());
        release(held);
        for reply in queued {
            reply.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
        }
        empty(writer);
        let mut views = Vec::new();
        for _ in 0..4 {
            let held = prepare(&client, true);
            views.push(held.observation().clone());
            release(held);
        }
        drop(views);
        // Original deadline can expire while blocked in the real ordinary queue.
        let held = prepare(&client, false);
        let mut pending = client
            .submit_preparation(request(writer, Duration::from_millis(40)))
            .unwrap();
        thread::sleep(Duration::from_millis(60));
        release(held);
        assert_eq!(
            pending.recv_timeout(Duration::from_secs(3)).unwrap().err(),
            Some(GlobalPreparationError::Deadline)
        );
        release(prepare(&client, false));
        empty(writer);
    });
}

#[test]
fn global_requester_actual_worker_panic_retains_typed_failure_after_shared_close() {
    fixture(|writer, _, _| {
        let client = writer.wal_preparation_client();
        let reply = writer.panic_owner_for_test().unwrap();
        let _ = reply.recv_timeout(Duration::from_secs(3));
        let deadline = Instant::now() + Duration::from_secs(3);
        while client.status() != GlobalLibraryWriterStatus::Closed {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert_eq!(
            writer.unavailable_reason(),
            Some(GlobalLibraryWriterUnavailable::WorkerPanicked)
        );
        assert!(matches!(
            client.submit_preparation(request(writer, Duration::from_secs(1))),
            Err(GlobalLibraryWriterQueueError::Closed)
        ));
        let report = writer.shutdown_with_report();
        assert_eq!(
            report.outcome(),
            &GlobalLibraryWriterDrainOutcome::WorkerPanicked
        );
        assert_eq!(client.status(), GlobalLibraryWriterStatus::Closed);
        assert!(matches!(
            client.submit_preparation(request(writer, Duration::from_secs(1))),
            Err(GlobalLibraryWriterQueueError::Closed)
        ));
    });
}
