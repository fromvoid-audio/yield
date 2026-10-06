//! Genuine original producer/worker paths. These cases require actual native resources.
//! Linux typechecks the shared harness; it cannot execute a positive physical reserve case.
use super::super::store::current_reserve_custody::CurrentReserveBoundary;
use super::*;
use std::{
    fs,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::Path,
};

const WAIT: Duration = Duration::from_secs(30);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Opened {
    fd: i32,
    device: u64,
    inode: u64,
    length: u64,
}
fn observe(file: &fs::File) -> Opened {
    let metadata = file.metadata().unwrap();
    Opened {
        fd: file.as_raw_fd(),
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.len(),
    }
}
pub(super) struct NativeSnapshot {
    opened: Opened,
    custody: current_source_custody_tests::Snapshot,
    owner_generation: u64,
    binding_generation: Option<u64>,
}
impl OwnerRuntime {
    // Test-only existing-consumer driver: primary create is installed on this original actor
    // BEFORE reopening under the SAME issued proof/generation. This is never an ordinary retry.
    pub(super) fn current_same_generation_reopen_for_test(
        &mut self,
        volume: &VolumeIdentity,
        scope: &crate::sample_sources::db::CurrentReserveUseProof<'_>,
    ) -> Result<(), String> {
        let original = self.source_volumes.get(volume).unwrap();
        let owner = original
            .owner
            .as_ref()
            .unwrap()
            .try_clone_for_context()
            .map_err(|e| e.to_string())?;
        let result = self.store().acquire_owned_current_reserve(
            owner,
            scope,
            &original._current_reserve_failure,
        );
        match result {
            Err(SourceReserveBindingFailure {
                error,
                volume_owner,
                reserve,
            }) => {
                let binding = self.source_volumes.get_mut(volume).unwrap();
                binding.owner = Some(volume_owner);
                binding.readiness = SourceVolumeBindingReadiness::Incomplete;
                if let Some(reserve) = reserve {
                    binding
                        ._current_reserve_failure
                        .retain_opened_for_test(reserve);
                }
                Err(error.to_string())
            }
            Ok(_) => panic!("native existing-consumer driver did not reach its declared fault"),
        }
    }

    pub(super) fn inspect_current_native_reserve(
        &mut self,
        volume: &VolumeIdentity,
    ) -> Result<NativeSnapshot, OperationJournalOwnerError> {
        let binding_generation = self.store().current_binding_generation_for_test(volume);
        let binding = self.source_volumes.get_mut(volume).unwrap();
        assert_eq!(binding.readiness, SourceVolumeBindingReadiness::Incomplete);
        let owner = binding
            .owner
            .as_ref()
            .expect("original native volume owner retained");
        owner.revalidate().unwrap();
        let owner_generation = owner.owner_generation();
        let opened = if let Some(reserve) = binding
            ._current_reserve_failure
            .opened_reserve_for_test()
            .as_ref()
        {
            observe(reserve.file_for_custody_test())
        } else {
            observe(
                binding
                    .reserve
                    .as_ref()
                    .expect("original opened reserve retained")
                    .file_for_custody_test(),
            )
        };
        let custody = current_source_custody_tests::install_native_reserve_drop_probe(
            binding,
            opened.fd,
            opened.device,
            opened.inode,
            opened.length,
        );
        Ok(NativeSnapshot {
            opened,
            custody,
            owner_generation,
            binding_generation,
        })
    }
}

