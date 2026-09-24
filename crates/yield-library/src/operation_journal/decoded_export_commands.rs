//! Strict, storage-neutral observations for one decoded selection export.
//!
//! The original selection-export worker crops samples, optionally applies its fixed short edge
//! fades, writes a WAV header/data stream, and registers the new source entry.  This module keeps
//! those inputs as one bounded full-progress-only observation.  It contains no samples, decoded
//! audio, path authority, source lease, filesystem handle, or permission to execute the export.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use crate::sample_sources::SourcePlaybackType;

use super::coordinator::{
    CopyCollisionPolicy, CopyEndpoint, CopyEndpointRole, CopyIntentValidationError, FiniteF32Bits,
    MAX_WAVEFORM_EDIT_FRAMES, StoredRelativePathError, WaveformEditFrameRange,
    WaveformFileRevisionObservation, WaveformSourceSpec,
};
use super::participants::{
    ParticipantKind, ParticipantRequirement, ParticipantSet, ParticipantValidationError,
};
use super::render_commands::{
    RenderCommandValidationError, WaveformRenderOutputArithmetic, WaveformRenderOutputSpec,
};

/// One output sample-rate policy from the reference selection-export worker.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecodedExportSampleRate {
    /// Use the source header rate without resampling.
    Source,
    /// Write this header rate without resampling the captured scalar samples.  Zero retains the
    /// reference `max(1)` behavior and therefore resolves to one Hertz.
    Hz(u32),
}

impl DecodedExportSampleRate {
    fn resolve(self, source_rate: u32) -> u32 {
        match self {
            Self::Source => source_rate.max(1),
            Self::Hz(rate) => rate.max(1),
        }
    }
}

/// Closed sample encoding policy supported by the reference WAV writer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecodedExportSampleFormat {
    /// Signed 16-bit integer PCM.
    Pcm16,
    /// Signed 24-bit integer PCM.
    Pcm24,
    /// IEEE-754 32-bit floating-point samples.
    Float32,
}

impl DecodedExportSampleFormat {
    const fn wav_parts(self) -> (u16, super::coordinator::WaveformSampleFormat) {
        match self {
            Self::Pcm16 => (16, super::coordinator::WaveformSampleFormat::Integer),
            Self::Pcm24 => (24, super::coordinator::WaveformSampleFormat::Integer),
            Self::Float32 => (32, super::coordinator::WaveformSampleFormat::Float),
        }
    }
}

/// Closed channel policy matching `AudioWriteFormatConfig::PreserveMonoStereo`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecodedExportChannelBehavior {
    /// Preserve mono and stereo; reinterpret higher-channel scalar buffers as stereo only when
    /// their scalar count is exactly divisible by two.
    PreserveMonoStereo,
}

impl DecodedExportChannelBehavior {
    const fn output_channels(self, source_channels: u16) -> u16 {
        match self {
            Self::PreserveMonoStereo => {
                if source_channels == 0 {
                    1
                } else if source_channels > 2 {
                    2
                } else {
                    source_channels
                }
            }
        }
    }
}

/// Closed dither policy matching the current reference writer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecodedExportDither {
    /// Do not add dither while writing integer PCM.
    None,
}

/// Exact output-header policy captured for one decoded export.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecodedExportOutputPolicy {
    sample_rate: DecodedExportSampleRate,
    sample_format: DecodedExportSampleFormat,
    channel_behavior: DecodedExportChannelBehavior,
    dither: DecodedExportDither,
}

impl DecodedExportOutputPolicy {
    /// Construct the closed output policy.  No resampling policy is represented.
    pub const fn new(
        sample_rate: DecodedExportSampleRate,
        sample_format: DecodedExportSampleFormat,
        channel_behavior: DecodedExportChannelBehavior,
        dither: DecodedExportDither,
    ) -> Self {
        Self {
            sample_rate,
            sample_format,
            channel_behavior,
            dither,
        }
    }

    /// Return the captured sample-rate policy.
    pub const fn sample_rate(self) -> DecodedExportSampleRate {
        self.sample_rate
    }

    /// Return the captured sample encoding.
    pub const fn sample_format(self) -> DecodedExportSampleFormat {
        self.sample_format
    }

