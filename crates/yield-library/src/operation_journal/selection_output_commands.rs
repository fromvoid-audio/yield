//! Storage-neutral observations for source-owned WAV selection output.
//!
//! This module deliberately covers the first durable extraction slice only: a raw WAV selection
//! copied to a concrete source-relative destination, and the ordered `ExtractAndTrim` compound
//! operation. It records observations captured before a later filesystem owner acts. It never
//! retains samples, source bytes, a path authority, a lease, a capacity reservation, or an open
//! handle.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

pub use crate::sample_sources::SourcePlaybackType;

use super::coordinator::{
    CopyEndpoint, CopyEndpointRole, CopyIntentValidationError, FiniteF32Bits,
    MAX_WAVEFORM_CHANGE_MARKER_BYTES, MAX_WAVEFORM_EDIT_FRAMES, MAX_WAVEFORM_FILE_IDENTITY_BYTES,
    StoredRelativePathError, WaveformEditFrameRange, WaveformFileRevisionObservation,
    WaveformOutputSpec, WaveformSampleFormat, WaveformSourceSpec,
};
use super::model::UniversalCommandKind;
use super::participants::{
    MAX_SOURCE_IDENTITY_BYTES, ParticipantKind, ParticipantRequirement, ParticipantSet,
    ParticipantValidationError,
};
use super::render_commands::{WaveformRenderOutputArithmetic, WaveformRenderOutputSpec};

/// Minimum raw WAV `fmt ` chunk length accepted by the native raw writer.
pub const MIN_RAW_WAV_FMT_CHUNK_BYTES: u32 = 16;
/// Maximum raw WAV `fmt ` chunk length accepted by the native raw writer.
pub const MAX_RAW_WAV_FMT_CHUNK_BYTES: u32 = 4096;

/// The fixed linear edge-fade policy used by raw selection extraction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawWavEdgeFadePolicy {
    /// Fade from zero to full gain and back using the native linear ramp. The writer retains its
    /// near-unity EPSILON shortcut for the middle, rounds and clamps integer PCM after scaling,
    /// and scales Float32 samples as Float32. A one-frame fade resolves to zero gain at its edge
    /// and never divides by zero.
    LinearShortEdgeFade,
}

/// Fixed metadata policy for a completed selection extraction.
///
/// A later metadata owner may register the extracted destination with the fixed `Rating::KEEP_1`
/// value and an unlocked state. This policy is deliberately not serialized in the
/// intent: it is a fixed consequence of this workflow, while the selected source's playback
/// behavior is captured in [`SelectionExtractIntentArgs::playback_type`]. This contract performs
/// no metadata writes and carries no arbitrary tags or BPM values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionExtractMetadataPolicy {
    /// Keep the extracted file at rating 1 and leave it unlocked.
    Keep1Unlocked,
}

/// Resolved raw WAV extraction processing.
///
/// The caller resolves audition gain and the edge fade frame count before creating the intent. The
/// later owner must use these exact values; it must not reinterpret a duration or choose an
/// alternate source fallback during recovery. The native default is a two-millisecond request,
/// but the resolved frame count is the durable contract. The source `fmt ` chunk, including its
/// optional padding, is preserved without converting to another header format.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawWavExtractionRecipe {
    fmt_chunk_bytes: u32,
    gain: FiniteF32Bits,
    edge_fade_frames: u64,
}

impl RawWavExtractionRecipe {
    /// Construct a bounded, resolved raw WAV recipe.
    pub fn new(
        fmt_chunk_bytes: u32,
        gain: FiniteF32Bits,
        edge_fade_frames: u64,
    ) -> Result<Self, SelectionOutputValidationError> {
        let recipe = Self {
            fmt_chunk_bytes,
            gain,
            edge_fade_frames,
        };
        recipe.validate_basic()?;
        Ok(recipe)
    }

    /// Validate the bounded recipe without resolving its source frame count or sample rate.
    pub fn validate(&self) -> Result<(), SelectionOutputValidationError> {
        self.validate_basic()
    }

    /// Return the observed source `fmt ` chunk length.
    pub const fn fmt_chunk_bytes(self) -> u32 {
        self.fmt_chunk_bytes
    }

    /// Return the exact persisted gain bits.
    pub const fn gain(self) -> FiniteF32Bits {
        self.gain
    }

    /// Return the exact resolved edge-fade frame count.
    pub const fn edge_fade_frames(self) -> u64 {
        self.edge_fade_frames
    }

    /// Return the resolved positive finite gain value.
    pub fn gain_value(self) -> Result<f32, SelectionOutputValidationError> {
        self.validate_basic()?;
        Ok(self.gain.value())
    }

    /// Return the fixed edge-fade policy.
    pub const fn edge_fade_policy(self) -> RawWavEdgeFadePolicy {
        RawWavEdgeFadePolicy::LinearShortEdgeFade
    }

    fn validate_basic(&self) -> Result<(), SelectionOutputValidationError> {
        if !(MIN_RAW_WAV_FMT_CHUNK_BYTES..=MAX_RAW_WAV_FMT_CHUNK_BYTES)
            .contains(&self.fmt_chunk_bytes)
        {
            return Err(SelectionOutputValidationError::InvalidFmtChunkBytes);
        }
        let gain = self.gain.value();
        if !gain.is_finite() || gain <= 0.0 {
            return Err(SelectionOutputValidationError::InvalidGain);
        }
        Ok(())
    }

    fn validate_for_source(
        &self,
        source_spec: WaveformSourceSpec,
        selected_frames: u64,
    ) -> Result<RawWavOutputArithmetic, SelectionOutputValidationError> {
        self.validate_basic()?;
        if self.edge_fade_frames > selected_frames / 2 {
            return Err(SelectionOutputValidationError::EdgeFadeTooLong {
                edge_fade_frames: self.edge_fade_frames,
                selected_frames,
            });
        }
        checked_raw_wav_arithmetic(source_spec, self.fmt_chunk_bytes, selected_frames)
    }
}

/// Closed source/output recipe family for the currently supported extraction slice.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum SelectionOutputRecipe {
    /// Preserve the source WAV format and apply resolved gain and edge fading.
    RawWav(RawWavExtractionRecipe),
}

impl SelectionOutputRecipe {
    /// Validate the recipe against the captured source layout and selection length.
    pub fn validate_for_source(
        &self,
        source_spec: WaveformSourceSpec,
        selected_frames: u64,
    ) -> Result<RawWavOutputArithmetic, SelectionOutputValidationError> {
        match self {
            Self::RawWav(recipe) => recipe.validate_for_source(source_spec, selected_frames),
        }
    }

