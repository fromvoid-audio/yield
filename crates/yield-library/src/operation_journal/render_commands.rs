//! Storage-neutral observations for whole-file WAV rendering commands.
//!
//! The values in this module describe the source revision, WAV layout, frame counts, output
//! layout, and participant fences captured before a later owner performs live checks. They never
//! open a file, reserve capacity, retain a filesystem root, or authorize a render.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use super::coordinator::{
    CopyCollisionPolicy, CopyEndpoint, CopyEndpointRole, CopyIntentValidationError,
    MAX_WAVEFORM_CHANGE_MARKER_BYTES, MAX_WAVEFORM_FILE_IDENTITY_BYTES,
    WaveformFileRevisionObservation, WaveformOutputEncoding, WaveformOutputSpec,
    WaveformSampleFormat, WaveformSourceSpec,
};
use super::participants::{
    MAX_SOURCE_IDENTITY_BYTES, ParticipantKind, ParticipantRequirement, ParticipantSet,
    ParticipantValidationError,
};
/// Maximum number of source or output frames represented by one render observation.
pub const MAX_WAVEFORM_RENDER_FRAMES: u64 = u32::MAX as u64;

const RIFF_PCM_CHUNK_SIZE_OVERHEAD: u64 = 36;
const RIFF_EXTENSIBLE_CHUNK_SIZE_OVERHEAD: u64 = 60;

/// WAV output layout retained by a whole-file render observation.
///
/// Unlike [`WaveformOutputSpec`], this type can represent the source encoding preserved by
/// `DuplicateDouble`, including 8-bit, 24-bit, and 32-bit integer PCM. It is an observation of
/// the intended output bytes and does not describe a writer capability.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaveformRenderOutputSpec {
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
    sample_format: WaveformSampleFormat,
}

impl WaveformRenderOutputSpec {
    /// Construct an output layout after checking the supported WAV representation.
    pub fn new(
        channels: u16,
        sample_rate: u32,
        bits_per_sample: u16,
        sample_format: WaveformSampleFormat,
    ) -> Result<Self, RenderCommandValidationError> {
        let spec = Self {
            channels,
            sample_rate,
            bits_per_sample,
            sample_format,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Construct output layout matching an observed source WAV specification.
    pub const fn from_source(source: WaveformSourceSpec) -> Self {
        Self {
            channels: source.channels(),
            sample_rate: source.sample_rate(),
            bits_per_sample: source.bits_per_sample(),
            sample_format: source.sample_format(),
        }
    }

    /// Construct the Float32 layout represented by the older edit output specification.
    pub const fn float32(output: WaveformOutputSpec) -> Self {
        Self {
            channels: output.channels(),
            sample_rate: output.sample_rate(),
            bits_per_sample: 32,
            sample_format: WaveformSampleFormat::Float,
        }
    }

    /// Validate output layout values recovered from an untrusted decoder.
    pub fn validate(self) -> Result<(), RenderCommandValidationError> {
        WaveformSourceSpec::new(
            self.channels,
            self.sample_rate,
            self.bits_per_sample,
            self.sample_format,
        )
        .map(|_| ())
        .map_err(|_| RenderCommandValidationError::InvalidOutputSpec)
    }

    /// Validate that this output preserves the supplied source layout exactly.
    pub fn validate_against_source(
        self,
        source: WaveformSourceSpec,
    ) -> Result<(), RenderCommandValidationError> {
        source
            .validate()
            .map_err(|_| RenderCommandValidationError::InvalidSourceSpec)?;
        self.validate()?;
        if self != Self::from_source(source) {
            return Err(RenderCommandValidationError::OutputDoesNotMatchSource);
        }
        Ok(())
    }

    /// Return the output channel count.
    pub const fn channels(self) -> u16 {
        self.channels
    }

    /// Return the output sample rate.
    pub const fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    /// Return the output bit depth.
    pub const fn bits_per_sample(self) -> u16 {
        self.bits_per_sample
    }

    /// Return the output sample representation.
    pub const fn sample_format(self) -> WaveformSampleFormat {
        self.sample_format
    }

    /// Calculate checked WAV layout and byte counts for `frames` output frames.
    pub fn checked_arithmetic(
        self,
        frames: u64,
    ) -> Result<WaveformRenderOutputArithmetic, RenderCommandValidationError> {
        checked_output_arithmetic(frames, self)
    }
}

/// Checked WAV byte arithmetic captured for one whole-file render output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaveformRenderOutputArithmetic {
    output_frames: u64,
    block_align: u64,
    byte_rate: u64,
    data_bytes: u64,
    riff_chunk_size: u64,
}

impl WaveformRenderOutputArithmetic {
    /// Return the checked output frame count.
    pub const fn output_frames(self) -> u64 {
        self.output_frames
    }

    /// Return bytes in one interleaved frame.
    pub const fn block_align(self) -> u64 {
        self.block_align
    }

    /// Return bytes per second in the output WAV header.
    pub const fn byte_rate(self) -> u64 {
        self.byte_rate
    }

    /// Return bytes in the WAV data chunk.
    pub const fn data_bytes(self) -> u64 {
        self.data_bytes
    }

    /// Return the RIFF chunk-size field value written by hound.
    pub const fn riff_chunk_size(self) -> u64 {
        self.riff_chunk_size
    }
}

/// Metadata policy for a whole-file DuplicateDouble observation.
///
/// The original operation registers the destination as a fresh source entry after writing it.
/// There is no inherited metadata or snapshot reference in this intent envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderMetadataPolicy {
    /// The destination receives fresh metadata when a later owner registers it.
    Fresh,
}

