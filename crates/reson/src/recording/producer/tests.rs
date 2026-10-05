//! Real serializer/settlement tests over a codec transport, never native qualification.
use super::*;
use std::io::{self, Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Cursor<Vec<u8>>>>);
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}
impl Seek for Sink {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.0.lock().unwrap().seek(from)
    }
}

#[test]
fn public_producer_stop_uses_original_bound_barrier_actual_wav_and_joined_evidence() {
    let output = Sink::default();
    let (binding, mut owner) = super::super::RecordingSinkBinding::bind(Box::new(output.clone()));
    let (_, foreign) = super::super::RecordingSinkBinding::bind(Box::new(Sink::default()));
    let limits = RecordingLimits::new(48_000, 1, 1024).unwrap();
    let mut producer = RecordingProducer::start(binding, limits).unwrap();
    producer.submit(&[0.25; 32]);
    assert_eq!(producer.health().writer_accepted_samples, 32);
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let barrier = owner.recv_barrier(Duration::from_secs(2)).unwrap();
            assert!(owner.owns_barrier(&barrier));
            assert!(!foreign.owns_barrier(&barrier));
            assert_eq!(barrier.frames(), 32);
            assert_eq!(barrier.write_inventory().object_bytes(), 68 + 32 * 4);
            assert!(barrier.write_inventory().pending_bytes().len() <= 16 * 1024);
            barrier.acknowledge().unwrap();
            let joined = owner.recv_joined(Duration::from_secs(2)).unwrap();
            assert!(owner.owns_completion(&joined));
            assert!(!foreign.owns_completion(&joined));
            assert_eq!(joined.frames(), 32);
        });
        let result = producer.stop().unwrap();
        assert_eq!(result.frames, 32);
        assert_eq!(
            result.prefix_state,
            RecordingPrefixState::StoppedUnpublished
        );
        assert_eq!(result.health.writer_encoded_samples, 32);
        assert!(result.health.writer_accounting_complete);
    });
    let bytes = output.0.lock().unwrap().get_ref().clone();
    let mut wav = hound::WavReader::new(Cursor::new(bytes)).unwrap();
    assert_eq!(
        wav.samples::<f32>().collect::<Result<Vec<_>, _>>().unwrap(),
        vec![0.25; 32]
    );
}

#[test]
fn public_producer_cancel_empty_prefix_still_requires_original_owner_ack_and_join() {
    let (binding, mut owner) = super::super::RecordingSinkBinding::bind(Box::new(Sink::default()));
    let producer =
        RecordingProducer::start(binding, RecordingLimits::new(48_000, 1, 10).unwrap()).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let barrier = owner.recv_barrier(Duration::from_secs(2)).unwrap();
            assert_eq!(barrier.state(), RecordingPrefixState::CancelledRetained);
            barrier.acknowledge().unwrap();
            let joined = owner.recv_joined(Duration::from_secs(2)).unwrap();
            assert_eq!(joined.state(), RecordingPrefixState::CancelledRetained);
            assert_eq!(joined.frames(), 0);
        });
        let result = producer.cancel().unwrap();
        assert_eq!(result.prefix_state, RecordingPrefixState::CancelledRetained);
        assert_eq!(result.frames, 0);
    });
}

#[test]
fn public_producer_denied_barrier_cannot_issue_joined_completion() {
    let (binding, mut owner) = super::super::RecordingSinkBinding::bind(Box::new(Sink::default()));
    let producer =
        RecordingProducer::start(binding, RecordingLimits::new(48_000, 1, 10).unwrap()).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            owner.recv_barrier(Duration::from_secs(2)).unwrap().deny();
            assert!(owner.recv_joined(Duration::from_secs(2)).is_err());
        });
        assert!(producer.stop().is_err());
    });
}
