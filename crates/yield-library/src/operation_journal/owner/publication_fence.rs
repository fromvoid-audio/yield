//! Original publication lifetime fences; no capacity or namespace authority.
use super::resource_custody::UseError;
use super::{InputPermit, OperationId, OperationJournalOwnerError, OwnerShared};
use crate::sample_sources::db::SourceRegistrationId;
use std::sync::{Arc, Mutex};

const MAX_FENCES: usize = 32;
pub(super) const HEADER_BYTES: usize = 8192;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Pending,
    WorkerActive,
    Entered,
    Unsettled,
    Closed,
    Unknown,
    CompletedNoEffect { digest: [u8; 32], observed: bool },
    UnknownCompletion { digest: [u8; 32] },
}
struct Fence {
    operation: OperationId,
    source: SourceRegistrationId,
    generation: u64,
    state: Mutex<State>,
}
pub(super) struct PendingFence {
    fence: Arc<Fence>,
    entered: bool,
}
pub(super) struct Registry {
    slots: [Option<Arc<Fence>>; MAX_FENCES],
    incarnation: uuid::Uuid,
    next: u64,
    exhausted: bool,
    input: Option<InputPermit>,
}
const _: () = assert!(
    std::mem::size_of::<Registry>()
        + MAX_FENCES * (std::mem::size_of::<Fence>() + 2 * std::mem::size_of::<usize>())
        + super::super::capacity_provider::PUBLICATION_MIRROR_BYTES
        <= HEADER_BYTES
);
fn quarantine(state: &mut State) {
    *state = match *state {
        State::CompletedNoEffect { digest, .. } | State::UnknownCompletion { digest } => {
            State::UnknownCompletion { digest }
        }
        _ => State::Unknown,
    };
}
fn refused(reason: &str) -> OperationJournalOwnerError {
    OperationJournalOwnerError::StagingGrant {
        reason: reason.into(),
    }
}
impl Registry {
    pub(super) fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            incarnation: uuid::Uuid::new_v4(),
            next: 0,
            exhausted: false,
            input: None,
        }
    }
    #[cfg(test)]
    pub(super) fn issue(
        &mut self,
        operation: OperationId,
        shared: &Arc<OwnerShared>,
    ) -> Result<PendingFence, OperationJournalOwnerError> {
        self.issue_for_source(
            operation,
            SourceRegistrationId::model_key_for_tests(0),
            shared,
        )
    }
    pub(super) fn issue_for_source(
        &mut self,
        operation: OperationId,
        source: SourceRegistrationId,
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
        if self.slots.iter().flatten().any(|fence| {
            matches!(
                fence.state.lock().map(|s| *s),
                Err(_) | Ok(State::Unknown | State::UnknownCompletion { .. })
            )
        }) {
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
            source,
            generation,
            state: Mutex::new(State::Pending),
        });
        *slot = Some(Arc::clone(&fence));
        Ok(PendingFence {
            fence,
            entered: false,
        })
    }
    /// Historical issuer identity only; it cannot acquire or reconstruct a fence.
    pub(super) fn issuance_observation(&self, fence: &PendingFence) -> Option<(uuid::Uuid, u64)> {
        self.slots
            .iter()
            .flatten()
            .any(|original| Arc::ptr_eq(original, &fence.fence))
            .then_some((self.incarnation, fence.fence.generation))
    }
    // Bounded original actor scan. Observation is deliberately not settlement.
    pub(super) fn poll_completions(&self) {
        for fence in self.slots.iter().flatten() {
            match fence.state.lock() {
                Ok(mut state) => {
                    if let State::CompletedNoEffect { observed, .. } = &mut *state {
                        *observed = true;
                    }
                }
                Err(poisoned) => quarantine(&mut poisoned.into_inner()),
            }
        }
    }
    pub(super) fn unresolved(&self) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|fence| !matches!(fence.state.lock().map(|s| *s), Ok(State::Closed)))
    }
    pub(super) fn close_for_source(
        &self,
        source: SourceRegistrationId,
    ) -> Result<(), OperationJournalOwnerError> {
        for fence in self
            .slots
            .iter()
            .flatten()
            .filter(|fence| fence.source == source)
        {
            if !close_pending(fence) {
                return Err(
                    OperationJournalOwnerError::PublicationSourceRetirementPending {
                        operation_id: fence.operation,
                    },
                );
            }
        }
        Ok(())
    }
    pub(super) fn close_for_mutation(
        &self,
        operation: OperationId,
    ) -> Result<(), OperationJournalOwnerError> {
        self.close_for_cancellation(operation).map_err(|_| {
            OperationJournalOwnerError::PublicationMutationPending {
                operation_id: operation,
            }
        })
    }
    pub(super) fn close_for_charge_release(
        &self,
        operation: OperationId,
    ) -> Result<(), OperationJournalOwnerError> {
        self.close_for_cancellation(operation).map_err(|_| {
            OperationJournalOwnerError::PublicationChargeReleasePending {
                operation_id: operation,
            }
        })
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
            if !close_pending(fence) {
                return Err(OperationJournalOwnerError::PublicationCancellationPending {
                    operation_id: operation,
                });
            }
        }
        Ok(())
    }
}
fn close_pending(fence: &Fence) -> bool {
    let Ok(mut state) = fence.state.lock() else {
        return false;
    };
    match *state {
        State::Pending | State::Closed => {
            *state = State::Closed;
            true
        }
        State::WorkerActive
        | State::Entered
        | State::Unsettled
        | State::Unknown
        | State::CompletedNoEffect { .. }
        | State::UnknownCompletion { .. } => false,
    }
}
impl PendingFence {
    #[cfg(all(test, unix))]
    pub(super) fn generation_observation_for_test(&self) -> u64 {
        self.fence.generation
    }

