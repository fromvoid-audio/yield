//! Bounded, file-fenced zero-crossing selection snapping for WAV sources.

use std::{fs::File, io::BufReader, path::Path, sync::atomic::AtomicBool};

use crate::{WaveformDetailContext, detail};

const SNAP_WINDOW_SECONDS: f32 = 0.005;
const ZERO_EPSILON: f32 = 1.0e-6;
const ZERO_CROSSING_READ_BLOCK_SAMPLES: usize = 4_096;

/// Normalized half-open bounds after zero-crossing snapping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WavZeroCrossingSnapRange {
    /// Start boundary as a precise normalized frame position.
    pub start_ratio: f64,
    /// Exclusive end boundary as a precise normalized frame position.
    pub end_ratio: f64,
}

/// A file-fenced request to snap normalized WAV selection edges to zero crossings.
///
/// The ratios are retained as caller-provided `f64` values so invalid, out-of-range, or reversed
/// selections can follow the native no-op contract. A valid edge is searched in an inclusive
/// five-millisecond frame window around `round(ratio * total_frames)`. Edge decoding retains only
/// those two small windows; a platform content-fingerprint fence may additionally hash the
/// retained source once at admission using bounded memory.
#[derive(Clone, Debug, PartialEq)]
pub struct WavZeroCrossingSnapRequest {
    /// Revision/spec context used to fence the source read, including a platform content marker
    /// when the source context provides one.
    pub context: WaveformDetailContext,
    /// Normalized inclusive start selection edge.
    pub start_ratio: f64,
    /// Normalized exclusive end selection edge.
    pub end_ratio: f64,
}

impl WavZeroCrossingSnapRequest {
    /// Create a request from normalized half-open selection bounds.
    ///
    /// Ratios outside `0.0..=1.0` and non-finite ratios are retained and leave that edge unchanged
    /// when the request is evaluated, matching the native behavior. Reversed bounds are retained and
    /// the original pair is restored if snapping would leave the bounds reversed.
    pub fn new(
        context: WaveformDetailContext,
        start_ratio: f64,
        end_ratio: f64,
    ) -> Result<Self, String> {
        let request = Self {
            context,
            start_ratio,
            end_ratio,
        };
        request.validate_context()?;
        Ok(request)
    }

    /// Alias for [`Self::new`] that names the normalized coordinate system explicitly.
    pub fn from_normalized(
        context: WaveformDetailContext,
        start_ratio: f64,
        end_ratio: f64,
    ) -> Result<Self, String> {
        Self::new(context, start_ratio, end_ratio)
    }

    /// Create a request for an exact, non-empty half-open frame range.
    pub fn from_frames(
        context: WaveformDetailContext,
        start_frame: usize,
        end_frame: usize,
    ) -> Result<Self, String> {
        if !context.is_valid() {
            return Err(String::from(
                "WAV zero-crossing snap context is unavailable",
            ));
        }
        if start_frame >= end_frame {
            return Err(String::from(
                "WAV zero-crossing snap frame range must be non-empty",
            ));
        }
        if end_frame > context.total_frames {
            return Err(String::from(
                "WAV zero-crossing snap frame range exceeds source frames",
            ));
        }
        if context.total_frames == 0 {
            return Err(String::from(
                "WAV zero-crossing snap context has no source frames",
            ));
        }
        let total_frames = context.total_frames as f64;
        Self::new(
            context,
            start_frame as f64 / total_frames,
            end_frame as f64 / total_frames,
        )
    }

    fn validate_context(&self) -> Result<(), String> {
        if !self.context.is_valid() {
            return Err(String::from(
                "WAV zero-crossing snap context is unavailable",
            ));
        }
        Ok(())
    }
}

/// Snap both edges of a WAV selection to nearby zero-crossing boundaries.
pub fn snap_wav_range_to_zero_crossings(
    request: &WavZeroCrossingSnapRequest,
) -> Result<WavZeroCrossingSnapRange, String> {
    snap_wav_range_to_zero_crossings_with_cancel(request, None)
}

