//! Genuine normal-build Linux global worker requester tests, with no physical grant fixtures.
#![cfg(all(target_os = "linux", not(feature = "test-support")))]
use rusqlite::Connection;
use std::{
    thread,
    time::{Duration, Instant},
};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    sample_sources::{
        GlobalLibraryWriter, GlobalLibraryWriterStatus,
        library::{
            GlobalLibraryWriterQueueError, GlobalPreparationError, GlobalPreparationRequest,
            GlobalPreparationReservation, GlobalWalPreparationClient, LIBRARY_DB_FILE_NAME,
        },
    },
};
fn start(profile: &WritableProfileGuard) -> GlobalLibraryWriter {
    let writer = GlobalLibraryWriter::start(profile).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while writer.status() != GlobalLibraryWriterStatus::Available {
        assert!(Instant::now() < deadline, "{:?}", writer.status());
        thread::sleep(Duration::from_millis(5));
    }
    writer
}
fn request(client: &GlobalWalPreparationClient) -> GlobalPreparationRequest {
    GlobalPreparationRequest::new(
        client.preparation_owner(),
        [81; 32],
        Instant::now() + Duration::from_secs(2),
        1,
        4096,
    )
    .unwrap()
    .with_wal_binding()
}
fn prepare(client: &GlobalWalPreparationClient) -> GlobalPreparationReservation {
    client
        .submit_preparation(request(client))
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap()
}
fn release(held: GlobalPreparationReservation) {
    held.abort()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
}
#[test]
fn original_normal_worker_scope_and_sqlite_exclusion_survive_limited_client_clone() {
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let mut writer = start(&profile);
    let client = writer.wal_preparation_client();
    let clone = client.clone();
    let held = prepare(&clone);
    assert_eq!(held.observation().owner(), &writer.preparation_owner());
    assert_eq!(held.observation().command_digest(), &[81; 32]);
    let scope = held.observation().wal_scope().unwrap();
    assert_ne!(scope.main().identity(), scope.wal().identity());
    assert_ne!(scope.main().identity(), scope.shm().identity());
    let outside = Connection::open(profile.profile_root().join(LIBRARY_DB_FILE_NAME)).unwrap();
    outside.busy_timeout(Duration::ZERO).unwrap();
    assert!(matches!(outside.execute_batch("BEGIN IMMEDIATE"),
        Err(rusqlite::Error::SqliteFailure(e,_)) if e.code==rusqlite::ErrorCode::DatabaseBusy));
    let queued = writer
        .replace_ui_preferences_payload("after-abort".into())
        .unwrap();
    assert!(matches!(
        queued.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    release(held);
    queued
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    outside.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    writer.shutdown();
    assert_eq!(clone.status(), GlobalLibraryWriterStatus::Closed);
    assert!(matches!(
        clone.submit_preparation(request(&clone)),
        Err(GlobalLibraryWriterQueueError::Closed)
    ));
}
#[test]
fn original_strict_byte_pool_shared_across_requesters_writer_and_retained_scope_copies() {
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let mut writer = start(&profile);
    let client = writer.wal_preparation_client();
    let clone = client.clone();
    let mut views = Vec::new();
    for i in 0..4 {
        let held = if i == 0 {
            writer
                .submit_preparation(request(&client))
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap()
        } else {
            prepare(&clone)
        };
        views.push(held.observation().clone());
        release(held);
    }
    assert!(matches!(
        client.submit_preparation(request(&client)),
        Err(GlobalLibraryWriterQueueError::Full)
    ));
    assert!(matches!(
        writer.submit_preparation(request(&client)),
        Err(GlobalLibraryWriterQueueError::Full)
    ));
    let copy = views[0].clone();
    drop(views.remove(0));
    assert!(matches!(
        clone.submit_preparation(request(&client)),
        Err(GlobalLibraryWriterQueueError::Full)
    ));
    drop(copy);
    release(prepare(&clone));
    drop(views);
    writer.shutdown();
}
#[test]
fn limited_requester_pending_drop_and_original_registration_cannot_survive_owner_restart() {
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let mut writer = start(&profile);
    let client = writer.wal_preparation_client();
    let old = client.preparation_owner();
    let held = prepare(&client);
    drop(client.submit_preparation(request(&client)).unwrap());
    release(held);
    writer
        .load_ui_preferences_payload()
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    // Lost pending delivery is cancelled by the original worker, then its original connection is usable.
    release(prepare(&client));
    writer.shutdown();
    let mut replacement = start(&profile);
    let current = replacement.wal_preparation_client();
    assert_ne!(
        old.registration(),
        current.preparation_owner().registration()
    );
    assert_eq!(old.acquisition(), current.preparation_owner().acquisition());
    let stale = GlobalPreparationRequest::new(
        old,
        [81; 32],
        Instant::now() + Duration::from_secs(2),
        1,
        4096,
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
    assert!(matches!(
        client.submit_preparation(request(&client)),
        Err(GlobalLibraryWriterQueueError::Closed)
    ));
    release(prepare(&current));
    replacement.shutdown();
}
