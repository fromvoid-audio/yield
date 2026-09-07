#![deny(missing_docs)]
#![deny(warnings)]

//! Backend helpers for the standalone GPUI sample browser.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use wavecrate_analysis::{decode_for_analysis_with_rate_limit, probe_metadata};
use wavecrate_library::sample_sources::is_recognized_audio;

/// Maximum number of bars generated for a selected sample's preview.
pub const MAX_WAVEFORM_POINTS: usize = 180;

/// Maximum analysis window requested for a waveform preview.
pub const MAX_PREVIEW_SECONDS: f32 = 6.0;

const PREVIEW_SAMPLE_RATE: u32 = 16_000;
const MAX_PREVIEW_SOURCE_SAMPLES: u64 = 8_000_000;
const ANALYSIS_WINDOW_PADDING_SECONDS: f64 = 2.0;
const MAX_PREVIEW_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// A source audio file discovered by a folder scan.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioFile {
    /// Absolute or user-provided path to the audio file.
    pub path: PathBuf,
    /// Path relative to the selected folder.
    pub relative_path: PathBuf,
    /// File size in bytes when metadata was available.
    pub byte_len: u64,
    /// Duration reported by a container-level metadata probe.
    pub duration_seconds: Option<f32>,
    /// Sample rate reported by a container-level metadata probe.
    pub sample_rate: Option<u32>,
}

/// Result of an in-memory folder scan.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioScan {
    /// Audio files found in deterministic relative-path order.
    pub files: Vec<AudioFile>,
    /// Number of directory entries skipped because they were links, unreadable, or invalid.
    pub skipped_entries: usize,
}

/// Bounded waveform data for drawing a sample preview.
#[derive(Clone, Debug, PartialEq)]
pub struct WaveformPreview {
    /// Peak magnitude for each display bar, normalized to the range `0.0..=1.0`.
    pub peaks: Vec<f32>,
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
    let root_metadata = fs::metadata(root)
        .map_err(|error| format!("Could not read folder {}: {error}", root.display()))?;
    if !root_metadata.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }

    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    let mut skipped_entries = 0;

    while let Some(directory) = pending.pop() {
        if cancellation.is_some_and(|flag| flag.load(AtomicOrdering::Acquire)) {
            return Err("scan canceled".to_string());
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
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
                .strip_prefix(root)
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

    Ok(AudioScan {
        files,
        skipped_entries,
    })
}

/// Decode a bounded, mono waveform preview from a source file.
pub fn load_waveform_preview(path: &Path) -> Result<WaveformPreview, String> {
    let decode_seconds = preview_decode_seconds(path)?;
    let decoded =
        decode_for_analysis_with_rate_limit(path, PREVIEW_SAMPLE_RATE, Some(decode_seconds))?;
    let peaks = peak_envelope(&decoded.mono, MAX_WAVEFORM_POINTS);
    if peaks.is_empty() {
        return Err(format!("{} contains no decodable samples", path.display()));
    }
    Ok(WaveformPreview {
        peaks,
        duration_seconds: decoded.duration_seconds,
        sample_rate: decoded.sample_rate_used,
    })
}

fn preview_decode_seconds(path: &Path) -> Result<f32, String> {
    let byte_len = fs::metadata(path)
        .map_err(|error| format!("Could not inspect {}: {error}", path.display()))?
        .len();
    if byte_len > MAX_PREVIEW_FILE_BYTES {
        return Err(format!(
            "{} is too large for a bounded waveform preview ({} MiB limit)",
            path.display(),
            MAX_PREVIEW_FILE_BYTES / (1024 * 1024)
        ));
    }

    let is_wav = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"));
    if !is_wav {
        return Err(format!(
            "{} waveform preview currently requires WAV channel metadata",
            path.display()
        ));
    }
    let reader = hound::WavReader::open(path)
        .map_err(|error| format!("WAV preview probe failed for {}: {error}", path.display()))?;
    let spec = reader.spec();
    let (sample_rate, channels) = (spec.sample_rate.max(1), spec.channels.max(1));

    let source_seconds = MAX_PREVIEW_SOURCE_SAMPLES as f64
        / (sample_rate as f64 * u64::from(channels.max(1)) as f64);
    let requested_seconds =
        MAX_PREVIEW_SECONDS.min((source_seconds - ANALYSIS_WINDOW_PADDING_SECONDS).max(0.0) as f32);
    if requested_seconds <= 0.0 {
        return Err(format!(
            "{} has too many source samples per second for a bounded waveform preview",
            path.display()
        ));
    }
    Ok(requested_seconds)
}

/// Format a duration for compact display in the browser chrome.
pub fn format_duration(duration_seconds: Option<f32>) -> String {
    let Some(duration_seconds) = duration_seconds.filter(|duration| duration.is_finite()) else {
        return "--:--".to_string();
    };
    let total_seconds = duration_seconds.max(0.0).round() as u64;
    format!("{:02}:{:02}", total_seconds / 60, total_seconds % 60)
}

fn peak_envelope(samples: &[f32], point_count: usize) -> Vec<f32> {
    if samples.is_empty() || point_count == 0 {
        return Vec::new();
    }
    let point_count = point_count.min(samples.len());
    (0..point_count)
        .map(|point| {
            let start = point * samples.len() / point_count;
            let end = ((point + 1) * samples.len() / point_count).max(start + 1);
            samples[start..end.min(samples.len())]
                .iter()
                .copied()
                .map(f32::abs)
                .fold(0.0, f32::max)
                .clamp(0.0, 1.0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::f32::consts::TAU;
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
    }

    #[test]
    fn waveform_preview_is_bounded_and_contains_real_signal() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("tone.wav");
        write_fixture(&path);

        let preview = load_waveform_preview(&path).expect("preview");
        assert!(preview.peaks.len() <= MAX_WAVEFORM_POINTS);
        assert!(preview.peaks.iter().any(|peak| *peak > 0.1));
        assert_eq!(preview.sample_rate, PREVIEW_SAMPLE_RATE);
        assert!((preview.duration_seconds - 1.0).abs() < 0.02);
    }

    #[test]
    fn preview_probe_caps_high_rate_multichannel_source() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("wide.wav");
        let spec = WavSpec {
            channels: 8,
            sample_rate: 384_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(&path, spec).expect("create fixture");
        for _ in 0..8 {
            for _ in 0..10 {
                writer.write_sample::<i16>(0).expect("write fixture");
            }
        }
        writer.finalize().expect("finish fixture");

        let seconds = preview_decode_seconds(&path).expect("bounded preview");
        assert!(seconds > 0.5);
        assert!(seconds < 0.7);
    }

    #[test]
    fn preview_probe_rejects_an_impossible_source_rate() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("impossible.wav");
        let spec = WavSpec {
            channels: 8,
            sample_rate: 10_000_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(&path, spec).expect("create fixture");
        for _ in 0..8 {
            writer.write_sample::<i16>(0).expect("write fixture");
        }
        writer.finalize().expect("finish fixture");

        let error = preview_decode_seconds(&path).expect_err("preview must be rejected");
        assert!(error.contains("too many source samples"));
    }

    #[test]
    fn duration_format_is_stable_for_missing_and_long_values() {
        assert_eq!(format_duration(None), "--:--");
        assert_eq!(format_duration(Some(65.4)), "01:05");
        assert_eq!(format_duration(Some(3600.0)), "60:00");
    }
}
