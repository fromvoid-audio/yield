use super::formats::file_drop_format;
use super::hglobal_payload::path_list_medium;
use windows::Win32::Foundation::{DATA_S_SAMEFORMATETC, DV_E_FORMATETC, E_INVALIDARG, E_NOTIMPL};
use windows::Win32::System::Com::{
    DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IEnumFORMATETC, STGMEDIUM,
    TYMED_HGLOBAL,
};
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::UI::Shell::SHCreateStdEnumFmtEtc;
use windows::core::{BOOL, HRESULT, Ref, implement};

/// A source data object containing only a cloned CF_HDROP path list.
#[implement(IDataObject)]
#[derive(Clone)]
pub(super) struct FileDropDataObject {
    paths: Vec<Vec<u16>>,
    format: FORMATETC,
}

impl FileDropDataObject {
    pub(super) fn new(paths: Vec<Vec<u16>>) -> Self {
        Self {
            paths,
            format: file_drop_format(),
        }
    }

    fn matches_format(&self, format: &FORMATETC) -> bool {
        format.cfFormat == CF_HDROP.0
            && format.dwAspect == DVASPECT_CONTENT.0
            && (format.lindex == -1 || format.lindex == 0)
            && format.tymed & TYMED_HGLOBAL.0 as u32 != 0
    }
}

#[allow(non_snake_case)]
impl windows::Win32::System::Com::IDataObject_Impl for FileDropDataObject_Impl {
    fn GetData(&self, format_in: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        if format_in.is_null() {
            return Err(windows::core::Error::from(E_INVALIDARG));
        }
        // SAFETY: COM supplies a valid FORMATETC pointer for the duration of
        // this callback; the value is copied before any native call.
        let format = unsafe { &*format_in };
        if !self.matches_format(format) {
            return Err(windows::core::Error::from(DV_E_FORMATETC));
        }
        // Build a fresh HGLOBAL for every request. The caller owns the
        // returned STGMEDIUM and will release it through ReleaseStgMedium.
        path_list_medium(&self.paths)
    }

    fn GetDataHere(
        &self,
        _format: *const FORMATETC,
        _medium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::from(E_NOTIMPL))
    }

    fn QueryGetData(&self, format_in: *const FORMATETC) -> HRESULT {
        if format_in.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: COM supplies a valid FORMATETC pointer for this callback.
        let format = unsafe { &*format_in };
        if self.matches_format(format) {
            HRESULT(0)
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        format_in: *const FORMATETC,
        format_out: *mut FORMATETC,
    ) -> HRESULT {
        if format_in.is_null() || format_out.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: both pointers are valid for this COM callback. Copy the
        // input descriptor, then clear ptd so the output never retains a
        // foreign target-device allocation.
        let input = unsafe { *format_in };
        let supported = self.matches_format(&input);
        let mut canonical = input;
        canonical.ptd = std::ptr::null_mut();
        unsafe {
            *format_out = canonical;
        }
        if supported {
            DATA_S_SAMEFORMATETC
        } else {
            DV_E_FORMATETC
        }
    }

    fn SetData(
        &self,
        _format: *const FORMATETC,
        _medium: *const STGMEDIUM,
        _release: BOOL,
    ) -> windows::core::Result<()> {
        // This source is immutable and publishes only CF_HDROP. Rejecting
        // SetData avoids taking ownership of a foreign STGMEDIUM.
        Err(windows::core::Error::from(E_NOTIMPL))
    }

    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(windows::core::Error::from(E_NOTIMPL));
        }
        unsafe { SHCreateStdEnumFmtEtc(&[self.format]) }
    }

    fn DAdvise(
        &self,
        _format: *const FORMATETC,
        _flags: u32,
        _sink: Ref<'_, IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(windows::core::Error::from(E_NOTIMPL))
    }

    fn DUnadvise(&self, _connection: u32) -> windows::core::Result<()> {
        Err(windows::core::Error::from(E_NOTIMPL))
    }

    fn EnumDAdvise(&self) -> windows::core::Result<windows::Win32::System::Com::IEnumSTATDATA> {
        Err(windows::core::Error::from(E_NOTIMPL))
    }
}