/// Typed whole-file render command arguments.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum RenderCommandArgs {
    /// Normalize one source file in place to Float32 while preserving rate and channels.
    Normalize(WaveformNormalizeIntentArgs),
    /// Create a fresh no-replace destination containing the source sequence twice.
    DuplicateDouble(WaveformDuplicateDoubleIntentArgs),
}

impl RenderCommandArgs {
    /// Return the universal command kind represented by this render observation.
    pub const fn command_kind(&self) -> super::model::UniversalCommandKind {
        match self {
            Self::Normalize(_) => super::model::UniversalCommandKind::DestructiveEdit,
            Self::DuplicateDouble(_) => super::model::UniversalCommandKind::Duplicate,
        }
    }

    /// Validate command-specific observations and required participant fences.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), RenderCommandValidationError> {
        match self {
            Self::Normalize(args) => args.validate_against_participants(participants),
            Self::DuplicateDouble(args) => args.validate_against_participants(participants),
        }
    }
}

/// Typed whole-file in-place Normalize observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaveformNormalizeIntentArgs {
    source: CopyEndpoint,
    source_file_revision: WaveformFileRevisionObservation,
    source_spec: WaveformSourceSpec,
    total_frames: u64,
    output: WaveformOutputSpec,
}

impl WaveformNormalizeIntentArgs {
    /// Construct a whole-file Normalize observation without reading or opening the source.
    pub fn new(
        source: CopyEndpoint,
        source_file_revision: WaveformFileRevisionObservation,
        source_spec: WaveformSourceSpec,
        total_frames: u64,
        output: WaveformOutputSpec,
    ) -> Result<Self, RenderCommandValidationError> {
        let args = Self {
            source,
            source_file_revision,
            source_spec,
            total_frames,
            output,
        };
        args.validate_shape()?;
        Ok(args)
    }

    /// Validate the observation independently of participant resolution.
    pub fn validate(&self) -> Result<(), RenderCommandValidationError> {
        self.validate_shape()
    }

    /// Validate source and filesystem participant fences for this in-place operation.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), RenderCommandValidationError> {
        self.validate_shape()?;
        participants.validate_for_new_intent()?;
        validate_endpoint(participants, CopyEndpointRole::Source, &self.source)?;
        validate_required_filesystem_participant(participants)
    }

    /// Return the source endpoint observation.
    pub fn source(&self) -> &CopyEndpoint {
        &self.source
    }

    /// Return the expected source file revision.
    pub fn source_file_revision(&self) -> &WaveformFileRevisionObservation {
        &self.source_file_revision
    }

    /// Return the observed source WAV layout.
    pub const fn source_spec(&self) -> WaveformSourceSpec {
        self.source_spec
    }

    /// Return the captured whole-source frame count.
    pub const fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// Return the explicit Float32 output layout.
    pub const fn output(&self) -> WaveformOutputSpec {
        self.output
    }

    /// Return the checked output frame count.
    pub fn output_frames(&self) -> Result<u64, RenderCommandValidationError> {
        validate_frame_count(self.total_frames).map(|_| self.total_frames)
    }

    /// Return checked output WAV arithmetic.
    pub fn output_arithmetic(
        &self,
    ) -> Result<WaveformRenderOutputArithmetic, RenderCommandValidationError> {
        float32_output_arithmetic(self.total_frames, self.output)
    }

    fn validate_shape(&self) -> Result<(), RenderCommandValidationError> {
        validate_endpoint_shape(&self.source)?;
        self.source_file_revision
            .validate()
            .map_err(|_| RenderCommandValidationError::InvalidFileRevision)?;
        self.source_spec
            .validate()
            .map_err(|_| RenderCommandValidationError::InvalidSourceSpec)?;
        validate_frame_count(self.total_frames)?;
        self.output
            .validate_against_source(self.source_spec)
            .map_err(|_| RenderCommandValidationError::InvalidOutputSpec)?;
        float32_output_arithmetic(self.total_frames, self.output)?;
        Ok(())
    }
}

/// Typed whole-file DuplicateDouble observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaveformDuplicateDoubleIntentArgs {
    source: CopyEndpoint,
    destination: CopyEndpoint,
    source_file_revision: WaveformFileRevisionObservation,
    source_spec: WaveformSourceSpec,
    total_frames: u64,
    output: WaveformRenderOutputSpec,
    collision_policy: CopyCollisionPolicy,
}

impl WaveformDuplicateDoubleIntentArgs {
    /// Construct a fresh DuplicateDouble observation without opening or writing either path.
    pub fn new(
        source: CopyEndpoint,
        destination: CopyEndpoint,
        source_file_revision: WaveformFileRevisionObservation,
        source_spec: WaveformSourceSpec,
        total_frames: u64,
        output: WaveformRenderOutputSpec,
        collision_policy: CopyCollisionPolicy,
    ) -> Result<Self, RenderCommandValidationError> {
        let args = Self {
            source,
            destination,
            source_file_revision,
            source_spec,
            total_frames,
            output,
            collision_policy,
        };
        args.validate_shape()?;
        Ok(args)
    }

    /// Validate the observation independently of participant resolution.
    pub fn validate(&self) -> Result<(), RenderCommandValidationError> {
        self.validate_shape()
    }

