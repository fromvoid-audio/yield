//! Real retained worker/SQLite dispatch observations, not physical admission fixtures.
use super::super::*;
use super::*;
use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
use crate::operation_journal::work_plan::{WorkUnitKey, WorkUnitRole};
use crate::operation_journal::{OperationId, ParticipantId};
use crate::test_runtime::TestRuntimeGuard;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn fixture(seed: Option<u64>, test: impl FnOnce(&mut GlobalLibraryWriter, &WritableProfileGuard)) {
    fixture_with_hook(seed, None, test);
}

fn fixture_with_hook(
    seed: Option<u64>,
    hook: Option<DatabaseHook>,
    test: impl FnOnce(&mut GlobalLibraryWriter, &WritableProfileGuard),
) {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("mutation-fence");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut writer =
        GlobalLibraryWriter::start_inner(&guard, None, None, None, hook, seed).unwrap();
    let limit = Instant::now() + Duration::from_secs(5);
    while writer.status() != GlobalLibraryWriterStatus::Available {
        assert!(Instant::now() < limit, "{:?}", writer.status());
        std::thread::sleep(Duration::from_millis(5));
    }
    test(&mut writer, &guard);
}
fn reserve(
    writer: &GlobalLibraryWriter,
) -> Result<GlobalPreparationReservation, GlobalPreparationError> {
    writer
        .submit_preparation(
            GlobalPreparationRequest::new(
                writer.preparation_owner(),
                [9; 32],
                Instant::now() + Duration::from_secs(1),
                1,
                128,
            )
            .unwrap(),
        )
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
}
fn observe(writer: &GlobalLibraryWriter) -> GlobalPreparationObservation {
    let held = reserve(writer).unwrap();
    let observation = held.observation().clone();
    held.abort()
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    observation
}
fn delayed(
    writer: &GlobalLibraryWriter,
    old: &GlobalPreparationObservation,
) -> Result<GlobalPreparationReservation, GlobalPreparationError> {
    writer
        .submit_preparation(
            GlobalPreparationRequest::from_observation(
                old,
                [9; 32],
                Instant::now() + Duration::from_secs(1),
                1,
                128,
            )
            .unwrap(),
        )
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
}
fn captured(
    writer: &GlobalLibraryWriter,
    request: GlobalMutationRequest,
) -> Result<crate::sample_sources::library::GlobalMutationValue, LibraryError> {
    let context = request.context().clone();
    writer
        .submit_captured_mutation(request)
        .unwrap()
        .recv()
        .unwrap()
        .into_result_for(&context)
}
fn preferences(writer: &GlobalLibraryWriter, value: &str) -> Result<(), LibraryError> {
    writer
        .replace_ui_preferences_payload(value.to_owned())
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
}

#[test]
fn mutation_fence_real_reads_and_preparations_preserve_revision_and_family_writes_advance() {
    fixture(None, |writer, _| {
        let old = observe(writer);
        assert_eq!(old.mutation_revision(), 1);
        writer
            .load_source_registry_snapshot()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        writer
            .lookup_retained_source_for_root(PathBuf::from("missing"))
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        writer
            .load_ui_preferences_payload()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        writer
            .lookup_profile_tag_categories(vec!["missing".into()])
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        writer
            .profile_tag_vocabulary_page(None, String::new(), None, 64)
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        // Compare the stable fence, not each reservation's deliberately fresh nonce.
        assert_eq!(observe(writer).mutation_revision(), old.mutation_revision());
        preferences(writer, "one").unwrap();
        assert_eq!(observe(writer).mutation_revision(), 2);
        writer
            .replace_source_registry(SourceRegistrySnapshot::new(vec![]))
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        assert_eq!(observe(writer).mutation_revision(), 3);
        writer
            .set_profile_tag_category(0, "tone".into(), ProfileTagCategory::Character)
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        assert_eq!(observe(writer).mutation_revision(), 4);
        writer
            .delete_profile_tag_vocabulary(1, "tone".into())
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        assert_eq!(observe(writer).mutation_revision(), 5);
        assert_eq!(
            delayed(writer, &old).err(),
            Some(GlobalPreparationError::StaleMutationFence)
        );
    });
}

#[test]
fn mutation_fence_captured_replay_and_rejected_attempt_invalidate_without_reapplying_metadata() {
    fixture(None, |writer, _| {
        let request =
            GlobalMutationRequest::preferences(writer.acquisition_id(), 1, "original".into())
                .unwrap();
        let before = observe(writer).mutation_revision();
        captured(writer, request.clone()).unwrap();
        captured(writer, request).unwrap();
        let conflict =
            GlobalMutationRequest::preferences(writer.acquisition_id(), 1, "different".into())
                .unwrap();
        assert!(captured(writer, conflict).is_err());
        assert_eq!(observe(writer).mutation_revision(), before + 3);
        assert_eq!(
            writer
                .load_ui_preferences_payload()
                .unwrap()
                .recv()
                .unwrap()
                .unwrap()
                .as_deref(),
            Some("original")
        );
    });
}

