//! Strict, storage-neutral observations for one decoded multi-output export.
//!
//! The reference selection-export worker processes its ordered ranges independently: one failed
//! write or registration does not erase successful siblings.  This module records that intent and
//! its independent output descriptors, but contains no samples, filesystem capability, retry
//! authority, or proof that any child effect completed.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::coordinator::{
    CopyCollisionPolicy, CopyEndpoint, CopyEndpointRole, CopyIntentArgs, CopyIntentValidationError,
    MAX_WAVEFORM_EDIT_FRAMES, StoredRelativePathError, WaveformEditFrameRange,
    WaveformFileRevisionObservation, WaveformSourceSpec,
};
use super::decoded_export_commands::{
    DecodedExportIntentArgs, DecodedExportOutputArithmetic, DecodedExportOutputPolicy,
    DecodedExportRegistrationMetadata, DecodedExportValidationError,
};
use super::participants::{
    ParticipantKind, ParticipantRequirement, ParticipantSet, ParticipantValidationError,
};
use crate::sample_sources::SourcePlaybackType;

/// Maximum number of independent outputs in one decoded export batch.
pub const MAX_DECODED_EXPORT_BATCH_OUTPUTS: usize = 256;

/// The only failure policy currently represented by the batch intent.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecodedExportBatchFailurePolicy {
    /// Continue processing siblings and report each failed output separately.
    ContinueAndReportPerOutput,
}

/// One ordered decoded-export child descriptor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecodedExportBatchOutput {
    range: WaveformEditFrameRange,
    destination: CopyEndpoint,
}

impl DecodedExportBatchOutput {
    /// Construct one output descriptor without normalizing its source range or destination.
    pub fn new(
        range: WaveformEditFrameRange,
        destination: CopyEndpoint,
    ) -> Result<Self, DecodedExportBatchValidationError> {
        let output = Self { range, destination };
        output.validate_shape()?;
        Ok(output)
    }

    /// Return the selected source range in the original caller order.
    pub const fn range(&self) -> WaveformEditFrameRange {
        self.range
    }

    /// Return the frozen destination endpoint.
    pub fn destination(&self) -> &CopyEndpoint {
        &self.destination
    }

    fn validate_shape(&self) -> Result<(), DecodedExportBatchValidationError> {
        if self.range.start_frame() >= self.range.end_frame() {
            return Err(DecodedExportBatchValidationError::InvalidFrameRange);
        }
        validate_endpoint_shape(&self.destination)
    }
}

/// Complete full-progress-only decoded multi-output export observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecodedExportBatchIntentArgs {
    source: CopyEndpoint,
    source_revision: u64,
    source_file_revision: WaveformFileRevisionObservation,
    source_spec: WaveformSourceSpec,
    total_frames: u64,
    outputs: Vec<DecodedExportBatchOutput>,
    output_policy: DecodedExportOutputPolicy,
    rating: i8,
    requested_edge_fade_frames: u64,
    failure_policy: DecodedExportBatchFailurePolicy,
    collision_policy: CopyCollisionPolicy,
}

