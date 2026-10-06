//! Private original Shared aliases; no raw Arc or Weak escapes this holder.
use std::{ops::Deref, sync::Arc};

pub(super) struct SharedCustody<T>(Option<Arc<T>>);
impl<T> SharedCustody<T> {
    pub(super) fn new(value: T) -> Self {
        Self(Some(Arc::new(value)))
    }
}
impl<T> Clone for SharedCustody<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T> Deref for SharedCustody<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.0.as_deref().expect("live original WAL shared custody")
    }
}
impl<T> Drop for SharedCustody<T> {
    fn drop(&mut self) {
        // All aliases use into_inner. With no explicit Weak, the last raw Arc
        // allocation frees before its moved payload/status/input keeper drops.
        // Exactly one concurrent last-drop obtains the payload.
        if let Some(value) = self.0.take().and_then(Arc::into_inner) {
            drop(value);
        }
    }
}