    /// Return the fixed raw edge-fade policy.
    pub const fn edge_fade_policy(&self) -> RawWavEdgeFadePolicy {
        match self {
            Self::RawWav(recipe) => recipe.edge_fade_policy(),
        }
    }
}

/// Checked byte arithmetic for one raw WAV selection output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawWavOutputArithmetic {
    output_frames: u64,
    block_align: u64,
    byte_rate: u64,
    fmt_chunk_bytes: u64,
    data_bytes: u64,
    riff_chunk_size: u64,
    output_file_bytes: u64,
}

impl RawWavOutputArithmetic {
    /// Return the number of selected output frames.
    pub const fn output_frames(self) -> u64 {
        self.output_frames
    }

    /// Return the source bytes in one interleaved frame.
    pub const fn block_align(self) -> u64 {
        self.block_align
    }

    /// Return the WAV header byte rate.
    pub const fn byte_rate(self) -> u64 {
        self.byte_rate
    }

    /// Return the output data chunk length before its optional pad byte.
    pub const fn data_bytes(self) -> u64 {
        self.data_bytes
    }

    /// Return the observed source `fmt ` chunk length.
    pub const fn fmt_chunk_bytes(self) -> u64 {
        self.fmt_chunk_bytes
    }

    /// Return the RIFF chunk-size field value.
    pub const fn riff_chunk_size(self) -> u64 {
        self.riff_chunk_size
    }

    /// Return the complete output byte length including the eight-byte RIFF prefix.
    pub const fn output_file_bytes(self) -> u64 {
        self.output_file_bytes
    }
}

/// Typed extraction observation for one source-owned raw WAV selection output.
///
/// `source_file_revision` and `source_revision` are observations. Before staging, the owner must
/// reopen the source through a live verified capability, recapture the exact `fmt ` bytes under
/// the strong file revision, and only then write the output. `fmt_chunk_bytes` is a length
/// observation used for checked RIFF accounting; it is not proof of the live header.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionExtractIntentArgs {
    source: CopyEndpoint,
    source_revision: u64,
    source_file_revision: WaveformFileRevisionObservation,
    source_spec: WaveformSourceSpec,
    total_frames: u64,
    range: WaveformEditFrameRange,
    recipe: SelectionOutputRecipe,
    playback_type: SourcePlaybackType,
    destination: CopyEndpoint,
    collision_policy: super::coordinator::CopyCollisionPolicy,
}

impl SelectionExtractIntentArgs {
    /// Construct a bounded raw WAV extraction observation.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: CopyEndpoint,
        source_revision: u64,
        source_file_revision: WaveformFileRevisionObservation,
        source_spec: WaveformSourceSpec,
        total_frames: u64,
        range: WaveformEditFrameRange,
        recipe: SelectionOutputRecipe,
        playback_type: SourcePlaybackType,
        destination: CopyEndpoint,
        collision_policy: super::coordinator::CopyCollisionPolicy,
    ) -> Result<Self, SelectionOutputValidationError> {
        let args = Self {
            source,
            source_revision,
            source_file_revision,
            source_spec,
            total_frames,
            range,
            recipe,
            playback_type,
            destination,
            collision_policy,
        };
        args.validate()?;
        Ok(args)
    }

    /// Validate the stored shape without acquiring participants or live capabilities.
    pub fn validate(&self) -> Result<(), SelectionOutputValidationError> {
        self.validate_shape()
    }

    /// Validate source/destination participant fences and the required filesystem participant.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), SelectionOutputValidationError> {
        self.validate_shape()?;
        participants.validate_for_new_intent()?;
        let source_descriptor =
            validate_endpoint(participants, CopyEndpointRole::Source, &self.source)?;
        let destination_descriptor = validate_endpoint(
            participants,
            CopyEndpointRole::Destination,
            &self.destination,
        )?;
        let source_fence = source_descriptor
            .source_fence()
            .ok_or(SelectionOutputValidationError::SourceFenceMissing)?;
        if source_fence.source_revision() != self.source_revision {
            return Err(SelectionOutputValidationError::SourceRevisionMismatch);
        }
        let destination_fence = destination_descriptor
            .source_fence()
            .ok_or(SelectionOutputValidationError::DestinationFenceMissing)?;
        if source_fence.root_identity() == destination_fence.root_identity()
            && self.source.relative_path() == self.destination.relative_path()
        {
            return Err(SelectionOutputValidationError::IdenticalPhysicalEntry);
        }
        validate_required_filesystem_participant(participants)
    }

    /// Return the source endpoint observation.
    pub fn source(&self) -> &CopyEndpoint {
        &self.source
    }

    /// Return the captured source-database revision.
    pub const fn source_revision(&self) -> u64 {
        self.source_revision
    }

    /// Return the strong file revision observation.
    pub fn source_file_revision(&self) -> &WaveformFileRevisionObservation {
        &self.source_file_revision
    }

    /// Return the source WAV specification.
    pub const fn source_spec(&self) -> WaveformSourceSpec {
        self.source_spec
    }

    /// Return the complete captured source frame count.
    pub const fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// Return the exact half-open selected source-frame range.
    pub const fn range(&self) -> WaveformEditFrameRange {
        self.range
    }

    /// Return the closed extraction recipe.
    pub const fn recipe(&self) -> &SelectionOutputRecipe {
        &self.recipe
    }

    /// Return the captured playback behavior to apply when the destination is registered.
    pub const fn playback_type(&self) -> SourcePlaybackType {
        self.playback_type
    }

    /// Return the concrete destination endpoint observation.
    pub fn destination(&self) -> &CopyEndpoint {
        &self.destination
    }

    /// Return the no-replace collision policy.
    pub const fn collision_policy(&self) -> super::coordinator::CopyCollisionPolicy {
        self.collision_policy
    }

    /// Return the fixed nonserialized metadata policy for this extraction workflow.
    pub const fn metadata_policy(&self) -> SelectionExtractMetadataPolicy {
        SelectionExtractMetadataPolicy::Keep1Unlocked
    }

    /// Return the checked number of selected frames.
    pub fn selected_frames(&self) -> Result<u64, SelectionOutputValidationError> {
        checked_selected_frames(self.total_frames, self.range)
    }

    /// Return checked arithmetic for the raw output WAV.
    pub fn output_arithmetic(
        &self,
    ) -> Result<RawWavOutputArithmetic, SelectionOutputValidationError> {
        self.validate_shape()?;
        let selected_frames = checked_selected_frames(self.total_frames, self.range)?;
        self.recipe
            .validate_for_source(self.source_spec, selected_frames)
    }

    /// Return the actual preserved output layout for this raw recipe.
    pub fn output_spec(&self) -> Result<WaveformRenderOutputSpec, SelectionOutputValidationError> {
        self.validate_shape()?;
        Ok(WaveformRenderOutputSpec::from_source(self.source_spec))
    }

    fn validate_shape(&self) -> Result<(), SelectionOutputValidationError> {
        validate_endpoint_shape(&self.source)?;
        validate_endpoint_shape(&self.destination)?;
        self.source_file_revision
            .validate()
            .map_err(|_| SelectionOutputValidationError::InvalidFileRevision)?;
        self.source_spec
            .validate()
            .map_err(|_| SelectionOutputValidationError::InvalidSourceSpec)?;
        validate_frame_count(self.total_frames)?;
        let selected_frames = checked_selected_frames(self.total_frames, self.range)?;
        self.recipe
            .validate_for_source(self.source_spec, selected_frames)?;
        if self.collision_policy != super::coordinator::CopyCollisionPolicy::NoReplace {
            return Err(SelectionOutputValidationError::UnsupportedCollisionPolicy);
        }
        Ok(())
    }
}

