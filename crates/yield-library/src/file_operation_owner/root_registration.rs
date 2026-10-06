//! Original root backing; no cleanup, namespace or publication authority.
use super::*;
use std::ops::Deref;

pub(super) struct RegistrationReply {
    result: Result<(), RegistrationFailure>,
    // Original refusal remains funded while queued or abandoned.
    _input: Option<InputPermit>,
}
#[derive(Clone)]
enum RegistrationFailure {
    Static(&'static str),
    Diagnostic(String),
}
impl RegistrationReply {
    pub(super) fn success() -> Self {
        Self {
            result: Ok(()),
            _input: None,
        }
    }
    pub(super) fn unfunded() -> Self {
        Self {
            result: Err(RegistrationFailure::Static(
                "root registration has no original backing",
            )),
            _input: None,
        }
    }
    pub(super) fn refusal(error: String, input: Option<InputPermit>) -> Self {
        Self {
            result: Err(RegistrationFailure::Diagnostic(error)),
            _input: input,
        }
    }
    pub(super) fn receive(self) -> Result<(), String> {
        // Return a caller-owned diagnostic; destroy the original queued image
        // before its credit. No raw original payload or alias escapes.
        match &self.result {
            Ok(()) => Ok(()),
            Err(RegistrationFailure::Static(error)) => Err((*error).to_owned()),
            Err(RegistrationFailure::Diagnostic(error)) => Err(error.clone()),
        }
    }
}

// All strong aliases use this private wrapper; no raw Arc or Weak escapes.
#[derive(Clone)]
pub(super) struct RootLiveness(Option<Arc<LivenessBacking>>);
struct LivenessBacking {
    value: AtomicBool,
    // Freed only after into_inner has destroyed the actual Arc allocation.
    _input: InputPermit,
}
impl RootLiveness {
    pub(super) const BACKING_BYTES: usize = std::mem::size_of::<(usize, usize, LivenessBacking)>();
    pub(super) fn new(shared: &Arc<OwnerShared>) -> Result<Self, FileOperationOwnerQueueError> {
        let input = shared.reserve_input(Self::BACKING_BYTES)?;
        Ok(Self(Some(Arc::new(LivenessBacking {
            value: AtomicBool::new(true),
            _input: input,
        }))))
    }
}
impl Deref for RootLiveness {
    type Target = AtomicBool;
    fn deref(&self) -> &AtomicBool {
        &self.0.as_deref().expect("original liveness alias").value
    }
}
impl Drop for RootLiveness {
    fn drop(&mut self) {
        if let Some(arc) = self.0.take() {
            // Pinned Rust into_inner frees the implicit Weak/header before returning
            // the backing. Every alias uses it, including concurrent last drops.
            drop(Arc::into_inner(arc));
        }
    }
}

type Slot = Option<(RootRegistrationRef, RootRegistrationState)>;

/// Exact original 128-slot policy, with no growing tree-node allocations.
pub(super) struct RootRegistry {
    slots: Option<Box<[Slot]>>,
    input: Option<InputPermit>,
}
impl RootRegistry {
    pub(super) fn new(shared: &Arc<OwnerShared>) -> Result<Self, FileOperationOwnerQueueError> {
        let bytes = std::mem::size_of::<Slot>()
            .checked_mul(FILE_OPERATION_OWNER_MAX_REGISTERED_ROOTS)
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Self>()))
            .ok_or(FileOperationOwnerQueueError::RecordTooLarge { actual: usize::MAX })?;
        let input = shared.reserve_input(bytes)?;
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(FILE_OPERATION_OWNER_MAX_REGISTERED_ROOTS)
            .map_err(|_| FileOperationOwnerQueueError::InputBytesFull { requested: bytes })?;
        if slots.capacity() != FILE_OPERATION_OWNER_MAX_REGISTERED_ROOTS {
            drop(slots);
            return Err(FileOperationOwnerQueueError::InputBytesFull { requested: bytes });
        }
        slots.resize_with(FILE_OPERATION_OWNER_MAX_REGISTERED_ROOTS, || None);
        Ok(Self {
            slots: Some(slots.into_boxed_slice()),
            input: Some(input),
        })
    }
    pub(super) fn get(&self, key: &RootRegistrationRef) -> Option<&RootRegistrationState> {
        self.slots
            .as_ref()?
            .iter()
            .flatten()
            .find_map(|(candidate, state)| (candidate == key).then_some(state))
    }
    pub(super) fn len(&self) -> usize {
        self.slots.as_ref().map_or(0, |slots| {
            slots.iter().filter(|slot| slot.is_some()).count()
        })
    }
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub(super) fn insert(&mut self, key: RootRegistrationRef, value: RootRegistrationState) {
        let slot = self
            .slots
            .as_mut()
            .expect("live root registry")
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("checked original root capacity");
        *slot = Some((key, value));
    }
    pub(super) fn remove(&mut self, key: &RootRegistrationRef) -> Option<RootRegistrationState> {
        let slot = self
            .slots
            .as_mut()?
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|(candidate, _)| candidate == key))?;
        slot.take().map(|(_, value)| value)
    }
    pub(super) fn clear(&mut self) {
        // Destroy every original capability and the actual slot allocation first.
        drop(self.slots.take());
        drop(self.input.take());
    }
}
impl Drop for RootRegistry {
    fn drop(&mut self) {
        self.clear();
    }
}
