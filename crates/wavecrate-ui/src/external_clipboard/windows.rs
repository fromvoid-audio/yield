use std::path::PathBuf;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::DataExchange::SetClipboardData;
use windows::Win32::System::Ole::{CF_HDROP, DROPEFFECT_COPY};
use windows::core::w;

use super::FileClipboardError;

mod handle;
mod hdrop;

use handle::{ClipboardWriter, OwnedHGlobal};

pub(super) fn copy_file_paths(paths: &[PathBuf]) -> Result<(), FileClipboardError> {
    // Build the mandatory file payload before opening and clearing the
    // clipboard. Allocation failures therefore leave the existing clipboard
    // intact.
    let hdrop = hdrop::create_hdrop(paths).map_err(platform_error)?;

    // The preferred drop effect is compatibility metadata. It must not make
    // an otherwise usable CF_HDROP handoff fail, so prepare it opportunistically.
    let optional_drop_effect = preferred_drop_effect_format().ok().and_then(|format| {
        hdrop::create_drop_effect(DROPEFFECT_COPY.0)
            .ok()
            .map(|payload| (format, payload))
    });

    let _clipboard = ClipboardWriter::new().map_err(platform_error)?;
    publish_file_drop_first(
        || set_clipboard_hglobal(CF_HDROP.0 as u32, hdrop, "CF_HDROP"),
        || {
            if let Some((format, payload)) = optional_drop_effect {
                let _ = set_clipboard_hglobal(format, payload, "Preferred DropEffect");
            }
            Ok(())
        },
    )
}

fn publish_file_drop_first<WriteFile, WriteEffect>(
    write_file: WriteFile,
    write_effect: WriteEffect,
) -> Result<(), FileClipboardError>
where
    WriteFile: FnOnce() -> Result<(), FileClipboardError>,
    WriteEffect: FnOnce() -> Result<(), FileClipboardError>,
{
    // CF_HDROP is the actual file handoff. The optional effect only improves
    // shell copy behavior and cannot invalidate a successful file payload.
    write_file()?;
    let _ = write_effect();
    Ok(())
}

fn set_clipboard_hglobal(
    format: u32,
    payload: OwnedHGlobal,
    label: &str,
) -> Result<(), FileClipboardError> {
    // SAFETY: ClipboardWriter keeps the clipboard open; ownership of this
    // HGLOBAL transfers to Windows only when SetClipboardData succeeds.
    unsafe { SetClipboardData(format, Some(HANDLE(payload.handle().0))) }
        .map_err(|error| platform_error(format!("SetClipboardData({label}) failed: {error}")))?;
    let _ = payload.release();
    Ok(())
}

fn preferred_drop_effect_format() -> Result<u32, String> {
    static FORMAT: std::sync::OnceLock<Result<u32, String>> = std::sync::OnceLock::new();
    FORMAT
        .get_or_init(|| {
            let format = unsafe {
                windows::Win32::System::DataExchange::RegisterClipboardFormatW(w!(
                    "Preferred DropEffect"
                ))
            };
            if format == 0 {
                Err("RegisterClipboardFormatW failed".to_string())
            } else {
                Ok(format)
            }
        })
        .clone()
}

fn platform_error(message: impl Into<String>) -> FileClipboardError {
    FileClipboardError::Platform(message.into())
}

#[cfg(test)]
mod tests {
    use super::{FileClipboardError, publish_file_drop_first};
    use std::cell::RefCell;

    #[test]
    fn file_drop_failure_skips_optional_effect_publication() {
        let calls = RefCell::new(Vec::new());
        let result = publish_file_drop_first(
            || {
                calls.borrow_mut().push("file");
                Err(FileClipboardError::Platform("file failure".into()))
            },
            || {
                calls.borrow_mut().push("effect");
                Ok(())
            },
        );

        assert_eq!(
            result,
            Err(FileClipboardError::Platform("file failure".into()))
        );
        assert_eq!(calls.into_inner(), ["file"]);
    }

    #[test]
    fn optional_effect_failure_keeps_successful_file_handoff() {
        let calls = RefCell::new(Vec::new());
        let result = publish_file_drop_first(
            || {
                calls.borrow_mut().push("file");
                Ok(())
            },
            || {
                calls.borrow_mut().push("effect");
                Err(FileClipboardError::Platform("effect failure".into()))
            },
        );

        assert_eq!(result, Ok(()));
        assert_eq!(calls.into_inner(), ["file", "effect"]);
    }
}