/// Ordered compound extraction followed by in-place trim of the exact source selection.
///
/// The extraction output must be verified and published before the original source trim begins.
/// The trim output is explicitly Float32 with the source channels and rate, matching the existing
/// pure media edit writer. Its checked remaining-frame calculation follows the existing `Trim`
/// operation: a bounded half-open range is subtracted and an empty output is rejected. This
/// remains one journal operation and is never represented as two independently accepted mutations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractAndTrimIntentArgs {
    extraction: SelectionExtractIntentArgs,
    trim_output: WaveformOutputSpec,
}

impl ExtractAndTrimIntentArgs {
    /// Construct the ordered extraction-then-trim observation.
    pub fn new(
        extraction: SelectionExtractIntentArgs,
        trim_output: WaveformOutputSpec,
    ) -> Result<Self, SelectionOutputValidationError> {
        let args = Self {
            extraction,
            trim_output,
        };
        args.validate()?;
        Ok(args)
    }

    /// Validate the compound observation without acquiring live capabilities.
    pub fn validate(&self) -> Result<(), SelectionOutputValidationError> {
        self.validate_shape()
    }

    /// Validate all source, destination, and filesystem participant bindings.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), SelectionOutputValidationError> {
        self.validate_shape()?;
        self.extraction.validate_against_participants(participants)
    }

    /// Return the ordered extraction step.
    pub const fn extraction(&self) -> &SelectionExtractIntentArgs {
        &self.extraction
    }

    /// Return the explicit Float32 trim output specification.
    pub const fn trim_output(&self) -> WaveformOutputSpec {
        self.trim_output
    }

    /// Return the exact selected source-frame range shared by both ordered steps.
    pub fn range(&self) -> WaveformEditFrameRange {
        self.extraction.range()
    }

    /// Return the checked number of frames left in the source after trim.
    pub fn trimmed_frames(&self) -> Result<u64, SelectionOutputValidationError> {
        let selected_frames = self.extraction.selected_frames()?;
        self.extraction
            .total_frames()
            .checked_sub(selected_frames)
            .filter(|frames| *frames > 0)
            .ok_or(SelectionOutputValidationError::WholeSourceTrim)
    }

    /// Return checked arithmetic for the in-place Float32 trim output.
    pub fn trim_output_arithmetic(
        &self,
    ) -> Result<WaveformRenderOutputArithmetic, SelectionOutputValidationError> {
        self.validate_shape()?;
        let frames = self.trimmed_frames()?;
        WaveformRenderOutputSpec::float32(self.trim_output)
            .checked_arithmetic(frames)
            .map_err(|_| SelectionOutputValidationError::TrimOutputArithmeticOverflow)
    }

    fn validate_shape(&self) -> Result<(), SelectionOutputValidationError> {
        self.extraction.validate_shape()?;
        let selected_frames = self.extraction.selected_frames()?;
        let remaining_frames = self
            .extraction
            .total_frames()
            .checked_sub(selected_frames)
            .filter(|frames| *frames > 0)
            .ok_or(SelectionOutputValidationError::WholeSourceTrim)?;
        self.trim_output
            .validate_against_source(self.extraction.source_spec())
            .map_err(|_| SelectionOutputValidationError::InvalidTrimOutputSpec)?;
        WaveformRenderOutputSpec::float32(self.trim_output)
            .checked_arithmetic(remaining_frames)
            .map_err(|_| SelectionOutputValidationError::TrimOutputArithmeticOverflow)?;
        Ok(())
    }
}

/// V13 typed selection-output command family.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum SelectionOutputCommandArgs {
    /// Publish one selected raw WAV output.
    Extract(SelectionExtractIntentArgs),
    /// Publish the selection, then trim that same selection from the source.
    ExtractAndTrim(ExtractAndTrimIntentArgs),
}

impl SelectionOutputCommandArgs {
    /// Return the universal command kind represented by this command.
    pub const fn command_kind(&self) -> UniversalCommandKind {
        match self {
            Self::Extract(_) => UniversalCommandKind::Extract,
            Self::ExtractAndTrim(_) => UniversalCommandKind::DestructiveEdit,
        }
    }

    /// Validate the command without acquiring live capabilities.
    pub fn validate(&self) -> Result<(), SelectionOutputValidationError> {
        match self {
            Self::Extract(args) => args.validate(),
            Self::ExtractAndTrim(args) => args.validate(),
        }
    }

    /// Validate command-specific participant bindings.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), SelectionOutputValidationError> {
        match self {
            Self::Extract(args) => args.validate_against_participants(participants),
            Self::ExtractAndTrim(args) => args.validate_against_participants(participants),
        }
    }
}

