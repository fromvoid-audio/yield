//! V19 original pre-lease initial intent. Stored bytes confer no custody or migration grant.
use super::*;

impl StoredOperationRecord {
    pub(crate) fn new_v19_legacy_adoption(
        operation: OperationId,
        args: super::super::legacy_adoption_commands::LegacyAdoptionIntentArgs,
        timestamp: i64,
    ) -> Result<Self, super::super::legacy_adoption_commands::LegacyAdoptionInputError> {
        use super::super::model::UniversalCommandKind;
        let plan = args.initial_capacity_plan()?;
        let intent = UniversalIntent::new(
            None,
            UniversalCommandKind::SourceLegacyAdoption,
            "operation.status.source_legacy_adoption",
            args.participant_observation()?,
        )
        .map_err(|_| super::super::legacy_adoption_commands::LegacyAdoptionInputError::Invalid)?;
        let mut record = Self::new_v16_command(
            operation,
            OperationIntent {
                actor: OperationActor::User,
                kind: OperationKind::SourceLegacyAdoption,
                label: "Adopt legacy source".into(),
            },
            intent,
            UniversalCommandArgs::SourceLegacyAdoption(args),
            plan,
            timestamp,
        );
        record.schema_version = SCHEMA_V19;
        Ok(record)
    }
}

pub(super) fn validate(record: &StoredOperationRecord) -> Result<(), String> {
    validate_tagged_coordinator_shape(
        record,
        SCHEMA_V19,
        "schema-v19",
        &["SourceLegacyAdoption"],
        OperationActor::User,
        OperationKind::SourceLegacyAdoption,
    )?;
    if record.disposition != OperationDisposition::None || record.capacity_plan.is_some() {
        return Err("legacy adoption supports only the full initial None intent".into());
    }
    let Some(UniversalCommandArgs::SourceLegacyAdoption(args)) =
        record.universal_command_args.as_ref()
    else {
        return Err("missing original legacy adoption arguments".into());
    };
    if record.full_capacity_plan.as_ref()
        != Some(&args.initial_capacity_plan().map_err(|e| e.to_string())?)
    {
        return Err("legacy adoption plan differs from original eight-object accounting".into());
    }
    Ok(())
}