/// Snap both edges of a WAV selection while honoring an optional cancellation flag.
///
/// The source is opened and retained for the whole operation. The request's revision, WAV
/// header, and retained source identity are checked before the bounded edge reads and after them.
/// On platforms with a content-fingerprint fence, admission hashes the retained source once with
/// fixed memory. Any source change or read failure returns an error; a caller should discard the
/// result and retry against a fresh context. Float samples use the playback conversion's
/// `clamp(-1.0, 1.0)`; non-finite sample bits therefore remain non-finite and participate in the
/// native `f32` `abs`/sign ordering, while malformed or truncated sample data is reported as an
/// error.
pub fn snap_wav_range_to_zero_crossings_with_cancel(
    request: &WavZeroCrossingSnapRequest,
    cancellation: Option<&AtomicBool>,
) -> Result<WavZeroCrossingSnapRange, String> {
    request.validate_context()?;
    if crate::is_cancelled(cancellation) {
        return Err(String::from("WAV zero-crossing snap canceled"));
    }

    let original = WavZeroCrossingSnapRange {
        start_ratio: request.start_ratio,
        end_ratio: request.end_ratio,
    };
    let start_is_valid = valid_ratio(request.start_ratio);
    let end_is_valid = valid_ratio(request.end_ratio);
    if !start_is_valid && !end_is_valid {
        return Ok(original);
    }

    let context = &request.context;
    let mut opened = detail::open_wav(&context.path, cancellation)
        .map_err(|error| zero_crossing_error(&context.path, error, cancellation))?;
    validate_opened_context(context, &opened)?;
    if crate::is_cancelled(cancellation) {
        return Err(String::from("WAV zero-crossing snap canceled"));
    }

    let start_boundary = start_is_valid
        .then(|| {
            scan_edge(
                &mut opened.reader,
                opened.spec,
                opened.total_frames,
                request.start_ratio,
                cancellation,
            )
        })
        .transpose()?
        .flatten();
    let end_boundary = end_is_valid
        .then(|| {
            scan_edge(
                &mut opened.reader,
                opened.spec,
                opened.total_frames,
                request.end_ratio,
                cancellation,
            )
        })
        .transpose()?
        .flatten();
    if crate::is_cancelled(cancellation) {
        return Err(String::from("WAV zero-crossing snap canceled"));
    }

    let after = detail::recheck_wav(&context.path, &opened, cancellation)
        .map_err(|error| stale_result_error(error, cancellation))?;
    detail::ensure_file_unchanged(&opened.observation, &after)
        .map_err(|error| stale_result_error(error, cancellation))?;
    if after.strong_revision().as_ref() != Some(&context.file_revision) {
        return Err(String::from("stale WAV zero-crossing snap result"));
    }
    if crate::is_cancelled(cancellation) {
        return Err(String::from("WAV zero-crossing snap canceled"));
    }

    let start_ratio = start_boundary
        .map(|boundary| boundary as f64 / opened.total_frames as f64)
        .unwrap_or(request.start_ratio);
    let end_ratio = end_boundary
        .map(|boundary| boundary as f64 / opened.total_frames as f64)
        .unwrap_or(request.end_ratio);
    if start_ratio >= end_ratio {
        Ok(original)
    } else {
        Ok(WavZeroCrossingSnapRange {
            start_ratio,
            end_ratio,
        })
    }
}

fn valid_ratio(ratio: f64) -> bool {
    ratio.is_finite() && (0.0..=1.0).contains(&ratio)
}

fn validate_opened_context(
    context: &WaveformDetailContext,
    opened: &detail::OpenWav,
) -> Result<(), String> {
    if detail::WaveformSpec::from_hound(opened.spec) != context.spec
        || opened.total_frames != context.total_frames
    {
        return Err(String::from("stale WAV zero-crossing snap request"));
    }
    if opened.observation.strong_revision().as_ref() != Some(&context.file_revision) {
        return Err(String::from("stale WAV zero-crossing snap request"));
    }
    validate_playback_spec(context.spec)
}

fn validate_playback_spec(spec: detail::WaveformSpec) -> Result<(), String> {
    match spec.sample_format {
        hound::SampleFormat::Float if spec.bits_per_sample == 32 => Ok(()),
        hound::SampleFormat::Int if (1..=32).contains(&spec.bits_per_sample) => Ok(()),
        hound::SampleFormat::Float => Err(format!(
            "unsupported WAV float bit depth for zero-crossing snap: {}",
            spec.bits_per_sample
        )),
        hound::SampleFormat::Int => Err(format!(
            "unsupported WAV integer bit depth for zero-crossing snap: {}",
            spec.bits_per_sample
        )),
    }
}

fn zero_crossing_error(path: &Path, error: String, cancellation: Option<&AtomicBool>) -> String {
    if crate::is_cancelled(cancellation) {
        return String::from("WAV zero-crossing snap canceled");
    }
    format!(
        "WAV zero-crossing snap failed for {}: {error}",
        path.display()
    )
}