    /// Validate source/destination fences and the required filesystem participant.
    ///
    /// DuplicateDouble permits different source databases and parent directories. It only rejects
    /// a destination that resolves to the same physical root identity and relative entry as the
    /// source, including source-ID aliases carrying the same root observation.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), RenderCommandValidationError> {
        self.validate_shape()?;
        participants.validate_for_new_intent()?;
        let source_descriptor =
            validate_endpoint(participants, CopyEndpointRole::Source, &self.source)?;
        let destination_descriptor = validate_endpoint(
            participants,
            CopyEndpointRole::Destination,
            &self.destination,
        )?;
        let source_root = source_descriptor
            .source_fence()
            .expect("source endpoint validation checked its fence")
            .root_identity();
        let destination_root = destination_descriptor
            .source_fence()
            .expect("destination endpoint validation checked its fence")
            .root_identity();
        if source_root == destination_root
            && self.source.relative_path() == self.destination.relative_path()
        {
            return Err(RenderCommandValidationError::IdenticalPhysicalEntry);
        }
        validate_required_filesystem_participant(participants)
    }

    /// Return the source endpoint observation.
    pub fn source(&self) -> &CopyEndpoint {
        &self.source
    }

    /// Return the destination endpoint observation.
    pub fn destination(&self) -> &CopyEndpoint {
        &self.destination
    }

    /// Return the expected source file revision.
    pub fn source_file_revision(&self) -> &WaveformFileRevisionObservation {
        &self.source_file_revision
    }

    /// Return the observed source WAV layout.
    pub const fn source_spec(&self) -> WaveformSourceSpec {
        self.source_spec
    }

    /// Return the captured source frame count.
    pub const fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// Return the explicit output WAV layout.
    pub const fn output(&self) -> WaveformRenderOutputSpec {
        self.output
    }

    /// Return the checked doubled frame count.
    pub fn output_frames(&self) -> Result<u64, RenderCommandValidationError> {
        checked_doubled_frames(self.total_frames)
    }

    /// Return the explicit no-replace collision policy.
    pub const fn collision_policy(&self) -> CopyCollisionPolicy {
        self.collision_policy
    }

    /// Return the fixed metadata policy used by the original DuplicateDouble workflow.
    pub const fn metadata_policy(&self) -> RenderMetadataPolicy {
        RenderMetadataPolicy::Fresh
    }

    /// Return checked output WAV arithmetic for the doubled sequence.
    pub fn output_arithmetic(
        &self,
    ) -> Result<WaveformRenderOutputArithmetic, RenderCommandValidationError> {
        let frames = checked_doubled_frames(self.total_frames)?;
        self.output.checked_arithmetic(frames)
    }

    fn validate_shape(&self) -> Result<(), RenderCommandValidationError> {
        validate_endpoint_shape(&self.source)?;
        validate_endpoint_shape(&self.destination)?;
        self.source_file_revision
            .validate()
            .map_err(|_| RenderCommandValidationError::InvalidFileRevision)?;
        self.source_spec
            .validate()
            .map_err(|_| RenderCommandValidationError::InvalidSourceSpec)?;
        validate_frame_count(self.total_frames)?;
        self.output.validate_against_source(self.source_spec)?;
        let frames = checked_doubled_frames(self.total_frames)?;
        self.output.checked_arithmetic(frames)?;
        if self.collision_policy != CopyCollisionPolicy::NoReplace {
            return Err(RenderCommandValidationError::UnsupportedCollisionPolicy);
        }
        Ok(())
    }
}