impl DecodedExportBatchIntentArgs {
    /// Construct a bounded ordered batch observation.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: CopyEndpoint,
        source_revision: u64,
        source_file_revision: WaveformFileRevisionObservation,
        source_spec: WaveformSourceSpec,
        total_frames: u64,
        outputs: Vec<DecodedExportBatchOutput>,
        output_policy: DecodedExportOutputPolicy,
        rating: i8,
        requested_edge_fade_frames: u64,
        failure_policy: DecodedExportBatchFailurePolicy,
        collision_policy: CopyCollisionPolicy,
    ) -> Result<Self, DecodedExportBatchValidationError> {
        let args = Self {
            source,
            source_revision,
            source_file_revision,
            source_spec,
            total_frames,
            outputs,
            output_policy,
            rating,
            requested_edge_fade_frames,
            failure_policy,
            collision_policy,
        };
        args.validate_shape()?;
        Ok(args)
    }

    /// Construct a batch using the reference metadata defaults.
    pub fn from_reference(
        source: CopyEndpoint,
        source_revision: u64,
        source_file_revision: WaveformFileRevisionObservation,
        source_spec: WaveformSourceSpec,
        total_frames: u64,
        outputs: Vec<DecodedExportBatchOutput>,
        output_policy: DecodedExportOutputPolicy,
        target_tag: Option<i8>,
        requested_edge_fade_frames: u64,
    ) -> Result<Self, DecodedExportBatchValidationError> {
        Self::new(
            source,
            source_revision,
            source_file_revision,
            source_spec,
            total_frames,
            outputs,
            output_policy,
            target_tag.unwrap_or(0),
            requested_edge_fade_frames,
            DecodedExportBatchFailurePolicy::ContinueAndReportPerOutput,
            CopyCollisionPolicy::NoReplace,
        )
    }

    /// Validate the bounded storage-neutral observation.
    pub fn validate(&self) -> Result<(), DecodedExportBatchValidationError> {
        self.validate_shape()
    }

    /// Validate all source/destination participant fences and same-source batch restrictions.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), DecodedExportBatchValidationError> {
        self.validate_shape()?;
        participants.validate_for_new_intent()?;
        let source_descriptor =
            CopyIntentArgs::validate_endpoint(participants, CopyEndpointRole::Source, &self.source)
                .map_err(DecodedExportBatchValidationError::SourceParticipant)?;
        let source_fence = source_descriptor
            .source_fence()
            .ok_or(DecodedExportBatchValidationError::SourceFenceMissing)?;
        if source_fence.source_revision() != self.source_revision {
            return Err(DecodedExportBatchValidationError::SourceRevisionMismatch);
        }
        if !participants.descriptors().iter().any(|descriptor| {
            descriptor.kind() == ParticipantKind::Filesystem
                && descriptor.requirement() == ParticipantRequirement::Required
        }) {
            return Err(DecodedExportBatchValidationError::RequiredFilesystemParticipantMissing);
        }

        let source_parent = relative_parent(self.source.relative_path().as_str());
        for (index, output) in self.outputs.iter().enumerate() {
            let destination_descriptor = CopyIntentArgs::validate_endpoint(
                participants,
                CopyEndpointRole::Destination,
                output.destination(),
            )
            .map_err(|source| {
                DecodedExportBatchValidationError::DestinationParticipant { index, source }
            })?;
            let destination_fence = destination_descriptor
                .source_fence()
                .ok_or(DecodedExportBatchValidationError::DestinationFenceMissing { index })?;
            if output.destination().source_id() != self.source.source_id() {
                return Err(DecodedExportBatchValidationError::DestinationSourceMismatch { index });
            }
            if source_fence.root_identity() != destination_fence.root_identity()
                || source_fence.database_identity() != destination_fence.database_identity()
            {
                return Err(DecodedExportBatchValidationError::DestinationRootMismatch { index });
            }
            if relative_parent(output.destination().relative_path().as_str()) != source_parent {
                return Err(DecodedExportBatchValidationError::DestinationParentMismatch { index });
            }
            if self.outputs[..index]
                .iter()
                .any(|prior| same_physical_endpoint(prior.destination(), output.destination()))
            {
                return Err(DecodedExportBatchValidationError::DuplicateDestination { index });
            }
            self.child_intent(output)
                .map_err(|source| DecodedExportBatchValidationError::Child { index, source })?
                .validate_against_participants(participants)
                .map_err(|source| DecodedExportBatchValidationError::Child { index, source })?;
        }
        Ok(())
    }

    /// Return the source endpoint observation shared by every child.
    pub fn source(&self) -> &CopyEndpoint {
        &self.source
    }

    /// Return the source revision expected by the source participant fence.
    pub const fn source_revision(&self) -> u64 {
        self.source_revision
    }

    /// Return the strong source-file revision observation.
    pub fn source_file_revision(&self) -> &WaveformFileRevisionObservation {
        &self.source_file_revision
    }

    /// Return the source WAV layout.
    pub const fn source_spec(&self) -> WaveformSourceSpec {
        self.source_spec
    }

    /// Return the complete source frame count.
    pub const fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// Borrow children in the exact order supplied by the caller.
    pub fn outputs(&self) -> &[DecodedExportBatchOutput] {
        &self.outputs
    }

    /// Return the explicit output policy shared by every child.
    pub const fn output_policy(&self) -> DecodedExportOutputPolicy {
        self.output_policy
    }

    /// Return the registration rating shared by every one-shot child.
    pub const fn rating(&self) -> i8 {
        self.rating
    }

    /// Return the caller-resolved global edge-fade request.
    pub const fn requested_edge_fade_frames(&self) -> u64 {
        self.requested_edge_fade_frames
    }

    /// Return the fixed independent-output failure policy.
    pub const fn failure_policy(&self) -> DecodedExportBatchFailurePolicy {
        self.failure_policy
    }

    /// Return the fixed no-replace collision policy.
    pub const fn collision_policy(&self) -> CopyCollisionPolicy {
        self.collision_policy
    }

    /// Return the per-child fade after the reference half-range clamp.
    pub fn child_edge_fade_frames(
        &self,
        index: usize,
    ) -> Result<u64, DecodedExportBatchValidationError> {
        let output = self
            .outputs
            .get(index)
            .ok_or(DecodedExportBatchValidationError::OutputIndexOutOfBounds { index })?;
        let frames = output
            .range()
            .len()
            .map_err(|_| DecodedExportBatchValidationError::InvalidFrameRange)?;
        Ok(self.requested_edge_fade_frames.min(frames / 2))
    }

    /// Return checked scalar/frame/WAV arithmetic for one ordered child.
    pub fn child_output_arithmetic(
        &self,
        index: usize,
    ) -> Result<DecodedExportOutputArithmetic, DecodedExportBatchValidationError> {
        let output = self
            .outputs
            .get(index)
            .ok_or(DecodedExportBatchValidationError::OutputIndexOutOfBounds { index })?;
        self.child_intent(output)
            .map_err(|source| DecodedExportBatchValidationError::Child { index, source })?
            .output_arithmetic()
            .map_err(|source| DecodedExportBatchValidationError::Child { index, source })
    }

    fn validate_shape(&self) -> Result<(), DecodedExportBatchValidationError> {
        validate_endpoint_shape(&self.source)?;
        self.source_file_revision
            .validate()
            .map_err(|_| DecodedExportBatchValidationError::InvalidFileRevision)?;
        self.source_spec
            .validate()
            .map_err(|_| DecodedExportBatchValidationError::InvalidSourceSpec)?;
        if self.total_frames == 0 || self.total_frames > MAX_WAVEFORM_EDIT_FRAMES {
            return Err(DecodedExportBatchValidationError::InvalidFrameCount);
        }
        if self.outputs.is_empty() {
            return Err(DecodedExportBatchValidationError::EmptyOutputs);
        }
        if self.outputs.len() > MAX_DECODED_EXPORT_BATCH_OUTPUTS {
            return Err(DecodedExportBatchValidationError::TooManyOutputs {
                actual: self.outputs.len(),
                maximum: MAX_DECODED_EXPORT_BATCH_OUTPUTS,
            });
        }
        if !(-3..=3).contains(&self.rating) {
            return Err(DecodedExportBatchValidationError::InvalidRating);
        }
        if self.collision_policy != CopyCollisionPolicy::NoReplace {
            return Err(DecodedExportBatchValidationError::UnsupportedCollisionPolicy);
        }

        for (index, output) in self.outputs.iter().enumerate() {
            output.validate_shape().map_err(|source| {
                DecodedExportBatchValidationError::ChildShape {
                    index,
                    source: Box::new(source),
                }
            })?;
            if same_physical_endpoint(output.destination(), &self.source) {
                return Err(DecodedExportBatchValidationError::IdenticalPhysicalEntry { index });
            }
            self.child_intent(output)
                .map_err(|source| DecodedExportBatchValidationError::Child { index, source })?;
            if self.outputs[..index]
                .iter()
                .any(|prior| same_physical_endpoint(prior.destination(), output.destination()))
            {
                return Err(DecodedExportBatchValidationError::DuplicateDestination { index });
            }
        }
        Ok(())
    }

    fn child_intent(
        &self,
        output: &DecodedExportBatchOutput,
    ) -> Result<DecodedExportIntentArgs, DecodedExportValidationError> {
        let registration =
            DecodedExportRegistrationMetadata::new(self.rating, SourcePlaybackType::OneShot, None)?;
        DecodedExportIntentArgs::new(
            self.source.clone(),
            output.destination.clone(),
            self.source_revision,
            self.source_file_revision.clone(),
            self.source_spec,
            self.total_frames,
            output.range,
            self.output_policy,
            output
                .range
                .len()
                .map_err(|_| DecodedExportValidationError::InvalidFrameRange)?
                .checked_div(2)
                .unwrap_or(0)
                .min(self.requested_edge_fade_frames),
            registration,
            self.collision_policy,
        )
    }
}

