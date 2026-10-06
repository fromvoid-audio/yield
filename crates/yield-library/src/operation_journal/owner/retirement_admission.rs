//! Original-owner retirement acknowledgement. Physical admission remains unavailable.
use super::*;

pub(super) struct Pending {
    prepared: PreparedAdmissionInput,
    result: SyncSender<Result<OperationId, OperationJournalOwnerError>>,
    phase: Phase,
    // Command backing is destroyed before its original input debit.
    _input: InputPermit,
}

enum Phase {
    Ready(super::super::wal_admission::OriginalReadyRound),
    Decision(Uuid),
}
// Separate prepaid metadata allowance; each overlapping round keeps its own debit.
const _: () = assert!(
    std::mem::size_of::<Phase>() + std::mem::size_of::<Option<u64>>()
        <= super::super::wal_admission::CurrentRound::READY_PHASE_BYTES
);

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
            let ready = collector.original_ready_round()?;
            collector.validate_retirement_policy(
                args,
                &prepared.participants,
                &prepared.plan,
                &prepared.claims,
                digest,
                &ready,
            )?;
            let phase = match collector.begin_decision_from_ready(digest, &ready) {
                Ok(()) => {
                    Phase::Decision(collector.decision_nonce().expect("original decision nonce"))
                }
                Err(WalPreparationError::Busy) => Phase::Ready(ready),
                Err(error) => return Err(error.into()),
            };
            Ok::<_, OperationJournalOwnerError>(phase)
        })();
        match checked {
            Ok(phase) => {
                self.retirement_admission = Some(Pending {
                    prepared,
                    result,
                    phase,
                    _input: input,
                });
            }
            Err(error) => {
                let _ = result.send(Err(error));
            }
        }
    }

    pub(super) fn poll_retirement_admission(&mut self) {
        let Some(mut pending) = self.retirement_admission.take() else {
            return;
        };
        if let Phase::Ready(original) = &pending.phase {
            let started = (|| {
                let digest = super::super::wal_admission::prepared_digest(
                    &pending.prepared.args,
                    &pending.prepared.participants,
                    &pending.prepared.plan,
                    &pending.prepared.claims,
                )?;
                let collector = self
                    .wal_preparation
                    .as_mut()
                    .ok_or(WalPreparationError::InvalidInput)?;
                let UniversalCommandArgs::SourceRegistryRetirement(args) = &pending.prepared.args
                else {
                    return Err(WalPreparationError::InvalidInput);
                };
                collector.validate_retirement_policy(
                    args,
                    &pending.prepared.participants,
                    &pending.prepared.plan,
                    &pending.prepared.claims,
                    digest,
                    original,
                )?;
                collector.begin_decision_from_ready(digest, original)?;
                Ok(collector
                    .decision_nonce()
                    .expect("single original decision nonce"))
            })();
            match started {
                Ok(nonce) => pending.phase = Phase::Decision(nonce),
                Err(WalPreparationError::Busy) => {
                    self.retirement_admission = Some(pending);
                    return;
                }
                Err(error) => {
                    let _ = pending.result.send(Err(error.into()));
                    return;
                }
            }
        }
        let Phase::Decision(nonce) = pending.phase else {
            unreachable!("original Ready phase resolved above");
        };
        let ready = self.wal_preparation.as_ref().is_some_and(|collector| {
            collector.decision_nonce() == Some(nonce) && collector.decision_acknowledged()
        });
        let lost = self
            .wal_preparation
            .as_ref()
            .is_none_or(|collector| collector.decision_nonce() != Some(nonce));
        if !ready && !lost {
            self.retirement_admission = Some(pending);
            return;
        }
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
