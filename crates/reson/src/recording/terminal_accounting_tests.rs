use std::sync::mpsc;

fn partition(
    health: &RecordingHealthState,
    accepted: u64,
    encoded: u64,
    abandoned: u64,
    unconfirmed: u64,
) {
    let snapshot = health.snapshot();
    assert!(snapshot.writer_accounting_complete);
    assert!(!snapshot.writer_accounting_overflowed);
    assert_eq!(snapshot.writer_accepted_samples, accepted);
    assert_eq!(snapshot.writer_encoded_samples, encoded);
    assert_eq!(snapshot.writer_abandoned_samples, abandoned);
    assert_eq!(snapshot.writer_unconfirmed_samples, unconfirmed);
    assert_eq!(accepted, encoded + abandoned + unconfirmed);
}

#[test]
fn terminal_accounting_stop_drains_and_cancel_counts_the_actual_queued_ring() {
    for cancelled in [false, true] {
        let health = Arc::new(RecordingHealthState::default());
        let settings = limits(2, 10);
        let (mut capture, consumer) = RecorderWriter::prepare_capture(settings, health.clone());
        capture.submit(&[0.25, -0.25, 0.5, -0.5]);
        capture.mode.store(
            if cancelled { CANCELLED } else { STOPPED },
            Ordering::Release,
        );
        let output = SharedSink::default();
        let mut writer = RecorderWriter::spawn_prepared(
            Box::new(output.clone()),
            settings,
            consumer,
            capture.mode.clone(),
            health.clone(),
        )
        .unwrap();
        let stats = writer.join().unwrap();
        if cancelled {
            assert_eq!(stats.state, RecordingPrefixState::CancelledRetained);
            assert_eq!(stats.frames, 0);
            partition(&health, 4, 0, 4, 0);
            assert!(samples(&output).is_empty());
        } else {
            assert_eq!(stats.state, RecordingPrefixState::StoppedUnpublished);
            assert_eq!(stats.frames, 2);
            partition(&health, 4, 4, 0, 0);
            assert_eq!(samples(&output), [0.25, -0.25, 0.5, -0.5]);
        }
        capture.submit(&[1.0, -1.0]);
        assert_eq!(health.snapshot().writer_accepted_samples, 4);
        assert_eq!(health.snapshot().writer_dropped_samples, 2);
    }
}

