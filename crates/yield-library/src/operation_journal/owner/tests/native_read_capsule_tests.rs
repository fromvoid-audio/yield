#[cfg(test)]
mod integration {
    use super::super::*;
    use crate::sample_sources::db::file_ops_journal::BoundDatabaseRoot;
    use native_read_capsule::NativeReadError;
    use std::path::Path;

    fn runtime(name: &str) -> (Fixture, OwnerRuntime, Arc<OwnerShared>) {
        let fixture = fixture(name);
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let mut runtime = OwnerRuntime::new(store).unwrap();
        let (commands, _receivers) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(commands));
        runtime.attach_resource_custody(&shared).unwrap();
        assert!(shared.resource_admission.activate(|| {
            shared
                .lifecycle
                .store(OwnerLifecycle::Available as u8, Ordering::Release);
            true
        }));
        (fixture, runtime, shared)
    }
    #[test]
    fn native_reader_actual_store_missing_closed_and_unknown_refund_all_temporary_credit() {
        let (fixture, mut runtime, shared) = runtime("native-reader-refusal");
        let volume = VolumeIdentity { device: 123 };
        let before = shared.input_bytes.lock().unwrap().remaining[InputClass::Ordinary.index()];
        assert!(matches!(
            runtime.capture_native_read_capsule(&volume),
            Err(NativeReadError::Missing)
        ));
        assert_eq!(
            shared.input_bytes.lock().unwrap().remaining[InputClass::Ordinary.index()],
            before
        );
        assert!(runtime.native_readers.is_empty());
        assert!(runtime.native_reader_registry_input.is_none());
        assert!(OperationJournalStore::open(&fixture.owner).is_err());
        shared.resource_admission.close();
        assert!(matches!(
            runtime.capture_native_read_capsule(&volume),
            Err(NativeReadError::Closed)
        ));
        shared.resource_admission.invalidate();
        assert!(matches!(
            runtime.capture_native_read_capsule(&volume),
            Err(NativeReadError::Unknown)
        ));
        drop(runtime);
        assert_eq!(
            shared.input_bytes.lock().unwrap().remaining[InputClass::Ordinary.index()],
            InputBudget::shared_initial().remaining[InputClass::Ordinary.index()]
        );
        assert!(OperationJournalStore::open(&fixture.owner).is_ok());
    }
    #[test]
    fn native_reader_actual_input_exhaustion_refuses_before_native_capture() {
        let (_fixture, mut runtime, shared) = runtime("native-reader-input-full");
        let remaining = shared.input_bytes.lock().unwrap().remaining[InputClass::Ordinary.index()];
        let input = shared
            .reserve_input(InputClass::Ordinary, remaining)
            .unwrap();
        let volume = VolumeIdentity { device: 123 };
        assert!(matches!(
            runtime.capture_native_read_capsule(&volume),
            Err(NativeReadError::Full)
        ));
        assert!(runtime.native_readers.is_empty());
        drop(input);
        shared.resource_admission.close();
    }
    #[cfg(unix)]
    #[test]
    fn native_reader_actual_root_and_child_replacement_refuse_without_namespace_effect() {
        let fixture = fixture("native-reader-root");
        let root =
            BoundDatabaseRoot::for_profile_child(&fixture.owner, Path::new("operation_journal"))
                .unwrap();
        let child = Path::new("observation");
        let file = root.open_file_nofollow(child, true).unwrap();
        let identity =
            crate::filesystem_identity::stable_filesystem_identity_from_open_file(&file).unwrap();
        let reader = root.readonly_observation().unwrap();
        reader.verify_direct_child(child, &identity).unwrap();
        let path = fixture.owner.profile_root().join("operation_journal");
        fs::rename(path.join(child), path.join("parked")).unwrap();
        fs::write(path.join(child), b"replacement").unwrap();
        assert!(reader.verify_direct_child(child, &identity).is_err());
        fs::remove_file(path.join(child)).unwrap();
        std::os::unix::fs::symlink("parked", path.join(child)).unwrap();
        assert!(reader.verify_direct_child(child, &identity).is_err());
        fs::rename(&path, fixture.owner.profile_root().join("parked-root")).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(reader.validate().is_err());
        assert!(fs::read_dir(&path).unwrap().next().is_none());
    }
    #[test]
    fn native_reader_observation_descriptor_does_not_retain_actual_profile_child_authority() {
        let fixture = fixture("native-reader-child-lease");
        let root =
            BoundDatabaseRoot::for_profile_child(&fixture.owner, Path::new("operation_journal"))
                .unwrap();
        let reader = root.readonly_observation().unwrap();
        assert!(
            BoundDatabaseRoot::for_profile_child(&fixture.owner, Path::new("operation_journal"))
                .is_err()
        );
        drop(root);
        reader
            .validate()
            .expect("observation alias must remain bound to its own descriptor");
        let rebound =
            BoundDatabaseRoot::for_profile_child(&fixture.owner, Path::new("operation_journal"))
                .expect("readonly witness must not retain the original child lease");
        drop(rebound);
        let Fixture {
            owner,
            _profile,
            _config,
            _base,
            _runtime,
        } = fixture;
        drop(owner);
        let reacquired = WritableProfileGuard::acquire_current()
            .expect("observation does not retain the profile lock");
        drop(reader);
        drop(reacquired);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_reader_unsupported_volume_witness_preserves_original_owner_lock() {
        use std::os::unix::fs::MetadataExt;
        let fixture = fixture("native-reader-unsupported");
        let registry = fixture._base.path().join("readonly-registry");
        fs::create_dir(&registry).unwrap();
        let descriptor = fs::File::open(fixture.owner.profile_root()).unwrap();
        let volume = VolumeIdentity {
            device: descriptor.metadata().unwrap().dev(),
        };
        let owner = crate::operation_journal::capacity_volume_owner::acquire_fixture_for_test(
            &descriptor,
            &registry,
            volume.clone(),
            fixture.owner.profile_root_identity(),
        )
        .unwrap();
        assert!(matches!(owner.readonly_observation(), Err(crate::operation_journal::capacity_volume_owner::VolumeOwnershipError::Unsupported { .. })));
        assert!(
            crate::operation_journal::capacity_volume_owner::acquire_fixture_for_test(
                &descriptor,
                &registry,
                volume,
                fixture.owner.profile_root_identity()
            )
            .is_err()
        );
        owner.revalidate().unwrap();
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn native_reader_readonly_fifo_refuses_without_waiting_and_deep_root_is_bounded() {
        use std::os::unix::ffi::OsStrExt;
        let fixture = fixture("native-reader-bounds");
        let root =
            BoundDatabaseRoot::for_profile_child(&fixture.owner, Path::new("operation_journal"))
                .unwrap();
        let reader = root.readonly_observation().unwrap();
        let fifo_path = fixture.owner.profile_root().join("operation_journal/fifo");
        let fifo = std::ffi::CString::new(fifo_path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(
            reader
                .verify_direct_child(Path::new("fifo"), "never-native")
                .is_err()
        );
        let mut deep = fixture._base.path().join("deep");
        for _ in 0..65 {
            deep.push("d");
        }
        fs::create_dir_all(&deep).unwrap();
        let root = BoundDatabaseRoot::for_external_source_root(&fixture.owner, &deep).unwrap();
        assert!(root.readonly_observation().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn native_reader_original_profile_lock_replacement_refuses_same_root_and_bytes() {
        use crate::operation_journal::capacity_reserve_namespace::ReadonlyProfileScope;
        let fixture = fixture("native-reader-profile-lock");
        let scope = ReadonlyProfileScope::capture(&fixture.owner).unwrap();
        scope.validate().unwrap();
        let lock = fixture
            .owner
            .profile_root()
            .join(crate::app_dirs::PROFILE_OWNER_LOCK_FILE_NAME);
        let bytes = fs::read(&lock).unwrap();
        fs::rename(&lock, fixture.owner.profile_root().join("parked-lock")).unwrap();
        fs::write(&lock, bytes).unwrap();
        assert!(scope.validate().is_err());
        assert!(fixture.owner.validate_current().is_err());
    }
    #[cfg(unix)]
    #[test]
    fn native_reader_original_profile_replacement_refuses_transplanted_unchanged_journal_root() {
        use crate::operation_journal::capacity_reserve_namespace::ReadonlyProfileScope;
        let fixture = fixture("native-reader-profile-parent");
        let scope = ReadonlyProfileScope::capture(&fixture.owner).unwrap();
        let root =
            BoundDatabaseRoot::for_profile_child(&fixture.owner, Path::new("operation_journal"))
                .unwrap();
        let child = root.readonly_observation().unwrap();
        let profile = fixture.owner.profile_root().to_path_buf();
        let parked = profile.with_extension("parked");
        fs::rename(&profile, &parked).unwrap();
        fs::create_dir(&profile).unwrap();
        fs::rename(
            parked.join("operation_journal"),
            profile.join("operation_journal"),
        )
        .unwrap();
        child
            .validate()
            .expect("unchanged actual journal inode is insufficient by itself");
        assert!(scope.validate().is_err());
        assert!(fixture.owner.validate_current().is_err());
    }
    #[test]
    fn native_reader_readonly_profile_scope_does_not_retain_actual_profile_lock() {
        use crate::operation_journal::capacity_reserve_namespace::ReadonlyProfileScope;
        let fixture = fixture("native-reader-profile-no-authority");
        let scope = ReadonlyProfileScope::capture(&fixture.owner).unwrap();
        let Fixture {
            owner,
            _profile,
            _config,
            _base,
            _runtime,
        } = fixture;
        drop(owner);
        let reacquired = WritableProfileGuard::acquire_current()
            .expect("readonly parent and lock observations retain no OS lock");
        drop(scope);
        drop(reacquired);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn native_reader_genuine_uuid_reserve_original_duplicate_floor_and_replacement_harness() {
        use std::os::unix::fs::MetadataExt;
        for failure in ["none", "reserve", "floor", "root", "marker", "owner"] {
            let (fixture, mut runtime, shared) = runtime(&format!("native-reader-{failure}"));
            let registry = fixture_volume_registry(&fixture);
            let root_path = fixture.owner.profile_root().join("operation_journal");
            let root_descriptor = fs::File::open(&root_path).unwrap();
            let owner =
                crate::operation_journal::capacity_volume_owner::acquire_native_reader_fixture_test(
                    &root_descriptor,
                    &registry,
                    &fixture.owner,
                )
                .expect("actual native UUID and original volume ownership");
            let volume = owner.volume().clone();
            runtime.install_volume_owner(owner);
            let reserve = runtime
                .store()
                .bootstrap_profile_journal_reserve(runtime.volume_owner(), Some(&registry))
                .expect("actual protected reserve and full physical floor");
            runtime.install_reserve(reserve);
            let before = shared.input_bytes.lock().unwrap().remaining[InputClass::Ordinary.index()];
            let capsule = runtime
                .capture_native_read_capsule(&volume)
                .expect("genuine capture");
            let duplicate = runtime
                .capture_native_read_capsule(&volume)
                .expect("fresh native duplicate proof");
            assert!(capsule.same_original(&duplicate));
            assert_eq!(runtime.native_readers.len(), 1);
            assert_eq!(
                shared.input_bytes.lock().unwrap().remaining[InputClass::Ordinary.index()],
                before - 64 * 1024 - 32 * 1024
            );
            let peer = ResourceAdmission::new();
            assert!(peer.activate(|| true));
            let mut peer_use = peer.begin_read(|| true).unwrap();
            assert!(matches!(
                capsule.read(&mut peer_use),
                Err(NativeReadError::Unknown)
            ));
            drop(peer_use);
            let mut read = shared.resource_admission.begin_read(|| true).unwrap();
            let facts = capsule
                .read(&mut read)
                .expect("fresh actual native descriptor facts");
            assert_eq!(facts.facts().identity, volume);
            assert!(facts.matches(&capsule, &read));
            assert!(matches!(
                capsule.read(&mut read),
                Err(NativeReadError::Closed)
            ));
            drop(read);
            if failure == "none" {
                let mut retained_facts = Vec::new();
                for _ in 1..32 {
                    let mut read = shared.resource_admission.begin_read(|| true).unwrap();
                    retained_facts.push(capsule.read(&mut read).unwrap());
                    drop(read);
                }
                let mut read = shared.resource_admission.begin_read(|| true).unwrap();
                assert!(matches!(
                    capsule.read(&mut read),
                    Err(NativeReadError::Full)
                ));
                drop(retained_facts.pop());
                let recovered = capsule
                    .read(&mut read)
                    .expect("header full refusal did not consume unique read");
                drop(read);
                drop(recovered);
                drop(retained_facts);
            }
            let reserve_path = root_path
                .join(crate::operation_journal::capacity_reserve_namespace::RESERVE_FILE_NAME);
            match failure {
                "owner" => {
                    let original = runtime.volume_owner.take().unwrap();
                    let generation = original.owner_generation();
                    drop(original);
                    let replacement = crate::operation_journal::capacity_volume_owner::acquire_native_reader_fixture_test(
                        &root_descriptor, &registry, &fixture.owner).unwrap();
                    // Dirty same-profile restart deliberately retains marker generation.
                    assert_eq!(replacement.owner_generation(), generation);
                    runtime.install_volume_owner(replacement);
                    assert!(matches!(
                        runtime.capture_native_read_capsule(&volume),
                        Err(NativeReadError::Conflict)
                    ));
                }
                "reserve" => {
                    fs::rename(&reserve_path, root_path.join("parked-reserve")).unwrap();
                    fs::write(&reserve_path, b"replacement").unwrap();
                }
                "floor" => {
                    fs::OpenOptions::new()
                        .write(true)
                        .open(&reserve_path)
                        .unwrap()
                        .set_len(0)
                        .unwrap();
                }
                "root" => {
                    fs::rename(&root_path, fixture.owner.profile_root().join("parked-root"))
                        .unwrap();
                    fs::create_dir(&root_path).unwrap();
                }
                "marker" => {
                    let marker = fs::read_dir(&registry)
                        .unwrap()
                        .map(|e| e.unwrap().path())
                        .find(|p| p.is_file())
                        .unwrap();
                    fs::OpenOptions::new()
                        .write(true)
                        .open(marker)
                        .unwrap()
                        .set_len(0)
                        .unwrap();
                }
                _ => {}
            }
            if failure != "none" {
                let mut read = shared.resource_admission.begin_read(|| true).unwrap();
                assert!(matches!(
                    capsule.read(&mut read),
                    Err(NativeReadError::Unknown)
                ));
                drop(read);
                assert!(shared.resource_admission.validate_open().is_err());
                assert!(runtime.capture_native_read_capsule(&volume).is_err());
            } else {
                assert_eq!(fs::metadata(&reserve_path).unwrap().nlink(), 1);
                let mut peer_use = peer.begin_read(|| true).unwrap();
                assert!(!facts.matches(&capsule, &peer_use));
                assert!(matches!(
                    capsule.read(&mut peer_use),
                    Err(NativeReadError::Unknown)
                ));
                drop(peer_use);
            }
            drop(facts);
            drop(duplicate);
            drop(capsule);
            assert_eq!(
                shared.input_bytes.lock().unwrap().remaining[InputClass::Ordinary.index()],
                before - 32 * 1024
            );
            if failure == "none" {
                let retained = runtime
                    .capture_native_read_capsule(&volume)
                    .expect("reuse only expired weak entry after fresh native proof");
                assert_eq!(runtime.native_readers.len(), 1);
                shared.resource_admission.close();
                assert!(matches!(
                    runtime.capture_native_read_capsule(&volume),
                    Err(NativeReadError::Closed)
                ));
                drop(runtime);
                assert!(shared.resource_admission.begin_read(|| true).is_err());
                // Retained observations keep their memory debit, but no journal child lease.
                let reopened = OperationJournalStore::open(&fixture.owner)
                    .expect("original child lease released despite queued observer");
                drop(reopened);
                assert_eq!(
                    shared.input_bytes.lock().unwrap().remaining[InputClass::Ordinary.index()],
                    InputBudget::shared_initial().remaining[InputClass::Ordinary.index()]
                        - 64 * 1024
                );
                drop(retained);
            } else {
                drop(runtime);
            }
            assert_eq!(
                shared.input_bytes.lock().unwrap().remaining[InputClass::Ordinary.index()],
                InputBudget::shared_initial().remaining[InputClass::Ordinary.index()]
            );
        }
    }
}
