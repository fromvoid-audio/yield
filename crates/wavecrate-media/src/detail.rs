//! Bounded, file-fenced waveform detail decoding.

use std::{
    fs::{self, File},
    io::BufReader,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::SystemTime,
};

use wavecrate_library::filesystem_identity::{
    filesystem_change_marker, stable_filesystem_identity, stable_filesystem_identity_from_open_file,
};

use crate::{
    is_cancelled, sanitize_stream_sample, stream_wav_frames, visual_bands::StreamingProjector,
};

/// Maximum number of source frames that one detail request may expose.
pub const MAX_WAVEFORM_DETAIL_VISIBLE_FRAMES: usize = 8 * 1024 * 1024;

/// Maximum number of columns returned by one detail request.
pub const MAX_WAVEFORM_DETAIL_COLUMNS: usize = 65_536;

const MAX_WARMUP_FRAMES: usize = 262_144;

/// The WAV sample specification captured with a waveform detail context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaveformSpec {
    /// Number of interleaved channels in the source file.
    pub channels: u16,
    /// Source samples per second.
    pub sample_rate: u32,
    /// Bits used by each stored sample.
    pub bits_per_sample: u16,
    /// Whether samples are integer PCM or IEEE floating point.
    pub sample_format: hound::SampleFormat,
}

impl WaveformSpec {
    pub(super) fn from_hound(spec: hound::WavSpec) -> Self {
        Self {
            channels: spec.channels,
            sample_rate: spec.sample_rate,
            bits_per_sample: spec.bits_per_sample,
            sample_format: spec.sample_format,
        }
    }

    fn is_valid(self) -> bool {
        self.channels > 0 && self.sample_rate > 0 && (1..=32).contains(&self.bits_per_sample)
    }
}

/// Strong filesystem revision markers captured alongside a waveform context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaveformFileRevision {
    /// Stable identity of the opened filesystem object.
    pub filesystem_identity: String,
    /// Platform change marker captured from the named path.
    pub change_marker: String,
    /// Byte length captured with the revision.
    pub byte_len: u64,
    /// Modified time captured when the platform exposes one.
    pub modified_time: Option<SystemTime>,
}

/// Immutable overview metadata and normalization needed to refine one viewport.
#[derive(Clone, Debug, PartialEq)]
pub struct WaveformDetailContext {
    /// Path whose opened file produced this context.
    pub path: PathBuf,
    /// Strong filesystem revision for [`Self::path`].
    pub file_revision: WaveformFileRevision,
    /// WAV specification captured by the overview read.
    pub spec: WaveformSpec,
    /// Exact interleaved frame count captured by the overview read.
    pub total_frames: usize,
    /// Global mono gain used before visual-band processing.
    pub global_mono_gain: f32,
    /// File-level low, mid, and high visual-band gains used by the overview.
    pub visual_band_gains: [f32; 3],
}

/// A non-empty, validated half-open frame range within a waveform context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaveformDetailRange {
    /// First included source frame.
    pub start_frame: usize,
    /// Exclusive end source frame.
    pub end_frame: usize,
}

impl WaveformDetailRange {
    /// Return the number of source frames in this range.
    pub fn len(self) -> usize {
        self.end_frame.saturating_sub(self.start_frame)
    }
}

/// A bounded request to refine a viewport using an overview's normalization context.
///
/// Detail decoding warms the visual processor with up to
/// `min(sample_rate / 4, 262_144)` preceding frames. Those frames affect filter and envelope
/// state but never contribute columns to the requested range.
#[derive(Clone, Debug, PartialEq)]
pub struct WaveformDetailRequest {
    /// Context captured by the overview read.
    pub context: WaveformDetailContext,
    /// Exact source frame range to decode.
    pub range: WaveformDetailRange,
    /// Number of output columns requested for the range.
    ///
    /// Source frames are divided into this many uniform half-open buckets over [`Self::range`].
    pub output_columns: usize,
}