#[test]
fn terminal_accounting_settles_a_callback_that_already_passed_running() {
    for cancelled in [false, true] {
        let health = Arc::new(RecordingHealthState::default());
        let settings = limits(1, 10);
        let (mut capture, consumer) = RecorderWriter::prepare_capture(settings, health.clone());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        capture.submission_hook = Some(Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        let mode = capture.mode.clone();
        let callback = thread::spawn(move || {
            capture.submit(&[0.25, 0.5]);
            capture
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(health.capture_in_flight.load(Ordering::SeqCst));
        let output = SharedSink::default();
        let mut writer = RecorderWriter::spawn_prepared(
            Box::new(output.clone()),
            settings,
            consumer,
            mode,
            health.clone(),
        )
        .unwrap();
        if cancelled {
            writer.cancel();
        } else {
            writer.stop();
        }
        let (joined_tx, joined_rx) = mpsc::channel();
        let worker = thread::spawn(move || joined_tx.send(writer.join()).unwrap());
        assert!(matches!(
            joined_rx.recv_timeout(Duration::from_millis(25)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(!health.snapshot().writer_accounting_complete);
        release_tx.send(()).unwrap();
        let mut capture = callback.join().unwrap();
        let stats = joined_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
        if cancelled {
            partition(&health, 2, 0, 2, 0);
            assert_eq!(stats.frames, 0);
            assert!(samples(&output).is_empty());
        } else {
            partition(&health, 2, 2, 0, 0);
            assert_eq!(stats.frames, 2);
            assert_eq!(samples(&output), [0.25, 0.5]);
        }
        capture.submit(&[0.75]);
        assert_eq!(health.snapshot().writer_accepted_samples, 2);
        assert_eq!(health.snapshot().writer_dropped_samples, 1);
    }
}

#[test]
fn terminal_accounting_failed_batch_and_unattempted_backlog_are_distinct() {
    let health = Arc::new(RecordingHealthState::default());
    let settings = limits(1, 1);
    let (mut capture, consumer) = RecorderWriter::prepare_capture(settings, health.clone());
    capture.submit(&vec![0.25; WRITER_DRAIN_SAMPLES + 2]);
    let mut writer = RecorderWriter::spawn_prepared(
        Box::new(SharedSink::default()),
        settings,
        consumer,
        capture.mode.clone(),
        health.clone(),
    )
    .unwrap();
    assert!(writer.join().is_err());
    partition(&health, 4098, 0, 2, 4096);
    assert!(health.snapshot().writer_failed);
    capture.submit(&[0.5]);
    assert_eq!(health.snapshot().writer_accepted_samples, 4098);
    assert_eq!(health.snapshot().writer_dropped_samples, 1);
}

#[test]
fn terminal_accounting_finalization_failure_never_becomes_durable_success() {
    let health = Arc::new(RecordingHealthState::default());
    let settings = limits(1, 10);
    let output = SharedSink::default();
    let writer = WavSampleWriter::new(Box::new(output.clone()), settings).unwrap();
    let (mut capture, consumer) = RecorderWriter::prepare_capture(settings, health.clone());
    capture.submit(&[0.25, 0.5]);
    capture.mode.store(STOPPED, Ordering::Release);
    output.fault.store(2, Ordering::Relaxed);
    assert!(writer_loop(writer, consumer, &capture.mode, &health).is_err());
    partition(&health, 2, 2, 0, 0);
    assert!(health.snapshot().writer_failed);
}

#[test]
fn terminal_accounting_counter_overflow_saturates_and_marks_exact_totals_unavailable() {
    let health = Arc::new(RecordingHealthState::default());
    health
        .writer_accepted_samples
        .store(u64::MAX - 1, Ordering::Relaxed);
    health
        .writer_dropped_samples
        .store(u64::MAX, Ordering::Relaxed);
    let (mut capture, _consumer) = RecorderWriter::prepare_capture(limits(1, 10), health.clone());
    capture.submit(&[0.25, 0.5]);
    capture.mode.store(CANCELLED, Ordering::Release);
    capture.submit(&[0.75]);
    let snapshot = health.snapshot();
    assert_eq!(snapshot.writer_accepted_samples, u64::MAX);
    assert_eq!(snapshot.writer_dropped_samples, u64::MAX);
    assert!(snapshot.writer_accounting_overflowed);
    assert!(!snapshot.writer_accounting_complete);
}

struct FinalizationPanicSink {
    inner: SharedSink,
    panic: Arc<std::sync::atomic::AtomicBool>,
}
impl Write for FinalizationPanicSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.inner.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl Seek for FinalizationPanicSink {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        assert!(
            !self.panic.load(Ordering::Acquire),
            "injected finalization panic"
        );
        self.inner.seek(position)
    }
}

#[test]
fn terminal_accounting_worker_panic_cannot_confirm_a_prefix_or_accept_late_samples() {
    let health = Arc::new(RecordingHealthState::default());
    let panic = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (mut writer, mut capture) = RecorderWriter::spawn(
        Box::new(FinalizationPanicSink {
            inner: SharedSink::default(),
            panic: panic.clone(),
        }),
        limits(1, 10),
        health.clone(),
    )
    .unwrap();
    panic.store(true, Ordering::Release);
    writer.cancel();
    assert!(writer.join().is_err());
    assert!(health.snapshot().writer_failed);
    assert!(!health.snapshot().writer_accounting_complete);
    capture.submit(&[0.5]);
    assert_eq!(health.snapshot().writer_accepted_samples, 0);
    assert_eq!(health.snapshot().writer_dropped_samples, 1);
}
