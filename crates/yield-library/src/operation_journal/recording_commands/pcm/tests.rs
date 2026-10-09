//! CPU data evidence; acknowledgements below are in-memory fixtures, never physical grants.
use super::*;
use reson::recording::{
    RecordingLimits, RecordingProducer, RecordingTransport, RecordingTransportRequestKind,
};
use std::{
    sync::mpsc::RecvTimeoutError,
    time::{Duration, Instant},
};

#[test]
fn arbitrary_write_boundaries_preserve_every_byte_and_terminal_retry() {
    for channels in 1..=8 {
        let proposal = recording_proposal_for_test(channels, 18000);
        let bytes: Vec<u8> = (0..2051 * usize::from(channels) * 4)
            .map(|n| (n % 251) as u8)
            .collect();
        for boundary in [1, 31, 4097, 16384] {
            let mut cursor = RecordingPcmNormalizer::new(&proposal).unwrap();
            let mut retained = Vec::new();
            let mut consumed = 0;
            // Keep a genuine raw tail, independent of complete-frame chunk boundaries.
            let tail = bytes.len().min(16000);
            while consumed < bytes.len() - tail {
                let count = boundary.min(bytes.len() - tail - consumed);
                let complete =
                    (cursor.carry_len + count) / proposal.format().pcm_chunk_bytes() as usize;
                let ids: Vec<_> = (0..complete).map(|_| Uuid::new_v4()).collect();
                let batch = cursor
                    .prepare(
                        &proposal,
                        68 + consumed as u64,
                        &bytes[consumed..consumed + count],
                        &ids,
                    )
                    .unwrap();
                for chunk in batch.chunks() {
                    retained.extend_from_slice(&chunk.bytes);
                }
                cursor.accept(batch).unwrap();
                consumed += count;
            }
            let terminal = Uuid::new_v4();
            let plan = cursor
                .prepare_terminal(
                    &proposal,
                    terminal,
                    68 + consumed as u64,
                    &bytes[consumed..],
                    68 + bytes.len() as u64,
                )
                .unwrap();
            let retry = cursor
                .prepare_terminal(
                    &proposal,
                    terminal,
                    68 + consumed as u64,
                    &bytes[consumed..],
                    68 + bytes.len() as u64,
                )
                .unwrap();
            assert_eq!(plan.chunks(), retry.chunks());
            for chunk in plan.chunks() {
                retained.extend_from_slice(&chunk.bytes);
            }
            cursor.accept(plan).unwrap();
            assert!(cursor.accept(retry).is_err());
            assert!(
                cursor
                    .prepare(&proposal, 68 + bytes.len() as u64, &[0; 4], &[])
                    .is_err()
            );
            assert_eq!(retained, bytes, "channels={channels}, boundary={boundary}");
        }
    }
}

#[test]
fn foreign_stale_gap_identity_and_extent_refusals_do_not_advance() {
    let proposal = recording_proposal_for_test(3, 4096);
    let mut cursor = RecordingPcmNormalizer::new(&proposal).unwrap();
    let foreign = RecordingPcmNormalizer::new(&proposal).unwrap();
    assert!(
        cursor
            .accept(foreign.prepare(&proposal, 68, &[1; 13], &[]).unwrap())
            .is_err()
    );
    assert!(cursor.prepare(&proposal, 69, &[1; 13], &[]).is_err());
    assert!(
        cursor
            .prepare(&proposal, 68, &[1; 16384], &[Uuid::nil()])
            .is_err()
    );
    assert!(cursor.prepare(&proposal, 68, &[1; 16384], &[]).is_err());
    let first = cursor.prepare(&proposal, 68, &[1; 13], &[]).unwrap();
    let stale = cursor.prepare(&proposal, 68, &[1; 13], &[]).unwrap();
    cursor.accept(first).unwrap();
    assert!(cursor.accept(stale).is_err());
    assert!(
        cursor
            .prepare_terminal(&proposal, Uuid::new_v4(), 81, &[], 81)
            .is_err()
    ); // partial frame
    assert!(
        cursor
            .prepare_terminal(&proposal, Uuid::new_v4(), 81, &[0; 11], 93)
            .is_err()
    );
    let terminal = cursor
        .prepare_terminal(&proposal, Uuid::new_v4(), 81, &[0; 11], 92)
        .unwrap();
    assert_eq!(terminal.chunks()[0].bytes.len(), 24);
    cursor.accept(terminal).unwrap();
    let short = recording_proposal_for_test(1, 1);
    let cursor = RecordingPcmNormalizer::new(&short).unwrap();
    assert!(
        cursor
            .prepare_terminal(&short, Uuid::new_v4(), 68, &[0; 8], 76)
            .is_err()
    );
}

