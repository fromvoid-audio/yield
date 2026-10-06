//! Actual serializer and bounded channel behavior, without physical qualification.
use super::*;
use crate::recording::RecordingProducer;
use std::io::Cursor;
use std::thread;

fn sink(timeout: Duration, limit: u64) -> (Sink, RecordingTransportReceiver) {
    let (tx, rx) = mpsc::sync_channel(1);
    (
        Sink {
            tx,
            position: 0,
            limit,
            failed: false,
            timeout,
            initialization: None,
        },
        RecordingTransportReceiver { rx },
    )
}

#[test]
fn actual_producer_serializes_wav_through_original_transport_and_terminal_route() {
    let limits = RecordingLimits::new(48_000, 1, 8192).unwrap();
    let (binding, receiver, mut settlements) = RecordingTransport::bind(limits);
    let (foreign_binding, _, foreign_settlements) = RecordingTransport::bind(limits);
    drop(foreign_binding);
    let bytes = thread::scope(|scope| {
        let owner = scope.spawn(move || {
            let mut object = Cursor::new(Vec::new());
            let mut flushes = 0;
            let mut barrier_seen = false;
            loop {
                match receiver.recv_timeout(Duration::from_millis(1)) {
                    Ok(request) => {
                        assert!(request.bytes().len() <= RECORDING_TRANSPORT_MAX_WRITE_BYTES);
                        match request.kind() {
                            RecordingTransportRequestKind::Write => {
                                object.seek(SeekFrom::Start(request.offset())).unwrap();
                                object.write_all(request.bytes()).unwrap();
                            }
                            RecordingTransportRequestKind::Flush => {
                                assert!(request.bytes().is_empty());
                                flushes += 1;
                            }
                        }
                        request.acknowledge().unwrap();
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => assert!(barrier_seen),
                }
                if !barrier_seen {
                    if let Ok(barrier) = settlements.recv_barrier(Duration::ZERO) {
                        assert!(settlements.owns_barrier(&barrier));
                        assert!(!foreign_settlements.owns_barrier(&barrier));
                        assert_eq!(barrier.frames(), 4096);
                        assert!(barrier.write_inventory().pending_bytes().len() <= 16384);
                        barrier.acknowledge().unwrap();
                        barrier_seen = true;
                    }
                }
                if let Ok(joined) = settlements.recv_joined(Duration::ZERO) {
                    assert!(settlements.owns_completion(&joined));
                    assert!(!foreign_settlements.owns_completion(&joined));
                    assert_eq!(joined.frames(), 4096);
                    assert!(flushes > 0);
                    break object.into_inner();
                }
            }
        });
        let mut producer = RecordingProducer::start(binding, limits).unwrap();
        producer.submit(&[0.25; 4096]);
        assert_eq!(producer.stop().unwrap().frames, 4096);
        owner.join().unwrap()
    });
    let mut reader = hound::WavReader::new(Cursor::new(bytes)).unwrap();
    assert_eq!(
        reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        vec![0.25; 4096]
    );
}

#[test]
fn original_producer_folds_initial_header_and_emits_whole_frame_batches() {
    let mut normalized = true;
    for channels in 1..=8u16 {
        let frames_per_batch = RECORDING_TRANSPORT_MAX_WRITE_BYTES / (usize::from(channels) * 4);
        for frames in [0, 1, frames_per_batch, frames_per_batch * 2 + 1, 7000] {
            let limits = RecordingLimits::new(48_000, channels, 16384).unwrap();
            let frame_bytes = usize::from(channels) * 4;
            let (binding, receiver, mut settlements) = RecordingTransport::bind(limits);
            let (initial, initial_header, pcm, pending, pending_bytes, bytes) =
                thread::scope(|scope| {
                    let owner = scope.spawn(move || {
                        let mut object = Cursor::new(Vec::new());
                        let mut initial = Vec::new();
                        let mut initial_header = None;
                        let mut pcm = Vec::new();
                        let mut capturing = false;
                        let mut pending = None;
                        let mut pending_bytes = Vec::new();
                        let deadline = std::time::Instant::now() + Duration::from_secs(10);
                        loop {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "original settlement completion"
                            );
                            match receiver.recv_timeout(Duration::from_millis(1)) {
                                Ok(request) => {
                                    match request.kind() {
                                        RecordingTransportRequestKind::Write => {
                                            if !capturing {
                                                initial.push((
                                                    request.offset(),
                                                    request.bytes().len(),
                                                ));
                                            } else if request.offset() >= 68 {
                                                pcm.push((request.offset(), request.bytes().len()));
                                            }
                                            object.seek(SeekFrom::Start(request.offset())).unwrap();
                                            object.write_all(request.bytes()).unwrap();
                                        }
                                        RecordingTransportRequestKind::Flush => {
                                            if !capturing {
                                                initial_header = Some(object.get_ref().clone());
                                            }
                                            capturing = true;
                                        }
                                    }
                                    request.acknowledge().unwrap();
                                }
                                Err(RecvTimeoutError::Timeout) => {}
                                Err(RecvTimeoutError::Disconnected) => assert!(pending.is_some()),
                            }
                            if pending.is_none() {
                                if let Ok(barrier) = settlements.recv_barrier(Duration::ZERO) {
                                    assert!(settlements.owns_barrier(&barrier));
                                    pending = Some((
                                        barrier.write_inventory().pending_offset(),
                                        barrier.write_inventory().pending_bytes().len(),
                                    ));
                                    pending_bytes =
                                        barrier.write_inventory().pending_bytes().to_vec();
                                    barrier.acknowledge().unwrap();
                                }
                            }
                            if let Ok(joined) = settlements.recv_joined(Duration::ZERO) {
                                assert!(settlements.owns_completion(&joined));
                                assert_eq!(joined.frames(), frames as u64);
                                assert_eq!(joined.write_inventory().pending_bytes(), pending_bytes);
                                assert_eq!(
                                    pending.unwrap(),
                                    (
                                        joined.write_inventory().pending_offset(),
                                        joined.write_inventory().pending_bytes().len()
                                    )
                                );
                                break (
                                    initial,
                                    initial_header.unwrap(),
                                    pcm,
                                    pending.unwrap(),
                                    pending_bytes,
                                    object.into_inner(),
                                );
                            }
                        }
                    });
                    let mut producer = RecordingProducer::start(binding, limits).unwrap();
                    let samples: Vec<_> = (0..frames * usize::from(channels))
                        .map(|index| ((index % 61) as f32 - 30.0) / 32.0)
                        .collect();
                    producer.submit(&samples);
                    assert_eq!(producer.stop().unwrap().frames, frames as u64);
                    owner.join().unwrap()
                });
            eprintln!(
                "original {channels}-channel {frames} frames initial={initial:?} pcm={pcm:?} pending={pending:?}"
            );
            let mut reference = Cursor::new(Vec::new());
            {
                let mut writer = hound::WavWriter::new(
                    &mut reference,
                    hound::WavSpec {
                        channels,
                        sample_rate: 48_000,
                        bits_per_sample: 32,
                        sample_format: hound::SampleFormat::Float,
                    },
                )
                .unwrap();
                writer.flush().unwrap();
                writer.finalize().unwrap();
            }
            normalized &= initial_header == reference.into_inner()
                && initial == [(0, 68)]
                && pcm.iter().all(|(offset, len)| {
                    (*offset - 68) % frame_bytes as u64 == 0
                        && *len % frame_bytes == 0
                        && *len <= RECORDING_TRANSPORT_MAX_WRITE_BYTES
                })
                && (pending.0 - 68) % frame_bytes as u64 == 0
                && pending.1 % frame_bytes == 0;
            assert_eq!(
                &bytes[pending.0 as usize..pending.0 as usize + pending.1],
                pending_bytes
            );
            let mut reader = hound::WavReader::new(Cursor::new(bytes)).unwrap();
            assert_eq!(reader.spec().channels, channels);
            assert_eq!(reader.duration(), frames as u32);
            assert!(
                reader
                    .samples::<f32>()
                    .enumerate()
                    .all(|(index, sample)| sample.unwrap() == ((index % 61) as f32 - 30.0) / 32.0)
            );
        }
    }
    assert!(
        normalized,
        "original header or PCM/pending tail is not normalized"
    );
}

