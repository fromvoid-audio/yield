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
