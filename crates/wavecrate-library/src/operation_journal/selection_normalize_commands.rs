//! Storage-neutral observations for selection-only peak normalization.
//!
//! The original selection Normalize operation rewrites the complete source file as Float32 while
//! normalizing only one selected interleaved frame range. This module records the source revision,
//! WAV layout, exact range, and the already-resolved edge-fade frame count before a later owner
//! performs live checks. It does not open or capture audio, compute a peak, retain a handle, reserve
//! capacity, or authorize replacement.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use super::coordinator::{
    CopyEndpoint, CopyEndpointRole, CopyIntentValidationError, MAX_WAVEFORM_CHANGE_MARKER_BYTES,
    MAX_WAVEFORM_EDIT_FRAMES, MAX_WAVEFORM_FILE_IDENTITY_BYTES, StoredRelativePath,
    StoredRelativePathError, WaveformEditFrameRange, WaveformFileRevisionObservation,
    WaveformOutputSpec, WaveformSampleFormat, WaveformSourceSpec,
};
use super::model::UniversalCommandKind;
use super::participants::{
    MAX_SOURCE_IDENTITY_BYTES, ParticipantKind, ParticipantRequirement, ParticipantSet,
    ParticipantValidationError,
};
use super::render_commands::WaveformRenderOutputSpec;

/// Fixed peak policy matching the original selection Normalize algorithm.
///
/// The later audio worker finds the maximum absolute value in the selected interleaved range,
/// leaves the range unchanged when that peak is non-positive or non-finite, and otherwise scales
/// by its reciprocal with the original clamp-to-`[-1, 1]` behavior. This policy is an invariant and
/// is therefore intentionally not serialized into the initial intent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionNormalizePeakPolicy {
    /// Normalize the selected range to full scale using its maximum absolute sample.
    FullScaleMaxAbs,
}

/// Fixed edge recipe matching the original selection Normalize crossfades.
///
/// The later worker blends original samples into normalized samples at the start and normalized
/// samples back into original samples at the end. It uses `frame / max(fade_frames - 1, 1)` and
/// forces the one-frame end fade to `t = 1`. The resolved frame count is persisted separately, so
/// a later owner does not reinterpret a UI duration setting. This policy is not serialized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionNormalizeEdgeFadePolicy {
    /// Crossfade each selected edge against the original samples.
    OriginalEdgeCrossfade,
}

/// Typed V12 selection Normalize observation.
///
/// The source endpoint and strong file revision are observations captured before a later owner
/// performs live checks. `range` is a non-empty half-open source-frame range. The full source frame
/// sequence is retained in the output, which is always Float32 with the source channel count and
/// rate. `edge_fade_frames` is already resolved from the original 5 ms controller default (or an
/// equivalent caller setting) and is bounded by half the selected range. No duration, peak, gain,
/// audio samples, metadata snapshot, filesystem handle, capacity reservation, or execution
/// authority is carried here.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionNormalizeIntentArgs {
    source: CopyEndpoint,
    source_file_revision: WaveformFileRevisionObservation,
    source_spec: WaveformSourceSpec,
    total_frames: u64,
    range: WaveformEditFrameRange,
    edge_fade_frames: u64,
    output: WaveformOutputSpec,
}

