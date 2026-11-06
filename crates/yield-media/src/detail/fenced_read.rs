use std::{
    fs::{self, File},
    io,
    path::Path,
    sync::atomic::AtomicBool,
    time::Instant,
};

use super::{WaveformDetailContext, ensure_file_unchanged, observe_file};

#[cfg(windows)]
const MAX_FINGERPRINT_BYTES: u64 = 8 * 1024 * 1024;

/// Run one bounded background read against the WAV represented by a waveform revision.
///
/// The operation receives the retained read-only file positioned at the start. The caller
/// bounds its own I/O and checks the shared deadline and cancellation during the operation.
/// Results are returned only if the selected object and its revision still match afterward.
/// On Windows, the deny-write lease is retained while validating the captured fingerprint,
/// with an 8 MiB input ceiling for that validation.
pub fn with_fenced_wav_read<T>(
    context: &WaveformDetailContext,
    cancellation: &AtomicBool,
    deadline: Instant,
    read: impl FnOnce(&mut File) -> Result<T, String>,
) -> Result<T, String> {
    let checkpoint = || {
        if crate::is_cancelled(Some(cancellation)) {
            Err(String::from("WAV preparation canceled"))
        } else if Instant::now() >= deadline {
            Err(String::from("WAV preparation deadline exceeded"))
        } else {
            Ok(())
        }
    };
    checkpoint()?;
    if !context.is_valid() {
        return Err(String::from(
            "WAV preparation requires a strong waveform revision",
        ));
    }
    #[cfg(windows)]
    if context.file_revision.byte_len > MAX_FINGERPRINT_BYTES {
        return Err(String::from(
            "WAV preparation exceeds the fingerprint budget",
        ));
    }

    let path = &context.path;
    require_regular_path(path)?;
    let mut file = open_regular_candidate(path).map_err(|error| error.to_string())?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err(String::from("WAV preparation requires a regular file"));
    }

    #[allow(unused_mut)]
    let mut before = observe_file(path, &file)?;
    let identity = Some(context.file_revision.filesystem_identity.as_str());
    if before.open_identity.as_deref() != identity
        || before.path_identity.as_deref() != identity
        || before.byte_len() != context.file_revision.byte_len
    {
        return Err(String::from("WAV preparation source revision changed"));
    }
    checkpoint()?;
    #[cfg(windows)]
    {
        before.content_fingerprint = Some(super::fingerprint_open_source_with_checkpoint(
            &mut file,
            Some(context.file_revision.byte_len),
            checkpoint,
        )?);
    }
    if before.strong_revision().as_ref() != Some(&context.file_revision) {
        return Err(String::from("WAV preparation source revision changed"));
    }

    checkpoint()?;
    let result = read(&mut file)?;
    checkpoint()?;
    require_regular_path(path)?;
    #[allow(unused_mut)]
    let mut after = observe_file(path, &file)?;
    ensure_file_unchanged(&before, &after)?;
    #[cfg(windows)]
    {
        // The retained handle denies writes, and the independent observation still identifies it.
        after.content_fingerprint = before.content_fingerprint.clone();
    }
    if after.strong_revision().as_ref() != Some(&context.file_revision) {
        return Err(String::from("WAV preparation source revision changed"));
    }
    checkpoint()?;
    Ok(result)
}

fn require_regular_path(path: &Path) -> Result<(), String> {
    if !fs::symlink_metadata(path)
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err(String::from("WAV preparation requires a regular file"));
    }
    Ok(())
}

#[cfg(unix)]
fn open_regular_candidate(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_regular_candidate(path: &Path) -> io::Result<File> {
    super::open_waveform_source(path)
}

#[cfg(test)]
mod tests;
