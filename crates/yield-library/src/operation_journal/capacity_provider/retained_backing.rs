//! Private original allocation custody. Raw strong/weak handles never escape.
use super::super::owner::StartupCredit;
use std::ops::Deref;
use std::rc::Rc;
use std::sync::{Arc, Weak};

pub(super) struct Credit(Option<Arc<StartupCredit>>);
impl Credit {
    pub(super) fn new(permit: StartupCredit) -> Self {
        Self(Some(Arc::new(permit)))
    }
}
impl Clone for Credit {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl Drop for Credit {
    fn drop(&mut self) {
        if let Some(raw) = self.0.take() {
            // Every alias uses into_inner; implicit Weak is freed before refund.
            drop(Arc::into_inner(raw));
        }
    }
}

pub(crate) struct RetainedArc<T> {
    raw: Option<Arc<T>>,
    credit: Option<Credit>,
}
impl<T> RetainedArc<T> {
    pub(super) fn new(value: T, credit: Option<Credit>) -> Self {
        Self {
            raw: Some(Arc::new(value)),
            credit,
        }
    }
    pub(super) fn ptr_eq(a: &Self, b: &Self) -> bool {
        Arc::ptr_eq(a.raw.as_ref().unwrap(), b.raw.as_ref().unwrap())
    }
    pub(super) fn downgrade(&self) -> RetainedWeak<T> {
        RetainedWeak {
            raw: Some(Arc::downgrade(self.raw.as_ref().unwrap())),
            credit: self.credit.clone(),
        }
    }
    pub(super) fn credit(&self) -> Option<Credit> {
        self.credit.clone()
    }
}
impl<T> Clone for RetainedArc<T> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            credit: self.credit.clone(),
        }
    }
}
impl<T> Deref for RetainedArc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.raw.as_ref().unwrap()
    }
}
impl<T: std::fmt::Debug> std::fmt::Debug for RetainedArc<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.deref().fmt(f)
    }
}
impl<T> Drop for RetainedArc<T> {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            drop(Arc::into_inner(raw));
        }
        // Explicit weak aliases separately retain this credit through header deallocation.
        self.credit.take();
    }
}

pub(super) struct RetainedWeak<T> {
    raw: Option<Weak<T>>,
    credit: Option<Credit>,
}
impl<T> RetainedWeak<T> {
    pub(super) fn upgrade(&self) -> Option<RetainedArc<T>> {
        self.raw.as_ref().unwrap().upgrade().map(|raw| RetainedArc {
            raw: Some(raw),
            credit: self.credit.clone(),
        })
    }
    pub(super) fn strong_count(&self) -> usize {
        self.raw.as_ref().unwrap().strong_count()
    }
    pub(super) fn as_ptr(&self) -> *const T {
        self.raw.as_ref().unwrap().as_ptr()
    }
}
impl<T> std::fmt::Debug for RetainedWeak<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedWeak").finish_non_exhaustive()
    }
}
impl<T> Drop for RetainedWeak<T> {
    fn drop(&mut self) {
        // Last explicit Weak must actually free RawArc before returning its debit.
        self.raw.take();
        self.credit.take();
    }
}

pub(super) struct RetainedRc<T> {
    raw: Option<Rc<T>>,
    credit: Option<Credit>,
}
impl<T> RetainedRc<T> {
    pub(super) fn new(value: T, credit: Option<Credit>) -> Self {
        Self {
            raw: Some(Rc::new(value)),
            credit,
        }
    }
    pub(super) fn ptr_eq(a: &Self, b: &Self) -> bool {
        Rc::ptr_eq(a.raw.as_ref().unwrap(), b.raw.as_ref().unwrap())
    }
}
impl<T> Clone for RetainedRc<T> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            credit: self.credit.clone(),
        }
    }
}
impl<T> Deref for RetainedRc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.raw.as_ref().unwrap()
    }
}
impl<T: std::fmt::Debug> std::fmt::Debug for RetainedRc<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.deref().fmt(f)
    }
}
impl<T> Drop for RetainedRc<T> {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            drop(Rc::into_inner(raw));
        }
        self.credit.take();
    }
}
