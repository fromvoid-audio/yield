//! Original publication lifetime fences; no capacity or namespace authority.
use super::resource_custody::UseError;
use super::{InputPermit, OperationId, OperationJournalOwnerError, OwnerShared};
use crate::sample_sources::db::SourceRegistrationId;
use std::sync::{Arc, Mutex, OnceLock};

mod custody;
use custody::Custody;

const MAX_FENCES: usize = 32;
// Preserve the original envelope where sufficient; fund any target-layout
// excess from the same existing pool and publication ceiling before allocation.
pub(super) const HEADER_BYTES: usize = if REQUIRED_HEADER_BYTES > 8192 {
    REQUIRED_HEADER_BYTES
} else {
    8192
};
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
    SettledNoEffect { digest: [u8; 32] },
}
struct Fence {
    operation: OperationId,
    source: SourceRegistrationId,
    generation: u64,
    incarnation: uuid::Uuid,
    output: OnceLock<uuid::Uuid>,
    state: Mutex<State>,
    detail: Mutex<Option<NoEffectDetail>>,
    // Last: scalar state drops before the original shared header debit.
    _backing: Custody<InputPermit>,
}
pub(super) struct PendingFence {
    fence: Custody<Fence>,
    entered: bool,
}
pub(super) struct Registry {
    slots: [Option<Custody<Fence>>; MAX_FENCES],
    incarnation: uuid::Uuid,
    next: u64,
    exhausted: bool,
    input: Option<Custody<InputPermit>>,
}
// Measure the nested optional-slot layout, plus32 retained cells and one
// conservative construction transient. Old eligible cells actually drop first.
const REQUIRED_HEADER_BYTES: usize = std::mem::size_of::<Registry>()
    + (MAX_FENCES + 1) * (std::mem::size_of::<Fence>() + 2 * std::mem::size_of::<usize>())
    + std::mem::size_of::<InputPermit>()
    + 2 * std::mem::size_of::<usize>()
    + super::super::capacity_provider::PUBLICATION_MIRROR_BYTES;
const _: () = assert!(REQUIRED_HEADER_BYTES <= HEADER_BYTES);
// The single prepaid debit covers shared admission plus all fixed slot cells.
// Per-slot completion bytes belong to slot backing, not shared admission state.
const SHARED_ADMISSION_BYTES: usize = std::mem::size_of::<Registry>()
    + std::mem::size_of::<InputPermit>()
    + 2 * std::mem::size_of::<usize>()
    + super::super::capacity_provider::PUBLICATION_MIRROR_BYTES;
const _: () = assert!(SHARED_ADMISSION_BYTES <= 16 * 1024);
const _: () = assert!(std::mem::size_of::<Fence>() + 2 * std::mem::size_of::<usize>() <= 64 * 1024);
const _: () = assert!(HEADER_BYTES <= super::PUBLICATION_INPUT_BYTES);

// Arc headers are two machine words; these private payload alignments need no extra pad.
const _: () = assert!(std::mem::align_of::<Fence>() <= std::mem::align_of::<usize>());
const _: () = assert!(std::mem::align_of::<InputPermit>() <= std::mem::align_of::<usize>());

