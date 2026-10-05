//! Real hound bytes exercise production reconstruction; namespace/accounting inputs are
//! explicitly synthetic observations, never a live admission or physical qualification.
use super::*;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Trace {
    position: usize,
    bytes: Vec<u8>,
    writes: Vec<(u64, Vec<u8>)>,
}
struct TraceSink(Arc<Mutex<Trace>>);
impl Write for TraceSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut trace = self.0.lock().unwrap();
        let position = trace.position;
        let end = position.checked_add(bytes.len()).unwrap();
        let size = trace.bytes.len().max(end);
        trace.bytes.resize(size, 0);
        trace.bytes[position..end].copy_from_slice(bytes);
        trace.writes.push((position as u64, bytes.to_vec()));
        trace.position = end;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Seek for TraceSink {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let mut trace = self.0.lock().unwrap();
        let position = match position {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(n) => trace.position as i128 + i128::from(n),
            SeekFrom::End(n) => trace.bytes.len() as i128 + i128::from(n),
        };
        trace.position = usize::try_from(position).unwrap();
        Ok(trace.position as u64)
    }
}
struct Fixture {
    proposal: RecordingProposal,
    inventory: RecordingPrefixInventory,
    headers: Vec<RecordingCapturedChunk>,
    pcm: Vec<RecordingCapturedChunk>,
    bytes: Vec<u8>,
    raw_pcm_write_lengths: Vec<usize>,
}
fn accounting(samples: u64) -> RecordingSampleAccounting {
    RecordingSampleAccounting {
        accepted: samples,
        encoded: samples,
        abandoned: 0,
        unconfirmed: 0,
        rejected: 0,
        overruns: 0,
        complete: true,
        overflowed: false,
        joined_successfully: true,
        writer_failed: false,
    }
}
fn fixture(channels: u16, frames: u64) -> Fixture {
    let proposal = super::super::tests::proposal(channels, frames.max(1) + 100);
    let trace = Arc::new(Mutex::new(Trace::default()));
    let mut encoder = hound::WavWriter::new(
        BufWriter::with_capacity(16384, TraceSink(Arc::clone(&trace))),
        hound::WavSpec {
            channels,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .unwrap();
    encoder.flush().unwrap(); // Actual reson initialization includes both length rewrites.
    let initial_writes = trace.lock().unwrap().writes.clone();
    assert!(initial_writes.len() >= 3);
    let mut initial = InitialRecordingHeader::new(&proposal).unwrap();
    for (offset, bytes) in &initial_writes {
        initial.apply_write(*offset, bytes).unwrap();
    }
    let mut headers = vec![initial.finish(Uuid::new_v4()).unwrap()];
    for sample in 0..frames * u64::from(channels) {
        encoder.write_sample((sample % 127) as f32 / 127.0).unwrap();
    }
    encoder.finalize().unwrap();
    let trace = trace.lock().unwrap();
    let mut raw_pcm_write_lengths = Vec::new();
    for (offset, bytes) in &trace.writes[initial_writes.len()..] {
        if *offset < RECORDING_WAV_HEADER_BYTES as u64 {
            headers.push(
                RecordingCapturedChunk::new(
                    &proposal,
                    Uuid::new_v4(),
                    RecordingChunkRole::Header {
                        revision: headers.len() as u32,
                    },
                    *offset,
                    bytes.clone(),
                )
                .unwrap(),
            );
        } else {
            raw_pcm_write_lengths.push(bytes.len());
        }
    }
    assert_eq!(headers.len(), 3);
    let pcm = trace.bytes[68..]
        .chunks(proposal.format.pcm_chunk_bytes() as usize)
        .enumerate()
        .map(|(ordinal, bytes)| {
            RecordingCapturedChunk::new(
                &proposal,
                Uuid::new_v4(),
                RecordingChunkRole::Pcm {
                    ordinal: ordinal as u32,
                },
                68 + ordinal as u64 * proposal.format.pcm_chunk_bytes(),
                bytes.to_vec(),
            )
            .unwrap()
        })
        .collect();
    let inventory = RecordingPrefixInventory::new(
        &proposal,
        Uuid::new_v4(),
        17,
        3,
        2,
        RecordingTerminalKind::Stop,
        accounting(frames * u64::from(channels)),
    )
    .unwrap();
    Fixture {
        proposal,
        inventory,
        headers,
        pcm,
        bytes: trace.bytes.clone(),
        raw_pcm_write_lengths,
    }
}
fn verifier(f: &Fixture) -> RecordingPrefixVerifier {
    RecordingPrefixVerifier::new(&f.proposal, f.inventory.clone()).unwrap()
}
fn seal(f: &Fixture) -> RecordingPrefixSeal {
    let mut verifier = verifier(f);
    for page in f.headers.chunks(8) {
        verifier.header_page(verifier.cursor(), page).unwrap();
    }
    for page in f.pcm.chunks(8) {
        verifier.pcm_page(verifier.cursor(), page).unwrap();
    }
    verifier.finish().unwrap()
}

#[test]
fn recording_typed_decode_prefix_inventory_and_seal_preserve_strict_json() {
    let f = fixture(2, 100);
    let sealed = seal(&f);
    for wire in [
        f.inventory.encode(&f.proposal).unwrap(),
        sealed.encode(&f.proposal).unwrap(),
    ] {
        let text = String::from_utf8(wire).unwrap();
        let noncanonical = format!(
            " \n{}\t",
            text.replace("\"version\"", "\"\\u0076ersion\"")
                .replace("\"role\":\"Stage\"", "\"role\":{\"Stage\":null}")
        );
        if text.contains("\"inventory\"") {
            assert_eq!(
                decode_tree_value::<RecordingPrefixSeal>(noncanonical.as_bytes()).unwrap(),
                sealed
            );
        } else {
            assert_eq!(
                decode_tree_value::<RecordingPrefixInventory>(noncanonical.as_bytes()).unwrap(),
                f.inventory
            );
        }
        RECORDING_VALUE_PARSE_ENTRIES.with(|count| count.set(0));
        if text.contains("\"inventory\"") {
            assert_eq!(
                RecordingPrefixSeal::decode(noncanonical.as_bytes(), &f.proposal).unwrap(),
                sealed
            );
        } else {
            assert_eq!(
                RecordingPrefixInventory::decode(noncanonical.as_bytes(), &f.proposal).unwrap(),
                f.inventory
            );
        }
        assert_eq!(RECORDING_VALUE_PARSE_ENTRIES.with(|count| count.get()), 0);
        for malformed in [
            text.replacen("{", "{\"version\":1,", 1),
            text.replace("\"accepted\":200", "\"accepted\":200,\"accepted\":200"),
            text.replace(
                "\"role\":\"Stage\"",
                "\"role\":{\"Stage\":null,\"Stage\":null}",
            ),
            text.replace(
                "\"role\":\"Stage\"",
                "\"role\":{\"Stage\":null,\"Unknown\":null}",
            ),
            text.replace(
                "\"terminal\":\"Stop\"",
                "\"terminal\":{\"Stop\":null,\"Stop\":null}",
            ),
            text.replace(
                "\"terminal\":\"Stop\"",
                "\"terminal\":{\"Stop\":null,\"Unknown\":null}",
            ),
            text.replacen("{", "{\"unknown\":null,", 1),
            format!("{text} null"),
            format!("{{\"unknown\":{}null{}}}", "[".repeat(100), "]".repeat(100)),
        ] {
            RECORDING_VALUE_PARSE_ENTRIES.with(|count| count.set(0));
            if text.contains("\"inventory\"") {
                assert!(decode_tree_value::<RecordingPrefixSeal>(malformed.as_bytes()).is_err());
                assert!(RecordingPrefixSeal::decode(malformed.as_bytes(), &f.proposal).is_err());
            } else {
                assert!(
                    decode_tree_value::<RecordingPrefixInventory>(malformed.as_bytes()).is_err()
                );
                assert!(
                    RecordingPrefixInventory::decode(malformed.as_bytes(), &f.proposal).is_err()
                );
            }
            // The comparison decoder above is the only generic parser in this turn.
            assert_eq!(RECORDING_VALUE_PARSE_ENTRIES.with(|count| count.get()), 1);
        }
    }
}

#[test]
fn actual_hound_matrix_matches_full_object_hash_and_exact_frames() {
    for channels in 1..=8 {
        let slot_frames = 16384 / (u64::from(channels) * 4);
        for frames in [0, 1, 123, slot_frames * 2, slot_frames * 2 + 7] {
            let f = fixture(channels, frames);
            let observed = seal(&f);
            assert_eq!(observed.frames(), frames);
            assert_eq!(observed.bytes(), f.bytes.len() as u64);
            assert_eq!(observed.content_hash(), *blake3::hash(&f.bytes).as_bytes());
            assert_eq!(
                observed.header_hash,
                *blake3::hash(&f.bytes[..68]).as_bytes()
            );
            let bytes = observed.encode(&f.proposal).unwrap();
            assert!(bytes.len() <= MAX_CONTROL_BYTES);
            assert_eq!(
                RecordingPrefixSeal::decode(&bytes, &f.proposal).unwrap(),
                observed
            );
            assert_eq!(
                RecordingPrefixInventory::decode(
                    &f.inventory.encode(&f.proposal).unwrap(),
                    &f.proposal
                )
                .unwrap(),
                f.inventory
            );
        }
    }
}

#[test]
fn actual_buffered_calls_can_split_frames_but_reconstructed_slots_do_not() {
    for channels in [3, 5, 6, 7] {
        let f = fixture(channels, 10000);
        let frame = usize::from(channels) * 4;
        assert!(
            f.raw_pcm_write_lengths
                .iter()
                .any(|length| length % frame != 0)
        );
        assert!(f.pcm.iter().all(|chunk| chunk.bytes.len() % frame == 0));
        assert_eq!(seal(&f).content_hash(), *blake3::hash(&f.bytes).as_bytes());
    }
}

#[test]
fn initial_normalization_requires_real_coverage_and_completed_initial_flush() {
    let f = fixture(2, 1);
    let header = &f.headers[0].bytes;
    let mut initial = InitialRecordingHeader::new(&f.proposal).unwrap();
    initial.apply_write(0, &header[..67]).unwrap();
    assert!(initial.finish(Uuid::new_v4()).is_err());
    let mut initial = InitialRecordingHeader::new(&f.proposal).unwrap();
    let mut before_flush = header.clone();
    before_flush[4..8].fill(0);
    initial.apply_write(0, &before_flush).unwrap();
    assert!(initial.finish(Uuid::new_v4()).is_err());
    let mut initial = InitialRecordingHeader::new(&f.proposal).unwrap();
    assert!(initial.apply_write(u64::MAX, &[1]).is_err());
    assert!(initial.apply_write(68, &[1]).is_err());
    assert!(initial.apply_write(0, &[]).is_err());
    initial.apply_write(0, header).unwrap();
    assert!(initial.finish(Uuid::nil()).is_err());
}

#[test]
fn every_header_format_or_length_substitution_fails_without_page_progress() {
    let f = fixture(3, 123);
    for offset in [
        0, 4, 8, 12, 16, 20, 22, 24, 28, 32, 34, 36, 38, 40, 44, 59, 60, 64, 67,
    ] {
        let mut headers = f.headers.clone();
        headers[0].bytes[offset] ^= 1;
        let mut verifier = verifier(&f);
        let cursor = verifier.cursor();
        assert!(
            verifier.header_page(cursor, &headers).is_err(),
            "offset {offset}"
        );
        assert_eq!(verifier.cursor(), cursor);
        assert!(verifier.finish().is_err());
    }
    let mut headers = f.headers.clone();
    headers[2].bytes[0] ^= 4;
    let mut verifier = verifier(&f);
    assert!(verifier.header_page(verifier.cursor(), &headers).is_err());
}

#[test]
fn missing_reordered_extra_and_non_frame_pcm_cannot_seal() {
    let f = fixture(3, 10000);
    let mut verified_header = verifier(&f);
    assert!(
        verified_header
            .pcm_page(verified_header.cursor(), &f.pcm[..1])
            .is_err()
    );
    assert!(
        verified_header
            .header_page(verified_header.cursor(), &f.headers[1..])
            .is_err()
    );
    verified_header
        .header_page(verified_header.cursor(), &f.headers)
        .unwrap();
    assert!(verified_header.clone().finish().is_err());
    assert!(
        verified_header
            .pcm_page(verified_header.cursor(), &f.pcm[1..])
            .is_err()
    );
    let mut malformed = f.pcm[0].clone();
    malformed.bytes.pop();
    let cursor = verified_header.cursor();
    assert!(verified_header.pcm_page(cursor, &[malformed]).is_err());
    assert_eq!(verified_header.cursor(), cursor);
    let mut short = f.pcm[0].clone();
    short.bytes.truncate(short.bytes.len() - 12);
    assert!(verified_header.pcm_page(cursor, &[short]).is_err());
    verified_header.pcm_page(cursor, &f.pcm).unwrap();
    assert!(
        verified_header
            .pcm_page(verified_header.cursor(), &f.pcm[..1])
            .is_err()
    );
    assert!(
        verified_header
            .header_page(verified_header.cursor(), &f.headers[..1])
            .is_err()
    );
    assert_eq!(
        verified_header.finish().unwrap().content_hash(),
        *blake3::hash(&f.bytes).as_bytes()
    );
}

#[test]
fn short_tail_must_equal_successfully_encoded_coverage_not_only_proposal_budget() {
    let f = fixture(2, 2050);
    let mut verifier = verifier(&f);
    verifier.header_page(verifier.cursor(), &f.headers).unwrap();
    let mut pcm = f.pcm.clone();
    pcm.last_mut().unwrap().bytes.extend_from_slice(&[0; 8]);
    assert!(pcm.last().unwrap().validate_against(&f.proposal).is_ok());
    assert!(verifier.pcm_page(verifier.cursor(), &pcm).is_err());
    assert_eq!(verifier.cursor().next_pcm, 0);
}

#[test]
fn page_credits_stale_and_other_frozen_inventory_cursors_fail_atomically() {
    let f = fixture(1, 40000); // Ten PCM slots exceed one eight-slot turn.
    assert!(f.pcm.len() > 8);
    let mut verifier = verifier(&f);
    let before = verifier.cursor();
    assert!(verifier.header_page(before, &[]).is_err());
    let oversized = vec![f.headers[0].clone(); 9];
    assert!(verifier.header_page(before, &oversized).is_err());
    verifier.header_page(before, &f.headers).unwrap();
    let after_header = verifier.cursor();
    assert!(verifier.header_page(before, &f.headers).is_err());
    assert!(verifier.pcm_page(after_header, &f.pcm[..9]).is_err());
    let mut other = f.inventory.clone();
    other.freeze_sequence += 1;
    let other = RecordingPrefixVerifier::new(&f.proposal, other).unwrap();
    assert!(verifier.pcm_page(other.cursor(), &f.pcm[..1]).is_err());
    let mut bad_page = f.pcm[..8].to_vec();
    bad_page[7].offset += 4;
    assert!(verifier.pcm_page(after_header, &bad_page).is_err());
    assert_eq!(verifier.cursor(), after_header);
    verifier.pcm_page(after_header, &f.pcm[..8]).unwrap();
    assert!(verifier.pcm_page(after_header, &f.pcm[..8]).is_err());
    verifier.pcm_page(verifier.cursor(), &f.pcm[8..]).unwrap();
    assert_eq!(
        verifier.finish().unwrap().content_hash(),
        *blake3::hash(&f.bytes).as_bytes()
    );
}

#[test]
fn chunk_proposal_output_work_and_revision_substitution_is_rejected() {
    let f = fixture(2, 128);
    for field in [
        "proposal_hash",
        "output_id",
        "operation_id",
        "work_key",
        "chunk_id",
    ] {
        let mut chunk = f.headers[0].clone();
        match field {
            "proposal_hash" => chunk.proposal_hash[0] ^= 1,
            "output_id" => chunk.output_id = Uuid::new_v4(),
            "operation_id" => chunk.operation_id = OperationId::new(),
            "work_key" => chunk.work_key = super::super::tests::proposal(2, 1000).work_key,
            "chunk_id" => chunk.chunk_id = Uuid::nil(),
            _ => unreachable!(),
        }
        let mut verifier = verifier(&f);
        assert!(
            verifier.header_page(verifier.cursor(), &[chunk]).is_err(),
            "{field}"
        );
    }
    let mut verifier = verifier(&f);
    let mut wrong = f.headers.clone();
    wrong[1].role = RecordingChunkRole::Header { revision: 2 };
    assert!(verifier.header_page(verifier.cursor(), &wrong).is_err());
}

#[test]
fn unknown_overflow_failed_incomplete_or_unaccounted_input_never_seals() {
    let f = fixture(2, 128);
    for kind in 0..10 {
        let mut inventory = f.inventory.clone();
        match kind {
            0 => inventory.accounting.complete = false,
            1 => inventory.accounting.overflowed = true,
            2 => inventory.accounting.writer_failed = true,
            3 => inventory.accounting.joined_successfully = false,
            4 => {
                inventory.accounting.unconfirmed = 2;
                inventory.accounting.accepted += 2;
            }
            5 => inventory.accounting.accepted += 2,
            6 => {
                inventory.accounting.abandoned = 2;
                inventory.accounting.accepted += 2;
            }
            7 => {
                inventory.accounting.accepted = u64::MAX;
                inventory.accounting.encoded = u64::MAX - 1;
                inventory.accounting.abandoned = 2;
            }
            8 => {
                inventory.accounting.accepted += 1;
                inventory.accounting.encoded += 1;
            }
            9 => {
                inventory.accounting.encoded = (f.proposal.format.max_frames + 1) * 2;
                inventory.accounting.accepted = inventory.accounting.encoded;
            }
            _ => unreachable!(),
        }
        assert!(
            RecordingPrefixVerifier::new(&f.proposal, inventory).is_err(),
            "{kind}"
        );
    }
    let mut cancelled = f.inventory.clone();
    cancelled.terminal = RecordingTerminalKind::Cancel;
    cancelled.accounting.abandoned = 12;
    cancelled.accounting.accepted += 12;
    cancelled.accounting.rejected = 20;
    cancelled.accounting.overruns = 3;
    let mut verifier = RecordingPrefixVerifier::new(&f.proposal, cancelled.clone()).unwrap();
    verifier.header_page(verifier.cursor(), &f.headers).unwrap();
    verifier.pcm_page(verifier.cursor(), &f.pcm).unwrap();
    let seal = verifier.finish().unwrap();
    assert_eq!(seal.inventory(), &cancelled);
    assert_eq!(seal.content_hash(), *blake3::hash(&f.bytes).as_bytes());
}

#[test]
fn strict_inventory_and_seal_codec_rejects_versions_duplicates_bounds_and_conflicts() {
    let f = fixture(2, 128);
    let observed = seal(&f);
    let bytes = observed.encode(&f.proposal).unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    let duplicate = text.replacen("\"version\":1", "\"version\":1,\"version\":1", 1);
    assert!(RecordingPrefixSeal::decode(duplicate.as_bytes(), &f.proposal).is_err());
    assert!(RecordingPrefixSeal::decode(&vec![b' '; MAX_CONTROL_BYTES + 1], &f.proposal).is_err());
    for field in ["version", "bytes", "frames", "unexpected"] {
        let mut value = serde_json::to_value(&observed).unwrap();
        value[field] = serde_json::Value::from(9000);
        assert!(
            RecordingPrefixSeal::decode(&serde_json::to_vec(&value).unwrap(), &f.proposal).is_err()
        );
    }
    for change in 0..7 {
        let mut inventory = f.inventory.clone();
        match change {
            0 => inventory.version = 2,
            1 => inventory.freeze_sequence = 0,
            2 => inventory.generation = 0,
            3 => inventory.terminal_id = Uuid::nil(),
            4 => inventory.last_header_revision = 0,
            5 => inventory.last_header_revision = 16,
            6 => inventory.output_id = Uuid::new_v4(),
            _ => unreachable!(),
        }
        assert!(inventory.encode(&f.proposal).is_err());
        assert!(
            RecordingPrefixInventory::decode(&serde_json::to_vec(&inventory).unwrap(), &f.proposal)
                .is_err()
        );
    }
    assert!(observed.validate_replay(&observed, &f.proposal).is_ok());
    let mut changed = observed.clone();
    changed.content_hash[0] ^= 1;
    assert!(observed.validate_replay(&changed, &f.proposal).is_err());
    changed = observed.clone();
    changed.inventory.accounting.rejected += 1;
    assert!(observed.validate_replay(&changed, &f.proposal).is_err());
    assert!(RecordingPrefixSeal::decode(&bytes, &super::super::tests::proposal(2, 228)).is_err());
}

#[test]
fn old_header_extent_and_unreserved_maximum_proposals_remain_unsupported() {
    let f = fixture(2, 128);
    let mut old = f.proposal.clone();
    old.format.header_bytes = 44;
    assert!(old.validate().is_ok());
    assert!(InitialRecordingHeader::new(&old).is_err());
    let proposal = super::super::tests::proposal(1, 8176 * 4096);
    assert_eq!(proposal.format.pcm_slots().unwrap(), 8176);
    assert!(
        RecordingPrefixInventory::new(
            &proposal,
            Uuid::new_v4(),
            1,
            1,
            2,
            RecordingTerminalKind::Stop,
            accounting(0)
        )
        .is_err()
    );
}