/// Failure while constructing or validating a V13 selection-output observation.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum SelectionOutputValidationError {
    /// A source or destination endpoint path is outside the portable locator contract.
    #[error("invalid selection output endpoint path: {0}")]
    InvalidPath(#[from] StoredRelativePathError),
    /// An endpoint source identity is outside its bounded contract.
    #[error("selection output endpoint source identity is invalid")]
    InvalidSourceIdentity,
    /// The strong file revision observation is malformed.
    #[error("selection output file revision is invalid")]
    InvalidFileRevision,
    /// The source WAV specification is malformed.
    #[error("selection output source specification is invalid")]
    InvalidSourceSpec,
    /// The complete source frame count is outside its checked bound.
    #[error("selection output source frame count is invalid")]
    InvalidFrameCount,
    /// The selected frame range is empty or overflowing.
    #[error("selection output frame range is invalid")]
    InvalidFrameRange,
    /// The selected range extends beyond the complete source.
    #[error("selection output frame range is outside the source")]
    SelectedRangeOutsideSource,
    /// The observed raw `fmt ` chunk length is outside the native bound.
    #[error("selection output raw WAV fmt chunk length is invalid")]
    InvalidFmtChunkBytes,
    /// The resolved gain is not positive and finite.
    #[error("selection output gain must be positive and finite")]
    InvalidGain,
    /// The resolved edge fade is longer than half the selected range.
    #[error(
        "selection output edge fade has {edge_fade_frames} frames for a {selected_frames}-frame selection"
    )]
    EdgeFadeTooLong {
        /// Persisted edge-fade frame count.
        edge_fade_frames: u64,
        /// Checked selected range length.
        selected_frames: u64,
    },
    /// Raw WAV sample or RIFF byte arithmetic exceeded its bound.
    #[error("selection output raw WAV arithmetic overflowed its bound")]
    OutputArithmeticOverflow,
    /// The extraction collision policy is not currently supported.
    #[error("selection output collision policy is unsupported")]
    UnsupportedCollisionPolicy,
    /// The source and destination resolve to the same physical source entry.
    #[error("selection output source and destination identify the same physical entry")]
    IdenticalPhysicalEntry,
    /// The source endpoint participant is invalid.
    #[error("invalid selection output source participant: {0}")]
    SourceParticipant(#[source] CopyIntentValidationError),
    /// The destination endpoint participant is invalid.
    #[error("invalid selection output destination participant: {0}")]
    DestinationParticipant(#[source] CopyIntentValidationError),
    /// The source participant had no stored source fence.
    #[error("selection output source participant has no source fence")]
    SourceFenceMissing,
    /// The destination participant had no stored source fence.
    #[error("selection output destination participant has no source fence")]
    DestinationFenceMissing,
    /// The source revision did not match its participant fence.
    #[error("selection output source revision does not match the source participant fence")]
    SourceRevisionMismatch,
    /// The compound trim would remove the complete source.
    #[error("ExtractAndTrim cannot trim the complete source")]
    WholeSourceTrim,
    /// The compound trim output is not source-rate/channel Float32.
    #[error("ExtractAndTrim trim output specification is invalid")]
    InvalidTrimOutputSpec,
    /// The compound trim output arithmetic exceeded its bound.
    #[error("ExtractAndTrim trim output arithmetic overflowed its bound")]
    TrimOutputArithmeticOverflow,
    /// No required filesystem participant was supplied.
    #[error("selection output requires a required Filesystem participant")]
    RequiredFilesystemParticipantMissing,
    /// Participant shape or checkpoint validation failed.
    #[error("invalid selection output participants: {0}")]
    Participants(#[from] ParticipantValidationError),
}

fn validate_endpoint_shape(endpoint: &CopyEndpoint) -> Result<(), SelectionOutputValidationError> {
    endpoint
        .relative_path()
        .validate()
        .map_err(SelectionOutputValidationError::InvalidPath)?;
    let source_id = endpoint.source_id().as_str();
    if source_id.is_empty()
        || source_id.len() > MAX_SOURCE_IDENTITY_BYTES
        || source_id.chars().any(char::is_control)
    {
        return Err(SelectionOutputValidationError::InvalidSourceIdentity);
    }
    Ok(())
}

fn validate_endpoint<'a>(
    participants: &'a ParticipantSet,
    role: CopyEndpointRole,
    endpoint: &CopyEndpoint,
) -> Result<&'a super::participants::ParticipantDescriptor, SelectionOutputValidationError> {
    validate_endpoint_shape(endpoint)?;
    let descriptor = participants
        .descriptors()
        .iter()
        .find(|descriptor| descriptor.id() == endpoint.participant_id())
        .ok_or_else(|| {
            endpoint_error(
                role,
                CopyIntentValidationError::ParticipantMissing {
                    role,
                    participant_id: endpoint.participant_id(),
                },
            )
        })?;
    if descriptor.kind() != ParticipantKind::SourceDatabase {
        return Err(endpoint_error(
            role,
            CopyIntentValidationError::ParticipantWrongKind {
                role,
                actual: descriptor.kind(),
            },
        ));
    }
    if descriptor.requirement() != ParticipantRequirement::Required {
        return Err(endpoint_error(
            role,
            CopyIntentValidationError::ParticipantNotRequired { role },
        ));
    }
    let fence = descriptor.source_fence().ok_or_else(|| {
        endpoint_error(
            role,
            CopyIntentValidationError::ParticipantFenceMissing { role },
        )
    })?;
    if fence.source_id() != endpoint.source_id() {
        return Err(endpoint_error(
            role,
            CopyIntentValidationError::ParticipantSourceMismatch { role },
        ));
    }
    Ok(descriptor)
}

fn endpoint_error(
    role: CopyEndpointRole,
    error: CopyIntentValidationError,
) -> SelectionOutputValidationError {
    match role {
        CopyEndpointRole::Source => SelectionOutputValidationError::SourceParticipant(error),
        CopyEndpointRole::Destination => {
            SelectionOutputValidationError::DestinationParticipant(error)
        }
    }
}

fn validate_required_filesystem_participant(
    participants: &ParticipantSet,
) -> Result<(), SelectionOutputValidationError> {
    if participants.descriptors().iter().any(|descriptor| {
        descriptor.kind() == ParticipantKind::Filesystem
            && descriptor.requirement() == ParticipantRequirement::Required
    }) {
        Ok(())
    } else {
        Err(SelectionOutputValidationError::RequiredFilesystemParticipantMissing)
    }
}

fn validate_frame_count(frames: u64) -> Result<(), SelectionOutputValidationError> {
    if frames == 0 || frames > MAX_WAVEFORM_EDIT_FRAMES {
        Err(SelectionOutputValidationError::InvalidFrameCount)
    } else {
        Ok(())
    }
}

fn checked_selected_frames(
    total_frames: u64,
    range: WaveformEditFrameRange,
) -> Result<u64, SelectionOutputValidationError> {
    validate_frame_count(total_frames)?;
    let selected_frames = range
        .len()
        .map_err(|_| SelectionOutputValidationError::InvalidFrameRange)?;
    if selected_frames == 0 {
        return Err(SelectionOutputValidationError::InvalidFrameRange);
    }
    if range.end_frame() > total_frames {
        return Err(SelectionOutputValidationError::SelectedRangeOutsideSource);
    }
    Ok(selected_frames)
}

