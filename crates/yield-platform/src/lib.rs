//! Small platform adapters used by the Yield UI.
//!
//! This crate deliberately keeps platform objects at the platform boundary.
//! Callers prepare and verify paths elsewhere, then hand a bounded list of
//! absolute paths to the synchronous native drag entry point.

use std::fmt;
#[cfg(any(target_os = "macos", all(test, not(target_os = "windows"))))]
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
#[cfg(target_os = "windows")]
use std::sync::{Arc, atomic::AtomicBool};

#[cfg(target_os = "macos")]
pub mod accessibility;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "windows", test))]
mod utf16_paths;
#[cfg(target_os = "windows")]
mod windows;

/// Maximum number of files admitted to one native drag session.
pub const MAX_FILE_DRAG_FILES: usize = 256;

/// Maximum combined path bytes admitted to one native drag session.
///
/// The Windows adapter counts exact UTF-16 path bytes; the macOS adapter
/// counts the platform's raw path bytes.
pub const MAX_FILE_DRAG_PATH_BYTES: usize = 1024 * 1024;

/// The bounded result returned when a native file drag session ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileDragCompletion {
    /// The destination accepted the files with the copy operation.
    Copy,
    /// The drag ended without an operation.
    Cancelled,
    /// The native platform reported an operation other than copy or no operation.
    Other,
}

/// Errors raised before a native file drag session is admitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileDragError {
    /// The caller supplied no paths.
    EmptyPathList,
    /// A path was not absolute. The caller must prepare absolute paths before
    /// entering the platform adapter.
    RelativePath {
        /// Zero-based path position in the rejected request.
        index: usize,
    },
    /// The request contains more paths than the adapter can retain.
    TooManyPaths {
        /// Maximum number of paths accepted by the adapter.
        limit: usize,
        /// Number of paths supplied by the caller.
        actual: usize,
    },
    /// The request's encoded path bytes exceed the adapter's retained input cap.
    PathBytesTooLarge {
        /// Maximum aggregate encoded path bytes accepted by the adapter.
        limit: usize,
        /// Aggregate encoded path bytes supplied by the caller.
        actual: usize,
    },
    /// A path contains an embedded NUL, which cannot be represented by native
    /// file-drag path formats.
    PathContainsNul {
        /// Zero-based path position in the rejected request.
        index: usize,
    },
    /// A path is not valid UTF-8 and therefore cannot be converted to an
    /// Objective-C `NSString` without changing its value.
    NonUtf8Path {
        /// Zero-based path position in the rejected request.
        index: usize,
    },
    /// The entry point must run on the AppKit main thread.
    NotMainThread,
    /// The current target has no native file-drag adapter yet.
    UnsupportedPlatform,
    /// The borrowed window handle is not an AppKit handle.
    UnsupportedWindowHandle,
    /// The AppKit view pointer in the handle could not be retained.
    WindowUnavailable,
    /// AppKit did not expose a current event for the callback.
    EventUnavailable,
    /// The current event is not a left mouse down or drag event.
    WrongEventType {
        /// Numeric AppKit event type observed at the source view.
        actual: u64,
    },
    /// The current event's mouse button is not the primary button.
    WrongMouseButton {
        /// Numeric AppKit mouse button observed in the current event.
        actual: i64,
    },
    /// The current event is historical, but the primary mouse button is no
    /// longer pressed.
    PrimaryButtonNotPressed {
        /// The platform's current primary-button state or pressed-button mask.
        mask: u64,
    },
    /// The current event belongs to a different window than the source view.
    EventWindowMismatch,
    /// A native drag is already retained by this thread's platform state.
    AlreadyActive {
        /// Identity of the retained in-flight session.
        identity: u64,
    },
    /// A Windows native API or COM operation failed.
    #[cfg(target_os = "windows")]
    WindowsApi {
        /// The native operation that failed.
        operation: &'static str,
        /// The failing HRESULT value.
        code: i32,
    },
    /// A Windows native drag is already running on this thread.
    #[cfg(target_os = "windows")]
    WindowsDragAlreadyActive,
}