impl WaveformDetailRequest {
    /// Validate and create a detail request for `start_frame..end_frame`.
    pub fn new(
        context: WaveformDetailContext,
        start_frame: usize,
        end_frame: usize,
        output_columns: usize,
    ) -> Result<Self, String> {
        if !context.is_valid() {
            return Err(String::from("waveform detail context is unavailable"));
        }
        if start_frame >= end_frame {
            return Err(String::from("waveform detail range must be non-empty"));
        }
        if end_frame > context.total_frames {
            return Err(String::from("waveform detail range exceeds source frames"));
        }
        let visible_frames = end_frame - start_frame;
        if visible_frames > MAX_WAVEFORM_DETAIL_VISIBLE_FRAMES {
            return Err(String::from("waveform detail range is too large"));
        }
        if output_columns == 0 || output_columns > MAX_WAVEFORM_DETAIL_COLUMNS {
            return Err(String::from(
                "waveform detail column count is out of bounds",
            ));
        }
        if output_columns > visible_frames {
            return Err(String::from(
                "waveform detail column count exceeds visible frames",
            ));
        }
        Ok(Self {
            context,
            range: WaveformDetailRange {
                start_frame,
                end_frame,
            },
            output_columns,
        })
    }

    fn validate(&self) -> Result<(), String> {
        let validated = Self::new(
            self.context.clone(),
            self.range.start_frame,
            self.range.end_frame,
            self.output_columns,
        )?;
        if validated.range != self.range || validated.output_columns != self.output_columns {
            return Err(String::from("waveform detail request is invalid"));
        }
        Ok(())
    }
}

/// Bounded frequency-layer columns decoded for one exact frame range.
///
/// For a range of `visible_frames` and `output_columns`, source frame offset `i` is assigned to
/// `floor(i * output_columns / visible_frames)`. This keeps bucket boundaries deterministic for
/// later viewport mapping.
#[derive(Clone, Debug, PartialEq)]
pub struct WaveformDetailResult {
    /// Overview context that fenced this result.
    pub context: WaveformDetailContext,
    /// Exact source frame range represented by [`Self::bands`].
    pub range: WaveformDetailRange,
    /// Low, mid, high, and raw magnitudes for each output column. Columns cover uniform
    /// half-open frame buckets over [`Self::range`], with the exact source range retained above.
    pub bands: Vec<[f32; 4]>,
}

#[derive(Clone, Debug)]
pub(super) struct FileObservation {
    open_identity: Option<String>,
    path_identity: Option<String>,
    change_marker: Option<String>,
    byte_len: u64,
    modified_time: Option<SystemTime>,
}

impl FileObservation {
    fn strong_revision(&self) -> Option<WaveformFileRevision> {
        let (Some(open_identity), Some(path_identity), Some(change_marker)) = (
            self.open_identity.as_ref(),
            self.path_identity.as_ref(),
            self.change_marker.as_ref(),
        ) else {
            return None;
        };
        (open_identity == path_identity).then(|| WaveformFileRevision {
            filesystem_identity: open_identity.clone(),
            change_marker: change_marker.clone(),
            byte_len: self.byte_len,
            modified_time: self.modified_time,
        })
    }
}

/// A WAV reader whose original descriptor remains retained for identity fencing.
pub(super) struct OpenWav {
    pub(super) file: File,
    pub(super) reader: hound::WavReader<BufReader<File>>,
    pub(super) spec: hound::WavSpec,
    pub(super) total_frames: usize,
    pub(super) observation: FileObservation,
}