fn checked_raw_wav_arithmetic(
    source_spec: WaveformSourceSpec,
    fmt_chunk_bytes: u32,
    output_frames: u64,
) -> Result<RawWavOutputArithmetic, SelectionOutputValidationError> {
    source_spec
        .validate()
        .map_err(|_| SelectionOutputValidationError::InvalidSourceSpec)?;
    if !(MIN_RAW_WAV_FMT_CHUNK_BYTES..=MAX_RAW_WAV_FMT_CHUNK_BYTES).contains(&fmt_chunk_bytes) {
        return Err(SelectionOutputValidationError::InvalidFmtChunkBytes);
    }
    validate_frame_count(output_frames)?;
    let bytes_per_sample = u64::from(source_spec.bits_per_sample() / 8);
    let block_align = u64::from(source_spec.channels())
        .checked_mul(bytes_per_sample)
        .ok_or(SelectionOutputValidationError::OutputArithmeticOverflow)?;
    let byte_rate = u64::from(source_spec.sample_rate())
        .checked_mul(block_align)
        .ok_or(SelectionOutputValidationError::OutputArithmeticOverflow)?;
    let _sample_count = output_frames
        .checked_mul(u64::from(source_spec.channels()))
        .ok_or(SelectionOutputValidationError::OutputArithmeticOverflow)?;
    let data_bytes = output_frames
        .checked_mul(block_align)
        .ok_or(SelectionOutputValidationError::OutputArithmeticOverflow)?;
    let fmt_chunk_bytes = u64::from(fmt_chunk_bytes);
    let fmt_chunk_total = chunk_total_len(fmt_chunk_bytes)
        .ok_or(SelectionOutputValidationError::OutputArithmeticOverflow)?;
    let data_chunk_total = chunk_total_len(data_bytes)
        .ok_or(SelectionOutputValidationError::OutputArithmeticOverflow)?;
    let riff_chunk_size = 4_u64
        .checked_add(fmt_chunk_total)
        .and_then(|size| size.checked_add(data_chunk_total))
        .ok_or(SelectionOutputValidationError::OutputArithmeticOverflow)?;
    if block_align == 0
        || block_align > u64::from(u16::MAX)
        || byte_rate > u64::from(u32::MAX)
        || data_bytes > u64::from(u32::MAX)
        || riff_chunk_size > u64::from(u32::MAX)
    {
        return Err(SelectionOutputValidationError::OutputArithmeticOverflow);
    }
    let output_file_bytes = riff_chunk_size
        .checked_add(8)
        .ok_or(SelectionOutputValidationError::OutputArithmeticOverflow)?;
    Ok(RawWavOutputArithmetic {
        output_frames,
        block_align,
        byte_rate,
        fmt_chunk_bytes,
        data_bytes,
        riff_chunk_size,
        output_file_bytes,
    })
}

fn chunk_total_len(data_len: u64) -> Option<u64> {
    8_u64
        .checked_add(data_len)
        .and_then(|size| size.checked_add(data_len % 2))
}

/// Validate the strict V13 selection-output JSON value before typed deserialization.
pub(crate) fn validate_persisted_selection_output_command_args_value(
    value: &Value,
) -> Result<(), &'static str> {
    let object = tagged_object(value, "persisted selection output command arguments")?;
    let (variant, arguments) = object
        .iter()
        .next()
        .ok_or("persisted selection output command arguments must contain one variant")?;
    match variant.as_str() {
        "Extract" => validate_persisted_extract_value(arguments, false).map(|_| ()),
        "ExtractAndTrim" => validate_persisted_extract_value(arguments, true).map(|_| ()),
        _ => Err("persisted selection output command arguments contain an unknown variant"),
    }
}

fn validate_persisted_extract_value(
    value: &Value,
    compound: bool,
) -> Result<(WaveformSourceSpec, u64, WaveformEditFrameRange, u64), &'static str> {
    if compound {
        let object = strict_object(value, &["extraction", "trim_output"])?;
        required_field(object, "extraction")?;
        required_field(object, "trim_output")?;
        let (source_spec, total_frames, range, selected_frames) =
            validate_persisted_extract_value(required_field(object, "extraction")?, false)?;
        let remaining = total_frames
            .checked_sub(selected_frames)
            .filter(|frames| *frames > 0)
            .ok_or("persisted ExtractAndTrim cannot trim the complete source")?;
        validate_persisted_trim_output(
            required_field(object, "trim_output")?,
            source_spec,
            remaining,
        )?;
        return Ok((source_spec, total_frames, range, selected_frames));
    }

    let object = strict_object(
        value,
        &[
            "source",
            "source_revision",
            "source_file_revision",
            "source_spec",
            "total_frames",
            "range",
            "recipe",
            "playback_type",
            "destination",
            "collision_policy",
        ],
    )?;
    for field in [
        "source",
        "source_revision",
        "source_file_revision",
        "source_spec",
        "total_frames",
        "range",
        "recipe",
        "playback_type",
        "destination",
        "collision_policy",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_endpoint(required_field(object, "source")?)?;
    required_field(object, "source_revision")?
        .as_u64()
        .ok_or("persisted selection output source revision must be unsigned")?;
    validate_persisted_revision(required_field(object, "source_file_revision")?)?;
    let source_spec = parse_persisted_source_spec(required_field(object, "source_spec")?)?;
    let total_frames = required_field(object, "total_frames")?
        .as_u64()
        .ok_or("persisted selection output total frames must be unsigned")?;
    validate_frame_count_raw(total_frames)?;
    let range = parse_persisted_range(required_field(object, "range")?)?;
    let selected_frames = checked_selected_frames_raw(total_frames, range)?;
    validate_persisted_recipe(
        required_field(object, "recipe")?,
        source_spec,
        selected_frames,
    )?;
    validate_persisted_playback_type(required_field(object, "playback_type")?)?;
    validate_persisted_endpoint(required_field(object, "destination")?)?;
    if required_field(object, "collision_policy")?.as_str() != Some("NoReplace") {
        return Err("persisted selection output collision policy is unsupported");
    }
    Ok((source_spec, total_frames, range, selected_frames))
}

fn validate_persisted_playback_type(value: &Value) -> Result<(), &'static str> {
    match value.as_str() {
        Some("Loop") | Some("OneShot") => Ok(()),
        _ => Err("persisted selection output playback type is invalid"),
    }
}