    /// Return the captured channel policy.
    pub const fn channel_behavior(self) -> DecodedExportChannelBehavior {
        self.channel_behavior
    }

    /// Return the captured dither policy.
    pub const fn dither(self) -> DecodedExportDither {
        self.dither
    }

    fn output_spec(
        self,
        source: WaveformSourceSpec,
    ) -> Result<WaveformRenderOutputSpec, RenderCommandValidationError> {
        let (bits_per_sample, sample_format) = self.sample_format.wav_parts();
        WaveformRenderOutputSpec::new(
            self.channel_behavior.output_channels(source.channels()),
            self.sample_rate.resolve(source.sample_rate()),
            bits_per_sample,
            sample_format,
        )
    }
}

impl Default for DecodedExportOutputPolicy {
    fn default() -> Self {
        Self::new(
            DecodedExportSampleRate::Source,
            DecodedExportSampleFormat::Float32,
            DecodedExportChannelBehavior::PreserveMonoStereo,
            DecodedExportDither::None,
        )
    }
}

/// Registration metadata captured for the newly written source entry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecodedExportRegistrationMetadata {
    rating: i8,
    playback_type: SourcePlaybackType,
    bpm_bits: Option<FiniteF32Bits>,
}

impl DecodedExportRegistrationMetadata {
    /// Construct metadata from the reference UI semantics.
    ///
    /// A missing target tag becomes neutral (`0`).  Invalid/non-positive BPM is ignored, and BPM
    /// is ignored for one-shot output.  Directly decoded values use [`Self::validate`] and reject
    /// those noncanonical states instead of applying this normalization.
    pub fn from_reference(
        target_tag: Option<i8>,
        looped: bool,
        bpm: Option<f32>,
    ) -> Result<Self, DecodedExportValidationError> {
        let rating = target_tag.unwrap_or(0);
        if !(-3..=3).contains(&rating) {
            return Err(DecodedExportValidationError::InvalidRating);
        }
        let playback_type = if looped {
            SourcePlaybackType::Loop
        } else {
            SourcePlaybackType::OneShot
        };
        let bpm_bits = looped
            .then_some(bpm)
            .flatten()
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| {
                FiniteF32Bits::new(value).expect("reference BPM filter guarantees a finite value")
            });
        Ok(Self {
            rating,
            playback_type,
            bpm_bits,
        })
    }

    /// Construct exact metadata without applying reference normalization.
    pub fn new(
        rating: i8,
        playback_type: SourcePlaybackType,
        bpm_bits: Option<FiniteF32Bits>,
    ) -> Result<Self, DecodedExportValidationError> {
        let metadata = Self {
            rating,
            playback_type,
            bpm_bits,
        };
        metadata.validate()?;
        Ok(metadata)
    }

    /// Validate exact persisted registration semantics.
    pub fn validate(self) -> Result<(), DecodedExportValidationError> {
        if !(-3..=3).contains(&self.rating) {
            return Err(DecodedExportValidationError::InvalidRating);
        }
        if let Some(bpm_bits) = self.bpm_bits {
            let bpm = bpm_bits.value();
            if !bpm.is_finite() || bpm <= 0.0 {
                return Err(DecodedExportValidationError::InvalidBpm);
            }
            if self.playback_type == SourcePlaybackType::OneShot {
                return Err(DecodedExportValidationError::BpmForOneShot);
            }
        }
        Ok(())
    }

    /// Return the exact rating to register.
    pub const fn rating(self) -> i8 {
        self.rating
    }

    /// Return the captured playback type.
    pub const fn playback_type(self) -> SourcePlaybackType {
        self.playback_type
    }

    /// Return whether the destination is registered as a loop.
    pub const fn looped(self) -> bool {
        matches!(self.playback_type, SourcePlaybackType::Loop)
    }

    /// Return the decoded positive BPM, when present.
    pub fn bpm(self) -> Option<f32> {
        self.bpm_bits.map(FiniteF32Bits::value)
    }

    /// Return the exact stored BPM bits, when present.
    pub const fn bpm_bits(self) -> Option<FiniteF32Bits> {
        self.bpm_bits
    }
}

impl Default for DecodedExportRegistrationMetadata {
    fn default() -> Self {
        Self {
            rating: 0,
            playback_type: SourcePlaybackType::OneShot,
            bpm_bits: None,
        }
    }
}

