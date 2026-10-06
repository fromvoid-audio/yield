use super::*;
use crate::operation_journal::ParticipantId;
use crate::{
    app_dirs::AppRootGuard,
    sample_sources::{DB_FILE_NAME, SourceDatabase},
    test_runtime::TestRuntimeGuard,
};
use std::fs;

pub(super) struct Fixture {
    pub(super) runtime: OwnerRuntime,
    pub(super) shared: Arc<OwnerShared>,
    _receivers: OwnerCommandReceivers,
    pub(super) guard: WritableProfileGuard,
    _root: AppRootGuard,
    pub(super) profile: tempfile::TempDir,
    _media: tempfile::TempDir,
    _test: TestRuntimeGuard,
}
impl Fixture {
    pub(super) fn new() -> Self {
        let test = TestRuntimeGuard::acquire();
        let profile = tempfile::tempdir().unwrap();
        let media = tempfile::tempdir().unwrap();
        let root = AppRootGuard::set(profile.path().to_owned()).unwrap();
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let child = profile.path().join("legacy");
        fs::create_dir(&child).unwrap();
        rusqlite::Connection::open(child.join(DB_FILE_NAME))
            .unwrap()
            .execute_batch(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/profile-v11/source.sql"
            )))
            .unwrap();
        let source =
            SourceDatabase::open_legacy_read_owner(media.path(), &guard, "legacy").unwrap();
        let store = OperationJournalStore::open(&guard).unwrap();
        let mut runtime = OwnerRuntime::new(store).unwrap();
        let (senders, receivers) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(senders));
        shared
            .lifecycle
            .store(OwnerLifecycle::Available as u8, Ordering::Release);
        let input = shared
            .reserve_input(
                InputClass::Ordinary,
                super::super::legacy_adoption_custody::INPUT_BYTES,
            )
            .unwrap();
        runtime
            .prepare_legacy_custody(
                source,
                ParticipantId::from_uuid(Uuid::from_u128(11)),
                SourceCommandId::from_uuid(Uuid::from_u128(12)),
                input,
            )
            .unwrap();
        Self {
            runtime,
            shared,
            _receivers: receivers,
            guard,
            _root: root,
            profile,
            _media: media,
            _test: test,
        }
    }
    fn input(&self) -> InputPermit {
        self.shared
            .reserve_input(InputClass::Ordinary, REPLY_BYTES)
            .unwrap()
    }
    pub(super) fn source_bytes(&self) -> Vec<u8> {
        fs::read(self.profile.path().join("legacy").join(DB_FILE_NAME)).unwrap()
    }
    pub(super) fn remaining(&self) -> usize {
        self.shared.input_bytes.lock().unwrap().remaining[0]
    }
}

#[test]
fn original_worker_prelease_missing_physical_floor_refuses_before_bootstrap_and_refunds_only_unused_input()
 {
    let mut f = Fixture::new();
    let source = f.source_bytes();
    let baseline = f.remaining();
    let retained = f.runtime.legacy_adoption.as_ref().unwrap();
    let command = retained.args.source_command_id();
    let digest = retained.digest;
    let input = f.input();
    let refused = f.runtime.prepare_legacy_volumes(command, digest, input);
    assert!(matches!(
        refused,
        Err(OperationJournalOwnerError::LegacyAdoptionCustody(
            LegacyAdoptionCustodyError::Changed
        ))
    ));
    assert_eq!(f.remaining(), baseline);
    assert!(
        f.runtime
            .legacy_adoption
            .as_ref()
            .unwrap()
            .volumes
            .is_none()
    );
    f.runtime.validate_legacy_custody(command, digest).unwrap();
    assert_eq!(f.source_bytes(), source);
    assert!(
        !f.profile
            .path()
            .join("legacy/recovery-reserve.bin")
            .exists()
    );
}

