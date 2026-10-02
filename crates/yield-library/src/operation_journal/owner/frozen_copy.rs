//! Privately issued persisted-copy family observations, never admission/completion authority.
use super::super::{
    coordinator::UniversalCommandArgs,
    participants::ParticipantSet,
    store::FrozenCopyData,
    work_plan::{WorkPlan, WorkUnitKey},
};
use super::*;
use std::sync::Weak;
#[cfg(test)]
mod tests;
/// Exact persisted initial copy/duplicate contract, issued only by the live journal owner.
/// No public constructor/decoder exists. This weak observation retains no profile lease.
#[derive(Clone, Debug)]
pub struct FrozenCopyObservation {
    data: FrozenCopyData,
    owner: Weak<OwnerShared>,
}
impl FrozenCopyObservation {
    /// Persisted operation identity.
    pub fn operation(&self) -> OperationId {
        self.data.operation
    }
    /// Validated exact copy/duplicate command, excluding rendered/move families.
    pub fn command(&self) -> &UniversalCommandArgs {
        &self.data.command
    }
    /// Original typed participant/source mapping.
    pub fn participants(&self) -> &ParticipantSet {
        &self.data.participants
    }
    /// Validated frozen planner result.
    pub fn work_plan(&self) -> &WorkPlan {
        &self.data.plan
    }
    /// Exactly one original required Harvest obligation.
    pub fn harvest_key(&self) -> WorkUnitKey {
        self.data.harvest
    }
    /// Actual issuing profile-root identity, not a path or caller UUID.
    pub fn profile_identity(&self) -> &str {
        &self.data.profile
    }
    /// Actual retained journal database identity.
    pub fn database_identity(&self) -> &str {
        &self.data.database
    }
    /// Canonical bounded historical family evidence for proposal correlation.
    /// Bytes cannot reconstruct this live owner-issued observation.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        self.data.canonical_bytes()
    }
    /// Revalidate exact frozen family and namespace through the actual retained owner queue.
    /// At most two seconds of caller waiting; no ownership is moved on timeout. Consumers
    /// must call this outside SQL transactions. It grants no physical or private completion.
    pub fn validate(&self) -> Result<(), FrozenCopyValidationError> {
        let shared = self
            .owner
            .upgrade()
            .ok_or(FrozenCopyValidationError::OwnerEnded)?;
        if shared.status() != OperationJournalOwnerStatus::Available {
            return Err(FrozenCopyValidationError::OwnerEnded);
        }
        let client = OperationJournalClient { shared };
        let mut request = client.observe_copy_intent(self.operation(), self.harvest_key())?;
        let current = request
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| FrozenCopyValidationError::ReplyUnavailable)?
            .map_err(|e| FrozenCopyValidationError::Owner(Box::new(e)))?
            .ok_or(FrozenCopyValidationError::Changed)?;
        if self.data != current.data {
            return Err(FrozenCopyValidationError::Changed);
        }
        Ok(())
    }
}
/// Fail-closed consumption failures; a timeout neither cancels nor transfers ownership.
#[derive(Debug, Error)]
pub enum FrozenCopyValidationError {
    /// The issuing journal owner closed or became unavailable.
    #[error("persisted-copy issuing owner ended")]
    OwnerEnded,
    /// The exact family/key/storage observations changed or disappeared.
    #[error("persisted-copy observation changed")]
    Changed,
    /// The fixed reply wait expired or disconnected.
    #[error("persisted-copy owner reply unavailable")]
    ReplyUnavailable,
    /// The bounded owner queue rejected revalidation.
    #[error(transparent)]
    Queue(#[from] OperationJournalOwnerQueueError),
    /// The actual owner/store rejected the retained family.
    #[error(transparent)]
    Owner(Box<OperationJournalOwnerError>),
}
impl OperationJournalClient {
    /// Queue one bounded privately issued observation of the persisted initial copy family.
    /// This verifies storage provenance; physical admission/capacity/publication remain separate.
    pub fn observe_copy_intent(
        &self,
        operation_id: OperationId,
        harvest_key: WorkUnitKey,
    ) -> Result<
        OperationJournalRequest<Option<FrozenCopyObservation>>,
        OperationJournalOwnerQueueError,
    > {
        if operation_id.as_uuid().is_nil()
            || harvest_key.participant_id().as_uuid().is_nil()
            || harvest_key.role() != super::super::work_plan::WorkUnitRole::Harvest
        {
            return Err(OperationJournalOwnerQueueError::InvalidInput {
                reason: "invalid persisted-copy operation/work key".into(),
            });
        }
        let observer_owner = Arc::downgrade(&self.shared);
        self.submit(CONTROL_INPUT_BYTES, move |input, result| {
            JournalCommand::ObserveCopy {
                operation_id,
                harvest_key,
                observer_owner,
                input,
                result,
            }
        })
    }
}
pub(super) fn execute(
    runtime: &OwnerRuntime,
    operation_id: OperationId,
    harvest_key: WorkUnitKey,
    owner: Weak<OwnerShared>,
) -> Result<Option<FrozenCopyObservation>, OperationJournalStoreError> {
    runtime
        .store()
        .read_frozen_copy(operation_id, harvest_key)
        .map(|data| data.map(|data| FrozenCopyObservation { data, owner }))
}