impl SelectionNormalizeIntentArgs {
    /// Construct a bounded selection Normalize observation without opening or reading the source.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: CopyEndpoint,
        source_file_revision: WaveformFileRevisionObservation,
        source_spec: WaveformSourceSpec,
        total_frames: u64,
        range: WaveformEditFrameRange,
        edge_fade_frames: u64,
        output: WaveformOutputSpec,
    ) -> Result<Self, SelectionNormalizeValidationError> {
        let args = Self {
            source,
            source_file_revision,
            source_spec,
            total_frames,
            range,
            edge_fade_frames,
            output,
        };
        args.validate_shape()?;
        Ok(args)
    }

    /// Validate the observation without resolving any participant or live capability.
    pub fn validate(&self) -> Result<(), SelectionNormalizeValidationError> {
        self.validate_shape()
    }

    /// Validate source and filesystem participant fences for this in-place operation.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), SelectionNormalizeValidationError> {
        self.validate_shape()?;
        participants.validate_for_new_intent()?;
        validate_source_endpoint(participants, &self.source)?;
        if !participants.descriptors().iter().any(|descriptor| {
            descriptor.kind() == ParticipantKind::Filesystem
                && descriptor.requirement() == ParticipantRequirement::Required
        }) {
            return Err(SelectionNormalizeValidationError::RequiredFilesystemParticipantMissing);
        }
        Ok(())
    }

    /// Return the source endpoint observation.
    pub fn source(&self) -> &CopyEndpoint {
        &self.source
    }

    /// Return the strong file revision observation.
    pub fn source_file_revision(&self) -> &WaveformFileRevisionObservation {
        &self.source_file_revision
    }

    /// Return the source WAV specification.
    pub const fn source_spec(&self) -> WaveformSourceSpec {
        self.source_spec
    }

    /// Return the captured complete source frame count.
    pub const fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// Return the exact half-open selected source-frame range.
    pub const fn range(&self) -> WaveformEditFrameRange {
        self.range
    }

    /// Return the resolved edge-fade frame count.
    pub const fn edge_fade_frames(&self) -> u64 {
        self.edge_fade_frames
    }

    /// Return the explicit Float32 output specification.
    pub const fn output(&self) -> WaveformOutputSpec {
        self.output
    }

    /// Return the checked number of selected frames.
    ///
    /// Direct deserialization intentionally permits an unvalidated value to be inspected. This
    /// getter performs checked subtraction and source-bound validation rather than panicking.
    pub fn selected_frames(&self) -> Result<u64, SelectionNormalizeValidationError> {
        checked_selected_frames(self.total_frames, self.range)
    }

    /// Return the checked full-file output frame count.
    pub fn output_frames(&self) -> Result<u64, SelectionNormalizeValidationError> {
        validate_frame_count(self.total_frames)?;
        Ok(self.total_frames)
    }

    /// Return the fixed full-scale peak policy.
    pub const fn peak_policy(&self) -> SelectionNormalizePeakPolicy {
        SelectionNormalizePeakPolicy::FullScaleMaxAbs
    }

    /// Return the fixed original-edge crossfade recipe policy.
    pub const fn edge_fade_policy(&self) -> SelectionNormalizeEdgeFadePolicy {
        SelectionNormalizeEdgeFadePolicy::OriginalEdgeCrossfade
    }

    /// Return checked WAV arithmetic for the complete Float32 output file.
    pub fn output_arithmetic(
        &self,
    ) -> Result<
        super::render_commands::WaveformRenderOutputArithmetic,
        SelectionNormalizeValidationError,
    > {
        self.validate_shape()?;
        self.checked_output_arithmetic()
    }

    fn validate_shape(&self) -> Result<(), SelectionNormalizeValidationError> {
        validate_endpoint_shape(&self.source)?;
        self.source_file_revision
            .validate()
            .map_err(|_| SelectionNormalizeValidationError::InvalidFileRevision)?;
        self.source_spec
            .validate()
            .map_err(|_| SelectionNormalizeValidationError::InvalidSourceSpec)?;
        let selected_frames = checked_selected_frames(self.total_frames, self.range)?;
        if self.edge_fade_frames > selected_frames / 2 {
            return Err(SelectionNormalizeValidationError::EdgeFadeTooLong {
                edge_fade_frames: self.edge_fade_frames,
                selected_frames,
            });
        }
        self.output
            .validate_against_source(self.source_spec)
            .map_err(|_| SelectionNormalizeValidationError::InvalidOutputSpec)?;
        self.checked_output_arithmetic()?;
        Ok(())
    }

    fn checked_output_arithmetic(
        &self,
    ) -> Result<
        super::render_commands::WaveformRenderOutputArithmetic,
        SelectionNormalizeValidationError,
    > {
        WaveformRenderOutputSpec::float32(self.output)
            .checked_arithmetic(self.total_frames)
            .map_err(|_| SelectionNormalizeValidationError::OutputArithmeticOverflow)
    }
}