/// Checked output arithmetic for one decoded selection export.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodedExportOutputArithmetic {
    output_spec: WaveformRenderOutputSpec,
    scalar_sample_count: u64,
    wav: WaveformRenderOutputArithmetic,
}

impl DecodedExportOutputArithmetic {
    /// Return the resolved output header layout.
    pub const fn output_spec(self) -> WaveformRenderOutputSpec {
        self.output_spec
    }

    /// Return the unchanged scalar sample count passed to the writer.
    pub const fn scalar_sample_count(self) -> u64 {
        self.scalar_sample_count
    }

    /// Return the output frame count implied by the output header channels.
    pub const fn output_frames(self) -> u64 {
        self.wav.output_frames()
    }

    /// Return the checked WAV arithmetic.
    pub const fn wav(self) -> WaveformRenderOutputArithmetic {
        self.wav
    }
}

/// Complete full-progress-only decoded selection-export observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecodedExportIntentArgs {
    source: CopyEndpoint,
    destination: CopyEndpoint,
    source_revision: u64,
    source_file_revision: WaveformFileRevisionObservation,
    source_spec: WaveformSourceSpec,
    total_frames: u64,
    range: WaveformEditFrameRange,
    output_policy: DecodedExportOutputPolicy,
    edge_fade_frames: u64,
    registration: DecodedExportRegistrationMetadata,
    collision_policy: CopyCollisionPolicy,
}

