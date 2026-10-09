//! Bounded delivery correlation for original owner requests, without operation authority.
use super::*;

const MAX_PENDING: usize = 64;
const MAX_POLLS: usize = 16;

/// Equality observations identifying an installed service, never an ownership grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalServiceInstallation {
    acquisition: Uuid,
    incarnation: Uuid,
    generation: u64,
}

/// Original delivery context. Payload identity must come from the family codec.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalRequestContext {
    installation: JournalServiceInstallation,
    correlation: Uuid,
    codec: u32,
    payload_identity: [u8; 32],
}
impl JournalRequestContext {
    /// Original installed-service equality observation.
    pub fn installation(self) -> JournalServiceInstallation {
        self.installation
    }
    /// UI-local correlation, distinct from an admitted OperationId.
    pub fn correlation(self) -> Uuid {
        self.correlation
    }
    /// Family codec/version observation.
    pub fn codec(self) -> u32 {
        self.codec
    }
    /// Exact canonical payload equality observation, not effect permission.
    pub fn payload_identity(self) -> [u8; 32] {
        self.payload_identity
    }
}

/// Delivery preserves a real owner outcome or unconfirmed channel loss.
pub enum JournalRequestDelivery<T> {
    /// Exact original owner result; success retains that command's semantics.
    Result(Result<T, OperationJournalOwnerError>),
    /// The original worker disconnected. This is not cancellation or rollback.
    Unconfirmed,
}
/// One consumed delivery with its original context and installation comparison.
pub struct JournalRequestCompletion<T> {
    /// Original correlation retained even for stale or lost delivery.
    pub context: JournalRequestContext,
    /// False prevents this delivery from confirming a replacement installation.
    pub matches_installation: bool,
    /// Exact result or retained unconfirmed state.
    pub delivery: JournalRequestDelivery<T>,
}

