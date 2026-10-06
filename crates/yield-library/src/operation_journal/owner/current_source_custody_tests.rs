//! Real worker recovery custody with a disposable app override and genuine Current source seal.
//! No fixture can make the physical provider available or publish a usable owner/reserve.
use super::*;
use std::{os::fd::AsRawFd, os::unix::fs::MetadataExt, path::Path, sync::Weak};

pub(super) struct Snapshot {
    fd: i32,
    device: u64,
    inode: u64,
    length: u64,
    root_device: u64,
    root_inode: u64,
    credit: Weak<InputPermit>,
    drops: Arc<Mutex<DropObservation>>,
}

#[derive(Default)]
struct DropObservation {
    native_before_root: bool,
    root_before_credit: bool,
    root_count: usize,
}
pub(super) struct BeforeRootDrop {
    native_fd: i32,
    native_device: u64,
    native_inode: u64,
    root: Weak<std::fs::File>,
    credit: Weak<InputPermit>,
    observations: Arc<Mutex<DropObservation>>,
}
impl Drop for BeforeRootDrop {
    fn drop(&mut self) {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let same = unsafe { libc::fstat(self.native_fd, stat.as_mut_ptr()) } == 0 && {
            let stat = unsafe { stat.assume_init() };
            stat.st_dev as u64 == self.native_device && stat.st_ino as u64 == self.native_inode
        };
        let count = self.root.strong_count();
        let mut observation = self.observations.lock().unwrap();
        observation.native_before_root = !same && count > 0 && self.credit.strong_count() > 0;
        observation.root_count = count;
    }
}
pub(super) struct AfterRootDrop {
    root: Weak<std::fs::File>,
    credit: Weak<InputPermit>,
    observations: Arc<Mutex<DropObservation>>,
}
impl Drop for AfterRootDrop {
    fn drop(&mut self) {
        let mut observation = self.observations.lock().unwrap();
        observation.root_before_credit = self.root.strong_count() + 1 == observation.root_count
            && self.credit.strong_count() > 0;
    }
}

