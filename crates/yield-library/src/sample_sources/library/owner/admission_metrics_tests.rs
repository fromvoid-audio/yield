use super::*;
use crate::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
use crate::sample_sources::library::owner::*;
use crate::test_runtime::TestRuntimeGuard;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

#[test]
fn fixed_buckets_and_saturating_counters() {
    let metrics = AdmissionMetrics::default();
    for (micros, expected) in [
        (0, 0),
        (999, 0),
        (1000, 1),
        (9999, 1),
        (10000, 2),
        (99999, 2),
        (100000, 3),
        (999999, 3),
        (1000000, 4),
    ] {
        assert_eq!(wait_bucket(Duration::from_micros(micros)), expected);
        metrics.record_wait(Duration::from_micros(micros));
    }
    assert_eq!(metrics.snapshot().gate_wait_buckets, [2, 2, 2, 2, 1]);
    metrics.outcomes[0].store(u64::MAX - 1, Ordering::Relaxed);
    metrics.record_outcome(&Ok(()));
    metrics.record_outcome(&Ok(()));
    assert_eq!(metrics.snapshot().accepted, u64::MAX);
    metrics.record_outcome(&Err(GlobalLibraryWriterQueueError::InvalidMutationRoute));
    assert_eq!(metrics.snapshot().full, 0);
}

#[test]
fn production_queue_saturation_and_client_share_exact_counts() {
    let mut writer = GlobalLibraryWriter::disabled();
    let client = writer.profile_tag_vocabulary_client();
    let clone = client.clone();
    assert!(
        clone
            .lookup_profile_tag_categories(vec!["x".repeat(4096)])
            .is_err()
    );
    assert_eq!(
        writer.admission_metrics(),
        GlobalAdmissionMetricsSnapshot::default()
    );
    for _ in 0..GLOBAL_LIBRARY_WRITER_COMMAND_CAPACITY {
        writer.load_source_registry_snapshot().unwrap();
    }
    assert!(matches!(
        clone.lookup_profile_tag_categories(vec![]),
        Err(GlobalLibraryWriterQueueError::Full)
    ));
    let snapshot = writer.admission_metrics();
    assert_eq!(
        snapshot.accepted,
        GLOBAL_LIBRARY_WRITER_COMMAND_CAPACITY as u64
    );
    assert_eq!(snapshot.full, 1);
    assert_eq!(
        snapshot.gate_wait_buckets.iter().sum::<u64>(),
        snapshot.accepted + 1
    );
    assert_eq!(snapshot, clone.admission_metrics());
    writer.begin_shutdown();
    assert!(matches!(
        clone.lookup_profile_tag_categories(vec![]),
        Err(GlobalLibraryWriterQueueError::Closed)
    ));
    assert_eq!(clone.admission_metrics().closed, 1);
    assert_eq!(
        clone.admission_metrics().gate_wait_buckets,
        snapshot.gate_wait_buckets
    );
    assert_eq!(
        GlobalLibraryWriter::disabled().admission_metrics(),
        GlobalAdmissionMetricsSnapshot::default()
    );
}

#[test]
fn disconnected_receiver_is_closed_after_one_actual_gate_attempt() {
    let mut writer = GlobalLibraryWriter::disabled();
    writer.blocked_receiver.take();
    assert!(matches!(
        writer.load_source_registry_snapshot(),
        Err(GlobalLibraryWriterQueueError::Closed)
    ));
    assert_eq!(writer.admission_metrics().closed, 1);
    assert_eq!(writer.admission_metrics().accepted, 0);
    assert_eq!(
        writer
            .admission_metrics()
            .gate_wait_buckets
            .iter()
            .sum::<u64>(),
        1
    );
}