    pub(super) fn original_worker_started(&self) {
        match self.fence.state.lock() {
            Ok(mut state) if *state == State::Pending => *state = State::WorkerActive,
            Ok(state) if *state == State::Closed => {}
            Ok(mut state) => quarantine(&mut state),
            Err(poisoned) => quarantine(&mut poisoned.into_inner()),
        }
    }
    #[cfg(all(test, unix))]
    pub(super) fn completion_observer(&self) -> impl Fn() -> bool + use<> {
        let fence = Arc::clone(&self.fence);
        move || {
            matches!(
                fence.state.lock().map(|s| *s),
                Ok(State::CompletedNoEffect { observed: true, .. })
            )
        }
    }
    #[cfg(test)]
    pub(super) fn completion_observed(&self) -> bool {
        matches!(
            self.fence.state.lock().map(|s| *s),
            Ok(State::CompletedNoEffect { observed: true, .. })
        )
    }
    // Only the exact original grant can publish; caller-data correlation is checked
    // against its sealed target before reaching this original slot.
    pub(super) fn complete_before_effect(&self, digest: [u8; 32], exact: bool) {
        let exact = exact && self.fence.generation != 0;
        match self.fence.state.lock() {
            Ok(mut state) => {
                *state = match *state {
                    State::Pending | State::Closed | State::WorkerActive
                        if exact && !self.entered =>
                    {
                        State::CompletedNoEffect {
                            digest,
                            observed: false,
                        }
                    }
                    State::CompletedNoEffect {
                        digest: original,
                        observed,
                    } if exact && original == digest && !self.entered => State::CompletedNoEffect {
                        digest: original,
                        observed,
                    },
                    State::CompletedNoEffect {
                        digest: original, ..
                    }
                    | State::UnknownCompletion { digest: original } => {
                        State::UnknownCompletion { digest: original }
                    }
                    _ => State::UnknownCompletion { digest },
                };
            }
            Err(poisoned) => quarantine(&mut poisoned.into_inner()),
        }
    }
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
            Ok(mut state) if matches!(*state, State::Pending | State::WorkerActive) => {
                *state = State::Entered;
                self.entered = true;
                Ok(())
            }
            Ok(state) if matches!(*state, State::Unknown | State::UnknownCompletion { .. }) => {
                Err(UseError::Unknown)
            }
            Ok(_) => Err(UseError::Closed),
            Err(poisoned) => {
                quarantine(&mut poisoned.into_inner());
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
                (
                    State::Closed
                    | State::Unsettled
                    | State::Unknown
                    | State::CompletedNoEffect { .. }
                    | State::UnknownCompletion { .. },
                    _,
                ) => {}
                _ => *state = State::Unknown,
            },
            Err(poisoned) => quarantine(&mut poisoned.into_inner()),
        }
    }
}
#[cfg(test)]
mod tests;