impl OwnerRuntime {
    pub(super) fn current_refused_prefix_for_test(
        &mut self,
        credit: Arc<InputPermit>,
        envelope: SourceAuthorityRegistrationRequest,
        app: &Path,
        panic_after_retention: bool,
        probe: Option<Arc<Mutex<Option<Snapshot>>>>,
    ) -> Result<SourceAuthorityRegistrationReceipt, OperationJournalOwnerError> {
        let request = envelope.request;
        let profile = self
            .final_profile_guard
            .as_ref()
            .unwrap()
            .try_clone()
            .unwrap();
        request.validate_consuming_boundary(&profile)?;
        let proof = request.accept_confirmation(envelope.confirmation)?;
        let root = request.media_root();
        let scope = request.reserve_use(root, &proof, &profile)?;
        let volume = descriptor_capacity_facts(scope.descriptor())
            .unwrap()
            .identity;
        let id = request.registration_id();
        let receipt = SourceAuthorityRegistrationReceipt::new(
            id,
            request.lease_identity().clone(),
            vec![volume.clone()],
        );
        // Actual original actor structs, ancestors and credit are installed BEFORE native effects.
        self.source_volumes.insert(
            volume.clone(),
            SourceVolumeBinding {
                reserve: None,
                owner: None,
                _current_volume_failure: Default::default(),
                _current_reserve_failure: Default::default(),
                _current_before_root_drop: None,
                _current_root: Some(root.try_clone_for_owned_capability().unwrap()),
                _current_after_root_drop: None,
                allocation_unit: None,
                readiness: SourceVolumeBindingReadiness::Incomplete,
                registrations: BTreeSet::from([id]),
                _current_input: Some(Arc::clone(&credit)),
            },
        );
        self.source_registrations.insert(id, SourceAuthorityRecord {
            metadata_registration: crate::sample_sources::db::source_registry_handoff::MetadataRegistrationCustody::new(),
            metadata_writer: None, request: Arc::clone(&request),receipt,
            drain_marker: None, readiness: SourceRegistrationReadiness::Registering,
            _current_input: Some(credit),
        });
        assert!(
            super::super::capacity_volume_owner::acquire_current_refused_prefix_for_test(
                &scope,
                &mut self
                    .source_volumes
                    .get_mut(&volume)
                    .unwrap()
                    ._current_volume_failure,
                app,
            )
            .is_err()
        );
        let binding = self.source_volumes.get_mut(&volume).unwrap();
        let file = binding
            ._current_volume_failure
            .held_lock_for_tests()
            .unwrap();
        let metadata = file.metadata().unwrap();
        let root = binding
            ._current_root
            .as_ref()
            .unwrap()
            .current_drop_root_for_test();
        let credit = Arc::downgrade(binding._current_input.as_ref().unwrap());
        let observations = Arc::new(Mutex::new(DropObservation::default()));
        binding._current_before_root_drop = Some(BeforeRootDrop {
            native_fd: file.as_raw_fd(),
            native_device: metadata.dev(),
            native_inode: metadata.ino(),
            root: root.clone(),
            credit: credit.clone(),
            observations: Arc::clone(&observations),
        });
        binding._current_after_root_drop = Some(AfterRootDrop {
            root,
            credit,
            observations,
        });
        let result = self.retain_source_recovery(
            id,
            "current-native-refused-prefix",
            "exact dirty backing retained".into(),
            Some(proof),
        );
        if let Some(probe) = probe {
            let snapshot = self.inspect_current_refused_prefix(&volume).unwrap();
            *probe.lock().unwrap() = Some(snapshot);
        }
        if panic_after_retention {
            panic!("Current custody panic after actual native prefix retention");
        }
        result
    }
    pub(super) fn inspect_current_refused_prefix(
        &self,
        volume: &VolumeIdentity,
    ) -> Result<Snapshot, OperationJournalOwnerError> {
        let binding = self.source_volumes.get(volume).unwrap();
        assert_eq!(binding.readiness, SourceVolumeBindingReadiness::Incomplete);
        assert!(binding.owner.is_none() && binding.reserve.is_none());
        assert!(self.capacity_provider.is_none());
        let file = binding
            ._current_volume_failure
            .held_lock_for_tests()
            .unwrap();
        let metadata = file.metadata().unwrap();
        let root = binding
            ._current_root
            .as_ref()
            .unwrap()
            .try_clone_descriptor()
            .unwrap()
            .metadata()
            .unwrap();
        Ok(Snapshot {
            fd: file.as_raw_fd(),
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            root_device: root.dev(),
            root_inode: root.ino(),
            credit: Arc::downgrade(binding._current_input.as_ref().unwrap()),
            drops: Arc::clone(
                &binding
                    ._current_before_root_drop
                    .as_ref()
                    .unwrap()
                    .observations,
            ),
        })
    }
}
fn inspect(client: &OperationJournalClient, volume: VolumeIdentity) -> Snapshot {
    let mut reply = client
        .submit(256, move |input, result| {
            JournalCommand::InspectCurrentRefusedPrefix {
                volume,
                input,
                result,
            }
        })
        .unwrap();
    let snapshot = reply
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    let mut barrier = client
        .submit_control(CONTROL_INPUT_BYTES, |input, result| JournalCommand::Get {
            operation_id: OperationId::new(),
            input,
            result,
        })
        .unwrap();
    assert!(
        barrier
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap()
            .is_none()
    );
    snapshot
}
fn same_open_object(snapshot: &Snapshot) -> bool {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(snapshot.fd, metadata.as_mut_ptr()) } != 0 {
        return false;
    }
    let metadata = unsafe { metadata.assume_init() };
    metadata.st_dev as u64 == snapshot.device && metadata.st_ino as u64 == snapshot.inode
}