impl SelectionNormalizeIntentArgs {
    /// Return the explicit V12 command kind represented by this observation.
    pub const fn command_kind(&self) -> UniversalCommandKind {
        UniversalCommandKind::DestructiveEdit
    }
}

/// Failure while constructing or validating a V12 selection Normalize observation.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum SelectionNormalizeValidationError {
    /// A source endpoint path is not portable.
    #[error("invalid selection Normalize source path: {0}")]
    InvalidPath(#[from] StoredRelativePathError),
    /// A source identity is outside its bounded text contract.
    #[error("selection Normalize source identity is invalid")]
    InvalidSourceIdentity,
    /// The strong source file revision was malformed or outside its bound.
    #[error("selection Normalize file revision is invalid")]
    InvalidFileRevision,
    /// The source WAV specification was malformed or unsupported.
    #[error("selection Normalize source specification is invalid")]
    InvalidSourceSpec,
    /// The full source frame count was zero or outside its checked bound.
    #[error("selection Normalize source frame count is invalid")]
    InvalidFrameCount,
    /// The selected range was empty or had an overflowing frame subtraction.
    #[error("selection Normalize frame range is invalid")]
    InvalidFrameRange,
    /// The selected range extended beyond the complete source.
    #[error("selection Normalize frame range is outside the source")]
    SelectedRangeOutsideSource,
    /// The resolved edge fade is longer than half of the selected range.
    #[error(
        "selection Normalize edge fade has {edge_fade_frames} frames for a {selected_frames}-frame selection"
    )]
    EdgeFadeTooLong {
        /// Persisted edge-fade frame count.
        edge_fade_frames: u64,
        /// Checked selected range length.
        selected_frames: u64,
    },
    /// The output is not matching Float32 with source channels and rate.
    #[error("selection Normalize output specification is invalid")]
    InvalidOutputSpec,
    /// Full-file Float32 WAV arithmetic exceeded its bounded representation.
    #[error("selection Normalize output arithmetic overflowed its bound")]
    OutputArithmeticOverflow,
    /// The source endpoint participant failed its role or fence check.
    #[error("invalid selection Normalize source participant: {0}")]
    SourceParticipant(#[source] CopyIntentValidationError),
    /// No required filesystem participant was supplied.
    #[error("selection Normalize requires a required Filesystem participant")]
    RequiredFilesystemParticipantMissing,
    /// Participant shape or checkpoint validation failed.
    #[error("invalid selection Normalize participants: {0}")]
    Participants(#[from] ParticipantValidationError),
}

fn validate_endpoint_shape(
    endpoint: &CopyEndpoint,
) -> Result<(), SelectionNormalizeValidationError> {
    endpoint
        .relative_path()
        .validate()
        .map_err(SelectionNormalizeValidationError::InvalidPath)?;
    let source_id = endpoint.source_id().as_str();
    if source_id.is_empty()
        || source_id.len() > MAX_SOURCE_IDENTITY_BYTES
        || source_id.chars().any(char::is_control)
    {
        return Err(SelectionNormalizeValidationError::InvalidSourceIdentity);
    }
    Ok(())
}

fn validate_source_endpoint<'a>(
    participants: &'a ParticipantSet,
    endpoint: &CopyEndpoint,
) -> Result<&'a super::participants::ParticipantDescriptor, SelectionNormalizeValidationError> {
    validate_endpoint_shape(endpoint)?;
    let descriptor = participants
        .descriptors()
        .iter()
        .find(|descriptor| descriptor.id() == endpoint.participant_id())
        .ok_or(SelectionNormalizeValidationError::SourceParticipant(
            CopyIntentValidationError::ParticipantMissing {
                role: CopyEndpointRole::Source,
                participant_id: endpoint.participant_id(),
            },
        ))?;
    if descriptor.kind() != ParticipantKind::SourceDatabase {
        return Err(SelectionNormalizeValidationError::SourceParticipant(
            CopyIntentValidationError::ParticipantWrongKind {
                role: CopyEndpointRole::Source,
                actual: descriptor.kind(),
            },
        ));
    }
    if descriptor.requirement() != ParticipantRequirement::Required {
        return Err(SelectionNormalizeValidationError::SourceParticipant(
            CopyIntentValidationError::ParticipantNotRequired {
                role: CopyEndpointRole::Source,
            },
        ));
    }
    let fence =
        descriptor
            .source_fence()
            .ok_or(SelectionNormalizeValidationError::SourceParticipant(
                CopyIntentValidationError::ParticipantFenceMissing {
                    role: CopyEndpointRole::Source,
                },
            ))?;
    if fence.source_id() != endpoint.source_id() {
        return Err(SelectionNormalizeValidationError::SourceParticipant(
            CopyIntentValidationError::ParticipantSourceMismatch {
                role: CopyEndpointRole::Source,
            },
        ));
    }
    Ok(descriptor)
}

