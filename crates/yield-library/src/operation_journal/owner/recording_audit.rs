//! Original owner scheduling of read-only retained-byte observations.
use super::*;
use crate::operation_journal::{
    recording_commands::prefix::RecordingPrefixSeal, store::RecordingRestartAudit,
};

pub(super) struct Pending {
    audit: RecordingRestartAudit,
    input: InputPermit,
    result: SyncSender<Result<RecordingPrefixSeal, OperationJournalOwnerError>>,
}
impl OwnerRuntime {
    fn recording_audit_open(&self, input: &InputPermit) -> Result<(), OperationJournalOwnerError> {
        self.resource_admission.validate_open().map_err(|cause| {
            OperationJournalOwnerError::ChunkClaim {
                reason: format!("restart audit owner closed: {cause:?}"),
            }
        })?;
        self.store()
            .validate_namespace_with_input(Some(input), false)?;
        Ok(())
    }
    fn rollback_recording_audit(&self, input: &InputPermit) {
        if let Err(cause) = self.store().rollback_recording_terminal_write(input) {
            self.resource_admission.invalidate();
            input.shared.mark_startup_unavailable(&cause, input);
        }
    }
    pub(super) fn start_recording_restart_audit(
        &mut self,
        operation: OperationId,
        input: InputPermit,
        result: SyncSender<Result<RecordingPrefixSeal, OperationJournalOwnerError>>,
    ) {
        let started = (|| {
            if self.recording_producer.is_some() || self.recording_restart_audit.is_some() {
                return Err(OperationJournalOwnerError::ChunkClaim {
                    reason: "original recording verifier already retained".into(),
                });
            }
            self.recording_audit_open(&input)?;
            let working = self
                .store()
                .begin_recording_terminal_write_with_input(&input)?;
            let candidate = self
                .store()
                .start_recording_restart_audit(operation, &input);
            let candidate = match candidate {
                Ok(candidate) => candidate,
                Err(cause) => {
                    self.rollback_recording_audit(&input);
                    return Err(cause.into());
                }
            };
            if let Err(cause) = self.store().finish_recording_terminal_write(&input) {
                self.rollback_recording_audit(&input);
                return Err(cause.into());
            }
            self.recording_audit_open(&input)?;
            drop(working);
            Ok(candidate)
        })();
        match started {
            Ok(audit) => {
                self.recording_restart_audit = Some(Pending {
                    audit,
                    input,
                    result,
                })
            }
            Err(cause) => {
                let _ = result.send(Err(cause));
            }
        }
    }
    pub(super) fn poll_recording_restart_audit(&mut self) {
        let Some(mut pending) = self.recording_restart_audit.take() else {
            return;
        };
        let page = (|| {
            self.recording_audit_open(&pending.input)?;
            let working = self
                .store()
                .begin_recording_terminal_write_with_input(&pending.input)?;
            let candidate = self
                .store()
                .recording_restart_audit_page(&pending.audit, &pending.input);
            let candidate = match candidate {
                Ok(candidate) => candidate,
                Err(cause) => {
                    self.rollback_recording_audit(&pending.input);
                    return Err(cause.into());
                }
            };
            if let Err(cause) = self.store().finish_recording_terminal_write(&pending.input) {
                self.rollback_recording_audit(&pending.input);
                return Err(cause.into());
            }
            self.recording_audit_open(&pending.input)?;
            drop(working);
            Ok::<_, OperationJournalOwnerError>(candidate)
        })();
        match page {
            Ok((next, None)) => {
                pending.audit.verifier = next;
                self.recording_restart_audit = Some(pending);
            }
            Ok((_, Some(observed))) => {
                let _ = pending.result.send(Ok(observed));
            }
            Err(cause) => {
                let _ = pending.result.send(Err(cause));
            }
        }
    }
}
