//! Normal production bootstrap stays fail closed on unqualified Linux volumes.
#![cfg(all(target_os = "linux", not(feature = "test-support")))]
use std::time::{Duration, Instant};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    operation_journal::{
        OperationJournalStore,
        owner::{
            OperationJournalOwner, OperationJournalOwnerQueueError, OperationJournalOwnerStatus,
        },
    },
};

#[test]
fn normal_unqualified_bootstrap_cannot_issue_wal_factory_memory_token() {
    let base = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(base.path().join("profile")).unwrap();
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let store = OperationJournalStore::open(&guard).unwrap();
    let mut journal = OperationJournalOwner::start(store).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match journal.status() {
            OperationJournalOwnerStatus::Initializing => {
                assert!(
                    Instant::now() < deadline,
                    "production bootstrap did not resolve"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            OperationJournalOwnerStatus::Unavailable { .. } => break,
            status => panic!("unqualified Linux bootstrap unexpectedly usable: {status:?}"),
        }
    }
    assert!(matches!(
        journal.client().reserve_wal_preparation(),
        Err(OperationJournalOwnerQueueError::Unavailable { .. })
    ));
    assert!(!journal.shutdown());
}
