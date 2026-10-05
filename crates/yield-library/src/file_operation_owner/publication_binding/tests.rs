#[cfg(all(test, unix))]
mod integration {
    //! Genuine profile/root/file-worker observations; no Native capacity or namespace effects.
    use super::super::*;
    use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
    use crate::test_runtime::TestRuntimeGuard;
    struct Fixture {
        owner: FileOperationOwner,
        guard: WritableProfileGuard,
        stage: RootRegistrationRef,
        target: RootRegistrationRef,
        _profile: PersistenceProfileGuard,
        _base: ConfigBaseGuard,
        _temp: tempfile::TempDir,
        _runtime: TestRuntimeGuard,
    }
    impl Fixture {
        fn new() -> Self {
            let runtime = TestRuntimeGuard::acquire();
            let temp =
                tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
            let base = ConfigBaseGuard::set(temp.path().to_path_buf());
            let profile = PersistenceProfileGuard::named("publication-binding-original");
            let guard = WritableProfileGuard::acquire_current().unwrap();
            std::fs::write(
                guard.profile_root().join("stage.bin"),
                b"actual original retained bytes",
            )
            .unwrap();
            let owner = FileOperationOwner::start().unwrap();
            let stage = owner.register_profile_root(&guard).unwrap();
            let target = owner.register_profile_root(&guard).unwrap();
            Self {
                owner,
                guard,
                stage,
                target,
                _profile: profile,
                _base: base,
                _temp: temp,
                _runtime: runtime,
            }
        }
        fn validation(&self) -> (FileOperationRequest, Arc<FileOperationResult>) {
            let bytes = b"actual original retained bytes";
            let command = FileOperationCommand::for_staging_validation(
                OperationId::new(),
                ParticipantId::new(),
                EffectId::new(),
                OutputIdentity::new(),
                self.stage,
                AuthorityFence::Profile(ProfileAuthorityFence {
                    owner: self.owner.owner_identity(),
                    generation: self.stage.generation,
                    lease_epoch: 1,
                }),
                BoundedRelativeLocator::new("stage.bin").unwrap(),
                OwnedObjectIdentity::new(),
                StagingValidation::new(bytes.len() as u64, *blake3::hash(bytes).as_bytes()),
                vec![],
                None,
            )
            .unwrap();
            let mut request = self.owner.submit(command).unwrap();
            let result = request
                .recv_original_staging_validation(Duration::from_secs(1))
                .unwrap();
            assert!(
                matches!(result.outcome, EffectOutcome::Applied { .. }),
                "{:?}",
                result.outcome
            );
            (request, result)
        }
        fn prepare(
            &self,
            result: Arc<FileOperationResult>,
            effect: EffectId,
            leaf: &str,
        ) -> PublicationTargetPending {
            self.owner
                .prepare_publication_target(
                    result,
                    self.target,
                    BoundedRelativeLocator::new(leaf).unwrap(),
                    effect,
                    Instant::now() + Duration::from_secs(2),
                )
                .unwrap()
        }
    }
    fn wait(
        pending: &mut PublicationTargetPending,
    ) -> Result<PreparedPublicationTarget, BindingError> {
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(target) = pending.try_target()? {
                return Ok(target);
            }
            assert!(Instant::now() < end, "original worker binding deadline");
            thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn held_staging_revalidation_refuses_changed_original_without_moving_cursor() {
        use std::io::Seek;
        let f = Fixture::new();
        let (mut validation, result) = f.validation();
        let mut pending = f.prepare(result, EffectId::new(), "final.wav");
        let target = wait(&mut pending).unwrap();
        let mut original = &target.data.staged_file;
        let cursor = original.stream_position().unwrap();
        assert_eq!(cursor, b"actual original retained bytes".len() as u64);
        assert_eq!(target.revalidate_held_staging(), Ok(()));
        assert_eq!(target.revalidate_held_staging(), Ok(()));
        assert_eq!(original.stream_position().unwrap(), cursor);
        let credit = f.owner.input_bytes_remaining_for_test();
        std::fs::write(
            f.guard.profile_root().join("stage.bin"),
            b"actual altered retained bytes",
        )
        .unwrap();
        assert_eq!(
            target.revalidate_held_staging(),
            Err(BindingError::Conflict)
        );
        assert_eq!(original.stream_position().unwrap(), cursor);
        assert_eq!(credit, f.owner.input_bytes_remaining_for_test());
        assert!(!f.guard.profile_root().join("final.wav").exists());
        validation.acknowledge().unwrap();
        drop((target, pending, validation));
        f.owner.acknowledge_custody_for_test();
        let _ = f.owner.shutdown();
    }

    #[test]
    fn held_staging_revalidation_preserves_renamed_original_and_foreign_replacement() {
        let f = Fixture::new();
        let (mut validation, result) = f.validation();
        let effect = EffectId::new();
        let mut pending = f.prepare(result.clone(), effect, "final.wav");
        let target = wait(&mut pending).unwrap();
        let credit = f.owner.input_bytes_remaining_for_test();
        let stage = f.guard.profile_root().join("stage.bin");
        let renamed = f.guard.profile_root().join("held-original.bin");
        std::fs::rename(&stage, &renamed).unwrap();
        std::fs::write(&stage, b"foreign replacement content").unwrap();
        assert_eq!(target.revalidate_held_staging(), Ok(()));
        std::fs::write(&renamed, b"actual altered retained bytes").unwrap();
        assert_eq!(
            target.revalidate_held_staging(),
            Err(BindingError::Conflict)
        );
        let mut duplicate = f.prepare(result, effect, "final.wav");
        assert!(matches!(wait(&mut duplicate), Err(BindingError::Conflict)));
        assert_eq!(credit, f.owner.input_bytes_remaining_for_test());
        assert_eq!(
            std::fs::read(&stage).unwrap(),
            b"foreign replacement content"
        );
        assert_eq!(
            std::fs::read(&renamed).unwrap(),
            b"actual altered retained bytes"
        );
        assert!(!f.guard.profile_root().join("final.wav").exists());
        validation.acknowledge().unwrap();
        drop((target, pending, duplicate, validation));
        f.owner.acknowledge_custody_for_test();
        let _ = f.owner.shutdown();
    }

    #[test]
    fn held_staging_revalidation_refuses_links_length_and_closed_original_root() {
        let f = Fixture::new();
        let (mut validation, result) = f.validation();
        let mut pending = f.prepare(result, EffectId::new(), "final.wav");
        let target = wait(&mut pending).unwrap();
        let stage = f.guard.profile_root().join("stage.bin");
        let alias = f.guard.profile_root().join("alias.bin");
        std::fs::hard_link(&stage, &alias).unwrap();
        assert_eq!(
            target.revalidate_held_staging(),
            Err(BindingError::Conflict)
        );
        std::fs::remove_file(&alias).unwrap();
        assert_eq!(target.revalidate_held_staging(), Ok(()));
        std::fs::write(&stage, b"short").unwrap();
        assert_eq!(
            target.revalidate_held_staging(),
            Err(BindingError::Conflict)
        );
        std::fs::write(&stage, b"actual original retained bytes").unwrap();
        assert_eq!(target.revalidate_held_staging(), Ok(()));
        f.owner.retire_root(f.stage).unwrap();
        assert_eq!(target.revalidate_held_staging(), Err(BindingError::Closed));
        validation.acknowledge().unwrap();
        drop((target, pending, validation));
        f.owner.acknowledge_custody_for_test();
        let _ = f.owner.shutdown();
    }

    #[test]
    fn original_publication_command_bytes_cannot_mint_primary_grant() {
        let f = Fixture::new();
        let (mut validation, result) = f.validation();
        let effect = EffectId::new();
        let mut pending = f.prepare(result.clone(), effect, "final.wav");
        let target = wait(&mut pending).unwrap();
        let before = f.owner.input_bytes_remaining_for_test();
        let executions = f.owner.shared.execution_count.load(Ordering::Acquire);
        let command = target.publication_command().unwrap();
        assert_eq!(command.operation_id(), result.operation_id());
        assert_eq!(command.participant_id(), result.participant_id());
        assert_eq!(command.effect_id(), effect);
        assert!(matches!(
            command.effect(),
            EffectRequest::PublishNoReplace { .. }
        ));
        assert!(matches!(
            f.owner.submit(command),
            Err(FileOperationOwnerQueueError::InvalidInput(_))
        ));
        assert_eq!(before, f.owner.input_bytes_remaining_for_test());
        assert_eq!(
            executions,
            f.owner.shared.execution_count.load(Ordering::Acquire)
        );
        assert!(!f.guard.profile_root().join("final.wav").exists());
        validation.acknowledge().unwrap();
        drop(target);
        let FileOperationOwnerShutdown::RetirementDeferred { pending, .. } = f.owner.shutdown()
        else {
            panic!("original validation evidence requires confirmed coordinator custody");
        };
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].effect_id, result.effect_id());
    }
    #[test]
    fn actual_original_worker_binds_exact_receipt_roots_descriptor_and_credit() {
        let f = Fixture::new();
        let (mut validation, result) = f.validation();
        let effect = EffectId::new();
        let before = f.owner.input_bytes_remaining_for_test();
        let executions = f.owner.shared.execution_count.load(Ordering::Acquire);
        let mut pending = f.prepare(result.clone(), effect, "final.wav");
        let target = wait(&mut pending).unwrap();
        assert!(target.is_current());
        assert_eq!(
            f.owner.shared.execution_count.load(Ordering::Acquire),
            executions,
            "target preparation performs no effect dispatch"
        );
        assert_eq!(target.publication_effect(), effect);
        assert_eq!(target.validation(), result.as_ref());
        assert_eq!(target.target().registration, f.target);
        assert_eq!(target.target().locator.as_str(), "final.wav");
        assert_eq!(target.target().expected, ExpectedObject::Absent);
        assert_eq!(
            target.volume().device,
            crate::durable_capacity::descriptor_capacity_facts(&target.data.staged_file)
                .unwrap()
                .identity
                .device
        );
        assert_eq!(
            before - f.owner.input_bytes_remaining_for_test(),
            BACKING + REGISTRY_BYTES
        );
        assert!(!f.guard.profile_root().join("final.wav").exists());
        let mut duplicate = f.prepare(result.clone(), effect, "final.wav");
        let alias = wait(&mut duplicate).unwrap();
        assert!(Arc::ptr_eq(&target.data, &alias.data));
        assert_eq!(target.fingerprint(), alias.fingerprint());
        assert_eq!(
            before - f.owner.input_bytes_remaining_for_test(),
            BACKING + REGISTRY_BYTES
        );
        let mut changed = f.prepare(result.clone(), effect, "different.wav");
        assert!(matches!(wait(&mut changed), Err(BindingError::Conflict)));
        let mut clone = f.prepare(
            Arc::new(result.as_ref().clone()),
            EffectId::new(),
            "clone.wav",
        );
        assert!(matches!(wait(&mut clone), Err(BindingError::Invalid)));
        validation.acknowledge().unwrap();
        let mut after_ack = f.prepare(result, effect, "final.wav");
        assert!(Arc::ptr_eq(
            &target.data,
            &wait(&mut after_ack).unwrap().data
        ));
        f.owner.retire_root(f.target).unwrap();
        assert!(!target.is_current());
        drop((
            alias, target, pending, duplicate, changed, clone, after_ack, validation,
        ));
        f.owner.acknowledge_custody_for_test();
        assert!(matches!(
            f.owner.shutdown(),
            FileOperationOwnerShutdown::DrainedAck
        ));
        assert_eq!(
            f.owner.input_bytes_remaining_for_test(),
            FILE_OPERATION_OWNER_MAX_INPUT_BYTES
        );
    }
    #[test]
    fn duplicate_rechecks_actual_content_and_expiry_retains_original_credit() {
        let f = Fixture::new();
        let (mut validation, result) = f.validation();
        let effect = EffectId::new();
        let before = f.owner.input_bytes_remaining_for_test();
        let mut pending = f
            .owner
            .prepare_publication_target(
                result.clone(),
                f.target,
                BoundedRelativeLocator::new("final.wav").unwrap(),
                effect,
                Instant::now() + Duration::from_millis(250),
            )
            .unwrap();
        let target = wait(&mut pending).unwrap();
        std::fs::write(
            f.guard.profile_root().join("stage.bin"),
            b"actual altered retained bytes",
        )
        .unwrap();
        let mut changed = f.prepare(result.clone(), effect, "final.wav");
        assert!(matches!(wait(&mut changed), Err(BindingError::Conflict)));
        std::fs::write(
            f.guard.profile_root().join("stage.bin"),
            b"actual original retained bytes",
        )
        .unwrap();
        let mut restored = f.prepare(result.clone(), effect, "final.wav");
        let alias = wait(&mut restored).unwrap();
        assert!(Arc::ptr_eq(&target.data, &alias.data));
        thread::sleep(
            target
                .data
                .deadline
                .saturating_duration_since(Instant::now())
                + Duration::from_millis(5),
        );
        assert!(!target.is_current());
        let mut renewed = f.prepare(result, effect, "final.wav");
        assert!(matches!(wait(&mut renewed), Err(BindingError::Deadline)));
        assert_eq!(
            before - f.owner.input_bytes_remaining_for_test(),
            BACKING + REGISTRY_BYTES
        );
        drop((target, alias));
        assert_eq!(
            before - f.owner.input_bytes_remaining_for_test(),
            REGISTRY_BYTES
        );
        validation.acknowledge().unwrap();
        drop((validation, pending, changed, restored, renewed));
        f.owner.acknowledge_custody_for_test();
        let _ = f.owner.shutdown();
    }
    #[test]
    fn original_result_capacity_refuses_and_refunds_preparation_input() {
        let f = Fixture::new();
        let (mut validation, result) = f.validation();
        let before = f.owner.input_bytes_remaining_for_test();
        let mut credits = Vec::new();
        while let Ok(credit) = f.owner.shared.reserve_result() {
            credits.push(credit);
        }
        assert_eq!(credits.len(), FILE_OPERATION_OWNER_RESULT_CAPACITY - 1);
        assert!(matches!(
            f.owner.prepare_publication_target(
                result.clone(),
                f.target,
                BoundedRelativeLocator::new("full.wav").unwrap(),
                EffectId::new(),
                Instant::now() + Duration::from_secs(2)
            ),
            Err(FileOperationOwnerQueueError::ResultsFull)
        ));
        assert_eq!(before, f.owner.input_bytes_remaining_for_test());
        credits.pop();
        let mut recovered = f.prepare(result, EffectId::new(), "full.wav");
        let target = wait(&mut recovered).unwrap();
        assert!(!f.guard.profile_root().join("full.wav").exists());
        validation.acknowledge().unwrap();
        drop((target, recovered, credits, validation));
        f.owner.acknowledge_custody_for_test();
        let _ = f.owner.shutdown();
    }
    #[test]
    fn genuine_staging_replacement_target_collision_and_owner_loss_refuse() {
        for case in 0..3 {
            let f = Fixture::new();
            let (mut validation, result) = f.validation();
            if case == 0 {
                std::fs::rename(
                    f.guard.profile_root().join("stage.bin"),
                    f.guard.profile_root().join("original.bin"),
                )
                .unwrap();
                std::fs::write(
                    f.guard.profile_root().join("stage.bin"),
                    b"actual original retained bytes",
                )
                .unwrap();
            } else if case == 1 {
                std::fs::write(f.guard.profile_root().join("final.wav"), b"foreign").unwrap();
            }
            let mut pending = f.prepare(result, EffectId::new(), "final.wav");
            match case {
                0 => assert!(matches!(wait(&mut pending), Err(BindingError::Conflict))),
                1 => assert!(matches!(wait(&mut pending), Err(BindingError::Occupied))),
                _ => {
                    let target = wait(&mut pending).unwrap();
                    f.owner
                        .shared
                        .mark_unavailable("genuine owner unavailable boundary");
                    assert!(!target.is_current());
                    drop(target);
                }
            }
            if case != 2 {
                validation.acknowledge().unwrap();
                f.owner.acknowledge_custody_for_test();
            }
            drop((pending, validation));
            let _ = f.owner.shutdown();
            if case == 1 {
                assert_eq!(
                    std::fs::read(f.guard.profile_root().join("final.wav")).unwrap(),
                    b"foreign"
                );
            }
        }
    }
    #[test]
    fn actual_binding_limit_result_and_input_saturation_preserve_original_limits() {
        let f = Fixture::new();
        let mut targets = Vec::new();
        for index in 0..32 {
            let (mut validation, result) = f.validation();
            let mut pending = f.prepare(result, EffectId::new(), &format!("final-{index}.wav"));
            targets.push(wait(&mut pending).unwrap());
            validation.acknowledge().unwrap();
            drop(validation);
        }
        let (mut extra, result) = f.validation();
        let mut full = f.prepare(result.clone(), EffectId::new(), "overflow.wav");
        assert!(matches!(wait(&mut full), Err(BindingError::Full)));
        drop(targets.remove(0));
        let mut available = f.prepare(result, EffectId::new(), "overflow.wav");
        targets.push(wait(&mut available).unwrap());
        let credit = f
            .owner
            .shared
            .reserve_input(f.owner.input_bytes_remaining_for_test())
            .unwrap();
        assert!(matches!(
            f.owner.prepare_publication_target(
                targets[0].data.validation.clone(),
                f.target,
                BoundedRelativeLocator::new("credit.wav").unwrap(),
                EffectId::new(),
                Instant::now() + Duration::from_secs(2)
            ),
            Err(FileOperationOwnerQueueError::InputBytesFull { .. })
        ));
        drop(credit);
        extra.acknowledge().unwrap();
        drop((targets, extra, full, available));
        f.owner.acknowledge_custody_for_test();
        let _ = f.owner.shutdown();
    }

    #[test]
    fn original_queue_cancel_deadline_and_fifo_substitution_do_not_mint_binding() {
        let f = Fixture::new();
        let (mut validation, result) = f.validation();
        f.owner.pause_before_execute_for_test();
        let queued = {
            let bytes = b"actual original retained bytes";
            let command = FileOperationCommand::for_staging_validation(
                OperationId::new(),
                ParticipantId::new(),
                EffectId::new(),
                OutputIdentity::new(),
                f.stage,
                AuthorityFence::Profile(ProfileAuthorityFence {
                    owner: f.owner.owner_identity(),
                    generation: f.stage.generation,
                    lease_epoch: 1,
                }),
                BoundedRelativeLocator::new("stage.bin").unwrap(),
                OwnedObjectIdentity::new(),
                StagingValidation::new(bytes.len() as u64, *blake3::hash(bytes).as_bytes()),
                vec![],
                None,
            )
            .unwrap();
            let queued = f.owner.submit(command).unwrap();
            queued
        };
        f.owner.wait_until_worker_paused_for_test();
        let cancelled = f.prepare(result.clone(), EffectId::new(), "cancelled.wav");
        drop(cancelled);
        let mut expired = f
            .owner
            .prepare_publication_target(
                result.clone(),
                f.target,
                BoundedRelativeLocator::new("expired.wav").unwrap(),
                EffectId::new(),
                Instant::now() + Duration::from_millis(20),
            )
            .unwrap();
        thread::sleep(Duration::from_millis(30));
        f.owner.release_worker_for_test();
        assert!(matches!(wait(&mut expired), Err(BindingError::Deadline)));
        let mut queued = queued;
        queued.recv_timeout(Duration::from_secs(1)).unwrap();
        queued.acknowledge().unwrap();
        assert!(!f.guard.profile_root().join("cancelled.wav").exists());
        assert!(!f.guard.profile_root().join("expired.wav").exists());
        std::fs::rename(
            f.guard.profile_root().join("stage.bin"),
            f.guard.profile_root().join("held-original.bin"),
        )
        .unwrap();
        let fifo = std::ffi::CString::new(
            f.guard
                .profile_root()
                .join("stage.bin")
                .as_os_str()
                .as_encoded_bytes(),
        )
        .unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let mut substituted = f.prepare(result, EffectId::new(), "fifo.wav");
        assert!(matches!(wait(&mut substituted), Err(BindingError::Unknown)));
        validation.acknowledge().unwrap();
        drop((validation, queued, expired, substituted));
        f.owner.acknowledge_custody_for_test();
        let _ = f.owner.shutdown();
    }
    #[test]
    fn genuine_registered_source_drain_closes_duplicate_preparation() {
        use crate::operation_journal::{
            OperationJournalOwner, OperationJournalOwnerStatus, OperationJournalStore,
        };
        use crate::sample_sources::db::SourceDatabase;
        let f = Fixture::new();
        let store = OperationJournalStore::open(&f.guard).unwrap();
        let mut journal = OperationJournalOwner::start(store).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        if !cfg!(target_os = "macos") {
            loop {
                match journal.status() {
                    OperationJournalOwnerStatus::Initializing if Instant::now() < end => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    OperationJournalOwnerStatus::Unavailable { reason } => {
                        assert!(
                            (reason.contains("reserve bootstrap")
                                && reason.contains("unsupported"))
                                || (reason.contains("volume ownership bootstrap failed")
                                    && reason.contains("shared volume ownership is unsupported")
                                    && reason.contains(
                                        "no qualified identity-safe per-user application root"
                                    )),
                            "{reason}"
                        );
                        assert!(!f.guard.profile_root().join("final.wav").exists());
                        assert!(!journal.shutdown());
                        f.owner.acknowledge_custody_for_test();
                        assert!(matches!(
                            f.owner.shutdown(),
                            FileOperationOwnerShutdown::DrainedAck
                        ));
                        return;
                    }
                    status => panic!("unsupported production bootstrap must refuse: {status:?}"),
                }
            }
        }
        loop {
            match journal.status() {
                OperationJournalOwnerStatus::Available => break,
                OperationJournalOwnerStatus::Initializing if Instant::now() < end => {
                    thread::sleep(Duration::from_millis(5))
                }
                status => panic!("genuine Native reserve/source registration required: {status:?}"),
            }
        }
        let source_dir =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            source_dir.path(),
            &f.guard,
            "publication-binding-source",
        )
        .unwrap();
        let mut authority = database.prepare_source_authority().unwrap();
        let registration = authority.journal_request().unwrap();
        let original = registration.request.clone();
        let client = journal.client();
        let receipt = client
            .register_source_authority(registration)
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        let fence = SourceFence::new(
            SourceId::from_string("publication-binding-source"),
            original.media_root().identity(),
            original.database_file_identity(),
            1,
            None,
            u64::try_from(original.lease_identity().epoch()).unwrap(),
        )
        .unwrap();
        let source = f
            .owner
            .register_source_root(original.clone(), &f.guard, fence.clone())
            .unwrap();
        let bytes = b"actual original retained bytes";
        let command = FileOperationCommand::for_staging_validation(
            OperationId::new(),
            ParticipantId::new(),
            EffectId::new(),
            OutputIdentity::new(),
            f.stage,
            AuthorityFence::Profile(ProfileAuthorityFence {
                owner: f.owner.owner_identity(),
                generation: f.stage.generation,
                lease_epoch: 1,
            }),
            BoundedRelativeLocator::new("stage.bin").unwrap(),
            OwnedObjectIdentity::new(),
            StagingValidation::new(bytes.len() as u64, *blake3::hash(bytes).as_bytes()),
            vec![fence],
            Some(source),
        )
        .unwrap();
        let mut validation = f.owner.submit(command).unwrap();
        let result = validation
            .recv_original_staging_validation(Duration::from_secs(2))
            .unwrap();
        assert!(matches!(result.outcome, EffectOutcome::Applied { .. }));
        let effect = EffectId::new();
        let mut pending = f.prepare(result.clone(), effect, "final.wav");
        let target = wait(&mut pending).unwrap();
        client
            .begin_source_drain(receipt.clone())
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        let mut duplicate = f.prepare(result.clone(), effect, "final.wav");
        assert!(matches!(wait(&mut duplicate), Err(BindingError::Closed)));
        f.owner.retire_root(source).unwrap();
        assert!(!target.is_current());
        assert!(!f.guard.profile_root().join("final.wav").exists());
        validation.acknowledge().unwrap();
        drop((target, pending, duplicate, validation, result));
        f.owner.acknowledge_custody_for_test();
        assert!(matches!(
            f.owner.shutdown(),
            FileOperationOwnerShutdown::DrainedAck
        ));
        let acknowledgement = authority.close_database(database).unwrap();
        client
            .finalize_source_retirement(receipt, acknowledgement)
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        drop((authority, original));
        assert!(journal.shutdown());
    }
    #[test]
    fn original_profile_root_substitution_refuses_without_new_effect_or_root_permission() {
        let f = Fixture::new();
        let (mut validation, result) = f.validation();
        let root = f.guard.profile_root().to_path_buf();
        let displaced = root.with_extension("displaced");
        std::fs::rename(&root, &displaced).unwrap();
        std::fs::create_dir(&root).unwrap();
        let mut pending = f.prepare(result, EffectId::new(), "final.wav");
        assert!(matches!(wait(&mut pending), Err(BindingError::Closed)));
        assert!(!root.join("final.wav").exists());
        std::fs::remove_dir(&root).unwrap();
        std::fs::rename(&displaced, &root).unwrap();
        validation.acknowledge().unwrap();
        drop((validation, pending));
        f.owner.acknowledge_custody_for_test();
        let _ = f.owner.shutdown();
    }
}
