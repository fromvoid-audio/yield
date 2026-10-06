#[cfg(test)]
mod integration {
    //! Actual original owner/store lifetime tests. Counted test uses grant no Native authority.
    use super::super::*;
    use resource_custody::{HEADER_BYTES, UseError};
    use std::time::Instant;
    fn wait_closed(admission: &ResourceAdmission) {
        let end = Instant::now() + Duration::from_secs(2);
        while !admission.snapshot().0 {
            assert!(
                Instant::now() < end,
                "original admission must close before teardown"
            );
            thread::yield_now();
        }
    }
    #[test]
    fn resource_custody_actual_shutdown_retains_original_store_and_header_until_read_release() {
        let (fixture, mut owner) = open_owner("resource-custody-shutdown");
        let shared = owner.shared.clone();
        let admission = shared.resource_admission.clone();
        assert_eq!(
            shared.input_bytes.lock().unwrap().remaining[InputClass::Ordinary.index()],
            InputBudget::shared_initial().remaining[InputClass::Ordinary.index()] - HEADER_BYTES
        );
        thread::scope(|scope| {
            let read = admission.begin_read(|| true).unwrap();
            let (tx, rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                tx.send(owner.shutdown()).unwrap();
            });
            wait_closed(&admission);
            assert!(matches!(
                admission.begin_read(|| true),
                Err(UseError::Closed)
            ));
            assert!(rx.try_recv().is_err());
            assert!(OperationJournalStore::open(&fixture.owner).is_err());
            assert_eq!(
                shared.input_bytes.lock().unwrap().remaining[InputClass::Ordinary.index()],
                InputBudget::shared_initial().remaining[InputClass::Ordinary.index()]
                    - HEADER_BYTES
            );
            drop(read);
            assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap());
        });
        assert!(OperationJournalStore::open(&fixture.owner).is_ok());
        assert_eq!(
            shared.input_bytes.lock().unwrap().remaining[InputClass::Ordinary.index()],
            InputBudget::shared_initial().remaining[InputClass::Ordinary.index()]
        );
    }
    #[test]
    fn resource_custody_actual_entered_use_quiesces_without_false_shutdown_or_settlement() {
        let (fixture, mut owner) = open_owner("resource-custody-entered");
        let admission = owner.shared.resource_admission.clone();
        thread::scope(|scope| {
            let mut read = admission.begin_read(|| true).unwrap();
            read.enter_effect(|| true).unwrap();
            assert_eq!(admission.snapshot().1, 1);
            let (tx, rx) = mpsc::sync_channel(1);
            scope.spawn(move || tx.send(owner.shutdown()).unwrap());
            wait_closed(&admission);
            assert!(OperationJournalStore::open(&fixture.owner).is_err());
            assert!(rx.try_recv().is_err());
            drop(read);
            assert!(!rx.recv_timeout(Duration::from_secs(2)).unwrap());
        });
        assert!(admission.snapshot().2);
        assert!(OperationJournalStore::open(&fixture.owner).is_ok());
    }
    #[test]
    fn resource_custody_actual_unavailable_linearizes_after_original_admission_transition() {
        let (fixture, mut owner) = open_owner("resource-custody-unavailable-race");
        let shared = owner.shared.clone();
        let admission = shared.resource_admission.clone();
        let ready = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        struct ReleaseOnDrop(Arc<std::sync::Barrier>, bool);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                if !self.1 {
                    self.0.wait();
                }
            }
        }
        thread::scope(|scope| {
            let (use_tx, use_rx) = mpsc::sync_channel(1);
            let native_admission = admission.clone();
            let worker_ready = ready.clone();
            let worker_release = release.clone();
            scope.spawn(move || {
                // Deterministic scalar slot barrier only: no native call or queue wait in
                // production admission callbacks, and this use grants no effect authority.
                use_tx
                    .send(
                        native_admission
                            .begin_read(|| {
                                worker_ready.wait();
                                worker_release.wait();
                                true
                            })
                            .unwrap(),
                    )
                    .unwrap();
            });
            ready.wait();
            let mut barrier = ReleaseOnDrop(release, false);
            let (close_tx, close_rx) = mpsc::sync_channel(1);
            let closing = shared.clone();
            scope.spawn(move || {
                closing.mark_unavailable("genuine external unavailable boundary");
                close_tx.send(()).unwrap();
            });
            assert!(matches!(
                owner.status(),
                OperationJournalOwnerStatus::Available
            ));
            assert!(close_rx.try_recv().is_err());
            barrier.1 = true;
            barrier.0.wait();
            let read = use_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            close_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(matches!(
                owner.status(),
                OperationJournalOwnerStatus::Unavailable { .. }
            ));
            assert!(matches!(
                admission.begin_read(|| true),
                Err(UseError::Unknown)
            ));
            assert_eq!(admission.snapshot().1, 1);
            assert!(OperationJournalStore::open(&fixture.owner).is_err());
            let (done_tx, done_rx) = mpsc::sync_channel(1);
            scope.spawn(move || done_tx.send(owner.shutdown()).unwrap());
            assert!(done_rx.try_recv().is_err());
            drop(read);
            assert!(!done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        });
        assert!(!admission.close_and_wait());
        assert!(OperationJournalStore::open(&fixture.owner).is_ok());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn resource_custody_genuine_native_reserve_volume_and_profile_survive_wait_and_unwind() {
        for unwind in [false, true] {
            let (fixture, mut runtime, registry_path, _reserve_path, descriptor, volume) =
                handoff_runtime(if unwind {
                    "resource-native-unwind"
                } else {
                    "resource-native-drop"
                });
            let Fixture {
                owner: profile,
                _profile,
                _config,
                _base,
                _runtime,
            } = fixture;
            let identity = profile.profile_root_identity().to_owned();
            let probe = OwnerRuntimeDropProbe {
                descriptor: descriptor.try_clone().unwrap(),
                registry_path: registry_path.clone(),
                volume: volume.clone(),
                profile_identity: identity.clone(),
            };
            let observer = Arc::new(Mutex::new(None));
            runtime.install_drop_observer(observer.clone(), probe);
            let admission = runtime.resource_admission.clone();
            assert!(admission.activate(|| true));
            let read = admission.begin_read(|| true).unwrap();
            drop(profile);
            thread::scope(|scope| {
                let observing = admission.clone();
                let helper = scope.spawn(move || {
                    wait_closed(&observing);
                    let probe = OwnerRuntimeDropProbe {
                        descriptor,
                        registry_path,
                        volume,
                        profile_identity: identity,
                    };
                    let held = (
                        OwnerRuntimeDropProbe::profile_is_held(),
                        probe.volume_is_held(),
                    );
                    drop(read);
                    held
                });
                if unwind {
                    assert!(
                        panic::catch_unwind(AssertUnwindSafe(move || {
                            let _runtime = runtime;
                            panic!("actual native runtime unwind with original counted use");
                        }))
                        .is_err()
                    );
                } else {
                    drop(runtime);
                }
                assert_eq!(helper.join().unwrap(), (true, true));
            });
            let observed = observer.lock().unwrap().clone().unwrap();
            assert!(observed.profile_held_after_store_drop);
            assert!(observed.volume_held_after_store_drop);
            assert!(observed.profile_available_after_teardown);
            assert!(observed.volume_available_after_teardown);
        }
    }
    #[test]
    fn resource_custody_actual_command_and_outer_panic_retain_store_until_native_use_drop() {
        for outer in [false, true] {
            let fixture = fixture(if outer {
                "resource-custody-outer-panic"
            } else {
                "resource-custody-command-panic"
            });
            let store = OperationJournalStore::open(&fixture.owner).unwrap();
            if !outer {
                store.set_insert_test_hook(|_| panic!("actual store command panic"));
            }
            let (mut owner, ready, release) = if outer {
                OperationJournalOwner::start_with_worker_panic_for_test(store)
            } else {
                OperationJournalOwner::start_with_command_gate_for_test(store)
            };
            wait_until_available(&owner);
            let client = owner.client();
            let admission = owner.shared.resource_admission.clone();
            thread::scope(|scope| {
                let read = admission.begin_read(|| true).unwrap();
                let mut request = client
                    .persist_intent(intent_record(OperationId::new()))
                    .unwrap();
                ready.wait();
                release.wait();
                wait_closed(&admission);
                assert!(OperationJournalStore::open(&fixture.owner).is_err());
                let end = Instant::now() + Duration::from_secs(2);
                while !matches!(
                    client.status(),
                    OperationJournalOwnerStatus::Unavailable { .. }
                ) {
                    assert!(
                        Instant::now() < end,
                        "unavailable publication follows native closure"
                    );
                    thread::yield_now();
                }
                let (tx, rx) = mpsc::sync_channel(1);
                scope.spawn(move || tx.send(owner.shutdown()).unwrap());
                assert!(rx.try_recv().is_err());
                drop(read);
                assert!(!rx.recv_timeout(Duration::from_secs(2)).unwrap());
                assert!(request.recv_timeout(Duration::from_secs(2)).is_err() || !outer);
            });
            assert!(OperationJournalStore::open(&fixture.owner).is_ok());
        }
    }
}
