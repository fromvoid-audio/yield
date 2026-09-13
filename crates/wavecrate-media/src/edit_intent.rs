//! Pure adapters between media edit preparation and the journal's typed edit observations.
//!
//! These adapters copy bounded observations only. They do not inspect a path, open a WAV, retain
//! a lease, or establish authority to replace the source. The execution preparation boundary must
//! still recheck the live source revision before writing.

use std::time::{SystemTime, UNIX_EPOCH};

use wavecrate_core::{WaveformEditFade, WaveformEditSettings};
use wavecrate_library::operation_journal::{
    CopyEndpoint, FiniteF32Bits, ParticipantSet, WaveformEditFadeArgs, WaveformEditFrameRange,
    WaveformEditIntentArgs, WaveformEditOperationArgs, WaveformEditSettingsArgs,
    WaveformEditValidationError, WaveformFileRevisionObservation, WaveformOutputSpec,
    WaveformSampleFormat, WaveformSourceSpec,
};

use crate::{
    WavEditOperation, WavEditPreparationRequest, WaveformDetailContext, WaveformDetailRange,
    WaveformFileRevision, WaveformSpec,
};

/// Failure while adapting an in-memory media edit to or from a durable journal observation.
#[derive(Clone, Debug, PartialEq)]
pub enum WaveformEditIntentAdapterError {
    /// The preparation request or caller-supplied context failed pure shape validation.
    InvalidRequest(String),
    /// A journal observation failed its bounded source or operation contract.
    InvalidObservation(WaveformEditValidationError),
    /// The caller-supplied context no longer matches the durable file observation.
    StaleContext,
    /// A SystemTime value cannot be represented by the signed journal timestamp.
    TimestampOutOfRange,
    /// A media WAV specification cannot be represented by the journal source specification.
    UnsupportedSourceSpec,
    /// A stored frame count cannot be represented by the media request's usize range.
    FrameRangeOutOfRange,
}

impl From<WaveformEditValidationError> for WaveformEditIntentAdapterError {
    fn from(error: WaveformEditValidationError) -> Self {
        Self::InvalidObservation(error)
    }
}

impl std::fmt::Display for WaveformEditIntentAdapterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(reason) => {
                write!(formatter, "invalid waveform edit request: {reason}")
            }
            Self::InvalidObservation(reason) => {
                write!(formatter, "invalid waveform edit observation: {reason}")
            }
            Self::StaleContext => formatter
                .write_str("waveform edit context does not match the stored source observation"),
            Self::TimestampOutOfRange => formatter
                .write_str("waveform edit modified time is outside the signed nanosecond range"),
            Self::UnsupportedSourceSpec => {
                formatter.write_str("waveform edit source specification is unsupported")
            }
            Self::FrameRangeOutOfRange => {
                formatter.write_str("waveform edit frame range does not fit the media platform")
            }
        }
    }
}

impl std::error::Error for WaveformEditIntentAdapterError {}

/// Convert a validated media preparation request into a bounded V7 journal observation.
///
/// The supplied endpoint and participants are observations used for role checking. This function
/// does no filesystem work and intentionally omits [`WaveformDetailContext::path`] from the
/// returned value.
pub fn waveform_edit_intent(
    source: CopyEndpoint,
    participants: &ParticipantSet,
    request: &WavEditPreparationRequest,
) -> Result<WaveformEditIntentArgs, WaveformEditIntentAdapterError> {
    WavEditPreparationRequest::with_operation(
        request.context.clone(),
        request.range,
        request.operation,
    )
    .map_err(WaveformEditIntentAdapterError::InvalidRequest)?;

    let source_spec = source_spec(request.context.spec)?;
    let range = WaveformEditFrameRange::new(
        u64::try_from(request.range.start_frame)
            .map_err(|_| WaveformEditIntentAdapterError::FrameRangeOutOfRange)?,
        u64::try_from(request.range.end_frame)
            .map_err(|_| WaveformEditIntentAdapterError::FrameRangeOutOfRange)?,
    )?;
    let operation = operation_args(request.operation)?;
    let output = WaveformOutputSpec::float32(source_spec.channels(), source_spec.sample_rate());
    let args = WaveformEditIntentArgs::new(
        source,
        file_revision(&request.context.file_revision)?,
        source_spec,
        u64::try_from(request.context.total_frames)
            .map_err(|_| WaveformEditIntentAdapterError::FrameRangeOutOfRange)?,
        range,
        operation,
        output,
    )?;
    args.validate_against_participants(participants)?;
    Ok(args)
}