/// Failure while constructing or validating a whole-file render observation.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum RenderCommandValidationError {
    /// A render endpoint path is not portable.
    #[error("invalid render endpoint path: {0}")]
    InvalidPath(#[from] super::coordinator::StoredRelativePathError),
    /// A render endpoint source identity is outside its bounded contract.
    #[error("render endpoint source identity is invalid")]
    InvalidSourceIdentity,
    /// A source endpoint participant failed its role or fence check.
    #[error("invalid render source participant: {0}")]
    SourceParticipant(#[source] CopyIntentValidationError),
    /// A destination endpoint participant failed its role or fence check.
    #[error("invalid render destination participant: {0}")]
    DestinationParticipant(#[source] CopyIntentValidationError),
    /// A file revision observation was malformed or outside its bound.
    #[error("render file revision is invalid")]
    InvalidFileRevision,
    /// A source WAV layout was malformed or unsupported.
    #[error("render source specification is invalid")]
    InvalidSourceSpec,
    /// A render output layout was malformed or unsupported.
    #[error("render output specification is invalid")]
    InvalidOutputSpec,
    /// A render output layout does not equal the observed source layout.
    #[error("render output specification does not match source")]
    OutputDoesNotMatchSource,
    /// A source frame count was zero or outside the checked envelope.
    #[error("render source frame count is invalid")]
    InvalidFrameCount,
    /// Doubling or WAV byte arithmetic exceeded a representable bound.
    #[error("render output arithmetic overflowed its bound")]
    OutputArithmeticOverflow,
    /// A DuplicateDouble source and destination resolve to one physical entry.
    #[error("DuplicateDouble source and destination identify the same physical entry")]
    IdenticalPhysicalEntry,
    /// Whole-file DuplicateDouble only permits no-replace creation.
    #[error("DuplicateDouble collision policy is unsupported")]
    UnsupportedCollisionPolicy,
    /// No required filesystem participant was supplied.
    #[error("render command requires a required Filesystem participant")]
    RequiredFilesystemParticipantMissing,
    /// Participant shape or checkpoint validation failed.
    #[error("invalid render command participants: {0}")]
    Participants(#[from] ParticipantValidationError),
}

fn validate_endpoint_shape(endpoint: &CopyEndpoint) -> Result<(), RenderCommandValidationError> {
    endpoint
        .relative_path()
        .validate()
        .map_err(RenderCommandValidationError::InvalidPath)?;
    if endpoint.source_id().as_str().is_empty()
        || endpoint.source_id().as_str().len() > MAX_SOURCE_IDENTITY_BYTES
        || endpoint.source_id().as_str().chars().any(char::is_control)
    {
        return Err(RenderCommandValidationError::InvalidSourceIdentity);
    }
    Ok(())
}

fn validate_endpoint<'a>(
    participants: &'a ParticipantSet,
    role: CopyEndpointRole,
    endpoint: &CopyEndpoint,
) -> Result<&'a super::participants::ParticipantDescriptor, RenderCommandValidationError> {
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
) -> RenderCommandValidationError {
    match role {
        CopyEndpointRole::Source => RenderCommandValidationError::SourceParticipant(error),
        CopyEndpointRole::Destination => {
            RenderCommandValidationError::DestinationParticipant(error)
        }
    }
}

fn validate_required_filesystem_participant(
    participants: &ParticipantSet,
) -> Result<(), RenderCommandValidationError> {
    if participants.descriptors().iter().any(|descriptor| {
        descriptor.kind() == ParticipantKind::Filesystem
            && descriptor.requirement() == ParticipantRequirement::Required
    }) {
        Ok(())
    } else {
        Err(RenderCommandValidationError::RequiredFilesystemParticipantMissing)
    }
}

fn validate_frame_count(frames: u64) -> Result<(), RenderCommandValidationError> {
    if frames == 0 || frames > MAX_WAVEFORM_RENDER_FRAMES {
        Err(RenderCommandValidationError::InvalidFrameCount)
    } else {
        Ok(())
    }
}

fn checked_doubled_frames(frames: u64) -> Result<u64, RenderCommandValidationError> {
    validate_frame_count(frames)?;
    let doubled = frames
        .checked_mul(2)
        .ok_or(RenderCommandValidationError::OutputArithmeticOverflow)?;
    if doubled > MAX_WAVEFORM_RENDER_FRAMES {
        return Err(RenderCommandValidationError::OutputArithmeticOverflow);
    }
    Ok(doubled)
}

fn float32_output_arithmetic(
    frames: u64,
    output: WaveformOutputSpec,
) -> Result<WaveformRenderOutputArithmetic, RenderCommandValidationError> {
    if output.encoding() != WaveformOutputEncoding::Float32 {
        return Err(RenderCommandValidationError::InvalidOutputSpec);
    }
    let spec = WaveformRenderOutputSpec::new(
        output.channels(),
        output.sample_rate(),
        32,
        WaveformSampleFormat::Float,
    )?;
    spec.checked_arithmetic(frames)
}

fn checked_output_arithmetic(
    frames: u64,
    output: WaveformRenderOutputSpec,
) -> Result<WaveformRenderOutputArithmetic, RenderCommandValidationError> {
    validate_frame_count(frames)?;
    output.validate()?;
    let bytes_per_sample = u64::from(output.bits_per_sample() / 8);
    let block_align = u64::from(output.channels())
        .checked_mul(bytes_per_sample)
        .ok_or(RenderCommandValidationError::OutputArithmeticOverflow)?;
    let byte_rate = u64::from(output.sample_rate())
        .checked_mul(block_align)
        .ok_or(RenderCommandValidationError::OutputArithmeticOverflow)?;
    let data_bytes = frames
        .checked_mul(block_align)
        .ok_or(RenderCommandValidationError::OutputArithmeticOverflow)?;
    let extensible = output.channels() > 2 || output.bits_per_sample() > 16;
    let header_overhead = if extensible {
        RIFF_EXTENSIBLE_CHUNK_SIZE_OVERHEAD
    } else {
        RIFF_PCM_CHUNK_SIZE_OVERHEAD
    };
    let riff_chunk_size = data_bytes
        .checked_add(header_overhead)
        .ok_or(RenderCommandValidationError::OutputArithmeticOverflow)?;
    if block_align == 0
        || block_align > u64::from(u16::MAX)
        || byte_rate > u64::from(u32::MAX)
        || data_bytes > u64::from(u32::MAX)
        || riff_chunk_size > u64::from(u32::MAX)
    {
        return Err(RenderCommandValidationError::OutputArithmeticOverflow);
    }
    Ok(WaveformRenderOutputArithmetic {
        output_frames: frames,
        block_align,
        byte_rate,
        data_bytes,
        riff_chunk_size,
    })
}

/// Validate the strict V9 render-command JSON boundary before typed deserialization.
pub(crate) fn validate_persisted_render_command_args_value(
    value: &Value,
) -> Result<(), &'static str> {
    let object = value
        .as_object()
        .ok_or("persisted render command arguments must be a tagged object")?;
    if object.len() != 1 {
        return Err("persisted render command arguments must contain exactly one variant");
    }
    let (variant, arguments) = object
        .iter()
        .next()
        .ok_or("persisted render command arguments must contain exactly one variant")?;
    match variant.as_str() {
        "Normalize" => validate_persisted_normalize_value(arguments),
        "DuplicateDouble" => validate_persisted_duplicate_double_value(arguments),
        _ => Err("persisted render command arguments contain an unknown variant"),
    }
}

fn validate_persisted_normalize_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "source",
            "source_file_revision",
            "source_spec",
            "total_frames",
            "output",
        ],
    )?;
    for field in [
        "source",
        "source_file_revision",
        "source_spec",
        "total_frames",
        "output",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_endpoint(required_field(object, "source")?)?;
    validate_persisted_revision(required_field(object, "source_file_revision")?)?;
    let source = parse_persisted_source_spec(required_field(object, "source_spec")?)?;
    let frames = required_field(object, "total_frames")?
        .as_u64()
        .ok_or("render total frames must be an unsigned integer")?;
    validate_frame_count_raw(frames)?;
    let output = strict_object(
        required_field(object, "output")?,
        &["encoding", "channels", "sample_rate"],
    )?;
    for field in ["encoding", "channels", "sample_rate"] {
        required_field(output, field)?;
    }
    if required_field(output, "encoding")?.as_str() != Some("Float32")
        || required_field(output, "channels")?.as_u64() != Some(u64::from(source.channels()))
        || required_field(output, "sample_rate")?.as_u64() != Some(u64::from(source.sample_rate()))
    {
        return Err("render Normalize output must be matching Float32");
    }
    let output = WaveformRenderOutputSpec::new(
        source.channels(),
        source.sample_rate(),
        32,
        WaveformSampleFormat::Float,
    )
    .map_err(|_| "render Normalize output specification is invalid")?;
    output
        .checked_arithmetic(frames)
        .map(|_| ())
        .map_err(|_| "render Normalize output arithmetic overflowed its bound")
}