/// Local correlation admission or original-owner association refusal.
#[derive(Debug, thiserror::Error)]
pub enum JournalRequestInboxError {
    /// Invalid/duplicate correlation, zero codec or invalid byte bound.
    #[error("invalid journal request correlation context")]
    InvalidContext,
    /// All 64 original delivery slots are retained.
    #[error("journal request correlation slots are full")]
    Full,
    /// Outstanding encoded proposals exceed the existing four MiB bound.
    #[error("journal request correlation input bound exceeded")]
    InputBytesFull,
    /// A handle belongs to another actual owner, or was already consumed.
    #[error("journal request does not retain this original owner result permit")]
    ForeignOrConsumed,
    /// Original service-generation counter cannot wrap or reuse an identity.
    #[error("journal service generations exhausted")]
    GenerationExhausted,
    /// Existing owner admission refused before local reservation.
    #[error(transparent)]
    Owner(#[from] OperationJournalOwnerQueueError),
}

struct Pending<T> {
    context: JournalRequestContext,
    encoded_bytes: usize,
    request: OperationJournalRequest<T>,
}

/// Renderer-neutral fixed delivery slots on the existing owner's input pool.
///
/// Reserve before family submission, then adopt only that owner's original handle.
/// This does not prepare/admit a family, allocate an OperationId, cancel work,
/// own teardown or manufacture authority from a result. Dropping or detaching
/// releases delivery custody only. The actual family adapter remains responsible
/// for canonical encoding, live admission and operation completion semantics.
pub struct JournalRequestInbox<T> {
    slots: [Option<Pending<T>>; MAX_PENDING],
    client: OperationJournalClient,
    installation: JournalServiceInstallation,
    pending: usize,
    encoded_bytes: usize,
    cursor: usize,
    // Last: all original handles/contexts drop before refunding their fixed backing.
    _backing: InputPermit,
}
impl<T> JournalRequestInbox<T> {
    #[cfg(test)]
    pub(super) fn first_context_for_test(&self) -> Option<JournalRequestContext> {
        self.slots.iter().flatten().next().map(|p| p.context)
    }
    #[cfg(test)]
    pub(super) fn encoded_bytes_for_test(&self) -> usize {
        self.encoded_bytes
    }
    /// Retain the original acquisition and prepay fixed slots before construction.
    /// Startup and teardown still belong to the existing session/owner.
    pub fn new(client: OperationJournalClient) -> Result<Self, JournalRequestInboxError> {
        let (installation, backing) = {
            let _gate = client
                .shared
                .admission_gate
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            client.shared.ensure_available()?;
            let acquisition = client
                .original_profile_acquisition()
                .ok_or(OperationJournalOwnerQueueError::Initializing)?;
            let generation = client
                .shared
                .service_generation
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |previous| {
                    previous.checked_add(1)
                })
                .map_err(|_| JournalRequestInboxError::GenerationExhausted)?
                + 1;
            let backing = client
                .shared
                // Fixed retained cells plus one complete construction transient.
                .reserve_input(InputClass::Ordinary, 2 * std::mem::size_of::<Self>())?;
            (
                JournalServiceInstallation {
                    acquisition,
                    incarnation: client.shared.service_incarnation,
                    generation,
                },
                backing,
            )
        };
        Ok(Self {
            slots: std::array::from_fn(|_| None),
            client,
            installation,
            pending: 0,
            encoded_bytes: 0,
            cursor: 0,
            _backing: backing,
        })
    }
    /// Original installed-service equality observation for delivery fencing.
    pub fn installation(&self) -> JournalServiceInstallation {
        self.installation
    }
    /// Number of accepted original handles retained for delivery.
    pub fn pending(&self) -> usize {
        self.pending
    }
    /// Reserve context/input bounds before queueing through the original client.
    /// Dropping this reservation on queue rejection refunds local proposal accounting.
    pub fn reserve(
        &mut self,
        correlation: Uuid,
        codec: u32,
        payload_identity: [u8; 32],
        encoded_bytes: usize,
    ) -> Result<JournalRequestReservation<'_, T>, JournalRequestInboxError> {
        self.client.shared.ensure_available()?;
        if correlation.is_nil()
            || codec == 0
            || encoded_bytes == 0
            || encoded_bytes > OPERATION_JOURNAL_OWNER_MAX_QUEUED_INPUT_BYTES
            || self
                .slots
                .iter()
                .flatten()
                .any(|p| p.context.correlation == correlation)
        {
            return Err(JournalRequestInboxError::InvalidContext);
        }
        let index = self
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(JournalRequestInboxError::Full)?;
        let next = self
            .encoded_bytes
            .checked_add(encoded_bytes)
            .filter(|n| *n <= OPERATION_JOURNAL_OWNER_MAX_QUEUED_INPUT_BYTES)
            .ok_or(JournalRequestInboxError::InputBytesFull)?;
        let context = JournalRequestContext {
            installation: self.installation,
            correlation,
            codec,
            payload_identity,
        };
        self.encoded_bytes = next;
        self.pending += 1;
        Ok(JournalRequestReservation {
            inbox: self,
            index,
            context,
            encoded_bytes,
            active: true,
        })
    }
    /// Poll at most 16 actual handles, rotating through the fixed slots fairly.
    /// Empty retains custody; stale results retain their original context/outcome.
    /// No lock, wait, SQL, hashing or owner preparation runs here.
    pub fn poll(
        &mut self,
        installed: JournalServiceInstallation,
        mut deliver: impl FnMut(JournalRequestCompletion<T>),
    ) -> usize {
        let mut polls = 0;
        for _ in 0..MAX_PENDING {
            let index = self.cursor;
            self.cursor = (self.cursor + 1) % MAX_PENDING;
            let Some(pending) = self.slots[index].as_mut() else {
                continue;
            };
            polls += 1;
            let result = pending.request.try_result();
            let delivery = match result {
                Err(OperationJournalRequestError::Empty) => None,
                Err(OperationJournalRequestError::Disconnected) => {
                    Some(JournalRequestDelivery::Unconfirmed)
                }
                Ok(result) => Some(JournalRequestDelivery::Result(result)),
            };
            if let Some(delivery) = delivery {
                let pending = self.slots[index].take().expect("original pending delivery");
                self.pending -= 1;
                self.encoded_bytes -= pending.encoded_bytes;
                deliver(JournalRequestCompletion {
                    context: pending.context,
                    matches_installation: pending.context.installation == installed,
                    delivery,
                });
            }
            if polls == MAX_POLLS {
                break;
            }
        }
        polls
    }
    /// Detach delivery while returning correlation for subsequent owner recovery.
    /// This never cancels accepted work or acknowledges a durable disposition.
    pub fn detach(&mut self, correlation: Uuid) -> Option<JournalRequestContext> {
        let index = self.slots.iter().position(|p| {
            p.as_ref()
                .is_some_and(|p| p.context.correlation == correlation)
        })?;
        let pending = self.slots[index].take()?;
        self.pending -= 1;
        self.encoded_bytes -= pending.encoded_bytes;
        Some(pending.context)
    }
}

/// One clone-free local reservation made before original family queue submission.
pub struct JournalRequestReservation<'a, T> {
    inbox: &'a mut JournalRequestInbox<T>,
    index: usize,
    context: JournalRequestContext,
    encoded_bytes: usize,
    active: bool,
}
impl<T> JournalRequestReservation<'_, T> {
    /// Submit through the retained original client; this clones no teardown authority.
    pub fn client(&self) -> &OperationJournalClient {
        &self.inbox.client
    }
    /// Adopt the actual original result permit, retaining it even if shutdown raced.
    /// Refusal returns the untouched handle; it does not cancel its accepted work.
    pub fn accept(
        mut self,
        request: OperationJournalRequest<T>,
    ) -> Result<JournalRequestContext, (JournalRequestInboxError, OperationJournalRequest<T>)> {
        if !request
            .result_permit
            .as_ref()
            .is_some_and(|p| Arc::ptr_eq(&p.shared, &self.inbox.client.shared))
        {
            return Err((JournalRequestInboxError::ForeignOrConsumed, request));
        }
        self.inbox.slots[self.index] = Some(Pending {
            context: self.context,
            encoded_bytes: self.encoded_bytes,
            request,
        });
        self.active = false;
        Ok(self.context)
    }
}
impl<T> Drop for JournalRequestReservation<'_, T> {
    fn drop(&mut self) {
        if self.active {
            self.inbox.pending -= 1;
            self.inbox.encoded_bytes -= self.encoded_bytes;
        }
    }
}