#[test]
fn current_original_worker_lost_reply_replay_and_shutdown_retain_dirty_backing_and_debit() {
    let app = tempfile::tempdir().unwrap();
    let _app = crate::app_dirs::AppRootGuard::set(app.path().to_owned()).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let media = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let source = crate::sample_sources::SourceDatabase::open_for_source_write_with_profile_guard(
        media.path(),
        &profile,
        db.path(),
    )
    .unwrap();
    let authority = source.prepare_source_authority().unwrap();
    let envelope = authority.journal_request().unwrap();
    let replay = SourceAuthorityRegistrationRequest {
        request: Arc::clone(&envelope.request),
        confirmation: envelope.confirmation.clone(),
    };
    let volume = descriptor_capacity_facts(
        &envelope
            .request
            .media_root()
            .try_clone_descriptor()
            .unwrap(),
    )
    .unwrap()
    .identity;
    let app_override = tempfile::tempdir().unwrap();
    let app_path = app_override.path().to_owned();
    let expected_root = std::fs::metadata(media.path()).unwrap();
    let store = OperationJournalStore::open(&profile).unwrap();
    let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
    let client = owner.client();
    let before = client.shared.input_bytes.lock().unwrap().remaining[0];
    let bytes = current_source_metadata::input_bytes(&envelope).unwrap();
    let reply = client
        .submit(bytes, move |input, result| {
            JournalCommand::CurrentRefusedPrefix {
                request: envelope,
                app: app_path,
                panic_after_retention: false,
                probe: None,
                input,
                result,
            }
        })
        .unwrap();
    drop(reply); // Real discarded result; the serial inspection proves execution completed.
    let first = inspect(&client, volume.clone());
    assert!(same_open_object(&first));
    assert!(first.length > 0);
    assert_eq!(
        (first.root_device, first.root_inode),
        (expected_root.dev(), expected_root.ino())
    );
    assert!(first.credit.upgrade().is_some());
    assert_eq!(
        client.shared.input_bytes.lock().unwrap().remaining[0],
        before - bytes
    );
    let registry = app_override.path().join("volume-ownership");
    let marker = std::fs::read_dir(&registry)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let original = std::fs::read(&marker).unwrap();
    assert!(String::from_utf8_lossy(&original).contains("state=dirty"));
    let contender = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&marker)
        .unwrap();
    assert!(contender.try_lock().is_err());
    let mut duplicate = client.register_source_authority(replay).unwrap();
    assert!(matches!(
        duplicate.recv_timeout(Duration::from_secs(10)).unwrap(),
        Err(OperationJournalOwnerError::SourceAuthorityRecoveryRequired { receipt: None, .. })
    ));
    let second = inspect(&client, volume);
    assert_eq!(
        (second.device, second.inode, second.length),
        (first.device, first.inode, first.length)
    );
    assert_eq!(std::fs::read(&marker).unwrap(), original);
    assert_eq!(
        client.shared.input_bytes.lock().unwrap().remaining[0],
        before - bytes
    );
    assert!(owner.shutdown()); // Worker closure only; no clean physical handoff is asserted.
    assert!(!same_open_object(&first));
    assert!(first.credit.upgrade().is_none());
    let drops = first.drops.lock().unwrap();
    assert!(drops.native_before_root && drops.root_before_credit);
    drop(drops);
    assert_eq!(
        client.shared.input_bytes.lock().unwrap().remaining[0],
        ORDINARY_INPUT_BYTES
    );
    contender.try_lock().unwrap();
    assert_eq!(std::fs::read(&marker).unwrap(), original);
}

#[test]
fn current_original_worker_panic_closes_native_capsule_before_releasing_metadata_credit() {
    let app = tempfile::tempdir().unwrap();
    let _app = crate::app_dirs::AppRootGuard::set(app.path().to_owned()).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let media = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let source = crate::sample_sources::SourceDatabase::open_for_source_write_with_profile_guard(
        media.path(),
        &profile,
        db.path(),
    )
    .unwrap();
    let authority = source.prepare_source_authority().unwrap();
    let envelope = authority.journal_request().unwrap();
    let app_override = tempfile::tempdir().unwrap();
    let app_path = app_override.path().to_owned();
    let store = OperationJournalStore::open(&profile).unwrap();
    let mut owner = OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
    let client = owner.client();
    let bytes = current_source_metadata::input_bytes(&envelope).unwrap();
    let probe = Arc::new(Mutex::new(None));
    let producer_probe = Arc::clone(&probe);
    let mut result = client
        .submit(bytes, move |input, result| {
            JournalCommand::CurrentRefusedPrefix {
                request: envelope,
                app: app_path,
                panic_after_retention: true,
                probe: Some(producer_probe),
                input,
                result,
            }
        })
        .unwrap();
    assert!(matches!(
        result.recv_timeout(Duration::from_secs(10)).unwrap(),
        Err(OperationJournalOwnerError::WorkerPanicked)
    ));
    assert!(!owner.shutdown());
    let snapshot = probe.lock().unwrap().take().unwrap();
    assert!(!same_open_object(&snapshot));
    assert!(snapshot.credit.upgrade().is_none());
    let drops = snapshot.drops.lock().unwrap();
    assert!(drops.native_before_root && drops.root_before_credit);
    drop(drops);
    assert_eq!(
        client.shared.input_bytes.lock().unwrap().remaining[0],
        ORDINARY_INPUT_BYTES
    );
    let path = std::fs::read_dir(app_override.path().join("volume-ownership"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let bytes = std::fs::read(&path).unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("state=dirty"));
    let contender = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    contender.try_lock().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}