fn validate_persisted_duplicate_double_value(value: &Value) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "source",
            "destination",
            "source_file_revision",
            "source_spec",
            "total_frames",
            "output",
            "collision_policy",
        ],
    )?;
    for field in [
        "source",
        "destination",
        "source_file_revision",
        "source_spec",
        "total_frames",
        "output",
        "collision_policy",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_endpoint(required_field(object, "source")?)?;
    validate_persisted_endpoint(required_field(object, "destination")?)?;
    validate_persisted_revision(required_field(object, "source_file_revision")?)?;
    let source = parse_persisted_source_spec(required_field(object, "source_spec")?)?;
    let frames = required_field(object, "total_frames")?
        .as_u64()
        .ok_or("render total frames must be an unsigned integer")?;
    validate_frame_count_raw(frames)?;
    let output = parse_persisted_render_output_spec(required_field(object, "output")?)?;
    if output != WaveformRenderOutputSpec::from_source(source) {
        return Err("render DuplicateDouble output must preserve source specification");
    }
    if required_field(object, "collision_policy")?.as_str() != Some("NoReplace") {
        return Err("render DuplicateDouble collision policy must be NoReplace");
    }
    let doubled = frames
        .checked_mul(2)
        .ok_or("render DuplicateDouble frame arithmetic overflowed")?;
    if doubled > MAX_WAVEFORM_RENDER_FRAMES {
        return Err("render DuplicateDouble doubled frames exceed their bound");
    }
    output
        .checked_arithmetic(doubled)
        .map(|_| ())
        .map_err(|_| "render DuplicateDouble output arithmetic overflowed its bound")
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
        return Err("render endpoint participant ID must be a UUID string");
    }
    let source_id = required_field(object, "source_id")?
        .as_str()
        .ok_or("render endpoint source ID must be a string")?;
    if !valid_bounded_text(source_id, MAX_SOURCE_IDENTITY_BYTES) {
        return Err("render endpoint source ID is outside its bound");
    }
    let path = required_field(object, "relative_path")?
        .as_str()
        .ok_or("render endpoint relative path must be a string")?;
    if super::coordinator::StoredRelativePath::new(path).is_err() {
        return Err("render endpoint relative path is invalid");
    }
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
            .ok_or("render file revision identity must be a string")?;
        if !valid_bounded_text(text, maximum) {
            return Err("render file revision identity is outside its bound");
        }
    }
    required_field(object, "byte_len")?
        .as_u64()
        .ok_or("render file byte length must be an unsigned integer")?;
    let modified = required_field(object, "modified_unix_ns")?;
    if !modified.is_null() && modified.as_i64().is_none() {
        return Err("render modified timestamp must be a signed integer or null");
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
        .ok_or("render source channels must be an unsigned integer")?;
    let sample_rate = required_field(object, "sample_rate")?
        .as_u64()
        .ok_or("render source rate must be an unsigned integer")?;
    let bits = required_field(object, "bits_per_sample")?
        .as_u64()
        .ok_or("render source bit depth must be an unsigned integer")?;
    let format = required_field(object, "sample_format")?
        .as_str()
        .ok_or("render source format must be a string")?;
    let channels = u16::try_from(channels).map_err(|_| "render source specification is invalid")?;
    let sample_rate =
        u32::try_from(sample_rate).map_err(|_| "render source specification is invalid")?;
    let bits = u16::try_from(bits).map_err(|_| "render source specification is invalid")?;
    let format = match format {
        "Integer" => WaveformSampleFormat::Integer,
        "Float" => WaveformSampleFormat::Float,
        _ => return Err("render source format is unsupported"),
    };
    WaveformSourceSpec::new(channels, sample_rate, bits, format)
        .map_err(|_| "render source specification is invalid")
}

fn parse_persisted_render_output_spec(
    value: &Value,
) -> Result<WaveformRenderOutputSpec, &'static str> {
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
        .ok_or("render output channels must be an unsigned 16-bit integer")?;
    let sample_rate = required_field(object, "sample_rate")?
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("render output rate must be an unsigned 32-bit integer")?;
    let bits = required_field(object, "bits_per_sample")?
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .ok_or("render output bit depth must be an unsigned 16-bit integer")?;
    let format = match required_field(object, "sample_format")?
        .as_str()
        .ok_or("render output format must be a string")?
    {
        "Integer" => WaveformSampleFormat::Integer,
        "Float" => WaveformSampleFormat::Float,
        _ => return Err("render output format is unsupported"),
    };
    WaveformRenderOutputSpec::new(channels, sample_rate, bits, format)
        .map_err(|_| "render output specification is invalid")
}

fn validate_frame_count_raw(frames: u64) -> Result<(), &'static str> {
    if frames == 0 || frames > MAX_WAVEFORM_RENDER_FRAMES {
        Err("render source frame count is outside its bound")
    } else {
        Ok(())
    }
}

fn valid_bounded_text(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

fn strict_object<'a>(
    value: &'a Value,
    allowed: &[&str],
) -> Result<&'a Map<String, Value>, &'static str> {
    let object = value
        .as_object()
        .ok_or("persisted render arguments must be an object")?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("persisted render arguments contain an unknown field");
    }
    if object.len() != allowed.len() {
        return Err("persisted render arguments have an incomplete field set");
    }
    Ok(object)
}