/// Reconstruct a pure media preparation request from a durable edit observation and a fresh
/// caller-supplied context observation.
///
/// Equality here only compares observations supplied by the caller; it does not prove current
/// ownership or file freshness. [`crate::prepare_wav_edit`] performs the live revision checks
/// required before any output is written.
pub fn waveform_edit_request(
    args: &WaveformEditIntentArgs,
    participants: &ParticipantSet,
    context: WaveformDetailContext,
) -> Result<WavEditPreparationRequest, WaveformEditIntentAdapterError> {
    args.validate_against_participants(participants)?;
    if file_revision(&context.file_revision)? != *args.source_file_revision()
        || source_spec(context.spec)? != args.source_spec()
        || u64::try_from(context.total_frames)
            .map_err(|_| WaveformEditIntentAdapterError::FrameRangeOutOfRange)?
            != args.total_frames()
    {
        return Err(WaveformEditIntentAdapterError::StaleContext);
    }
    let range = WaveformDetailRange {
        start_frame: usize::try_from(args.range().start_frame())
            .map_err(|_| WaveformEditIntentAdapterError::FrameRangeOutOfRange)?,
        end_frame: usize::try_from(args.range().end_frame())
            .map_err(|_| WaveformEditIntentAdapterError::FrameRangeOutOfRange)?,
    };
    let operation = core_operation(args.operation())?;
    WavEditPreparationRequest::with_operation(context, range, operation)
        .map_err(WaveformEditIntentAdapterError::InvalidRequest)
}

fn source_spec(spec: WaveformSpec) -> Result<WaveformSourceSpec, WaveformEditIntentAdapterError> {
    let format = match spec.sample_format {
        hound::SampleFormat::Int => WaveformSampleFormat::Integer,
        hound::SampleFormat::Float => WaveformSampleFormat::Float,
    };
    WaveformSourceSpec::new(
        spec.channels,
        spec.sample_rate,
        spec.bits_per_sample,
        format,
    )
    .map_err(WaveformEditIntentAdapterError::InvalidObservation)
}

fn file_revision(
    revision: &WaveformFileRevision,
) -> Result<WaveformFileRevisionObservation, WaveformEditIntentAdapterError> {
    WaveformFileRevisionObservation::new(
        revision.filesystem_identity.clone(),
        revision.change_marker.clone(),
        revision.byte_len,
        revision
            .modified_time
            .map(system_time_to_signed_unix_nanos)
            .transpose()?,
    )
    .map_err(WaveformEditIntentAdapterError::InvalidObservation)
}

fn system_time_to_signed_unix_nanos(
    time: SystemTime,
) -> Result<i64, WaveformEditIntentAdapterError> {
    let (negative, duration) = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (false, duration),
        Err(error) => (true, error.duration()),
    };
    let nanos = u128::from(duration.as_secs())
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(u128::from(duration.subsec_nanos())))
        .ok_or(WaveformEditIntentAdapterError::TimestampOutOfRange)?;
    if negative {
        let limit = i64::MAX as u128 + 1;
        if nanos > limit {
            return Err(WaveformEditIntentAdapterError::TimestampOutOfRange);
        }
        if nanos == limit {
            Ok(i64::MIN)
        } else {
            i64::try_from(nanos)
                .map(|value| -value)
                .map_err(|_| WaveformEditIntentAdapterError::TimestampOutOfRange)
        }
    } else {
        i64::try_from(nanos).map_err(|_| WaveformEditIntentAdapterError::TimestampOutOfRange)
    }
}

