//! Windows implementation of the bounded outbound file drag.

mod com_apartment;
mod data_object;
mod drop_source;
mod formats;
mod hglobal_payload;

use crate::utf16_paths::{Utf16PathError, collect_bounded_utf16_paths};
use crate::{FileDragCompletion, FileDragError, MAX_FILE_DRAG_FILES, MAX_FILE_DRAG_PATH_BYTES};
use com_apartment::ComApartment;
use data_object::FileDropDataObject;
use drop_source::FileDropSource;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use windows::Win32::Foundation::DRAGDROP_S_CANCEL;
use windows::Win32::System::Ole::{DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};

thread_local! {
    static ACTIVE_DRAG: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Starts a synchronous Windows copy-only file drag on the calling thread.
pub(super) fn start_file_drag_blocking(
    paths: &[PathBuf],
    cancellation: Arc<AtomicBool>,
) -> Result<FileDragCompletion, FileDragError> {
    let encoded_paths = validate_paths(paths)?;
    if cancellation.load(Ordering::Acquire) {
        return Ok(FileDragCompletion::Cancelled);
    }

    let button_state = current_left_button_state();
    if !left_button_is_pressed(button_state) {
        return Err(FileDragError::PrimaryButtonNotPressed {
            mask: button_state as u16 as u64,
        });
    }
    if cancellation.load(Ordering::Acquire) {
        return Ok(FileDragCompletion::Cancelled);
    }
    let _active = ActiveDragGuard::acquire()?;

    // OleInitialize must run on this foreground/UI thread. DoDragDrop enters
    // a nested OLE message loop and must not be moved to a worker thread.
    let _apartment = ComApartment::new().map_err(|error| FileDragError::WindowsApi {
        operation: "OleInitialize",
        code: error.code().0,
    })?;

    if cancellation.load(Ordering::Acquire) {
        return Ok(FileDragCompletion::Cancelled);
    }
    let button_state = current_left_button_state();
    if !left_button_is_pressed(button_state) {
        return Err(FileDragError::PrimaryButtonNotPressed {
            mask: button_state as u16 as u64,
        });
    }

    // The COM objects own only the copied path data and cancellation token;
    // neither object retains a window handle or any GPUI state.
    let data_object: windows::Win32::System::Com::IDataObject =
        FileDropDataObject::new(encoded_paths).into();
    let drop_source: windows::Win32::System::Ole::IDropSource =
        FileDropSource::new(cancellation).into();
    let mut effect = DROPEFFECT_NONE;
    let result = unsafe { DoDragDrop(&data_object, &drop_source, DROPEFFECT_COPY, &mut effect) };

    if result.is_err() {
        return Err(FileDragError::WindowsApi {
            operation: "DoDragDrop",
            code: result.0,
        });
    }

    if result == DRAGDROP_S_CANCEL || effect == DROPEFFECT_NONE {
        Ok(FileDragCompletion::Cancelled)
    } else if effect.0 & DROPEFFECT_COPY.0 != 0 {
        Ok(FileDragCompletion::Copy)
    } else {
        Ok(FileDragCompletion::Other)
    }
}

fn validate_paths(paths: &[PathBuf]) -> Result<Vec<Vec<u16>>, FileDragError> {
    if paths.is_empty() {
        return Err(FileDragError::EmptyPathList);
    }
    if paths.len() > MAX_FILE_DRAG_FILES {
        return Err(FileDragError::TooManyPaths {
            limit: MAX_FILE_DRAG_FILES,
            actual: paths.len(),
        });
    }

    for (index, path) in paths.iter().enumerate() {
        if !path.is_absolute() {
            return Err(FileDragError::RelativePath { index });
        }
    }

    // Windows paths are native WTF-16 values. Keep the exact OsStr encoding;
    // converting through UTF-8 would be lossy for valid paths.
    collect_bounded_utf16_paths(
        paths.iter().map(|path| path.as_os_str().encode_wide()),
        MAX_FILE_DRAG_PATH_BYTES,
    )
    .map_err(|error| match error {
        Utf16PathError::EmbeddedNul { index } => FileDragError::PathContainsNul { index },
        Utf16PathError::TooLarge { actual } => FileDragError::PathBytesTooLarge {
            limit: MAX_FILE_DRAG_PATH_BYTES,
            actual,
        },
    })
}

fn current_left_button_state() -> i16 {
    // GetAsyncKeyState reports the current physical state. This is checked
    // immediately before DoDragDrop as well as before OLE setup so a deferred
    // foreground task cannot admit a drag after the gesture was released.
    unsafe { GetAsyncKeyState(i32::from(VK_LBUTTON.0)) }
}

fn left_button_is_pressed(state: i16) -> bool {
    state & i16::MIN != 0
}

struct ActiveDragGuard;

impl ActiveDragGuard {
    fn acquire() -> Result<Self, FileDragError> {
        ACTIVE_DRAG.with(|active| {
            if active.replace(true) {
                Err(FileDragError::WindowsDragAlreadyActive)
            } else {
                Ok(Self)
            }
        })
    }
}

impl Drop for ActiveDragGuard {
    fn drop(&mut self) {
        ACTIVE_DRAG.with(|active| active.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::{left_button_is_pressed, validate_paths};
    use crate::{FileDragError, MAX_FILE_DRAG_FILES, MAX_FILE_DRAG_PATH_BYTES};
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    #[test]
    fn left_button_state_uses_high_bit() {
        assert!(!left_button_is_pressed(0));
        assert!(left_button_is_pressed(i16::MIN));
        assert!(left_button_is_pressed(-1));
    }

    #[test]
    fn validates_windows_paths_without_utf8_conversion() {
        assert_eq!(
            validate_paths(&[]).unwrap_err(),
            FileDragError::EmptyPathList
        );
        assert_eq!(
            validate_paths(&[PathBuf::from(r"relative\kick.wav")]).unwrap_err(),
            FileDragError::RelativePath { index: 0 }
        );
        assert_eq!(
            validate_paths(&[PathBuf::from(r"C:\kick\0.wav")])
                .unwrap()
                .len(),
            1
        );
        let nul = PathBuf::from(OsString::from_wide(&[
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            b'k' as u16,
            0,
            b'.' as u16,
            b'w' as u16,
            b'a' as u16,
            b'v' as u16,
        ]));
        assert_eq!(
            validate_paths(&[nul]).unwrap_err(),
            FileDragError::PathContainsNul { index: 0 }
        );
        let native_wtf16 = PathBuf::from(OsString::from_wide(&[
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0xD800,
        ]));
        let encoded = validate_paths(&[native_wtf16]).expect("WTF-16 paths are preserved");
        assert_eq!(encoded[0].last().copied(), Some(0xD800));

        let paths = (0..=MAX_FILE_DRAG_FILES)
            .map(|index| PathBuf::from(format!(r"C:\samples\{index}.wav")))
            .collect::<Vec<_>>();
        assert_eq!(
            validate_paths(&paths).unwrap_err(),
            FileDragError::TooManyPaths {
                limit: MAX_FILE_DRAG_FILES,
                actual: MAX_FILE_DRAG_FILES + 1,
            }
        );

        let long = PathBuf::from(format!(r"C:\{}", "x".repeat(MAX_FILE_DRAG_PATH_BYTES)));
        assert!(matches!(
            validate_paths(&[long]),
            Err(FileDragError::PathBytesTooLarge { .. })
        ));
    }
}
