#![deny(missing_docs)]
#![deny(warnings)]

//! Read-only audio scanning and bounded waveform previews for the GPUI browser.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use wavecrate_analysis::probe_metadata;
use wavecrate_core::AudioFile;
use wavecrate_library::sample_sources::is_recognized_audio;

mod visual_bands;

/// Maximum number of bars generated for a selected sample's preview.
pub const MAX_WAVEFORM_POINTS: usize = 180;

/// Maximum number of frequency-layer columns generated for a selected sample's preview.
pub const MAX_WAVEFORM_BAND_POINTS: usize = 2_048;

/// Result of an in-memory folder scan.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioScan {
    /// Canonical root used for this scan, resolved on the background worker.
    pub root: PathBuf,
    /// Audio files found in deterministic relative-path order.
    pub files: Vec<AudioFile>,
    /// Relative directories discovered during traversal, including empty directories and `.`.
    pub directories: Vec<PathBuf>,
    /// Number of directory entries skipped because they were links, unreadable, or invalid.
    pub skipped_entries: usize,
}

/// Bounded waveform data for drawing a sample preview.
#[derive(Clone, Debug, PartialEq)]
pub struct WaveformPreview {
    /// Peak magnitude for each display bar, normalized to the range `0.0..=1.0`.
    pub peaks: Vec<f32>,
    /// Layered frequency magnitudes for each display column, in low/mid/high/raw order.
    ///
    /// The first three lanes are nonnegative low, mid, and high envelope magnitudes. The final
    /// lane is the raw peak magnitude. A renderer should paint the lanes in this order so low
    /// frequency content is the back layer, high frequency content remains visible above it, and
    /// the raw lane is the front detail layer.
    pub bands: Vec<[f32; 4]>,
    /// Duration represented by the decoded preview buffer.
    pub duration_seconds: f32,
    /// Sample rate used for the preview buffer.
    pub sample_rate: u32,
}

/// Enumerate recognized audio files below `root` without following directory links.
///
/// This function does not open or create a Wavecrate database and does not mutate the source
/// folder. Container metadata is probed only after a file is discovered; sample data is decoded
/// separately by [`load_waveform_preview`].
pub fn enumerate_audio_files(root: &Path) -> Result<AudioScan, String> {
    enumerate_audio_files_with_cancel(root, None)
}