struct Pause {
    entered: Receiver<Opened>,
    resume: Option<SyncSender<()>>,
    calls: Arc<AtomicUsize>,
}
impl Drop for Pause {
    fn drop(&mut self) {
        if let Some(resume) = self.resume.take() {
            let _ = resume.try_send(());
        }
    }
}
fn pause(store: &OperationJournalStore, selected: CurrentReserveBoundary) -> Pause {
    let (entered_tx, entered) = mpsc::sync_channel(1);
    let (resume, resumed) = mpsc::sync_channel(1);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&calls);
    store.set_current_reserve_test_hook(move |boundary, file| {
        if boundary != selected {
            return;
        }
        assert_eq!(observed_calls.fetch_add(1, Ordering::SeqCst), 0);
        entered_tx.send(observe(file)).unwrap();
        resumed.recv_timeout(WAIT).unwrap();
    });
    Pause {
        entered,
        resume: Some(resume),
        calls,
    }
}
fn wait_native(owner: &OperationJournalOwner) {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        match owner.status() {
            OperationJournalOwnerStatus::Available => return,
            OperationJournalOwnerStatus::Unavailable { reason } => {
                panic!("genuine native bootstrap failed: {reason}")
            }
            status if std::time::Instant::now() >= deadline => {
                panic!("native bootstrap timed out: {status:?}")
            }
            _ => thread::sleep(Duration::from_millis(5)),
        }
    }
}
fn open_source(
    profile: &WritableProfileGuard,
    root: &Path,
) -> crate::sample_sources::SourceDatabase {
    crate::sample_sources::SourceDatabase::open_for_source_write_with_profile_guard(
        root.join("media"),
        profile,
        root.join("database"),
    )
    .unwrap()
}
fn native_case(selected: CurrentReserveBoundary, clean: bool, lost_reply: bool) {
    assert!(
        cfg!(target_os = "macos"),
        "positive case requires native macOS SDK/runtime/APFS"
    );
    let external = fs::canonicalize(
        std::env::var_os("YIELD_CURRENT_CUSTODY_EXTERNAL_ROOT")
            .expect("fresh distinct mounted native volume required"),
    )
    .unwrap();
    // Preserve uncertain profile/source evidence, including on assertion failure.
    let app = tempfile::tempdir().unwrap().keep();
    let _app = crate::app_dirs::AppRootGuard::set(app.clone()).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let source_facts = descriptor_capacity_facts(&fs::File::open(&external).unwrap()).unwrap();
    let profile_facts =
        descriptor_capacity_facts(&fs::File::open(profile.profile_root()).unwrap()).unwrap();
    assert_ne!(source_facts.identity, profile_facts.identity);
    assert!(source_facts.free_bytes >= 2 * crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR);
    let root = external.join(format!("current-native-fault-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("media")).unwrap();
    fs::create_dir(root.join("database")).unwrap();
    eprintln!(
        "preserved native fault roots profile={} source={} boundary={selected:?}",
        app.display(),
        root.display()
    );
    let mut prior_identity = None;
    if clean {
        let mut owner =
            OperationJournalOwner::start(OperationJournalStore::open(&profile).unwrap()).unwrap();
        wait_native(&owner);
        let source = open_source(&profile, &root);
        let mut authority = source.prepare_source_authority().unwrap();
        let mut registration = owner
            .client()
            .register_source_authority(authority.journal_request().unwrap())
            .unwrap();
        let receipt = registration.recv_timeout(WAIT).unwrap().unwrap();
        let reserve = fs::File::open(root.join("media/recovery-reserve.bin")).unwrap();
        let metadata = reserve.metadata().unwrap();
        prior_identity = Some((metadata.dev(), metadata.ino(), metadata.len()));
        let mut drain = owner.client().begin_source_drain(receipt.clone()).unwrap();
        drain.recv_timeout(WAIT).unwrap().unwrap();
        let ack = authority.close_database(source).unwrap();
        let mut retirement = owner
            .client()
            .finalize_source_retirement(receipt, ack)
            .unwrap();
        retirement.recv_timeout(WAIT).unwrap().unwrap();
        drop(authority);
        assert!(owner.shutdown());
    }
    let store = OperationJournalStore::open(&profile).unwrap();
    let mut gate = pause(&store, selected);
    if selected == CurrentReserveBoundary::RecoveryReopened {
        store.set_current_reopen_fixture();
    }
    let mut owner = OperationJournalOwner::start(store).unwrap();
    wait_native(&owner);
    // Acquire original Source lease/confirmation only AFTER real profile reserve bootstrap.
    let source = open_source(&profile, &root);
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
    let bytes = current_source_metadata::input_bytes(&envelope).unwrap();
    let client = owner.client();
    let mut reply = Some(client.register_source_authority(envelope).unwrap());
    if lost_reply {
        drop(reply.take());
    }
    let opened = gate.entered.recv_timeout(WAIT).unwrap();
    assert!(opened.length >= crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR);
    if let Some(prior) = prior_identity {
        assert_eq!((opened.device, opened.inode, opened.length), prior);
    }
    // Genuine original Source-owner-thread revocation. It closes admission without waiting
    // on the effect pin held by the paused native consumer.
    drop(authority);
    gate.resume.take().unwrap().send(()).unwrap();
    if let Some(mut reply) = reply {
        assert!(matches!(
            reply.recv_timeout(WAIT).unwrap(),
            Err(
                OperationJournalOwnerError::SourceAuthorityRecoveryRequired {
                    receipt: Some(_),
                    ..
                }
            )
        ));
    }
    let mut inspection = client
        .submit(512, move |input, result| {
            JournalCommand::InspectCurrentNativeReserve {
                volume,
                input,
                result,
            }
        })
        .unwrap();
    let snapshot = inspection.recv_timeout(WAIT).unwrap().unwrap();
    let mut barrier = client.get(OperationId::new()).unwrap();
    assert!(barrier.recv_timeout(WAIT).unwrap().unwrap().is_none());
    assert_eq!(snapshot.opened, opened);
    current_source_custody_tests::assert_native_reserve_retained(&snapshot.custody);
    match selected {
        CurrentReserveBoundary::Created => assert_eq!(snapshot.binding_generation, None),
        CurrentReserveBoundary::CreatedBound | CurrentReserveBoundary::CleanRebound => {
            assert_eq!(snapshot.binding_generation, Some(snapshot.owner_generation))
        }
        CurrentReserveBoundary::CleanRebindOpened => {
            assert!(snapshot.binding_generation.unwrap() < snapshot.owner_generation)
        }
        CurrentReserveBoundary::RecoveryReopened => {
            assert_eq!(snapshot.binding_generation, Some(snapshot.owner_generation))
        }
    }
    assert_eq!(
        client.shared.input_bytes.lock().unwrap().remaining[0],
        ORDINARY_INPUT_BYTES - resource_custody::HEADER_BYTES - bytes
    );
    let mut duplicate = client.register_source_authority(replay).unwrap();
    assert!(matches!(
        duplicate.recv_timeout(WAIT).unwrap(),
        Err(OperationJournalOwnerError::SourceAuthorityRecoveryRequired { .. })
    ));
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    assert!(owner.shutdown());
    current_source_custody_tests::assert_native_reserve_closed_in_order(&snapshot.custody);
    assert_eq!(
        client.shared.input_bytes.lock().unwrap().remaining[0],
        ORDINARY_INPUT_BYTES
    );
    let metadata = fs::metadata(root.join("media/recovery-reserve.bin")).unwrap();
    assert_eq!(
        (metadata.dev(), metadata.ino(), metadata.len()),
        (opened.device, opened.inode, opened.length)
    );
    drop(source);
}

#[test]
#[ignore = "requires fresh isolated native macOS account and distinct mounted APFS volume"]
fn current_original_native_created_revocation_retains_opened_backing() {
    native_case(CurrentReserveBoundary::Created, false, false);
}
#[test]
#[ignore = "requires separate fresh native account/APFS volume; preserves committed observations"]
fn current_original_native_created_binding_lost_reply_retains_backing() {
    native_case(CurrentReserveBoundary::CreatedBound, false, true);
}
#[test]
#[ignore = "requires separate fresh native account/APFS volume and genuine completed prior retirement"]
fn current_original_native_clean_rebind_open_revocation_retains_backing() {
    native_case(CurrentReserveBoundary::CleanRebindOpened, true, false);
}
#[test]
#[ignore = "requires separate fresh native account/APFS volume; committed rebind is not rollback"]
fn current_original_native_clean_rebind_commit_revocation_retains_backing() {
    native_case(CurrentReserveBoundary::CleanRebound, true, false);
}

#[test]
#[ignore = "requires separate fresh native account/APFS volume; direct existing consumer, never ordinary retry"]
fn current_original_native_same_generation_reopen_revocation_retains_opened_backing() {
    native_case(CurrentReserveBoundary::RecoveryReopened, false, false);
}
