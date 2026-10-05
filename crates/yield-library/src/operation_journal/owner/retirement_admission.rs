//! Original-owner retirement acknowledgement. Physical admission remains unavailable.
use super::*;

pub(super) struct Pending {
    prepared: PreparedAdmissionInput,
    result: SyncSender<Result<OperationId, OperationJournalOwnerError>>,
    nonce: Uuid,
    // Command backing is destroyed before its original input debit.
    _input: InputPermit,
}

impl OwnerRuntime {
    pub(super) fn begin_retirement_admission(
        &mut self,
        prepared: PreparedAdmissionInput,
        input: InputPermit,
        result: SyncSender<Result<OperationId, OperationJournalOwnerError>>,
    ) {
        let checked = (|| {
            prepared.validate_shape()?;
            self.store().validate_for_admission()?;
            if self.retirement_admission.is_some() || self.wal_preparation_unconfirmed {
                return Err(WalPreparationError::Busy.into());
            }
            let digest = super::super::wal_admission::prepared_digest(
                &prepared.args,
                &prepared.participants,
                &prepared.plan,
                &prepared.claims,
            )?;
            let collector = self
                .wal_preparation
                .as_mut()
                .ok_or(WalPreparationError::InvalidInput)?;
            let UniversalCommandArgs::SourceRegistryRetirement(args) = &prepared.args else {
                return Err(WalPreparationError::InvalidInput.into());
            };
            collector.validate_retirement_policy(
                args,
                &prepared.participants,
                &prepared.plan,
                &prepared.claims,
                digest,
            )?;
            collector.begin_decision(digest)?;
            Ok::<_, OperationJournalOwnerError>(
                collector.decision_nonce().expect("original decision nonce"),
            )
        })();
        match checked {
            Ok(nonce) => {
                self.retirement_admission = Some(Pending {
                    prepared,
                    result,
                    nonce,
                    _input: input,
                });
            }
            Err(error) => {
                let _ = result.send(Err(error));
            }
        }
    }

    pub(super) fn poll_retirement_admission(&mut self) {
        let Some(pending) = self.retirement_admission.as_ref() else {
            return;
        };
        let ready = self.wal_preparation.as_ref().is_some_and(|collector| {
            collector.decision_nonce() == Some(pending.nonce) && collector.decision_acknowledged()
        });
        let lost = self
            .wal_preparation
            .as_ref()
            .is_none_or(|collector| collector.decision_nonce() != Some(pending.nonce));
        if !ready && !lost {
            return;
        }
        let pending = self.retirement_admission.take().expect("original command");
        // ACK is owner cleanup custody, never a qualified physical-family policy. In
        // particular, the generic journal LiveWitness cannot qualify these transitions.
        // No ID, capacity lease or journal SQL is created on this boundary.
        if let Some(collector) = self.wal_preparation.as_mut() {
            collector.refuse_decision(WalPreparationError::Canceled);
        }
        let operation = pending.prepared.validation_operation_id();
        let reason = if ready {
            format!(
                "retirement {operation} original owners acknowledged; physical-family admission is not qualified"
            )
        } else {
            "retirement original acknowledgement was canceled, expired or lost".into()
        };
        let _ = pending
            .result
            .send(Err(PreparedAdmissionError::Unsupported { reason }.into()));
    }
}