/// Enumerate recognized audio files while honoring a caller-owned cancellation flag.
///
/// The flag is checked between directory entries so replacing a pending UI scan does not leave
/// an abandoned traversal running through a large source tree.
pub fn enumerate_audio_files_with_cancel(
    root: &Path,
    cancellation: Option<&AtomicBool>,
) -> Result<AudioScan, String> {
    // Resolve the selected root once in the worker. Projection and UI code can then treat the
    // paths as stable identities without performing filesystem calls during filtering or sorting.
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let root_metadata = fs::metadata(&root)
        .map_err(|error| format!("Could not read folder {}: {error}", root.display()))?;
    if !root_metadata.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }

    let mut pending = vec![root.clone()];
    let mut directories = Vec::new();
    let mut files = Vec::new();
    let mut skipped_entries = 0;

    while let Some(directory) = pending.pop() {
        if cancellation.is_some_and(|flag| flag.load(AtomicOrdering::Acquire)) {
            return Err("scan canceled".to_string());
        }
        let relative_directory = directory
            .strip_prefix(&root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| PathBuf::from("."));
        directories.push(if relative_directory.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            relative_directory
        });
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if directory == root => {
                return Err(format!("Could not read folder {}: {error}", root.display()));
            }
            Err(_) => {
                skipped_entries += 1;
                continue;
            }
        };

        for entry in entries {
            if cancellation.is_some_and(|flag| flag.load(AtomicOrdering::Acquire)) {
                return Err("scan canceled".to_string());
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    skipped_entries += 1;
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => {
                    skipped_entries += 1;
                    continue;
                }
            };
            if file_type.is_symlink() {
                skipped_entries += 1;
                continue;
            }

            let path = entry.path();
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if !file_type.is_file() || !is_recognized_audio(&path) {
                continue;
            }

            let byte_len = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            let probe = probe_metadata(&path).ok();
            let relative_path = path
                .strip_prefix(&root)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| {
                    path.file_name()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| path.clone())
                });
            files.push(AudioFile {
                path,
                relative_path,
                byte_len,
                duration_seconds: probe.as_ref().and_then(|value| value.duration_seconds),
                sample_rate: probe.and_then(|value| value.sample_rate),
            });
        }
    }

    files.sort_by(|left, right| {
        let left_key = left.relative_path.to_string_lossy().to_ascii_lowercase();
        let right_key = right.relative_path.to_string_lossy().to_ascii_lowercase();
        left_key
            .cmp(&right_key)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
    directories.sort_by(|left, right| {
        let left_key = left.to_string_lossy().to_ascii_lowercase();
        let right_key = right.to_string_lossy().to_ascii_lowercase();
        left_key.cmp(&right_key).then_with(|| left.cmp(right))
    });

    Ok(AudioScan {
        root,
        files,
        directories,
        skipped_entries,
    })
}

/// Decode a full-length, bounded-memory mono waveform preview from a source file.
pub fn load_waveform_preview(path: &Path) -> Result<WaveformPreview, String> {
    load_waveform_preview_with_cancel(path, None)
}

/// Decode a full-length WAV waveform while honoring an optional cancellation flag.
pub fn load_waveform_preview_with_cancel(
    path: &Path,
    cancellation: Option<&AtomicBool>,
) -> Result<WaveformPreview, String> {
    if is_cancelled(cancellation) {
        return Err(String::from("waveform preview canceled"));
    }
    if !is_wav_path(path) {
        return Err(format!(
            "{} waveform preview currently requires WAV channel metadata",
            path.display()
        ));
    }
    let mut reader = hound::WavReader::open(path)
        .map_err(|error| format!("WAV preview failed for {}: {error}", path.display()))?;
    let spec = reader.spec();
    if spec.sample_rate == 0 {
        return Err(format!(
            "{} WAV has an invalid zero sample rate",
            path.display(),
        ));
    }
    if spec.channels == 0 {
        return Err(format!("{} WAV has no channels", path.display()));
    }
    let total_frames = reader.duration() as usize;
    if total_frames == 0 {
        return Err(format!("{} contains no decodable samples", path.display()));
    }
    let channels = usize::from(spec.channels);
    let global_peak =
        stream_global_mono_peak(&mut reader, spec, channels, total_frames, cancellation)?;
    if is_cancelled(cancellation) {
        return Err(String::from("waveform preview canceled"));
    }
    reader
        .seek(0)
        .map_err(|error| format!("Failed to rewind WAV {}: {error}", path.display()))?;
    let normalization_gain = if global_peak.is_finite() && global_peak >= f32::MIN_POSITIVE {
        1.0 / global_peak
    } else {
        1.0
    };
    let peak_count = total_frames.min(MAX_WAVEFORM_POINTS);
    let mut peaks = vec![0.0_f32; peak_count];
    let mut bands = visual_bands::StreamingProjector::new(
        spec.sample_rate,
        total_frames,
        MAX_WAVEFORM_BAND_POINTS,
    );
    stream_wav_frames(
        &mut reader,
        spec,
        channels,
        total_frames,
        cancellation,
        |frame_index, mono| {
            let normalized = (mono * normalization_gain).clamp(-1.0, 1.0);
            let peak_index = column_index(frame_index, total_frames, peak_count);
            peaks[peak_index] = peaks[peak_index].max(normalized.abs());
            bands.push(frame_index, normalized);
            Ok(())
        },
    )?;
    if is_cancelled(cancellation) {
        return Err(String::from("waveform preview canceled"));
    }
    let bands = bands.finish();
    let duration_seconds = total_frames as f64 / spec.sample_rate as f64;
    let duration_seconds = duration_seconds as f32;
    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        return Err(format!("{} has an invalid WAV duration", path.display()));
    }
    Ok(WaveformPreview {
        peaks,
        bands,
        duration_seconds,
        sample_rate: spec.sample_rate,
    })
}

fn stream_global_mono_peak<R: Read>(
    reader: &mut hound::WavReader<R>,
    spec: hound::WavSpec,
    channels: usize,
    total_frames: usize,
    cancellation: Option<&AtomicBool>,
) -> Result<f32, String> {
    let mut global_peak = 0.0_f32;
    stream_wav_frames(
        reader,
        spec,
        channels,
        total_frames,
        cancellation,
        |_, mono| {
            global_peak = global_peak.max(mono.abs());
            Ok(())
        },
    )?;
    Ok(global_peak)
}

