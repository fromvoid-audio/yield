use std::{
    ffi::{CStr, c_char, c_void},
    path::{Path, PathBuf},
};

use super::FileClipboardError;

type Id = *mut c_void;
type Sel = *mut c_void;
type ObjcBool = i8;

const NO: ObjcBool = 0;
const NS_UTF8_STRING_ENCODING: usize = 4;
const NS_FILENAMES_PBOARD_TYPE: &str = "NSFilenamesPboardType";
const NS_URL_PBOARD_TYPE: &str = "NSURLPboardType";
const NSPASTEBOARD_TYPE_FILE_URL: &str = "public.file-url";
const NSPASTEBOARD_TYPE_STRING: &str = "public.utf8-plain-text";

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
}

pub(super) fn copy_file_paths(paths: &[PathBuf]) -> Result<(), FileClipboardError> {
    let _pool = AutoreleasePool::new().map_err(platform_error)?;
    let urls = unsafe { ns_mutable_array(paths.len()).map_err(platform_error)? };
    let filenames = unsafe { ns_mutable_array(paths.len()).map_err(platform_error)? };
    let mut first_file_url_string = None;

    // Build every Objective-C object before touching the pasteboard. A path
    // encoding failure therefore leaves the user's existing clipboard intact.
    for path in paths {
        unsafe {
            let path_string = path_to_pasteboard_string(path).map_err(platform_error)?;
            let url = file_url_for_path(path).map_err(platform_error)?;
            msg_void_id(urls, selector(c"addObject:"), url);
            msg_void_id(
                filenames,
                selector(c"addObject:"),
                ns_string(&path_string).map_err(platform_error)?,
            );
            if first_file_url_string.is_none() {
                first_file_url_string = Some(msg_id(url, selector(c"absoluteString")));
            }
        }
    }

    let pasteboard = unsafe { general_pasteboard().map_err(platform_error)? };
    unsafe {
        let _ = msg_isize(pasteboard, selector(c"clearContents"));
        let ok = msg_bool_id(pasteboard, selector(c"writeObjects:"), urls);
        if ok == NO {
            return Err(platform_error("NSPasteboard writeObjects failed"));
        }
        set_pasteboard_property_list(pasteboard, filenames, NS_FILENAMES_PBOARD_TYPE)
            .map_err(platform_error)?;
        if let Some(file_url_string) = first_file_url_string {
            set_pasteboard_string(pasteboard, file_url_string, NSPASTEBOARD_TYPE_FILE_URL)
                .map_err(platform_error)?;
            set_pasteboard_string(pasteboard, file_url_string, NS_URL_PBOARD_TYPE)
                .map_err(platform_error)?;
        }
    }
    Ok(())
}

pub(super) fn copy_text(text: &str) -> Result<(), String> {
    let _pool = AutoreleasePool::new()?;
    // Prepare NSString before clearing the user's existing clipboard.
    let ns_text = unsafe { ns_string(text)? };
    let text_type = unsafe { ns_string(NSPASTEBOARD_TYPE_STRING)? };
    let pasteboard = unsafe { general_pasteboard()? };
    unsafe {
        let _ = msg_isize(pasteboard, selector(c"clearContents"));
        set_pasteboard_string_id(pasteboard, ns_text, text_type)?;
    }
    Ok(())
}

struct AutoreleasePool {
    pool: Id,
}

impl AutoreleasePool {
    fn new() -> Result<Self, String> {
        let pool = unsafe {
            let class = class(c"NSAutoreleasePool")?;
            msg_id(class, selector(c"new"))
        };
        if pool.is_null() {
            Err(String::from("Failed to create NSAutoreleasePool"))
        } else {
            Ok(Self { pool })
        }
    }
}

impl Drop for AutoreleasePool {
    fn drop(&mut self) {
        unsafe {
            msg_void(self.pool, selector(c"drain"));
        }
    }
}

unsafe fn general_pasteboard() -> Result<Id, String> {
    let pasteboard = unsafe {
        let class = class(c"NSPasteboard")?;
        msg_id(class, selector(c"generalPasteboard"))
    };
    if pasteboard.is_null() {
        Err(String::from("NSPasteboard generalPasteboard returned nil"))
    } else {
        Ok(pasteboard)
    }
}

unsafe fn ns_mutable_array(capacity: usize) -> Result<Id, String> {
    let array = unsafe {
        let class = class(c"NSMutableArray")?;
        msg_id_usize(class, selector(c"arrayWithCapacity:"), capacity)
    };
    if array.is_null() {
        Err(String::from("Failed to create NSMutableArray"))
    } else {
        Ok(array)
    }
}

unsafe fn file_url_for_path(path: &Path) -> Result<Id, String> {
    let path = path_to_pasteboard_string(path)?;
    let ns_path = unsafe { ns_string(&path)? };
    let url = unsafe {
        let class = class(c"NSURL")?;
        msg_id_id(class, selector(c"fileURLWithPath:"), ns_path)
    };
    if url.is_null() {
        Err(format!("Failed to create file URL for {path}"))
    } else {
        Ok(url)
    }
}

