use super::*;
use crate::load_waveform_preview;
use hound::{SampleFormat, WavSpec, WavWriter};
use std::{io::Read, sync::atomic::Ordering, time::Duration};

fn deadline() -> Instant {
    Instant::now() + Duration::from_millis(500)
}

fn write_wav(path: &Path, sample: i16) {
    let mut writer = WavWriter::create(
        path,
        WavSpec {
            channels: 1,
            sample_rate: 12_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        },
    )
    .unwrap();
    for _ in 0..64 {
        writer.write_sample(sample).unwrap();
    }
    writer.finalize().unwrap();
}

fn context(path: &Path) -> WaveformDetailContext {
    load_waveform_preview(path).unwrap().detail_context.unwrap()
}

#[test]
fn fenced_wav_returns_a_result_only_for_the_captured_revision() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.wav");
    write_wav(&path, 1_000);
    let captured = context(&path);
    let cancel = AtomicBool::new(false);
    let header = with_fenced_wav_read(&captured, &cancel, deadline(), |source| {
        let mut header = [0; 4];
        source
            .read_exact(&mut header)
            .map_err(|err| err.to_string())?;
        Ok(header)
    })
    .unwrap();
    assert_eq!(&header, b"RIFF");

    let replacement = root.path().join("replacement.wav");
    write_wav(&replacement, 2_000);
    fs::rename(&replacement, &path).unwrap();
    let result = with_fenced_wav_read(&captured, &cancel, deadline(), |_| {
        panic!("must reject a different source before invoking the read")
    });
    assert!(matches!(result, Err::<(), _>(error) if error.contains("revision changed")));
}

#[test]
fn fenced_wav_discards_a_successful_read_after_path_replacement() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.wav");
    let replacement = root.path().join("replacement.wav");
    write_wav(&path, 1_000);
    write_wav(&replacement, 2_000);
    let captured = context(&path);
    let result = with_fenced_wav_read(&captured, &AtomicBool::new(false), deadline(), |source| {
        let mut header = [0; 4];
        source.read_exact(&mut header).unwrap();
        assert_eq!(&header, b"RIFF");
        fs::rename(&replacement, &path).unwrap();
        Ok(())
    });
    assert!(matches!(result, Err(error) if error.contains("replaced")));
}

#[cfg(unix)]
#[test]
fn fenced_wav_rejects_same_length_changes_even_when_mtime_is_restored() {
    use std::io::{Seek, SeekFrom, Write};

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.wav");
    write_wav(&path, 1_000);
    let cancel = AtomicBool::new(false);
    let captured = context(&path);
    let original_mtime = captured.file_revision.modified_time.unwrap();
    let result = with_fenced_wav_read(&captured, &cancel, deadline(), |_| {
        let mut writer = File::options().write(true).open(&path).unwrap();
        writer.seek(SeekFrom::End(-2)).unwrap();
        writer.write_all(&2_000_i16.to_le_bytes()).unwrap();
        writer.set_modified(original_mtime).unwrap();
        Ok(())
    });
    assert!(matches!(result, Err(error) if error.contains("changed")));
    let stale = with_fenced_wav_read(&captured, &cancel, deadline(), |_| {
        panic!("must reject a changed revision before invoking the read")
    });
    assert!(matches!(stale, Err::<(), _>(error) if error.contains("revision changed")));
}

#[test]
fn fenced_wav_rejects_missing_revision_expiry_and_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.wav");
    write_wav(&path, 1_000);
    let captured = context(&path);
    let cancel = AtomicBool::new(false);
    let mut unidentified = captured.clone();
    unidentified.file_revision.filesystem_identity.clear();
    assert!(
        with_fenced_wav_read(
            &unidentified,
            &cancel,
            deadline(),
            |_| -> Result<(), String> { panic!("the worker must not start without a revision") }
        )
        .unwrap_err()
        .contains("strong waveform revision")
    );
    assert!(
        with_fenced_wav_read(
            &captured,
            &cancel,
            Instant::now(),
            |_| -> Result<(), String> { panic!("the worker must not start after expiry") }
        )
        .unwrap_err()
        .contains("deadline exceeded")
    );
    assert!(
        with_fenced_wav_read(&captured, &cancel, deadline(), |_| {
            cancel.store(true, Ordering::Release);
            Ok(())
        })
        .unwrap_err()
        .contains("canceled")
    );
}

#[cfg(windows)]
#[test]
fn fenced_wav_rejects_a_windows_fingerprint_over_budget() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.wav");
    write_wav(&path, 1_000);
    let mut captured = context(&path);
    captured.file_revision.byte_len = MAX_FINGERPRINT_BYTES + 1;
    assert!(
        with_fenced_wav_read(&captured, &AtomicBool::new(false), deadline(), |_| {
            panic!("fingerprint budget must be checked before decoding")
        })
        .map(|_: ()| ())
        .unwrap_err()
        .contains("fingerprint budget")
    );
}
