use std::{
    collections::BTreeMap,
    fs,
    io::{BufReader, Read, Seek, SeekFrom, Take},
    path::{Path, PathBuf},
};

use wavecrate_library::sample_sources::db::SourceManifestEntry;
use wavecrate_library::sample_sources::reconciliation::RootIdentity;

use super::scan::ScanError;
use super::scan_capability::{SourcePathBinding, SourceRootCapability};
use super::scan_fs::read_facts_from_open_file;

const WAV_HEADER_READ_LIMIT: u64 = 1024 * 1024;
const WAV_METADATA_BYTE_BUDGET: u64 = 64 * 1024 * 1024;
const WAV_METADATA_ENTRY_BUDGET: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeaderReadStop {
    Budget,
    Canceled,
}

struct MetadataReadBudget {
    remaining_bytes: u64,
    remaining_entries: usize,
    consumed_bytes: u64,
    stop: Option<HeaderReadStop>,
}

impl Default for MetadataReadBudget {
    fn default() -> Self {
        Self::new(WAV_METADATA_BYTE_BUDGET, WAV_METADATA_ENTRY_BUDGET)
    }
}

impl MetadataReadBudget {
    fn new(bytes: u64, entries: usize) -> Self {
        Self {
            remaining_bytes: bytes,
            remaining_entries: entries,
            consumed_bytes: 0,
            stop: None,
        }
    }

    fn admit_entry(&mut self) -> bool {
        if self.remaining_entries == 0 {
            return false;
        }
        self.remaining_entries -= 1;
        true
    }

    fn stop(&mut self, reason: HeaderReadStop) {
        self.stop.get_or_insert(reason);
    }
}

struct HeaderBudgetReader<'a, F> {
    inner: Take<fs::File>,
    budget: &'a mut MetadataReadBudget,
    should_cancel: &'a mut F,
    position: u64,
}

impl<'a, F: FnMut() -> bool> HeaderBudgetReader<'a, F> {
    fn new(
        inner: Take<fs::File>,
        budget: &'a mut MetadataReadBudget,
        should_cancel: &'a mut F,
    ) -> Self {
        Self {
            inner,
            budget,
            should_cancel,
            position: 0,
        }
    }
}

impl<F: FnMut() -> bool> Read for HeaderBudgetReader<'_, F> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.budget.stop.is_some() {
            return Ok(0);
        }
        if (self.should_cancel)() {
            self.budget.stop(HeaderReadStop::Canceled);
            return Ok(0);
        }
        if self.budget.remaining_bytes == 0 {
            self.budget.stop(HeaderReadStop::Budget);
            return Ok(0);
        }
        let request = buffer.len().min(
            usize::try_from(self.budget.remaining_bytes)
                .ok()
                .unwrap_or(usize::MAX),
        );
        let read = self.inner.read(&mut buffer[..request])?;
        if read > 0 {
            self.budget.remaining_bytes -= read as u64;
            self.budget.consumed_bytes += read as u64;
            self.position += read as u64;
        }
        Ok(read)
    }
}

/// Header metadata used by catalog consumers to display a WAV duration and sample rate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WavDisplayMetadata {
    /// Number of interleaved audio frames in the WAV data chunk.
    pub frames: u32,
    /// WAV sample rate in samples per second.
    pub sample_rate: u32,
}

/// Read bounded WAV header metadata for an already committed source manifest.
///
/// The helper opens only paths present in `manifest` through a retained source-root capability.
/// Each descriptor is checked against its committed size, modification time, and identity both
/// before and after the header read. A changed, missing, malformed, or non-regular entry is
/// omitted from the returned map so the manifest membership remains authoritative. A changed
/// source-root identity or a cancellation stops the whole operation. Header reads are bounded to
/// 64 MiB and 4096 manifest entries per call; exhausting either budget returns the coherent
/// metadata collected so far.
pub fn read_wav_display_metadata_for_manifest(
    root: &Path,
    expected_root_identity: &RootIdentity,
    manifest: &[SourceManifestEntry],
    mut should_cancel: impl FnMut() -> bool,
) -> Result<BTreeMap<PathBuf, WavDisplayMetadata>, ScanError> {
    let mut budget = MetadataReadBudget::default();
    read_wav_display_metadata_for_manifest_with_budget(
        root,
        expected_root_identity,
        manifest,
        &mut should_cancel,
        &mut budget,
    )
}