unsafe fn set_pasteboard_property_list(
    pasteboard: Id,
    property_list: Id,
    pasteboard_type: &str,
) -> Result<(), String> {
    let pasteboard_type = unsafe { ns_string(pasteboard_type)? };
    let ok = unsafe {
        msg_bool_id_id(
            pasteboard,
            selector(c"setPropertyList:forType:"),
            property_list,
            pasteboard_type,
        )
    };
    if ok == NO {
        Err(String::from("NSPasteboard setPropertyList failed"))
    } else {
        Ok(())
    }
}

unsafe fn set_pasteboard_string(
    pasteboard: Id,
    string: Id,
    pasteboard_type: &str,
) -> Result<(), String> {
    let pasteboard_type = unsafe { ns_string(pasteboard_type)? };
    unsafe { set_pasteboard_string_id(pasteboard, string, pasteboard_type) }
}

unsafe fn set_pasteboard_string_id(
    pasteboard: Id,
    string: Id,
    pasteboard_type: Id,
) -> Result<(), String> {
    let ok = unsafe {
        msg_bool_id_id(
            pasteboard,
            selector(c"setString:forType:"),
            string,
            pasteboard_type,
        )
    };
    if ok == NO {
        Err(String::from("NSPasteboard setString failed"))
    } else {
        Ok(())
    }
}

unsafe fn ns_string(value: &str) -> Result<Id, String> {
    let allocated = unsafe {
        let class = class(c"NSString")?;
        msg_id(class, selector(c"alloc"))
    };
    if allocated.is_null() {
        return Err(String::from("Failed to allocate NSString"));
    }
    let string = unsafe {
        msg_id_ptr_usize_usize(
            allocated,
            selector(c"initWithBytes:length:encoding:"),
            value.as_ptr().cast(),
            value.len(),
            NS_UTF8_STRING_ENCODING,
        )
    };
    if string.is_null() {
        Err(String::from("Failed to create NSString"))
    } else {
        Ok(unsafe { msg_id(string, selector(c"autorelease")) })
    }
}

unsafe fn class(name: &'static CStr) -> Result<Id, String> {
    let class = unsafe { objc_getClass(name.as_ptr()) };
    if class.is_null() {
        Err(format!(
            "Objective-C class {} not found",
            name.to_string_lossy()
        ))
    } else {
        Ok(class)
    }
}

unsafe fn selector(name: &'static CStr) -> Sel {
    unsafe { sel_registerName(name.as_ptr()) }
}

unsafe fn msg_id(receiver: Id, selector: Sel) -> Id {
    let msg: unsafe extern "C" fn(Id, Sel) -> Id =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { msg(receiver, selector) }
}

unsafe fn msg_id_id(receiver: Id, selector: Sel, arg: Id) -> Id {
    let msg: unsafe extern "C" fn(Id, Sel, Id) -> Id =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { msg(receiver, selector, arg) }
}

unsafe fn msg_id_usize(receiver: Id, selector: Sel, arg: usize) -> Id {
    let msg: unsafe extern "C" fn(Id, Sel, usize) -> Id =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { msg(receiver, selector, arg) }
}

unsafe fn msg_id_ptr_usize_usize(
    receiver: Id,
    selector: Sel,
    bytes: *const c_void,
    length: usize,
    encoding: usize,
) -> Id {
    let msg: unsafe extern "C" fn(Id, Sel, *const c_void, usize, usize) -> Id =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { msg(receiver, selector, bytes, length, encoding) }
}

unsafe fn msg_isize(receiver: Id, selector: Sel) -> isize {
    let msg: unsafe extern "C" fn(Id, Sel) -> isize =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { msg(receiver, selector) }
}

unsafe fn msg_bool_id(receiver: Id, selector: Sel, arg: Id) -> ObjcBool {
    let msg: unsafe extern "C" fn(Id, Sel, Id) -> ObjcBool =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { msg(receiver, selector, arg) }
}

unsafe fn msg_bool_id_id(receiver: Id, selector: Sel, first: Id, second: Id) -> ObjcBool {
    let msg: unsafe extern "C" fn(Id, Sel, Id, Id) -> ObjcBool =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { msg(receiver, selector, first, second) }
}

unsafe fn msg_void(receiver: Id, selector: Sel) {
    let msg: unsafe extern "C" fn(Id, Sel) =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { msg(receiver, selector) }
}

unsafe fn msg_void_id(receiver: Id, selector: Sel, arg: Id) {
    let msg: unsafe extern "C" fn(Id, Sel, Id) =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { msg(receiver, selector, arg) }
}

fn path_to_pasteboard_string(path: &Path) -> Result<String, String> {
    path.to_str().map(ToOwned::to_owned).ok_or_else(|| {
        format!(
            "Cannot copy non-UTF-8 path to clipboard: {}",
            path.display()
        )
    })
}

fn platform_error(message: impl Into<String>) -> FileClipboardError {
    FileClipboardError::Platform(message.into())
}

#[cfg(test)]
mod tests {
    use super::path_to_pasteboard_string;
    use std::path::Path;

    #[test]
    fn path_to_pasteboard_string_keeps_spaces_and_unicode() {
        assert_eq!(
            path_to_pasteboard_string(Path::new("/tmp/é sample.wav")).unwrap(),
            "/tmp/é sample.wav"
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_to_pasteboard_string_rejects_non_utf8_paths() {
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/bad-\xFF.wav"));
        assert!(path_to_pasteboard_string(path).is_err());
    }
}
