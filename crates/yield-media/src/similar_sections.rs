//! Bounded, file-fenced Similar Sections matching for WAV sources.

use std::{fs::File, io::BufReader, path::Path, sync::atomic::AtomicBool};

use crate::{WaveformDetailContext, WaveformDetailRange, detail};

const PROFILE_BINS: usize = 192;
const MAX_SCAN_WINDOWS: usize = 8_000;
const MIN_ANCHOR_FRAMES: usize = 64;
const SIMILAR_SECTION_THRESHOLD: f32 = 0.86;
const MIN_PROFILE_RMS: f32 = 1.0e-4;

/// A file-fenced Similar Sections request.
///
/// The context is captured by a waveform overview and carries the source path, WAV
/// specification, frame count, and strong filesystem revision. The anchor is an exact,
/// half-open frame range in that context.
#[derive(Clone, Debug, PartialEq)]
pub struct WavSimilarSectionsRequest {
    /// Waveform context used to fence the source read.
    pub context: WaveformDetailContext,
    /// Non-empty playmark range used as the similarity anchor.
    pub anchor: WaveformDetailRange,
}

impl WavSimilarSectionsRequest {
    /// Validate and create a Similar Sections request for an exact frame range.
    pub fn new(
        context: WaveformDetailContext,
        anchor: WaveformDetailRange,
    ) -> Result<Self, String> {
        let request = Self { context, anchor };
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<(), String> {
        if !self.context.is_valid() {
            return Err(String::from("WAV similar sections context is unavailable"));
        }
        if self.anchor.start_frame >= self.anchor.end_frame {
            return Err(String::from(
                "WAV similar sections anchor must be non-empty",
            ));
        }
        if self.anchor.end_frame > self.context.total_frames {
            return Err(String::from(
                "WAV similar sections anchor exceeds source frames",
            ));
        }
        Ok(())
    }
}

/// Find sections whose bounded waveform profiles resemble the request anchor.
pub fn scan_wav_similar_sections(
    request: &WavSimilarSectionsRequest,
) -> Result<Vec<WaveformDetailRange>, String> {
    scan_wav_similar_sections_with_cancel(request, None)
}

/// Find similar sections while honoring an optional caller-owned cancellation flag.
///
/// The source is opened and retained for the complete operation. The request's WAV header,
/// strong filesystem revision, and frame range are checked before scanning and after all profile
/// reads. Profile reads seek directly to each small bin neighborhood, retaining only two fixed
/// 192-value profiles and the bounded candidate list in memory.
pub fn scan_wav_similar_sections_with_cancel(
    request: &WavSimilarSectionsRequest,
    cancellation: Option<&AtomicBool>,
) -> Result<Vec<WaveformDetailRange>, String> {
    request.validate()?;
    check_cancelled(cancellation)?;

    let context = &request.context;
    let mut opened = detail::open_wav(&context.path, cancellation)
        .map_err(|error| scan_error(&context.path, error, cancellation))?;
    validate_opened_context(context, &opened, cancellation)?;

    let window_frames = request.anchor.len();
    if window_frames < MIN_ANCHOR_FRAMES {
        return Err(String::from("Select a slightly longer playmark section"));
    }

    let Some(anchor_profile) = build_profile(
        &mut opened.reader,
        opened.spec,
        opened.total_frames,
        request.anchor.start_frame,
        window_frames,
        cancellation,
    )?
    else {
        return Err(String::from("Selected playmark is too quiet to compare"));
    };

    let hop = scan_hop_frames(opened.total_frames, window_frames, opened.spec.sample_rate)?;
    let candidates = collect_candidate_matches(
        &mut opened.reader,
        opened.spec,
        opened.total_frames,
        &anchor_profile,
        request.anchor.start_frame,
        request.anchor.end_frame,
        window_frames,
        hop,
        cancellation,
    )?;
    let ranges = group_candidate_matches(candidates, opened.total_frames, window_frames, hop);
    finish_scan(context, &opened, ranges, cancellation)
}

fn finish_scan(
    context: &WaveformDetailContext,
    opened: &detail::OpenWav,
    ranges: Vec<WaveformDetailRange>,
    cancellation: Option<&AtomicBool>,
) -> Result<Vec<WaveformDetailRange>, String> {
    check_cancelled(cancellation)?;
    let after = detail::recheck_wav(&context.path, opened, cancellation)
        .map_err(|error| scan_error(&context.path, error, cancellation))?;
    detail::ensure_file_unchanged(&opened.observation, &after)
        .map_err(|_| String::from("stale WAV similar sections result"))?;
    if after.strong_revision().as_ref() != Some(&context.file_revision) {
        return Err(String::from("stale WAV similar sections result"));
    }
    check_cancelled(cancellation)?;
    Ok(ranges)
}

fn validate_opened_context(
    context: &WaveformDetailContext,
    opened: &detail::OpenWav,
    cancellation: Option<&AtomicBool>,
) -> Result<(), String> {
    check_cancelled(cancellation)?;
    if detail::WaveformSpec::from_hound(opened.spec) != context.spec
        || opened.total_frames != context.total_frames
    {
        return Err(String::from("stale WAV similar sections request"));
    }
    if opened.observation.strong_revision().as_ref() != Some(&context.file_revision) {
        return Err(String::from("stale WAV similar sections request"));
    }
    validate_playback_spec(opened.spec)
}

fn validate_playback_spec(spec: hound::WavSpec) -> Result<(), String> {
    if spec.channels == 0 || spec.sample_rate == 0 {
        return Err(String::from(
            "WAV similar sections source has invalid audio metadata",
        ));
    }
    match spec.sample_format {
        hound::SampleFormat::Float if spec.bits_per_sample == 32 => Ok(()),
        hound::SampleFormat::Int if (1..=32).contains(&spec.bits_per_sample) => Ok(()),
        hound::SampleFormat::Float => Err(format!(
            "unsupported WAV float bit depth for Similar Sections: {}",
            spec.bits_per_sample
        )),
        hound::SampleFormat::Int => Err(format!(
            "unsupported WAV integer bit depth for Similar Sections: {}",
            spec.bits_per_sample
        )),
    }
}

fn scan_hop_frames(frames: usize, window_frames: usize, sample_rate: u32) -> Result<usize, String> {
    if frames == 0 || window_frames == 0 || window_frames > frames || sample_rate == 0 {
        return Err(String::from(
            "WAV similar sections scan dimensions are invalid",
        ));
    }
    let relative_hop = (window_frames / 128).max(1);
    let time_hop = (sample_rate as usize / 250).max(1);
    let mut hop = relative_hop.min(time_hop).max(1);
    let scan_span = frames - window_frames;

    // Starts are 0, hop, ... <= scan_span. Choosing ceil((scan_span + 1) / limit)
    // as the minimum hop gives a strict MAX_SCAN_WINDOWS bound without overflowing on
    // platforms where usize is only 32 bits.
    if scan_span / hop >= MAX_SCAN_WINDOWS {
        let minimum_hop = scan_span / MAX_SCAN_WINDOWS + 1;
        hop = hop.max(minimum_hop);
    }
    Ok(hop)
}

#[derive(Clone, Debug)]
struct SimilarityProfile {
    signed: Vec<f32>,
    energy: Vec<f32>,
    rms: f32,
}

#[derive(Clone, Copy, Debug)]
struct CandidateMatch {
    start_frame: usize,
    score: f32,
}

fn collect_candidate_matches(
    reader: &mut hound::WavReader<BufReader<File>>,
    spec: hound::WavSpec,
    frames: usize,
    anchor_profile: &SimilarityProfile,
    anchor_start: usize,
    anchor_end: usize,
    window_frames: usize,
    hop: usize,
    cancellation: Option<&AtomicBool>,
) -> Result<Vec<CandidateMatch>, String> {
    let max_start = frames
        .checked_sub(window_frames)
        .ok_or_else(|| String::from("WAV similar sections window exceeds source frames"))?;
    let mut candidates = Vec::new();
    let mut start_frame = 0usize;
    let mut candidate_count = 0usize;
    loop {
        check_cancelled(cancellation)?;
        candidate_count = candidate_count
            .checked_add(1)
            .ok_or_else(|| String::from("WAV similar sections candidate count overflowed"))?;
        if candidate_count > MAX_SCAN_WINDOWS {
            return Err(String::from(
                "WAV similar sections scan exceeded its work limit",
            ));
        }
        let end_frame = start_frame
            .checked_add(window_frames)
            .ok_or_else(|| String::from("WAV similar sections candidate range overflowed"))?;
        if !ranges_overlap(start_frame, end_frame, anchor_start, anchor_end) {
            if let Some(profile) = build_profile(
                reader,
                spec,
                frames,
                start_frame,
                window_frames,
                cancellation,
            )? {
                let score = profile_similarity(anchor_profile, &profile)?;
                if score >= SIMILAR_SECTION_THRESHOLD {
                    candidates.push(CandidateMatch { start_frame, score });
                }
            }
        }
        if start_frame == max_start {
            break;
        }
        let next_start = start_frame
            .checked_add(hop)
            .ok_or_else(|| String::from("WAV similar sections candidate offset overflowed"))?;
        start_frame = next_start.min(max_start);
    }
    Ok(candidates)
}

fn group_candidate_matches(
    mut candidates: Vec<CandidateMatch>,
    frames: usize,
    window_frames: usize,
    hop: usize,
) -> Vec<WaveformDetailRange> {
    candidates.sort_by_key(|candidate| candidate.start_frame);
    let mut ranges = Vec::new();
    let mut group: Option<CandidateMatch> = None;
    let mut group_end = 0usize;

    for candidate in candidates {
        let candidate_end = candidate.start_frame.saturating_add(window_frames);
        if group.is_some() && candidate.start_frame <= group_end.saturating_add(hop) {
            if let Some(best) = group.as_mut()
                && candidate.score > best.score
            {
                *best = candidate;
            }
            group_end = group_end.max(candidate_end);
            continue;
        }

        if let Some(best) = group.take() {
            ranges.push(range_for_match(best, frames, window_frames));
        }
        group = Some(candidate);
        group_end = candidate_end;
    }

    if let Some(best) = group {
        ranges.push(range_for_match(best, frames, window_frames));
    }
    ranges
}

fn range_for_match(
    match_: CandidateMatch,
    frames: usize,
    window_frames: usize,
) -> WaveformDetailRange {
    WaveformDetailRange {
        start_frame: match_.start_frame,
        end_frame: match_.start_frame.saturating_add(window_frames).min(frames),
    }
}

fn build_profile(
    reader: &mut hound::WavReader<BufReader<File>>,
    spec: hound::WavSpec,
    frames: usize,
    start_frame: usize,
    window_frames: usize,
    cancellation: Option<&AtomicBool>,
) -> Result<Option<SimilarityProfile>, String> {
    if window_frames == 0 || start_frame >= frames {
        return Ok(None);
    }
    let window_end = start_frame
        .checked_add(window_frames)
        .ok_or_else(|| String::from("WAV similar sections profile range overflowed"))?;
    if window_end > frames {
        return Ok(None);
    }

    let radius = sample_radius(window_frames);
    let mut signed = Vec::with_capacity(PROFILE_BINS);
    let mut energy = Vec::with_capacity(PROFILE_BINS);
    for bin in 0..PROFILE_BINS {
        check_cancelled(cancellation)?;
        let center_offset_float =
            ((bin as f64 + 0.5) * window_frames as f64 / PROFILE_BINS as f64).floor();
        if !center_offset_float.is_finite() || center_offset_float < 0.0 {
            return Err(String::from(
                "WAV similar sections profile position is invalid",
            ));
        }
        let center_offset = (center_offset_float as usize).min(window_frames - 1);
        let center = start_frame
            .checked_add(center_offset)
            .ok_or_else(|| String::from("WAV similar sections profile center overflowed"))?;
        let frame_start = center.saturating_sub(radius).max(start_frame);
        let frame_end = center
            .checked_add(
                radius
                    .checked_add(1)
                    .ok_or_else(|| String::from("WAV similar sections radius overflowed"))?,
            )
            .ok_or_else(|| String::from("WAV similar sections profile end overflowed"))?
            .min(window_end)
            .min(frames);
        if frame_start >= frame_end {
            return Ok(None);
        }
        let (signed_mean, energy_mean) =
            read_profile_bin(reader, spec, frame_start, frame_end, cancellation)?;
        signed.push(signed_mean);
        energy.push(energy_mean);
    }

    if signed.len() != PROFILE_BINS || energy.len() != PROFILE_BINS {
        return Ok(None);
    }
    let rms = root_mean_square(&signed)?;
    if rms < MIN_PROFILE_RMS {
        return Ok(None);
    }
    if !normalize_zero_mean(&mut signed)? {
        return Ok(None);
    }
    // Native behavior keeps an all-flat energy profile centered but unnormalized.
    let _ = normalize_zero_mean(&mut energy)?;
    Ok(Some(SimilarityProfile {
        signed,
        energy,
        rms,
    }))
}

fn sample_radius(window_frames: usize) -> usize {
    (window_frames / PROFILE_BINS / 4).clamp(1, 8)
}

fn read_profile_bin(
    reader: &mut hound::WavReader<BufReader<File>>,
    spec: hound::WavSpec,
    frame_start: usize,
    frame_end: usize,
    cancellation: Option<&AtomicBool>,
) -> Result<(f32, f32), String> {
    check_cancelled(cancellation)?;
    let frame_count = frame_end
        .checked_sub(frame_start)
        .ok_or_else(|| String::from("WAV similar sections bin range underflowed"))?;
    if frame_count == 0 || frame_count > 17 {
        return Err(String::from(
            "WAV similar sections bin neighborhood is out of bounds",
        ));
    }
    reader
        .seek(u32::try_from(frame_start).map_err(|_| {
            String::from("WAV similar sections frame position cannot be represented by WAV reader")
        })?)
        .map_err(|error| format!("failed to seek Similar Sections WAV source: {error}"))?;
    match spec.sample_format {
        hound::SampleFormat::Float => {
            let mut samples = reader.samples::<f32>();
            read_mono_means(
                &mut samples,
                usize::from(spec.channels),
                frame_count,
                cancellation,
                "float",
                |sample| sample,
            )
        }
        hound::SampleFormat::Int if spec.bits_per_sample <= 16 => {
            let scale = integer_sample_scale(spec.bits_per_sample);
            let mut samples = reader.samples::<i16>();
            read_mono_means(
                &mut samples,
                usize::from(spec.channels),
                frame_count,
                cancellation,
                "integer",
                move |sample| f32::from(sample) / scale,
            )
        }
        hound::SampleFormat::Int => {
            let scale = integer_sample_scale(spec.bits_per_sample);
            let mut samples = reader.samples::<i32>();
            read_mono_means(
                &mut samples,
                usize::from(spec.channels),
                frame_count,
                cancellation,
                "integer",
                move |sample| sample as f32 / scale,
            )
        }
    }
}

fn read_mono_means<T, I, D>(
    samples: &mut I,
    channels: usize,
    frame_count: usize,
    cancellation: Option<&AtomicBool>,
    sample_kind: &str,
    mut decode: D,
) -> Result<(f32, f32), String>
where
    I: Iterator<Item = Result<T, hound::Error>>,
    D: FnMut(T) -> f32,
{
    if channels == 0 {
        return Err(String::from("WAV similar sections source has no channels"));
    }
    let mut signed_sum = 0.0_f32;
    let mut energy_sum = 0.0_f32;
    for _ in 0..frame_count {
        check_cancelled(cancellation)?;
        let mut mono_sum = 0.0_f32;
        for _ in 0..channels {
            check_cancelled(cancellation)?;
            let sample = samples
                .next()
                .transpose()
                .map_err(|error| {
                    format!("failed to read {sample_kind} Similar Sections sample: {error}")
                })?
                .ok_or_else(|| {
                    String::from("WAV ended before declared Similar Sections frame count")
                })?;
            let sample = decode(sample);
            if !sample.is_finite() {
                return Err(String::from(
                    "WAV similar sections source contains a non-finite sample",
                ));
            }
            let sample = sample.clamp(-1.0, 1.0);
            mono_sum += sample;
            if !mono_sum.is_finite() {
                return Err(String::from(
                    "WAV similar sections mono arithmetic overflowed",
                ));
            }
        }
        let mono = mono_sum / channels as f32;
        if !mono.is_finite() {
            return Err(String::from(
                "WAV similar sections mono arithmetic overflowed",
            ));
        }
        signed_sum += mono;
        energy_sum += mono.abs();
        if !signed_sum.is_finite() || !energy_sum.is_finite() {
            return Err(String::from(
                "WAV similar sections profile arithmetic overflowed",
            ));
        }
    }
    let scale = 1.0 / frame_count as f32;
    Ok((signed_sum * scale, energy_sum * scale))
}

fn normalize_zero_mean(values: &mut [f32]) -> Result<bool, String> {
    if values.is_empty() {
        return Ok(false);
    }
    let mean = values.iter().copied().sum::<f32>() / values.len() as f32;
    if !mean.is_finite() {
        return Err(String::from(
            "WAV similar sections profile arithmetic overflowed",
        ));
    }
    for value in values.iter_mut() {
        *value -= mean;
        if !value.is_finite() {
            return Err(String::from(
                "WAV similar sections profile arithmetic overflowed",
            ));
        }
    }
    let rms = root_mean_square(values)?;
    if rms < MIN_PROFILE_RMS {
        return Ok(false);
    }
    for value in values {
        *value /= rms;
        if !value.is_finite() {
            return Err(String::from(
                "WAV similar sections profile arithmetic overflowed",
            ));
        }
    }
    Ok(true)
}

fn root_mean_square(values: &[f32]) -> Result<f32, String> {
    if values.is_empty() {
        return Ok(0.0);
    }
    let sum = values
        .iter()
        .map(|value| value * value)
        .try_fold(0.0_f32, |sum, value| {
            let value = sum + value;
            value.is_finite().then_some(value)
        })
        .ok_or_else(|| String::from("WAV similar sections profile arithmetic overflowed"))?;
    let rms = (sum / values.len() as f32).sqrt();
    if rms.is_finite() {
        Ok(rms)
    } else {
        Err(String::from(
            "WAV similar sections profile arithmetic overflowed",
        ))
    }
}

fn profile_similarity(
    anchor: &SimilarityProfile,
    candidate: &SimilarityProfile,
) -> Result<f32, String> {
    let signed_similarity = normalized_dot(&anchor.signed, &candidate.signed).max(0.0);
    let energy_similarity = normalized_dot(&anchor.energy, &candidate.energy).max(0.0);
    let loudness_ratio = anchor.rms.min(candidate.rms) / anchor.rms.max(candidate.rms).max(1.0e-9);
    let score = (signed_similarity * 0.72 + energy_similarity * 0.23 + loudness_ratio * 0.05)
        .clamp(0.0, 1.0);
    score
        .is_finite()
        .then_some(score)
        .ok_or_else(|| String::from("WAV similar sections score arithmetic overflowed"))
}

fn normalized_dot(a: &[f32], b: &[f32]) -> f32 {
    let len = a.len().min(b.len());
    if len == 0 {
        return 0.0;
    }
    a.iter()
        .zip(b.iter())
        .take(len)
        .map(|(left, right)| left * right)
        .sum::<f32>()
        / len as f32
}

fn ranges_overlap(start_a: usize, end_a: usize, start_b: usize, end_b: usize) -> bool {
    start_a < end_b && start_b < end_a
}

fn integer_sample_scale(bits_per_sample: u16) -> f32 {
    (1_i64 << bits_per_sample.saturating_sub(1).min(31)).max(1) as f32
}

fn check_cancelled(cancellation: Option<&AtomicBool>) -> Result<(), String> {
    if crate::is_cancelled(cancellation) {
        Err(String::from("WAV similar sections canceled"))
    } else {
        Ok(())
    }
}

fn scan_error(path: &Path, error: String, cancellation: Option<&AtomicBool>) -> String {
    if crate::is_cancelled(cancellation) {
        String::from("WAV similar sections canceled")
    } else {
        format!(
            "WAV similar sections failed for {}: {error}",
            path.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::{f32::consts::TAU, fs, path::Path, sync::atomic::AtomicBool};
    use tempfile::TempDir;

    fn float_spec(channels: u16) -> WavSpec {
        WavSpec {
            channels,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        }
    }

    fn write_float_wav(path: &Path, channels: u16, frames: &[[f32; 2]]) {
        let mut writer = WavWriter::create(path, float_spec(channels)).expect("create WAV");
        for frame in frames {
            for sample in frame.iter().take(usize::from(channels)) {
                writer.write_sample(*sample).expect("write WAV sample");
            }
        }
        writer.finalize().expect("finish WAV");
    }

    fn context_for(path: &Path) -> WaveformDetailContext {
        crate::load_waveform_preview(path)
            .expect("load waveform context")
            .detail_context
            .expect("waveform context")
    }

    fn shape(frames: usize, phase: f32) -> Vec<f32> {
        (0..frames)
            .map(|frame| {
                let t = frame as f32 / frames as f32;
                let envelope = (1.0 - t).powf(1.7);
                (TAU * 13.0 * t + phase).sin() * envelope
            })
            .collect()
    }

    fn write_shape(target: &mut [[f32; 2]], start: usize, shape: &[f32], gain: f32) {
        for (offset, sample) in shape.iter().enumerate() {
            if let Some(frame) = target.get_mut(start + offset) {
                frame[0] = *sample * gain;
                frame[1] = *sample * gain;
            }
        }
    }

    #[test]
    fn scan_marks_repeated_stereo_motifs_but_not_anchor() {
        let directory = TempDir::new().expect("create fixture directory");
        let path = directory.path().join("motifs.wav");
        let frames = 20_000;
        let motif = shape(720, 0.0);
        let mut samples = vec![[0.0_f32; 2]; frames];
        write_shape(&mut samples, 1_000, &motif, 1.0);
        write_shape(&mut samples, 7_000, &motif, 0.96);
        write_shape(&mut samples, 13_000, &motif, 1.02);
        write_float_wav(&path, 2, &samples);

        let context = context_for(&path);
        let request = WavSimilarSectionsRequest::new(
            context,
            WaveformDetailRange {
                start_frame: 1_000,
                end_frame: 1_720,
            },
        )
        .expect("create request");
        let ranges = scan_wav_similar_sections(&request).expect("scan succeeds");
        let starts = ranges
            .iter()
            .map(|range| range.start_frame)
            .collect::<Vec<_>>();

        assert!(
            starts.iter().any(|start| start.abs_diff(7_000) <= 180),
            "second motif should be marked: {starts:?}"
        );
        assert!(
            starts.iter().any(|start| start.abs_diff(13_000) <= 180),
            "third motif should be marked: {starts:?}"
        );
        assert!(
            starts.iter().all(|start| start.abs_diff(1_000) > 180),
            "anchor should not be marked: {starts:?}"
        );
    }

    #[test]
    fn short_and_quiet_anchors_return_actionable_errors() {
        let directory = TempDir::new().expect("create fixture directory");
        let path = directory.path().join("quiet.wav");
        let samples = vec![[0.0_f32; 2]; 2_000];
        write_float_wav(&path, 2, &samples);
        let context = context_for(&path);

        let quiet_request = WavSimilarSectionsRequest::new(
            context.clone(),
            WaveformDetailRange {
                start_frame: 100,
                end_frame: 900,
            },
        )
        .expect("create quiet request");
        assert_eq!(
            scan_wav_similar_sections(&quiet_request),
            Err(String::from("Selected playmark is too quiet to compare"))
        );

        let short_request = WavSimilarSectionsRequest::new(
            context,
            WaveformDetailRange {
                start_frame: 100,
                end_frame: 163,
            },
        )
        .expect("create short request");
        assert_eq!(
            scan_wav_similar_sections(&short_request),
            Err(String::from("Select a slightly longer playmark section"))
        );
    }

    #[test]
    fn candidate_hop_is_strictly_bounded_to_eight_thousand_windows() {
        let frames = u32::MAX as usize;
        let window_frames = 64;
        let hop = scan_hop_frames(frames, window_frames, 48_000).expect("valid scan dimensions");
        let scan_span = frames - window_frames;
        let count = (scan_span as u64 / hop as u64) + 1;
        assert!(count <= MAX_SCAN_WINDOWS as u64, "candidate count: {count}");
        assert_eq!(count, MAX_SCAN_WINDOWS as u64);
    }

    #[test]
    fn precancelled_scan_is_rejected_before_opening_source() {
        let directory = TempDir::new().expect("create fixture directory");
        let path = directory.path().join("cancel.wav");
        write_float_wav(&path, 1, &vec![[0.1, 0.0]; 128]);
        let context = context_for(&path);
        let request = WavSimilarSectionsRequest::new(
            context,
            WaveformDetailRange {
                start_frame: 0,
                end_frame: 64,
            },
        )
        .expect("create request");
        let cancellation = AtomicBool::new(true);
        assert_eq!(
            scan_wav_similar_sections_with_cancel(&request, Some(&cancellation)),
            Err(String::from("WAV similar sections canceled"))
        );
    }

    #[test]
    fn stale_file_revision_is_rejected() {
        let directory = TempDir::new().expect("create fixture directory");
        let path = directory.path().join("stale.wav");
        write_float_wav(&path, 1, &vec![[0.1, 0.0]; 256]);
        let context = context_for(&path);
        let request = WavSimilarSectionsRequest::new(
            context,
            WaveformDetailRange {
                start_frame: 0,
                end_frame: 64,
            },
        )
        .expect("create request");

        write_float_wav(&path, 1, &vec![[0.2, 0.0]; 256]);
        let error = scan_wav_similar_sections(&request).expect_err("stale request rejected");
        assert!(
            error.contains("stale WAV similar sections request"),
            "{error}"
        );
    }

    #[test]
    fn invalid_anchor_ranges_are_rejected() {
        let directory = TempDir::new().expect("create fixture directory");
        let path = directory.path().join("invalid.wav");
        write_float_wav(&path, 1, &vec![[0.1, 0.0]; 128]);
        let context = context_for(&path);

        let empty = WavSimilarSectionsRequest::new(
            context.clone(),
            WaveformDetailRange {
                start_frame: 10,
                end_frame: 10,
            },
        )
        .expect_err("empty anchor rejected");
        assert_eq!(empty, "WAV similar sections anchor must be non-empty");

        let outside = WavSimilarSectionsRequest::new(
            context,
            WaveformDetailRange {
                start_frame: 10,
                end_frame: 129,
            },
        )
        .expect_err("outside anchor rejected");
        assert_eq!(outside, "WAV similar sections anchor exceeds source frames");
    }

    #[test]
    fn changed_wav_fixture_keeps_original_context_revision() {
        let directory = TempDir::new().expect("create fixture directory");
        let path = directory.path().join("revision.wav");
        write_float_wav(&path, 1, &vec![[0.1, 0.0]; 128]);
        let context = context_for(&path);
        let original_revision = context.file_revision.clone();
        fs::write(&path, fs::read(&path).expect("read fixture")).expect("rewrite fixture");
        assert_eq!(context.file_revision, original_revision);
    }

    #[test]
    #[ignore = "manual release timing probe"]
    fn release_timing_probe_scans_a_representative_sixty_second_source() {
        let directory = TempDir::new().expect("create timing fixture directory");
        let path = directory.path().join("sixty-seconds.wav");
        let frames: usize = 48_000 * 60;
        let window_frames: usize = 48_000;
        let mut writer = WavWriter::create(&path, float_spec(1)).expect("create timing WAV");
        for frame in 0..frames {
            let phase = (frame % window_frames) as f32 / window_frames as f32;
            let sample = (TAU * phase).sin() * 0.25;
            writer
                .write_sample(sample)
                .expect("write timing WAV sample");
        }
        writer.finalize().expect("finish timing WAV");

        let context = context_for(&path);
        let anchor_start = window_frames;
        let anchor_end = anchor_start + window_frames;
        let request = WavSimilarSectionsRequest::new(
            context,
            WaveformDetailRange {
                start_frame: anchor_start,
                end_frame: anchor_end,
            },
        )
        .expect("create timing request");
        let hop = scan_hop_frames(frames, window_frames, 48_000).expect("timing scan dimensions");
        let candidate_windows = (frames - window_frames) / hop + 1;
        let radius = sample_radius(window_frames);
        assert!(candidate_windows <= MAX_SCAN_WINDOWS);
        assert_eq!(candidate_windows, 7_978);
        assert_eq!(radius, 8);
        let started = std::time::Instant::now();
        let ranges = scan_wav_similar_sections(&request).expect("timing scan succeeds");
        eprintln!(
            "Similar Sections 60s/48k mono: {:?}, {} ranges, {} candidate windows, hop {}, radius {} ({} frames/bin neighborhood)",
            started.elapsed(),
            ranges.len(),
            candidate_windows,
            hop,
            radius,
            radius * 2 + 1,
        );
    }
}