fn stale_result_error(error: String, cancellation: Option<&AtomicBool>) -> String {
    if crate::is_cancelled(cancellation) {
        return String::from("WAV zero-crossing snap canceled");
    }
    format!("stale WAV zero-crossing snap result: {error}")
}

fn scan_edge(
    reader: &mut hound::WavReader<BufReader<File>>,
    spec: hound::WavSpec,
    total_frames: usize,
    ratio: f64,
    cancellation: Option<&AtomicBool>,
) -> Result<Option<usize>, String> {
    if crate::is_cancelled(cancellation) {
        return Err(String::from("WAV zero-crossing snap canceled"));
    }
    if total_frames < 2 {
        return Ok(None);
    }
    let target = target_boundary_for_ratio(ratio, total_frames);
    let radius = snap_window_frames(spec.sample_rate);
    let candidate_start = target.saturating_sub(radius);
    let candidate_end = target.saturating_add(radius).min(total_frames);
    let read_start = candidate_start.saturating_sub(1);
    let read_end = candidate_end.saturating_add(1).min(total_frames);
    if read_start >= read_end {
        return Ok(None);
    }
    reader
        .seek(u32::try_from(read_start).map_err(|_| {
            String::from("WAV zero-crossing snap window cannot be represented by WAV reader")
        })?)
        .map_err(|error| format!("failed to seek zero-crossing WAV source: {error}"))?;
    if crate::is_cancelled(cancellation) {
        return Err(String::from("WAV zero-crossing snap canceled"));
    }

    match spec.sample_format {
        hound::SampleFormat::Float => {
            let mut samples = reader.samples::<f32>();
            scan_sample_window(
                &mut samples,
                usize::from(spec.channels),
                read_start,
                read_end,
                candidate_start,
                candidate_end,
                target,
                cancellation,
                "float",
                |sample| sample.clamp(-1.0, 1.0),
            )
        }
        hound::SampleFormat::Int if spec.bits_per_sample <= 16 => {
            let scale = integer_sample_max_i32(spec.bits_per_sample);
            let mut samples = reader.samples::<i16>();
            scan_sample_window(
                &mut samples,
                usize::from(spec.channels),
                read_start,
                read_end,
                candidate_start,
                candidate_end,
                target,
                cancellation,
                "integer",
                move |sample| (f32::from(sample) / scale).clamp(-1.0, 1.0),
            )
        }
        hound::SampleFormat::Int => {
            let scale = integer_sample_max_i64(spec.bits_per_sample);
            let mut samples = reader.samples::<i32>();
            scan_sample_window(
                &mut samples,
                usize::from(spec.channels),
                read_start,
                read_end,
                candidate_start,
                candidate_end,
                target,
                cancellation,
                "integer",
                move |sample| ((sample as f32) / scale).clamp(-1.0, 1.0),
            )
        }
    }
}

fn target_boundary_for_ratio(ratio: f64, total_frames: usize) -> usize {
    (ratio.clamp(0.0, 1.0) * total_frames as f64).round() as usize
}

fn snap_window_frames(sample_rate: u32) -> usize {
    ((sample_rate as f32 * SNAP_WINDOW_SECONDS).ceil() as usize).max(1)
}

#[derive(Clone, Copy, Debug)]
struct SnapCandidate {
    boundary: usize,
    distance: usize,
    boundary_level: f32,
}

impl SnapCandidate {
    fn new(boundary: usize, target: usize, boundary_level: f32) -> Self {
        Self {
            boundary,
            distance: boundary.abs_diff(target),
            boundary_level,
        }
    }

    fn cmp(left: &Self, right: &Self) -> std::cmp::Ordering {
        left.distance
            .cmp(&right.distance)
            .then_with(|| left.boundary_level.total_cmp(&right.boundary_level))
            .then_with(|| left.boundary.cmp(&right.boundary))
    }
}

