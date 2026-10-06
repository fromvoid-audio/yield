//! Limited access to the original retained global owner; no mutation or shutdown authority.
use super::*;

/// Cloneable requester for empty pre-intent WAL preparation on the original global worker.
///
/// Clones share that worker's existing queue, admission/lifecycle state and strict handoff
/// byte budget. This handle retains no SQLite connection, profile ownership or worker join
/// handle. The actual [`GlobalLibraryWriter`] owns shutdown; closing its shared sender also
/// disconnects every clone. Requests and historical replies issue no journal decision or permit.
///
/// ```compile_fail
/// use yield_library::sample_sources::library::GlobalWalPreparationClient;
/// let forged = GlobalWalPreparationClient {};
/// ```
#[derive(Clone)]
pub struct GlobalWalPreparationClient {
    owner: GlobalPreparationOwner,
    binding_budget: Arc<binding::Budget>,
    // Reuse the exact existing queue machinery privately, without exposing vocabulary mutation.
    queue: ProfileTagVocabularyClient,
}

impl GlobalLibraryWriter {
    /// Issue limited cloneable preparation access to this exact worker registration.
    /// The requester shares existing admission/credit limits and cannot shut down the owner.
    pub fn wal_preparation_client(&self) -> GlobalWalPreparationClient {
        GlobalWalPreparationClient {
            owner: self.preparation_owner.clone(),
            binding_budget: Arc::clone(&self.preparation_binding_budget),
            queue: self.profile_tag_vocabulary_client(),
        }
    }
}

impl GlobalWalPreparationClient {
    /// Observe the original worker registration before forming a delayed request.
    /// Copying this diagnostic never supplies current reservation or settlement authority.
    pub fn preparation_owner(&self) -> GlobalPreparationOwner {
        self.owner.clone()
    }

    /// Read the original shared lifecycle without waiting for I/O.
    pub fn status(&self) -> GlobalLibraryWriterStatus {
        self.queue.status()
    }

    /// Queue bounded empty preparation on the original owner with its unchanged deadline.
    /// The worker validates the exact request registration, original connection and policy;
    /// submission grants no SQLite write, physical capacity, durable intent or decision latch.
    pub fn submit_preparation(
        &self,
        request: GlobalPreparationRequest,
    ) -> Result<GlobalPreparationPending, GlobalLibraryWriterQueueError> {
        self.queue.ensure_available()?;
        let binding_credit = if request.wal_binding {
            Some(self.binding_budget.reserve()?)
        } else {
            None
        };
        let handshake_credit = request
            .handshake
            .as_ref()
            .map(|budget| binding::SharedHandshakeCredit::reserve(budget))
            .transpose()?;
        let (ready, receiver) = mpsc::sync_channel(1);
        let canceled = Arc::new(AtomicBool::new(false));
        self.queue.send(WriterCommand::Prepare(Command {
            request,
            canceled: Arc::clone(&canceled),
            ready,
            binding_credit,
            handshake_credit: handshake_credit.clone(),
        }))?;
        Ok(GlobalPreparationPending {
            ready: receiver,
            canceled,
            armed: true,
            _handshake_credit: handshake_credit,
        })
    }
}

#[cfg(test)]
mod tests;