fn stream_wav_frames<R: Read, F>(
    reader: &mut hound::WavReader<R>,
    spec: hound::WavSpec,
    channels: usize,
    total_frames: usize,
    cancellation: Option<&AtomicBool>,
    process: F,
) -> Result<(), String>
where
    F: FnMut(usize, f32) -> Result<(), String>,
{
    match spec.sample_format {
        hound::SampleFormat::Float => {
            let mut samples = reader.samples::<f32>();
            stream_sample_frames(
                &mut samples,
                channels,
                total_frames,
                cancellation,
                "float",
                sanitize_stream_sample,
                process,
            )
        }
        hound::SampleFormat::Int if spec.bits_per_sample <= 16 => {
            let scale = integer_sample_scale(spec.bits_per_sample);
            let mut samples = reader.samples::<i16>();
            stream_sample_frames(
                &mut samples,
                channels,
                total_frames,
                cancellation,
                "integer",
                move |sample| sanitize_stream_sample(f32::from(sample) / scale),
                process,
            )
        }
        hound::SampleFormat::Int => {
            let scale = integer_sample_scale(spec.bits_per_sample);
            let mut samples = reader.samples::<i32>();
            stream_sample_frames(
                &mut samples,
                channels,
                total_frames,
                cancellation,
                "integer",
                move |sample| sanitize_stream_sample(sample as f32 / scale),
                process,
            )
        }
    }
}

fn stream_sample_frames<T, I, D, F>(
    samples: &mut I,
    channels: usize,
    total_frames: usize,
    cancellation: Option<&AtomicBool>,
    sample_kind: &str,
    mut decode: D,
    mut process: F,
) -> Result<(), String>
where
    I: Iterator<Item = Result<T, hound::Error>>,
    D: FnMut(T) -> f32,
    F: FnMut(usize, f32) -> Result<(), String>,
{
    for frame_index in 0..total_frames {
        if is_cancelled(cancellation) {
            return Err(String::from("waveform preview canceled"));
        }
        let mut sum = 0.0_f32;
        for _ in 0..channels {
            let sample = samples
                .next()
                .transpose()
                .map_err(|error| format!("failed to read {sample_kind} sample: {error}"))?
                .ok_or_else(|| String::from("WAV ended before declared frame count"))?;
            sum += decode(sample);
        }
        process(frame_index, sum / channels as f32)?;
    }
    Ok(())
}

fn integer_sample_scale(bits_per_sample: u16) -> f32 {
    (1_i64 << bits_per_sample.saturating_sub(1).min(31)).max(1) as f32
}

