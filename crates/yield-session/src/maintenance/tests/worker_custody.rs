use super::*;
use std::time::Duration;
use tempfile::TempDir;
use yield_library::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard};

#[test]
fn maintenance_original_predecessor_cannot_be_detached_by_successor_shutdown() {
    let temp = TempDir::new().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("maintenance-predecessor-custody");
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let admission = Arc::new(MaintenanceAdmission::new());
    let mut service = MaintenanceService::new(Arc::clone(&admission));
    let first_gate = service.arm_completion_for_test(false);
    let first = admission.reserve(MaintenanceKind::StorageUsage).unwrap();
    let (first_tx, first_rx) = mpsc::sync_channel(1);
    service.start_storage(Some(&profile), first, first_tx);
    assert!(first_gate.wait_entered(Duration::from_secs(5)));
    let _first_result = first_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let second = admission.reserve(MaintenanceKind::StorageUsage).unwrap();
    let (second_tx, second_rx) = mpsc::sync_channel(1);
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let owner = thread::spawn(move || {
        service.start_storage(Some(&profile), second, second_tx);
        drop(profile);
        service.shutdown();
        finished_tx.send(()).unwrap();
    });
    let completed_before_original_exit = finished_rx.recv_timeout(Duration::from_millis(150));
    assert!(WritableProfileGuard::acquire_current().is_err());
    first_gate.release();
    owner.join().unwrap();
    let _second_result = second_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        matches!(
            completed_before_original_exit,
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "successor shutdown must wait for the original predecessor: {completed_before_original_exit:?}"
    );
    let profile = WritableProfileGuard::acquire_current().expect("both original workers joined");
    drop(profile);
}

#[test]
fn maintenance_successor_reservation_survives_original_predecessor_join() {
    let temp = TempDir::new().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("maintenance-successor-reservation");
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let admission = Arc::new(MaintenanceAdmission::new());
    let mut service = MaintenanceService::new(Arc::clone(&admission));
    let first_gate = service.arm_completion_for_test(false);
    let (first_tx, first_rx) = mpsc::sync_channel(1);
    service.start_storage(
        Some(&profile),
        admission.reserve(MaintenanceKind::StorageUsage).unwrap(),
        first_tx,
    );
    assert!(first_gate.wait_entered(Duration::from_secs(5)));
    let _first_result = first_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let second_gate = service.arm_completion_for_test(false);
    let second = admission.reserve(MaintenanceKind::StorageUsage).unwrap();
    let second_id = second.id;
    let (second_tx, second_rx) = mpsc::sync_channel(1);
    let owner = thread::spawn(move || {
        service.start_storage(Some(&profile), second, second_tx);
        drop(profile);
        service
    });
    assert!(
        !second_gate.wait_entered(Duration::from_millis(150)),
        "successor cannot execute before original predecessor join"
    );
    assert!(admission.reserve(MaintenanceKind::CacheClear).is_none());
    first_gate.release();
    assert!(second_gate.wait_entered(Duration::from_secs(5)));
    let mut service = owner.join().unwrap();
    let _second_result = second_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(service.active.as_ref().unwrap().id, second_id);
    // The second operation has released admission, but its original handle/custody remains.
    assert!(WritableProfileGuard::acquire_current().is_err());
    second_gate.release();
    assert!(service.shutdown());
    assert!(service.shutdown());
    let profile = WritableProfileGuard::acquire_current().expect("both original handles joined");
    drop(profile);
}

#[test]
fn maintenance_original_join_panic_rejects_unstarted_successor_and_remains_sticky() {
    let temp = TempDir::new().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("maintenance-original-panic");
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let admission = Arc::new(MaintenanceAdmission::new());
    let mut service = MaintenanceService::new(Arc::clone(&admission));
    let gate = service.arm_completion_for_test(true);
    let (first_tx, first_rx) = mpsc::sync_channel(1);
    service.start_storage(
        Some(&profile),
        admission.reserve(MaintenanceKind::StorageUsage).unwrap(),
        first_tx,
    );
    assert!(gate.wait_entered(Duration::from_secs(5)));
    let _first_result = first_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let second = admission.reserve(MaintenanceKind::StorageUsage).unwrap();
    gate.release();
    let (second_tx, second_rx) = mpsc::sync_channel(1);
    service.start_storage(Some(&profile), second, second_tx);
    assert_eq!(
        second_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        Err(MaintenanceError::WorkerPanicked)
    );
    assert!(
        service.active.is_none(),
        "no successor thread/effect was started"
    );
    let third = admission
        .reserve(MaintenanceKind::CacheClear)
        .expect("only refused successor reservation released");
    let (third_tx, third_rx) = mpsc::sync_channel(1);
    service.start_clear(Some(&profile), &[], None, third, third_tx);
    assert_eq!(
        third_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        Err(MaintenanceError::WorkerPanicked)
    );
    assert!(admission.reserve(MaintenanceKind::StorageUsage).is_some());
    drop(profile);
    assert!(!service.shutdown());
    assert!(!service.shutdown());
    let profile =
        WritableProfileGuard::acquire_current().expect("panicked original was actually joined");
    drop(profile);
}

#[test]
fn maintenance_original_cancelled_operation_does_not_become_thread_join_failure() {
    let temp = TempDir::new().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("maintenance-original-cancelled");
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let admission = Arc::new(MaintenanceAdmission::new());
    let mut service = MaintenanceService::new(Arc::clone(&admission));
    let reservation = admission.reserve(MaintenanceKind::StorageUsage).unwrap();
    reservation.cancel.store(true, Ordering::Release);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    service.start_storage(Some(&profile), reservation, result_tx);
    assert_eq!(
        result_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        Err(MaintenanceError::Cancelled)
    );
    assert!(service.shutdown());
    assert!(service.shutdown());
    assert!(admission.reserve(MaintenanceKind::CacheClear).is_some());
}

#[cfg(unix)]
#[test]
fn maintenance_original_cache_predecessor_is_joined_before_storage_successor() {
    let temp = TempDir::new().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("maintenance-cache-predecessor");
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let admission = Arc::new(MaintenanceAdmission::new());
    let mut service = MaintenanceService::new(Arc::clone(&admission));
    let gate = service.arm_completion_for_test(false);
    let (first_tx, first_rx) = mpsc::sync_channel(1);
    service.start_clear(
        Some(&profile),
        &[],
        None,
        admission.reserve(MaintenanceKind::CacheClear).unwrap(),
        first_tx,
    );
    assert!(gate.wait_entered(Duration::from_secs(5)));
    let outcome = first_rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .expect("real original empty cache operation");
    assert_eq!(outcome.removed_files, 0);
    let second = admission.reserve(MaintenanceKind::StorageUsage).unwrap();
    let (second_tx, second_rx) = mpsc::sync_channel(1);
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let owner = thread::spawn(move || {
        service.start_storage(Some(&profile), second, second_tx);
        drop(profile);
        assert!(service.shutdown());
        finished_tx.send(()).unwrap();
    });
    let early = finished_rx.recv_timeout(Duration::from_millis(150));
    gate.release();
    owner.join().unwrap();
    let _second_result = second_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        matches!(early, Err(mpsc::RecvTimeoutError::Timeout)),
        "original cache namespace custody cannot be detached: {early:?}"
    );
    let profile =
        WritableProfileGuard::acquire_current().expect("original cache and storage threads joined");
    drop(profile);
}
