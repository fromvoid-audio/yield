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

fn proposal(channels: u16, frames: u64) -> RecordingProposal {
    let source_id = SourceId::from_string("recording-source");
    let source = ParticipantId::new();
    let fs = ParticipantId::new();
    let participants = ParticipantSet::new(
        vec![
            ParticipantDescriptor::new(
                source,
                ParticipantKind::SourceDatabase,
                ParticipantRequirement::Required,
                Some(
                    StoredSourceFence::new(source_id.clone(), "root", "database", 3, 7, 1).unwrap(),
                ),
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
        ],
        [source, fs]
            .into_iter()
            .map(|id| {
                ParticipantCheckpoint::new(id, ParticipantCheckpointState::Pending, None, None)
                    .unwrap()
            })
            .collect(),
    )
    .unwrap();
    let parent = NamespaceDirectoryEndpoint::new(
        source,
        source_id,
        NamespaceDirectoryLocator::Root,
        NamespaceDirectoryExpectation::new(7, "root", None).unwrap(),
    )
    .unwrap();
    let root = RootRegistrationRef::synthetic_for_test();
    RecordingProposal::new(
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
    .unwrap()
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
