//! Strict, storage-neutral observations for the decoded in-place edit workflows.
//!
//! These values describe the inputs and reviewed ranges for a later owner.  They do not contain
//! decoded samples, an analysis result, a source lease, or permission to mutate a source file.
//! The click-repair operation records the source range whose interior is replaced by the
//! reference implementation's cubic smoothstep interpolation between its two neighboring
//! frames.  Duplicate cleanup records the complete reviewed union that one in-place edit removes.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::coordinator::{
    CopyEndpoint, CopyEndpointRole, CopyIntentValidationError, MAX_WAVEFORM_EDIT_FRAMES,
    WaveformEditFrameRange, WaveformFileRevisionObservation, WaveformOutputSpec,
    WaveformSourceSpec,
};
use super::participants::{
    ParticipantKind, ParticipantRequirement, ParticipantSet, ParticipantValidationError,
};

/// Maximum number of reviewed duplicate ranges accepted before canonicalization.
pub const MAX_DECODED_EDIT_RANGES: usize = 4096;

/// One reviewed decoded-edit operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum DecodedEditOperationArgs {
    /// Replace each selected frame using cubic smoothstep interpolation between the immediate
    /// frame before and after the selection.  The operation preserves the source frame count.
    ClickRepair {
        /// Non-empty interior range; both neighboring frames must remain in the source.
        range: WaveformEditFrameRange,
    },
    /// Remove the reviewed union of duplicate ranges in one in-place edit.
    DuplicateCleanup {
        /// Strictly sorted, non-overlapping, non-touching canonical ranges.
        ranges: Vec<WaveformEditFrameRange>,
    },
}

/// Short alias matching the operation naming used by the existing coordinator models.
pub type DecodedEditOperation = DecodedEditOperationArgs;

impl DecodedEditOperationArgs {
    /// Construct a click-repair operation from one explicit range.
    pub const fn click_repair(range: WaveformEditFrameRange) -> Self {
        Self::ClickRepair { range }
    }

    /// Construct duplicate cleanup by sorting and merging the reviewed ranges.
    ///
    /// Directly decoded values must already contain this canonical union; this helper is the
    /// explicit caller-side normalization boundary and is never used by validation or recovery.
    pub fn duplicate_cleanup(
        mut ranges: Vec<WaveformEditFrameRange>,
    ) -> Result<Self, DecodedEditValidationError> {
        if ranges.is_empty() {
            return Err(DecodedEditValidationError::EmptyDuplicateRanges);
        }
        if ranges.len() > MAX_DECODED_EDIT_RANGES {
            return Err(DecodedEditValidationError::TooManyDuplicateRanges {
                actual: ranges.len(),
                maximum: MAX_DECODED_EDIT_RANGES,
            });
        }
        if ranges
            .iter()
            .any(|range| range.start_frame() >= range.end_frame())
        {
            return Err(DecodedEditValidationError::InvalidDuplicateRange);
        }
        ranges.sort_by_key(|range| (range.start_frame(), range.end_frame()));
        let mut merged: Vec<WaveformEditFrameRange> = Vec::with_capacity(ranges.len());
        for range in ranges {
            if let Some(previous) = merged.last_mut() {
                if range.start_frame() <= previous.end_frame() {
                    if range.end_frame() > previous.end_frame() {
                        *previous =
                            WaveformEditFrameRange::new(previous.start_frame(), range.end_frame())
                                .expect("merged ranges remain non-empty");
                    }
                    continue;
                }
            }
            merged.push(range);
        }
        Ok(Self::DuplicateCleanup { ranges: merged })
    }

    /// Borrow the canonical duplicate-cleanup ranges, when this is a cleanup operation.
    pub fn ranges(&self) -> Option<&[WaveformEditFrameRange]> {
        match self {
            Self::DuplicateCleanup { ranges } => Some(ranges),
            Self::ClickRepair { .. } => None,
        }
    }

    /// Return the click-repair range, when this is a click operation.
    pub const fn click_range(&self) -> Option<WaveformEditFrameRange> {
        match self {
            Self::ClickRepair { range } => Some(*range),
            Self::DuplicateCleanup { .. } => None,
        }
    }

