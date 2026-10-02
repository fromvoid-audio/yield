use super::*;
use std::time::{Duration, Instant};

#[test]
fn queue_observations_cover_actual_timeout_full_dispatch_and_retained_custody() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = SourceWriter::new(
        fixture.database,
        SourceId::from_string("snapshot-metrics"),
        7,
    )
    .unwrap();
    let expected = writer.current_revision().unwrap();
    let owner = SourceSnapshotOwner::start(writer).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    owner.block_for_tests(entered_tx, release_rx);
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(owner.queue_metrics(), Default::default());
    assert!(matches!(
        owner.read_browser_metadata_page_with_deadline(
            &expected,
            None,
            1,
            Instant::now() + Duration::from_millis(100)
        ),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    let waiting = owner.queue_metrics();
    assert_eq!(
        (
            waiting.attempts,
            waiting.accepted,
            waiting.pending_dispatch,
            waiting.dequeued
        ),
        (1, 1, 1, 0)
    );
    // A caller timeout must not refund already accepted work or release the source writer.
    assert!(matches!(
        owner.read_browser_metadata_page(&expected, None, 1),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    let rejected = owner.queue_metrics();
    assert_eq!(
        (
            rejected.attempts,
            rejected.accepted,
            rejected.full,
            rejected.pending_dispatch
        ),
        (2, 1, 1, 1)
    );
    release_tx.send(()).unwrap();
    owner.wait_idle_for_tests();
    let expired = owner.queue_metrics();
    assert_eq!((expired.dequeued, expired.pending_dispatch), (1, 0));
    assert_eq!(expired.dispatch_wait_buckets.iter().sum::<u64>(), 1);
    assert!(expired.dispatch_wait_micros >= 80_000);
    assert!(matches!(
        owner
            .read_browser_metadata_page(&expected, None, 1)
            .unwrap(),
        SourceSnapshotPage::FreshAsOf { .. }
    ));
    let ready = owner.queue_metrics();
    assert_eq!(
        (
            ready.attempts,
            ready.accepted,
            ready.full,
            ready.dequeued,
            ready.pending_dispatch
        ),
        (3, 2, 1, 2, 0)
    );
    assert_eq!(ready.dispatch_wait_buckets.iter().sum::<u64>(), 2);
    let writer = owner.begin_retirement().join().unwrap();
    assert_eq!(writer.current_revision().unwrap(), expected);
    let owner = SourceSnapshotOwner::start(writer).unwrap();
    assert_eq!(
        owner.queue_metrics(),
        Default::default(),
        "new worker observations start empty"
    );
    owner.begin_retirement().join().unwrap().close().unwrap();
}