fn scan_sample_window<T, I, D>(
    samples: &mut I,
    channels: usize,
    read_start: usize,
    read_end: usize,
    candidate_start: usize,
    candidate_end: usize,
    target: usize,
    cancellation: Option<&AtomicBool>,
    sample_kind: &str,
    mut decode: D,
) -> Result<Option<usize>, String>
where
    I: Iterator<Item = Result<T, hound::Error>>,
    D: FnMut(T) -> f32,
{
    if channels == 0 {
        return Err(String::from(
            "WAV zero-crossing snap source has no channels",
        ));
    }
    let mut samples_since_cancel_check = 0usize;
    let mut first = None;
    let mut previous = None;
    let mut best = None;
    for frame in read_start..read_end {
        if crate::is_cancelled(cancellation) {
            return Err(String::from("WAV zero-crossing snap canceled"));
        }
        let mono = read_mono_frame(
            samples,
            channels,
            &mut samples_since_cancel_check,
            cancellation,
            sample_kind,
            &mut decode,
        )?;
        if first.is_none() {
            first = Some(mono);
        }
        if frame > read_start {
            let boundary = frame;
            if (candidate_start..=candidate_end).contains(&boundary) {
                if let Some(previous) = previous {
                    consider_candidate(
                        &mut best,
                        boundary,
                        target,
                        zero_crossing_boundary_level(previous, mono),
                    );
                }
            }
        }
        previous = Some(mono);
    }
    if candidate_start == 0 {
        if let Some(first) = first {
            consider_candidate(
                &mut best,
                0,
                target,
                near_zero(first).then_some(first.abs()),
            );
        }
    }
    if candidate_end >= read_end && candidate_end >= 1 {
        if let Some(last) = previous {
            consider_candidate(
                &mut best,
                candidate_end,
                target,
                near_zero(last).then_some(last.abs()),
            );
        }
    }
    if crate::is_cancelled(cancellation) {
        return Err(String::from("WAV zero-crossing snap canceled"));
    }
    Ok(best.map(|candidate| candidate.boundary))
}

fn read_mono_frame<T, I, D>(
    samples: &mut I,
    channels: usize,
    samples_since_cancel_check: &mut usize,
    cancellation: Option<&AtomicBool>,
    sample_kind: &str,
    decode: &mut D,
) -> Result<f32, String>
where
    I: Iterator<Item = Result<T, hound::Error>>,
    D: FnMut(T) -> f32,
{
    let mut mono = 0.0_f32;
    for _ in 0..channels {
        let sample = samples
            .next()
            .transpose()
            .map_err(|error| format!("failed to read {sample_kind} WAV sample: {error}"))?
            .ok_or_else(|| String::from("WAV ended before zero-crossing window"))?;
        mono += decode(sample);
        *samples_since_cancel_check += 1;
        if *samples_since_cancel_check == ZERO_CROSSING_READ_BLOCK_SAMPLES {
            *samples_since_cancel_check = 0;
            if crate::is_cancelled(cancellation) {
                return Err(String::from("WAV zero-crossing snap canceled"));
            }
        }
    }
    Ok(mono / channels as f32)
}

fn consider_candidate(
    best: &mut Option<SnapCandidate>,
    boundary: usize,
    target: usize,
    boundary_level: Option<f32>,
) {
    let Some(boundary_level) = boundary_level else {
        return;
    };
    let candidate = SnapCandidate::new(boundary, target, boundary_level);
    if best
        .as_ref()
        .is_none_or(|current| SnapCandidate::cmp(&candidate, current).is_lt())
    {
        *best = Some(candidate);
    }
}

fn zero_crossing_boundary_level(previous: f32, next: f32) -> Option<f32> {
    crosses_zero(previous, next).then_some(previous.abs().min(next.abs()))
}

fn crosses_zero(left: f32, right: f32) -> bool {
    near_zero(left) || near_zero(right) || left.is_sign_positive() != right.is_sign_positive()
}

fn near_zero(value: f32) -> bool {
    value.abs() <= ZERO_EPSILON
}

fn integer_sample_max_i32(bits_per_sample: u16) -> f32 {
    ((1_i32 << u32::from(bits_per_sample).saturating_sub(1)) - 1).max(1) as f32
}