#[test]
fn original_transport_limits_refuse_changed_producer_before_ring_and_header() {
    let original = RecordingLimits::new(48_000, 1, 8192).unwrap();
    let mut all_refused_before_resources = true;
    for (rate, channels, frames) in [
        (192_000, 1, 8192),
        (48_000, 2, 4096), // Identical encoded byte ceiling, different channel/frame meaning.
        (48_000, 1, 4096),
        (192_000, 8, 1024),
    ] {
        let supplied = RecordingLimits::new(rate, channels, frames).unwrap();
        let (binding, receiver, mut settlements) = RecordingTransport::bind(original);
        super::super::writer::PREPARED_RING_BYTES.with(|bytes| bytes.set(0));
        let (started, header_bytes, barriers, joined) = thread::scope(|scope| {
            let owner = scope.spawn(move || {
                let mut header_bytes = 0;
                let mut barriers = 0;
                loop {
                    match receiver.recv_timeout(Duration::from_millis(1)) {
                        Ok(request) => {
                            header_bytes += request.bytes().len();
                            request.acknowledge().unwrap();
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => {
                            return (
                                header_bytes,
                                barriers,
                                settlements.recv_joined(Duration::ZERO).is_ok(),
                            );
                        }
                    }
                    if let Ok(barrier) = settlements.recv_barrier(Duration::ZERO) {
                        barriers += 1;
                        barrier.deny();
                    }
                }
            });
            let started = match RecordingProducer::start(binding, supplied) {
                Ok(producer) => {
                    // Cleanly settle the real original worker without inventing completion.
                    assert!(producer.cancel().is_err());
                    true
                }
                Err(crate::input::AudioInputError::RecordingFailed { detail }) => {
                    assert_eq!(
                        detail,
                        "recording startup differs from original transport limits"
                    );
                    false
                }
                Err(error) => panic!("unexpected startup refusal: {error:?}"),
            };
            let (header_bytes, barriers, joined) = owner.join().unwrap();
            (started, header_bytes, barriers, joined)
        });
        let ring_bytes = super::super::writer::PREPARED_RING_BYTES.with(|bytes| bytes.get());
        eprintln!(
            "original transport mismatch {rate}/{channels}/{frames}: started={started}, ring_bytes={ring_bytes}, header_bytes={header_bytes}"
        );
        all_refused_before_resources &=
            !started && ring_bytes == 0 && header_bytes == 0 && barriers == 0 && !joined;
    }
    assert!(
        all_refused_before_resources,
        "changed original format reached producer resources/header"
    );
}

#[test]
fn original_producer_waits_for_header_pcm_and_separate_flush_acknowledgements() {
    let limits = RecordingLimits::new(48_000, 3, 8192).unwrap();
    let (binding, receiver, mut settlements) = RecordingTransport::bind(limits);
    thread::scope(|scope| {
        let producer = scope.spawn(move || {
            let mut producer = RecordingProducer::start(binding, limits).unwrap();
            producer.submit(&vec![0.5; 5000 * 3]);
            producer.stop().unwrap()
        });
        let header = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(header.kind(), RecordingTransportRequestKind::Write);
        assert_eq!((header.offset(), header.bytes().len()), (0, 68));
        assert!(receiver.recv_timeout(Duration::from_millis(20)).is_err());
        assert!(!producer.is_finished());
        header.acknowledge().unwrap();

        let initial_flush = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(initial_flush.kind(), RecordingTransportRequestKind::Flush);
        assert_eq!(initial_flush.offset(), 68);
        assert!(receiver.recv_timeout(Duration::from_millis(20)).is_err());
        assert!(!producer.is_finished());
        initial_flush.acknowledge().unwrap();

        let pcm = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(pcm.kind(), RecordingTransportRequestKind::Write);
        assert_eq!((pcm.offset(), pcm.bytes().len()), (68, 16380));
        assert!(receiver.recv_timeout(Duration::from_millis(20)).is_err());
        assert!(!producer.is_finished());
        pcm.acknowledge().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "original blocked producer completion"
            );
            match receiver.recv_timeout(Duration::from_millis(1)) {
                Ok(request) => request.acknowledge().unwrap(),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {}
            }
            if let Ok(barrier) = settlements.recv_barrier(Duration::ZERO) {
                assert!(settlements.owns_barrier(&barrier));
                assert_eq!(barrier.frames(), 5000);
                barrier.acknowledge().unwrap();
            }
            if let Ok(joined) = settlements.recv_joined(Duration::ZERO) {
                assert!(settlements.owns_completion(&joined));
                assert_eq!(joined.frames(), 5000);
                break;
            }
        }
        assert_eq!(producer.join().unwrap().frames, 5000);
    });
}