#[test]
fn complete_private_checkpoint_advances_after_each_of_two_real_journal_prefixes_without_refreshing_old_public_handle()
 {
    let mut f = Fixture::new();
    let source = f.source_bytes();
    let mut retained = f.runtime.legacy_adoption.take().unwrap();
    let original = retained.args.clone();
    let deadline = Instant::now() + Duration::from_secs(2);
    retained.volumes = Some(RetainedLegacyVolumes {
        entries: Vec::new(),
        original_digest: retained.digest,
        checkpoint: retained.args.clone(),
        ready: false,
        _input: f
            .shared
            .reserve_input(InputClass::Ordinary, RETAINED_BYTES)
            .unwrap(),
    });
    for prefix in 0..2 {
        f.runtime
            .verify_legacy_prefix(&retained, &f.guard, deadline)
            .unwrap();
        // Exact prefix hook writes the real journal's compiled binding table and forces growth.
        // It supplies neither a physical reserve nor a qualified two-volume mount fixture.
        f.runtime.store().legacy_binding_prefix_for_test(prefix);
        assert!(
            f.runtime
                .verify_legacy_prefix(&retained, &f.guard, deadline)
                .is_err()
        );
        f.runtime
            .advance_legacy_prefix(&mut retained, &f.guard, deadline)
            .unwrap();
        f.runtime
            .verify_legacy_prefix(&retained, &f.guard, deadline)
            .unwrap();
        assert_eq!(retained.args, original); // Public custody stays frozen until final success.
    }
    assert_ne!(retained.volumes.as_ref().unwrap().checkpoint, original);
    assert_eq!(f.source_bytes(), source);
    let command = retained.args.source_command_id();
    let digest = retained.digest;
    f.runtime.legacy_adoption = Some(retained);
    assert!(f.runtime.validate_legacy_custody(command, digest).is_err());
}

#[test]
fn partial_slot_refuses_exact_reentry_without_new_bootstrap_or_credit_release() {
    let mut f = Fixture::new();
    let retained = f.runtime.legacy_adoption.as_mut().unwrap();
    retained.volumes = Some(RetainedLegacyVolumes {
        entries: Vec::new(),
        original_digest: retained.digest,
        checkpoint: retained.args.clone(),
        ready: false,
        _input: f
            .shared
            .reserve_input(InputClass::Ordinary, RETAINED_BYTES)
            .unwrap(),
    });
    let command = retained.args.source_command_id();
    let digest = retained.digest;
    let baseline = f.remaining();
    let source = f.source_bytes();
    for _ in 0..2 {
        let input = f.input();
        assert!(matches!(
            f.runtime.prepare_legacy_volumes(command, digest, input),
            Err(OperationJournalOwnerError::LegacyAdoptionCustody(
                LegacyAdoptionCustodyError::ReserveRecovery
            ))
        ));
        assert_eq!(f.remaining(), baseline);
    }
    assert_eq!(f.source_bytes(), source);
    assert!(
        f.runtime
            .legacy_adoption
            .as_ref()
            .unwrap()
            .volumes
            .is_some()
    );
}

#[test]
fn completed_private_checkpoint_duplicate_retains_each_reply_copy_credit_and_does_no_bootstrap() {
    let mut f = Fixture::new();
    let retained = f.runtime.legacy_adoption.as_mut().unwrap();
    // Isolate passive duplicate/copy lifetime; no native floor or physical volume fixture is
    // constructed by this test. Actual qualified Existing entries are verified on macOS.
    retained.volumes = Some(RetainedLegacyVolumes {
        entries: Vec::new(),
        original_digest: retained.digest,
        checkpoint: retained.args.clone(),
        ready: true,
        _input: f
            .shared
            .reserve_input(InputClass::Ordinary, RETAINED_BYTES)
            .unwrap(),
    });
    let command = retained.args.source_command_id();
    let digest = retained.digest;
    let baseline = f.remaining();
    let bytes = f.source_bytes();
    let first_input = f.input();
    let first = f
        .runtime
        .prepare_legacy_volumes(command, digest, first_input)
        .unwrap();
    assert_eq!(f.remaining(), baseline - REPLY_BYTES);
    let second_input = f.input();
    let second = f
        .runtime
        .prepare_legacy_volumes(command, digest, second_input)
        .unwrap();
    assert_eq!(first.input(), second.input());
    assert_eq!(f.remaining(), baseline - 2 * REPLY_BYTES);
    assert_eq!(f.source_bytes(), bytes);
    assert!(
        !f.profile
            .path()
            .join("legacy/recovery-reserve.bin")
            .exists()
    );
    drop(first);
    assert_eq!(f.remaining(), baseline - REPLY_BYTES);
    drop(second); // Same credit release as a dropped/lost queued reply.
    assert_eq!(f.remaining(), baseline);
    assert!(
        f.runtime
            .legacy_adoption
            .as_ref()
            .unwrap()
            .volumes
            .is_some()
    );
}