fn sanitize_stream_sample(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn column_index(frame_index: usize, total_frames: usize, column_count: usize) -> usize {
    debug_assert!(total_frames > 0);
    debug_assert!(column_count > 0);
    ((frame_index as u128 * column_count as u128) / total_frames as u128) as usize
}

fn is_cancelled(cancellation: Option<&AtomicBool>) -> bool {
    cancellation.is_some_and(|flag| flag.load(AtomicOrdering::Acquire))
}

fn is_wav_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::f32::consts::TAU;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    fn write_fixture(path: &Path) {
        let spec = WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut writer = WavWriter::create(path, spec).expect("create fixture");
        for index in 0..16_000 {
            let sample = (index as f32 * TAU * 440.0 / 16_000.0).sin() * 0.5;
            writer.write_sample(sample).expect("write fixture");
        }
        writer.finalize().expect("finish fixture");
    }

    #[test]
    fn scan_is_recursive_and_skips_apple_double_sidecars() {
        let directory = TempDir::new().expect("temporary directory");
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).expect("nested directory");
        write_fixture(&directory.path().join("root.wav"));
        write_fixture(&nested.join("nested.WAV"));
        write_fixture(&directory.path().join("._root.wav"));
        std::fs::write(directory.path().join("notes.txt"), b"notes").expect("notes");

        let scan = enumerate_audio_files(directory.path()).expect("scan");
        assert_eq!(scan.files.len(), 2);
        assert_eq!(
            scan.files[0].relative_path,
            PathBuf::from("nested/nested.WAV")
        );
        assert_eq!(scan.files[1].relative_path, PathBuf::from("root.wav"));
        assert_eq!(scan.files[1].duration_seconds, Some(1.0));
        assert!(scan.directories.contains(&PathBuf::from(".")));
        assert!(scan.directories.contains(&PathBuf::from("nested")));
    }

    #[test]
    fn scan_retains_empty_directories_and_canonical_root() {
        let directory = TempDir::new().expect("temporary directory");
        std::fs::create_dir(directory.path().join("empty")).expect("empty directory");

        let scan = enumerate_audio_files(directory.path()).expect("scan");
        assert_eq!(
            scan.root,
            std::fs::canonicalize(directory.path()).expect("canonical root")
        );
        assert_eq!(
            scan.directories,
            vec![PathBuf::from("."), PathBuf::from("empty")]
        );
    }

    #[test]
    fn waveform_preview_is_bounded_and_contains_real_signal() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("tone.wav");
        write_fixture(&path);

        let preview = load_waveform_preview(&path).expect("preview");
        assert!(preview.peaks.len() <= MAX_WAVEFORM_POINTS);
        assert!(preview.peaks.iter().any(|peak| *peak > 0.1));
        assert!(preview.bands.len() <= MAX_WAVEFORM_BAND_POINTS);
        assert!(
            preview
                .bands
                .iter()
                .flatten()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
        );
        assert_eq!(preview.sample_rate, 16_000);
        assert!((preview.duration_seconds - 1.0).abs() < 0.02);
    }

    #[test]
    fn full_length_preview_keeps_a_late_transient_after_six_seconds() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("long.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut writer = WavWriter::create(&path, spec).expect("create fixture");
        let frames = 8 * spec.sample_rate as usize;
        for frame in 0..frames {
            let sample = if frame == frames - 1 { 0.75 } else { 0.0 };
            writer.write_sample(sample).expect("write fixture");
        }
        writer.finalize().expect("finish fixture");

        let preview = load_waveform_preview(&path).expect("full preview");
        assert!((preview.duration_seconds - 8.0).abs() < 0.001);
        assert_eq!(preview.peaks.len(), MAX_WAVEFORM_POINTS);
        assert_eq!(preview.bands.len(), MAX_WAVEFORM_BAND_POINTS);
        assert!(preview.peaks.last().copied().unwrap_or_default() > 0.99);
        assert!(preview.bands.last().copied().unwrap_or_default()[3] > 0.99);
    }

    #[test]
    fn streaming_projection_matches_the_slice_projector_after_global_normalization() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("projection.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut writer = WavWriter::create(&path, spec).expect("create fixture");
        let samples = [0.1_f32, -0.25, 0.8, 0.3, -0.4, 0.05, 0.2, -0.7];
        for sample in samples {
            writer.write_sample(sample).expect("write fixture");
        }
        writer.finalize().expect("finish fixture");

        let preview = load_waveform_preview(&path).expect("full preview");
        let peak = samples.iter().copied().map(f32::abs).fold(0.0, f32::max);
        let normalized = samples
            .iter()
            .copied()
            .map(|sample| sample / peak)
            .collect::<Vec<_>>();
        let expected =
            visual_bands::project(&normalized, spec.sample_rate, MAX_WAVEFORM_BAND_POINTS);
        assert_eq!(preview.bands, expected);
        assert_eq!(preview.peaks.iter().copied().fold(0.0, f32::max), 1.0);
    }

    #[test]
    fn stereo_duration_is_frame_based_and_high_rate_multichannel_streams() {
        let directory = TempDir::new().expect("temporary directory");
        let stereo_path = directory.path().join("stereo.wav");
        let stereo_spec = WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut stereo = WavWriter::create(&stereo_path, stereo_spec).expect("create stereo");
        for _ in 0..4 {
            stereo.write_sample::<i16>(16_000).expect("left sample");
            stereo.write_sample::<i16>(-16_000).expect("right sample");
        }
        stereo.finalize().expect("finish stereo");
        let stereo_preview = load_waveform_preview(&stereo_path).expect("stereo preview");
        assert!((stereo_preview.duration_seconds - 4.0 / 48_000.0).abs() < 1e-6);
        assert_eq!(stereo_preview.sample_rate, 48_000);
        assert_eq!(stereo_preview.peaks.len(), 4);

        let wide_path = directory.path().join("wide.wav");
        let wide_spec = WavSpec {
            channels: 8,
            sample_rate: 384_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut wide = WavWriter::create(&wide_path, wide_spec).expect("create wide");
        for frame in 0..32 {
            for channel in 0..8 {
                let sample = if frame == 31 && channel == 7 {
                    i16::MAX
                } else {
                    0
                };
                wide.write_sample::<i16>(sample).expect("wide sample");
            }
        }
        wide.finalize().expect("finish wide");
        let wide_preview = load_waveform_preview(&wide_path).expect("wide preview");
        assert!((wide_preview.duration_seconds - 32.0 / 384_000.0).abs() < 1e-8);
        assert_eq!(wide_preview.sample_rate, 384_000);
        assert_eq!(wide_preview.peaks.len(), 32);
        assert!(wide_preview.peaks.last().copied().unwrap_or_default() > 0.99);
    }

    #[test]
    fn pcm24_float_nonfinite_and_malformed_inputs_are_handled() {
        let directory = TempDir::new().expect("temporary directory");
        let pcm_path = directory.path().join("pcm24.wav");
        let pcm_spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 24,
            sample_format: SampleFormat::Int,
        };
        let mut pcm = WavWriter::create(&pcm_path, pcm_spec).expect("create pcm24");
        pcm.write_sample::<i32>(8_388_607).expect("positive pcm24");
        pcm.write_sample::<i32>(-8_388_608).expect("negative pcm24");
        pcm.finalize().expect("finish pcm24");
        let pcm_preview = load_waveform_preview(&pcm_path).expect("pcm24 preview");
        assert!(pcm_preview.peaks.iter().copied().any(|peak| peak > 0.99));

        let float_path = directory.path().join("nonfinite.wav");
        let float_spec = WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut float = WavWriter::create(&float_path, float_spec).expect("create float");
        for sample in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.25] {
            float.write_sample(sample).expect("float sample");
        }
        float.finalize().expect("finish float");
        let float_preview = load_waveform_preview(&float_path).expect("float preview");
        assert!(
            float_preview
                .peaks
                .iter()
                .chain(float_preview.bands.iter().flatten())
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
        );
        assert_eq!(float_preview.peaks[0], 0.0);
        assert!(float_preview.peaks[3] > 0.99);

        let mut truncated = std::fs::read(&pcm_path).expect("read pcm24");
        truncated.pop();
        let truncated_path = directory.path().join("truncated.wav");
        std::fs::write(&truncated_path, truncated).expect("write truncated");
        let error = load_waveform_preview(&truncated_path).expect_err("truncated must fail");
        assert!(error.contains("failed to read") || error.contains("ended before"));
    }

    #[test]
    fn empty_and_non_wav_inputs_are_rejected() {
        let directory = TempDir::new().expect("temporary directory");
        let empty_path = directory.path().join("empty.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let writer = WavWriter::create(&empty_path, spec).expect("create empty");
        writer.finalize().expect("finish empty");
        let empty_error = load_waveform_preview(&empty_path).expect_err("empty must fail");
        assert!(empty_error.contains("no decodable samples"));

        let text_path = directory.path().join("not-a-wav.txt");
        std::fs::write(&text_path, b"audio").expect("write text");
        let text_error = load_waveform_preview(&text_path).expect_err("non-wav must fail");
        assert!(text_error.contains("requires WAV"));
    }

    #[test]
    fn cancellation_is_honored_before_and_during_streaming() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("cancel.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(&path, spec).expect("create cancel fixture");
        for _ in 0..16_000 {
            writer.write_sample::<i16>(1_000).expect("cancel sample");
        }
        writer.finalize().expect("finish cancel fixture");

        let already_cancelled = AtomicBool::new(true);
        let error = load_waveform_preview_with_cancel(&path, Some(&already_cancelled))
            .expect_err("pre-cancel must stop");
        assert_eq!(error, "waveform preview canceled");

        let mut reader = hound::WavReader::open(&path).expect("open cancel fixture");
        let flag = AtomicBool::new(false);
        let mut seen = 0usize;
        let total_frames = reader.duration() as usize;
        let result = stream_wav_frames(&mut reader, spec, 1, total_frames, Some(&flag), |_, _| {
            seen += 1;
            if seen == 8 {
                flag.store(true, AtomicOrdering::Release);
            }
            Ok(())
        });
        assert_eq!(result, Err(String::from("waveform preview canceled")));
        assert_eq!(seen, 8);
    }
}