fn required_field<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a Value, &'static str> {
    object
        .get(field)
        .ok_or("persisted render arguments are missing a required field")
}

#[cfg(test)]
mod tests {
    use super::super::participants::ParticipantId;
    use super::super::participants::{
        ParticipantCheckpoint, ParticipantCheckpointState, ParticipantDescriptor, StoredSourceFence,
    };
    use super::*;
    use crate::sample_sources::SourceId;

    fn source_spec(bits: u16, format: WaveformSampleFormat) -> WaveformSourceSpec {
        WaveformSourceSpec::new(2, 48_000, bits, format).expect("source spec")
    }

    #[test]
    fn duplicate_double_uses_source_encoding_and_checked_doubled_frames() {
        let output =
            WaveformRenderOutputSpec::from_source(source_spec(8, WaveformSampleFormat::Integer));
        let arithmetic = output.checked_arithmetic(2).expect("arithmetic");
        assert_eq!(arithmetic.output_frames(), 2);
        assert_eq!(arithmetic.block_align(), 2);
        assert_eq!(arithmetic.data_bytes(), 4);
        assert_eq!(arithmetic.riff_chunk_size(), 40);
    }

    #[test]
    fn render_output_arithmetic_uses_hound_extensible_header_and_checked_bounds() {
        let output = WaveformRenderOutputSpec::new(3, 48_000, 24, WaveformSampleFormat::Integer)
            .expect("extensible output spec");
        let arithmetic = output.checked_arithmetic(2).expect("arithmetic");
        assert_eq!(arithmetic.block_align(), 9);
        assert_eq!(arithmetic.byte_rate(), 432_000);
        assert_eq!(arithmetic.data_bytes(), 18);
        assert_eq!(arithmetic.riff_chunk_size(), 78);

        let float_output =
            WaveformRenderOutputSpec::new(2, 48_000, 32, WaveformSampleFormat::Float)
                .expect("float output spec");
        assert!(matches!(
            float_output.checked_arithmetic(MAX_WAVEFORM_RENDER_FRAMES),
            Err(RenderCommandValidationError::OutputArithmeticOverflow)
        ));
    }

    #[test]
    fn duplicate_double_rejects_doubled_frame_overflow() {
        let output =
            WaveformRenderOutputSpec::from_source(source_spec(16, WaveformSampleFormat::Integer));
        let source = CopyEndpoint::new(
            ParticipantId::new(),
            SourceId::from_string("source"),
            super::super::coordinator::StoredRelativePath::new("one.wav").expect("path"),
        )
        .expect("endpoint");
        let destination = CopyEndpoint::new(
            source.participant_id(),
            source.source_id().clone(),
            super::super::coordinator::StoredRelativePath::new("two.wav").expect("path"),
        )
        .expect("endpoint");
        let revision =
            WaveformFileRevisionObservation::new("file", "change", 10, None).expect("revision");
        assert!(matches!(
            WaveformDuplicateDoubleIntentArgs::new(
                source,
                destination,
                revision,
                source_spec(16, WaveformSampleFormat::Integer),
                MAX_WAVEFORM_RENDER_FRAMES,
                output,
                CopyCollisionPolicy::NoReplace,
            ),
            Err(RenderCommandValidationError::OutputArithmeticOverflow)
        ));
    }

    #[test]
    fn duplicate_double_untrusted_deserialize_frame_getter_is_fallible() {
        let value = serde_json::json!({
            "source": {
                "participant_id": uuid::Uuid::new_v4().to_string(),
                "source_id": "source",
                "relative_path": "one.wav"
            },
            "destination": {
                "participant_id": uuid::Uuid::new_v4().to_string(),
                "source_id": "source",
                "relative_path": "two.wav"
            },
            "source_file_revision": {
                "filesystem_identity": "file",
                "change_marker": "change",
                "byte_len": 10,
                "modified_unix_ns": null
            },
            "source_spec": {
                "channels": 2,
                "sample_rate": 48000,
                "bits_per_sample": 16,
                "sample_format": "Integer"
            },
            "total_frames": u64::MAX,
            "output": {
                "channels": 2,
                "sample_rate": 48000,
                "bits_per_sample": 16,
                "sample_format": "Integer"
            },
            "collision_policy": "NoReplace"
        });
        let args: WaveformDuplicateDoubleIntentArgs =
            serde_json::from_value(value).expect("malformed shape remains decodable");
        assert!(args.output_frames().is_err());
    }

    fn two_source_participants(same_root: bool) -> (ParticipantSet, ParticipantId, ParticipantId) {
        two_source_participants_with_filesystem(same_root, true)
    }