impl DecodedExportIntentArgs {
    /// Construct one bounded decoded export observation.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: CopyEndpoint,
        destination: CopyEndpoint,
        source_revision: u64,
        source_file_revision: WaveformFileRevisionObservation,
        source_spec: WaveformSourceSpec,
        total_frames: u64,
        range: WaveformEditFrameRange,
        output_policy: DecodedExportOutputPolicy,
        edge_fade_frames: u64,
        registration: DecodedExportRegistrationMetadata,
        collision_policy: CopyCollisionPolicy,
    ) -> Result<Self, DecodedExportValidationError> {
        let args = Self {
            source,
            destination,
            source_revision,
            source_file_revision,
            source_spec,
            total_frames,
            range,
            output_policy,
            edge_fade_frames,
            registration,
            collision_policy,
        };
        args.validate_shape()?;
        Ok(args)
    }

    /// Validate only the bounded storage-neutral observation.
    pub fn validate(&self) -> Result<(), DecodedExportValidationError> {
        self.validate_shape()
    }

    /// Validate source/destination participant fences and the required Filesystem participant.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), DecodedExportValidationError> {
        self.validate_shape()?;
        participants.validate_for_new_intent()?;
        let source = super::coordinator::CopyIntentArgs::validate_endpoint(
            participants,
            CopyEndpointRole::Source,
            &self.source,
        )
        .map_err(DecodedExportValidationError::SourceParticipant)?;
        let destination = super::coordinator::CopyIntentArgs::validate_endpoint(
            participants,
            CopyEndpointRole::Destination,
            &self.destination,
        )
        .map_err(DecodedExportValidationError::DestinationParticipant)?;
        let source_fence = source
            .source_fence()
            .ok_or(DecodedExportValidationError::SourceFenceMissing)?;
        let destination_fence = destination
            .source_fence()
            .ok_or(DecodedExportValidationError::DestinationFenceMissing)?;
        if source_fence.source_revision() != self.source_revision {
            return Err(DecodedExportValidationError::SourceRevisionMismatch);
        }
        if source_fence.root_identity() == destination_fence.root_identity()
            && source_fence.database_identity() == destination_fence.database_identity()
            && self.source.relative_path() == self.destination.relative_path()
        {
            return Err(DecodedExportValidationError::IdenticalPhysicalEntry);
        }
        if !participants.descriptors().iter().any(|descriptor| {
            descriptor.kind() == ParticipantKind::Filesystem
                && descriptor.requirement() == ParticipantRequirement::Required
        }) {
            return Err(DecodedExportValidationError::RequiredFilesystemParticipantMissing);
        }
        Ok(())
    }

    /// Return the source endpoint observation.
    pub fn source(&self) -> &CopyEndpoint {
        &self.source
    }

    /// Return the destination endpoint observation.
    pub fn destination(&self) -> &CopyEndpoint {
        &self.destination
    }

    /// Return the source revision expected by its participant fence.
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

    /// Return the non-empty selected source-frame range.
    pub const fn range(&self) -> WaveformEditFrameRange {
        self.range
    }

    /// Return the explicit output policy.
    pub const fn output_policy(&self) -> DecodedExportOutputPolicy {
        self.output_policy
    }

    /// Return the resolved edge-fade source-frame count.
    pub const fn edge_fade_frames(&self) -> u64 {
        self.edge_fade_frames
    }

    /// Return the explicit registration metadata.
    pub const fn registration(&self) -> DecodedExportRegistrationMetadata {
        self.registration
    }

    /// Return the no-replace collision policy.
    pub const fn collision_policy(&self) -> CopyCollisionPolicy {
        self.collision_policy
    }

    /// Return the checked selected frame count.
    pub fn selected_frames(&self) -> Result<u64, DecodedExportValidationError> {
        self.range
            .len()
            .map_err(|_| DecodedExportValidationError::InvalidFrameRange)
            .and_then(|frames| {
                if self.range.end_frame() > self.total_frames || self.total_frames == 0 {
                    Err(DecodedExportValidationError::InvalidFrameRange)
                } else {
                    Ok(frames)
                }
            })
    }

    /// Return the resolved output header layout.
    pub fn output_spec(&self) -> Result<WaveformRenderOutputSpec, DecodedExportValidationError> {
        self.output_policy
            .output_spec(self.source_spec)
            .map_err(|_| DecodedExportValidationError::InvalidOutputSpec)
    }

    /// Return checked scalar/frame/RIFF arithmetic for the export.
    pub fn output_arithmetic(
        &self,
    ) -> Result<DecodedExportOutputArithmetic, DecodedExportValidationError> {
        self.validate_shape_without_arithmetic()?;
        let selected_frames = self.selected_frames()?;
        let scalar_sample_count = selected_frames
            .checked_mul(u64::from(self.source_spec.channels()))
            .ok_or(DecodedExportValidationError::OutputArithmeticOverflow)?;
        let output_spec = self.output_spec()?;
        let output_channels = u64::from(output_spec.channels());
        if output_channels == 0 || scalar_sample_count % output_channels != 0 {
            return Err(
                DecodedExportValidationError::OutputSampleCountNotDivisible {
                    scalar_sample_count,
                    output_channels,
                },
            );
        }
        let output_frames = scalar_sample_count / output_channels;
        let wav = output_spec
            .checked_arithmetic(output_frames)
            .map_err(|_| DecodedExportValidationError::OutputArithmeticOverflow)?;
        Ok(DecodedExportOutputArithmetic {
            output_spec,
            scalar_sample_count,
            wav,
        })
    }

    /// Return the checked output frame count implied by the unchanged scalar buffer.
    pub fn output_frames(&self) -> Result<u64, DecodedExportValidationError> {
        Ok(self.output_arithmetic()?.output_frames())
    }

    fn validate_shape(&self) -> Result<(), DecodedExportValidationError> {
        self.validate_shape_without_arithmetic()?;
        self.output_arithmetic()?;
        Ok(())
    }

    fn validate_shape_without_arithmetic(&self) -> Result<(), DecodedExportValidationError> {
        validate_endpoint_shape(&self.source)?;
        validate_endpoint_shape(&self.destination)?;
        self.source_file_revision
            .validate()
            .map_err(|_| DecodedExportValidationError::InvalidFileRevision)?;
        self.source_spec
            .validate()
            .map_err(|_| DecodedExportValidationError::InvalidSourceSpec)?;
        if self.total_frames == 0 || self.total_frames > MAX_WAVEFORM_EDIT_FRAMES {
            return Err(DecodedExportValidationError::InvalidFrameCount);
        }
        let selected_frames = self.selected_frames()?;
        if self.edge_fade_frames > selected_frames / 2 {
            return Err(DecodedExportValidationError::EdgeFadeTooLong {
                edge_fade_frames: self.edge_fade_frames,
                selected_frames,
            });
        }
        self.registration.validate()?;
        if self.collision_policy != CopyCollisionPolicy::NoReplace {
            return Err(DecodedExportValidationError::UnsupportedCollisionPolicy);
        }
        Ok(())
    }
}

