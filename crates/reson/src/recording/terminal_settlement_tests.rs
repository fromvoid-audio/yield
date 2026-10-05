use crate::recording::settlement::*;

fn bound_writer(
    output: Box<dyn RecordingSink>,
    cancelled: bool,
    data: &[f32],
) -> (
    RecorderWriter,
    RecorderCapture,
    RecordingSettlements,
    Arc<RecordingHealthState>,
) {
    let health = Arc::new(RecordingHealthState::default());
    let settings = limits(1, 20_000);
    let (mut capture, consumer) = RecorderWriter::prepare_capture(settings, health.clone());
    capture.submit(data);
    capture.mode.store(
        if cancelled { CANCELLED } else { STOPPED },
        Ordering::SeqCst,
    );
    let (binding, receiver) = RecordingSinkBinding::bind(output);
    let writer = RecorderWriter::spawn_prepared_with_settlement(
        binding.sink,
        settings,
        consumer,
        capture.mode.clone(),
        health.clone(),
        Some(binding.producer),
    )
    .unwrap();
    (writer, capture, receiver, health)
}
fn bytes(output: &SharedSink) -> Vec<u8> {
    output.bytes.lock().unwrap().get_ref().clone()
}
fn barrier(receiver: &mut RecordingSettlements) -> RecordingTerminalBarrier {
    receiver.recv_barrier(Duration::from_secs(3)).unwrap()
}
fn no_joined(receiver: &mut RecordingSettlements) {
    assert!(receiver.recv_joined(Duration::from_millis(20)).is_err());
}