#[test]
fn mutation_fence_registry_receipt_commit_and_replay_each_invalidate() {
    fixture(None, |writer, guard| {
        let empty = SourceRegistrySnapshot::new(vec![]);
        let request = SourceRegistryParticipantRequest::new(
            guard.owner_identity(),
            OperationId::new(),
            WorkUnitKey::new(
                1,
                1,
                ParticipantId::from_uuid(uuid::Uuid::new_v4()),
                WorkUnitRole::GlobalLibrary,
            ),
            &empty,
            empty.clone(),
        )
        .unwrap();
        let before = observe(writer).mutation_revision();
        let first = writer
            .apply_source_registry_participant(request.clone())
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        let replay = writer
            .apply_source_registry_participant(request)
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        assert_eq!(first, replay);
        assert_eq!(observe(writer).mutation_revision(), before + 2);
    });
}

#[test]
fn mutation_fence_held_reservation_keeps_queue_and_counter_unchanged_then_rejects_old_observation()
{
    fixture(None, |writer, _| {
        let held = reserve(writer).unwrap();
        let old = held.observation().clone();
        let reply = writer
            .replace_ui_preferences_payload("queued".into())
            .unwrap();
        assert!(matches!(reply.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert_eq!(held.observation().mutation_revision(), 1);
        held.abort()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        reply.recv().unwrap().unwrap();
        assert_eq!(observe(writer).mutation_revision(), 2);
        assert_eq!(
            delayed(writer, &old).err(),
            Some(GlobalPreparationError::StaleMutationFence)
        );
    });
}

#[test]
fn mutation_fence_restart_resets_counter_but_old_registration_cannot_replay() {
    fixture(None, |writer, guard| {
        let old = observe(writer);
        assert!(writer.shutdown());
        *writer = GlobalLibraryWriter::start(guard).unwrap();
        let limit = Instant::now() + Duration::from_secs(5);
        while writer.status() != GlobalLibraryWriterStatus::Available {
            assert!(Instant::now() < limit);
            std::thread::sleep(Duration::from_millis(5));
        }
        let fresh = observe(writer);
        assert_eq!(fresh.mutation_revision(), old.mutation_revision());
        assert_ne!(fresh.owner(), old.owner());
        assert_eq!(
            delayed(writer, &old).err(),
            Some(GlobalPreparationError::StaleOwner)
        );
    });
}

#[test]
fn mutation_fence_exhaustion_rejects_before_sql_preserves_reads_and_never_wraps() {
    let begins = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&begins);
    let hook: DatabaseHook = Arc::new(move |connection| {
        let seen = Arc::clone(&seen);
        connection.authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Transaction {
                    operation: TransactionOperation::Begin
                }
            ) {
                seen.fetch_add(1, Ordering::SeqCst);
            }
            Authorization::Allow
        }));
    });
    fixture_with_hook(Some(MAX_REVISION - 1), Some(hook), |writer, _| {
        let old = observe(writer);
        preferences(writer, "last accepted").unwrap();
        assert!(
            begins.swap(0, Ordering::SeqCst) >= 2,
            "actual preparation and metadata transaction observed"
        );
        assert!(matches!(
            preferences(writer, "must not write"),
            Err(LibraryError::GlobalMutationFenceExhausted)
        ));
        assert!(matches!(
            writer
                .replace_source_registry(SourceRegistrySnapshot::new(vec![]))
                .unwrap()
                .recv()
                .unwrap(),
            Err(LibraryError::GlobalMutationFenceExhausted)
        ));
        assert_eq!(
            reserve(writer).err(),
            Some(GlobalPreparationError::MutationFenceExhausted)
        );
        assert_eq!(
            delayed(writer, &old).err(),
            Some(GlobalPreparationError::StaleMutationFence)
        );
        assert_eq!(
            writer
                .load_ui_preferences_payload()
                .unwrap()
                .recv()
                .unwrap()
                .unwrap()
                .as_deref(),
            Some("last accepted")
        );
        let request = GlobalMutationRequest::preferences(
            writer.acquisition_id(),
            2,
            "captured must not write".into(),
        )
        .unwrap();
        assert!(matches!(
            captured(writer, request),
            Err(LibraryError::GlobalMutationFenceExhausted)
        ));
        assert_eq!(
            writer
                .load_ui_preferences_payload()
                .unwrap()
                .recv()
                .unwrap()
                .unwrap()
                .as_deref(),
            Some("last accepted")
        );
        assert_eq!(
            begins.load(Ordering::SeqCst),
            0,
            "exhaustion and stale preparation must reject before BEGIN"
        );
    });
}

#[test]
fn mutation_fence_stale_observation_rejects_before_begin_on_nonexhausted_worker() {
    let begins = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&begins);
    let hook: DatabaseHook = Arc::new(move |connection| {
        let seen = Arc::clone(&seen);
        connection.authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Transaction {
                    operation: TransactionOperation::Begin
                }
            ) {
                seen.fetch_add(1, Ordering::SeqCst);
            }
            Authorization::Allow
        }));
    });
    fixture_with_hook(None, Some(hook), |writer, _| {
        let old = observe(writer);
        preferences(writer, "changed").unwrap();
        assert!(begins.swap(0, Ordering::SeqCst) >= 2);
        assert_eq!(
            delayed(writer, &old).err(),
            Some(GlobalPreparationError::StaleMutationFence)
        );
        assert_eq!(begins.load(Ordering::SeqCst), 0);
        assert_eq!(
            observe(writer).mutation_revision(),
            old.mutation_revision() + 1
        );
        assert_eq!(
            begins.load(Ordering::SeqCst),
            1,
            "fresh preparation uses the actual retained connection"
        );
    });
}
