//! Original-runtime resource custody, not source, capacity or namespace authorization.
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

pub(super) const HEADER_BYTES: usize = 4096;
const MAX_USES: usize = 32;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UseError {
    Closed,
    Full,
    Unknown,
}
#[derive(Clone, Copy)]
struct Use {
    generation: u64,
    entered: bool,
}
struct State {
    open: bool,
    closed: bool,
    unknown: bool,
    unsettled: bool,
    next: u64,
    count: usize,
    uses: [Option<Use>; MAX_USES],
}
impl State {
    fn valid(&self) -> bool {
        self.count <= MAX_USES && self.count == self.uses.iter().filter(|u| u.is_some()).count()
    }
    fn close_unknown(&mut self) {
        self.open = false;
        self.closed = true;
        self.unknown = true;
    }
}
pub(super) struct ResourceAdmission {
    state: Mutex<State>,
    quiescent: Condvar,
}
/// Unique counted use only. It grants no right to read a native capsule or enter an effect.
#[allow(dead_code)] // Actual slot/capsule composition remains OPT-2215.
pub(super) struct ResourceUse {
    admission: Arc<ResourceAdmission>,
    index: usize,
    generation: u64,
    observed: bool,
}
const _: () = assert!(
    2 * std::mem::size_of::<ResourceAdmission>()
        + MAX_USES * std::mem::size_of::<ResourceUse>()
        + 128
        <= HEADER_BYTES
);
impl ResourceAdmission {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                open: false,
                closed: false,
                unknown: false,
                unsettled: false,
                next: 0,
                count: 0,
                uses: [None; MAX_USES],
            }),
            quiescent: Condvar::new(),
        })
    }
    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.close_unknown();
                state
            }
        }
    }
    /// Run only the original lifecycle CAS under this gate. No IO or queue wait in callback.
    pub(super) fn activate(&self, publish: impl FnOnce() -> bool) -> bool {
        let mut state = self.lock();
        if state.unknown || !state.valid() {
            state.close_unknown();
            return false;
        }
        if state.closed {
            return false;
        }
        if publish() {
            state.open = true;
            true
        } else {
            state.open = false;
            state.closed = true;
            false
        }
    }
    #[cfg(test)]
    pub(super) fn snapshot(&self) -> (bool, usize, bool) {
        let state = self.lock();
        (state.closed, state.count, state.unsettled)
    }
    pub(super) fn validate_open(&self) -> Result<(), UseError> {
        let mut state = self.lock();
        if state.unknown || !state.valid() {
            state.close_unknown();
            return Err(UseError::Unknown);
        }
        if state.closed || !state.open {
            return Err(UseError::Closed);
        }
        Ok(())
    }
    pub(super) fn invalidate(&self) {
        self.lock().close_unknown();
    }
    pub(super) fn close(&self) {
        let mut state = self.lock();
        state.open = false;
        state.closed = true;
    }
    /// Future slot entry locks its scalar slot ONLY inside this admission callback. Native
    /// reads, SQL, source validation and queue waits must occur after releasing both locks.
    #[allow(dead_code)] // Private original issuer/slot consumer is full OPT-2215.
    pub(super) fn begin_read(
        self: &Arc<Self>,
        validate_slot: impl FnOnce() -> bool,
    ) -> Result<ResourceUse, UseError> {
        let mut state = self.lock();
        if !state.valid() || state.unknown {
            state.close_unknown();
            return Err(UseError::Unknown);
        }
        if !state.open || state.closed {
            return Err(UseError::Closed);
        }
        let index = state
            .uses
            .iter()
            .position(Option::is_none)
            .ok_or(UseError::Full)?;
        let generation = state
            .next
            .checked_add(1)
            .filter(|g| *g != 0)
            .ok_or_else(|| {
                state.close_unknown();
                UseError::Unknown
            })?;
        if !validate_slot() {
            return Err(UseError::Closed);
        }
        state.next = generation;
        state.uses[index] = Some(Use {
            generation,
            entered: false,
        });
        state.count = state
            .count
            .checked_add(1)
            .filter(|n| *n <= MAX_USES)
            .ok_or_else(|| {
                state.close_unknown();
                UseError::Unknown
            })?;
        Ok(ResourceUse {
            admission: Arc::clone(self),
            index,
            generation,
            observed: false,
        })
    }
    /// Retain original actor resources while this condition wait releases the admission lock.
    /// Quiescence is distinct from clean handoff and durable settlement.
    pub(super) fn close_and_wait(&self) -> bool {
        let mut state = self.lock();
        state.open = false;
        state.closed = true;
        while state.count != 0 || state.uses.iter().any(Option::is_some) {
            if !state.valid() {
                state.close_unknown();
            }
            state = match self.quiescent.wait(state) {
                Ok(state) => state,
                Err(poisoned) => {
                    let mut state = poisoned.into_inner();
                    state.close_unknown();
                    state
                }
            };
        }
        state.valid() && !state.unknown && !state.unsettled
    }
}
#[allow(dead_code)] // Read-to-effect upgrade is called only by full original slot entry.
impl ResourceUse {
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }
    pub(super) fn verify_read_for(
        &self,
        original: &Arc<ResourceAdmission>,
    ) -> Result<(), UseError> {
        if !Arc::ptr_eq(&self.admission, original) {
            return Err(UseError::Unknown);
        }
        let state = self.admission.lock();
        if state.unknown || !state.valid() {
            return Err(UseError::Unknown);
        }
        if state.closed || !state.open {
            return Err(UseError::Closed);
        }
        match state.uses.get(self.index).and_then(|use_| *use_) {
            Some(use_) if use_.generation == self.generation && !use_.entered => Ok(()),
            _ => Err(UseError::Unknown),
        }
    }
    pub(super) fn begin_observation(
        &mut self,
        original: &Arc<ResourceAdmission>,
    ) -> Result<u64, UseError> {
        self.verify_read_for(original)?;
        if self.observed {
            return Err(UseError::Closed);
        }
        self.observed = true;
        Ok(self.generation)
    }
    pub(super) fn enter_effect(
        &mut self,
        validate_slot: impl FnOnce() -> bool,
    ) -> Result<(), UseError> {
        self.enter_effect_checked(|| {
            if validate_slot() {
                Ok(())
            } else {
                Err(UseError::Closed)
            }
        })
    }
    pub(super) fn enter_effect_checked(
        &mut self,
        validate_slot: impl FnOnce() -> Result<(), UseError>,
    ) -> Result<(), UseError> {
        let mut state = self.admission.lock();
        if !state.valid() || state.unknown {
            state.close_unknown();
            return Err(UseError::Unknown);
        }
        if !state.open || state.closed {
            return Err(UseError::Closed);
        }
        let current = state
            .uses
            .get(self.index)
            .and_then(|u| *u)
            .ok_or(UseError::Unknown)?;
        if current.generation != self.generation || current.entered {
            state.close_unknown();
            return Err(UseError::Unknown);
        }
        if let Err(error) = validate_slot() {
            if error == UseError::Unknown {
                state.close_unknown();
            }
            return Err(error);
        }
        // No decrement/reincrement: original native read stays counted throughout entry.
        state.uses[self.index]
            .as_mut()
            .expect("checked original use")
            .entered = true;
        Ok(())
    }
}
impl Drop for ResourceUse {
    fn drop(&mut self) {
        let mut state = self.admission.lock();
        match state.uses.get(self.index).and_then(|u| *u) {
            Some(current) if current.generation == self.generation => {
                state.unsettled |= current.entered;
                state.uses[self.index] = None;
                match state.count.checked_sub(1) {
                    Some(count) => state.count = count,
                    None => state.close_unknown(),
                }
            }
            _ => state.close_unknown(),
        }
        if !state.valid() {
            state.close_unknown();
        }
        self.admission.quiescent.notify_all();
    }
}
#[cfg(test)]
mod tests;