fn validate_endpoint_shape(endpoint: &CopyEndpoint) -> Result<(), DecodedExportValidationError> {
    endpoint
        .relative_path()
        .validate()
        .map_err(DecodedExportValidationError::InvalidPath)?;
    super::coordinator::validate_source_id(endpoint.source_id())
        .map_err(|_| DecodedExportValidationError::InvalidSourceIdentity)
}

/// Failure while constructing or validating a decoded export observation.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum DecodedExportValidationError {
    /// A source or destination endpoint path is not portable.
    #[error("invalid decoded-export endpoint path: {0}")]
    InvalidPath(#[from] StoredRelativePathError),
    /// An endpoint source identity is invalid.
    #[error("decoded-export source identity is invalid")]
    InvalidSourceIdentity,
    /// The strong source-file revision is malformed.
    #[error("decoded-export source file revision is invalid")]
    InvalidFileRevision,
    /// The source WAV specification is malformed.
    #[error("decoded-export source specification is invalid")]
    InvalidSourceSpec,
    /// The complete source frame count is zero or outside the model bound.
    #[error("decoded-export source frame count is invalid")]
    InvalidFrameCount,
    /// The selected range is empty or outside the source.
    #[error("decoded-export selection range is invalid")]
    InvalidFrameRange,
    /// The resolved short edge fade is longer than half the selection.
    #[error(
        "decoded-export edge fade has {edge_fade_frames} frames for a {selected_frames}-frame selection"
    )]
    EdgeFadeTooLong {
        /// Resolved edge-fade frame count.
        edge_fade_frames: u64,
        /// Selected source-frame count.
        selected_frames: u64,
    },
    /// The output policy could not produce a bounded WAV header layout.
    #[error("decoded-export output policy is invalid")]
    InvalidOutputSpec,
    /// The unchanged scalar buffer cannot be represented by the selected output channel count.
    #[error(
        "decoded-export scalar sample count {scalar_sample_count} is not divisible by {output_channels} output channels"
    )]
    OutputSampleCountNotDivisible {
        /// Scalar samples retained by the source layout.
        scalar_sample_count: u64,
        /// Header channels requested by the output policy.
        output_channels: u64,
    },
    /// Checked output frame or RIFF arithmetic exceeded a bound.
    #[error("decoded-export output arithmetic overflowed its bound")]
    OutputArithmeticOverflow,
    /// Registration rating is outside the persisted -3..=3 range.
    #[error("decoded-export registration rating must be between -3 and 3")]
    InvalidRating,
    /// Registration BPM is not finite and positive.
    #[error("decoded-export registration BPM must be finite and positive")]
    InvalidBpm,
    /// One-shot registrations cannot retain BPM metadata.
    #[error("decoded-export one-shot registration cannot carry BPM")]
    BpmForOneShot,
    /// The source endpoint participant failed validation.
    #[error("invalid decoded-export source participant: {0}")]
    SourceParticipant(#[source] CopyIntentValidationError),
    /// The destination endpoint participant failed validation.
    #[error("invalid decoded-export destination participant: {0}")]
    DestinationParticipant(#[source] CopyIntentValidationError),
    /// The source participant had no source fence.
    #[error("decoded-export source participant has no source fence")]
    SourceFenceMissing,
    /// The destination participant had no source fence.
    #[error("decoded-export destination participant has no source fence")]
    DestinationFenceMissing,
    /// The source revision did not match its participant fence.
    #[error("decoded-export source revision does not match the source participant fence")]
    SourceRevisionMismatch,
    /// Source and destination identify one physical source entry.
    #[error("decoded-export source and destination identify the same physical entry")]
    IdenticalPhysicalEntry,
    /// The collision policy is not the fixed no-replace policy.
    #[error("decoded-export collision policy is unsupported")]
    UnsupportedCollisionPolicy,
    /// No required Filesystem participant was supplied.
    #[error("decoded-export requires a required Filesystem participant")]
    RequiredFilesystemParticipantMissing,
    /// Participant shape or checkpoint validation failed.
    #[error("invalid decoded-export participants: {0}")]
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
    use serde_json::Value;
    use uuid::Uuid;

    fn range(start: u64, end: u64) -> WaveformEditFrameRange {
        WaveformEditFrameRange::new(start, end).expect("range")
    }

    fn endpoint(id: u128, source: &str, path: &str) -> CopyEndpoint {
        CopyEndpoint::new(
            ParticipantId::from_uuid(Uuid::from_u128(id)),
            SourceId::from_string(source),
            StoredRelativePath::new(path).expect("path"),
        )
        .expect("endpoint")
    }

    fn valid_export(
        source_spec: WaveformSourceSpec,
        policy: DecodedExportOutputPolicy,
        registration: DecodedExportRegistrationMetadata,
    ) -> Result<DecodedExportIntentArgs, DecodedExportValidationError> {
        DecodedExportIntentArgs::new(
            endpoint(1, "source-a", "input.wav"),
            endpoint(2, "source-b", "output.wav"),
            1,
            WaveformFileRevisionObservation::new("file", "marker", 100, Some(1)).expect("revision"),
            source_spec,
            100,
            range(10, 40),
            policy,
            2,
            registration,
            CopyCollisionPolicy::NoReplace,
        )
    }

    fn participants() -> ParticipantSet {
        let source_id = SourceId::from_string("source-a");
        let destination_id = SourceId::from_string("source-b");
        let source = ParticipantDescriptor::new(
            ParticipantId::from_uuid(Uuid::from_u128(1)),
            ParticipantKind::SourceDatabase,
            ParticipantRequirement::Required,
            Some(StoredSourceFence::new(source_id, "root-a", "db-a", 1, 1, 1).expect("fence")),
            None,
        )
        .expect("source descriptor");
        let destination = ParticipantDescriptor::new(
            ParticipantId::from_uuid(Uuid::from_u128(2)),
            ParticipantKind::SourceDatabase,
            ParticipantRequirement::Required,
            Some(StoredSourceFence::new(destination_id, "root-b", "db-b", 1, 1, 1).expect("fence")),
            None,
        )
        .expect("destination descriptor");
        let filesystem = ParticipantDescriptor::new(
            ParticipantId::from_uuid(Uuid::from_u128(3)),
            ParticipantKind::Filesystem,
            ParticipantRequirement::Required,
            None,
            None,
        )
        .expect("filesystem descriptor");
        let descriptors = vec![source, destination, filesystem];
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

    #[test]
    fn output_policy_preserves_scalar_samples_and_header_only_rate_changes() {
        let policy = DecodedExportOutputPolicy::new(
            DecodedExportSampleRate::Hz(0),
            DecodedExportSampleFormat::Pcm24,
            DecodedExportChannelBehavior::PreserveMonoStereo,
            DecodedExportDither::None,
        );
        let export = valid_export(
            WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float).expect("source"),
            policy,
            DecodedExportRegistrationMetadata::default(),
        )
        .expect("export");
        let arithmetic = export.output_arithmetic().expect("arithmetic");
        assert_eq!(arithmetic.scalar_sample_count(), 60);
        assert_eq!(arithmetic.output_frames(), 30);
        assert_eq!(arithmetic.output_spec().sample_rate(), 1);
        assert_eq!(arithmetic.output_spec().bits_per_sample(), 24);
    }

    #[test]
    fn higher_channel_scalar_reinterpretation_requires_divisible_output_frames() {
        let policy = DecodedExportOutputPolicy::default();
        let export = valid_export(
            WaveformSourceSpec::new(3, 48_000, 32, WaveformSampleFormat::Float).expect("source"),
            policy,
            DecodedExportRegistrationMetadata::default(),
        )
        .expect("export");
        assert_eq!(
            export
                .output_arithmetic()
                .expect("divisible")
                .output_frames(),
            45
        );

        let mut value = serde_json::to_value(export).expect("JSON");
        value["range"] = serde_json::json!({"start_frame": 10, "end_frame": 41});
        let invalid: DecodedExportIntentArgs = serde_json::from_value(value).expect("shape");
        assert!(matches!(
            invalid.validate(),
            Err(DecodedExportValidationError::OutputSampleCountNotDivisible { .. })
        ));
    }

    #[test]
    fn edge_fade_and_registration_reference_semantics_are_bounded() {
        let metadata =
            DecodedExportRegistrationMetadata::from_reference(None, true, Some(f32::NAN))
                .expect("reference metadata");
        assert_eq!(metadata.rating(), 0);
        assert!(metadata.looped());
        assert_eq!(metadata.bpm(), None);

        let export = valid_export(
            WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float).expect("source"),
            DecodedExportOutputPolicy::default(),
            metadata,
        )
        .expect("export");
        let mut value = serde_json::to_value(export).expect("JSON");
        value["edge_fade_frames"] = Value::from(16_u64);
        let invalid: DecodedExportIntentArgs = serde_json::from_value(value).expect("shape");
        assert!(matches!(
            invalid.validate(),
            Err(DecodedExportValidationError::EdgeFadeTooLong { .. })
        ));

        let one_shot = DecodedExportRegistrationMetadata::new(
            0,
            SourcePlaybackType::OneShot,
            Some(FiniteF32Bits::new(120.0).expect("BPM")),
        );
        assert!(matches!(
            one_shot,
            Err(DecodedExportValidationError::BpmForOneShot)
        ));
    }

    #[test]
    fn malformed_registration_and_output_values_are_rejected_without_normalization() {
        let export = valid_export(
            WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float).expect("source"),
            DecodedExportOutputPolicy::default(),
            DecodedExportRegistrationMetadata::default(),
        )
        .expect("export");
        let mut invalid_rating = serde_json::to_value(&export).expect("JSON");
        invalid_rating["registration"]["rating"] = Value::from(4_i64);
        let invalid: DecodedExportIntentArgs =
            serde_json::from_value(invalid_rating).expect("shape");
        assert!(matches!(
            invalid.validate(),
            Err(DecodedExportValidationError::InvalidRating)
        ));

        let mut invalid_bpm = serde_json::to_value(&export).expect("JSON");
        invalid_bpm["registration"]["playback_type"] = Value::from("Loop");
        invalid_bpm["registration"]["bpm_bits"] = Value::from(f32::INFINITY.to_bits());
        let invalid: DecodedExportIntentArgs = serde_json::from_value(invalid_bpm).expect("shape");
        assert!(matches!(
            invalid.validate(),
            Err(DecodedExportValidationError::InvalidBpm)
        ));

        let mut unknown = serde_json::to_value(&export).expect("JSON");
        unknown["output_policy"]["resample"] = Value::Bool(true);
        assert!(serde_json::from_value::<DecodedExportIntentArgs>(unknown).is_err());
    }

    #[test]
    fn participant_revision_and_same_entry_contracts_are_checked() {
        let export = valid_export(
            WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float).expect("source"),
            DecodedExportOutputPolicy::default(),
            DecodedExportRegistrationMetadata::default(),
        )
        .expect("export");
        export
            .validate_against_participants(&participants())
            .expect("participants");

        let mut value = serde_json::to_value(&export).expect("JSON");
        value["source_revision"] = Value::from(2_u64);
        let invalid: DecodedExportIntentArgs = serde_json::from_value(value).expect("shape");
        assert!(matches!(
            invalid.validate_against_participants(&participants()),
            Err(DecodedExportValidationError::SourceRevisionMismatch)
        ));

        let mut value = serde_json::to_value(&export).expect("JSON");
        value["destination"]["source_id"] = Value::from("source-a");
        let invalid: DecodedExportIntentArgs = serde_json::from_value(value).expect("shape");
        assert!(matches!(
            invalid.validate_against_participants(&participants()),
            Err(DecodedExportValidationError::IdenticalPhysicalEntry)
                | Err(DecodedExportValidationError::DestinationParticipant(_))
        ));
    }

    #[test]
    fn registration_roundtrip_keeps_exact_bpm_bits() {
        let metadata =
            DecodedExportRegistrationMetadata::from_reference(Some(-3), true, Some(123.25))
                .expect("metadata");
        let value = serde_json::to_value(metadata).expect("metadata JSON");
        let decoded: DecodedExportRegistrationMetadata =
            serde_json::from_value(value).expect("metadata decode");
        assert_eq!(decoded, metadata);
        assert_eq!(decoded.bpm_bits(), metadata.bpm_bits());
    }
}