fn validate_persisted_recipe(
    value: &Value,
    source_spec: WaveformSourceSpec,
    selected_frames: u64,
) -> Result<(), &'static str> {
    let object = tagged_object(value, "persisted selection output recipe")?;
    let (variant, arguments) = object
        .iter()
        .next()
        .ok_or("persisted selection output recipe must contain one variant")?;
    if variant != "RawWav" {
        return Err("persisted selection output recipe contains an unknown variant");
    }
    let recipe = strict_object(arguments, &["fmt_chunk_bytes", "gain", "edge_fade_frames"])?;
    for field in ["fmt_chunk_bytes", "gain", "edge_fade_frames"] {
        required_field(recipe, field)?;
    }
    let fmt_chunk_bytes = required_field(recipe, "fmt_chunk_bytes")?
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("persisted raw WAV fmt chunk length must be a 32-bit unsigned integer")?;
    if !(u64::from(MIN_RAW_WAV_FMT_CHUNK_BYTES)..=u64::from(MAX_RAW_WAV_FMT_CHUNK_BYTES))
        .contains(&u64::from(fmt_chunk_bytes))
    {
        return Err("persisted raw WAV fmt chunk length is outside its bound");
    }
    let gain_bits = required_field(recipe, "gain")?
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("persisted raw WAV gain must be a 32-bit bit pattern")?;
    let gain = f32::from_bits(gain_bits);
    if !gain.is_finite() || gain <= 0.0 {
        return Err("persisted raw WAV gain must be positive and finite");
    }
    let edge_fade_frames = required_field(recipe, "edge_fade_frames")?
        .as_u64()
        .ok_or("persisted raw WAV edge fade frames must be unsigned")?;
    if edge_fade_frames > selected_frames / 2 {
        return Err("persisted raw WAV edge fade exceeds half the selected range");
    }
    checked_raw_wav_arithmetic(source_spec, fmt_chunk_bytes, selected_frames)
        .map(|_| ())
        .map_err(|_| "persisted raw WAV output arithmetic overflowed its bound")
}

fn validate_persisted_trim_output(
    value: &Value,
    source_spec: WaveformSourceSpec,
    remaining_frames: u64,
) -> Result<(), &'static str> {
    let object = strict_object(value, &["encoding", "channels", "sample_rate"])?;
    for field in ["encoding", "channels", "sample_rate"] {
        required_field(object, field)?;
    }
    if required_field(object, "encoding")?.as_str() != Some("Float32") {
        return Err("persisted ExtractAndTrim trim output must be Float32");
    }
    let channels = required_field(object, "channels")?
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .ok_or("persisted ExtractAndTrim trim channels are invalid")?;
    let sample_rate = required_field(object, "sample_rate")?
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("persisted ExtractAndTrim trim sample rate is invalid")?;
    if channels != source_spec.channels() || sample_rate != source_spec.sample_rate() {
        return Err("persisted ExtractAndTrim trim output must preserve source channels and rate");
    }
    WaveformRenderOutputSpec::new(channels, sample_rate, 32, WaveformSampleFormat::Float)
        .map_err(|_| "persisted ExtractAndTrim trim output is invalid")?
        .checked_arithmetic(remaining_frames)
        .map(|_| ())
        .map_err(|_| "persisted ExtractAndTrim trim output arithmetic overflowed its bound")
}

fn validate_persisted_endpoint(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(value, &["participant_id", "source_id", "relative_path"])?;
    for field in ["participant_id", "source_id", "relative_path"] {
        required_field(object, field)?;
    }
    if required_field(object, "participant_id")?
        .as_str()
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .is_none()
    {
        return Err("persisted selection output endpoint participant ID must be a UUID string");
    }
    let source_id = required_field(object, "source_id")?
        .as_str()
        .ok_or("persisted selection output endpoint source ID must be a string")?;
    if !valid_bounded_text(source_id, MAX_SOURCE_IDENTITY_BYTES) {
        return Err("persisted selection output endpoint source ID is outside its bound");
    }
    let path = required_field(object, "relative_path")?
        .as_str()
        .ok_or("persisted selection output endpoint path must be a string")?;
    super::coordinator::StoredRelativePath::new(path)
        .map_err(|_| "persisted selection output endpoint path is invalid")?;
    Ok(())
}

fn validate_persisted_revision(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "filesystem_identity",
            "change_marker",
            "byte_len",
            "modified_unix_ns",
        ],
    )?;
    for field in [
        "filesystem_identity",
        "change_marker",
        "byte_len",
        "modified_unix_ns",
    ] {
        required_field(object, field)?;
    }
    for (field, maximum) in [
        ("filesystem_identity", MAX_WAVEFORM_FILE_IDENTITY_BYTES),
        ("change_marker", MAX_WAVEFORM_CHANGE_MARKER_BYTES),
    ] {
        let text = required_field(object, field)?
            .as_str()
            .ok_or("persisted selection output revision identity must be a string")?;
        if !valid_bounded_text(text, maximum) {
            return Err("persisted selection output revision identity is outside its bound");
        }
    }
    required_field(object, "byte_len")?
        .as_u64()
        .ok_or("persisted selection output revision byte length must be unsigned")?;
    let modified = required_field(object, "modified_unix_ns")?;
    if !modified.is_null() && modified.as_i64().is_none() {
        return Err("persisted selection output revision timestamp must be signed or null");
    }
    Ok(())
}

fn parse_persisted_source_spec(value: &Value) -> Result<WaveformSourceSpec, &'static str> {
    let object = strict_object(
        value,
        &[
            "channels",
            "sample_rate",
            "bits_per_sample",
            "sample_format",
        ],
    )?;
    for field in [
        "channels",
        "sample_rate",
        "bits_per_sample",
        "sample_format",
    ] {
        required_field(object, field)?;
    }
    let channels = required_field(object, "channels")?
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .ok_or("persisted selection output source channels are invalid")?;
    let sample_rate = required_field(object, "sample_rate")?
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("persisted selection output source sample rate is invalid")?;
    let bits_per_sample = required_field(object, "bits_per_sample")?
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .ok_or("persisted selection output source bit depth is invalid")?;
    let sample_format = match required_field(object, "sample_format")?
        .as_str()
        .ok_or("persisted selection output source format must be a string")?
    {
        "Integer" => WaveformSampleFormat::Integer,
        "Float" => WaveformSampleFormat::Float,
        _ => return Err("persisted selection output source format is unsupported"),
    };
    WaveformSourceSpec::new(channels, sample_rate, bits_per_sample, sample_format)
        .map_err(|_| "persisted selection output source specification is invalid")
}

fn parse_persisted_range(value: &Value) -> Result<WaveformEditFrameRange, &'static str> {
    let object = strict_object(value, &["start_frame", "end_frame"])?;
    for field in ["start_frame", "end_frame"] {
        required_field(object, field)?;
    }
    let start = required_field(object, "start_frame")?
        .as_u64()
        .ok_or("persisted selection output range start must be unsigned")?;
    let end = required_field(object, "end_frame")?
        .as_u64()
        .ok_or("persisted selection output range end must be unsigned")?;
    WaveformEditFrameRange::new(start, end)
        .map_err(|_| "persisted selection output frame range is invalid")
}

fn validate_frame_count_raw(frames: u64) -> Result<(), &'static str> {
    if frames == 0 || frames > MAX_WAVEFORM_EDIT_FRAMES {
        Err("persisted selection output source frame count is outside its bound")
    } else {
        Ok(())
    }
}

