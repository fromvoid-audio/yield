//! Bounded child execution and numeric evidence for actual owned SQLite storage workloads.
#![cfg(all(test, target_os = "linux"))]
use super::LifecycleDatabaseAllocation;
use crate::sqlite_budget::{BudgetReport, OwnedConnection};
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub(crate) fn isolated(test: &str, case: &str) {
    // Fresh process facts and one immutable VFS attempt. Child output is small fixed metadata;
    // no native paths, payloads, identities, or alleged admission tokens are printed.
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            test.strip_prefix("yield_library::").unwrap_or(test),
            "--nocapture",
            "--test-threads=1",
        ])
        .env("YIELD_LIFECYCLE_VFS_WORKLOAD", case)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "bounded workload {case} timed out: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "workload {case}: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("running 1 test\n")
            && stdout.contains(&format!("logical-workload={case} ")),
        "child must execute its exact workload and emit evidence: {stdout}"
    );
    print!("{stdout}");
}
pub(crate) fn evidence(
    case: &str,
    connection: &OwnedConnection,
    allocation: &LifecycleDatabaseAllocation,
    report: BudgetReport,
    actual_shm: crate::sqlite_budget::wal_objects::WalObjectFacts,
) {
    let lengths = [
        crate::sqlite_wal::retained_database_bytes(connection).unwrap(),
        crate::sqlite_wal::retained_wal_bytes(connection).unwrap(),
    ];
    let objects = [allocation.main(), allocation.wal(), allocation.shm()];
    for (charged, object) in report.charged.iter().zip(objects) {
        assert!(*charged <= object.write_ceiling());
        assert_eq!(*charged % object.allocation_unit(), 0);
    }
    assert!(report.charged[2] >= allocation.shm().observed_bytes());
    assert_eq!(actual_shm.identity, allocation.shm().identity());
    assert_eq!(lengths[0], allocation.main().observed_bytes()); // No checkpoint during effect.
    assert!(lengths[1].saturating_sub(allocation.wal().observed_bytes()) <= report.charged[1]);
    assert!(
        actual_shm
            .bytes
            .saturating_sub(allocation.shm().observed_bytes())
            <= report.charged[2]
    );
    println!(
        "logical-workload={case} sqlite={} page_size={} pages={} selected_pages={} ceilings={:?} charged={:?} retained_main_wal={lengths:?} original_shm={} retained_shm={} failure={:?}",
        rusqlite::version(),
        allocation.page_size(),
        allocation.page_count(),
        allocation.selected_maximum_pages(),
        objects.map(|o| o.write_ceiling()),
        report.charged,
        allocation.shm().observed_bytes(),
        actual_shm.bytes,
        report.failure
    );
}
