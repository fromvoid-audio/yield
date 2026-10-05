//! Exercise the production composition against the original scalar admission.
//! Callback sentinels verify custody/ordering only; they confer no source/native authority.
use super::super::resource_custody::UseError;
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn original_grant_custody_closed_unknown_and_full_refuse_before_source_acquisition() {
    for unknown in [false, true] {
        let original = ResourceAdmission::new();
        assert!(original.activate(|| true));
        let issued = original.clone();
        if unknown {
            original.invalidate();
        } else {
            original.close();
        }
        assert!(
            counted_entry(&issued, || -> Result<(), OperationJournalOwnerError> {
                panic!("source acquisition after original admission closure")
            })
            .is_err()
        );
        assert_eq!(original.snapshot().1, 0);
        assert_eq!(
            original.validate_open(),
            Err(if unknown {
                UseError::Unknown
            } else {
                UseError::Closed
            })
        );
        assert_eq!(original.close_and_wait(), !unknown);
    }
    let original = ResourceAdmission::new();
    assert!(original.activate(|| true));
    let retained: Vec<_> = (0..32)
        .map(|_| original.begin_read(|| true).unwrap())
        .collect();
    assert!(
        counted_entry(&original, || -> Result<(), OperationJournalOwnerError> {
            panic!("source acquisition when original use ceiling is full")
        })
        .is_err()
    );
    assert_eq!(original.snapshot().1, 32);
    drop(retained);
    assert!(original.close_and_wait());
}

#[test]
fn original_grant_custody_source_failure_and_close_during_acquisition_release_without_entry() {
    let original = ResourceAdmission::new();
    assert!(original.activate(|| true));
    assert!(
        counted_entry(&original, || -> Result<(), OperationJournalOwnerError> {
            assert_eq!(original.snapshot().1, 1);
            Err(OperationJournalOwnerError::StagingGrant {
                reason: "source acquisition refused".into(),
            })
        })
        .is_err()
    );
    assert_eq!(original.snapshot(), (false, 0, false));
    struct SourceSentinel(Arc<ResourceAdmission>, Arc<AtomicBool>);
    impl Drop for SourceSentinel {
        fn drop(&mut self) {
            assert_eq!(
                self.0.snapshot().1,
                1,
                "source releases while runtime remains retained"
            );
            self.1.store(true, Ordering::Release);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    assert!(
        counted_entry(&original, || {
            original.close();
            Ok(SourceSentinel(original.clone(), dropped.clone()))
        })
        .is_err()
    );
    assert!(dropped.load(Ordering::Acquire));
    assert_eq!(original.snapshot(), (true, 0, false));
    assert!(original.close_and_wait());
}

#[test]
fn original_grant_custody_entry_preserves_count_and_teardown_waits_without_settlement() {
    let original = ResourceAdmission::new();
    assert!(original.activate(|| true));
    struct SourceSentinel(Arc<ResourceAdmission>);
    impl Drop for SourceSentinel {
        fn drop(&mut self) {
            assert_eq!(
                self.0.snapshot().1,
                1,
                "source drops before original runtime use"
            );
        }
    }
    let entered = counted_entry(&original, || {
        assert_eq!(original.snapshot().1, 1);
        Ok(SourceSentinel(original.clone()))
    })
    .unwrap();
    assert_eq!(original.snapshot(), (false, 1, false));
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let closer = scope.spawn(|| {
            original.close();
            tx.send(()).unwrap();
            assert!(
                !original.close_and_wait(),
                "entered Drop never settles or makes clean handoff"
            );
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            counted_entry(&original, || -> Result<(), OperationJournalOwnerError> {
                panic!("new source entry after closing")
            })
            .is_err()
        );
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
        drop(entered);
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        closer.join().unwrap();
    });
    assert_eq!(original.snapshot(), (true, 0, true));
    assert!(!original.activate(|| true));
}