pub(super) fn open_wav(path: &Path, cancellation: Option<&AtomicBool>) -> Result<OpenWav, String> {
    if is_cancelled(cancellation) {
        return Err(String::from("waveform preview canceled"));
    }
    let file = File::open(path)
        .map_err(|error| format!("WAV preview failed for {}: {error}", path.display()))?;
    let observation = observe_file(path, &file)?;
    if is_cancelled(cancellation) {
        return Err(String::from("waveform preview canceled"));
    }
    let reader_file = file
        .try_clone()
        .map_err(|error| format!("WAV preview failed to retain {}: {error}", path.display()))?;
    let reader = hound::WavReader::new(BufReader::new(reader_file))
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
    let after_header = observe_file(path, &file)?;
    ensure_file_unchanged(&observation, &after_header)?;
    Ok(OpenWav {
        file,
        reader,
        spec,
        total_frames,
        observation,
    })
}

pub(super) fn observe_file(path: &Path, file: &File) -> Result<FileObservation, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("failed to inspect WAV {}: {error}", path.display()))?;
    let open_identity = stable_filesystem_identity_from_open_file(file);
    let path_identity = stable_filesystem_identity(path, &metadata);
    if let (Some(open_identity), Some(path_identity)) = (&open_identity, &path_identity)
        && open_identity != path_identity
    {
        return Err(format!(
            "waveform source was replaced while opening {}",
            path.display()
        ));
    }
    Ok(FileObservation {
        open_identity,
        path_identity,
        change_marker: filesystem_change_marker(path, &metadata),
        byte_len: metadata.len(),
        modified_time: metadata.modified().ok(),
    })
}

pub(super) fn recheck_wav(
    path: &Path,
    retained_file: &File,
    expected_spec: hound::WavSpec,
    expected_total_frames: usize,
    cancellation: Option<&AtomicBool>,
) -> Result<FileObservation, String> {
    if is_cancelled(cancellation) {
        return Err(String::from("waveform preview canceled"));
    }
    let retained_observation = observe_file(path, retained_file)?;
    let file = File::open(path)
        .map_err(|error| format!("failed to revalidate WAV {}: {error}", path.display()))?;
    let observation = observe_file(path, &file)?;
    let reader_file = file
        .try_clone()
        .map_err(|error| format!("failed to revalidate WAV {}: {error}", path.display()))?;
    let reader = hound::WavReader::new(BufReader::new(reader_file))
        .map_err(|error| format!("failed to revalidate WAV {}: {error}", path.display()))?;
    let actual_spec = reader.spec();
    let actual_total_frames = reader.duration() as usize;
    if actual_spec != expected_spec || actual_total_frames != expected_total_frames {
        return Err(String::from("waveform source changed while reading"));
    }
    if is_cancelled(cancellation) {
        return Err(String::from("waveform preview canceled"));
    }
    let after_header = observe_file(path, &file)?;
    ensure_file_unchanged(&retained_observation, &after_header)?;
    ensure_file_unchanged(&observation, &after_header)?;
    Ok(after_header)
}

pub(super) fn ensure_file_unchanged(
    before: &FileObservation,
    after: &FileObservation,
) -> Result<(), String> {
    if before.byte_len != after.byte_len {
        return Err(String::from("waveform source changed while reading"));
    }
    if optional_changed(&before.open_identity, &after.open_identity)
        || optional_changed(&before.path_identity, &after.path_identity)
        || optional_changed(&before.change_marker, &after.change_marker)
        || optional_changed(&before.modified_time, &after.modified_time)
    {
        return Err(String::from("waveform source changed while reading"));
    }
    Ok(())
}

fn optional_changed<T: PartialEq>(before: &Option<T>, after: &Option<T>) -> bool {
    before
        .as_ref()
        .zip(after.as_ref())
        .is_some_and(|(before, after)| before != after)
}

pub(super) fn context_from_observations(
    path: &Path,
    before: &FileObservation,
    after: &FileObservation,
    spec: hound::WavSpec,
    total_frames: usize,
    global_mono_gain: f32,
    visual_band_gains: [f32; 3],
) -> Option<WaveformDetailContext> {
    ensure_file_unchanged(before, after).ok()?;
    let file_revision = before.strong_revision()?;
    let after_revision = after.strong_revision()?;
    (file_revision == after_revision).then(|| WaveformDetailContext {
        path: path.to_path_buf(),
        file_revision,
        spec: WaveformSpec::from_hound(spec),
        total_frames,
        global_mono_gain,
        visual_band_gains,
    })
}

