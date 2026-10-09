//! Retained readonly recovery on the original journal worker; no write or capacity grant.
use super::*;
use crate::sample_sources::db::{LegacyAdoptionRecoveryImage, LegacySourceReadOwner};

// The Source capture uses the same bounded schema/row scratch as original adoption.
// The complete journal record receives a separate measured loan in its visitor.
const INPUT_BYTES: usize = legacy_adoption_custody::INPUT_BYTES;

/// Historical evidence tied to a retained readonly Source on its issuing journal worker.
/// This is not an admission, current Source lease, migration success or audit grant.
///
/// ```compile_fail
/// fn clone(v: yield_library::operation_journal::owner::LegacyRecoveryCustody) { let _ = v.clone(); }
/// ```
/// ```compile_fail
/// let _ = yield_library::operation_journal::owner::LegacyRecoveryCustody {};
/// ```
pub struct LegacyRecoveryCustody {
    operation: OperationId,
    owner: std::sync::Weak<OwnerShared>,
}
impl LegacyRecoveryCustody {
    /// Original persisted identity, which does not establish a live writable owner.
    pub fn operation_id(&self) -> OperationId {
        self.operation
    }

    /// Reinspect the actual receipt and journal row on the original worker. Lost replies
    /// retain its Source reservation. This does not replay a migration or bootstrap body.
    pub fn revalidate(
        &self,
    ) -> Result<OperationJournalRequest<()>, OperationJournalOwnerQueueError> {
        let shared = self
            .owner
            .upgrade()
            .ok_or(OperationJournalOwnerQueueError::Closed)?;
        let operation = self.operation;
        OperationJournalClient { shared }.submit_funded_with_reply(
            InputClass::Ordinary,
            INPUT_BYTES,
            false,
            true,
            move |input, result| JournalCommand::ValidateLegacyRecovery {
                operation,
                capacity: false,
                input,
                result,
            },
        )
    }

    /// Check immutable initial claims against the V19 plan, complete unreleased rebuild
    /// image and current provider ledger. This read-only confirmation issues no replacement
    /// provisional lease, physical allocation or writable Source authority.
    pub fn validate_claims(
        &self,
    ) -> Result<OperationJournalRequest<()>, OperationJournalOwnerQueueError> {
        let shared = self
            .owner
            .upgrade()
            .ok_or(OperationJournalOwnerQueueError::Closed)?;
        let operation = self.operation;
        OperationJournalClient { shared }.submit_funded_with_reply(
            InputClass::Ordinary,
            INPUT_BYTES,
            false,
            true,
            move |input, result| JournalCommand::ValidateLegacyRecovery {
                operation,
                capacity: true,
                input,
                result,
            },
        )
    }
}

pub(super) struct RetainedLegacyRecovery {
    // SQLite/root/sidecar custody and the bounded image all die before input credit.
    source: LegacySourceReadOwner,
    image: Option<LegacyAdoptionRecoveryImage>,
    _input: InputPermit,
}

impl OperationJournalClient {
    /// Transfer an already-open no-effect recovery reader to the journal worker. It
    /// retains that reader even if inspection fails or the caller drops the reply.
    /// No current ownership, capacity or writable promotion is reconstructed from bytes.
    pub fn recover_legacy_read_custody(
        &self,
        source: LegacySourceReadOwner,
    ) -> Result<OperationJournalRequest<LegacyRecoveryCustody>, OperationJournalOwnerQueueError>
    {
        self.submit_funded_with_reply(
            InputClass::Ordinary,
            INPUT_BYTES,
            false,
            true,
            move |input, result| JournalCommand::RecoverLegacyReadCustody {
                source,
                input,
                result,
            },
        )
    }
}

impl OwnerRuntime {
    pub(super) fn recover_legacy_read_custody(
        &mut self,
        source: LegacySourceReadOwner,
        input: InputPermit,
    ) -> Result<LegacyRecoveryCustody, OperationJournalOwnerError> {
        use LegacyAdoptionCustodyError as E;
        if self.legacy_adoption.is_some() || self.legacy_recovery.is_some() {
            return Err(E::Busy.into());
        }
        if input.shared.status() != OperationJournalOwnerStatus::Available {
            return Err(E::Closed.into());
        }
        let owner = Arc::downgrade(&input.shared);
        // Install before inspection: malformed evidence cannot accidentally make this
        // acquisition available for a new migration or sidecar replacement.
        self.legacy_recovery = Some(RetainedLegacyRecovery {
            source,
            image: None,
            _input: input,
        });
        let retained = self
            .legacy_recovery
            .as_ref()
            .expect("installed recovery custody");
        retained._input.fund_historical_consumer()?;
        let profile = self.final_profile_guard.as_ref().ok_or(E::Closed)?;
        let image = retained
            .source
            .capture_recovery_inspected(profile, |operation, request| {
                self.store()
                    .inspect_legacy_recovery_intent(
                        profile,
                        operation,
                        request,
                        &retained._input,
                        None,
                    )
                    .map_err(|_| {
                        crate::sample_sources::db::LegacyObservationError(
                            "funded recovery intent inspection refused",
                        )
                    })
            })
            .map_err(|_| E::Changed)?;
        if retained._input.shared.status() != OperationJournalOwnerStatus::Available {
            return Err(E::Closed.into());
        }
        let operation = image.operation_id();
        self.legacy_recovery
            .as_mut()
            .expect("retained recovery custody")
            .image = Some(image);
        Ok(LegacyRecoveryCustody { operation, owner })
    }

    pub(super) fn validate_legacy_recovery(
        &self,
        operation: OperationId,
        capacity: bool,
        input: InputPermit,
    ) -> Result<(), OperationJournalOwnerError> {
        use LegacyAdoptionCustodyError as E;
        let retained = self.legacy_recovery.as_ref().ok_or(E::Closed)?;
        let expected = retained.image.as_ref().ok_or(E::Changed)?;
        if expected.operation_id() != operation {
            return Err(E::Changed.into());
        }
        if input.shared.status() != OperationJournalOwnerStatus::Available {
            return Err(E::Closed.into());
        }
        input.fund_historical_consumer()?;
        let snapshot = if capacity {
            let snapshot = self.store().rebuild_admission_snapshot_for_input(&input)?;
            if !matches!(snapshot.readiness(), CapacityRebuildReadiness::Ready) {
                return Err(E::ReserveRecovery.into());
            }
            let provider = self.capacity_provider.as_ref().ok_or(E::ReserveRecovery)?;
            snapshot
                .validate_provider(provider, Some(&input))
                .map_err(|_| E::ReserveRecovery)?;
            provider
                .validate_recovery_state()
                .map_err(|_| E::ReserveRecovery)?;
            Some(snapshot)
        } else {
            None
        };
        let profile = self.final_profile_guard.as_ref().ok_or(E::Closed)?;
        let actual = retained
            .source
            .capture_recovery_inspected(profile, |operation, request| {
                self.store()
                    .inspect_legacy_recovery_intent(
                        profile,
                        operation,
                        request,
                        &input,
                        snapshot.as_ref().map(|snapshot| snapshot.claims()),
                    )
                    .map_err(|_| {
                        crate::sample_sources::db::LegacyObservationError(
                            "funded recovery intent inspection refused",
                        )
                    })
            })
            .map_err(|_| E::Changed)?;
        if actual.operation_id() != operation
            || actual.request() != expected.request()
            || actual.lease_table_present() != expected.lease_table_present()
            || input.shared.status() != OperationJournalOwnerStatus::Available
        {
            return Err(E::Changed.into());
        }
        Ok(())
    }
}
