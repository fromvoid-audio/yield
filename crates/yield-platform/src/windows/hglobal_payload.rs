use std::mem::ManuallyDrop;
use std::ptr::{copy_nonoverlapping, write_unaligned};
use windows::Win32::Foundation::{E_OUTOFMEMORY, GlobalFree, HGLOBAL, POINT};
use windows::Win32::System::Com::{STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock,
};
use windows::Win32::UI::Shell::DROPFILES;

pub(super) fn path_list_medium(paths: &[Vec<u16>]) -> windows::core::Result<STGMEDIUM> {
    let encoded = encode_hdrop_path_list(paths);
    let payload_bytes = std::mem::size_of::<DROPFILES>()
        .checked_add(
            encoded
                .len()
                .checked_mul(std::mem::size_of::<u16>())
                .ok_or_else(|| windows::core::Error::from(E_OUTOFMEMORY))?,
        )
        .ok_or_else(|| windows::core::Error::from(E_OUTOFMEMORY))?;

    let owned = OwnedHGlobal::new(payload_bytes)?;
    let lock = GlobalLockGuard::new(owned.handle())?;
    // SAFETY: the allocation has enough space for the packed DROPFILES header
    // and the complete double-null-terminated UTF-16 path list.
    unsafe {
        write_unaligned(
            lock.ptr().cast::<DROPFILES>(),
            DROPFILES {
                pFiles: std::mem::size_of::<DROPFILES>() as u32,
                pt: POINT { x: 0, y: 0 },
                fNC: false.into(),
                fWide: true.into(),
            },
        );
        let data = lock
            .ptr()
            .cast::<u8>()
            .add(std::mem::size_of::<DROPFILES>());
        copy_nonoverlapping(
            encoded.as_ptr().cast::<u8>(),
            data,
            encoded.len() * std::mem::size_of::<u16>(),
        );
    }

    Ok(owned.into_medium())
}

pub(super) fn encode_hdrop_path_list(paths: &[Vec<u16>]) -> Vec<u16> {
    let total_units = paths
        .iter()
        .map(|path| path.len().saturating_add(1))
        .fold(1usize, usize::saturating_add);
    let mut encoded = Vec::with_capacity(total_units);
    for path in paths {
        encoded.extend_from_slice(path);
        encoded.push(0);
    }
    // CF_HDROP uses one additional NUL to terminate the complete list.
    encoded.push(0);
    encoded
}

struct OwnedHGlobal {
    handle: HGLOBAL,
    released: bool,
}

impl OwnedHGlobal {
    fn new(bytes: usize) -> windows::core::Result<Self> {
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes) }?;
        Ok(Self {
            handle,
            released: false,
        })
    }

    fn handle(&self) -> HGLOBAL {
        self.handle
    }

    fn into_medium(mut self) -> STGMEDIUM {
        self.released = true;
        hglobal_medium(self.handle)
    }
}

impl Drop for OwnedHGlobal {
    fn drop(&mut self) {
        if !self.released {
            unsafe {
                let _ = GlobalFree(Some(self.handle));
            }
        }
    }
}

struct GlobalLockGuard {
    handle: HGLOBAL,
    ptr: *mut std::ffi::c_void,
}

impl GlobalLockGuard {
    fn new(handle: HGLOBAL) -> windows::core::Result<Self> {
        let ptr = unsafe { GlobalLock(handle) };
        if ptr.is_null() {
            return Err(windows::core::Error::from_thread());
        }
        Ok(Self { handle, ptr })
    }

    fn ptr(&self) -> *mut std::ffi::c_void {
        self.ptr
    }
}

impl Drop for GlobalLockGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = GlobalUnlock(self.handle);
        }
    }
}

fn hglobal_medium(handle: HGLOBAL) -> STGMEDIUM {
    STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: handle },
        pUnkForRelease: ManuallyDrop::new(None),
    }
}

#[cfg(test)]
mod tests {
    use super::{encode_hdrop_path_list, hglobal_medium};
    use windows::Win32::Foundation::HGLOBAL;

    #[test]
    fn hdrop_path_list_is_double_null_terminated() {
        let encoded = encode_hdrop_path_list(&[vec![b'C' as u16, b':' as u16]]);
        assert_eq!(encoded, vec![b'C' as u16, b':' as u16, 0, 0]);
    }

    #[test]
    fn hglobal_medium_marks_hglobal_and_has_no_release_unknown() {
        let medium = hglobal_medium(HGLOBAL::default());
        assert_eq!(medium.tymed, 1);
        assert!(medium.pUnkForRelease.is_none());
    }
}