    fn validate(&self, total_frames: u64) -> Result<u64, DecodedEditValidationError> {
        match self {
            Self::ClickRepair { range } => {
                if range.start_frame() == 0
                    || range.end_frame() >= total_frames
                    || range.start_frame() >= range.end_frame()
                {
                    return Err(DecodedEditValidationError::ClickNeighborsRequired);
                }
                Ok(total_frames)
            }
            Self::DuplicateCleanup { ranges } => {
                validate_canonical_ranges(ranges, total_frames)?;
                let mut removed = 0u64;
                for range in ranges {
                    removed = removed
                        .checked_add(
                            range
                                .len()
                                .map_err(|_| DecodedEditValidationError::InvalidDuplicateRange)?,
                        )
                        .ok_or(DecodedEditValidationError::OutputArithmeticOverflow)?;
                }
                if removed >= total_frames {
                    return Err(DecodedEditValidationError::FullSourceRemoval);
                }
                total_frames
                    .checked_sub(removed)
                    .ok_or(DecodedEditValidationError::OutputArithmeticOverflow)
            }
        }
    }
}

/// Complete source-bound intent for one decoded in-place edit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecodedEditIntentArgs {
    source: CopyEndpoint,
    source_file_revision: WaveformFileRevisionObservation,
    source_spec: WaveformSourceSpec,
    total_frames: u64,
    operation: DecodedEditOperationArgs,
    output: WaveformOutputSpec,
}

impl DecodedEditIntentArgs {
    /// Construct a bounded decoded-edit observation without opening or changing the source.
    pub fn new(
        source: CopyEndpoint,
        source_file_revision: WaveformFileRevisionObservation,
        source_spec: WaveformSourceSpec,
        total_frames: u64,
        operation: DecodedEditOperationArgs,
        output: WaveformOutputSpec,
    ) -> Result<Self, DecodedEditValidationError> {
        let args = Self {
            source,
            source_file_revision,
            source_spec,
            total_frames,
            operation,
            output,
        };
        args.validate_shape()?;
        Ok(args)
    }

    /// Validate the source endpoint and required Filesystem participant.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), DecodedEditValidationError> {
        self.validate_shape()?;
        participants.validate_for_new_intent()?;
        super::coordinator::CopyIntentArgs::validate_endpoint(
            participants,
            CopyEndpointRole::Source,
            &self.source,
        )
        .map_err(DecodedEditValidationError::SourceParticipant)?;
        if !participants.descriptors().iter().any(|descriptor| {
            descriptor.kind() == ParticipantKind::Filesystem
                && descriptor.requirement() == ParticipantRequirement::Required
        }) {
            return Err(DecodedEditValidationError::RequiredFilesystemParticipantMissing);
        }
        Ok(())
    }

    /// Validate only the storage-neutral shape and operation arithmetic.
    pub fn validate(&self) -> Result<(), DecodedEditValidationError> {
        self.validate_shape()
    }

    /// Borrow the source endpoint observation.
    pub fn source(&self) -> &CopyEndpoint {
        &self.source
    }

    /// Borrow the captured source file revision.
    pub fn source_file_revision(&self) -> &WaveformFileRevisionObservation {
        &self.source_file_revision
    }

    /// Return the source WAV layout.
    pub const fn source_spec(&self) -> WaveformSourceSpec {
        self.source_spec
    }

    /// Return the captured source frame count.
    pub const fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// Borrow the explicit reviewed operation.
    pub fn operation(&self) -> &DecodedEditOperationArgs {
        &self.operation
    }

    /// Return the explicit Float32 output layout.
    pub const fn output(&self) -> WaveformOutputSpec {
        self.output
    }

    /// Return the checked output frame count.
    pub fn output_frames(&self) -> Result<u64, DecodedEditValidationError> {
        self.operation.validate(self.total_frames)
    }

    fn validate_shape(&self) -> Result<(), DecodedEditValidationError> {
        self.source
            .relative_path()
            .validate()
            .map_err(DecodedEditValidationError::InvalidPath)?;
        super::coordinator::validate_source_id(self.source.source_id())
            .map_err(|_| DecodedEditValidationError::InvalidSourceIdentity)?;
        self.source_file_revision
            .validate()
            .map_err(|_| DecodedEditValidationError::InvalidFileRevision)?;
        self.source_spec
            .validate()
            .map_err(|_| DecodedEditValidationError::InvalidSourceSpec)?;
        if self.total_frames == 0 || self.total_frames > MAX_WAVEFORM_EDIT_FRAMES {
            return Err(DecodedEditValidationError::InvalidFrameCount);
        }
        let output_frames = self.operation.validate(self.total_frames)?;
        self.output
            .validate_against_source(self.source_spec)
            .map_err(|_| DecodedEditValidationError::InvalidOutputSpec)?;
        let samples = output_frames
            .checked_mul(u64::from(self.output.channels()))
            .ok_or(DecodedEditValidationError::OutputArithmeticOverflow)?;
        let output_bytes = samples
            .checked_mul(4)
            .and_then(|bytes| bytes.checked_add(60))
            .ok_or(DecodedEditValidationError::OutputArithmeticOverflow)?;
        if output_bytes > u64::from(u32::MAX) {
            return Err(DecodedEditValidationError::OutputArithmeticOverflow);
        }
        Ok(())
    }
}