#[test]
fn original_initialization_refuses_holes_overflow_and_incomplete_flush_without_dispatch() {
    assert!(size_of::<Option<Initialization>>() <= 128);
    for case in 0..3 {
        let (mut sink, receiver) = sink(Duration::from_secs(1), 1024);
        sink.initialization = Some(Initialization::new());
        match case {
            0 => assert!(sink.seek(SeekFrom::Start(1)).is_err()),
            1 => assert!(sink.write_all(&[0; 69]).is_err()),
            _ => {
                sink.write_all(&[0; 67]).unwrap();
                assert!(sink.flush().is_err());
            }
        }
        assert!(sink.write_all(&[0]).is_err());
        assert!(sink.flush().is_err());
        assert!(receiver.recv_timeout(Duration::ZERO).is_err());
    }
}

#[test]
fn original_producer_start_refuses_denied_or_dropped_header_and_flush() {
    for fail_flush in [false, true] {
        for deny in [false, true] {
            let limits = RecordingLimits::new(48_000, 3, 8192).unwrap();
            let (binding, receiver, _settlements) = RecordingTransport::bind(limits);
            thread::scope(|scope| {
                let start = scope.spawn(move || RecordingProducer::start(binding, limits).is_err());
                let header = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
                assert_eq!((header.offset(), header.bytes().len()), (0, 68));
                let request = if fail_flush {
                    header.acknowledge().unwrap();
                    let flush = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
                    assert_eq!(flush.kind(), RecordingTransportRequestKind::Flush);
                    flush
                } else {
                    header
                };
                if deny {
                    request.deny().unwrap();
                } else {
                    drop(request);
                }
                assert!(start.join().unwrap());
                assert!(receiver.recv_timeout(Duration::ZERO).is_err());
            });
        }
    }
}

