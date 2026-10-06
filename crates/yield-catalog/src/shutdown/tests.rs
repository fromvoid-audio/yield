use super::*;
use crate::{CatalogCommand, CatalogSubmitError, SourceId, tests::Fixture};
use std::sync::{Arc, TryLockError, mpsc};
use std::time::{Duration, Instant};
use yield_library::app_dirs::WritableProfileGuard;

#[test]
fn shutdown_report_healthy_join_releases_original_profile_and_repeats() {
    let fixture = Fixture::new("catalog-thread-healthy");
    let service = CatalogService::start(&fixture.guard).unwrap();
    drop(fixture.guard);
    assert!(WritableProfileGuard::acquire_current().is_err());
    let report = service.shutdown_with_report();
    assert_eq!(report.outcome(), CatalogThreadJoinOutcome::Joined);
    assert!(report.is_confirmed_join());
    assert_eq!(service.shutdown_with_report(), report);
    assert_closed(&service);
    let profile = WritableProfileGuard::acquire_current().expect("original worker clone released");
    drop(profile);
}

#[test]
fn shutdown_report_poisoned_join_control_still_joins_original_worker() {
    let fixture = Fixture::new("catalog-thread-join-poison");
    let service = CatalogService::start(&fixture.guard).unwrap();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _original_handle = service.worker.lock().unwrap();
            panic!("poison original join mutex while retaining handle");
        }))
        .is_err()
    );
    drop(fixture.guard);
    let report = service.shutdown_with_report();
    assert_eq!(report.outcome(), CatalogThreadJoinOutcome::Joined);
    assert!(report.control_poisoned());
    assert!(!report.is_confirmed_join());
    assert_eq!(service.shutdown_with_report(), report);
    assert_closed(&service);
    let profile =
        WritableProfileGuard::acquire_current().expect("poison did not skip original join");
    drop(profile);
}

#[test]
fn shutdown_report_poisoned_admission_still_joins_and_retains_failure() {
    let fixture = Fixture::new("catalog-thread-admission-poison");
    let service = CatalogService::start(&fixture.guard).unwrap();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _original_control = service.admission.lock().unwrap();
            panic!("poison original admission control");
        }))
        .is_err()
    );
    drop(fixture.guard);
    let report = service.shutdown_with_report();
    assert_eq!(report.outcome(), CatalogThreadJoinOutcome::Joined);
    assert!(report.control_poisoned());
    assert!(!report.is_confirmed_join());
    assert_eq!(service.shutdown_with_report(), report);
    assert_closed(&service);
    let profile = WritableProfileGuard::acquire_current().expect("original worker still joined");
    drop(profile);
}

#[test]
fn shutdown_report_concurrent_callers_wait_for_same_original_join() {
    let fixture = Fixture::new("catalog-thread-concurrent");
    let (service, gate) = CatalogService::start_with_test_gate(&fixture.guard).unwrap();
    let pause = gate.pause_next();
    let service = Arc::new(service);
    drop(fixture.guard);
    let (done, results) = mpsc::channel();
    let first_service = Arc::clone(&service);
    let first_done = done.clone();
    let first = std::thread::spawn(move || {
        first_done
            .send(first_service.shutdown_with_report())
            .unwrap()
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(service.worker.try_lock(), Err(TryLockError::WouldBlock)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "first caller did not begin original join"
        );
        std::thread::yield_now();
    }
    assert_closed(&service);
    assert!(WritableProfileGuard::acquire_current().is_err());
    let second_service = Arc::clone(&service);
    let (started, start) = mpsc::channel();
    let second = std::thread::spawn(move || {
        started.send(()).unwrap();
        done.send(second_service.shutdown_with_report()).unwrap();
    });
    start.recv_timeout(Duration::from_secs(2)).unwrap();
    let premature = results.recv_timeout(Duration::from_millis(50));
    drop(pause); // Release the genuine worker even if the timing assertion fails.
    first.join().unwrap();
    second.join().unwrap();
    assert!(matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)));
    let first_report = results.recv_timeout(Duration::from_secs(2)).unwrap();
    let second_report = results.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(first_report.is_confirmed_join());
    assert_eq!(first_report, second_report);
    assert_eq!(service.shutdown_with_report(), first_report);
    let profile =
        WritableProfileGuard::acquire_current().expect("concurrent original join completed");
    drop(profile);
}

#[cfg(feature = "test-support")]
#[test]
fn shutdown_report_original_worker_panic_is_retained_without_control_poison() {
    let fixture = Fixture::new("catalog-thread-worker-panic");
    let service = CatalogService::start(&fixture.guard).unwrap();
    let fault = service.arm_original_worker_panic().unwrap();
    assert!(fault.wait_entered(Duration::from_secs(5)));
    assert!(matches!(
        service.arm_original_worker_panic(),
        Err(crate::test_support::WorkerPanicError::Occupied)
    ));
    drop(fixture.guard);
    assert!(WritableProfileGuard::acquire_current().is_err());
    fault.release();
    let report = service.shutdown_with_report();
    assert_eq!(report.outcome(), CatalogThreadJoinOutcome::WorkerPanicked);
    assert!(!report.control_poisoned());
    assert!(!report.is_confirmed_join());
    assert_eq!(service.shutdown_with_report(), report);
    assert_closed(&service);
    assert!(matches!(
        service.arm_original_worker_panic(),
        Err(crate::test_support::WorkerPanicError::Closed)
    ));
    let profile = WritableProfileGuard::acquire_current().expect("panicked original worker joined");
    drop(profile);
}

fn assert_closed(service: &CatalogService) {
    assert_eq!(
        service.submit(CatalogCommand::ReadSnapshot {
            source_id: SourceId::from_string("closed-catalog-thread"),
            expected_revision: None,
        }),
        Err(CatalogSubmitError::Closed)
    );
}