fn validate_frame_count(frames: u64) -> Result<(), SelectionNormalizeValidationError> {
    if frames == 0 || frames > MAX_WAVEFORM_EDIT_FRAMES {
        Err(SelectionNormalizeValidationError::InvalidFrameCount)
    } else {
        Ok(())
    }
}

fn checked_selected_frames(
    total_frames: u64,
    range: WaveformEditFrameRange,
) -> Result<u64, SelectionNormalizeValidationError> {
    validate_frame_count(total_frames)?;
    let selected_frames = range
        .len()
        .map_err(|_| SelectionNormalizeValidationError::InvalidFrameRange)?;
    if selected_frames == 0 {
        return Err(SelectionNormalizeValidationError::InvalidFrameRange);
    }
    if range.end_frame() > total_frames {
        return Err(SelectionNormalizeValidationError::SelectedRangeOutsideSource);
    }
    Ok(selected_frames)
}

/// Validate the strict V12 selection Normalize JSON value before typed deserialization.
pub(crate) fn validate_persisted_selection_normalize_intent_args_value(
    value: &Value,
) -> Result<(), &'static str> {
    let object = strict_object(
        value,
        &[
            "source",
            "source_file_revision",
            "source_spec",
            "total_frames",
            "range",
            "edge_fade_frames",
            "output",
        ],
    )?;
    for field in [
        "source",
        "source_file_revision",
        "source_spec",
        "total_frames",
        "range",
        "edge_fade_frames",
        "output",
    ] {
        required_field(object, field)?;
    }
    validate_persisted_endpoint(required_field(object, "source")?)?;
    validate_persisted_revision(required_field(object, "source_file_revision")?)?;
    let source = parse_persisted_source_spec(required_field(object, "source_spec")?)?;
    let total_frames = required_field(object, "total_frames")?
        .as_u64()
        .ok_or("selection Normalize total frames must be an unsigned integer")?;
    validate_frame_count_raw(total_frames)?;
    let range = parse_persisted_range(required_field(object, "range")?)?;
    let selected_frames = range
        .len()
        .map_err(|_| "selection Normalize frame range is invalid")?;
    if selected_frames == 0 {
        return Err("selection Normalize frame range is invalid");
    }
    if range.end_frame() > total_frames {
        return Err("selection Normalize frame range is outside the source");
    }
    let edge_fade_frames = required_field(object, "edge_fade_frames")?
        .as_u64()
        .ok_or("selection Normalize edge fade frames must be an unsigned integer")?;
    if edge_fade_frames > selected_frames / 2 {
        return Err("selection Normalize edge fade exceeds half the selected range");
    }
    let output = strict_object(
        required_field(object, "output")?,
        &["encoding", "channels", "sample_rate"],
    )?;
    for field in ["encoding", "channels", "sample_rate"] {
        required_field(output, field)?;
    }
    if required_field(output, "encoding")?.as_str() != Some("Float32") {
        return Err("selection Normalize output must be Float32");
    }
    let channels = required_field(output, "channels")?
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .ok_or("selection Normalize output channels are invalid")?;
    let sample_rate = required_field(output, "sample_rate")?
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("selection Normalize output sample rate is invalid")?;
    if channels != source.channels() || sample_rate != source.sample_rate() {
        return Err("selection Normalize output must preserve source channels and rate");
    }
    WaveformRenderOutputSpec::new(channels, sample_rate, 32, WaveformSampleFormat::Float)
        .map_err(|_| "selection Normalize output specification is invalid")?
        .checked_arithmetic(total_frames)
        .map(|_| ())
        .map_err(|_| "selection Normalize output arithmetic overflowed its bound")
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
        return Err("selection Normalize endpoint participant ID must be a UUID string");
    }
    let source_id = required_field(object, "source_id")?
        .as_str()
        .ok_or("selection Normalize endpoint source ID must be a string")?;
    if !valid_bounded_text(source_id, MAX_SOURCE_IDENTITY_BYTES) {
        return Err("selection Normalize endpoint source ID is outside its bound");
    }
    let path = required_field(object, "relative_path")?
        .as_str()
        .ok_or("selection Normalize endpoint relative path must be a string")?;
    StoredRelativePath::new(path)
        .map_err(|_| "selection Normalize endpoint relative path is invalid")?;
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
            .ok_or("selection Normalize revision identity must be a string")?;
        if !valid_bounded_text(text, maximum) {
            return Err("selection Normalize revision identity is outside its bound");
        }
    }
    required_field(object, "byte_len")?
        .as_u64()
        .ok_or("selection Normalize revision byte length must be unsigned")?;
    let modified = required_field(object, "modified_unix_ns")?;
    if !modified.is_null() && modified.as_i64().is_none() {
        return Err("selection Normalize revision timestamp must be signed or null");
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
        .ok_or("selection Normalize source channels are invalid")?;
    let sample_rate = required_field(object, "sample_rate")?
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("selection Normalize source sample rate is invalid")?;
    let bits = required_field(object, "bits_per_sample")?
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .ok_or("selection Normalize source bit depth is invalid")?;
    let sample_format = match required_field(object, "sample_format")?
        .as_str()
        .ok_or("selection Normalize source format must be a string")?
    {
        "Integer" => WaveformSampleFormat::Integer,
        "Float" => WaveformSampleFormat::Float,
        _ => return Err("selection Normalize source format is unsupported"),
    };
    WaveformSourceSpec::new(channels, sample_rate, bits, sample_format)
        .map_err(|_| "selection Normalize source specification is invalid")
}

