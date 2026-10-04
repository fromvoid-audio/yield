use std::io::Cursor;
use std::sync::{Mutex, atomic::AtomicU8};

#[derive(Clone, Default)]
struct SharedSink {
    bytes: Arc<Mutex<Cursor<Vec<u8>>>>,
    fault: Arc<AtomicU8>,
}
impl Write for SharedSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fault.load(Ordering::Relaxed) == 1 {
            return Err(io::Error::other("write fault"));
        }
        self.bytes.lock().unwrap().write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.fault.load(Ordering::Relaxed) == 3 {
            Err(io::Error::other("flush fault"))
        } else {
            Ok(())
        }
    }
}
impl Seek for SharedSink {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if self.fault.load(Ordering::Relaxed) == 2 {
            return Err(io::Error::other("seek fault"));
        }
        self.bytes.lock().unwrap().seek(position)
    }
}
fn limits(channels: u16, frames: u64) -> RecordingLimits {
    RecordingLimits::new(48_000, channels, frames).unwrap()
}
fn samples(sink: &SharedSink) -> Vec<f32> {
    let bytes = sink.bytes.lock().unwrap().get_ref().clone();
    hound::WavReader::new(Cursor::new(bytes))
        .unwrap()
        .into_samples::<f32>()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn retained_writer_serializes_float_wav_and_reports_only_unpublished_prefix() {
    let output = SharedSink::default();
    let mut writer = WavSampleWriter::new(Box::new(output.clone()), limits(2, 10)).unwrap();
    writer.write_samples(&[0.0, 0.5, -0.5, 1.0]).unwrap();
    let stats = writer
        .finalize(RecordingPrefixState::StoppedUnpublished)
        .unwrap();
    assert_eq!(stats.frames, 2);
    assert_eq!(stats.state, RecordingPrefixState::StoppedUnpublished);
    let bytes = output.bytes.lock().unwrap().get_ref().clone();
    let reader = hound::WavReader::new(Cursor::new(bytes)).unwrap();
    assert_eq!(reader.spec().channels, 2);
    assert_eq!(reader.spec().sample_rate, 48_000);
    assert_eq!(reader.spec().sample_format, hound::SampleFormat::Float);
    assert_eq!(samples(&output), vec![0.0, 0.5, -0.5, 1.0]);
}

#[test]
fn retained_writer_rejects_invalid_limits_origin_frames_and_frame_budget_before_extra_data() {
    for (rate, channels, frames) in [
        (0, 1, 1),
        (192_001, 1, 1),
        (48_000, 0, 1),
        (48_000, 9, 1),
        (48_000, 2, 0),
        (48_000, 8, u64::MAX),
    ] {
        assert!(RecordingLimits::new(rate, channels, frames).is_err());
    }
    let maximal = RecordingLimits::new(192_000, 8, 1).unwrap();
    assert_eq!(maximal.ring_samples, 3_072_000);
    let output = SharedSink::default();
    let mut writer = WavSampleWriter::new(Box::new(output.clone()), limits(2, 1)).unwrap();
    assert!(writer.write_samples(&[1.0]).is_err());
    assert_eq!(writer.written_samples, 0);
    writer.write_samples(&[0.25, -0.25]).unwrap();
    assert!(writer.write_samples(&[0.5, -0.5]).is_err());
    writer
        .finalize(RecordingPrefixState::StoppedUnpublished)
        .unwrap();
    assert_eq!(samples(&output), vec![0.25, -0.25]);
    let mut at_wrong_origin = SharedSink::default();
    at_wrong_origin.seek(SeekFrom::Start(7)).unwrap();
    assert!(WavSampleWriter::new(Box::new(at_wrong_origin.clone()), limits(1, 1)).is_err());
    assert!(at_wrong_origin.bytes.lock().unwrap().get_ref().is_empty());
}

#[test]
fn retained_writer_bounds_each_transport_write_and_seek_without_fallback() {
    let output = SharedSink::default();
    let mut bounded = BoundedSink {
        inner: Box::new(output.clone()),
        position: 0,
        limit: 4,
        terminal_fence: None,
        terminal_bytes: None,
    };
    bounded.write_all(&[1, 2, 3, 4]).unwrap();
    assert!(bounded.write_all(&[5]).is_err());
    assert!(bounded.seek(SeekFrom::Start(5)).is_err());
    assert!(bounded.seek(SeekFrom::Current(-5)).is_err());
    assert!(bounded.seek(SeekFrom::End(0)).is_err());
    assert_eq!(output.bytes.lock().unwrap().get_ref(), &[1, 2, 3, 4]);
    assert_eq!(bounded.seek(SeekFrom::Start(2)).unwrap(), 2);
    bounded.write_all(&[8, 9]).unwrap();
    assert_eq!(output.bytes.lock().unwrap().get_ref(), &[1, 2, 8, 9]);
}

#[test]
fn retained_writer_header_sample_write_seek_and_flush_failures_return_no_completion() {
    for fault in [1, 2, 3] {
        let output = SharedSink::default();
        output.fault.store(fault, Ordering::Relaxed);
        assert!(WavSampleWriter::new(Box::new(output), limits(1, 10)).is_err());
    }
    let output = SharedSink::default();
    let mut writer = WavSampleWriter::new(Box::new(output.clone()), limits(1, 10_000)).unwrap();
    output.fault.store(1, Ordering::Relaxed);
    assert!(writer.write_samples(&vec![0.5; 8192]).is_err());
    assert!(
        writer
            .finalize(RecordingPrefixState::StoppedUnpublished)
            .is_err()
    );
    for fault in [2, 3] {
        let output = SharedSink::default();
        let mut writer = WavSampleWriter::new(Box::new(output.clone()), limits(2, 10)).unwrap();
        writer.write_samples(&[0.5, -0.5]).unwrap();
        output.fault.store(fault, Ordering::Relaxed);
        assert!(
            writer
                .finalize(RecordingPrefixState::StoppedUnpublished)
                .is_err()
        );
    }
}

#[test]
fn recorder_stop_drains_complete_frames_for_non_power_of_two_channel_counts() {
    for channels in [2, 3, 5, 7, 8] {
        let output = SharedSink::default();
        let health = Arc::new(RecordingHealthState::default());
        let (mut writer, mut capture) =
            RecorderWriter::spawn(Box::new(output.clone()), limits(channels, 3000), health)
                .unwrap();
        let data = vec![0.25; usize::from(channels) * 2000];
        capture.submit(&data);
        writer.stop();
        capture.submit(&vec![0.5; usize::from(channels)]);
        let stats = writer.join().unwrap();
        assert_eq!(stats.frames, 2000);
        assert_eq!(stats.state, RecordingPrefixState::StoppedUnpublished);
        assert_eq!(samples(&output), data);
    }
}

#[test]
fn recorder_cancellation_and_drop_retain_prefix_without_publication_or_path_cleanup() {
    let output = SharedSink::default();
    let health = Arc::new(RecordingHealthState::default());
    let settings = limits(2, 100);
    let (mut capture, consumer) = RecorderWriter::prepare_capture(settings, Arc::clone(&health));
    capture.submit(&[0.25, -0.25]);
    capture.mode.store(CANCELLED, Ordering::Release);
    let mut writer = RecorderWriter::spawn_prepared(
        Box::new(output.clone()),
        settings,
        consumer,
        Arc::clone(&capture.mode),
        Arc::clone(&health),
    )
    .unwrap();
    let stats = writer.join().unwrap();
    assert_eq!(stats.state, RecordingPrefixState::CancelledRetained);
    assert_eq!(stats.frames, 0);
    assert!(samples(&output).is_empty());
    let output = SharedSink::default();
    let (writer, mut capture) =
        RecorderWriter::spawn(Box::new(output.clone()), settings, health).unwrap();
    drop(writer);
    capture.submit(&[0.5, -0.5]);
    assert_eq!(capture.mode.load(Ordering::Acquire), CANCELLED);
    assert!(samples(&output).is_empty());
}

#[test]
fn stalled_consumer_and_invalid_callback_frames_have_nonblocking_health_evidence() {
    let health = Arc::new(RecordingHealthState::default());
    let (mut capture, _consumer) = RecorderWriter::prepare_capture(
        RecordingLimits::new(1, 2, 10).unwrap(),
        Arc::clone(&health),
    );
    capture.submit(&[0.0; 6]);
    assert_eq!(health.snapshot().writer_dropped_samples, 6);
    assert_eq!(health.snapshot().writer_overrun_events, 1);
    capture.submit(&[0.0; 3]);
    assert!(health.snapshot().writer_failed);
    assert_eq!(capture.mode.load(Ordering::Acquire), CANCELLED);
}

#[test]
fn recorder_worker_budget_failure_is_visible_and_subsequent_callbacks_do_not_write() {
    let health = Arc::new(RecordingHealthState::default());
    let output = SharedSink::default();
    let (mut writer, mut capture) =
        RecorderWriter::spawn(Box::new(output.clone()), limits(1, 1), Arc::clone(&health)).unwrap();
    capture.submit(&[0.0, 0.5]);
    writer.stop();
    assert!(writer.join().is_err());
    assert!(health.snapshot().writer_failed);
    capture.submit(&[0.75]);
    assert_eq!(health.snapshot().writer_dropped_samples, 1);
    assert!(samples(&output).is_empty());
}

mod terminal_accounting {
    use super::*;
    include!("terminal_accounting_tests.rs");
}

mod terminal_settlement {
    use super::*;
    include!("terminal_settlement_tests.rs");
}