    fn two_source_participants_with_filesystem(
        same_root: bool,
        include_filesystem: bool,
    ) -> (ParticipantSet, ParticipantId, ParticipantId) {
        let source_a = ParticipantId::new();
        let source_b = ParticipantId::new();
        let filesystem = ParticipantId::new();
        let source_a_id = SourceId::from_string("source-a");
        let source_b_id = SourceId::from_string("source-b");
        let root_b = if same_root { "root-a" } else { "root-b" };
        let mut descriptors = vec![
            ParticipantDescriptor::new(
                source_a,
                ParticipantKind::SourceDatabase,
                ParticipantRequirement::Required,
                Some(
                    StoredSourceFence::new(source_a_id, "root-a", "database-a", 1, 7, 1)
                        .expect("source A fence"),
                ),
                None,
            )
            .expect("source A descriptor"),
            ParticipantDescriptor::new(
                source_b,
                ParticipantKind::SourceDatabase,
                ParticipantRequirement::Required,
                Some(
                    StoredSourceFence::new(source_b_id, root_b, "database-b", 1, 8, 1)
                        .expect("source B fence"),
                ),
                None,
            )
            .expect("source B descriptor"),
        ];
        let mut checkpoint_ids = vec![source_a, source_b];
        if include_filesystem {
            descriptors.push(
                ParticipantDescriptor::new(
                    filesystem,
                    ParticipantKind::Filesystem,
                    ParticipantRequirement::Required,
                    None,
                    None,
                )
                .expect("filesystem descriptor"),
            );
            checkpoint_ids.push(filesystem);
        }
        let checkpoints = checkpoint_ids
            .into_iter()
            .map(|id| {
                ParticipantCheckpoint::new(id, ParticipantCheckpointState::Pending, None, None)
                    .expect("pending checkpoint")
            })
            .collect();
        (
            ParticipantSet::new(descriptors, checkpoints).expect("participant set"),
            source_a,
            source_b,
        )
    }

    fn normalize_args(
        source_participant: ParticipantId,
        source_id: &str,
    ) -> WaveformNormalizeIntentArgs {
        let source_spec = source_spec(16, WaveformSampleFormat::Integer);
        let source = CopyEndpoint::new(
            source_participant,
            SourceId::from_string(source_id),
            super::super::coordinator::StoredRelativePath::new("in.wav").expect("source path"),
        )
        .expect("source endpoint");
        WaveformNormalizeIntentArgs::new(
            source,
            WaveformFileRevisionObservation::new("file", "change", 10, None).expect("revision"),
            source_spec,
            10,
            WaveformOutputSpec::float32(source_spec.channels(), source_spec.sample_rate()),
        )
        .expect("normalize arguments")
    }

    fn duplicate_args(
        source_participant: ParticipantId,
        destination_participant: ParticipantId,
        source_id: &str,
        destination_id: &str,
        source_path: &str,
        destination_path: &str,
    ) -> WaveformDuplicateDoubleIntentArgs {
        let source = CopyEndpoint::new(
            source_participant,
            SourceId::from_string(source_id),
            super::super::coordinator::StoredRelativePath::new(source_path).expect("source path"),
        )
        .expect("source endpoint");
        let destination = CopyEndpoint::new(
            destination_participant,
            SourceId::from_string(destination_id),
            super::super::coordinator::StoredRelativePath::new(destination_path)
                .expect("destination path"),
        )
        .expect("destination endpoint");
        let source_spec = source_spec(8, WaveformSampleFormat::Integer);
        WaveformDuplicateDoubleIntentArgs::new(
            source,
            destination,
            WaveformFileRevisionObservation::new("file", "change", 10, None).expect("revision"),
            source_spec,
            10,
            WaveformRenderOutputSpec::from_source(source_spec),
            CopyCollisionPolicy::NoReplace,
        )
        .expect("duplicate arguments")
    }

    #[test]
    fn duplicate_double_allows_cross_source_and_parent_destinations() {
        let (participants, source_participant, destination_participant) =
            two_source_participants(false);
        let args = duplicate_args(
            source_participant,
            destination_participant,
            "source-a",
            "source-b",
            "in.wav",
            "archive/out.wav",
        );
        assert_eq!(args.metadata_policy(), RenderMetadataPolicy::Fresh);
        assert!(args.validate_against_participants(&participants).is_ok());
    }

    #[test]
    fn duplicate_double_rejects_same_physical_entry_through_source_aliases() {
        let (participants, source_participant, destination_participant) =
            two_source_participants(true);
        let args = duplicate_args(
            source_participant,
            destination_participant,
            "source-a",
            "source-b",
            "same.wav",
            "same.wav",
        );
        assert!(matches!(
            args.validate_against_participants(&participants),
            Err(RenderCommandValidationError::IdenticalPhysicalEntry)
        ));
    }

    #[test]
    fn render_commands_reject_missing_or_wrong_participant_roles() {
        let (participants, source_participant, destination_participant) =
            two_source_participants(false);
        let filesystem_participant = participants
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.kind() == ParticipantKind::Filesystem)
            .expect("filesystem participant")
            .id();

        let normalize = normalize_args(source_participant, "source-a");
        assert!(matches!(
            normalize.validate_against_participants(&participants),
            Ok(())
        ));
        assert!(matches!(
            normalize_args(ParticipantId::new(), "source-a")
                .validate_against_participants(&participants),
            Err(RenderCommandValidationError::SourceParticipant(
                CopyIntentValidationError::ParticipantMissing { .. }
            ))
        ));
        assert!(matches!(
            normalize_args(source_participant, "source-b")
                .validate_against_participants(&participants),
            Err(RenderCommandValidationError::SourceParticipant(
                CopyIntentValidationError::ParticipantSourceMismatch { .. }
            ))
        ));
        assert!(matches!(
            normalize_args(filesystem_participant, "source-a")
                .validate_against_participants(&participants),
            Err(RenderCommandValidationError::SourceParticipant(
                CopyIntentValidationError::ParticipantWrongKind { .. }
            ))
        ));