fn parse_persisted_range(value: &Value) -> Result<WaveformEditFrameRange, &'static str> {
    let object = strict_object(value, &["start_frame", "end_frame"])?;
    for field in ["start_frame", "end_frame"] {
        required_field(object, field)?;
    }
    let start = required_field(object, "start_frame")?
        .as_u64()
        .ok_or("selection Normalize range start must be unsigned")?;
    let end = required_field(object, "end_frame")?
        .as_u64()
        .ok_or("selection Normalize range end must be unsigned")?;
    WaveformEditFrameRange::new(start, end)
        .map_err(|_| "selection Normalize frame range is invalid")
}

fn validate_frame_count_raw(frames: u64) -> Result<(), &'static str> {
    if frames == 0 || frames > MAX_WAVEFORM_EDIT_FRAMES {
        Err("selection Normalize source frame count is outside its bound")
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
        .ok_or("persisted selection Normalize value must be an object")?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("persisted selection Normalize value contains an unknown field");
    }
    if object.len() != allowed.len() {
        return Err("persisted selection Normalize value has an incomplete field set");
    }
    Ok(object)
}

fn required_field<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a Value, &'static str> {
    object
        .get(field)
        .ok_or("persisted selection Normalize value is missing a required field")
}

#[cfg(test)]
mod tests {
    use super::super::coordinator::{
        CopyEndpoint, WaveformFileRevisionObservation, WaveformOutputSpec,
    };
    use super::super::participants::{
        ParticipantCheckpoint, ParticipantCheckpointState, ParticipantDescriptor, ParticipantId,
        StoredSourceFence,
    };
    use super::*;
    use crate::sample_sources::SourceId;