fn read_wav_display_metadata_for_manifest_with_budget<F: FnMut() -> bool>(
    root: &Path,
    expected_root_identity: &RootIdentity,
    manifest: &[SourceManifestEntry],
    should_cancel: &mut F,
    budget: &mut MetadataReadBudget,
) -> Result<BTreeMap<PathBuf, WavDisplayMetadata>, ScanError> {
    if should_cancel() {
        return Err(ScanError::Canceled);
    }
    let source_root = SourceRootCapability::open(root)?;
    ensure_expected_generation(&source_root, root, expected_root_identity)?;

    let mut metadata = BTreeMap::new();
    for entry in manifest {
        if !budget.admit_entry() {
            break;
        }
        if should_cancel() {
            return Err(ScanError::Canceled);
        }
        if !is_wav_path(&entry.relative_path) {
            continue;
        }
        let Some(file) = open_manifest_file(&source_root, &entry.relative_path) else {
            continue;
        };
        let absolute_path = root.join(&entry.relative_path);
        let before = match read_facts_from_open_file(root, &absolute_path, &file) {
            Ok(facts) if facts.matches_manifest_entry(entry) => facts,
            Ok(_) | Err(ScanError::Io { .. }) | Err(ScanError::InvalidRoot(_)) => continue,
            Err(error) => return Err(error),
        };
        ensure_expected_generation(&source_root, root, expected_root_identity)?;
        if should_cancel() {
            return Err(ScanError::Canceled);
        }

        let wav_metadata = match read_wav_header(&file, budget, should_cancel) {
            Ok(Some(metadata)) => metadata,
            Ok(None) => continue,
            Err(HeaderReadStop::Budget) => break,
            Err(HeaderReadStop::Canceled) => return Err(ScanError::Canceled),
        };

        if should_cancel() {
            return Err(ScanError::Canceled);
        }
        let after = match read_facts_from_open_file(root, &absolute_path, &file) {
            Ok(facts) if facts.matches_manifest_entry(entry) => facts,
            Ok(_) | Err(ScanError::Io { .. }) | Err(ScanError::InvalidRoot(_)) => continue,
            Err(error) => return Err(error),
        };
        if !before.same_content_snapshot(&after) {
            continue;
        }
        if !matches!(
            source_root.path_binding(&entry.relative_path, &file),
            Ok(SourcePathBinding::Matches)
        ) {
            continue;
        }
        ensure_expected_generation(&source_root, root, expected_root_identity)?;
        metadata.insert(entry.relative_path.clone(), wav_metadata);
    }

    if should_cancel() {
        return Err(ScanError::Canceled);
    }
    ensure_expected_generation(&source_root, root, expected_root_identity)?;
    Ok(metadata)
}

fn ensure_expected_generation(
    source_root: &SourceRootCapability,
    root: &Path,
    expected_root_identity: &RootIdentity,
) -> Result<(), ScanError> {
    if source_root.generation().as_bytes() != expected_root_identity.as_bytes() {
        return Err(ScanError::StaleRootGeneration {
            root: root.to_path_buf(),
        });
    }
    source_root.ensure_current_generation()
}

fn open_manifest_file(
    source_root: &SourceRootCapability,
    relative_path: &Path,
) -> Option<fs::File> {
    match source_root.open_regular_file(relative_path) {
        Ok(file) => file,
        Err(ScanError::Io { .. } | ScanError::InvalidRoot(_)) => None,
        Err(_) => None,
    }
}

fn is_wav_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
}

