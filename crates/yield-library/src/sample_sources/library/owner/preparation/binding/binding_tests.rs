#[cfg(test)]
mod integration {
    //! Actual global worker/native scope tests; historical bytes never grant authority.
    use super::super::super::tests::{empty, external, fixture, release, request};
    use super::super::*;

    fn reserve_strict(
        writer: &GlobalLibraryWriter,
        duration: Duration,
    ) -> GlobalPreparationReservation {
        writer
            .submit_preparation(request(writer, duration).with_wal_binding())
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap()
    }
    fn charged(writer: &GlobalLibraryWriter) -> usize {
        *writer.preparation_binding_budget.0.lock().unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn original_global_native_mapping_deadline_during_capture_preserves_deadline_refusal() {
        static HIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        HIT.store(false, std::sync::atomic::Ordering::Release);
        fixture(|writer, guard, _| {
            let mut pending = writer
                .submit_preparation(
                    request(writer, Duration::from_millis(150))
                        .with_wal_binding()
                        .with_original_observation_expiry_probe(false, &HIT),
                )
                .unwrap();
            let outcome = pending.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(HIT.load(std::sync::atomic::Ordering::Acquire));
            assert_eq!(outcome.err(), Some(GlobalPreparationError::Deadline));
            empty(writer);
            assert_eq!(charged(writer), 0);
            external(guard)
                .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
                .unwrap();
            release(reserve_strict(writer, Duration::from_secs(1)));
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn original_global_native_mapping_deadline_while_parked_preserves_confirmed_cleanup() {
        static HIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        HIT.store(false, std::sync::atomic::Ordering::Release);
        fixture(|writer, guard, _| {
            let mut held = writer
                .submit_preparation(
                    request(writer, Duration::from_millis(150))
                        .with_wal_binding()
                        .with_original_observation_expiry_probe(true, &HIT),
                )
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            let observation = held.observation().clone();
            let outcome = held
                .released
                .take()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
            assert!(HIT.load(std::sync::atomic::Ordering::Acquire));
            assert_eq!(outcome, Err(GlobalPreparationError::Deadline));
            drop(held);
            external(guard)
                .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
                .unwrap();
            empty(writer);
            drop(observation);
            assert_eq!(charged(writer), 0);
            release(reserve_strict(writer, Duration::from_secs(1)));
        });
    }

    #[test]
    fn original_global_descriptor_seals_retained_database_root() {
        fixture(|writer, guard, _| {
            let held = reserve_strict(writer, Duration::from_secs(2));
            let view = held.observation().clone();
            let encoded: serde_json::Value =
                serde_json::from_slice(view.wal_scope().unwrap().descriptor_bytes()).unwrap();
            assert_eq!(encoded["root_identity"], guard.profile_root_identity());
            assert_eq!(
                view.wal_scope().unwrap().root_identity(),
                guard.profile_root_identity()
            );
            let outside = external(guard);
            assert_eq!(
                outside
                    .execute_batch("BEGIN IMMEDIATE")
                    .unwrap_err()
                    .sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy)
            );
            release(held);
            assert_eq!(charged(writer), HANDOFF_BYTES);
            drop(view);
            // Release acknowledges native cleanup before the worker drops its scope.
            // An ordinary command on that same owner also observes credit disposal.
            empty(writer);
            assert_eq!(charged(writer), 0);
            outside.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        });
    }

    #[test]
    fn original_global_descriptor_generation_changes_on_same_acquisition_worker_restart() {
        fixture(|writer, guard, _| {
            let held = reserve_strict(writer, Duration::from_secs(2));
            let first = held.observation().clone();
            let encoded: serde_json::Value =
                serde_json::from_slice(first.wal_scope().unwrap().descriptor_bytes()).unwrap();
            let generation = encoded["registration_generation"]
                .as_u64()
                .expect("actual issued generation");
            assert!(generation > 0 && generation <= i64::MAX as u64);
            assert_eq!(generation, first.owner().generation());
            release(held);
            assert!(writer.shutdown());
            *writer = GlobalLibraryWriter::start(guard).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while writer.status() != GlobalLibraryWriterStatus::Available {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            let second = reserve_strict(writer, Duration::from_secs(2));
            let encoded: serde_json::Value = serde_json::from_slice(
                second.observation().wal_scope().unwrap().descriptor_bytes(),
            )
            .unwrap();
            assert!(encoded["registration_generation"].as_u64().unwrap() > generation);
            assert_eq!(
                encoded["registration_generation"],
                second.observation().owner().generation()
            );
            assert_eq!(first.acquisition(), second.observation().acquisition());
            assert_ne!(
                first.owner().registration(),
                second.observation().owner().registration()
            );
            release(second);
            drop(first);
            empty(writer);
            assert_eq!(charged(writer), 0);
        });
    }

    #[test]
    fn dropped_queued_strict_requests_keep_credit_until_real_owner_disposal() {
        fixture(|writer, _, _| {
            let held = writer
                .submit_preparation(request(writer, Duration::from_secs(2)))
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            let mut queued = Vec::new();
            for _ in 0..GLOBAL_PREPARATION_MAX_BYTES / HANDOFF_BYTES {
                queued.push(
                    writer
                        .submit_preparation(
                            request(writer, Duration::from_secs(1)).with_wal_binding(),
                        )
                        .unwrap(),
                );
            }
            assert_eq!(charged(writer), GLOBAL_PREPARATION_MAX_BYTES);
            drop(queued);
            // The worker is still held: a caller drop cannot free retained queue payload.
            assert_eq!(charged(writer), GLOBAL_PREPARATION_MAX_BYTES);
            assert!(matches!(
                writer
                    .submit_preparation(request(writer, Duration::from_secs(1)).with_wal_binding()),
                Err(GlobalLibraryWriterQueueError::Full)
            ));
            release(held);
            empty(writer);
            assert_eq!(charged(writer), 0);
            release(reserve_strict(writer, Duration::from_secs(1)));
            empty(writer);
            assert_eq!(charged(writer), 0);
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn original_scope_shares_complete_reply_and_excludes_external_and_queued_writes() {
        fixture(|writer, guard, _| {
            let held = reserve_strict(writer, Duration::from_secs(2));
            let view = held.observation().clone();
            let copy = view.clone();
            assert!(Arc::ptr_eq(&view.data, &copy.data));
            let scope = view.wal_scope().unwrap();
            assert_eq!(scope.main().identity(), view.database_identity());
            assert_ne!(scope.main().identity(), scope.wal().identity());
            assert_ne!(scope.main().identity(), scope.shm().identity());
            assert_ne!(scope.wal().identity(), scope.shm().identity());
            for object in [scope.main(), scope.wal(), scope.shm()] {
                assert_eq!(object.device(), scope.main().device());
                assert_eq!(object.allocation_unit(), scope.main().allocation_unit());
                assert!(object.allocation_unit().is_power_of_two());
            }
            assert!(scope.shm().observed_bytes() >= 32768);
            let encoded: serde_json::Value =
                serde_json::from_slice(scope.descriptor_bytes()).unwrap();
            assert_eq!(encoded["version"], 1);
            assert_eq!(encoded["acquisition"], guard.owner_identity());
            assert_eq!(
                encoded["registration"],
                view.owner().registration().to_string()
            );
            assert_eq!(encoded["preparation_nonce"], view.nonce().to_string());
            assert_eq!(encoded["mutation_revision"], view.mutation_revision());
            assert_eq!(
                encoded["command_digest"],
                serde_json::to_value(vec![7; 32]).unwrap()
            );
            assert!(scope.descriptor_bytes().len() <= DESCRIPTOR_MAX_BYTES);
            let outside = external(guard);
            assert!(matches!(outside.execute_batch("BEGIN IMMEDIATE"),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::DatabaseBusy));
            let queued = writer
                .replace_ui_preferences_payload("after-release".into())
                .unwrap();
            assert!(matches!(queued.try_recv(), Err(mpsc::TryRecvError::Empty)));
            release(held);
            queued
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            assert_eq!(charged(writer), HANDOFF_BYTES);
            drop(view);
            assert_eq!(charged(writer), HANDOFF_BYTES);
            drop(copy);
            assert_eq!(charged(writer), 0);
            outside.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn historical_views_exhaust_local_bytes_until_last_copy_drops_even_after_shutdown() {
        fixture(|writer, _, _| {
            let mut views = Vec::new();
            for _ in 0..GLOBAL_PREPARATION_MAX_BYTES / HANDOFF_BYTES {
                let held = reserve_strict(writer, Duration::from_secs(1));
                let first = held.observation().clone();
                views.push(first.clone());
                release(held);
                drop(first);
            }
            assert_eq!(charged(writer), GLOBAL_PREPARATION_MAX_BYTES);
            assert!(matches!(
                writer
                    .submit_preparation(request(writer, Duration::from_secs(1)).with_wal_binding()),
                Err(GlobalLibraryWriterQueueError::Full)
            ));
            // The original diagnostics remain available; held views do not retain a SQL lock.
            empty(writer);
            let one = views.pop().unwrap();
            let copy = one.clone();
            drop(one);
            assert_eq!(charged(writer), GLOBAL_PREPARATION_MAX_BYTES);
            drop(copy);
            let replacement = reserve_strict(writer, Duration::from_secs(1));
            assert_eq!(charged(writer), GLOBAL_PREPARATION_MAX_BYTES);
            release(replacement);
            let budget = Arc::clone(&writer.preparation_binding_budget);
            writer.shutdown_with_report();
            assert_eq!(
                *budget.0.lock().unwrap(),
                GLOBAL_PREPARATION_MAX_BYTES - HANDOFF_BYTES
            );
            for view in &views {
                assert!(view.wal_scope().is_some());
            }
            drop(views);
            assert_eq!(*budget.0.lock().unwrap(), 0);
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strict_expiry_keeps_reply_credit_but_restores_original_writer_only_after_rollback() {
        fixture(|writer, guard, _| {
            let mut held = reserve_strict(writer, Duration::from_millis(150));
            let view = held.observation().clone();
            let release_rx = held.released.take().unwrap();
            assert_eq!(
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
                Err(GlobalPreparationError::Deadline)
            );
            drop(held);
            assert_eq!(charged(writer), HANDOFF_BYTES);
            external(guard)
                .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
                .unwrap();
            empty(writer);
            let next = reserve_strict(writer, Duration::from_secs(1));
            assert_ne!(view.nonce(), next.observation().nonce());
            release(next);
            // Release delivery precedes disposal of the worker's final observation.
            // A following actual command proves that custody has dropped.
            empty(writer);
            drop(view);
            assert_eq!(charged(writer), 0);
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strict_rejection_releases_queue_credit_and_preserves_original_rows() {
        fixture(|writer, guard, _| {
            let outside = external(guard);
            outside.execute_batch("BEGIN IMMEDIATE").unwrap();
            for _ in 0..6 {
                let result = writer
                    .submit_preparation(request(writer, Duration::from_secs(1)).with_wal_binding())
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
                assert_eq!(
                    result.err(),
                    Some(GlobalPreparationError::Acquisition {
                        sqlite_code: rusqlite::ffi::SQLITE_BUSY
                    })
                );
            }
            outside.execute_batch("ROLLBACK").unwrap();
            let held = writer
                .submit_preparation(
                    request(writer, Duration::from_secs(1))
                        .with_wal_binding()
                        .with_lifecycle_allocation(),
                )
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
            let facts = held.observation().lifecycle_allocation().unwrap();
            let scope = held.observation().wal_scope().unwrap();
            for (scope, facts) in [
                (scope.main(), facts.main()),
                (scope.wal(), facts.wal()),
                (scope.shm(), facts.shm()),
            ] {
                assert_eq!(scope.identity(), facts.identity());
                assert_eq!(scope.observed_bytes(), facts.observed_bytes());
            }
            release(held);
            // A following real worker command establishes that the previous command was disposed.
            empty(writer);
            assert_eq!(charged(writer), 0);
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn original_scope_rejects_same_size_sidecar_substitution_before_ordinary_dispatch() {
        for suffix in ["-wal", "-shm"] {
            fixture(|writer, guard, temp| {
                let mut held = reserve_strict(writer, Duration::from_secs(2));
                let retained = held.observation().clone();
                let released = held.released.take().unwrap();
                let queued = writer
                    .replace_ui_preferences_payload("must-not-run".into())
                    .unwrap();
                let path = guard.profile_root().join(format!(
                    "{}{}",
                    super::super::super::super::super::LIBRARY_DB_FILE_NAME,
                    suffix
                ));
                let bytes = std::fs::read(&path).unwrap();
                std::fs::rename(&path, temp.path().join("original-sidecar")).unwrap();
                std::fs::write(&path, bytes).unwrap();
                assert_eq!(
                    released.recv_timeout(Duration::from_secs(2)).unwrap(),
                    Err(GlobalPreparationError::DatabaseBinding)
                );
                assert!(
                    queued
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap()
                        .is_err()
                );
                writer.shutdown_with_report();
                assert_eq!(charged(writer), HANDOFF_BYTES);
                drop(held);
                drop(retained);
                assert_eq!(charged(writer), 0);
            });
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strict_capture_refuses_missing_sidecar_and_returns_no_scope() {
        for suffix in ["-wal", "-shm"] {
            fixture(|writer, guard, temp| {
                let path = guard.profile_root().join(format!(
                    "{}{}",
                    super::super::super::super::super::LIBRARY_DB_FILE_NAME,
                    suffix
                ));
                std::fs::rename(&path, temp.path().join("missing-sidecar")).unwrap();
                let result = writer
                    .submit_preparation(request(writer, Duration::from_secs(1)).with_wal_binding())
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
                assert_eq!(result.err(), Some(GlobalPreparationError::DatabaseBinding));
                writer.shutdown_with_report();
                assert_eq!(charged(writer), 0);
            });
        }
    }
}