impl fmt::Display for FileDragError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPathList => formatter.write_str("a file drag requires at least one path"),
            Self::RelativePath { index } => {
                write!(formatter, "file drag path {index} is not absolute")
            }
            Self::TooManyPaths { limit, actual } => {
                write!(
                    formatter,
                    "file drag contains {actual} paths; limit is {limit}"
                )
            }
            Self::PathBytesTooLarge { limit, actual } => write!(
                formatter,
                "file drag paths contain {actual} encoded bytes; limit is {limit}"
            ),
            Self::PathContainsNul { index } => {
                write!(formatter, "file drag path {index} contains an embedded NUL")
            }
            Self::NonUtf8Path { index } => {
                write!(formatter, "file drag path {index} is not valid UTF-8")
            }
            Self::NotMainThread => {
                formatter.write_str("native file drags must start on the main thread")
            }
            Self::UnsupportedPlatform => {
                formatter.write_str("native file drags are unsupported on this platform")
            }
            Self::UnsupportedWindowHandle => {
                formatter.write_str("native file drags require an AppKit window handle")
            }
            Self::WindowUnavailable => formatter.write_str("the AppKit source view is unavailable"),
            Self::EventUnavailable => {
                formatter.write_str("the current AppKit mouse event is unavailable")
            }
            Self::WrongEventType { actual } => {
                write!(
                    formatter,
                    "AppKit event type {actual} cannot start a file drag"
                )
            }
            Self::WrongMouseButton { actual } => {
                write!(
                    formatter,
                    "AppKit mouse button {actual} cannot start a file drag"
                )
            }
            Self::PrimaryButtonNotPressed { mask } => write!(
                formatter,
                "native primary mouse button is not pressed (state {mask})"
            ),
            Self::EventWindowMismatch => {
                formatter.write_str("the current mouse event belongs to a different window")
            }
            Self::AlreadyActive { identity } => {
                write!(formatter, "native file drag {identity} is already active")
            }
            #[cfg(target_os = "windows")]
            Self::WindowsApi { operation, code } => {
                write!(
                    formatter,
                    "Windows {operation} failed (HRESULT 0x{code:08X})"
                )
            }
            #[cfg(target_os = "windows")]
            Self::WindowsDragAlreadyActive => {
                formatter.write_str("a Windows native file drag is already active")
            }
        }
    }
}

impl std::error::Error for FileDragError {}

/// The receiver for one admitted native drag session.
///
/// Dropping this handle only drops the notification receiver. The platform
/// adapter retains the native drag objects until AppKit reports completion.
#[derive(Debug)]
pub struct FileDragHandle {
    identity: u64,
    receiver: Receiver<FileDragCompletion>,
}

impl FileDragHandle {
    #[cfg(target_os = "macos")]
    pub(crate) fn new(identity: u64, receiver: Receiver<FileDragCompletion>) -> Self {
        Self { identity, receiver }
    }

    /// Returns the identity used to match this handle to its native session.
    pub fn identity(&self) -> u64 {
        self.identity
    }

    /// Polls for a terminal drag result without blocking the UI thread.
    pub fn try_recv(&self) -> Result<FileDragCompletion, TryRecvError> {
        self.receiver.try_recv()
    }
}

#[cfg(any(target_os = "macos", all(test, not(target_os = "windows"))))]
#[derive(Debug)]
struct ValidatedPaths<'a> {
    paths: &'a [PathBuf],
}

#[cfg(any(target_os = "macos", all(test, not(target_os = "windows"))))]
fn validate_paths(paths: &[PathBuf]) -> Result<ValidatedPaths<'_>, FileDragError> {
    if paths.is_empty() {
        return Err(FileDragError::EmptyPathList);
    }
    if paths.len() > MAX_FILE_DRAG_FILES {
        return Err(FileDragError::TooManyPaths {
            limit: MAX_FILE_DRAG_FILES,
            actual: paths.len(),
        });
    }

    let mut total_bytes = 0usize;
    for (index, path) in paths.iter().enumerate() {
        if !path.is_absolute() {
            return Err(FileDragError::RelativePath { index });
        }
        let text = path.to_str().ok_or(FileDragError::NonUtf8Path { index })?;
        if text.as_bytes().contains(&0) {
            return Err(FileDragError::PathContainsNul { index });
        }

        total_bytes = total_bytes.saturating_add(raw_path_bytes(path));
        if total_bytes > MAX_FILE_DRAG_PATH_BYTES {
            return Err(FileDragError::PathBytesTooLarge {
                limit: MAX_FILE_DRAG_PATH_BYTES,
                actual: total_bytes,
            });
        }
    }

    Ok(ValidatedPaths { paths })
}