fn read_wav_header<F: FnMut() -> bool>(
    file: &fs::File,
    budget: &mut MetadataReadBudget,
    should_cancel: &mut F,
) -> Result<Option<WavDisplayMetadata>, HeaderReadStop> {
    let Some((data_start, data_len)) = read_data_chunk_bounds(file, budget, should_cancel)? else {
        return Ok(None);
    };
    let Some(mut bounded_file) = file.try_clone().ok() else {
        return Ok(None);
    };
    if bounded_file.seek(SeekFrom::Start(0)).is_err() {
        return Ok(None);
    }
    let bounded = bounded_file.take(WAV_HEADER_READ_LIMIT);
    let parsed = hound::WavReader::new(BufReader::new(HeaderBudgetReader::new(
        bounded,
        budget,
        should_cancel,
    )))
    .map(|reader| (reader.spec(), reader.duration()));
    let (spec, frames) = match parsed {
        Ok(parsed) => parsed,
        Err(_) => {
            return match budget.stop {
                Some(stop) => Err(stop),
                None => Ok(None),
            };
        }
    };
    if let Some(stop) = budget.stop {
        return Err(stop);
    }
    if spec.channels == 0 || spec.sample_rate == 0 {
        return Ok(None);
    }
    let Some(file_len) = file.metadata().ok().map(|metadata| metadata.len()) else {
        return Ok(None);
    };
    let Some(data_end) = data_start.checked_add(u64::from(data_len)) else {
        return Ok(None);
    };
    if file_len < data_end {
        return Ok(None);
    }
    Ok(Some(WavDisplayMetadata {
        frames,
        sample_rate: spec.sample_rate,
    }))
}

fn read_data_chunk_bounds<F: FnMut() -> bool>(
    file: &fs::File,
    budget: &mut MetadataReadBudget,
    should_cancel: &mut F,
) -> Result<Option<(u64, u32)>, HeaderReadStop> {
    let Some(mut bounded_file) = file.try_clone().ok() else {
        return Ok(None);
    };
    if bounded_file.seek(SeekFrom::Start(0)).is_err() {
        return Ok(None);
    }
    let bounded = bounded_file.take(WAV_HEADER_READ_LIMIT);
    let mut reader = HeaderBudgetReader::new(bounded, budget, should_cancel);
    let mut riff_header = [0_u8; 12];
    if reader.read_exact(&mut riff_header).is_err() {
        return match reader.budget.stop {
            Some(stop) => Err(stop),
            None => Ok(None),
        };
    }
    if &riff_header[..4] != b"RIFF" || &riff_header[8..] != b"WAVE" {
        return Ok(None);
    }

    loop {
        let mut chunk_header = [0_u8; 8];
        if reader.read_exact(&mut chunk_header).is_err() {
            return match reader.budget.stop {
                Some(stop) => Err(stop),
                None => Ok(None),
            };
        }
        let Ok(chunk_len_bytes) = chunk_header[4..].try_into() else {
            return Ok(None);
        };
        let chunk_len = u32::from_le_bytes(chunk_len_bytes);
        if &chunk_header[..4] == b"data" {
            return Ok(Some((reader.position, chunk_len)));
        }
        if skip_header_bytes(&mut reader, u64::from(chunk_len)).is_none() {
            return match reader.budget.stop {
                Some(stop) => Err(stop),
                None => Ok(None),
            };
        }
        if chunk_len % 2 != 0 {
            if skip_header_bytes(&mut reader, 1).is_none() {
                return match reader.budget.stop {
                    Some(stop) => Err(stop),
                    None => Ok(None),
                };
            }
        }
    }
}