#[test]
fn original_initialization_header_and_flush_failure_are_sticky_without_retry() {
    for fail_flush in [false, true] {
        for outcome in 0..3 {
            let (mut sink, mut receiver) = sink(Duration::from_millis(20), 1024);
            sink.initialization = Some(Initialization::new());
            sink.write_all(&[7; 68]).unwrap();
            sink.seek(SeekFrom::Start(4)).unwrap();
            sink.write_all(&[8; 4]).unwrap();
            sink.seek(SeekFrom::Start(68)).unwrap();
            assert!(receiver.recv_timeout(Duration::ZERO).is_err());
            thread::scope(|scope| {
                let owner_receiver = &mut receiver;
                let owner = scope.spawn(move || {
                    let header = owner_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
                    assert_eq!((header.offset(), header.bytes().len()), (0, 68));
                    assert_eq!(&header.bytes()[4..8], &[8; 4]);
                    let request = if fail_flush {
                        header.acknowledge().unwrap();
                        let flush = owner_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
                        assert_eq!(flush.kind(), RecordingTransportRequestKind::Flush);
                        flush
                    } else {
                        header
                    };
                    match outcome {
                        0 => {
                            request.deny().unwrap();
                            None
                        }
                        1 => {
                            drop(request);
                            None
                        }
                        _ => Some(request),
                    }
                });
                assert!(sink.flush().is_err());
                if let Some(late) = owner.join().unwrap() {
                    assert!(late.acknowledge().is_err());
                }
            });
            assert!(sink.initialization.is_none());
            assert!(sink.write_all(&[1]).is_err());
            assert!(sink.flush().is_err());
            assert!(sink.seek(SeekFrom::Start(0)).is_err());
            assert!(receiver.recv_timeout(Duration::ZERO).is_err());
        }
    }
    let (mut sink, receiver) = sink(Duration::from_secs(1), 1024);
    sink.initialization = Some(Initialization::new());
    sink.write_all(&[0; 68]).unwrap();
    drop(receiver);
    assert!(sink.flush().is_err());
    assert!(sink.flush().is_err());
}