#[test]
fn gate_revalidation_rejects_closed_lifecycle_once() {
    let writer = GlobalLibraryWriter::disabled();
    let client = writer.profile_tag_vocabulary_client();
    // The initial public-request availability check passed; shutdown then closes
    // the lifecycle before the existing send gate revalidates it.
    client.ensure_available().unwrap();
    writer
        .lifecycle
        .store(WriterLifecycle::Closing as u8, Ordering::Release);
    let (result, _) = std::sync::mpsc::sync_channel(1);
    assert!(matches!(
        client.send(WriterCommand::LoadSourceRegistry { result }),
        Err(GlobalLibraryWriterQueueError::Closed)
    ));
    assert_eq!(writer.admission_metrics().closed, 1);
    assert_eq!(writer.admission_metrics().accepted, 0);
    assert_eq!(
        writer
            .admission_metrics()
            .gate_wait_buckets
            .iter()
            .sum::<u64>(),
        1
    );
}

#[test]
fn lifecycle_rejections_do_not_export_reasons_or_fabricate_gate_wait() {
    let writer = GlobalLibraryWriter::disabled();
    writer
        .lifecycle
        .store(WriterLifecycle::Initializing as u8, Ordering::Release);
    assert!(matches!(
        writer.load_source_registry_snapshot(),
        Err(GlobalLibraryWriterQueueError::Initializing)
    ));
    *writer.unavailable.lock().unwrap() =
        Some(GlobalLibraryWriterUnavailable::DatabaseUnavailable {
            reason: "private/path/password".into(),
        });
    writer
        .lifecycle
        .store(WriterLifecycle::Unavailable as u8, Ordering::Release);
    assert!(matches!(
        writer.load_source_registry_snapshot(),
        Err(GlobalLibraryWriterQueueError::Unavailable(_))
    ));
    let snapshot = writer.admission_metrics();
    assert_eq!(snapshot.initializing, 1);
    assert_eq!(snapshot.unavailable, 1);
    assert_eq!(snapshot.gate_wait_buckets, [0; 5]);
    assert!(!format!("{snapshot:?}").contains("private"));
}

#[test]
fn real_owner_admission_is_not_sql_completion_and_survives_shutdown() {
    let _runtime = TestRuntimeGuard::acquire();
    let temp = tempfile::tempdir().unwrap();
    let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
    let _profile = PersistenceProfileGuard::named("admission-metrics");
    let guard = WritableProfileGuard::acquire_current().unwrap();
    let (mut writer, ready, release) =
        GlobalLibraryWriter::start_with_post_command_gate_for_test(&guard);
    let deadline = Instant::now() + Duration::from_secs(5);
    while writer.status() != GlobalLibraryWriterStatus::Available {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    let client = writer.profile_tag_vocabulary_client();
    let result = client.lookup_profile_tag_categories(vec![]).unwrap();
    ready.wait();
    assert_eq!(writer.admission_metrics().accepted, 1);
    assert!(matches!(
        result.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    release.wait();
    assert!(result.recv_timeout(Duration::from_secs(5)).unwrap().is_ok());
    writer.shutdown_with_report();
    assert!(matches!(
        client.lookup_profile_tag_categories(vec![]),
        Err(GlobalLibraryWriterQueueError::Closed)
    ));
    assert_eq!(client.admission_metrics().accepted, 1);
    assert_eq!(client.admission_metrics().closed, 1);
}

#[test]
fn actual_admission_gate_contention_records_one_wait_without_replaying_request() {
    let writer = GlobalLibraryWriter::disabled();
    let gate = writer.admission_gate.lock().unwrap();
    let ready = Arc::new(Barrier::new(2));
    let queued = thread::spawn({
        let writer = writer.profile_tag_vocabulary_client();
        let ready = Arc::clone(&ready);
        move || {
            ready.wait();
            writer.lookup_profile_tag_categories(vec![]).unwrap();
        }
    });
    ready.wait();
    // The mutex is held while the caller attempts admission; timing bucket itself
    // is not asserted because OS scheduling may delay that caller until release.
    assert_eq!(writer.admission_metrics().accepted, 0);
    drop(gate);
    queued.join().unwrap();
    assert_eq!(writer.admission_metrics().accepted, 1);
    assert_eq!(
        writer
            .admission_metrics()
            .gate_wait_buckets
            .iter()
            .sum::<u64>(),
        1
    );
}