#[test]
fn original_serializer_transport_and_barrier_normalize_all_channel_counts() {
    for channels in 1..=8 {
        for frames in [0, 1, 513, 7000] {
            let proposal = recording_proposal_for_test(channels, 16384);
            let limits = RecordingLimits::new(48000, channels, 16384).unwrap();
            let (binding, receiver, mut settlements) = RecordingTransport::bind(limits);
            let samples: Vec<_> = (0..frames * usize::from(channels))
                .map(|n| (n % 61) as f32 / 64.0)
                .collect();
            let expected: Vec<_> = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
            let retained = std::thread::scope(|scope| {
                let owner = scope.spawn(move || {
                    let mut cursor = RecordingPcmNormalizer::new(&proposal).unwrap();
                    let mut pcm = Vec::new();
                    let mut capturing = false;
                    let mut terminal_done = false;
                    let deadline = Instant::now() + Duration::from_secs(10);
                    loop {
                        assert!(Instant::now() < deadline);
                        match receiver.recv_timeout(Duration::from_millis(1)) {
                            Ok(request) => {
                                if capturing
                                    && !terminal_done
                                    && request.kind() == RecordingTransportRequestKind::Write
                                    && request.offset() >= 68
                                {
                                    let count = (cursor.carry_len + request.bytes().len())
                                        / proposal.format().pcm_chunk_bytes() as usize;
                                    let ids: Vec<_> = (0..count).map(|_| Uuid::new_v4()).collect();
                                    let batch = cursor
                                        .prepare(&proposal, request.offset(), request.bytes(), &ids)
                                        .unwrap();
                                    for chunk in batch.chunks() {
                                        pcm.extend_from_slice(&chunk.bytes);
                                    }
                                    cursor.accept(batch).unwrap();
                                }
                                if request.kind() == RecordingTransportRequestKind::Flush {
                                    capturing = true;
                                }
                                request.acknowledge().unwrap();
                            }
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => assert!(terminal_done),
                        }
                        if !terminal_done {
                            if let Ok(barrier) = settlements.recv_barrier(Duration::ZERO) {
                                assert!(settlements.owns_barrier(&barrier));
                                let terminal = Uuid::new_v4();
                                let (_, _, foreign) = RecordingTransport::bind(limits);
                                assert!(
                                    cursor
                                        .prepare_original_terminal(
                                            &proposal, terminal, &foreign, &barrier
                                        )
                                        .is_err()
                                );
                                let plan = cursor
                                    .prepare_original_terminal(
                                        &proposal,
                                        terminal,
                                        &settlements,
                                        &barrier,
                                    )
                                    .unwrap();
                                let retry = cursor
                                    .prepare_original_terminal(
                                        &proposal,
                                        terminal,
                                        &settlements,
                                        &barrier,
                                    )
                                    .unwrap();
                                assert_eq!(plan.chunks(), retry.chunks());
                                let patches: Vec<_> = plan
                                    .chunks()
                                    .iter()
                                    .filter(|chunk| {
                                        matches!(chunk.role, RecordingChunkRole::Header { .. })
                                    })
                                    .collect();
                                assert_eq!(patches.len(), 2);
                                assert_eq!(patches[0].offset, 4);
                                assert_eq!(
                                    patches[0].bytes,
                                    barrier.write_inventory().riff_length_patch()
                                );
                                assert_eq!(patches[1].offset, 64);
                                assert_eq!(
                                    patches[1].bytes,
                                    barrier.write_inventory().data_length_patch()
                                );
                                for chunk in plan.chunks() {
                                    if matches!(chunk.role, RecordingChunkRole::Pcm { .. }) {
                                        pcm.extend_from_slice(&chunk.bytes);
                                    }
                                }
                                cursor.accept(plan).unwrap();
                                terminal_done = true;
                                barrier.acknowledge().unwrap();
                            }
                        }
                        if let Ok(joined) = settlements.recv_joined(Duration::ZERO) {
                            assert!(settlements.owns_completion(&joined));
                            assert_eq!(joined.frames(), frames as u64);
                            return pcm;
                        }
                    }
                });
                let mut producer = RecordingProducer::start(binding, limits).unwrap();
                producer.submit(&samples);
                assert_eq!(producer.stop().unwrap().frames, frames as u64);
                owner.join().unwrap()
            });
            assert_eq!(retained, expected, "channels={channels}, frames={frames}");
        }
    }
}
