//! Synthetic owner observations exercise the actual strict codec, not physical qualification.
use super::*;
use crate::operation_journal::namespace_commands::{
    NamespaceDirectoryEndpoint, NamespaceDirectoryExpectation, NamespaceDirectoryLocator,
    NamespaceLeafName,
};
use crate::operation_journal::participants::{
    ParticipantCheckpoint, ParticipantCheckpointState, ParticipantDescriptor, ParticipantId,
    StoredSourceFence,
};
use crate::sample_sources::SourceId;
use serde_json::Value;

#[test]
fn recording_typed_decode_dense_pcm_does_not_enter_generic_value_parser() {
    let proposal = proposal(2, 2048);
    let bytes: Vec<u8> = (0..MAX_RECORDING_CHUNK_BYTES).map(|i| i as u8).collect();
    let chunk = RecordingCapturedChunk::new(
        &proposal,
        Uuid::new_v4(),
        RecordingChunkRole::Pcm { ordinal: 0 },
        u64::from(proposal.format.header_bytes),
        bytes,
    )
    .unwrap();
    let wire = chunk.encode(&proposal).unwrap();
    RECORDING_VALUE_PARSE_ENTRIES.with(|count| count.set(0));
    assert_eq!(
        RecordingCapturedChunk::decode(&wire, &proposal).unwrap(),
        chunk
    );
    assert_eq!(RECORDING_VALUE_PARSE_ENTRIES.with(|count| count.get()), 0);
}

#[test]
fn recording_typed_decode_preserves_noncanonical_and_strict_chunk_acceptance() {
    let proposal = proposal(2, 2048);
    let chunk = RecordingCapturedChunk::new(
        &proposal,
        Uuid::new_v4(),
        RecordingChunkRole::Pcm { ordinal: 0 },
        68,
        (0..MAX_RECORDING_CHUNK_BYTES).map(|i| i as u8).collect(),
    )
    .unwrap();
    let canonical = String::from_utf8(chunk.encode(&proposal).unwrap()).unwrap();
    let noncanonical = format!(" \n{}\t", canonical.replace("\"bytes\"", "\"\\u0062ytes\""));
    let enum_object = canonical.replace("\"role\":\"Stage\"", "\"role\":{\"Stage\":null}");
    for wire in [
        canonical.as_bytes(),
        noncanonical.as_bytes(),
        enum_object.as_bytes(),
    ] {
        let old: RecordingCapturedChunk = decode_tree_value(wire).unwrap();
        assert_eq!(old, chunk);
        assert_eq!(
            RecordingCapturedChunk::decode(wire, &proposal).unwrap(),
            old
        );
    }
    for wire in [
        canonical.replacen("{", "{\"version\":1,", 1),
        canonical.replace("\"ordinal\":0", "\"ordinal\":0,\"ordinal\":0"),
        canonical.replace(
            "\"role\":\"Stage\"",
            "\"role\":{\"Stage\":null,\"Stage\":null}",
        ),
        canonical.replace(
            "\"role\":\"Stage\"",
            "\"role\":{\"Stage\":null,\"Unknown\":null}",
        ),
        canonical.replace("\"offset\":68", "\"offset\":68,\"\\u006fffset\":68"),
        canonical.replacen("{", "{\"unknown\":null,", 1),
        format!("{canonical} null"),
        canonical.replace("\"version\":1", "\"version\":1.0"),
        canonical.replace("\"bytes\":[", "\"bytes\":[256,"),
        format!("{{\"unknown\":{}null{}}}", "[".repeat(100), "]".repeat(100)),
    ] {
        assert!(decode_tree_value::<RecordingCapturedChunk>(wire.as_bytes()).is_err());
        RECORDING_VALUE_PARSE_ENTRIES.with(|count| count.set(0));
        assert!(RecordingCapturedChunk::decode(wire.as_bytes(), &proposal).is_err());
        assert_eq!(RECORDING_VALUE_PARSE_ENTRIES.with(|count| count.get()), 0);
    }
    // Preserve serde's historical array-shaped struct representation as well.
    let json: Value = serde_json::from_str(&canonical).unwrap();
    let array = serde_json::to_vec(&serde_json::json!([
        json["version"],
        json["proposal_hash"],
        json["operation_id"],
        json["output_id"],
        json["work_key"],
        json["chunk_id"],
        json["role"],
        json["offset"],
        json["bytes"]
    ]))
    .unwrap();
    let old: RecordingCapturedChunk = decode_tree_value(&array).unwrap();
    assert_eq!(
        RecordingCapturedChunk::decode(&array, &proposal).unwrap(),
        old
    );
}