#[cfg(any(target_os = "macos", all(test, not(target_os = "windows"))))]
fn raw_path_bytes(path: &Path) -> usize {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().len()
    }
    #[cfg(not(unix))]
    {
        path.as_os_str().len()
    }
}

/// Starts an outbound native copy drag for prepared absolute paths.
///
/// The `window` handle must be borrowed from the live GPUI callback that owns
/// the source view. The adapter retains only the typed AppKit view needed by
/// the native session; it never retains the borrowed raw handle itself. This
/// function is synchronous and must be called on the AppKit main thread.
pub fn start_file_drag(
    window: raw_window_handle::WindowHandle<'_>,
    paths: &[PathBuf],
) -> Result<FileDragHandle, FileDragError> {
    #[cfg(target_os = "macos")]
    {
        let validated = validate_paths(paths)?;
        macos::start_file_drag(window, validated)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
        let _ = paths;
        Err(FileDragError::UnsupportedPlatform)
    }
}

/// Starts a synchronous Windows copy-only file drag.
///
/// This entry point intentionally has a separate, explicitly blocking API.
/// It must be called by the GPUI foreground/OLE thread after all GPUI,
/// entity, and window borrows have ended. The call enters Windows' nested OLE
/// drag loop and returns only after the target accepts or cancels the drag.
/// The adapter does not need or retain an HWND. `paths` may be an owned
/// `Vec<PathBuf>` or a borrowed slice; every path is copied into the native
/// data object before the call enters OLE.
///
/// The cancellation token is checked before admission and while the native
/// drag is active. Setting it to `true` requests cancellation through the OLE
/// callback; if the target completes the drag first, the completion result can
/// win that race.
#[cfg(target_os = "windows")]
pub fn start_file_drag_blocking<P>(
    paths: P,
    cancellation: Arc<AtomicBool>,
) -> Result<FileDragCompletion, FileDragError>
where
    P: AsRef<[PathBuf]>,
{
    windows::start_file_drag_blocking(paths.as_ref(), cancellation)
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn validation_rejects_empty_and_count_overflow() {
        assert_eq!(
            validate_paths(&[]).unwrap_err(),
            FileDragError::EmptyPathList
        );
        let paths = (0..=MAX_FILE_DRAG_FILES)
            .map(|index| PathBuf::from(format!("/tmp/{index}")))
            .collect::<Vec<_>>();
        assert_eq!(
            validate_paths(&paths).unwrap_err(),
            FileDragError::TooManyPaths {
                limit: MAX_FILE_DRAG_FILES,
                actual: MAX_FILE_DRAG_FILES + 1,
            }
        );
    }

    #[test]
    fn validation_rejects_nul_and_aggregate_path_bytes() {
        let nul = PathBuf::from(OsString::from("/tmp/yield\0file.wav"));
        assert_eq!(
            validate_paths(&[nul]).unwrap_err(),
            FileDragError::PathContainsNul { index: 0 }
        );

        let too_large = PathBuf::from(format!("/tmp/{}", "x".repeat(MAX_FILE_DRAG_PATH_BYTES)));
        assert_eq!(
            validate_paths(&[too_large]).unwrap_err(),
            FileDragError::PathBytesTooLarge {
                limit: MAX_FILE_DRAG_PATH_BYTES,
                actual: MAX_FILE_DRAG_PATH_BYTES + 5,
            }
        );
    }

    #[test]
    fn validation_rejects_relative_and_empty_paths() {
        assert_eq!(
            validate_paths(&[PathBuf::from("relative.wav")]).unwrap_err(),
            FileDragError::RelativePath { index: 0 }
        );
        assert_eq!(
            validate_paths(&[PathBuf::new()]).unwrap_err(),
            FileDragError::RelativePath { index: 0 }
        );
    }

    #[cfg(unix)]
    #[test]
    fn validation_rejects_non_utf8_without_lossy_conversion() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]));
        assert_eq!(
            validate_paths(&[path]).unwrap_err(),
            FileDragError::NonUtf8Path { index: 0 }
        );
    }

    #[test]
    fn validation_accepts_bounded_utf8_paths() {
        let paths = [PathBuf::from("/tmp/one.wav"), PathBuf::from("/tmp/two.wav")];
        assert_eq!(validate_paths(&paths).unwrap().paths.len(), 2);
    }
}