fn validate_endpoint_shape(
    endpoint: &CopyEndpoint,
) -> Result<(), DecodedExportBatchValidationError> {
    endpoint
        .relative_path()
        .validate()
        .map_err(DecodedExportBatchValidationError::InvalidPath)?;
    super::coordinator::validate_source_id(endpoint.source_id())
        .map_err(|_| DecodedExportBatchValidationError::InvalidSourceIdentity)
}

fn relative_parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn same_physical_endpoint(left: &CopyEndpoint, right: &CopyEndpoint) -> bool {
    left.source_id() == right.source_id() && left.relative_path() == right.relative_path()
}

/// Failure while constructing or validating a decoded export batch observation.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum DecodedExportBatchValidationError {
    /// An endpoint path is not portable.
    #[error("invalid decoded-export-batch endpoint path: {0}")]
    InvalidPath(#[from] StoredRelativePathError),
    /// An endpoint source identity is invalid.
    #[error("decoded-export-batch source identity is invalid")]
    InvalidSourceIdentity,
    /// The strong source-file revision is malformed.
    #[error("decoded-export-batch source file revision is invalid")]
    InvalidFileRevision,
    /// The source WAV specification is malformed.
    #[error("decoded-export-batch source specification is invalid")]
    InvalidSourceSpec,
    /// The complete source frame count is zero or outside its bound.
    #[error("decoded-export-batch source frame count is invalid")]
    InvalidFrameCount,
    /// The batch must contain at least one output.
    #[error("decoded-export-batch must contain at least one output")]
    EmptyOutputs,
    /// The output count exceeds the batch bound.
    #[error("decoded-export-batch contains {actual} outputs, maximum is {maximum}")]
    TooManyOutputs {
        /// Supplied output count.
        actual: usize,
        /// Maximum output count.
        maximum: usize,
    },
    /// One output range is empty or malformed.
    #[error("decoded-export-batch output range is invalid")]
    InvalidFrameRange,
    /// The output rating is outside the persisted -3..=3 range.
    #[error("decoded-export-batch registration rating must be between -3 and 3")]
    InvalidRating,
    /// The fixed no-replace policy was changed.
    #[error("decoded-export-batch collision policy is unsupported")]
    UnsupportedCollisionPolicy,
    /// A child failed storage-neutral decoded-export validation.
    #[error("decoded-export-batch child {index} is invalid: {source}")]
    Child {
        /// Child index in caller order.
        index: usize,
        /// Underlying single-export validation error.
        source: DecodedExportValidationError,
    },
    /// A child descriptor failed its direct shape checks.
    #[error("decoded-export-batch child {index} shape is invalid: {source}")]
    ChildShape {
        /// Child index in caller order.
        index: usize,
        /// Underlying descriptor validation error.
        source: Box<DecodedExportBatchValidationError>,
    },
    /// A destination is the source entry itself.
    #[error("decoded-export-batch child {index} destination is the source entry")]
    IdenticalPhysicalEntry {
        /// Child index in caller order.
        index: usize,
    },
    /// A destination names another source identity.
    #[error("decoded-export-batch child {index} destination source differs from the source")]
    DestinationSourceMismatch {
        /// Child index in caller order.
        index: usize,
    },
    /// A destination is not bound to the source root/database.
    #[error("decoded-export-batch child {index} destination root differs from the source")]
    DestinationRootMismatch {
        /// Child index in caller order.
        index: usize,
    },
    /// A destination is outside the source's parent directory.
    #[error("decoded-export-batch child {index} destination parent differs from the source")]
    DestinationParentMismatch {
        /// Child index in caller order.
        index: usize,
    },
    /// Two outputs target the same physical endpoint.
    #[error("decoded-export-batch child {index} duplicates an earlier destination")]
    DuplicateDestination {
        /// Child index in caller order.
        index: usize,
    },
    /// The source participant failed validation.
    #[error("invalid decoded-export-batch source participant: {0}")]
    SourceParticipant(#[source] CopyIntentValidationError),
    /// A child destination participant failed validation.
    #[error("invalid decoded-export-batch child {index} destination participant: {source}")]
    DestinationParticipant {
        /// Child index in caller order.
        index: usize,
        /// Underlying participant error.
        source: CopyIntentValidationError,
    },
    /// The source participant had no fence.
    #[error("decoded-export-batch source participant has no source fence")]
    SourceFenceMissing,
    /// A child destination participant had no fence.
    #[error("decoded-export-batch child {index} destination has no source fence")]
    DestinationFenceMissing {
        /// Child index in caller order.
        index: usize,
    },
    /// The source revision did not match the source fence.
    #[error("decoded-export-batch source revision does not match the source participant fence")]
    SourceRevisionMismatch,
    /// The required filesystem participant was absent.
    #[error("decoded-export-batch requires a required Filesystem participant")]
    RequiredFilesystemParticipantMissing,
    /// A child output index was outside the ordered list.
    #[error("decoded-export-batch output index {index} is out of bounds")]
    OutputIndexOutOfBounds {
        /// Requested output index.
        index: usize,
    },
    /// Participant shape or checkpoint validation failed.
    #[error("invalid decoded-export-batch participants: {0}")]
    Participants(#[from] ParticipantValidationError),
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

    fn endpoint(id: u128, source: &str, path: &str) -> CopyEndpoint {
        CopyEndpoint::new(
            ParticipantId::from_uuid(Uuid::from_u128(id)),
            SourceId::from_string(source),
            StoredRelativePath::new(path).expect("path"),
        )
        .expect("endpoint")
    }

    fn participants() -> ParticipantSet {
        let source_id = SourceId::from_string("source-a");
        let source = ParticipantDescriptor::new(
            ParticipantId::from_uuid(Uuid::from_u128(1)),
            ParticipantKind::SourceDatabase,
            ParticipantRequirement::Required,
            Some(StoredSourceFence::new(source_id, "root-a", "db-a", 1, 7, 1).expect("fence")),
            None,
        )
        .expect("source");
        let filesystem = ParticipantDescriptor::new(
            ParticipantId::from_uuid(Uuid::from_u128(2)),
            ParticipantKind::Filesystem,
            ParticipantRequirement::Required,
            None,
            None,
        )
        .expect("filesystem");
        let global = ParticipantDescriptor::new(
            ParticipantId::from_uuid(Uuid::from_u128(3)),
            ParticipantKind::GlobalLibrary,
            ParticipantRequirement::Required,
            None,
            None,
        )
        .expect("global");
        let harvest = ParticipantDescriptor::new(
            ParticipantId::from_uuid(Uuid::from_u128(4)),
            ParticipantKind::Harvest,
            ParticipantRequirement::Required,
            None,
            None,
        )
        .expect("harvest");
        let projection = ParticipantDescriptor::new(
            ParticipantId::from_uuid(Uuid::from_u128(5)),
            ParticipantKind::Projection,
            ParticipantRequirement::Required,
            Some(
                StoredSourceFence::new(
                    SourceId::from_string("source-a"),
                    "root-a",
                    "db-a",
                    1,
                    7,
                    1,
                )
                .expect("projection fence"),
            ),
            None,
        )
        .expect("projection");
        let readiness = ParticipantDescriptor::new(
            ParticipantId::from_uuid(Uuid::from_u128(6)),
            ParticipantKind::Readiness,
            ParticipantRequirement::Required,
            Some(
                StoredSourceFence::new(
                    SourceId::from_string("source-a"),
                    "root-a",
                    "db-a",
                    1,
                    7,
                    1,
                )
                .expect("readiness fence"),
            ),
            None,
        )
        .expect("readiness");
        let descriptors = vec![source, filesystem, global, harvest, projection, readiness];
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
        ParticipantSet::new(descriptors, checkpoints).expect("participants")
    }

    fn valid_batch() -> DecodedExportBatchIntentArgs {
        let source = endpoint(1, "source-a", "folder/input.wav");
        let outputs = vec![
            DecodedExportBatchOutput::new(
                WaveformEditFrameRange::new(10, 30).expect("range"),
                endpoint(1, "source-a", "folder/a.wav"),
            )
            .expect("output a"),
            DecodedExportBatchOutput::new(
                WaveformEditFrameRange::new(20, 40).expect("range"),
                endpoint(1, "source-a", "folder/c.wav"),
            )
            .expect("output c"),
        ];
        DecodedExportBatchIntentArgs::from_reference(
            source,
            7,
            WaveformFileRevisionObservation::new("file", "marker", 1024, Some(1))
                .expect("revision"),
            WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float)
                .expect("source spec"),
            100,
            outputs,
            DecodedExportOutputPolicy::default(),
            None,
            20,
        )
        .expect("batch")
    }

    #[test]
    fn ordered_overlapping_outputs_are_retained_and_fades_clamp_per_child() {
        let batch = valid_batch();
        assert_eq!(batch.outputs().len(), 2);
        assert_eq!(
            batch.outputs()[0].destination().relative_path().as_str(),
            "folder/a.wav"
        );
        assert_eq!(
            batch.outputs()[1].range(),
            WaveformEditFrameRange::new(20, 40).unwrap()
        );
        assert_eq!(batch.child_edge_fade_frames(0).unwrap(), 10);
        assert_eq!(batch.child_edge_fade_frames(1).unwrap(), 10);
        batch
            .validate_against_participants(&participants())
            .expect("participants");
    }

    #[test]
    fn empty_excess_and_duplicate_destinations_are_rejected() {
        let source = endpoint(1, "source-a", "folder/input.wav");
        let empty = DecodedExportBatchIntentArgs::from_reference(
            source.clone(),
            7,
            WaveformFileRevisionObservation::new("file", "marker", 1024, Some(1)).unwrap(),
            WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float).unwrap(),
            100,
            Vec::new(),
            DecodedExportOutputPolicy::default(),
            None,
            0,
        );
        assert!(matches!(
            empty,
            Err(DecodedExportBatchValidationError::EmptyOutputs)
        ));

        let output = |path| {
            DecodedExportBatchOutput::new(
                WaveformEditFrameRange::new(1, 5).unwrap(),
                endpoint(1, "source-a", path),
            )
            .unwrap()
        };
        let duplicate = DecodedExportBatchIntentArgs::from_reference(
            source.clone(),
            7,
            WaveformFileRevisionObservation::new("file", "marker", 1024, Some(1)).unwrap(),
            WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float).unwrap(),
            100,
            vec![output("folder/a.wav"), output("folder/a.wav")],
            DecodedExportOutputPolicy::default(),
            None,
            0,
        );
        assert!(matches!(
            duplicate,
            Err(DecodedExportBatchValidationError::DuplicateDestination { index: 1 })
        ));

        let too_many = DecodedExportBatchIntentArgs::from_reference(
            source.clone(),
            7,
            WaveformFileRevisionObservation::new("file", "marker", 1024, Some(1)).unwrap(),
            WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float).unwrap(),
            100,
            vec![output("folder/overflow.wav"); MAX_DECODED_EXPORT_BATCH_OUTPUTS + 1],
            DecodedExportOutputPolicy::default(),
            None,
            0,
        );
        match too_many {
            Err(DecodedExportBatchValidationError::TooManyOutputs { actual, maximum }) => {
                assert_eq!(actual, MAX_DECODED_EXPORT_BATCH_OUTPUTS + 1);
                assert_eq!(maximum, MAX_DECODED_EXPORT_BATCH_OUTPUTS);
            }
            other => panic!("expected output bound rejection, got {other:?}"),
        }

        let wrong_parent = DecodedExportBatchIntentArgs::from_reference(
            source,
            7,
            WaveformFileRevisionObservation::new("file", "marker", 1024, Some(1)).unwrap(),
            WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float).unwrap(),
            100,
            vec![output("other/a.wav")],
            DecodedExportOutputPolicy::default(),
            None,
            0,
        )
        .unwrap();
        assert!(matches!(
            wrong_parent.validate_against_participants(&participants()),
            Err(DecodedExportBatchValidationError::DestinationParentMismatch { index: 0 })
        ));
    }

    #[test]
    fn header_rate_keeps_scalar_samples_and_reinterprets_high_channel_source() {
        let source = endpoint(1, "source-a", "folder/input.wav");
        let output = DecodedExportBatchOutput::new(
            WaveformEditFrameRange::new(10, 30).unwrap(),
            endpoint(1, "source-a", "folder/a.wav"),
        )
        .unwrap();
        let batch = DecodedExportBatchIntentArgs::from_reference(
            source,
            7,
            WaveformFileRevisionObservation::new("file", "marker", 1024, Some(1)).unwrap(),
            WaveformSourceSpec::new(4, 48_000, 32, WaveformSampleFormat::Float).unwrap(),
            100,
            vec![output],
            DecodedExportOutputPolicy::new(
                super::super::decoded_export_commands::DecodedExportSampleRate::Hz(44_100),
                super::super::decoded_export_commands::DecodedExportSampleFormat::Float32,
                super::super::decoded_export_commands::DecodedExportChannelBehavior::PreserveMonoStereo,
                super::super::decoded_export_commands::DecodedExportDither::None,
            ),
            None,
            4,
        )
        .unwrap();
        let arithmetic = batch.child_output_arithmetic(0).unwrap();
        assert_eq!(arithmetic.scalar_sample_count(), 80);
        assert_eq!(arithmetic.output_spec().sample_rate(), 44_100);
        assert_eq!(arithmetic.output_spec().channels(), 2);
        assert_eq!(arithmetic.output_frames(), 40);
    }
}
