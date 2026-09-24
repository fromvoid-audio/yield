use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::copy_nonoverlapping;

use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Shell::DROPFILES;

use super::handle::{GlobalLockGuard, OwnedHGlobal};

pub(super) fn create_hdrop(paths: &[PathBuf]) -> Result<OwnedHGlobal, String> {
    let utf16_paths = encode_hdrop_path_list(paths);
    let owned = OwnedHGlobal::new(hdrop_payload_bytes_for_units(utf16_paths.len()))?;
    // SAFETY: the allocation was created with enough bytes for DROPFILES and
    // the double-null-terminated UTF-16 path list.
    let lock = unsafe { GlobalLockGuard::new(owned.handle()) }?;
    unsafe {
        let header = lock.as_mut_ptr::<DROPFILES>();
        *header = DROPFILES {
            pFiles: std::mem::size_of::<DROPFILES>() as u32,
            pt: POINT { x: 0, y: 0 },
            fNC: false.into(),
            fWide: true.into(),
        };
        let data_ptr = (lock.ptr() as *mut u8).add(std::mem::size_of::<DROPFILES>());
        copy_nonoverlapping(
            utf16_paths.as_ptr().cast::<u8>(),
            data_ptr,
            utf16_paths.len() * std::mem::size_of::<u16>(),
        );
    }
    Ok(owned)
}

pub(super) fn create_drop_effect(effect: u32) -> Result<OwnedHGlobal, String> {
    let owned = OwnedHGlobal::new(drop_effect_payload_bytes())?;
    // SAFETY: the allocation is exactly large enough for one u32 drop effect.
    let lock = unsafe { GlobalLockGuard::new(owned.handle()) }?;
    unsafe {
        *lock.as_mut_ptr::<u32>() = effect;
    }
    Ok(owned)
}

fn encode_hdrop_path_list(paths: &[PathBuf]) -> Vec<u16> {
    let mut utf16_paths = Vec::new();
    for path in paths {
        utf16_paths.extend(path.as_os_str().encode_wide().chain(std::iter::once(0)));
    }
    utf16_paths.push(0);
    utf16_paths
}

fn hdrop_payload_bytes_for_units(path_units: usize) -> usize {
    std::mem::size_of::<DROPFILES>() + path_units * std::mem::size_of::<u16>()
}

fn drop_effect_payload_bytes() -> usize {
    std::mem::size_of::<u32>()
}

#[cfg(test)]
mod tests {
    use super::{drop_effect_payload_bytes, encode_hdrop_path_list, hdrop_payload_bytes_for_units};
    use std::path::PathBuf;

    #[test]
    fn hdrop_path_list_is_double_null_terminated_and_preserves_unicode() {
        let encoded = encode_hdrop_path_list(&[
            PathBuf::from(r"C:\samples\kick é.wav"),
            PathBuf::from(r"D:\snare sample.wav"),
        ]);
        assert_eq!(encoded.last().copied(), Some(0));
        assert_eq!(encoded.iter().filter(|&&unit| unit == 0).count(), 3);
        assert!(encoded.contains(&0x00E9));
    }

    #[test]
    fn hdrop_payload_size_includes_header_and_utf16_units() {
        let encoded = encode_hdrop_path_list(&[PathBuf::from(r"C:\samples\hat.wav")]);
        assert_eq!(
            hdrop_payload_bytes_for_units(encoded.len()),
            std::mem::size_of::<windows::Win32::UI::Shell::DROPFILES>()
                + encoded.len() * std::mem::size_of::<u16>()
        );
    }

    #[test]
    fn drop_effect_payload_size_matches_u32() {
        assert_eq!(drop_effect_payload_bytes(), std::mem::size_of::<u32>());
    }
}