fn validate_canonical_ranges(
    ranges: &[WaveformEditFrameRange],
    total_frames: u64,
) -> Result<(), DecodedEditValidationError> {
    if ranges.is_empty() {
        return Err(DecodedEditValidationError::EmptyDuplicateRanges);
    }
    if ranges.len() > MAX_DECODED_EDIT_RANGES {
        return Err(DecodedEditValidationError::TooManyDuplicateRanges {
            actual: ranges.len(),
            maximum: MAX_DECODED_EDIT_RANGES,
        });
    }
    let mut previous: Option<WaveformEditFrameRange> = None;
    for range in ranges {
        if range.end_frame() > total_frames || range.start_frame() >= range.end_frame() {
            return Err(DecodedEditValidationError::InvalidDuplicateRange);
        }
        if let Some(previous) = previous {
            if range.start_frame() <= previous.end_frame() {
                return Err(DecodedEditValidationError::DuplicateRangesNotCanonical);
            }
        }
        previous = Some(*range);
    }
    Ok(())
}

/// Failure while constructing or validating a decoded-edit observation.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum DecodedEditValidationError {
    /// A source endpoint path is not portable.
    #[error("invalid decoded-edit source path: {0}")]
    InvalidPath(#[from] super::coordinator::StoredRelativePathError),
    /// The source identity observation was empty.
    #[error("decoded-edit source identity is invalid")]
    InvalidSourceIdentity,
    /// The source file revision observation was malformed.
    #[error("decoded-edit source file revision is invalid")]
    InvalidFileRevision,
    /// The source WAV layout was malformed.
    #[error("decoded-edit source specification is invalid")]
    InvalidSourceSpec,
    /// The source frame count was zero or exceeded the bounded model.
    #[error("decoded-edit frame count is invalid")]
    InvalidFrameCount,
    /// A click repair needs both neighboring source frames.
    #[error("decoded-edit click repair requires neighboring frames")]
    ClickNeighborsRequired,
    /// Duplicate cleanup contained no reviewed ranges.
    #[error("decoded-edit duplicate cleanup requires at least one range")]
    EmptyDuplicateRanges,
    /// The reviewed range count exceeded the bounded model.
    #[error("decoded-edit duplicate cleanup has {actual} ranges, maximum is {maximum}")]
    TooManyDuplicateRanges {
        /// Number of supplied reviewed ranges.
        actual: usize,
        /// Maximum number of reviewed ranges accepted.
        maximum: usize,
    },
    /// A duplicate range was empty or outside the source.
    #[error("decoded-edit duplicate cleanup range is invalid")]
    InvalidDuplicateRange,
    /// Directly decoded duplicate ranges were not already canonical.
    #[error("decoded-edit duplicate cleanup ranges are not canonical")]
    DuplicateRangesNotCanonical,
    /// The reviewed union would remove the whole source.
    #[error("decoded-edit duplicate cleanup cannot remove the entire source")]
    FullSourceRemoval,
    /// Checked frame/output arithmetic exceeded its bound.
    #[error("decoded-edit output arithmetic overflowed its bound")]
    OutputArithmeticOverflow,
    /// The output layout did not preserve the source Float32/channel/rate contract.
    #[error("decoded-edit output specification is invalid")]
    InvalidOutputSpec,
    /// The source participant did not match the endpoint contract.
    #[error("invalid decoded-edit source participant: {0}")]
    SourceParticipant(#[source] CopyIntentValidationError),
    /// A required Filesystem participant was absent.
    #[error("decoded-edit requires a required Filesystem participant")]
    RequiredFilesystemParticipantMissing,
    /// Participant shape validation failed.
    #[error("invalid decoded-edit participants: {0}")]
    Participants(#[from] ParticipantValidationError),
}

/// Validate the raw JSON payload of the full-progress-only command tag.
pub(crate) fn validate_persisted_decoded_edit_value(value: &Value) -> Result<(), &'static str> {
    let decoded: DecodedEditIntentArgs = serde_json::from_value(value.clone())
        .map_err(|_| "decoded-edit command arguments have invalid shape")?;
    decoded
        .validate()
        .map_err(|_| "decoded-edit command arguments are invalid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_journal::{
        ParticipantCheckpoint, ParticipantCheckpointState, ParticipantDescriptor, ParticipantId,
        StoredRelativePath, StoredSourceFence, WaveformSampleFormat,
    };
    use crate::sample_sources::SourceId;
    use uuid::Uuid;

    fn range(start: u64, end: u64) -> WaveformEditFrameRange {
        WaveformEditFrameRange::new(start, end).expect("range")
    }

    fn valid_edit(
        operation: DecodedEditOperationArgs,
    ) -> Result<DecodedEditIntentArgs, DecodedEditValidationError> {
        let source_id = SourceId::from_string("decoded-source");
        let source = CopyEndpoint::new(
            ParticipantId::from_uuid(Uuid::from_u128(1)),
            source_id,
            StoredRelativePath::new("source.wav").expect("path"),
        )
        .expect("endpoint");
        DecodedEditIntentArgs::new(
            source,
            WaveformFileRevisionObservation::new("file", "marker", 100, Some(1)).expect("revision"),
            WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float)
                .expect("source spec"),
            100,
            operation,
            WaveformOutputSpec::float32(2, 48_000),
        )
    }

    #[test]
    fn click_repair_requires_neighbors_and_preserves_frame_count() {
        let edit =
            valid_edit(DecodedEditOperationArgs::click_repair(range(1, 99))).expect("decoded edit");
        assert_eq!(edit.output_frames().expect("output"), 100);
        assert!(matches!(
            DecodedEditIntentArgs::new(
                edit.source().clone(),
                edit.source_file_revision().clone(),
                edit.source_spec(),
                100,
                DecodedEditOperationArgs::click_repair(range(0, 10)),
                edit.output(),
            ),
            Err(DecodedEditValidationError::ClickNeighborsRequired)
        ));
    }

    #[test]
    fn duplicate_cleanup_helper_canonicalizes_union_and_output() {
        let operation = DecodedEditOperationArgs::duplicate_cleanup(vec![
            range(20, 30),
            range(5, 12),
            range(10, 25),
            range(40, 45),
        ])
        .expect("canonical operation");
        assert_eq!(
            operation.ranges().expect("ranges"),
            &[range(5, 30), range(40, 45)]
        );
        let edit = valid_edit(operation).expect("decoded edit");
        assert_eq!(edit.output_frames().expect("output"), 70);
    }

    #[test]
    fn duplicate_cleanup_direct_values_must_be_canonical_and_not_empty() {
        let noncanonical = valid_edit(DecodedEditOperationArgs::DuplicateCleanup {
            ranges: vec![range(10, 20), range(20, 30)],
        });
        assert!(matches!(
            noncanonical,
            Err(DecodedEditValidationError::DuplicateRangesNotCanonical)
        ));
        assert!(matches!(
            DecodedEditOperationArgs::duplicate_cleanup(Vec::new()),
            Err(DecodedEditValidationError::EmptyDuplicateRanges)
        ));
        assert!(matches!(
            valid_edit(DecodedEditOperationArgs::DuplicateCleanup {
                ranges: vec![range(90, 101)],
            }),
            Err(DecodedEditValidationError::InvalidDuplicateRange)
        ));

        let malformed: DecodedEditOperationArgs = serde_json::from_value(serde_json::json!({
            "DuplicateCleanup": {
                "ranges": [
                    {"start_frame": 5, "end_frame": 3},
                    {"start_frame": 0, "end_frame": 10}
                ]
            }
        }))
        .expect("malformed range shape");
        assert!(matches!(
            DecodedEditOperationArgs::duplicate_cleanup(
                malformed.ranges().expect("malformed ranges").to_vec()
            ),
            Err(DecodedEditValidationError::InvalidDuplicateRange)
        ));
    }

    #[test]
    fn duplicate_cleanup_rejects_full_source_and_excess_ranges() {
        assert!(matches!(
            valid_edit(DecodedEditOperationArgs::DuplicateCleanup {
                ranges: vec![range(0, 100)],
            }),
            Err(DecodedEditValidationError::FullSourceRemoval)
        ));
        let too_many = (0..=MAX_DECODED_EDIT_RANGES)
            .map(|index| range(index as u64 * 2, index as u64 * 2 + 1))
            .collect();
        assert!(matches!(
            DecodedEditOperationArgs::duplicate_cleanup(too_many),
            Err(DecodedEditValidationError::TooManyDuplicateRanges { .. })
        ));
    }

    #[test]
    fn source_and_output_contract_is_checked_against_participants() {
        let source_id = SourceId::from_string("decoded-source");
        let source = ParticipantId::from_uuid(Uuid::from_u128(1));
        let filesystem = ParticipantId::from_uuid(Uuid::from_u128(2));
        let fence =
            StoredSourceFence::new(source_id.clone(), "root", "db", 1, 1, 1).expect("fence");
        let descriptors = vec![
            ParticipantDescriptor::new(
                source,
                ParticipantKind::SourceDatabase,
                ParticipantRequirement::Required,
                Some(fence),
                None,
            )
            .expect("source"),
            ParticipantDescriptor::new(
                filesystem,
                ParticipantKind::Filesystem,
                ParticipantRequirement::Required,
                None,
                None,
            )
            .expect("filesystem"),
        ];
        let checkpoints = descriptors
            .iter()
            .map(|descriptor| {
                ParticipantCheckpoint::new(
                    descriptor.id(),
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("checkpoint")
            })
            .collect();
        let participants = ParticipantSet::new(descriptors, checkpoints).expect("participants");
        let endpoint = CopyEndpoint::new(
            source,
            source_id,
            StoredRelativePath::new("source.wav").expect("path"),
        )
        .expect("endpoint");
        let edit = DecodedEditIntentArgs::new(
            endpoint,
            WaveformFileRevisionObservation::new("file", "marker", 100, Some(1)).expect("revision"),
            WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float)
                .expect("source spec"),
            100,
            DecodedEditOperationArgs::click_repair(range(1, 99)),
            WaveformOutputSpec::float32(2, 48_000),
        )
        .expect("edit");
        edit.validate_against_participants(&participants)
            .expect("participant contract");
        let mismatched = DecodedEditIntentArgs::new(
            edit.source().clone(),
            edit.source_file_revision().clone(),
            edit.source_spec(),
            edit.total_frames(),
            edit.operation().clone(),
            WaveformOutputSpec::float32(1, 48_000),
        );
        assert!(matches!(
            mismatched,
            Err(DecodedEditValidationError::InvalidOutputSpec)
        ));

        let wrong_source = CopyEndpoint::new(
            source,
            SourceId::from_string("other-source"),
            StoredRelativePath::new("source.wav").expect("path"),
        )
        .expect("wrong source endpoint");
        let wrong_source_edit = DecodedEditIntentArgs::new(
            wrong_source,
            edit.source_file_revision().clone(),
            edit.source_spec(),
            edit.total_frames(),
            edit.operation().clone(),
            edit.output(),
        )
        .expect("wrong source shape");
        assert!(matches!(
            wrong_source_edit.validate_against_participants(&participants),
            Err(DecodedEditValidationError::SourceParticipant(_))
        ));
    }

    #[test]
    fn decoded_edit_json_roundtrip_keeps_canonical_ranges() {
        let edit = valid_edit(
            DecodedEditOperationArgs::duplicate_cleanup(vec![range(4, 9), range(20, 25)])
                .expect("canonical"),
        )
        .expect("decoded edit");
        let value = serde_json::to_value(&edit).expect("encode");
        let decoded: DecodedEditIntentArgs = serde_json::from_value(value).expect("decode");
        assert_eq!(decoded, edit);
        let mut unknown = serde_json::to_value(&edit).expect("unknown field value");
        unknown["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<DecodedEditIntentArgs>(unknown).is_err());
        assert!(
            validate_persisted_decoded_edit_value(&serde_json::to_value(&decoded).expect("value"))
                .is_ok()
        );
    }
}