fn quarantine(state: &mut State) {
    *state = match *state {
        State::CompletedNoEffect { digest, .. }
        | State::SettledNoEffect { digest }
        | State::UnknownCompletion { digest } => State::UnknownCompletion { digest },
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
            self.input = Some(Custody::new(
                shared
                    .reserve_publication_input(HEADER_BYTES)
                    .map_err(|error| {
                        refused(&format!(
                            "original publication fence backing unavailable: {error}"
                        ))
                    })?,
            ));
        }
        // The original registry is the sole remaining owner only after all executable
        // grant/entry custody dropped. Unsettled or unknown attempts are never reclaimed.
        for slot in &mut self.slots {
            let reusable = slot.as_ref().is_some_and(|fence| {
                fence.strong_count() == 1
                    && matches!(
                        fence.state.lock().map(|state| *state),
                        Ok(State::Pending | State::Closed | State::SettledNoEffect { .. })
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
        let fence = Custody::new(Fence {
            operation,
            source,
            generation,
            incarnation: self.incarnation,
            output: OnceLock::new(),
            state: Mutex::new(State::Pending),
            detail: Mutex::new(None),
            _backing: self
                .input
                .as_ref()
                .expect("prepaid original fence header")
                .clone(),
        });
        *slot = Some(fence.clone());
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
            .any(|original| original.ptr_eq(&fence.fence))
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
    pub(super) fn original_input(&self) -> Option<&InputPermit> {
        self.input.as_deref()
    }
    /// SQLite runs outside the slot mutex; completion does not need queue admission.
    pub(super) fn settle_no_effects(&self, mut persist: impl FnMut(&NoEffectCompletion) -> bool) {
        for fence in self.slots.iter().flatten() {
            let Some(output) = fence.output.get().copied() else {
                continue;
            };
            let digest = match fence.state.lock().map(|state| *state) {
                Ok(State::CompletedNoEffect { digest, .. }) => digest,
                _ => continue,
            };
            // Release the detail guard (including a poisoned guard) before acquiring state.
            // Worker completion always acquires state before detail.
            let captured_detail = match fence.detail.lock() {
                Ok(detail) => Ok(*detail),
                Err(poisoned) => {
                    drop(poisoned.into_inner());
                    Err(())
                }
            };
            let detail = match captured_detail {
                Ok(Some(detail)) => detail,
                Ok(None) => continue,
                Err(()) => {
                    match fence.state.lock() {
                        Ok(mut state) => quarantine(&mut state),
                        Err(poisoned) => quarantine(&mut poisoned.into_inner()),
                    }
                    continue;
                }
            };
            let completion = NoEffectCompletion {
                operation: fence.operation,
                output,
                incarnation: fence.incarnation,
                generation: fence.generation,
                digest,
                detail,
            };
            let confirmed = persist(&completion);
            match fence.state.lock() {
                Ok(mut state) => {
                    *state = match *state {
                        State::CompletedNoEffect {
                            digest: original, ..
                        } if confirmed && original == digest => State::SettledNoEffect { digest },
                        _ => State::UnknownCompletion { digest },
                    };
                }
                Err(poisoned) => quarantine(&mut poisoned.into_inner()),
            }
        }
    }
    pub(super) fn unresolved(&self) -> bool {
        self.slots.iter().flatten().any(|fence| {
            !matches!(
                fence.state.lock().map(|s| *s),
                Ok(State::Closed | State::SettledNoEffect { .. })
            )
        })
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
        State::SettledNoEffect { .. } => true,
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
/// Original live-latch observation; stored bytes cannot reconstruct this value.
/// No public constructor, Clone or decoder exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NoEffectDetail {
    kind: u8,
    length: u16,
    reason: [u8; crate::file_operation_owner::FILE_OPERATION_OWNER_MAX_DIAGNOSTIC_BYTES],
}
impl NoEffectDetail {
    fn capture(kind: u8, reason: &str) -> Option<Self> {
        if !(1..=4).contains(&kind)
            || reason.len() > crate::file_operation_owner::FILE_OPERATION_OWNER_MAX_DIAGNOSTIC_BYTES
        {
            return None;
        }
        let mut bytes = [0; crate::file_operation_owner::FILE_OPERATION_OWNER_MAX_DIAGNOSTIC_BYTES];
        bytes[..reason.len()].copy_from_slice(reason.as_bytes());
        Some(Self {
            kind,
            length: reason.len() as u16,
            reason: bytes,
        })
    }
    fn reason(&self) -> &str {
        std::str::from_utf8(&self.reason[..usize::from(self.length)])
            .expect("original bounded UTF-8 completion")
    }
}
pub(crate) struct NoEffectCompletion {
    operation: OperationId,
    output: uuid::Uuid,
    incarnation: uuid::Uuid,
    generation: u64,
    digest: [u8; 32],
    detail: NoEffectDetail,
}
impl NoEffectCompletion {
    pub(crate) fn evidence(&self) -> (u8, &str, [u8; 32]) {
        (self.detail.kind, self.detail.reason(), self.digest)
    }
    pub(crate) fn binding(&self) -> (OperationId, uuid::Uuid, uuid::Uuid, u64, [u8; 32]) {
        (
            self.operation,
            self.output,
            self.incarnation,
            self.generation,
            self.digest,
        )
    }
}
impl PendingFence {
    /// An unconfirmed durable write must retain the operation fence after issuer Drop.
    pub(super) fn quarantine_uncertain_write(&self) {
        let mut state = self
            .fence
            .state
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        quarantine(&mut state);
    }

    pub(super) fn bind_reserved_output(
        &self,
        output: uuid::Uuid,
    ) -> Result<(), OperationJournalOwnerError> {
        if output.is_nil() || self.fence.output.set(output).is_err() {
            return Err(refused("original publication output binding changed"));
        }
        Ok(())
    }
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
        let fence = self.fence.clone();
        move || {
            matches!(
                fence.state.lock().map(|s| *s),
                Ok(State::CompletedNoEffect { observed: true, .. } | State::SettledNoEffect { .. })
            )
        }
    }
    #[cfg(test)]
    pub(super) fn completion_observed(&self) -> bool {
        matches!(
            self.fence.state.lock().map(|s| *s),
            Ok(State::CompletedNoEffect { observed: true, .. } | State::SettledNoEffect { .. })
        )
    }
    // Only the exact original grant can publish; caller-data correlation is checked
    // against its sealed target before reaching this original slot.
    #[cfg(test)]
    pub(super) fn complete_before_effect(&self, digest: [u8; 32], exact: bool) {
        self.complete_before_effect_detail(digest, exact, 1, "");
    }
    pub(super) fn complete_before_effect_detail(
        &self,
        digest: [u8; 32],
        exact: bool,
        kind: u8,
        reason: &str,
    ) {
        let captured = NoEffectDetail::capture(kind, reason);
        let exact = exact && captured.is_some() && self.fence.generation != 0;
        match self.fence.state.lock() {
            Ok(mut state) => {
                // state→detail only; actor persistence holds neither mutex.
                let mut detail = match self.fence.detail.lock() {
                    Ok(detail) => detail,
                    Err(_) => {
                        quarantine(&mut state);
                        return;
                    }
                };
                let exact = exact && detail.is_none_or(|original| Some(original) == captured);
                if detail.is_none() {
                    *detail = captured;
                }
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
                    State::SettledNoEffect { digest: original }
                        if exact && original == digest && !self.entered =>
                    {
                        State::SettledNoEffect { digest: original }
                    }
                    State::SettledNoEffect { digest: original }
                    | State::CompletedNoEffect {
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
                    | State::SettledNoEffect { .. }
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
