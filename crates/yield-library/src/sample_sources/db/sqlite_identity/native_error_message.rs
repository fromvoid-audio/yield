//! Exact pinned windows-result 0.4.1 BOOL-error text with a pre-copy funding point.
//! This is deliberately not a generic renderer for COM ErrorInfo-bearing errors.

#[cfg(any(windows, test))]
const PREFIX: &str = "GetFileInformationByHandle failed: ";

#[cfg(any(windows, test))]
fn trimmed(mut wide: &[u16]) -> &[u16] {
    while matches!(wide.last(), Some(32 | 9..=13)) {
        wide = &wide[..wide.len() - 1];
    }
    wide
}

#[cfg(test)]
fn reason<E>(
    wide: &[u16],
    code: i32,
    reserve: impl FnOnce(usize) -> Result<(), E>,
) -> Result<String, E> {
    reason_with_prefix(PREFIX, wide, code, reserve)
}

#[cfg(any(windows, test))]
fn reason_with_prefix<E>(
    prefix: &str,
    wide: &[u16],
    code: i32,
    reserve: impl FnOnce(usize) -> Result<(), E>,
) -> Result<String, E> {
    use std::fmt::Write;
    let wide = trimmed(wide);
    let message_bytes: usize = char::decode_utf16(wide.iter().copied())
        .map(|value| value.unwrap_or(char::REPLACEMENT_CHARACTER).len_utf8())
        .sum();
    // UTF16 native length is u32. Its at-most-three UTF8 bytes per unit plus
    // these fixed literals fit usize on the supported 64-bit native targets.
    let bytes = prefix.len()
        + if wide.is_empty() {
            10
        } else {
            message_bytes + 13
        };
    reserve(bytes)?;
    let mut output = String::with_capacity(bytes);
    output.push_str(prefix);
    if !wide.is_empty() {
        for value in char::decode_utf16(wide.iter().copied()) {
            output.push(value.unwrap_or(char::REPLACEMENT_CHARACTER));
        }
        output.push_str(" (");
    }
    write!(output, "0x{:08X}", code as u32).expect("String formatting cannot fail");
    if !wide.is_empty() {
        output.push(')');
    }
    debug_assert_eq!(output.len(), bytes);
    Ok(output)
}

#[cfg(windows)]
pub(super) fn win32_reason<E>(
    code: i32,
    reserve: impl FnOnce(usize) -> Result<(), E>,
) -> Result<String, E> {
    capture_reason(PREFIX, code, reserve)
}

// Pinned BOOL errors have no COM ErrorInfo. Preserve their complete original
// SDK Display without letting Error::message allocate an uncharged Rust String.
#[cfg(windows)]
pub(crate) fn win32_display<E>(
    code: i32,
    reserve: impl FnOnce(usize) -> Result<(), E>,
) -> Result<String, E> {
    capture_reason("", code, reserve)
}

#[cfg(windows)]
fn capture_reason<E>(
    prefix: &str,
    code: i32,
    reserve: impl FnOnce(usize) -> Result<(), E>,
) -> Result<String, E> {
    const _: () = assert!(std::mem::size_of::<usize>() >= 8);
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FormatMessageW(
            flags: u32,
            source: *const c_void,
            code: u32,
            language: u32,
            buffer: *mut u16,
            size: u32,
            arguments: *const c_void,
        ) -> u32;
        fn GetProcessHeap() -> *mut c_void;
        fn HeapFree(heap: *mut c_void, flags: u32, memory: *mut c_void) -> i32;
        fn LoadLibraryExA(name: *const u8, file: *mut c_void, flags: u32) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
    }
    struct Capture {
        text: *mut u16,
        module: *mut c_void,
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            unsafe {
                if !self.text.is_null() {
                    // Match pinned windows-result HeapString's allocator/destructor.
                    HeapFree(GetProcessHeap(), 0, self.text.cast());
                }
                if !self.module.is_null() {
                    FreeLibrary(self.module);
                }
            }
        }
    }
    let mut capture = Capture {
        text: std::ptr::null_mut(),
        module: std::ptr::null_mut(),
    };
    let mut flags = 0x100 | 0x1000 | 0x200; // ALLOCATE_BUFFER | FROM_SYSTEM | IGNORE_INSERTS
    let mut native_code = code as u32;
    if native_code & 0x1000_0000 != 0 {
        native_code ^= 0x1000_0000;
        flags |= 0x800; // FROM_HMODULE
        capture.module =
            unsafe { LoadLibraryExA(c"ntdll.dll".as_ptr().cast(), std::ptr::null_mut(), 0x1000) };
    }
    let length = unsafe {
        FormatMessageW(
            flags,
            capture.module,
            native_code,
            0,
            (&mut capture.text as *mut *mut u16).cast(),
            0,
            std::ptr::null(),
        )
    };
    let wide = if capture.text.is_null() || length == 0 {
        &[]
    } else {
        // Native allocation is retained until copying/refusal completes; no native
        // message/query is repeated between exact measurement and materialization.
        unsafe { std::slice::from_raw_parts(capture.text, length as usize) }
    };
    reason_with_prefix(prefix, wide, code, reserve)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_native_text_is_measured_before_copy_without_truncation() {
        let wide = [b'A' as u16, 0xd83d, 0xde80, 0xd800, 0, 32, 13, 10];
        let expected = "GetFileInformationByHandle failed: A🚀�\0 (0x80070006)";
        let value = reason(&wide, 0x80070006u32 as i32, |bytes| {
            assert_eq!(bytes, expected.len());
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(value, expected);
        assert_eq!(value.capacity(), expected.len());
        assert!(reason(&wide, -1, |_| Err::<(), _>("original pool refused")).is_err());
    }

    #[test]
    fn empty_message_and_pinned_trim_rules_preserve_original_text() {
        assert_eq!(
            reason(&[9, 32, 13], -1, |_| Ok::<_, ()>(())).unwrap(),
            "GetFileInformationByHandle failed: 0xFFFFFFFF"
        );
        assert_eq!(
            reason(&[0xa0], 0, |_| Ok::<_, ()>(())).unwrap(),
            "GetFileInformationByHandle failed: \u{a0} (0x00000000)"
        );
    }

    #[test]
    fn unprefixed_sdk_display_measures_full_variable_text_before_copy() {
        let wide = vec![0xd800; 16384];
        let text = reason_with_prefix("", &wide, -1, |bytes| {
            assert_eq!(bytes, 16384 * 3 + 13);
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(text, format!("{} (0xFFFFFFFF)", "�".repeat(16384)));
        assert!(reason_with_prefix("", &wide, -1, |_| Err::<(), _>("refused")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn actual_bool_file_information_error_preserves_complete_sdk_display() {
        use windows::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle},
        };
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        let error = unsafe { GetFileInformationByHandle(HANDLE::default(), &mut information) }
            .expect_err("a null native handle is not a file capability");
        let expected = format!("{PREFIX}{error}");
        let mut reserved = false;
        let actual = win32_reason(error.code().0, |bytes| {
            assert_eq!(bytes, expected.len());
            reserved = true;
            Ok::<_, ()>(())
        })
        .unwrap();
        assert!(
            reserved,
            "actual message must cross the pre-copy funding point"
        );
        assert_eq!(actual, expected);
        let actual = win32_display(error.code().0, |bytes| {
            assert_eq!(bytes, error.to_string().len());
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(actual, error.to_string());
    }
}