fn integer_sample_max_i64(bits_per_sample: u16) -> f32 {
    ((1_i64 << u32::from(bits_per_sample).saturating_sub(1)) - 1).max(1) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::{
        fs,
        path::Path,
        sync::atomic::{AtomicBool, Ordering},
    };
    use tempfile::TempDir;

    fn float_spec(channels: u16, sample_rate: u32) -> WavSpec {
        WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        }
    }

    fn write_float_frames(path: &Path, channels: u16, sample_rate: u32, frames: &[[f32; 2]]) {
        let spec = float_spec(channels, sample_rate);
        let mut writer = WavWriter::create(path, spec).expect("create WAV fixture");
        for frame in frames {
            for sample in frame.iter().take(usize::from(channels)) {
                writer.write_sample(*sample).expect("write WAV sample");
            }
        }
        writer.finalize().expect("finish WAV fixture");
    }

    fn context_for(path: &Path) -> WaveformDetailContext {
        crate::load_waveform_preview(path)
            .expect("WAV overview")
            .detail_context
            .expect("strong WAV context")
    }

    #[test]
    fn candidate_tie_prefers_lower_level_then_lower_boundary() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("candidate-order.wav");
        let mut frames = vec![[1.0, 0.0]; 120];
        frames[58][0] = -0.25;
        frames[59][0] = -0.5;
        frames[60][0] = -0.5;
        frames[61][0] = -0.5;
        frames[62][0] = 0.1;
        write_float_frames(&path, 1, 1_000, &frames);
        let context = context_for(&path);

        let request = WavZeroCrossingSnapRequest::new(context.clone(), 0.5, 0.9)
            .expect("zero-crossing request");
        let snapped = snap_wav_range_to_zero_crossings(&request).expect("snap range");
        assert_eq!(snapped.start_ratio, 62.0 / 120.0);
        assert_eq!(snapped.end_ratio, 0.9);

        frames[62][0] = 0.25;
        write_float_frames(&path, 1, 1_000, &frames);
        let refreshed = context_for(&path);
        let request = WavZeroCrossingSnapRequest::new(refreshed, 0.5, 0.9).expect("tie request");
        let snapped = snap_wav_range_to_zero_crossings(&request).expect("tie snap range");
        assert_eq!(snapped.start_ratio, 58.0 / 120.0);
    }

    #[test]
    fn native_target_and_window_arithmetic_is_preserved() {
        assert_eq!(target_boundary_for_ratio(0.5, 101), 51);
        assert_eq!(snap_window_frames(44_100), 221);
        assert_eq!(snap_window_frames(48_000), 240);
        assert_eq!(snap_window_frames(1), 1);
    }

    #[test]
    fn five_millisecond_window_includes_its_clipped_boundary() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("window-edge.wav");
        let mut frames = vec![[0.25, 0.0]; 40];
        for frame in &mut frames[15..] {
            frame[0] = -0.25;
        }
        write_float_frames(&path, 1, 1_000, &frames);
        let request = WavZeroCrossingSnapRequest::new(context_for(&path), 0.5, 0.9)
            .expect("window-edge request");
        let snapped = snap_wav_range_to_zero_crossings(&request).expect("window-edge snap");
        assert_eq!(snapped.start_ratio, 15.0 / 40.0);
    }

    #[test]
    fn antiphase_stereo_candidate_uses_raw_channel_mean() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("stereo-mean.wav");
        let mut frames = vec![[1.0, 1.0]; 20];
        frames[9] = [1.0, -0.5];
        frames[10] = [0.5, -1.0];
        write_float_frames(&path, 2, 1_000, &frames);
        let request =
            WavZeroCrossingSnapRequest::new(context_for(&path), 0.5, 0.9).expect("stereo request");
        let snapped = snap_wav_range_to_zero_crossings(&request).expect("stereo snap range");
        assert_eq!(snapped.start_ratio, 10.0 / 20.0);
    }

    #[test]
    fn endpoint_near_zero_samples_snap_to_zero_and_one() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("endpoints.wav");
        let mut frames = vec![[0.25, 0.0]; 12];
        frames[0][0] = 0.0;
        frames[11][0] = 0.0;
        write_float_frames(&path, 1, 1_000, &frames);
        let request = WavZeroCrossingSnapRequest::new(context_for(&path), 0.01, 0.99)
            .expect("endpoint request");
        let snapped = snap_wav_range_to_zero_crossings(&request).expect("endpoint snap range");
        assert_eq!(snapped.start_ratio, 0.0);
        assert_eq!(snapped.end_ratio, 1.0);
    }

    #[test]
    fn invalid_ratios_are_no_ops_without_opening_source() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("invalid.wav");
        write_float_frames(&path, 1, 1_000, &[[0.25, 0.0]; 12]);
        let context = context_for(&path);
        fs::remove_file(&path).expect("remove source after context");

        for (start_ratio, end_ratio) in [(f64::NAN, f64::INFINITY), (-0.1, 1.1)] {
            let request = WavZeroCrossingSnapRequest::new(context.clone(), start_ratio, end_ratio)
                .expect("invalid-ratio request");
            let snapped = snap_wav_range_to_zero_crossings(&request).expect("invalid no-op");
            assert_eq!(snapped.end_ratio, end_ratio);
            if start_ratio.is_nan() {
                assert!(snapped.start_ratio.is_nan());
            } else {
                assert_eq!(snapped.start_ratio, start_ratio);
            }
        }
    }

    #[test]
    fn one_invalid_edge_leaves_that_edge_unchanged_and_snaps_the_other() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("one-invalid.wav");
        let mut frames = vec![[0.25, 0.0]; 12];
        frames[0][0] = 0.0;
        frames[11][0] = 0.0;
        write_float_frames(&path, 1, 1_000, &frames);
        let context = context_for(&path);

        let request = WavZeroCrossingSnapRequest::new(context.clone(), f64::NAN, 0.01)
            .expect("invalid start request");
        let snapped = snap_wav_range_to_zero_crossings(&request).expect("invalid start snap");
        assert!(snapped.start_ratio.is_nan());
        assert_eq!(snapped.end_ratio, 0.0);

        let request = WavZeroCrossingSnapRequest::new(context, 0.99, f64::INFINITY)
            .expect("invalid end request");
        let snapped = snap_wav_range_to_zero_crossings(&request).expect("invalid end snap");
        assert_eq!(snapped.start_ratio, 1.0);
        assert!(snapped.end_ratio.is_infinite());
    }

    #[test]
    fn reversed_valid_range_preserves_original_after_source_fence() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("reversed.wav");
        write_float_frames(&path, 1, 1_000, &[[0.25, 0.0]; 12]);
        let request = WavZeroCrossingSnapRequest::new(context_for(&path), 0.75, 0.25)
            .expect("reversed request");
        let snapped = snap_wav_range_to_zero_crossings(&request).expect("reversed snap");
        assert_eq!(
            snapped,
            WavZeroCrossingSnapRange {
                start_ratio: 0.75,
                end_ratio: 0.25,
            }
        );
    }

    #[test]
    fn stale_context_is_rejected_before_returning_a_snap() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("stale.wav");
        write_float_frames(&path, 1, 1_000, &[[0.25, 0.0]; 12]);
        let request =
            WavZeroCrossingSnapRequest::new(context_for(&path), 0.25, 0.75).expect("stale request");
        let replacement = directory.path().join("replacement.wav");
        write_float_frames(&replacement, 1, 1_000, &[[0.5, 0.0]; 12]);
        fs::rename(replacement, &path).expect("replace WAV source");
        let error = snap_wav_range_to_zero_crossings(&request).expect_err("stale source");
        assert!(
            error.contains("stale") || error.contains("replaced"),
            "{error}"
        );
    }

    #[test]
    fn cancellation_is_checked_before_and_within_interleaved_window_reads() {
        let canceled = AtomicBool::new(true);
        let mut samples = std::iter::repeat_with(|| Ok::<f32, hound::Error>(0.25));
        let error = scan_sample_window(
            &mut samples,
            1,
            0,
            2,
            0,
            1,
            0,
            Some(&canceled),
            "float",
            |sample| sample,
        )
        .expect_err("pre-canceled window");
        assert_eq!(error, "WAV zero-crossing snap canceled");

        let canceled = AtomicBool::new(false);
        let mut samples = std::iter::repeat_with(|| Ok::<f32, hound::Error>(0.25));
        let mut decoded = 0usize;
        let error = scan_sample_window(
            &mut samples,
            u16::MAX as usize,
            0,
            2,
            0,
            1,
            0,
            Some(&canceled),
            "float",
            |sample| {
                decoded += 1;
                if decoded == 3 {
                    canceled.store(true, Ordering::Release);
                }
                sample
            },
        )
        .expect_err("canceled interleaved window");
        assert_eq!(error, "WAV zero-crossing snap canceled");
        assert_eq!(decoded, ZERO_CROSSING_READ_BLOCK_SAMPLES);
    }

    #[test]
    fn frame_constructor_preserves_exact_frame_ratios() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("frames.wav");
        write_float_frames(&path, 1, 1_000, &[[0.25, 0.0]; 120]);
        let context = context_for(&path);
        let request =
            WavZeroCrossingSnapRequest::from_frames(context, 7, 113).expect("frame request");
        assert_eq!(request.start_ratio, 7.0 / 120.0);
        assert_eq!(request.end_ratio, 113.0 / 120.0);
    }
}