pub(super) fn proposal(channels: u16, frames: u64) -> RecordingProposal {
    proposal_with_root(channels, frames).0
}
pub(super) fn proposal_with_root(
    channels: u16,
    frames: u64,
) -> (RecordingProposal, RootRegistrationRef) {
    let source_id = SourceId::from_string("recording-source");
    let source = ParticipantId::new();
    let fs = ParticipantId::new();
    let fence = StoredSourceFence::new(source_id.clone(), "root", "database", 3, 7, 1).unwrap();
    let mut descriptors = vec![
        ParticipantDescriptor::new(
            source,
            ParticipantKind::SourceDatabase,
            ParticipantRequirement::Required,
            Some(fence.clone()),
            None,
        )
        .unwrap(),
        ParticipantDescriptor::new(
            fs,
            ParticipantKind::Filesystem,
            ParticipantRequirement::Required,
            None,
            None,
        )
        .unwrap(),
        ParticipantDescriptor::new(
            ParticipantId::new(),
            ParticipantKind::GlobalLibrary,
            ParticipantRequirement::Required,
            None,
            None,
        )
        .unwrap(),
    ];
    for kind in [
        ParticipantKind::Harvest,
        ParticipantKind::Projection,
        ParticipantKind::Readiness,
    ] {
        descriptors.push(
            ParticipantDescriptor::new(
                ParticipantId::new(),
                kind,
                ParticipantRequirement::Required,
                Some(fence.clone()),
                None,
            )
            .unwrap(),
        );
    }
    let checkpoints = descriptors
        .iter()
        .map(|d| {
            ParticipantCheckpoint::new(d.id(), ParticipantCheckpointState::Pending, None, None)
                .unwrap()
        })
        .collect();
    let participants = ParticipantSet::new(descriptors, checkpoints).unwrap();
    let parent = NamespaceDirectoryEndpoint::new(
        source,
        source_id,
        NamespaceDirectoryLocator::Root,
        NamespaceDirectoryExpectation::new(7, "root", None).unwrap(),
    )
    .unwrap();
    let root = RootRegistrationRef::synthetic_for_test();
    let proposal = RecordingProposal::new(
        OperationId::new(),
        OutputIdentity::new(),
        WorkUnitKey::new(0, 30, fs, WorkUnitRole::Stage),
        participants,
        root,
        root,
        NamespaceTarget::new(parent, NamespaceLeafName::new("capture.wav").unwrap()).unwrap(),
        StoredRelativePath::new("staging.wav").unwrap(),
        RecordingFormat::new(48_000, channels, frames, 68).unwrap(),
    )
    .unwrap();
    (proposal, root)
}
#[test]
fn proposal_codec_freezes_all_identities_and_rejects_untrusted_observations() {
    let original = proposal(2, 10_000);
    assert_eq!(
        RecordingProposal::decode(&original.encode().unwrap()).unwrap(),
        original
    );
    for (field, value) in [
        ("version", Value::from(2)),
        ("output_id", Value::from(Uuid::nil().to_string())),
        ("unexpected", Value::Bool(true)),
    ] {
        let mut wire = serde_json::to_value(&original).unwrap();
        wire[field] = value;
        assert!(RecordingProposal::decode(&serde_json::to_vec(&wire).unwrap()).is_err());
    }
    let mut mismatched = original.clone();
    mismatched.staging_root.owner = Uuid::new_v4();
    assert!(mismatched.encode().is_err());
    mismatched = original.clone();
    mismatched.staging_path = StoredRelativePath::new("capture.wav").unwrap();
    assert!(mismatched.encode().is_err());
    mismatched = original.clone();
    mismatched.work_key = WorkUnitKey::new(
        0,
        30,
        original.destination.parent().participant_id(),
        WorkUnitRole::Stage,
    );
    assert!(mismatched.encode().is_err());
    let mut wire = serde_json::to_value(original).unwrap();
    wire["destination"]["parent"]["source_id"] = Value::from("wrong-source");
    assert!(RecordingProposal::decode(&serde_json::to_vec(&wire).unwrap()).is_err());
}
#[test]
fn immutable_actual_bytes_round_trip_and_conflicting_replay_is_rejected() {
    let proposal = proposal(3, 5000);
    let bytes = vec![255; proposal.format.pcm_chunk_bytes() as usize];
    let chunk = RecordingCapturedChunk::new(
        &proposal,
        Uuid::new_v4(),
        RecordingChunkRole::Pcm { ordinal: 0 },
        68,
        bytes.clone(),
    )
    .unwrap();
    let recovered =
        RecordingCapturedChunk::decode(&chunk.encode(&proposal).unwrap(), &proposal).unwrap();
    assert_eq!(recovered.bytes(), bytes);
    chunk.validate_replay(&recovered, &proposal).unwrap();
    let mut conflict = recovered;
    conflict.bytes[0] = 0;
    assert!(chunk.validate_replay(&conflict, &proposal).is_err());
    let mut changed_proposal = proposal.clone();
    changed_proposal.format.max_frames += 1;
    assert!(chunk.encode(&changed_proposal).is_err());
    let mut conflict = chunk.clone();
    conflict.output_id = Uuid::new_v4();
    assert!(conflict.encode(&proposal).is_err());
    let mut conflict = chunk;
    conflict.chunk_id = Uuid::nil();
    assert!(conflict.encode(&proposal).is_err());
}
#[test]
fn complete_frame_graph_and_header_patch_bounds_cover_all_channel_counts() {
    for channels in 1..=8 {
        let proposal = proposal(channels, 5000);
        let format = proposal.format;
        let last = format.pcm_slots().unwrap() - 1;
        let offset = 68 + u64::from(last) * format.pcm_chunk_bytes();
        let length =
            format.max_frames * format.frame_bytes() - u64::from(last) * format.pcm_chunk_bytes();
        RecordingCapturedChunk::new(
            &proposal,
            Uuid::new_v4(),
            RecordingChunkRole::Pcm { ordinal: last },
            offset,
            vec![0; length as usize],
        )
        .unwrap();
        assert!(
            RecordingCapturedChunk::new(
                &proposal,
                Uuid::new_v4(),
                RecordingChunkRole::Pcm { ordinal: last + 1 },
                offset,
                vec![0; channels as usize * 4]
            )
            .is_err()
        );
        assert!(
            RecordingCapturedChunk::new(
                &proposal,
                Uuid::new_v4(),
                RecordingChunkRole::Pcm { ordinal: 0 },
                68,
                vec![0; channels as usize * 4 - 1]
            )
            .is_err()
        );
        assert!(
            RecordingCapturedChunk::new(
                &proposal,
                Uuid::new_v4(),
                RecordingChunkRole::Pcm { ordinal: 0 },
                u64::MAX,
                vec![0; channels as usize * 4]
            )
            .is_err()
        );
        RecordingCapturedChunk::new(
            &proposal,
            Uuid::new_v4(),
            RecordingChunkRole::Header { revision: 15 },
            64,
            vec![0; 4],
        )
        .unwrap();
        for (revision, offset, length) in [(16, 0, 4), (0, 67, 2), (0, 0, 129)] {
            assert!(
                RecordingCapturedChunk::new(
                    &proposal,
                    Uuid::new_v4(),
                    RecordingChunkRole::Header { revision },
                    offset,
                    vec![0; length]
                )
                .is_err()
            );
        }
    }
}
#[test]
fn finite_format_checks_riff_and_retained_graph_capacity_before_work() {
    for (rate, channels, frames, header) in [
        (0, 2, 1, 68),
        (192001, 2, 1, 68),
        (48000, 0, 1, 68),
        (48000, 9, 1, 68),
        (48000, 2, 0, 68),
        (48000, 2, u64::MAX, 68),
        (48000, 2, 1, 129),
        (48000, 2, 1, 43),
    ] {
        assert!(RecordingFormat::new(rate, channels, frames, header).is_err());
    }
    for channels in 1..=8 {
        let frame_bytes = u64::from(channels) * 4;
        let frames =
            MAX_RECORDING_CHUNK_BYTES as u64 / frame_bytes * u64::from(MAX_RECORDING_PCM_CHUNKS);
        let format = RecordingFormat::new(192000, channels, frames, 68).unwrap();
        assert_eq!(format.pcm_slots().unwrap(), MAX_RECORDING_PCM_CHUNKS);
        assert!(RecordingFormat::new(192000, channels, frames + 1, 68).is_err());
    }
}
#[test]
fn strict_chunk_codec_rejects_duplicate_unknown_and_oversized_input() {
    let proposal = proposal(2, 100);
    let chunk = RecordingCapturedChunk::new(
        &proposal,
        Uuid::new_v4(),
        RecordingChunkRole::Header { revision: 0 },
        0,
        vec![0; 68],
    )
    .unwrap();
    let encoded = chunk.encode(&proposal).unwrap();
    let mut duplicate = b"{\"version\":1,".to_vec();
    duplicate.extend_from_slice(&encoded[1..]);
    assert!(RecordingCapturedChunk::decode(&duplicate, &proposal).is_err());
    for (field, value) in [
        ("version", Value::from(2)),
        ("unknown", Value::Bool(true)),
        ("role", serde_json::json!({"FutureRole":{}})),
    ] {
        let mut wire = serde_json::to_value(&chunk).unwrap();
        wire[field] = value;
        assert!(
            RecordingCapturedChunk::decode(&serde_json::to_vec(&wire).unwrap(), &proposal).is_err()
        );
    }
    let mut wire = serde_json::to_value(&chunk).unwrap();
    wire["role"]["Header"]["extra"] = Value::Bool(true);
    assert!(
        RecordingCapturedChunk::decode(&serde_json::to_vec(&wire).unwrap(), &proposal).is_err()
    );
    assert!(
        RecordingCapturedChunk::decode(&vec![b' '; MAX_RECORDING_VALUE_BYTES + 1], &proposal)
            .is_err()
    );
}