fn operation_args(
    operation: WavEditOperation,
) -> Result<WaveformEditOperationArgs, WaveformEditIntentAdapterError> {
    Ok(match operation {
        WavEditOperation::GainAndFades(settings) => {
            WaveformEditOperationArgs::GainAndFades(settings_args(settings)?)
        }
        WavEditOperation::Reverse => WaveformEditOperationArgs::Reverse,
        WavEditOperation::Mute => WaveformEditOperationArgs::Mute,
        WavEditOperation::Trim => WaveformEditOperationArgs::Trim,
        WavEditOperation::Crop => WaveformEditOperationArgs::Crop,
        WavEditOperation::Slide { frame_offset } => {
            WaveformEditOperationArgs::Slide { frame_offset }
        }
        WavEditOperation::CropWithSilence {
            start_frame,
            end_frame,
        } => WaveformEditOperationArgs::CropWithSilence {
            start_frame,
            end_frame,
        },
    })
}

fn settings_args(
    settings: WaveformEditSettings,
) -> Result<WaveformEditSettingsArgs, WaveformEditIntentAdapterError> {
    WaveformEditSettingsArgs::new(
        FiniteF32Bits::new(settings.gain())?,
        settings.fade_in().map(fade_args).transpose()?,
        settings.fade_out().map(fade_args).transpose()?,
    )
    .map_err(WaveformEditIntentAdapterError::InvalidObservation)
}

fn fade_args(
    fade: WaveformEditFade,
) -> Result<WaveformEditFadeArgs, WaveformEditIntentAdapterError> {
    WaveformEditFadeArgs::new(
        FiniteF32Bits::new(fade.length())?,
        FiniteF32Bits::new(fade.curve())?,
        FiniteF32Bits::new(fade.mute())?,
        FiniteF32Bits::new(fade.outer_gain())?,
    )
    .map_err(WaveformEditIntentAdapterError::InvalidObservation)
}

fn core_operation(
    operation: WaveformEditOperationArgs,
) -> Result<WavEditOperation, WaveformEditIntentAdapterError> {
    Ok(match operation {
        WaveformEditOperationArgs::GainAndFades(settings) => {
            WavEditOperation::GainAndFades(core_settings(settings)?)
        }
        WaveformEditOperationArgs::Reverse => WavEditOperation::Reverse,
        WaveformEditOperationArgs::Mute => WavEditOperation::Mute,
        WaveformEditOperationArgs::Trim => WavEditOperation::Trim,
        WaveformEditOperationArgs::Crop => WavEditOperation::Crop,
        WaveformEditOperationArgs::Slide { frame_offset } => {
            WavEditOperation::Slide { frame_offset }
        }
        WaveformEditOperationArgs::CropWithSilence {
            start_frame,
            end_frame,
        } => WavEditOperation::CropWithSilence {
            start_frame,
            end_frame,
        },
    })
}

fn core_settings(
    settings: WaveformEditSettingsArgs,
) -> Result<WaveformEditSettings, WaveformEditIntentAdapterError> {
    WaveformEditSettings::from_exact(
        settings.gain().value(),
        settings.fade_in().map(core_fade).transpose()?,
        settings.fade_out().map(core_fade).transpose()?,
    )
    .ok_or_else(|| {
        WaveformEditIntentAdapterError::InvalidRequest(String::from(
            "stored waveform edit settings are outside the core domain",
        ))
    })
}