fn skip_header_bytes(reader: &mut impl Read, mut remaining: u64) -> Option<()> {
    let mut buffer = [0_u8; 8192];
    while remaining > 0 {
        let request = remaining.min(buffer.len() as u64) as usize;
        let read = reader.read(&mut buffer[..request]).ok()?;
        if read == 0 {
            return None;
        }
        remaining -= read as u64;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::FileTime;
    use std::cell::Cell;
    use wavecrate_library::filesystem_identity::stable_filesystem_identity;
    use wavecrate_library::timestamps::system_time_to_unix_nanos;

    fn root_identity(root: &Path) -> RootIdentity {
        let metadata = fs::metadata(root).unwrap();
        RootIdentity::from_bytes(
            stable_filesystem_identity(root, &metadata)
                .unwrap()
                .into_bytes(),
        )
    }

    fn write_wav(path: &Path, channels: u16, sample_rate: u32, frames: u32) {
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for frame in 0..frames {
            for channel in 0..channels {
                writer
                    .write_sample((frame as i16).wrapping_add(channel as i16))
                    .unwrap();
            }
        }
        writer.finalize().unwrap();
    }

    fn manifest_entry(root: &Path, relative_path: &Path) -> SourceManifestEntry {
        let path = root.join(relative_path);
        let metadata = fs::metadata(&path).unwrap();
        SourceManifestEntry {
            relative_path: relative_path.to_path_buf(),
            file_identity: stable_filesystem_identity(&path, &metadata),
            content_hash: None,
            file_size: metadata.len(),
            modified_ns: system_time_to_unix_nanos(metadata.modified().unwrap()),
        }
    }

    fn read_metadata(
        root: &Path,
        manifest: &[SourceManifestEntry],
    ) -> Result<BTreeMap<PathBuf, WavDisplayMetadata>, ScanError> {
        let identity = root_identity(root);
        read_wav_display_metadata_for_manifest(root, &identity, manifest, || false)
    }

    #[test]
    fn reads_mono_and_stereo_duration_from_headers() {
        let source = tempfile::tempdir().unwrap();
        write_wav(&source.path().join("mono.wav"), 1, 8_000, 3);
        write_wav(&source.path().join("stereo.wav"), 2, 44_100, 5);
        let manifest = vec![
            manifest_entry(source.path(), Path::new("mono.wav")),
            manifest_entry(source.path(), Path::new("stereo.wav")),
        ];

        let metadata = read_metadata(source.path(), &manifest).unwrap();
        assert_eq!(
            metadata.get(Path::new("mono.wav")),
            Some(&WavDisplayMetadata {
                frames: 3,
                sample_rate: 8_000,
            })
        );
        assert_eq!(
            metadata.get(Path::new("stereo.wav")),
            Some(&WavDisplayMetadata {
                frames: 5,
                sample_rate: 44_100,
            })
        );
    }

    #[test]
    fn malformed_and_truncated_wav_headers_are_omitted() {
        let source = tempfile::tempdir().unwrap();
        let malformed = source.path().join("malformed.wav");
        fs::write(&malformed, b"not a wave").unwrap();
        let truncated = source.path().join("truncated.wav");
        write_wav(&truncated, 1, 16_000, 4);
        let length = fs::metadata(&truncated).unwrap().len();
        fs::OpenOptions::new()
            .write(true)
            .open(&truncated)
            .unwrap()
            .set_len(length - 1)
            .unwrap();
        let manifest = vec![
            manifest_entry(source.path(), Path::new("malformed.wav")),
            manifest_entry(source.path(), Path::new("truncated.wav")),
        ];

        assert!(read_metadata(source.path(), &manifest).unwrap().is_empty());
    }

    #[test]
    fn oversized_unknown_chunks_are_rejected_with_a_bounded_header_read() {
        let source = tempfile::tempdir().unwrap();
        let valid = source.path().join("valid.wav");
        write_wav(&valid, 1, 16_000, 4);
        let mut bytes = fs::read(&valid).unwrap();
        let mut oversized_chunk = b"JUNK".to_vec();
        oversized_chunk
            .extend_from_slice(&u32::try_from(WAV_HEADER_READ_LIMIT).unwrap().to_le_bytes());
        oversized_chunk.extend(std::iter::repeat_n(
            0,
            usize::try_from(WAV_HEADER_READ_LIMIT).unwrap(),
        ));
        bytes.splice(12..12, oversized_chunk);
        fs::write(&valid, bytes).unwrap();
        let manifest = vec![manifest_entry(source.path(), Path::new("valid.wav"))];

        assert!(read_metadata(source.path(), &manifest).unwrap().is_empty());
    }

    #[test]
    fn data_bounds_honor_odd_unknown_chunk_padding() {
        let source = tempfile::tempdir().unwrap();
        let valid = source.path().join("valid.wav");
        write_wav(&valid, 1, 16_000, 4);
        let mut bytes = fs::read(&valid).unwrap();
        let mut odd_chunk = b"JUNK".to_vec();
        odd_chunk.extend_from_slice(&1_u32.to_le_bytes());
        odd_chunk.extend_from_slice(&[0x5a, 0]);
        bytes.splice(12..12, odd_chunk);
        let riff_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) + 10;
        bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
        fs::write(&valid, bytes).unwrap();

        let file = fs::File::open(&valid).unwrap();
        let mut budget = MetadataReadBudget::default();
        let mut should_cancel = || false;
        assert_eq!(
            read_data_chunk_bounds(&file, &mut budget, &mut should_cancel),
            Ok(Some((54, 8)))
        );
    }

    #[test]
    fn metadata_budget_stops_across_oversized_headers_without_changing_manifest() {
        let source = tempfile::tempdir().unwrap();
        let valid = source.path().join("oversized.wav");
        write_wav(&valid, 1, 16_000, 4);
        let mut bytes = fs::read(&valid).unwrap();
        let mut oversized_chunk = b"JUNK".to_vec();
        oversized_chunk
            .extend_from_slice(&u32::try_from(WAV_HEADER_READ_LIMIT).unwrap().to_le_bytes());
        oversized_chunk.extend(std::iter::repeat_n(
            0,
            usize::try_from(WAV_HEADER_READ_LIMIT).unwrap(),
        ));
        bytes.splice(12..12, oversized_chunk);
        fs::write(&valid, bytes).unwrap();
        let entry = manifest_entry(source.path(), Path::new("oversized.wav"));
        let manifest = vec![entry.clone(), entry.clone(), entry];
        let identity = root_identity(source.path());
        let mut budget = MetadataReadBudget::new(WAV_HEADER_READ_LIMIT * 2, 16);
        let mut should_cancel = || false;

        let metadata = read_wav_display_metadata_for_manifest_with_budget(
            source.path(),
            &identity,
            &manifest,
            &mut should_cancel,
            &mut budget,
        )
        .unwrap();

        assert!(metadata.is_empty());
        assert_eq!(manifest.len(), 3);
        assert_eq!(budget.remaining_bytes, 0);
        assert_eq!(budget.consumed_bytes, WAV_HEADER_READ_LIMIT * 2);
    }

    #[test]
    fn entry_budget_stops_before_examining_the_remaining_manifest() {
        let source = tempfile::tempdir().unwrap();
        let path = source.path().join("note.txt");
        fs::write(&path, b"not audio").unwrap();
        let entry = manifest_entry(source.path(), Path::new("note.txt"));
        let manifest = vec![entry.clone(), entry.clone(), entry];
        let identity = root_identity(source.path());
        let mut budget = MetadataReadBudget::new(WAV_METADATA_BYTE_BUDGET, 2);
        let mut should_cancel = || false;

        let metadata = read_wav_display_metadata_for_manifest_with_budget(
            source.path(),
            &identity,
            &manifest,
            &mut should_cancel,
            &mut budget,
        )
        .unwrap();

        assert!(metadata.is_empty());
        assert_eq!(manifest.len(), 3);
        assert_eq!(budget.remaining_entries, 0);
    }

    #[test]
    fn cancellation_during_header_skip_stops_the_whole_metadata_read() {
        let source = tempfile::tempdir().unwrap();
        let valid = source.path().join("cancel.wav");
        write_wav(&valid, 1, 16_000, 4);
        let mut bytes = fs::read(&valid).unwrap();
        let mut large_chunk = b"JUNK".to_vec();
        large_chunk.extend_from_slice(&16_384_u32.to_le_bytes());
        large_chunk.extend(std::iter::repeat_n(0, 16_384));
        bytes.splice(12..12, large_chunk);
        fs::write(&valid, bytes).unwrap();
        let manifest = vec![manifest_entry(source.path(), Path::new("cancel.wav"))];
        let identity = root_identity(source.path());
        let calls = Cell::new(0usize);

        let result =
            read_wav_display_metadata_for_manifest(source.path(), &identity, &manifest, || {
                let call = calls.get();
                calls.set(call + 1);
                call >= 5
            });

        assert!(matches!(result, Err(ScanError::Canceled)));
        assert!(calls.get() >= 6);
    }

    #[cfg(unix)]
    #[test]
    fn same_size_replacement_with_restored_timestamp_is_omitted() {
        use std::fs::rename;

        let source = tempfile::tempdir().unwrap();
        let target = source.path().join("same.wav");
        write_wav(&target, 1, 8_000, 3);
        let old_mtime = FileTime::from_last_modification_time(&fs::metadata(&target).unwrap());
        let manifest = vec![manifest_entry(source.path(), Path::new("same.wav"))];
        let replacement = source.path().join("replacement.wav");
        write_wav(&replacement, 1, 8_000, 3);
        filetime::set_file_mtime(&replacement, old_mtime).unwrap();
        rename(replacement, target).unwrap();

        assert!(read_metadata(source.path(), &manifest).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn same_size_in_place_mutation_with_restored_timestamp_is_omitted() {
        use std::io::{Seek, SeekFrom, Write};

        let source = tempfile::tempdir().unwrap();
        let target = source.path().join("same.wav");
        write_wav(&target, 1, 8_000, 3);
        let old_mtime = FileTime::from_last_modification_time(&fs::metadata(&target).unwrap());
        let manifest = vec![manifest_entry(source.path(), Path::new("same.wav"))];

        let identity = root_identity(source.path());
        let calls = Cell::new(0usize);
        let result =
            read_wav_display_metadata_for_manifest(source.path(), &identity, &manifest, || {
                let call = calls.get();
                calls.set(call + 1);
                if call == 2 {
                    let mut file = fs::OpenOptions::new().write(true).open(&target).unwrap();
                    file.seek(SeekFrom::End(-1)).unwrap();
                    file.write_all(&[0x7f]).unwrap();
                    file.sync_all().unwrap();
                    filetime::set_file_mtime(&target, old_mtime).unwrap();
                }
                false
            })
            .unwrap();

        assert!(result.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn replaced_ancestor_is_omitted_without_following_the_link() {
        use std::os::unix::fs::symlink;

        let source = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let nested = source.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let target = nested.join("sample.wav");
        write_wav(&target, 1, 8_000, 2);
        let manifest = vec![manifest_entry(
            source.path(),
            Path::new("nested/sample.wav"),
        )];
        write_wav(&outside.path().join("sample.wav"), 1, 8_000, 9);
        fs::remove_dir_all(&nested).unwrap();
        symlink(outside.path(), &nested).unwrap();

        assert!(read_metadata(source.path(), &manifest).unwrap().is_empty());
    }

    #[test]
    fn root_replacement_fails_the_whole_metadata_snapshot() {
        let source = tempfile::tempdir().unwrap();
        write_wav(&source.path().join("sample.wav"), 1, 8_000, 2);
        let manifest = vec![manifest_entry(source.path(), Path::new("sample.wav"))];
        let expected = root_identity(source.path());
        let old_root = source.path().with_extension("old");
        fs::rename(source.path(), &old_root).unwrap();
        fs::create_dir(source.path()).unwrap();

        let result =
            read_wav_display_metadata_for_manifest(source.path(), &expected, &manifest, || false);
        assert!(matches!(result, Err(ScanError::StaleRootGeneration { .. })));
        fs::remove_dir_all(old_root).unwrap();
    }

    #[test]
    fn cancellation_is_checked_before_each_manifest_entry() {
        let source = tempfile::tempdir().unwrap();
        write_wav(&source.path().join("one.wav"), 1, 8_000, 2);
        write_wav(&source.path().join("two.wav"), 1, 8_000, 2);
        let manifest = vec![
            manifest_entry(source.path(), Path::new("one.wav")),
            manifest_entry(source.path(), Path::new("two.wav")),
        ];
        let calls = Cell::new(0usize);
        let identity = root_identity(source.path());
        let result =
            read_wav_display_metadata_for_manifest(source.path(), &identity, &manifest, || {
                let count = calls.get();
                calls.set(count + 1);
                count >= 3
            });
        assert!(matches!(result, Err(ScanError::Canceled)));
        assert!(calls.get() >= 3);
    }

    #[test]
    fn non_wav_manifest_membership_is_ignored_without_a_probe() {
        let source = tempfile::tempdir().unwrap();
        let path = source.path().join("note.txt");
        fs::write(&path, b"not audio").unwrap();
        let manifest = vec![manifest_entry(source.path(), Path::new("note.txt"))];

        assert!(read_metadata(source.path(), &manifest).unwrap().is_empty());
    }

    #[test]
    fn missing_manifest_identity_cannot_admit_wav_metadata() {
        let source = tempfile::tempdir().unwrap();
        let path = source.path().join("sample.wav");
        write_wav(&path, 1, 8_000, 2);
        let mut entry = manifest_entry(source.path(), Path::new("sample.wav"));
        entry.file_identity = None;

        assert!(read_metadata(source.path(), &[entry]).unwrap().is_empty());
    }
}