        let duplicate = duplicate_args(
            source_participant,
            destination_participant,
            "source-a",
            "source-b",
            "in.wav",
            "out.wav",
        );
        assert!(matches!(
            duplicate.validate_against_participants(&participants),
            Ok(())
        ));
        let duplicate_wrong_source = duplicate_args(
            filesystem_participant,
            destination_participant,
            "source-a",
            "source-b",
            "in.wav",
            "out.wav",
        );
        assert!(matches!(
            duplicate_wrong_source.validate_against_participants(&participants),
            Err(RenderCommandValidationError::SourceParticipant(
                CopyIntentValidationError::ParticipantWrongKind { .. }
            ))
        ));
        assert!(matches!(
            duplicate_args(
                ParticipantId::new(),
                destination_participant,
                "source-a",
                "source-b",
                "in.wav",
                "out.wav",
            )
            .validate_against_participants(&participants),
            Err(RenderCommandValidationError::SourceParticipant(
                CopyIntentValidationError::ParticipantMissing { .. }
            ))
        ));
        assert!(matches!(
            duplicate_args(
                source_participant,
                destination_participant,
                "source-b",
                "source-b",
                "in.wav",
                "out.wav",
            )
            .validate_against_participants(&participants),
            Err(RenderCommandValidationError::SourceParticipant(
                CopyIntentValidationError::ParticipantSourceMismatch { .. }
            ))
        ));
        let duplicate_wrong_destination = duplicate_args(
            source_participant,
            filesystem_participant,
            "source-a",
            "source-b",
            "in.wav",
            "out.wav",
        );
        assert!(matches!(
            duplicate_wrong_destination.validate_against_participants(&participants),
            Err(RenderCommandValidationError::DestinationParticipant(
                CopyIntentValidationError::ParticipantWrongKind { .. }
            ))
        ));
        assert!(matches!(
            duplicate_args(
                source_participant,
                destination_participant,
                "source-a",
                "source-a",
                "in.wav",
                "out.wav",
            )
            .validate_against_participants(&participants),
            Err(RenderCommandValidationError::DestinationParticipant(
                CopyIntentValidationError::ParticipantSourceMismatch { .. }
            ))
        ));
    }

    #[test]
    fn render_commands_require_a_required_filesystem_participant() {
        let (participants, source_participant, destination_participant) =
            two_source_participants_with_filesystem(false, false);
        assert!(matches!(
            normalize_args(source_participant, "source-a")
                .validate_against_participants(&participants),
            Err(RenderCommandValidationError::RequiredFilesystemParticipantMissing)
        ));
        assert!(matches!(
            duplicate_args(
                source_participant,
                destination_participant,
                "source-a",
                "source-b",
                "in.wav",
                "out.wav",
            )
            .validate_against_participants(&participants),
            Err(RenderCommandValidationError::RequiredFilesystemParticipantMissing)
        ));

        let (valid, valid_source_participant, _) = two_source_participants(false);
        let mut optional_filesystem = serde_json::to_value(valid).expect("participants JSON");
        optional_filesystem["descriptors"][2]["requirement"] = Value::from("OptionalRebuildable");
        let optional_filesystem: ParticipantSet =
            serde_json::from_value(optional_filesystem).expect("decoded participant inventory");
        assert!(matches!(
            normalize_args(valid_source_participant, "source-a")
                .validate_against_participants(&optional_filesystem),
            Err(RenderCommandValidationError::Participants(_))
        ));

        let (valid, valid_source_participant, valid_destination_participant) =
            two_source_participants(false);
        let mut missing_source_fence = serde_json::to_value(valid).expect("participants JSON");
        missing_source_fence["descriptors"][0]["source_fence"] = Value::Null;
        let missing_source_fence: ParticipantSet =
            serde_json::from_value(missing_source_fence).expect("decoded participant inventory");
        assert!(matches!(
            normalize_args(valid_source_participant, "source-a")
                .validate_against_participants(&missing_source_fence),
            Err(RenderCommandValidationError::Participants(_))
        ));
        assert!(matches!(
            duplicate_args(
                valid_source_participant,
                valid_destination_participant,
                "source-a",
                "source-b",
                "in.wav",
                "out.wav",
            )
            .validate_against_participants(&missing_source_fence),
            Err(RenderCommandValidationError::Participants(_))
        ));
    }

    #[test]
    fn render_commands_reject_malformed_decoded_revision_and_source_spec() {
        let mut value = serde_json::to_value(normalize_args(ParticipantId::new(), "source-a"))
            .expect("normalize JSON");
        value["source_file_revision"]["filesystem_identity"] = Value::String(String::new());
        let args: WaveformNormalizeIntentArgs =
            serde_json::from_value(value).expect("decoded revision observation");
        assert!(matches!(
            args.validate(),
            Err(RenderCommandValidationError::InvalidFileRevision)
        ));

        let mut value = serde_json::to_value(normalize_args(ParticipantId::new(), "source-a"))
            .expect("normalize JSON");
        value["source_spec"]["sample_rate"] = Value::from(0_u64);
        let args: WaveformNormalizeIntentArgs =
            serde_json::from_value(value).expect("decoded source observation");
        assert!(matches!(
            args.validate(),
            Err(RenderCommandValidationError::InvalidSourceSpec)
        ));
    }

    #[test]
    fn normalize_untrusted_zero_channel_spec_is_rejected_without_clamping() {
        let value = serde_json::json!({
            "source": {
                "participant_id": uuid::Uuid::new_v4().to_string(),
                "source_id": "source",
                "relative_path": "one.wav"
            },
            "source_file_revision": {
                "filesystem_identity": "file",
                "change_marker": "change",
                "byte_len": 10,
                "modified_unix_ns": null
            },
            "source_spec": {
                "channels": 0,
                "sample_rate": 48000,
                "bits_per_sample": 16,
                "sample_format": "Integer"
            },
            "total_frames": 10,
            "output": {
                "encoding": "Float32",
                "channels": 0,
                "sample_rate": 48000
            }
        });
        let args: WaveformNormalizeIntentArgs =
            serde_json::from_value(value).expect("untrusted struct decodes");
        assert!(matches!(
            args.validate(),
            Err(RenderCommandValidationError::InvalidSourceSpec)
        ));
    }
}