fn checked_selected_frames_raw(
    total_frames: u64,
    range: WaveformEditFrameRange,
) -> Result<u64, &'static str> {
    validate_frame_count_raw(total_frames)?;
    let selected = range
        .len()
        .map_err(|_| "persisted selection output frame range is invalid")?;
    if selected == 0 {
        return Err("persisted selection output frame range is invalid");
    }
    if range.end_frame() > total_frames {
        return Err("persisted selection output frame range is outside the source");
    }
    Ok(selected)
}

fn valid_bounded_text(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

fn tagged_object<'a>(
    value: &'a Value,
    description: &str,
) -> Result<&'a Map<String, Value>, &'static str> {
    let object = value
        .as_object()
        .ok_or("persisted tagged value must be an object")?;
    if object.len() != 1 {
        return Err(match description {
            "persisted selection output command arguments" => {
                "persisted selection output command arguments must contain exactly one variant"
            }
            "persisted selection output recipe" => {
                "persisted selection output recipe must contain exactly one variant"
            }
            _ => "persisted tagged value must contain exactly one variant",
        });
    }
    Ok(object)
}

fn strict_object<'a>(
    value: &'a Value,
    allowed: &[&str],
) -> Result<&'a Map<String, Value>, &'static str> {
    let object = value
        .as_object()
        .ok_or("persisted selection output value must be an object")?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("persisted selection output value contains an unknown field");
    }
    if object.len() != allowed.len() {
        return Err("persisted selection output value has an incomplete field set");
    }
    Ok(object)
}

fn required_field<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a Value, &'static str> {
    object
        .get(field)
        .ok_or("persisted selection output value is missing a required field")
}

#[cfg(test)]
mod tests {
    use super::super::coordinator::{
        CopyCollisionPolicy, CopyEndpoint, WaveformFileRevisionObservation, WaveformOutputSpec,
    };
    use super::super::participants::{
        ParticipantCheckpoint, ParticipantCheckpointState, ParticipantDescriptor, ParticipantId,
        StoredSourceFence,
    };
    use super::*;
    use crate::sample_sources::{SourceId, SourcePlaybackType};

