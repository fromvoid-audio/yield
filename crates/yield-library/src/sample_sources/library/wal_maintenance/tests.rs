use super::*;
use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};
use crate::sample_sources::library::*;
use crate::test_runtime::TestRuntimeGuard;
use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

mod operation_link;
mod ownership;
mod registry;

#[derive(Clone, Copy)]
enum Fault {
    None,
    CheckpointDenied,
    ObservationAfterCommitDenied,
}

fn pressure_hook(
    counter: Arc<AtomicUsize>,
    fault: Fault,
) -> Arc<dyn Fn(&Connection) + Send + Sync> {
    Arc::new(move |connection| {
        connection
            .execute_batch(
                "PRAGMA wal_autocheckpoint=0;
            CREATE TABLE maintenance_payload(value BLOB);
            INSERT INTO maintenance_payload VALUES (zeroblob(34603008));
            INSERT INTO profile_tag_vocabulary(label, category) VALUES ('kick', 'sound-type');",
            )
            .unwrap();
        assert!(
            crate::sqlite_wal::retained_wal_bytes(connection).unwrap()
                >= crate::WAL_SOFT_WATERMARK_BYTES
        );
        let committed = Arc::new(AtomicBool::new(false));
        if matches!(fault, Fault::ObservationAfterCommitDenied) {
            let committed = committed.clone();
            connection.commit_hook(Some(move || {
                committed.store(true, Ordering::SeqCst);
                false
            }));
        }
        let counter = counter.clone();
        connection.authorizer(Some(move |context: AuthContext<'_>| match context.action {
            AuthAction::Pragma {
                pragma_name: "wal_checkpoint",
                ..
            } => {
                counter.fetch_add(1, Ordering::SeqCst);
                if matches!(fault, Fault::CheckpointDenied) {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }
            AuthAction::Pragma {
                pragma_name: "journal_mode",
                ..
            } if committed.load(Ordering::SeqCst) => Authorization::Deny,
            _ => Authorization::Allow,
        }));
    })
}

fn available(writer: &GlobalLibraryWriter) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while writer.status() != GlobalLibraryWriterStatus::Available {
        assert!(Instant::now() < deadline, "{:?}", writer.status());
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn closed_after_profile_change(writer: &GlobalLibraryWriter) {
    // The command's error reply precedes terminal closure; shutdown's boolean only
    // identifies who initiated closure, so observe the actual terminal state first.
    let deadline = Instant::now() + Duration::from_secs(5);
    while writer.status() != GlobalLibraryWriterStatus::Closed {
        assert!(Instant::now() < deadline, "{:?}", writer.status());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(
        writer.unavailable_reason(),
        Some(GlobalLibraryWriterUnavailable::ProfileOwnershipChanged { .. })
    ));
    assert!(matches!(
        writer.load_source_registry_snapshot(),
        Err(GlobalLibraryWriterQueueError::Closed)
    ));
}

fn complete(
    writer: &GlobalLibraryWriter,
    request: GlobalMutationRequest,
) -> Result<GlobalMutationValue, LibraryError> {
    let context = request.context().clone();
    writer
        .submit_captured_mutation(request)
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .into_result_for(&context)
}

#[test]
fn ordinary_global_wal_actual_worker_all_four_commits_checkpoint_once_and_reads_never_do() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    for route in 0..4 {
        let _profile = PersistenceProfileGuard::named(format!("ordinary-maintenance-{route}"));
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
            &guard,
            pressure_hook(counter.clone(), Fault::None),
            None,
        )
        .unwrap();
        available(&writer);
        writer
            .load_source_registry_snapshot()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        assert!(
            writer
                .load_ui_preferences_payload()
                .unwrap()
                .recv()
                .unwrap()
                .unwrap()
                .is_none()
        );
        // Both no-op vocabulary routes roll back. A stale CAS also must not mark a commit.
        assert!(
            !writer
                .set_profile_tag_category(0, "kick".into(), ProfileTagCategory::SoundType)
                .unwrap()
                .recv()
                .unwrap()
                .unwrap()
                .changed
        );
        assert!(
            writer
                .delete_profile_tag_vocabulary(0, "missing".into())
                .unwrap()
                .recv()
                .unwrap()
                .unwrap()
                .removed
                .is_none()
        );
        assert!(
            writer
                .set_profile_tag_category(1, "kick".into(), ProfileTagCategory::Character)
                .unwrap()
                .recv()
                .unwrap()
                .is_err()
        );
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        let request = match route {
            0 => GlobalMutationRequest::registry(
                writer.acquisition_id(),
                1,
                SourceRegistrySnapshot::new(Vec::new()),
            )
            .unwrap(),
            1 => GlobalMutationRequest::preferences(writer.acquisition_id(), 1, "committed".into())
                .unwrap(),
            2 => GlobalMutationRequest::vocabulary_set(
                writer.acquisition_id(),
                1,
                0,
                "kick".into(),
                ProfileTagCategory::Character,
            )
            .unwrap(),
            _ => GlobalMutationRequest::vocabulary_delete(
                writer.acquisition_id(),
                1,
                0,
                "kick".into(),
            )
            .unwrap(),
        };
        complete(&writer, request).unwrap();
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "route {route} must use its retained connection"
        );
        complete(
            &writer,
            GlobalMutationRequest::preferences(writer.acquisition_id(), 2, "another".into())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "connection-local interval bounds repeated commits"
        );
        writer
            .load_ui_preferences_payload()
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(writer.shutdown());
    }
}

#[test]
fn ordinary_global_wal_actual_worker_maintenance_failures_preserve_committed_result() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    for (index, fault) in [Fault::CheckpointDenied, Fault::ObservationAfterCommitDenied]
        .into_iter()
        .enumerate()
    {
        let _profile = PersistenceProfileGuard::named(format!("maintenance-fault-{index}"));
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
            &guard,
            pressure_hook(counter.clone(), fault),
            None,
        )
        .unwrap();
        available(&writer);
        complete(
            &writer,
            GlobalMutationRequest::preferences(writer.acquisition_id(), 1, "committed".into())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            writer
                .load_ui_preferences_payload()
                .unwrap()
                .recv()
                .unwrap()
                .unwrap()
                .as_deref(),
            Some("committed")
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            if index == 0 { 1 } else { 0 }
        );
        assert!(writer.shutdown());
    }
}

#[test]
fn ordinary_global_wal_actual_worker_preserves_pinned_reader_snapshot() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("maintenance-reader");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let mut writer = GlobalLibraryWriter::start_with_database_hook_for_test(
        &guard,
        pressure_hook(counter.clone(), Fault::None),
        None,
    )
    .unwrap();
    available(&writer);
    let reader = Connection::open_with_flags(
        guard.profile_root().join(LIBRARY_DB_FILE_NAME),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM metadata")
        .unwrap();
    complete(
        &writer,
        GlobalMutationRequest::preferences(writer.acquisition_id(), 1, "new".into()).unwrap(),
    )
    .unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert_eq!(
        reader
            .query_row(
                "SELECT count(*) FROM metadata WHERE key = ?1",
                [UI_PREFERENCES_KEY],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    reader.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        reader
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                [UI_PREFERENCES_KEY],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "new"
    );
    drop(reader);
    assert!(writer.shutdown());
}