#[test]
fn serializer_waits_for_exact_request_and_limits_each_batch() {
    let (mut sink, receiver) = sink(Duration::from_secs(2), 65536);
    thread::scope(|scope| {
        let writer = scope.spawn(move || {
            sink.write_all(&vec![7; 32769]).unwrap();
            sink.flush().unwrap();
            assert_eq!(sink.position, 32769);
        });
        for (offset, size) in [(0, 16384), (16384, 16384), (32768, 1)] {
            let request = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(request.offset(), offset);
            assert_eq!(request.bytes(), vec![7; size]);
            assert_eq!(
                receiver.recv_timeout(Duration::from_millis(10)).err(),
                Some(RecvTimeoutError::Timeout)
            );
            request.acknowledge().unwrap();
        }
        let flush = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(flush.kind(), RecordingTransportRequestKind::Flush);
        assert_eq!(flush.offset(), 32769);
        flush.acknowledge().unwrap();
        writer.join().unwrap();
    });
}

#[test]
fn timeout_retains_exact_request_but_late_ack_cannot_restart_serializer() {
    let (mut sink, receiver) = sink(Duration::from_millis(20), 1024);
    assert!(sink.write_all(&[1, 2, 3]).is_err());
    let request = receiver.recv_timeout(Duration::ZERO).unwrap();
    assert_eq!(request.bytes(), &[1, 2, 3]);
    assert_eq!(request.offset(), 0);
    assert!(request.acknowledge().is_err());
    assert_eq!(sink.position, 0);
    assert!(sink.write_all(&[4]).is_err());
    assert!(sink.flush().is_err());
    assert!(sink.seek(SeekFrom::Start(0)).is_err());
    assert_eq!(
        receiver.recv_timeout(Duration::ZERO).err(),
        Some(RecvTimeoutError::Timeout)
    );
}

#[test]
fn refusal_dropped_request_and_disconnected_owner_are_sticky() {
    for deny in [false, true] {
        let (mut sink, receiver) = sink(Duration::from_secs(1), 1024);
        thread::scope(|scope| {
            let owner = scope.spawn(move || {
                let request = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
                if deny {
                    request.deny().unwrap();
                } else {
                    drop(request);
                }
            });
            assert!(sink.write_all(&[9]).is_err());
            assert!(sink.write_all(&[10]).is_err());
            owner.join().unwrap();
        });
    }
    let (mut sink, receiver) = sink(Duration::from_secs(1), 1024);
    drop(receiver);
    assert!(sink.flush().is_err());
    assert!(sink.write_all(&[1]).is_err());
}

#[test]
fn checked_cursor_and_budget_reject_without_emitting_a_request() {
    for from in [
        SeekFrom::Start(u64::MAX),
        SeekFrom::Current(-1),
        SeekFrom::End(0),
    ] {
        let (mut sink, receiver) = sink(Duration::from_secs(1), 1024);
        assert!(sink.seek(from).is_err());
        assert!(sink.seek(SeekFrom::Start(0)).is_err());
        assert_eq!(
            receiver.recv_timeout(Duration::ZERO).err(),
            Some(RecvTimeoutError::Timeout)
        );
    }
    let (mut sink, receiver) = sink(Duration::from_secs(1), 1024);
    assert_eq!(sink.seek(SeekFrom::Start(1024)).unwrap(), 1024);
    assert!(sink.write_all(&[1]).is_err());
    assert_eq!(
        receiver.recv_timeout(Duration::ZERO).err(),
        Some(RecvTimeoutError::Timeout)
    );
}