impl WaveformDetailContext {
    fn is_valid(&self) -> bool {
        !self.path.as_os_str().is_empty()
            && !self.file_revision.filesystem_identity.is_empty()
            && !self.file_revision.change_marker.is_empty()
            && self.total_frames > 0
            && self.total_frames <= u32::MAX as usize
            && self.spec.is_valid()
            && self.global_mono_gain.is_finite()
            && self.global_mono_gain > 0.0
            && self
                .visual_band_gains
                .iter()
                .all(|gain| gain.is_finite() && *gain > 0.0 && *gain <= 4.0)
    }
}

/// Decode one bounded frame range using the gains and revision from its overview.
pub fn load_waveform_detail(
    request: &WaveformDetailRequest,
) -> Result<WaveformDetailResult, String> {
    load_waveform_detail_with_cancel(request, None)
}

/// Decode one bounded frame range while honoring an optional cancellation flag.
pub fn load_waveform_detail_with_cancel(
    request: &WaveformDetailRequest,
    cancellation: Option<&AtomicBool>,
) -> Result<WaveformDetailResult, String> {
    request.validate()?;
    if is_cancelled(cancellation) {
        return Err(String::from("waveform detail canceled"));
    }
    let context = &request.context;
    let mut opened = open_wav(&context.path, cancellation)
        .map_err(|error| format!("WAV detail failed for {}: {error}", context.path.display()))?;
    if WaveformSpec::from_hound(opened.spec) != context.spec
        || opened.total_frames != context.total_frames
    {
        return Err(String::from("stale waveform detail request"));
    }
    if opened.observation.strong_revision().as_ref() != Some(&context.file_revision) {
        return Err(String::from("stale waveform detail request"));
    }
    let range = request.range;
    let visible_frames = range.len();
    let warmup_frames = (opened.spec.sample_rate as usize / 4)
        .min(MAX_WARMUP_FRAMES)
        .min(range.start_frame);
    let warmup_start = range.start_frame - warmup_frames;
    let frames_to_read = warmup_frames
        .checked_add(visible_frames)
        .ok_or_else(|| String::from("waveform detail range is too large"))?;
    opened
        .reader
        .seek(u32::try_from(warmup_start).map_err(|_| {
            String::from("waveform detail range cannot be represented by WAV reader")
        })?)
        .map_err(|error| format!("failed to seek detail WAV: {error}"))?;
    let mut projector = StreamingProjector::new(
        opened.spec.sample_rate,
        visible_frames,
        request.output_columns,
    );
    stream_wav_frames(
        &mut opened.reader,
        opened.spec,
        usize::from(opened.spec.channels),
        frames_to_read,
        cancellation,
        |local_index, mono| {
            let absolute_frame = warmup_start + local_index;
            let normalized = sanitize_stream_sample(mono * context.global_mono_gain);
            if absolute_frame < range.start_frame {
                projector.warmup(normalized);
            } else {
                projector.push(absolute_frame - range.start_frame, normalized);
            }
            Ok(())
        },
    )?;
    if is_cancelled(cancellation) {
        return Err(String::from("waveform detail canceled"));
    }
    let after = recheck_wav(
        &context.path,
        &opened.file,
        opened.spec,
        opened.total_frames,
        cancellation,
    )?;
    ensure_file_unchanged(&opened.observation, &after)
        .map_err(|_| String::from("stale waveform detail result"))?;
    if after.strong_revision().as_ref() != Some(&context.file_revision) {
        return Err(String::from("stale waveform detail result"));
    }
    let bands = projector.finish_with_gains(context.visual_band_gains);
    if bands.len() != request.output_columns {
        return Err(String::from("waveform detail projection was incomplete"));
    }
    Ok(WaveformDetailResult {
        context: request.context.clone(),
        range,
        bands,
    })
}
