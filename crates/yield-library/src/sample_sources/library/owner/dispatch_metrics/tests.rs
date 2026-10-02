use super::*;
use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
use crate::operation_journal::StoredRelativePath;
use crate::sample_sources::library::{
    GlobalLibraryWriter, GlobalLibraryWriterQueueError, GlobalLibraryWriterStatus,
    HarvestFactsCancellation, HarvestFactsRequest,
};
use crate::sample_sources::{SourceDatabase, SourceId, SourceWriter};
use crate::test_runtime::TestRuntimeGuard;
use std::{path::Path, sync::mpsc, thread};

fn command(metrics: &Arc<DispatchMetrics>) -> QueuedCommand {
    let (result, _) = mpsc::sync_channel(1);
    QueuedCommand::new(
        WriterCommand::LoadSourceRegistry { result },
        Arc::clone(metrics),
    )
}

#[test]
fn fixed_numeric_storage_saturates_and_buckets_cover_boundaries() {
    let metrics = DispatchMetrics::default();
    for millis in [0, 1, 9, 10, 99, 100, 999, 1000, u64::MAX] {
        metrics.record(Duration::from_millis(millis));
    }
    assert_eq!(metrics.snapshot().wait_buckets, [1, 2, 2, 2, 2]);
    assert_eq!(metrics.snapshot().dequeued, 9);
    for counter in std::iter::once(&metrics.dequeued).chain(&metrics.waits) {
        counter.store(u64::MAX - 1, Ordering::Relaxed);
        increment(counter);
        increment(counter);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }
    assert_eq!(std::mem::size_of::<GlobalDispatchMetricsSnapshot>(), 6 * 8);
    assert_eq!(std::mem::size_of::<DispatchMetrics>(), 6 * 8);
}

#[test]
fn real_bounded_channel_rejection_discard_and_unvalidated_dequeue_are_excluded() {
    let metrics = Arc::new(DispatchMetrics::default());
    let (send, receive) = mpsc::sync_channel(1);
    assert!(send.try_send(command(&metrics)).is_ok());
    assert!(matches!(
        send.try_send(command(&metrics)),
        Err(mpsc::TrySendError::Full(_))
    ));
    drop(
        receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .discard(),
    );
    assert_eq!(metrics.snapshot(), GlobalDispatchMetricsSnapshot::default());
    assert!(send.try_send(command(&metrics)).is_ok());
    let (_, pending) = receive
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .received();
    drop(pending); // Represents failed profile revalidation, not normal dispatch.
    drop(receive);
    assert!(matches!(
        send.try_send(command(&metrics)),
        Err(mpsc::TrySendError::Disconnected(_))
    ));
    assert_eq!(metrics.snapshot(), GlobalDispatchMetricsSnapshot::default());
}

#[test]
fn actual_queued_delay_is_captured_before_sql_or_profile_validation_delay() {
    let metrics = Arc::new(DispatchMetrics::default());
    let (send, receive) = mpsc::sync_channel(1);
    assert!(send.try_send(command(&metrics)).is_ok());
    thread::sleep(Duration::from_millis(20));
    let (_, pending) = receive
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .received();
    let captured = pending.wait;
    assert!(captured >= Duration::from_millis(20));
    thread::sleep(Duration::from_millis(20));
    assert_eq!(pending.wait, captured);
    assert_eq!(metrics.snapshot().dequeued, 0);
    pending.record();
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.dequeued, 1);
    assert_eq!(snapshot.wait_buckets.iter().sum::<u64>(), 1);
    assert_eq!(&snapshot.wait_buckets[..2], &[0, 0]);
}

#[test]
fn production_queue_keeps_capacity_and_discard_does_not_become_dispatch() {
    let mut writer = GlobalLibraryWriter::disabled();
    let client = writer.profile_tag_vocabulary_client();
    for _ in 0..super::super::GLOBAL_LIBRARY_WRITER_COMMAND_CAPACITY {
        writer.load_source_registry_snapshot().unwrap();
    }
    assert!(matches!(
        client.lookup_profile_tag_categories(vec![]),
        Err(GlobalLibraryWriterQueueError::Full)
    ));
    assert_eq!(
        writer.dispatch_metrics(),
        GlobalDispatchMetricsSnapshot::default()
    );
    writer.begin_shutdown();
    assert_eq!(
        writer.dispatch_metrics(),
        GlobalDispatchMetricsSnapshot::default()
    );
    assert!(matches!(
        client.lookup_profile_tag_categories(vec![]),
        Err(GlobalLibraryWriterQueueError::Closed)
    ));
}

#[test]
fn real_registry_vocabulary_and_source_fenced_harvest_routes_record_dequeue_once() {
    let _runtime = TestRuntimeGuard::acquire();
    let base = tempfile::tempdir().unwrap();
    let _config = ConfigBaseGuard::set(base.path().into());
    let _profile = PersistenceProfileGuard::named("global-dispatch");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let mut writer = GlobalLibraryWriter::start(&guard).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while writer.status() != GlobalLibraryWriterStatus::Available {
        assert!(Instant::now() < deadline, "{:?}", writer.status());
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        writer.dispatch_metrics(),
        GlobalDispatchMetricsSnapshot::default()
    );
    writer
        .load_source_registry_snapshot()
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    writer
        .profile_tag_vocabulary_client()
        .lookup_profile_tag_categories(vec![])
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();

    // Actual profile/source lease, SQLite manifest audit and sealed observation; no grant/model.
    let source_root = tempfile::tempdir().unwrap();
    let db = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &guard,
        Path::new("dispatch-source"),
    )
    .unwrap();
    let mut batch = db.write_batch().unwrap();
    batch
        .upsert_file_with_hash(Path::new("one.wav"), 10, 1, &"a".repeat(64))
        .unwrap();
    batch
        .set_file_identity(Path::new("one.wav"), Some("object-dispatch"))
        .unwrap();
    batch.commit().unwrap();
    db.begin_source_directory_truth_generation(1, 0).unwrap();
    let source =
        SourceWriter::new_for_scan(db, SourceId::from_string("dispatch-source"), 1).unwrap();
    let fence = source.current_scan_fence().unwrap();
    source
        .catalog_database()
        .write_batch()
        .unwrap()
        .commit_manifest_audit_with_staged_directory_truth(
            1,
            fence.revision(),
            10,
            fence.root_identity().clone(),
            None,
        )
        .unwrap();
    let observed = source
        .observe_file_identity(StoredRelativePath::new("one.wav").unwrap())
        .unwrap();
    let request =
        HarvestFactsRequest::new(&guard, vec![observed], HarvestFactsCancellation::new()).unwrap();
    writer
        .read_harvest_facts(request)
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    let snapshot = writer.dispatch_metrics();
    assert_eq!(snapshot.dequeued, 3);
    assert_eq!(snapshot.wait_buckets.iter().sum::<u64>(), 3);
    assert!(writer.shutdown());
    assert_eq!(writer.dispatch_metrics(), snapshot);
}