    fn fixture() -> (ParticipantSet, SelectionNormalizeIntentArgs) {
        let source_participant = ParticipantId::from_uuid(uuid::Uuid::from_u128(1));
        let filesystem_participant = ParticipantId::from_uuid(uuid::Uuid::from_u128(2));
        let source_id = SourceId::from_string("source-a");
        let fence = StoredSourceFence::new(source_id.clone(), "source-root", "source-db", 1, 7, 1)
            .expect("source fence");
        let participants = ParticipantSet::new(
            vec![
                ParticipantDescriptor::new(
                    source_participant,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(fence),
                    None,
                )
                .expect("source descriptor"),
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
            StoredRelativePath::new("folder/sample.wav").expect("source path"),
        )
        .expect("source endpoint");
        let args = SelectionNormalizeIntentArgs::new(
            source,
            WaveformFileRevisionObservation::new("file", "change", 10, Some(11)).expect("revision"),
            source_spec,
            100,
            WaveformEditFrameRange::new(10, 50).expect("range"),
            20,
            WaveformOutputSpec::float32(2, 48_000),
        )
        .expect("selection Normalize args");
        (participants, args)
    }

    #[test]
    fn valid_selection_normalize_preserves_full_file_and_fixed_policies() {
        let (participants, args) = fixture();
        args.validate_against_participants(&participants)
            .expect("participant validation");
        assert_eq!(args.selected_frames().expect("selected frames"), 40);
        assert_eq!(args.output_frames().expect("output frames"), 100);
        assert_eq!(
            args.peak_policy(),
            SelectionNormalizePeakPolicy::FullScaleMaxAbs
        );
        assert_eq!(
            args.edge_fade_policy(),
            SelectionNormalizeEdgeFadePolicy::OriginalEdgeCrossfade
        );
        let arithmetic = args.output_arithmetic().expect("output arithmetic");
        assert_eq!(arithmetic.output_frames(), 100);
        assert_eq!(arithmetic.block_align(), 8);
    }

    #[test]
    fn selection_normalize_rejects_empty_out_of_source_and_too_long_fade() {
        let (participants, valid) = fixture();
        let mut empty = serde_json::to_value(&valid).expect("args JSON");
        empty["range"] = serde_json::json!({"start_frame": 10, "end_frame": 10});
        let empty: SelectionNormalizeIntentArgs =
            serde_json::from_value(empty).expect("untrusted empty range decodes");
        assert_eq!(
            empty.validate(),
            Err(SelectionNormalizeValidationError::InvalidFrameRange)
        );
        assert!(empty.selected_frames().is_err());

        let mut out_of_source = serde_json::to_value(&valid).expect("args JSON");
        out_of_source["range"] = serde_json::json!({"start_frame": 90, "end_frame": 101});
        let out_of_source: SelectionNormalizeIntentArgs =
            serde_json::from_value(out_of_source).expect("untrusted out-of-source decodes");
        assert_eq!(
            out_of_source.validate(),
            Err(SelectionNormalizeValidationError::SelectedRangeOutsideSource)
        );

        let mut too_long = serde_json::to_value(&valid).expect("args JSON");
        too_long["edge_fade_frames"] = Value::from(21_u64);
        let too_long: SelectionNormalizeIntentArgs =
            serde_json::from_value(too_long).expect("untrusted fade decodes");
        assert!(matches!(
            too_long.validate_against_participants(&participants),
            Err(SelectionNormalizeValidationError::EdgeFadeTooLong { .. })
        ));
    }

    #[test]
    fn direct_deserialization_getters_are_fallible_for_invalid_frames_and_output() {
        let (_, valid) = fixture();
        let mut malformed = serde_json::to_value(&valid).expect("args JSON");
        malformed["total_frames"] = Value::from(u64::MAX);
        let malformed: SelectionNormalizeIntentArgs =
            serde_json::from_value(malformed).expect("untrusted frame count decodes");
        assert!(malformed.output_frames().is_err());
        assert!(malformed.output_arithmetic().is_err());

        let mut malformed_range = serde_json::to_value(&valid).expect("args JSON");
        malformed_range["range"] = serde_json::json!({
            "start_frame": u64::MAX,
            "end_frame": u64::MAX
        });
        let malformed_range: SelectionNormalizeIntentArgs =
            serde_json::from_value(malformed_range).expect("untrusted range decodes");
        assert!(malformed_range.selected_frames().is_err());
    }

    #[test]
    fn selection_normalize_requires_fenced_source_and_filesystem_participants() {
        let (participants, args) = fixture();
        let mut missing_source = serde_json::to_value(&participants).expect("participants JSON");
        missing_source["descriptors"]
            .as_array_mut()
            .expect("descriptors")
            .remove(0);
        missing_source["checkpoints"]
            .as_array_mut()
            .expect("checkpoints")
            .remove(0);
        let missing_source: ParticipantSet =
            serde_json::from_value(missing_source).expect("missing source participants");
        assert!(matches!(
            args.validate_against_participants(&missing_source),
            Err(SelectionNormalizeValidationError::SourceParticipant(
                CopyIntentValidationError::ParticipantMissing { .. }
            ))
        ));

        let mut missing_filesystem =
            serde_json::to_value(&participants).expect("participants JSON");
        missing_filesystem["descriptors"]
            .as_array_mut()
            .expect("descriptors")
            .remove(1);
        missing_filesystem["checkpoints"]
            .as_array_mut()
            .expect("checkpoints")
            .remove(1);
        let missing_filesystem: ParticipantSet =
            serde_json::from_value(missing_filesystem).expect("missing filesystem participants");
        assert_eq!(
            args.validate_against_participants(&missing_filesystem),
            Err(SelectionNormalizeValidationError::RequiredFilesystemParticipantMissing)
        );
    }

    #[test]
    fn selection_normalize_rejects_participant_fence_role_and_inventory_mismatches() {
        let (participants, args) = fixture();

        let mut source_mismatch = serde_json::to_value(&args).expect("args JSON");
        source_mismatch["source"]["source_id"] = Value::from("different-source");
        let source_mismatch: SelectionNormalizeIntentArgs =
            serde_json::from_value(source_mismatch).expect("source mismatch decodes");
        assert!(matches!(
            source_mismatch.validate_against_participants(&participants),
            Err(SelectionNormalizeValidationError::SourceParticipant(
                CopyIntentValidationError::ParticipantSourceMismatch { .. }
            ))
        ));

        let mut optional_filesystem =
            serde_json::to_value(&participants).expect("participants JSON");
        optional_filesystem["descriptors"][1]["kind"] = Value::from("Artifact");
        optional_filesystem["descriptors"][1]["requirement"] = Value::from("OptionalRebuildable");
        optional_filesystem["descriptors"][1]["artifact_key"] = Value::from("filesystem");
        optional_filesystem["descriptors"][1]["source_fence"] =
            optional_filesystem["descriptors"][0]["source_fence"].clone();
        let optional_filesystem: ParticipantSet =
            serde_json::from_value(optional_filesystem).expect("optional filesystem");
        assert_eq!(
            args.validate_against_participants(&optional_filesystem),
            Err(SelectionNormalizeValidationError::RequiredFilesystemParticipantMissing)
        );

        let mut wrong_source_kind = serde_json::to_value(&participants).expect("participants JSON");
        wrong_source_kind["descriptors"][0]["kind"] = Value::from("Filesystem");
        wrong_source_kind["descriptors"][0]["source_fence"] = Value::Null;
        let wrong_source_kind: ParticipantSet =
            serde_json::from_value(wrong_source_kind).expect("wrong source kind");
        assert!(matches!(
            args.validate_against_participants(&wrong_source_kind),
            Err(SelectionNormalizeValidationError::SourceParticipant(
                CopyIntentValidationError::ParticipantWrongKind { .. }
            ))
        ));

        let mut wrong_filesystem_kind =
            serde_json::to_value(&participants).expect("participants JSON");
        wrong_filesystem_kind["descriptors"][1]["kind"] = Value::from("SourceDatabase");
        wrong_filesystem_kind["descriptors"][1]["source_fence"] = Value::Null;
        let wrong_filesystem_kind: ParticipantSet =
            serde_json::from_value(wrong_filesystem_kind).expect("wrong filesystem kind");
        assert!(matches!(
            args.validate_against_participants(&wrong_filesystem_kind),
            Err(SelectionNormalizeValidationError::Participants(_))
                | Err(SelectionNormalizeValidationError::RequiredFilesystemParticipantMissing)
        ));

        let mut duplicate_inventory =
            serde_json::to_value(&participants).expect("participants JSON");
        let duplicate_descriptor = duplicate_inventory["descriptors"][0].clone();
        duplicate_inventory["descriptors"]
            .as_array_mut()
            .expect("descriptor array")
            .push(duplicate_descriptor);
        let duplicate_inventory: ParticipantSet =
            serde_json::from_value(duplicate_inventory).expect("duplicate inventory");
        assert!(matches!(
            args.validate_against_participants(&duplicate_inventory),
            Err(SelectionNormalizeValidationError::Participants(_))
        ));
    }

    #[test]
    fn raw_selection_normalize_boundary_rejects_unknown_and_incomplete_values() {
        let (_, args) = fixture();
        let value = serde_json::to_value(&args).expect("args JSON");
        validate_persisted_selection_normalize_intent_args_value(&value)
            .expect("valid raw selection Normalize value");
        let mut unknown = value.clone();
        unknown["unknown"] = Value::Null;
        assert!(validate_persisted_selection_normalize_intent_args_value(&unknown).is_err());
        let mut incomplete = value;
        incomplete.as_object_mut().expect("object").remove("range");
        assert!(validate_persisted_selection_normalize_intent_args_value(&incomplete).is_err());

        let mut malformed_revision = serde_json::to_value(&args).expect("args JSON");
        malformed_revision["source_file_revision"]["filesystem_identity"] =
            Value::from("x".repeat(MAX_WAVEFORM_FILE_IDENTITY_BYTES + 1));
        assert!(
            validate_persisted_selection_normalize_intent_args_value(&malformed_revision).is_err()
        );

        let mut mismatched_output = serde_json::to_value(&args).expect("args JSON");
        mismatched_output["output"]["channels"] = Value::from(1_u64);
        assert!(
            validate_persisted_selection_normalize_intent_args_value(&mismatched_output).is_err()
        );

        let mut arithmetic_overflow = serde_json::to_value(&args).expect("args JSON");
        arithmetic_overflow["total_frames"] = Value::from(u64::from(u32::MAX));
        arithmetic_overflow["range"] = serde_json::json!({
            "start_frame": 1,
            "end_frame": 2
        });
        assert!(
            validate_persisted_selection_normalize_intent_args_value(&arithmetic_overflow).is_err()
        );

        let mut maximum_frames = serde_json::to_value(&args).expect("args JSON");
        maximum_frames["total_frames"] = Value::from(u64::from(u32::MAX));
        assert!(matches!(
            validate_persisted_selection_normalize_intent_args_value(&maximum_frames),
            Err("selection Normalize output arithmetic overflowed its bound")
        ));

        let mut invalid_source_spec = serde_json::to_value(&args).expect("args JSON");
        invalid_source_spec["source_spec"]["channels"] = Value::from(0_u64);
        assert!(
            validate_persisted_selection_normalize_intent_args_value(&invalid_source_spec).is_err()
        );
    }
}
