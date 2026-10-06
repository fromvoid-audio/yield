//! Private original allocation aliases: no raw Arc or Weak escapes this holder.
use std::ops::Deref;
use std::sync::Arc;

pub(super) struct Custody<T> {
    inner: Option<Arc<T>>,
}
impl<T> Custody<T> {
    pub(super) fn new(value: T) -> Self {
        Self {
            inner: Some(Arc::new(value)),
        }
    }
    pub(super) fn strong_count(&self) -> usize {
        Arc::strong_count(
            self.inner
                .as_ref()
                .expect("live original publication custody"),
        )
    }
    pub(super) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(
            self.inner
                .as_ref()
                .expect("live original publication custody"),
            other
                .inner
                .as_ref()
                .expect("live original publication custody"),
        )
    }
}
impl<T> Clone for Custody<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
impl<T> Deref for Custody<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.inner
            .as_deref()
            .expect("live original publication custody")
    }
}
impl<T> Drop for Custody<T> {
    fn drop(&mut self) {
        // Every clone uses this path. into_inner frees the Arc allocation before
        // the moved payload drops its nested backing (or refunds InputPermit).
        // Unlike try_unwrap, exactly one concurrent last-drop obtains the value.
        if let Some(value) = self.inner.take().and_then(Arc::into_inner) {
            drop(value);
        }
    }
}