    fn fixture() -> (ParticipantSet, SelectionExtractIntentArgs) {
        let source_participant = ParticipantId::from_uuid(uuid::Uuid::from_u128(1));
        let destination_participant = ParticipantId::from_uuid(uuid::Uuid::from_u128(2));
        let filesystem_participant = ParticipantId::from_uuid(uuid::Uuid::from_u128(3));
        let source_id = SourceId::from_string("source-a");
        let destination_id = SourceId::from_string("source-b");
        let source_fence = StoredSourceFence::new(source_id.clone(), "root-a", "db-a", 1, 7, 1)
            .expect("source fence");
        let destination_fence =
            StoredSourceFence::new(destination_id.clone(), "root-b", "db-b", 1, 9, 1)
                .expect("destination fence");
        let participants = ParticipantSet::new(
            vec![
                ParticipantDescriptor::new(
                    source_participant,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(source_fence),
                    None,
                )
                .expect("source descriptor"),
                ParticipantDescriptor::new(
                    destination_participant,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(destination_fence),
                    None,
                )
                .expect("destination descriptor"),
                ParticipantDescriptor::new(
                    filesystem_participant,
                    ParticipantKind::Filesystem,
                    ParticipantRequirement::Required,
                    None,
                    None,
                )
                .expect("filesystem descriptor"),
            ],
            vec![
                ParticipantCheckpoint::new(
                    source_participant,
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("source checkpoint"),
                ParticipantCheckpoint::new(
                    destination_participant,
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("destination checkpoint"),
                ParticipantCheckpoint::new(
                    filesystem_participant,
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("filesystem checkpoint"),
            ],
        )
        .expect("participants");
        let source_spec = WaveformSourceSpec::new(2, 48_000, 16, WaveformSampleFormat::Integer)
            .expect("source spec");
        let source = CopyEndpoint::new(
            source_participant,
            source_id,
            super::super::coordinator::StoredRelativePath::new("folder/source.wav")
                .expect("source path"),
        )
        .expect("source endpoint");
        let destination = CopyEndpoint::new(
            destination_participant,
            destination_id,
            super::super::coordinator::StoredRelativePath::new("folder/clip.wav")
                .expect("destination path"),
        )
        .expect("destination endpoint");
        let recipe = SelectionOutputRecipe::RawWav(
            RawWavExtractionRecipe::new(16, FiniteF32Bits::new(1.0).expect("gain"), 20)
                .expect("recipe"),
        );
        let args = SelectionExtractIntentArgs::new(
            source,
            7,
            WaveformFileRevisionObservation::new("file", "change", 1000, Some(11))
                .expect("revision"),
            source_spec,
            100,
            WaveformEditFrameRange::new(10, 50).expect("range"),
            recipe,
            SourcePlaybackType::OneShot,
            destination,
            CopyCollisionPolicy::NoReplace,
        )
        .expect("selection output args");
        (participants, args)
    }

    #[test]
    fn raw_selection_output_validates_revision_participants_and_arithmetic() {
        let (participants, args) = fixture();
        args.validate_against_participants(&participants)
            .expect("participant validation");
        assert_eq!(args.selected_frames().expect("selected frames"), 40);
        let arithmetic = args.output_arithmetic().expect("arithmetic");
        assert_eq!(arithmetic.output_frames(), 40);
        assert_eq!(arithmetic.block_align(), 4);
        assert_eq!(arithmetic.data_bytes(), 160);
        assert_eq!(arithmetic.riff_chunk_size(), 196);
        assert_eq!(arithmetic.output_file_bytes(), 204);
        assert_eq!(args.playback_type(), SourcePlaybackType::OneShot);
        assert_eq!(
            args.metadata_policy(),
            SelectionExtractMetadataPolicy::Keep1Unlocked
        );
    }

    #[test]
    fn raw_selection_output_rejects_source_revision_mismatch_and_physical_alias() {
        let (participants, valid) = fixture();

        let mut mismatched_revision = serde_json::to_value(&valid).expect("args JSON");
        mismatched_revision["source_revision"] = Value::from(8_u64);
        let mismatched_revision: SelectionExtractIntentArgs =
            serde_json::from_value(mismatched_revision).expect("revision decodes");
        assert_eq!(
            mismatched_revision.validate_against_participants(&participants),
            Err(SelectionOutputValidationError::SourceRevisionMismatch)
        );

        let mut alias = serde_json::to_value(&valid).expect("args JSON");
        alias["destination"] = alias["source"].clone();
        let alias: SelectionExtractIntentArgs = serde_json::from_value(alias).expect("alias");
        assert_eq!(
            alias.validate_against_participants(&participants),
            Err(SelectionOutputValidationError::IdenticalPhysicalEntry)
        );
    }

    #[test]
    fn raw_selection_output_rejects_mismatched_fade_and_invalid_gain() {
        let (_, valid) = fixture();
        let mut malformed = serde_json::to_value(&valid).expect("args JSON");
        malformed["recipe"]["RawWav"]["edge_fade_frames"] = Value::from(21_u64);
        let malformed: SelectionExtractIntentArgs =
            serde_json::from_value(malformed).expect("untrusted recipe decodes");
        assert!(matches!(
            malformed.validate(),
            Err(SelectionOutputValidationError::EdgeFadeTooLong { .. })
        ));

        let mut malformed = serde_json::to_value(&valid).expect("args JSON");
        malformed["recipe"]["RawWav"]["gain"] = Value::from(f32::INFINITY.to_bits());
        let malformed: SelectionExtractIntentArgs =
            serde_json::from_value(malformed).expect("untrusted gain decodes");
        assert_eq!(
            malformed.validate(),
            Err(SelectionOutputValidationError::InvalidGain)
        );
    }

    #[test]
    fn resolved_edge_fade_is_preserved_without_requiring_the_default_duration() {
        let (_, valid) = fixture();
        for edge_fade_frames in [0_u64, 1, 20] {
            let mut value = serde_json::to_value(&valid).expect("args JSON");
            value["recipe"]["RawWav"]["edge_fade_frames"] = Value::from(edge_fade_frames);
            let args: SelectionExtractIntentArgs =
                serde_json::from_value(value).expect("resolved fade decodes");
            args.validate().expect("resolved fade is bounded");
        }

        let mut too_long = serde_json::to_value(&valid).expect("args JSON");
        too_long["recipe"]["RawWav"]["edge_fade_frames"] = Value::from(21_u64);
        let too_long: SelectionExtractIntentArgs =
            serde_json::from_value(too_long).expect("too-long fade decodes");
        assert!(matches!(
            too_long.validate(),
            Err(SelectionOutputValidationError::EdgeFadeTooLong { .. })
        ));
    }

    #[test]
    fn odd_format_and_data_lengths_include_both_riff_padding_bytes() {
        let source_spec = WaveformSourceSpec::new(1, 48_000, 8, WaveformSampleFormat::Integer)
            .expect("8-bit mono source");
        let recipe = RawWavExtractionRecipe::new(17, FiniteF32Bits::new(1.0).expect("gain"), 0)
            .expect("odd fmt recipe");
        let arithmetic = recipe
            .validate_for_source(source_spec, 3)
            .expect("odd raw arithmetic");
        assert_eq!(arithmetic.block_align(), 1);
        assert_eq!(arithmetic.data_bytes(), 3);
        assert_eq!(arithmetic.riff_chunk_size(), 42);
        assert_eq!(arithmetic.output_file_bytes(), 50);
    }

    #[test]
    fn direct_deserialization_derived_getters_fail_closed() {
        let (_, valid) = fixture();
        let mut whole = serde_json::to_value(&valid).expect("args JSON");
        whole["range"] = serde_json::json!({"start_frame": 0, "end_frame": 100});
        let whole: SelectionExtractIntentArgs =
            serde_json::from_value(whole).expect("whole range decodes");
        let compound: ExtractAndTrimIntentArgs = serde_json::from_value(serde_json::json!({
            "extraction": whole,
            "trim_output": {"encoding": "Float32", "channels": 2, "sample_rate": 48000}
        }))
        .expect("compound decodes");
        assert_eq!(
            compound.trimmed_frames(),
            Err(SelectionOutputValidationError::WholeSourceTrim)
        );

        let mut overflow = serde_json::to_value(&valid).expect("args JSON");
        overflow["source_spec"]["channels"] = Value::from(u16::MAX);
        overflow["source_spec"]["bits_per_sample"] = Value::from(32_u16);
        let overflow: SelectionExtractIntentArgs =
            serde_json::from_value(overflow).expect("overflow source decodes");
        assert!(overflow.output_arithmetic().is_err());

        let mut invalid_format = serde_json::to_value(&valid).expect("args JSON");
        invalid_format["source_spec"]["sample_format"] = Value::from("Float");
        invalid_format["source_spec"]["bits_per_sample"] = Value::from(16_u16);
        let invalid_format: SelectionExtractIntentArgs =
            serde_json::from_value(invalid_format).expect("invalid format decodes");
        assert!(invalid_format.output_spec().is_err());
        assert!(invalid_format.output_arithmetic().is_err());
    }

    #[test]
    fn extract_and_trim_is_ordered_and_rejects_whole_source() {
        let (_, extraction) = fixture();
        let compound = ExtractAndTrimIntentArgs::new(
            extraction.clone(),
            WaveformOutputSpec::float32(2, 48_000),
        )
        .expect("compound");
        assert_eq!(compound.trimmed_frames().expect("trimmed frames"), 60);
        assert_eq!(
            compound
                .trim_output_arithmetic()
                .expect("trim arithmetic")
                .output_frames(),
            60
        );

        let mut whole = serde_json::to_value(&extraction).expect("args JSON");
        whole["range"] = serde_json::json!({"start_frame": 0, "end_frame": 100});
        let whole: SelectionExtractIntentArgs =
            serde_json::from_value(whole).expect("whole range decodes");
        let compound = ExtractAndTrimIntentArgs::new(whole, WaveformOutputSpec::float32(2, 48_000));
        assert_eq!(
            compound,
            Err(SelectionOutputValidationError::WholeSourceTrim)
        );
    }

    #[test]
    fn persisted_shape_rejects_unknown_fields_and_accepts_bounded_recipe() {
        let (_, args) = fixture();
        let command = SelectionOutputCommandArgs::Extract(args);
        let value = serde_json::to_value(&command).expect("command JSON");
        validate_persisted_selection_output_command_args_value(&value)
            .expect("strict persisted command");
        let mut unknown = value.clone();
        unknown["Extract"]["unknown"] = Value::Null;
        assert!(validate_persisted_selection_output_command_args_value(&unknown).is_err());

        let mut missing_playback = value.clone();
        missing_playback["Extract"]
            .as_object_mut()
            .expect("extract object")
            .remove("playback_type");
        assert!(validate_persisted_selection_output_command_args_value(&missing_playback).is_err());

        let mut invalid_playback = value;
        invalid_playback["Extract"]["playback_type"] = Value::from("Unknown");
        assert!(validate_persisted_selection_output_command_args_value(&invalid_playback).is_err());
    }
}
