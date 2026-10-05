//! Original issuer versus cancellation only; no capacity or namespace authority.
use super::resource_custody::UseError;
use super::{InputPermit, OperationId, OperationJournalOwnerError, OwnerShared};
use std::sync::{Arc, Mutex};

const MAX_FENCES: usize = 32;
pub(super) const HEADER_BYTES: usize = 8192;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Pending,
    Entered,
    Unsettled,
    Closed,
    Unknown,
}
struct Fence {
    operation: OperationId,
    generation: u64,
    state: Mutex<State>,
}
pub(super) struct PendingFence {
    fence: Arc<Fence>,
    entered: bool,
}
pub(super) struct Registry {
    slots: [Option<Arc<Fence>>; MAX_FENCES],
    next: u64,
    exhausted: bool,
    input: Option<InputPermit>,
}
const _: () = assert!(
    std::mem::size_of::<Registry>()
        + MAX_FENCES * (std::mem::size_of::<Fence>() + 2 * std::mem::size_of::<usize>())
        <= HEADER_BYTES
);
fn refused(reason: &str) -> OperationJournalOwnerError {
    OperationJournalOwnerError::StagingGrant {
        reason: reason.into(),
    }
}
impl Registry {
    pub(super) fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            next: 0,
            exhausted: false,
            input: None,
        }
    }
    pub(super) fn issue(
        &mut self,
        operation: OperationId,
        shared: &Arc<OwnerShared>,
    ) -> Result<PendingFence, OperationJournalOwnerError> {
        if self.exhausted {
            return Err(refused("original publication fence generations exhausted"));
        }
        if self
            .input
            .as_ref()
            .is_some_and(|input| !Arc::ptr_eq(&input.shared, shared))
        {
            return Err(refused(
                "publication fence belongs to another original runtime",
            ));
        }
        if self
            .slots
            .iter()
            .flatten()
            .any(|fence| matches!(fence.state.lock().map(|s| *s), Err(_) | Ok(State::Unknown)))
        {
            self.exhausted = true;
            shared.resource_admission.invalidate();
            return Err(refused("original publication fence state is unknown"));
        }
        if self.input.is_none() {
            self.input = Some(
                shared
                    .reserve_publication_input(HEADER_BYTES)
                    .map_err(|error| {
                        refused(&format!(
                            "original publication fence backing unavailable: {error}"
                        ))
                    })?,
            );
        }
        // The original registry is the sole remaining owner only after all executable
        // grant/entry custody dropped. Unsettled or unknown attempts are never reclaimed.
        for slot in &mut self.slots {
            let reusable = slot.as_ref().is_some_and(|fence| {
                Arc::strong_count(fence) == 1
                    && matches!(
                        fence.state.lock().map(|state| *state),
                        Ok(State::Pending | State::Closed)
                    )
            });
            if reusable {
                *slot = None;
            }
        }
        if self
            .slots
            .iter()
            .flatten()
            .any(|fence| fence.operation == operation)
        {
            return Err(refused(
                "original operation already has publication fence custody",
            ));
        }
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or_else(|| refused("original publication fence registry is full"))?;
        let Some(generation) = self.next.checked_add(1) else {
            self.exhausted = true;
            return Err(refused("original publication fence generations exhausted"));
        };
        self.next = generation;
        let fence = Arc::new(Fence {
            operation,
            generation,
            state: Mutex::new(State::Pending),
        });
        *slot = Some(Arc::clone(&fence));
        Ok(PendingFence {
            fence,
            entered: false,
        })
    }
    pub(super) fn unresolved(&self) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|fence| !matches!(fence.state.lock().map(|s| *s), Ok(State::Closed)))
    }
    pub(super) fn close_for_cancellation(
        &self,
        operation: OperationId,
    ) -> Result<(), OperationJournalOwnerError> {
        for fence in self
            .slots
            .iter()
            .flatten()
            .filter(|fence| fence.operation == operation)
        {
            let Ok(mut state) = fence.state.lock() else {
                return Err(OperationJournalOwnerError::PublicationCancellationPending {
                    operation_id: operation,
                });
            };
            match *state {
                State::Pending | State::Closed => *state = State::Closed,
                State::Entered | State::Unsettled | State::Unknown => {
                    return Err(OperationJournalOwnerError::PublicationCancellationPending {
                        operation_id: operation,
                    });
                }
            }
        }
        Ok(())
    }
}
impl PendingFence {
    // Called only under original resource admission, establishing admission→fence order.
    #[cfg(test)]
    pub(super) fn enter(&mut self) -> bool {
        self.enter_checked().is_ok()
    }
    pub(super) fn enter_checked(&mut self) -> Result<(), UseError> {
        if self.entered || self.fence.generation == 0 {
            return Err(UseError::Unknown);
        }
        match self.fence.state.lock() {
            Ok(mut state) if *state == State::Pending => {
                *state = State::Entered;
                self.entered = true;
                Ok(())
            }
            Ok(state) if *state == State::Unknown => Err(UseError::Unknown),
            Ok(_) => Err(UseError::Closed),
            Err(poisoned) => {
                *poisoned.into_inner() = State::Unknown;
                Err(UseError::Unknown)
            }
        }
    }
}
impl Drop for PendingFence {
    fn drop(&mut self) {
        match self.fence.state.lock() {
            Ok(mut state) => match (*state, self.entered) {
                (State::Pending, false) => *state = State::Closed,
                (State::Entered, true) => *state = State::Unsettled,
                (State::Closed | State::Unsettled | State::Unknown, _) => {}
                _ => *state = State::Unknown,
            },
            Err(poisoned) => *poisoned.into_inner() = State::Unknown,
        }
    }
}
#[cfg(test)]
mod tests;
