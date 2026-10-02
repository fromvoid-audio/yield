//! Fixed-budget persisted family facts; this read never admits physical execution.
use super::super::{
    codec::CappedWriter,
    coordinator::UniversalCommandArgs,
    participants::ParticipantSet,
    work_plan::{WorkPlan, WorkUnitKey, WorkUnitRole, derive_work_plan},
};
use super::*;
use serde::Serialize;
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct FrozenCopyData {
    pub(crate) operation: OperationId,
    pub(crate) command: UniversalCommandArgs,
    pub(crate) participants: ParticipantSet,
    pub(crate) plan: WorkPlan,
    pub(crate) harvest: WorkUnitKey,
    pub(crate) profile: String,
    pub(crate) database: String,
    pub(crate) binding: String,
}
impl FrozenCopyData {
    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut writer = CappedWriter::new(65536);
        serde_json::to_writer(&mut writer, self)?;
        Ok(writer.finish())
    }
}
impl OperationJournalStore {
    pub(crate) fn read_frozen_copy(
        &self,
        operation: OperationId,
        key: WorkUnitKey,
    ) -> Result<Option<FrozenCopyData>, OperationJournalStoreError> {
        let bad = || {
            audit_error(
                &self.database_path,
                "invalid, unsupported or unbounded persisted copy observation",
            )
        };
        if operation.as_uuid().is_nil()
            || key.participant_id().as_uuid().is_nil()
            || key.role() != WorkUnitRole::Harvest
        {
            return Err(bad());
        }
        self.validate_namespace()?;
        // One statement sees both journal families. CASE prevents oversized or mistyped BLOB
        // materialization; LIMIT 2 rejects duplicate keys and cross-family ambiguity.
        let mut statement=self.connection.prepare("SELECT family,sequence, CASE WHEN typeof(operation_id)='text' AND typeof(sequence)='integer' AND sequence>0 AND typeof(record)='blob' AND length(record) BETWEEN 1 AND 65536 THEN record END FROM (SELECT 'legacy' AS family,operation_id,sequence,record FROM operation_journal WHERE operation_id=?1 UNION ALL SELECT 'full',operation_id,sequence,record FROM operation_journal_full_progress WHERE operation_id=?1) LIMIT 2")?;
        let mut rows = statement.query([operation.to_string()])?;
        let Some(row) = rows.next()? else {
            self.validate_namespace()?;
            return Ok(None);
        };
        let family: String = row.get(0)?;
        let sequence: i64 = row.get(1)?;
        let encoded: Option<Vec<u8>> = row.get(2)?;
        if rows.next()?.is_some() {
            return Err(bad());
        }
        let encoded = encoded.ok_or_else(bad)?;
        self.ensure_sequence_unique(sequence)?;
        let (command, participants, plan) = if family == "legacy" {
            let record = decode(&encoded).map_err(|_| bad())?;
            if record.operation_id != operation
                || encode(&record).map_err(|_| bad())? != encoded
                || !matches!(classify_record(&record), RecordConsistency::Consistent)
                || record.cancellation_request().is_some()
                || !matches!(
                    record.disposition,
                    super::super::model::OperationDisposition::None
                        | super::super::model::OperationDisposition::RetryPending
                        | super::super::model::OperationDisposition::PartialNeedsRetry
                        | super::super::model::OperationDisposition::Succeeded
                        | super::super::model::OperationDisposition::SucceededWithDeferredArtifacts
                )
            {
                return Err(bad());
            }
            let command = record
                .universal_command_args()
                .cloned()
                .or_else(|| {
                    record
                        .command_args()
                        .cloned()
                        .map(UniversalCommandArgs::Copy)
                })
                .ok_or_else(bad)?;
            let participants = record
                .universal_intent
                .as_ref()
                .ok_or_else(bad)?
                .participants()
                .clone();
            let plan = derive_work_plan(&command, &participants).map_err(|_| bad())?;
            (command, participants, plan)
        } else {
            let record = super::super::full_progress_codec::decode(&encoded).map_err(|_| bad())?;
            if super::super::full_progress_codec::encode(&record).map_err(|_| bad())? != encoded {
                return Err(bad());
            }
            let snapshot = record.snapshot();
            let intent = snapshot.intent();
            if intent.operation_id()!=operation||snapshot.cancellation_request().is_some()
                || !matches!(snapshot.disposition(),super::super::full_status::FullProgressDisposition::None|super::super::full_status::FullProgressDisposition::RetryPending|super::super::full_status::FullProgressDisposition::PartialNeedsRetry|super::super::full_status::FullProgressDisposition::Succeeded|super::super::full_status::FullProgressDisposition::SucceededWithDeferredArtifacts){return Err(bad());}
            (
                intent.args().clone(),
                intent.participants().clone(),
                intent.plan().clone(),
            )
        };
        if !matches!(
            command,
            UniversalCommandArgs::Copy(_) | UniversalCommandArgs::DuplicateSame(_)
        ) {
            return Err(bad());
        }
        plan.validate_against(&command, &participants)
            .map_err(|_| bad())?;
        let harvest: Vec<_> = plan
            .units()
            .iter()
            .filter(|unit| unit.is_applicable() && unit.key().role() == WorkUnitRole::Harvest)
            .collect();
        if harvest.len() != 1
            || harvest[0].key() != key
            || plan
                .units()
                .iter()
                .filter(|unit| {
                    unit.is_applicable() && unit.key().role() == WorkUnitRole::GlobalLibrary
                })
                .count()
                != 1
        {
            return Err(bad());
        }
        let data = FrozenCopyData {
            operation,
            command,
            participants,
            plan,
            harvest: key,
            profile: self.profile_guard.profile_root_identity().into(),
            database: self.database_identity.clone(),
            binding: self.database_root.identity().into(),
        };
        if [&data.profile, &data.database, &data.binding]
            .iter()
            .any(|s| s.is_empty() || s.len() > 256 || s.contains('\0'))
        {
            return Err(bad());
        }
        data.canonical_bytes().map_err(|_| bad())?;
        self.validate_namespace()?;
        Ok(Some(data))
    }
}
