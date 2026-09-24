use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};

/// RAII guard balancing OLE initialization on the calling thread.
pub(super) struct ComApartment;

impl ComApartment {
    pub(super) fn new() -> windows::core::Result<Self> {
        // OleInitialize selects the STA apartment required by OLE drag/drop.
        // A changed existing apartment is returned as an error and is never
        // silently handled by moving the drag to another thread.
        unsafe { OleInitialize(None) }?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            OleUninitialize();
        }
    }
}