fn core_fade(
    fade: WaveformEditFadeArgs,
) -> Result<WaveformEditFade, WaveformEditIntentAdapterError> {
    WaveformEditFade::from_exact(
        fade.length().value(),
        fade.curve().value(),
        fade.mute().value(),
        fade.outer_gain().value(),
    )
    .ok_or_else(|| {
        WaveformEditIntentAdapterError::InvalidRequest(String::from(
            "stored waveform edit fade is outside the core domain",
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use wavecrate_library::operation_journal::{
        ParticipantCheckpoint, ParticipantCheckpointState, ParticipantDescriptor, ParticipantId,
        ParticipantKind, ParticipantRequirement, StoredSourceFence,
    };
    use wavecrate_library::sample_sources::SourceId;

    fn fixture() -> (CopyEndpoint, ParticipantSet, WaveformDetailContext) {
        let source_id = SourceId::from_string("edit-source");
        let source_participant = ParticipantId::new();
        let filesystem_participant = ParticipantId::new();
        let participants = ParticipantSet::new(
            vec![
                ParticipantDescriptor::new(
                    source_participant,
                    ParticipantKind::SourceDatabase,
                    ParticipantRequirement::Required,
                    Some(
                        StoredSourceFence::new(
                            source_id.clone(),
                            "root-edit",
                            "database-edit",
                            1,
                            7,
                            1,
                        )
                        .expect("source fence"),
                    ),
                    None,
                )
                .expect("source participant"),
                ParticipantDescriptor::new(
                    filesystem_participant,
                    ParticipantKind::Filesystem,
                    ParticipantRequirement::Required,
                    None,
                    None,
                )
                .expect("filesystem participant"),
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
        let endpoint = CopyEndpoint::new(
            source_participant,
            source_id,
            wavecrate_library::operation_journal::StoredRelativePath::new("audio/source.wav")
                .expect("source path"),
        )
        .expect("endpoint");
        let context = WaveformDetailContext {
            path: PathBuf::from("/private/marker/should-not-be-persisted.wav"),
            file_revision: WaveformFileRevision {
                filesystem_identity: String::from("file-identity"),
                change_marker: String::from("change-marker"),
                byte_len: 1024,
                modified_time: Some(UNIX_EPOCH + Duration::from_secs(12)),
            },
            spec: WaveformSpec {
                channels: 2,
                sample_rate: 48_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
            total_frames: 10,
            global_mono_gain: 1.0,
            visual_band_gains: [1.0, 1.0, 1.0],
        };
        (endpoint, participants, context)
    }

    fn requests() -> [WavEditPreparationRequest; 7] {
        let (_, _, context) = fixture();
        let fade = WaveformEditFade::from_exact(-0.0, 0.25, -0.0, 0.75).expect("exact fade");
        let settings =
            WaveformEditSettings::from_exact(-0.0, Some(fade), Some(fade)).expect("exact settings");
        [
            WavEditPreparationRequest::with_operation(
                context.clone(),
                WaveformDetailRange {
                    start_frame: 2,
                    end_frame: 8,
                },
                WavEditOperation::GainAndFades(settings),
            )
            .expect("gain request"),
            WavEditPreparationRequest::with_operation(
                context.clone(),
                WaveformDetailRange {
                    start_frame: 2,
                    end_frame: 8,
                },
                WavEditOperation::Reverse,
            )
            .expect("reverse request"),
            WavEditPreparationRequest::with_operation(
                context.clone(),
                WaveformDetailRange {
                    start_frame: 2,
                    end_frame: 8,
                },
                WavEditOperation::Mute,
            )
            .expect("mute request"),
            WavEditPreparationRequest::with_operation(
                context.clone(),
                WaveformDetailRange {
                    start_frame: 2,
                    end_frame: 8,
                },
                WavEditOperation::Trim,
            )
            .expect("trim request"),
            WavEditPreparationRequest::with_operation(
                context.clone(),
                WaveformDetailRange {
                    start_frame: 2,
                    end_frame: 8,
                },
                WavEditOperation::Crop,
            )
            .expect("crop request"),
            WavEditPreparationRequest::with_slide(context.clone(), -3).expect("slide request"),
            WavEditPreparationRequest::with_padded_crop(context, -2, 12)
                .expect("padded crop request"),
        ]
    }

    #[test]
    fn all_seven_operations_round_trip_without_path_derivation() {
        let (endpoint, participants, context) = fixture();
        for request in requests() {
            let args = waveform_edit_intent(endpoint.clone(), &participants, &request)
                .expect("intent adapts");
            let displayed = format!("{args:?}");
            assert!(!displayed.contains("should-not-be-persisted"));
            assert!(!displayed.contains("private/marker"));
            let reconstructed = waveform_edit_request(&args, &participants, context.clone())
                .expect("request reconstructs");
            assert_eq!(reconstructed.range, request.range);
            assert_eq!(reconstructed.operation, request.operation);
        }
    }

    #[test]
    fn changed_context_revision_is_rejected_without_filesystem_access() {
        let (endpoint, participants, context) = fixture();
        let request = requests()[1].clone();
        let args = waveform_edit_intent(endpoint, &participants, &request).expect("intent adapts");
        let mut changed = context;
        changed.file_revision.change_marker = String::from("changed");
        assert_eq!(
            waveform_edit_request(&args, &participants, changed),
            Err(WaveformEditIntentAdapterError::StaleContext)
        );
    }

    #[test]
    fn signed_zero_settings_survive_exact_adapter_round_trip() {
        let (endpoint, participants, context) = fixture();
        let request = requests()[0].clone();
        let args = waveform_edit_intent(endpoint, &participants, &request).expect("intent adapts");
        let WaveformEditOperationArgs::GainAndFades(settings) = args.operation() else {
            panic!("expected settings operation");
        };
        assert_eq!(settings.gain().bits(), (-0.0_f32).to_bits());
        let fade = settings.fade_in().expect("fade");
        assert_eq!(fade.length().bits(), (-0.0_f32).to_bits());
        let reconstructed =
            waveform_edit_request(&args, &participants, context).expect("request reconstructs");
        let WavEditOperation::GainAndFades(settings) = reconstructed.operation else {
            panic!("expected settings operation");
        };
        assert_eq!(settings.gain().to_bits(), (-0.0_f32).to_bits());
        assert_eq!(
            settings.fade_in().expect("fade").length().to_bits(),
            (-0.0_f32).to_bits()
        );
    }

    #[test]
    fn signed_timestamp_conversion_preserves_epoch_edges() {
        assert_eq!(
            system_time_to_signed_unix_nanos(UNIX_EPOCH).expect("epoch"),
            0
        );
        assert_eq!(
            system_time_to_signed_unix_nanos(
                UNIX_EPOCH
                    .checked_sub(Duration::from_nanos(1))
                    .expect("pre-epoch")
            )
            .expect("pre-epoch nanos"),
            -1
        );
        let positive_limit = Duration::from_nanos(i64::MAX as u64);
        let negative_limit = Duration::from_nanos(i64::MAX as u64 + 1);
        assert_eq!(
            system_time_to_signed_unix_nanos(
                UNIX_EPOCH
                    .checked_add(positive_limit)
                    .expect("positive limit")
            ),
            Ok(i64::MAX)
        );
        assert_eq!(
            system_time_to_signed_unix_nanos(
                UNIX_EPOCH
                    .checked_sub(negative_limit)
                    .expect("negative limit")
            ),
            Ok(i64::MIN)
        );
        for time in [
            UNIX_EPOCH
                .checked_add(negative_limit)
                .expect("above positive limit"),
            UNIX_EPOCH
                .checked_sub(Duration::from_nanos(i64::MAX as u64 + 2))
                .expect("below negative limit"),
        ] {
            assert_eq!(
                system_time_to_signed_unix_nanos(time),
                Err(WaveformEditIntentAdapterError::TimestampOutOfRange)
            );
        }
    }
}