#[test]
fn terminal_settlement_real_hound_stop_cancel_ordering_and_actual_join_only() {
    for cancelled in [false, true] {
        let output = SharedSink::default();
        let (mut writer, mut capture, mut receiver, health) =
            bound_writer(Box::new(output.clone()), cancelled, &[0.25, 0.5]);
        let before = bytes(&output);
        let terminal = barrier(&mut receiver);
        assert!(receiver.owns_barrier(&terminal));
        assert_eq!(terminal.frames(), if cancelled { 0 } else { 2 });
        assert_eq!(terminal.limits().channels(), 1);
        assert_eq!(terminal.limits().sample_rate(), 48_000);
        assert_eq!(terminal.accounting().accepted_samples, 2);
        assert_eq!(
            terminal.accounting().encoded_samples,
            if cancelled { 0 } else { 2 }
        );
        assert_eq!(
            terminal.accounting().abandoned_samples,
            if cancelled { 2 } else { 0 }
        );
        assert_eq!(
            bytes(&output),
            before,
            "no tail or terminal header rewrite before acknowledgement"
        );
        no_joined(&mut receiver);
        terminal.acknowledge().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !writer.join.as_ref().unwrap().is_finished() {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        no_joined(&mut receiver); // Worker finished, but actual join has not happened.
        let stats = writer.join().unwrap();
        let joined = receiver.recv_joined(Duration::from_secs(1)).unwrap();
        assert!(receiver.owns_completion(&joined));
        assert_eq!(joined.state(), stats.state);
        assert_eq!(joined.frames(), stats.frames);
        assert_eq!(
            samples(&output),
            if cancelled { vec![] } else { vec![0.25, 0.5] }
        );
        assert!(writer.join().is_err());
        no_joined(&mut receiver);
        assert!(receiver.recv_barrier(Duration::from_millis(1)).is_err());
        capture.submit(&[0.75]);
        assert_eq!(health.snapshot().writer_accepted_samples, 2);
    }
}

#[test]
fn terminal_settlement_denial_dropped_token_and_missing_ack_preserve_exact_prefix_through_hound_drop()
 {
    for denial in 0..3 {
        let output = SharedSink::default();
        let (mut writer, _capture, mut receiver, health) =
            bound_writer(Box::new(output.clone()), false, &[0.25, 0.5]);
        let terminal = barrier(&mut receiver);
        let before = bytes(&output);
        match denial {
            0 => terminal.deny(),
            1 => drop(terminal),
            _ => {
                // Hold the actual one-shot sender beyond the fixed deadline, then try late delivery.
                assert!(writer.join().is_err());
                assert!(terminal.acknowledge().is_err());
            }
        }
        if denial != 2 {
            assert!(writer.join().is_err());
        }
        assert!(health.snapshot().writer_failed);
        assert_eq!(
            bytes(&output),
            before,
            "hound and BufWriter destructors must not rewrite/flush after denial"
        );
        no_joined(&mut receiver);
        drop(writer);
        assert_eq!(bytes(&output), before);
    }
}

#[test]
fn terminal_settlement_dropped_consumer_and_actual_writer_drop_cannot_finalize() {
    let output = SharedSink::default();
    let (mut writer, _capture, receiver, _health) =
        bound_writer(Box::new(output.clone()), false, &[0.25]);
    let before = bytes(&output);
    drop(receiver);
    assert!(writer.join().is_err());
    assert_eq!(bytes(&output), before);
    let output = SharedSink::default();
    let (writer, _capture, mut receiver, _health) =
        bound_writer(Box::new(output.clone()), false, &[0.25]);
    let terminal = barrier(&mut receiver);
    let before = bytes(&output);
    drop(writer);
    assert!(terminal.acknowledge().is_err());
    assert_eq!(bytes(&output), before);
    no_joined(&mut receiver);
}

#[test]
fn terminal_settlement_two_actual_serializers_have_nontransferable_issuance() {
    let one = SharedSink::default();
    let two = SharedSink::default();
    let (mut first, _capture, mut first_receiver, _health) =
        bound_writer(Box::new(one.clone()), false, &[0.25]);
    let (mut second, _capture, mut second_receiver, _health) =
        bound_writer(Box::new(two.clone()), true, &[0.5]);
    let first_barrier = barrier(&mut first_receiver);
    let second_barrier = barrier(&mut second_receiver);
    assert!(!first_receiver.owns_barrier(&second_barrier));
    assert!(!second_receiver.owns_barrier(&first_barrier));
    first_barrier.acknowledge().unwrap();
    second_barrier.acknowledge().unwrap();
    first.join().unwrap();
    second.join().unwrap();
    let first_joined = first_receiver.recv_joined(Duration::from_secs(1)).unwrap();
    let second_joined = second_receiver.recv_joined(Duration::from_secs(1)).unwrap();
    assert!(!first_receiver.owns_completion(&second_joined));
    assert!(!second_receiver.owns_completion(&first_joined));
    assert_eq!(samples(&one), vec![0.25]);
    assert!(samples(&two).is_empty());
}

#[test]
fn terminal_settlement_cancel_after_stop_barrier_or_already_winning_cancel_never_promotes_stop() {
    let output = SharedSink::default();
    let (mut writer, _capture, mut receiver, _health) =
        bound_writer(Box::new(output.clone()), false, &[0.25]);
    let terminal = barrier(&mut receiver);
    let before = bytes(&output);
    writer.cancel();
    writer.stop();
    terminal.acknowledge().unwrap();
    assert!(writer.join().is_err());
    assert_eq!(bytes(&output), before);
    no_joined(&mut receiver);
    let output = SharedSink::default();
    let (mut writer, _capture, mut receiver, _health) =
        bound_writer(Box::new(output.clone()), true, &[0.25]);
    writer.stop();
    writer.stop();
    writer.cancel();
    let terminal = barrier(&mut receiver);
    assert_eq!(terminal.state(), RecordingPrefixState::CancelledRetained);
    terminal.acknowledge().unwrap();
    assert_eq!(
        writer.join().unwrap().state,
        RecordingPrefixState::CancelledRetained
    );
    assert!(samples(&output).is_empty());
}

#[test]
fn terminal_settlement_lost_joined_delivery_retains_real_finalized_bytes_without_completion_grant()
{
    let output = SharedSink::default();
    let (mut writer, _capture, mut receiver, _health) =
        bound_writer(Box::new(output.clone()), false, &[0.25]);
    barrier(&mut receiver).acknowledge().unwrap();
    drop(receiver);
    assert!(
        writer.join().is_err(),
        "successful codec cannot manufacture delivered joined evidence"
    );
    assert_eq!(samples(&output), vec![0.25]);
    assert!(writer.join().is_err());
}

#[test]
fn terminal_settlement_real_finalization_error_and_panic_issue_no_joined_completion() {
    for fault in [1, 2, 3, 4] {
        let output = SharedSink::default();
        let panic = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let late = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink: Box<dyn RecordingSink> = Box::new(FailureAuditSink {
            inner: output.clone(),
            panic: panic.clone(),
            failed: failed.clone(),
            late: late.clone(),
        });
        let (mut writer, mut capture, mut receiver, health) = bound_writer(sink, false, &[0.25]);
        let terminal = barrier(&mut receiver);
        if fault == 4 {
            panic.store(true, Ordering::SeqCst);
        } else {
            output.fault.store(fault, Ordering::SeqCst);
        }
        let before = bytes(&output);
        terminal.acknowledge().unwrap();
        assert!(writer.join().is_err());
        no_joined(&mut receiver);
        capture.submit(&[0.5]);
        assert_eq!(health.snapshot().writer_accepted_samples, 1);
        assert!(health.snapshot().writer_failed);
        assert!(failed.load(Ordering::SeqCst));
        assert_eq!(
            late.load(Ordering::SeqCst),
            0,
            "no IO forwarded after the first failure, including codec Drop"
        );
        let retained = bytes(&output);
        assert!(retained.len() >= before.len());
        drop(writer);
        assert_eq!(
            bytes(&output),
            retained,
            "actual partial writes stay retained without compensation"
        );
    }
}

#[test]
fn terminal_settlement_inflight_callback_settles_before_barrier_and_has_no_late_acceptance() {
    for cancelled in [false, true] {
        let health = Arc::new(RecordingHealthState::default());
        let settings = limits(1, 10);
        let (mut capture, consumer) = RecorderWriter::prepare_capture(settings, health.clone());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        capture.submission_hook = Some(Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        let mode = capture.mode.clone();
        let callback = thread::spawn(move || {
            capture.submit(&[0.25, 0.5]);
            capture
        });
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let output = SharedSink::default();
        let (binding, mut receiver) = RecordingSinkBinding::bind(Box::new(output.clone()));
        let mut writer = RecorderWriter::spawn_prepared_with_settlement(
            binding.sink,
            settings,
            consumer,
            mode,
            health.clone(),
            Some(binding.producer),
        )
        .unwrap();
        if cancelled {
            writer.cancel();
        } else {
            writer.stop();
        }
        assert!(matches!(
            receiver.recv_barrier(Duration::from_millis(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        release_tx.send(()).unwrap();
        let mut capture = callback.join().unwrap();
        let terminal = barrier(&mut receiver);
        assert_eq!(terminal.accounting().accepted_samples, 2);
        assert_eq!(
            terminal.accounting().encoded_samples,
            if cancelled { 0 } else { 2 }
        );
        assert_eq!(
            terminal.accounting().abandoned_samples,
            if cancelled { 2 } else { 0 }
        );
        terminal.acknowledge().unwrap();
        writer.join().unwrap();
        receiver.recv_joined(Duration::from_secs(1)).unwrap();
        capture.submit(&[0.75]);
        assert_eq!(health.snapshot().writer_accepted_samples, 2);
    }
}

struct FailureAuditSink {
    inner: SharedSink,
    panic: Arc<std::sync::atomic::AtomicBool>,
    failed: Arc<std::sync::atomic::AtomicBool>,
    late: Arc<std::sync::atomic::AtomicUsize>,
}
impl FailureAuditSink {
    fn entered(&self) {
        if self.failed.load(Ordering::SeqCst) {
            self.late.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn result<T>(&self, result: io::Result<T>) -> io::Result<T> {
        if result.is_err() {
            self.failed.store(true, Ordering::SeqCst);
        }
        result
    }
}
impl Write for FailureAuditSink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.entered();
        let result = self.inner.write(data);
        self.result(result)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.entered();
        let result = self.inner.flush();
        self.result(result)
    }
}
impl Seek for FailureAuditSink {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.entered();
        if self.panic.swap(false, Ordering::SeqCst) {
            self.failed.store(true, Ordering::SeqCst);
            panic!("actual retained transport finalization panic");
        }
        let result = self.inner.seek(position);
        self.result(result)
    }
}

#[test]
fn terminal_settlement_actual_failed_pcm_batch_arms_drop_without_barrier_or_completion() {
    let output = SharedSink::default();
    let health = Arc::new(RecordingHealthState::default());
    let settings = limits(1, 1);
    let (mut capture, consumer) = RecorderWriter::prepare_capture(settings, health.clone());
    capture.submit(&vec![0.25; WRITER_DRAIN_SAMPLES + 2]);
    let (binding, mut receiver) = RecordingSinkBinding::bind(Box::new(output.clone()));
    let mut writer = RecorderWriter::spawn_prepared_with_settlement(
        binding.sink,
        settings,
        consumer,
        capture.mode.clone(),
        health.clone(),
        Some(binding.producer),
    )
    .unwrap();
    let before = bytes(&output);
    assert!(writer.join().is_err());
    assert!(receiver.recv_barrier(Duration::from_millis(20)).is_err());
    no_joined(&mut receiver);
    assert_eq!(bytes(&output), before);
    let observed = health.snapshot();
    assert_eq!(observed.writer_accepted_samples, 4098);
    assert_eq!(observed.writer_unconfirmed_samples, 4096);
    assert_eq!(observed.writer_abandoned_samples, 2);
    assert!(observed.writer_failed);
    assert!(observed.writer_accounting_complete);
}

#[cfg(feature = "native-audio")]
#[test]
fn terminal_settlement_actual_audio_recorder_stop_cancel_repeat_and_drop_routes() {
    use crate::input::ResolvedInput;
    use crate::recording::{AudioRecorder, monitor};
    for route in 0..3 {
        let output = SharedSink::default();
        let health = Arc::new(RecordingHealthState::default());
        let settings = limits(1, 10);
        let (mut capture, consumer) = RecorderWriter::prepare_capture(settings, health.clone());
        let (binding, mut receiver) = RecordingSinkBinding::bind(Box::new(output.clone()));
        let writer = RecorderWriter::spawn_prepared_with_settlement(
            binding.sink,
            settings,
            consumer,
            capture.mode.clone(),
            health.clone(),
            Some(binding.producer),
        )
        .unwrap();
        let (monitor, _) = monitor::start_recording_monitor(48_000, 1, health.clone());
        let mut recorder = AudioRecorder {
            stream: None,
            writer: Some(writer),
            resolved: ResolvedInput {
                host_id: "test".into(),
                device_name: "retained transport fixture".into(),
                sample_rate: 48_000,
                buffer_size_frames: Some(128),
                stream_channel_count: 1,
                recorded_channel_count: 1,
                selected_channels: vec![1],
                used_fallback: false,
            },
            path: "diagnostic-only-never-opened.wav".into(),
            monitor,
            health: health.clone(),
            active: true,
        };
        if route == 2 {
            let before = bytes(&output);
            drop(recorder);
            assert_eq!(bytes(&output), before);
            assert!(receiver.recv_barrier(Duration::from_millis(20)).is_err());
            no_joined(&mut receiver);
        } else {
            capture.submit(&[0.25, 0.5]);
            let owner = thread::spawn(move || {
                let terminal = barrier(&mut receiver);
                assert_eq!(
                    terminal.state(),
                    if route == 0 {
                        RecordingPrefixState::StoppedUnpublished
                    } else {
                        RecordingPrefixState::CancelledRetained
                    }
                );
                terminal.acknowledge().unwrap();
                receiver.recv_joined(Duration::from_secs(3)).unwrap()
            });
            let outcome = if route == 0 {
                recorder.stop().unwrap()
            } else {
                recorder.cancel().unwrap()
            };
            let proof = owner.join().unwrap();
            assert_eq!(proof.frames(), outcome.frames);
            assert_eq!(proof.state(), outcome.prefix_state);
            assert!(!recorder.is_active());
            assert!(recorder.stop().is_err());
            assert!(recorder.cancel().is_err());
        }
        capture.submit(&[0.75]);
        assert!(health.snapshot().writer_dropped_samples >= 1);
    }
}

#[test]
fn terminal_settlement_initial_serializer_errors_cannot_issue_barrier_or_rewrite_on_drop() {
    for fault in [1, 2, 3] {
        let output = SharedSink::default();
        output.fault.store(fault, Ordering::SeqCst);
        let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let late = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink = FailureAuditSink {
            inner: output.clone(),
            panic: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            failed: failed.clone(),
            late: late.clone(),
        };
        let (binding, mut receiver) = RecordingSinkBinding::bind(Box::new(sink));
        let health = Arc::new(RecordingHealthState::default());
        let settings = limits(1, 10);
        let (capture, consumer) = RecorderWriter::prepare_capture(settings, health.clone());
        assert!(
            RecorderWriter::spawn_prepared_with_settlement(
                binding.sink,
                settings,
                consumer,
                capture.mode.clone(),
                health,
                Some(binding.producer)
            )
            .is_err()
        );
        assert!(failed.load(Ordering::SeqCst));
        assert_eq!(late.load(Ordering::SeqCst), 0);
        assert!(receiver.recv_barrier(Duration::from_millis(1)).is_err());
        no_joined(&mut receiver);
    }
}

#[test]
fn terminal_settlement_real_counter_overflow_is_observation_only_and_never_authenticates_terminal_work()
 {
    let output = SharedSink::default();
    let health = Arc::new(RecordingHealthState::default());
    health
        .writer_accepted_samples
        .store(u64::MAX - 1, Ordering::SeqCst);
    let settings = limits(1, 10);
    let (mut capture, consumer) = RecorderWriter::prepare_capture(settings, health.clone());
    capture.submit(&[0.25, 0.5]); // Actual callback path saturates, rather than fabricating a health receipt.
    capture.mode.store(STOPPED, Ordering::SeqCst);
    let (binding, mut receiver) = RecordingSinkBinding::bind(Box::new(output.clone()));
    let mut writer = RecorderWriter::spawn_prepared_with_settlement(
        binding.sink,
        settings,
        consumer,
        capture.mode.clone(),
        health.clone(),
        Some(binding.producer),
    )
    .unwrap();
    let before = bytes(&output);
    assert!(writer.join().is_err());
    assert!(health.snapshot().writer_accounting_overflowed);
    assert_eq!(bytes(&output), before);
    assert!(receiver.recv_barrier(Duration::from_millis(1)).is_err());
    no_joined(&mut receiver);
}

#[test]
fn terminal_inventory_exact_real_hound_bytes_all_channels_and_buffer_boundaries() {
    for channels in 1..=8 {
        for frames in [0, 1, 4096 / channels as usize, 4097, 10_001] {
            let output = SharedSink::default();
            let health = Arc::new(RecordingHealthState::default());
            let settings = limits(channels, 20_000);
            let (mut capture, consumer) = RecorderWriter::prepare_capture(settings, health.clone());
            let data: Vec<f32> = (0..frames * channels as usize)
                .map(|i| (i % 31) as f32 / 32.0 - 0.5)
                .collect();
            capture.submit(&data);
            capture.mode.store(STOPPED, Ordering::SeqCst);
            let (binding, mut receiver) = RecordingSinkBinding::bind(Box::new(output.clone()));
            let mut writer = RecorderWriter::spawn_prepared_with_settlement(
                binding.sink,
                settings,
                consumer,
                capture.mode.clone(),
                health,
                Some(binding.producer),
            )
            .unwrap();
            let terminal = barrier(&mut receiver);
            let inventory = terminal.write_inventory();
            let before = bytes(&output);
            assert_eq!(inventory.pending_offset(), before.len() as u64);
            assert!(inventory.pending_bytes().len() <= 16_384);
            assert_eq!(inventory.object_bytes(), 68 + data.len() as u64 * 4);
            assert_eq!(
                inventory.pending_offset() + inventory.pending_bytes().len() as u64,
                inventory.object_bytes()
            );
            let mut expected = before.clone();
            expected.extend_from_slice(inventory.pending_bytes());
            expected[4..8].copy_from_slice(inventory.riff_length_patch());
            expected[64..68].copy_from_slice(inventory.data_length_patch());
            let expected_pending = inventory.pending_bytes().to_vec();
            let expected_offset = inventory.pending_offset();
            assert_eq!(bytes(&output), before, "snapshot does no transport IO");
            no_joined(&mut receiver);
            terminal.acknowledge().unwrap();
            let stats = writer.join().unwrap();
            let joined = receiver.recv_joined(Duration::from_secs(1)).unwrap();
            assert_eq!(stats.frames, frames as u64);
            assert_eq!(bytes(&output), expected);
            assert_eq!(samples(&output), data);
            assert_eq!(joined.write_inventory().pending_bytes(), expected_pending);
            assert_eq!(joined.write_inventory().pending_offset(), expected_offset);
            assert_eq!(
                joined.write_inventory().object_bytes(),
                expected.len() as u64
            );
        }
    }
}

struct PartialTransport(SharedSink);
impl Write for PartialTransport {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(&bytes[..bytes.len().min(3)])
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
impl Seek for PartialTransport {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.0.seek(position)
    }
}

#[test]
fn terminal_inventory_partial_transport_writes_preserve_exact_plan_and_join() {
    let output = SharedSink::default();
    let data: Vec<f32> = (0..12_000).map(|i| i as f32 / 16_384.0).collect();
    let (mut writer, _, mut receiver, _) =
        bound_writer(Box::new(PartialTransport(output.clone())), false, &data);
    let terminal = barrier(&mut receiver);
    let mut expected = bytes(&output);
    let inventory = terminal.write_inventory();
    expected.extend_from_slice(inventory.pending_bytes());
    expected[4..8].copy_from_slice(inventory.riff_length_patch());
    expected[64..68].copy_from_slice(inventory.data_length_patch());
    terminal.acknowledge().unwrap();
    writer.join().unwrap();
    let joined = receiver.recv_joined(Duration::from_secs(1)).unwrap();
    assert_eq!(
        joined.write_inventory().object_bytes(),
        expected.len() as u64
    );
    assert_eq!(bytes(&output), expected);
    assert_eq!(samples(&output), data);
}

#[test]
fn terminal_inventory_real_buffer_mismatch_refuses_without_snapshot_io() {
    for fault in 0..4 {
        let output = SharedSink::default();
        let mut writer = WavSampleWriter::new(Box::new(output.clone()), limits(3, 10)).unwrap();
        writer.write_samples(&[0.25, 0.5, 0.75]).unwrap();
        let before = bytes(&output);
        match fault {
            0 => writer.buffer.0.lock().unwrap().get_mut().position += 4,
            1 => writer.written_samples += 3,
            2 => writer.buffer.0.lock().unwrap().get_mut().position = 64,
            _ => writer.buffer.0.lock().unwrap().get_mut().position = u64::MAX,
        }
        assert!(writer.prepare_terminal_inventory().is_err());
        assert_eq!(bytes(&output), before);
    }
}

#[test]
fn terminal_inventory_cursor_rejects_changed_extra_missing_or_reordered_actual_writes() {
    let output = SharedSink::default();
    let mut writer = WavSampleWriter::new(Box::new(output), limits(1, 10)).unwrap();
    writer.write_samples(&[0.25, 0.5]).unwrap();
    let inventory = writer.prepare_terminal_inventory().unwrap();
    assert!(
        writer.prepare_terminal_inventory().is_err(),
        "cannot reissue inventory"
    );
    let mut cursor = TerminalByteCursor {
        inventory,
        stage: 0,
        written: 0,
    };
    assert!(cursor.seek(4).is_err(), "cannot omit buffered tail");
    assert!(cursor.check_write(68, &[0xff]).is_err());
    let tail = cursor.inventory.pending.clone();
    cursor.check_write(68, &tail[..3]).unwrap();
    cursor.written = 3;
    cursor.check_write(71, &tail[3..]).unwrap();
    cursor.written = tail.len();
    assert!(cursor.seek(64).is_err());
    cursor.seek(4).unwrap();
    assert!(cursor.check_write(4, &[0; 4]).is_err());
    cursor
        .check_write(4, cursor.inventory.riff_length_patch())
        .unwrap();
    cursor.written = 4;
    assert!(!cursor.complete());
    cursor.seek(64).unwrap();
    cursor
        .check_write(64, cursor.inventory.data_length_patch())
        .unwrap();
    cursor.written = 4;
    assert!(cursor.complete());
    assert!(cursor.seek(4).is_err());
    assert!(cursor.check_write(68, &[1]).is_err());
}

#[test]
fn terminal_inventory_actual_finalizer_refuses_changed_buffer_without_transport_writes() {
    let output = SharedSink::default();
    let mut writer = WavSampleWriter::new(Box::new(output.clone()), limits(1, 10)).unwrap();
    writer.write_samples(&[0.25, 0.5]).unwrap();
    let inventory = writer.prepare_terminal_inventory().unwrap();
    let before = bytes(&output);
    // Adversarial private serializer corruption after the genuine snapshot, never caller authority.
    writer.buffer.write_all(&[0xff]).unwrap();
    assert!(
        writer
            .finalize(RecordingPrefixState::StoppedUnpublished)
            .is_err()
    );
    assert_eq!(inventory.pending_bytes().len(), 8);
    assert_eq!(
        bytes(&output),
        before,
        "neither finalize nor Drop can dispatch changed tail"
    );
}
