//! Pure consistency checks for decoded operation-journal records.
//!
//! These checks only compare values already stored in one record. They do not inspect a path,
//! acquire a descriptor, establish profile ownership, or authorize a filesystem action. A
//! record classified as consistent is still untrusted recovery data until a future owner boundary
//! validates the live capabilities needed for a specific operation.

use std::collections::BTreeSet;
use std::path::{Component, Path};

use super::codec::{
    StoredOperationRecord, validate_v3_initial_shape, validate_v4_cancel_shape,
    validate_v5_coordinator_shape, validate_v6_coordinator_shape, validate_v7_coordinator_shape,
    validate_v8_coordinator_shape, validate_v9_coordinator_shape, validate_v10_coordinator_shape,
    validate_v11_coordinator_shape, validate_v12_coordinator_shape, validate_v13_coordinator_shape,
};
use super::evidence::{
    AbsentFinalAdoptionEvidence, AbsentFinalCapabilityScope, AbsentFinalObservation,
    AbsentFinalRecoveryObservation, AbsentFinalTransactionOwnedProof, FilesystemPublicationMode,
    FilesystemPublishedWaveformRestore, FilesystemStagedParticipant,
    FilesystemStagedWaveformRestore, FinalClaimPrimitive, FinalClaimResult, FinalNamespaceClaim,
    PreparedAbsentFinalNoReplace, PreparedFileEvidence, PreparedTargetContract,
    PreparedWaveformRestore, PublicationSynchronization, PublicationVisibility,
    ReplaceExpectedIdentity, RootPathContinuity, WholePublicationAtomicity,
};
use super::model::{
    OperationDisposition, OperationPhase, SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5,
    SCHEMA_V6, SCHEMA_V7, SCHEMA_V8, SCHEMA_V9, SCHEMA_V10, SCHEMA_V11, SCHEMA_V12, SCHEMA_V13,
};
use crate::durable_capacity::{
    DurableCapacityPlan, PROTECTED_FREE_SPACE_FLOOR, round_up_allocation,
};

/// Pure consistency classification for one decoded stored record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordConsistency {
    /// Every stored relationship required by the record's phase is internally consistent.
    Consistent,
    /// A legacy schema-v1 post-publication record has no publication evidence and must remain
    /// visible as retained, read-only recovery data.
    LegacyMissingPublicationEvidence,
    /// The stored record violates a bounded phase, evidence, identity, or capacity relationship.
    Invalid(String),
}

/// Classify one decoded operation record without consulting the filesystem.
pub fn classify_record(record: &StoredOperationRecord) -> RecordConsistency {
    match validate_record_inner(record) {
        Ok(RecordValidation::Consistent) => RecordConsistency::Consistent,
        Ok(RecordValidation::LegacyMissingPublicationEvidence) => {
            RecordConsistency::LegacyMissingPublicationEvidence
        }
        Err(reason) => RecordConsistency::Invalid(reason),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordValidation {
    Consistent,
    LegacyMissingPublicationEvidence,
}

fn validate_record_inner(record: &StoredOperationRecord) -> Result<RecordValidation, String> {
    if !matches!(
        record.schema_version,
        SCHEMA_V1
            | SCHEMA_V2
            | SCHEMA_V3
            | SCHEMA_V4
            | SCHEMA_V5
            | SCHEMA_V6
            | SCHEMA_V7
            | SCHEMA_V8
            | SCHEMA_V9
            | SCHEMA_V10
            | SCHEMA_V11
            | SCHEMA_V12
            | SCHEMA_V13
    ) {
        return Err(format!(
            "unsupported operation journal schema version: {}",
            record.schema_version
        ));
    }

    if record.schema_version == SCHEMA_V3 {
        validate_v3_initial_shape(record)?;
        validate_capacity_plan_option(record.capacity_plan.as_ref())?;
        return Ok(RecordValidation::Consistent);
    }
    if record.schema_version == SCHEMA_V4 {
        validate_v4_cancel_shape(record)?;
        validate_capacity_plan_option(record.capacity_plan.as_ref())?;
        return Ok(RecordValidation::Consistent);
    }
    if record.schema_version == SCHEMA_V5 {
        validate_v5_coordinator_shape(record)?;
        validate_capacity_plan_option(record.capacity_plan.as_ref())?;
        return Ok(RecordValidation::Consistent);
    }
    if record.schema_version == SCHEMA_V6 {
        validate_v6_coordinator_shape(record)?;
        validate_capacity_plan_option(record.capacity_plan.as_ref())?;
        return Ok(RecordValidation::Consistent);
    }
    if record.schema_version == SCHEMA_V7 {
        validate_v7_coordinator_shape(record)?;
        validate_capacity_plan_option(record.capacity_plan.as_ref())?;
        return Ok(RecordValidation::Consistent);
    }
    if record.schema_version == SCHEMA_V8 {
        validate_v8_coordinator_shape(record)?;
        validate_capacity_plan_option(record.capacity_plan.as_ref())?;
        return Ok(RecordValidation::Consistent);
    }
    if record.schema_version == SCHEMA_V9 {
        validate_v9_coordinator_shape(record)?;
        validate_capacity_plan_option(record.capacity_plan.as_ref())?;
        return Ok(RecordValidation::Consistent);
    }
    if record.schema_version == SCHEMA_V10 {
        validate_v10_coordinator_shape(record)?;
        validate_capacity_plan_option(record.capacity_plan.as_ref())?;
        return Ok(RecordValidation::Consistent);
    }
    if record.schema_version == SCHEMA_V11 {
        validate_v11_coordinator_shape(record)?;
        validate_capacity_plan_option(record.capacity_plan.as_ref())?;
        return Ok(RecordValidation::Consistent);
    }
    if record.schema_version == SCHEMA_V12 {
        validate_v12_coordinator_shape(record)?;
        validate_capacity_plan_option(record.capacity_plan.as_ref())?;
        return Ok(RecordValidation::Consistent);
    }
    if record.schema_version == SCHEMA_V13 {
        validate_v13_coordinator_shape(record)?;
        validate_capacity_plan_option(record.capacity_plan.as_ref())?;
        return Ok(RecordValidation::Consistent);
    }
    if record.cancellation_request().is_some() {
        return Err(String::from(
            "legacy operation-journal schemas cannot carry a cancellation request",
        ));
    }
    if record.universal_intent.is_some() {
        return Err(String::from(
            "legacy operation-journal schemas cannot carry a universal intent",
        ));
    }
    if record.command_args().is_some()
        || record.retry_observation().is_some()
        || record.universal_command_args().is_some()
    {
        return Err(String::from(
            "legacy operation-journal schemas cannot carry coordinator observations",
        ));
    }

    validate_phase_disposition(record.phase, record.disposition)?;
    if record.schema_version == SCHEMA_V2 {
        validate_schema_v2_phase_evidence_record(record)?;
    }
    validate_capacity_plan_option(record.capacity_plan.as_ref())?;
    if let Some(prepared) = record.prepared.as_ref() {
        validate_prepared_contract(prepared)?;
    }
    if record.schema_version == SCHEMA_V1
        && record
            .published()
            .is_some_and(is_absent_final_no_replace_publication)
    {
        return Err(String::from(
            "schema-v1 cannot contain absent-final publication evidence",
        ));
    }
    if record.phase == OperationPhase::Prepared && record.prepared.is_none() {
        return Err(String::from("prepared phase requires prepared evidence"));
    }

    let staged_valid = match (record.phase, record.staged.as_ref()) {
        (OperationPhase::FilesystemStaged, Some(staged)) => record
            .prepared
            .as_ref()
            .ok_or_else(|| String::from("filesystem-staged phase requires prepared evidence"))
            .and_then(|prepared| validate_staged_checkpoint(prepared, staged)),
        (OperationPhase::FilesystemStaged, None)
        | (OperationPhase::IntentDurable | OperationPhase::Prepared, Some(_)) => Err(String::from(
            "stored staged evidence does not match the record phase",
        )),
        (_, Some(staged)) => record
            .prepared
            .as_ref()
            .ok_or_else(|| String::from("staged evidence requires prepared evidence"))
            .and_then(|prepared| validate_staged_checkpoint(prepared, staged)),
        (_, None) => Ok(()),
    };
    staged_valid?;

    let legacy_post_publication_without_evidence = record.schema_version == SCHEMA_V1
        && record.published().is_none()
        && (record.phase.is_post_publication()
            || (record.phase == OperationPhase::Terminal
                && record.disposition.is_terminal()
                && record.disposition != OperationDisposition::CancelledBeforePublish));
    if legacy_post_publication_without_evidence {
        return Ok(RecordValidation::LegacyMissingPublicationEvidence);
    }

    let publication_validation = if record.phase.is_pre_publication() {
        if record.published().is_none() {
            Ok(())
        } else {
            Err(String::from(
                "pre-publication phase carries publication evidence",
            ))
        }
    } else if record.phase.is_post_publication() {
        validate_record_publication(record)
    } else if record.phase == OperationPhase::Terminal
        && record.disposition == OperationDisposition::CancelledBeforePublish
    {
        if record.published().is_none() {
            Ok(())
        } else {
            Err(String::from(
                "pre-publication cancellation cannot carry publication evidence",
            ))
        }
    } else {
        record
            .published()
            .map_or(Ok(()), |_| validate_record_publication(record))
    };
    publication_validation?;

    Ok(RecordValidation::Consistent)
}

fn validate_phase_disposition(
    phase: OperationPhase,
    disposition: OperationDisposition,
) -> Result<(), String> {
    use OperationDisposition as D;
    use OperationPhase as P;
    // This checks the stored shape only. Transition guards and participant evidence must
    // separately establish why an otherwise legal retry, cancellation, or terminal state exists.
    let legal = match phase {
        P::IntentDurable | P::Prepared => {
            matches!(
                disposition,
                D::None | D::RetryPending | D::CancelRequestedBeforePublish
            )
        }
        P::FilesystemStaged => matches!(
            disposition,
            D::None | D::RetryPending | D::PartialNeedsRetry | D::CancelRequestedBeforePublish
        ),
        P::FilesystemPublished | P::SourceReconciled => matches!(
            disposition,
            D::None
                | D::RetryPending
                | D::PartialNeedsRetry
                | D::AuditRequired
                | D::CancelRequestedAfterPublish
        ),
        P::GlobalReconciled | P::ProjectionPublished | P::ReadinessScheduled => matches!(
            disposition,
            D::None | D::RetryPending | D::AuditRequired | D::CancelRequestedAfterPublish
        ),
        P::Terminal => disposition.is_terminal(),
    };
    if legal {
        Ok(())
    } else {
        Err(format!(
            "invalid phase/disposition pairing: {phase:?}/{disposition:?}"
        ))
    }
}

fn validate_capacity_plan_option(plan: Option<&DurableCapacityPlan>) -> Result<(), String> {
    if let Some(plan) = plan {
        validate_capacity_plan(plan)
    } else {
        Ok(())
    }
}

fn validate_capacity_plan(plan: &DurableCapacityPlan) -> Result<(), String> {
    if plan.volumes.is_empty() {
        return Err(String::from(
            "capacity plan must contain at least one volume",
        ));
    }
    let mut identities = BTreeSet::new();
    for volume in &plan.volumes {
        let expected_allocated = round_up_allocation(volume.logical_bytes, volume.allocation_unit)
            .map_err(|error| format!("capacity plan allocation is invalid: {error}"))?;
        if !identities.insert(volume.identity.clone()) {
            return Err(String::from(
                "capacity plan contains duplicate volume identities",
            ));
        }
        if volume.allocated_bytes != expected_allocated {
            return Err(String::from(
                "capacity plan allocated bytes do not match rounded logical bytes",
            ));
        }
        if volume.protected_free_bytes != PROTECTED_FREE_SPACE_FLOOR {
            return Err(String::from(
                "capacity plan protected free-space floor is invalid",
            ));
        }
    }
    Ok(())
}

fn validate_prepared_contract(prepared: &PreparedTargetContract) -> Result<(), String> {
    match prepared {
        PreparedTargetContract::ExistingExpectedIdentity(prepared) => {
            validate_existing_prepared_contract(prepared)
        }
        PreparedTargetContract::AbsentFinalNoReplace(prepared) => {
            validate_absent_final_prepared_contract(prepared)
        }
    }
}

fn validate_existing_prepared_contract(prepared: &PreparedWaveformRestore) -> Result<(), String> {
    for (label, root) in [
        ("source root", &prepared.source_root),
        ("target root", &prepared.target_root),
        ("backup root", &prepared.backup_root),
    ] {
        if !root.path.is_absolute() || root.identity.stable_id.is_empty() {
            return Err(format!(
                "{label} locator is not an absolute, identified root"
            ));
        }
    }
    if prepared.source_id.is_empty() {
        return Err(String::from("existing-target source identity is empty"));
    }
    single_clean_normal_leaf("target", &prepared.target.relative_path)?;
    single_clean_normal_leaf("backup", &prepared.backup.relative_path)?;
    single_clean_normal_leaf("staging", &prepared.staging.relative_path)?;
    if prepared.target.relative_path == prepared.staging.relative_path {
        return Err(String::from(
            "existing-target target and staging leaves must be distinct",
        ));
    }
    if !prepared.staging.absent {
        return Err(String::from(
            "existing-target staging locator must retain its absent preparation observation",
        ));
    }
    if prepared.target.identity.stable_id.is_empty()
        || prepared.backup.identity.stable_id.is_empty()
    {
        return Err(String::from(
            "existing-target leaf identities must not be empty",
        ));
    }
    let ReplaceExpectedIdentity::Existing(expected_target) = &prepared.replacement;
    if expected_target != &prepared.target.identity {
        return Err(String::from(
            "existing-target replacement identity does not match target preparation",
        ));
    }
    Ok(())
}

fn validate_absent_final_prepared_contract(
    prepared: &PreparedAbsentFinalNoReplace,
) -> Result<(), String> {
    if !prepared.source_root.path.is_absolute()
        || prepared.source_root.identity.stable_id.is_empty()
    {
        return Err(String::from(
            "absent-final source-root locator is not an absolute, identified root",
        ));
    }
    if !prepared.target_parent.path.is_absolute() {
        return Err(String::from(
            "absent-final target-parent locator must be absolute",
        ));
    }
    if prepared.target_parent.identity.stable_id.is_empty() {
        return Err(String::from("absent-final target-parent identity is empty"));
    }
    if prepared.source_id.is_empty() {
        return Err(String::from("absent-final source identity is empty"));
    }
    single_clean_normal_leaf("final", &prepared.final_leaf)?;
    single_clean_normal_leaf("staging", &prepared.staging.relative_path)?;
    if prepared.final_leaf == prepared.staging.relative_path {
        return Err(String::from(
            "absent-final final and staging leaves must be distinct",
        ));
    }
    if !prepared.staging.absent {
        return Err(String::from(
            "absent-final staging locator must retain its absent preparation observation",
        ));
    }
    if prepared.final_observation != AbsentFinalObservation::ObservedAbsent {
        return Err(String::from(
            "absent-final preparation observation is not qualified as absent",
        ));
    }
    if !matches!(
        prepared.copy_validated_evidence,
        PreparedFileEvidence::ContentHash(_)
    ) {
        return Err(String::from(
            "absent-final preparation requires exact CopyValidated content evidence",
        ));
    }
    Ok(())
}

fn validate_staged_checkpoint(
    prepared: &PreparedTargetContract,
    staged: &FilesystemStagedWaveformRestore,
) -> Result<(), String> {
    match prepared {
        PreparedTargetContract::ExistingExpectedIdentity(prepared) => {
            validate_existing_staged_checkpoint(prepared, staged)
        }
        PreparedTargetContract::AbsentFinalNoReplace(prepared) => {
            validate_absent_final_staged_checkpoint(prepared, staged)
        }
    }
}

fn validate_existing_staged_checkpoint(
    prepared: &PreparedWaveformRestore,
    staged: &FilesystemStagedWaveformRestore,
) -> Result<(), String> {
    let FilesystemStagedParticipant::CopyValidated { staging, evidence } = &staged.participant;
    if staging.relative_path != prepared.staging.relative_path {
        return Err(String::from(
            "staged locator does not match prepared staging locator",
        ));
    }
    single_clean_normal_leaf("staged", &staging.relative_path)?;
    if staging.identity.stable_id.is_empty() {
        return Err(String::from("staged identity is empty"));
    }
    if staging.identity.len != prepared.backup.identity.len {
        return Err(String::from(
            "staged identity does not match prepared backup",
        ));
    }
    validate_staged_evidence(&prepared.evidence.backup, evidence)
}

fn validate_absent_final_staged_checkpoint(
    prepared: &PreparedAbsentFinalNoReplace,
    staged: &FilesystemStagedWaveformRestore,
) -> Result<(), String> {
    validate_absent_final_prepared_contract(prepared)?;
    let FilesystemStagedParticipant::CopyValidated { staging, evidence } = &staged.participant;
    if staging.relative_path != prepared.staging.relative_path {
        return Err(String::from(
            "absent-final staged locator does not match prepared staging leaf",
        ));
    }
    single_clean_normal_leaf("absent-final staged", &staging.relative_path)?;
    if staging.identity.stable_id.is_empty() {
        return Err(String::from("absent-final staged identity is empty"));
    }
    let (PreparedFileEvidence::ContentHash(expected), PreparedFileEvidence::ContentHash(actual)) =
        (&prepared.copy_validated_evidence, evidence)
    else {
        return Err(String::from(
            "absent-final CopyValidated evidence must be an exact content hash",
        ));
    };
    if expected != actual {
        return Err(String::from(
            "absent-final CopyValidated evidence does not match preparation",
        ));
    }
    Ok(())
}

fn validate_staged_evidence(
    backup: &PreparedFileEvidence,
    staging: &PreparedFileEvidence,
) -> Result<(), String> {
    match (backup, staging) {
        (PreparedFileEvidence::ContentHash(backup), PreparedFileEvidence::ContentHash(staging))
            if backup == staging =>
        {
            Ok(())
        }
        (
            PreparedFileEvidence::Metadata {
                len: backup_len,
                is_dir: backup_is_dir,
                ..
            },
            PreparedFileEvidence::Metadata {
                len: staging_len,
                is_dir: staging_is_dir,
                ..
            },
        ) if backup_len == staging_len && backup_is_dir == staging_is_dir && !staging_is_dir => {
            Ok(())
        }
        (PreparedFileEvidence::Unverifiable, PreparedFileEvidence::Unverifiable) => Ok(()),
        _ => Err(String::from(
            "staged content evidence does not match backup",
        )),
    }
}

fn validate_record_publication(record: &StoredOperationRecord) -> Result<(), String> {
    let prepared = record
        .prepared
        .as_ref()
        .ok_or_else(|| String::from("publication evidence is missing prepared evidence"))?;
    let staged = record
        .staged
        .as_ref()
        .ok_or_else(|| String::from("publication evidence is missing staged evidence"))?;
    validate_staged_checkpoint(prepared, staged)?;
    let published = record
        .published()
        .ok_or_else(|| String::from("publication evidence is missing"))?;
    match prepared {
        PreparedTargetContract::ExistingExpectedIdentity(prepared) => {
            validate_publication_evidence(prepared, staged, published)
        }
        PreparedTargetContract::AbsentFinalNoReplace(_) => {
            validate_absent_final_no_replace_publication(prepared, staged, published)
        }
    }
}

fn validate_publication_evidence(
    prepared: &PreparedWaveformRestore,
    staged: &FilesystemStagedWaveformRestore,
    published: &FilesystemPublishedWaveformRestore,
) -> Result<(), String> {
    let FilesystemStagedParticipant::CopyValidated {
        staging,
        evidence: staged_content,
    } = &staged.participant;

    if published.mode != FilesystemPublicationMode::NonAtomicCopyValidatePublish {
        return Err(String::from(
            "waveform restore publication mode is not the qualified bytewise staging mode",
        ));
    }
    if published.whole_publication != WholePublicationAtomicity::WholePublicationNonAtomic {
        return Err(String::from(
            "bytewise waveform restore staging cannot claim whole-publication atomicity",
        ));
    }
    if published.visibility != PublicationVisibility::VisibilityVerified {
        return Err(String::from(
            "waveform restore publication visibility is not verified",
        ));
    }
    if published.synchronization == PublicationSynchronization::PowerLossSynchronized {
        return Err(String::from(
            "non-atomic waveform restore publication cannot claim power-loss synchronization",
        ));
    }
    if published.reopened_final.identity != staging.identity {
        return Err(String::from(
            "reopened final identity does not match validated staging identity",
        ));
    }
    validate_publication_file_evidence(staged_content, &published.reopened_final.content)?;

    match &published.final_claim {
        FinalNamespaceClaim::ExpectedIdentityReplacement {
            primitive,
            result,
            expected_target,
            displaced_target,
        } => {
            if !matches!(
                primitive,
                FinalClaimPrimitive::QualifiedExpectedIdentityReplacement
            ) {
                return Err(String::from(
                    "expected-identity replacement primitive is not qualified",
                ));
            }
            if *result != FinalClaimResult::ExpectedIdentityReplaced {
                return Err(String::from(
                    "expected-identity replacement result is not established",
                ));
            }
            let ReplaceExpectedIdentity::Existing(prepared_target) = &prepared.replacement;
            if expected_target != prepared_target {
                return Err(String::from(
                    "publication expected target identity does not match preparation",
                ));
            }
            if displaced_target != expected_target {
                return Err(String::from(
                    "publication displaced target identity does not match expected identity",
                ));
            }
        }
        FinalNamespaceClaim::AbsentFinalNoReplace {
            primitive, result, ..
        } => {
            if !matches!(
                primitive,
                FinalClaimPrimitive::QualifiedAbsentFinalNoReplace
            ) {
                return Err(String::from(
                    "absent-final no-replace primitive is not qualified",
                ));
            }
            if *result != FinalClaimResult::AbsentFinalInstalled {
                return Err(String::from(
                    "absent-final no-replace result is not established",
                ));
            }
            return Err(String::from(
                "waveform restore requires expected-identity replacement evidence",
            ));
        }
    }
    Ok(())
}

fn validate_absent_final_no_replace_publication(
    prepared: &PreparedTargetContract,
    staged: &FilesystemStagedWaveformRestore,
    published: &FilesystemPublishedWaveformRestore,
) -> Result<(), String> {
    let PreparedTargetContract::AbsentFinalNoReplace(prepared) = prepared else {
        return Err(String::from(
            "absent-final publication requires the absent-final preparation contract",
        ));
    };
    let FilesystemStagedParticipant::CopyValidated {
        staging,
        evidence: staged_content,
    } = &staged.participant;

    if published.mode != FilesystemPublicationMode::NonAtomicCopyValidatePublish {
        return Err(String::from(
            "absent-final publication mode is not the qualified bytewise staging mode",
        ));
    }
    if published.whole_publication != WholePublicationAtomicity::WholePublicationNonAtomic {
        return Err(String::from(
            "absent-final publication cannot claim whole-publication atomicity",
        ));
    }
    if published.visibility != PublicationVisibility::VisibilityVerified {
        return Err(String::from(
            "absent-final publication visibility is not verified",
        ));
    }
    if published.synchronization != PublicationSynchronization::SyncUnsupportedOrUnverified {
        return Err(String::from(
            "absent-final publication requires unsupported-or-unverified synchronization",
        ));
    }

    let FinalNamespaceClaim::AbsentFinalNoReplace {
        primitive,
        result,
        capability_scope,
    } = &published.final_claim
    else {
        return Err(String::from(
            "publication evidence is not an absent-final no-replace claim",
        ));
    };
    if !matches!(
        primitive,
        FinalClaimPrimitive::QualifiedAbsentFinalNoReplace
    ) {
        return Err(String::from(
            "absent-final no-replace primitive is not qualified",
        ));
    }
    if *result != FinalClaimResult::AbsentFinalInstalled {
        return Err(String::from(
            "absent-final no-replace result is not established",
        ));
    }
    let AbsentFinalCapabilityScope::TargetParentDescriptor {
        target_parent_identity,
        root_path_continuity,
    } = capability_scope;
    // A publication changes directory metadata. Compare the recorded stable identity here;
    // current capability ownership and pathname continuity require separate live validation.
    if target_parent_identity.stable_id != prepared.target_parent.identity.stable_id {
        return Err(String::from(
            "absent-final capability scope does not match the prepared target parent",
        ));
    }
    if !matches!(root_path_continuity, RootPathContinuity::NotClaimed) {
        return Err(String::from(
            "absent-final evidence overclaims root pathname continuity",
        ));
    }
    if prepared.final_observation != AbsentFinalObservation::ObservedAbsent {
        return Err(String::from(
            "absent-final preparation did not retain an absent observation",
        ));
    }
    if prepared.staging.relative_path != staging.relative_path {
        return Err(String::from(
            "absent-final publication staging leaf does not match preparation",
        ));
    }
    if prepared.final_leaf == prepared.staging.relative_path {
        return Err(String::from(
            "absent-final publication final and staging leaves must be distinct",
        ));
    }
    let (PreparedFileEvidence::ContentHash(expected), PreparedFileEvidence::ContentHash(staged)) =
        (&prepared.copy_validated_evidence, staged_content)
    else {
        return Err(String::from(
            "absent-final publication requires exact CopyValidated content hashes",
        ));
    };
    if expected != staged {
        return Err(String::from(
            "absent-final publication content does not match preparation",
        ));
    }
    if published.reopened_final.identity.stable_id != staging.identity.stable_id
        || published.reopened_final.identity.len != staging.identity.len
    {
        return Err(String::from(
            "reopened absent-final identity does not match validated staging identity",
        ));
    }
    validate_publication_file_evidence(staged_content, &published.reopened_final.content)
}

fn validate_publication_file_evidence(
    expected: &PreparedFileEvidence,
    actual: &PreparedFileEvidence,
) -> Result<(), String> {
    match (expected, actual) {
        (
            PreparedFileEvidence::ContentHash(expected),
            PreparedFileEvidence::ContentHash(actual),
        ) if expected == actual => Ok(()),
        _ => Err(String::from(
            "reopened final content evidence does not match validated staging",
        )),
    }
}

fn validate_absent_final_recovery_observation_shape(
    observation: &AbsentFinalRecoveryObservation,
) -> Result<(), String> {
    if observation.target_parent_stable_id.is_empty() {
        return Err(String::from(
            "absent-final recovery observation target-parent identity is empty",
        ));
    }
    if observation.final_stable_id.is_empty() {
        return Err(String::from(
            "absent-final recovery observation final identity is empty",
        ));
    }
    if !matches!(
        observation.final_content,
        PreparedFileEvidence::ContentHash(_)
    ) {
        return Err(String::from(
            "absent-final recovery observation requires exact final content hash evidence",
        ));
    }
    Ok(())
}

fn validate_absent_final_recovery_observation_record(
    record: &StoredOperationRecord,
) -> Result<(), String> {
    let Some(observation) = record.absent_final_recovery_observation.as_ref() else {
        return Ok(());
    };
    validate_absent_final_recovery_observation_shape(observation)?;
    if record.phase != OperationPhase::FilesystemStaged {
        return Err(String::from(
            "absent-final recovery observation requires the filesystem-staged phase",
        ));
    }
    let Some(PreparedTargetContract::AbsentFinalNoReplace(prepared)) = record.prepared.as_ref()
    else {
        return Err(String::from(
            "absent-final recovery observation requires absent-final preparation evidence",
        ));
    };
    let Some(staged) = record.staged.as_ref() else {
        return Err(String::from(
            "absent-final recovery observation requires staged evidence",
        ));
    };
    validate_absent_final_staged_checkpoint(prepared, staged)?;
    if observation.target_parent_stable_id != prepared.target_parent.identity.stable_id {
        return Err(String::from(
            "absent-final recovery observation target-parent identity does not match preparation",
        ));
    }
    let FilesystemStagedParticipant::CopyValidated { staging, evidence } = &staged.participant;
    if observation.final_stable_id != staging.identity.stable_id
        || observation.final_len != staging.identity.len
    {
        return Err(String::from(
            "absent-final recovery observation final identity does not match staged evidence",
        ));
    }
    let (
        PreparedFileEvidence::ContentHash(staged_hash),
        PreparedFileEvidence::ContentHash(observed_hash),
    ) = (evidence, &observation.final_content)
    else {
        return Err(String::from(
            "absent-final recovery observation content is not an exact hash",
        ));
    };
    if staged_hash != observed_hash {
        return Err(String::from(
            "absent-final recovery observation content does not match staged evidence",
        ));
    }
    Ok(())
}

fn validate_absent_final_transaction_owned_proof_shape(
    proof: &AbsentFinalTransactionOwnedProof,
) -> Result<(), String> {
    if proof.target_parent_stable_id.is_empty() {
        return Err(String::from(
            "absent-final transaction-owned proof target-parent identity is empty",
        ));
    }
    if proof.final_stable_id.is_empty() {
        return Err(String::from(
            "absent-final transaction-owned proof final identity is empty",
        ));
    }
    if !matches!(proof.final_content, PreparedFileEvidence::ContentHash(_)) {
        return Err(String::from(
            "absent-final transaction-owned proof requires exact final content hash evidence",
        ));
    }
    Ok(())
}

fn validate_absent_final_transaction_owned_proof_record(
    record: &StoredOperationRecord,
) -> Result<(), String> {
    let Some(proof) = record.absent_final_transaction_owned_proof.as_ref() else {
        return Ok(());
    };
    validate_absent_final_transaction_owned_proof_shape(proof)?;
    if record.phase != OperationPhase::FilesystemStaged {
        return Err(String::from(
            "absent-final transaction-owned proof requires the filesystem-staged phase",
        ));
    }
    let Some(recovery_observation) = record.absent_final_recovery_observation.as_ref() else {
        return Err(String::from(
            "absent-final transaction-owned proof requires a recovery observation",
        ));
    };
    validate_absent_final_recovery_observation_record(record)?;
    if proof.target_parent_stable_id != recovery_observation.target_parent_stable_id
        || proof.final_stable_id != recovery_observation.final_stable_id
        || proof.final_len != recovery_observation.final_len
        || proof.final_content != recovery_observation.final_content
    {
        return Err(String::from(
            "absent-final transaction-owned proof does not match recovery observation",
        ));
    }
    let Some(PreparedTargetContract::AbsentFinalNoReplace(prepared)) = record.prepared.as_ref()
    else {
        return Err(String::from(
            "absent-final transaction-owned proof requires absent-final preparation evidence",
        ));
    };
    let Some(staged) = record.staged.as_ref() else {
        return Err(String::from(
            "absent-final transaction-owned proof requires staged evidence",
        ));
    };
    validate_absent_final_staged_checkpoint(prepared, staged)?;
    let FilesystemStagedParticipant::CopyValidated { staging, evidence } = &staged.participant;
    if proof.target_parent_stable_id != prepared.target_parent.identity.stable_id
        || proof.final_stable_id != staging.identity.stable_id
        || proof.final_len != staging.identity.len
    {
        return Err(String::from(
            "absent-final transaction-owned proof does not match prepared or staged identity",
        ));
    }
    let (
        PreparedFileEvidence::ContentHash(staged_hash),
        PreparedFileEvidence::ContentHash(proof_hash),
    ) = (evidence, &proof.final_content)
    else {
        return Err(String::from(
            "absent-final transaction-owned proof content is not an exact hash",
        ));
    };
    if staged_hash != proof_hash {
        return Err(String::from(
            "absent-final transaction-owned proof content does not match staged evidence",
        ));
    }
    Ok(())
}

fn validate_absent_final_adoption_evidence_shape(
    evidence: &AbsentFinalAdoptionEvidence,
) -> Result<(), String> {
    if evidence.target_parent_stable_id.is_empty() {
        return Err(String::from(
            "absent-final adoption evidence target-parent identity is empty",
        ));
    }
    if evidence.final_stable_id.is_empty() {
        return Err(String::from(
            "absent-final adoption evidence final identity is empty",
        ));
    }
    if !matches!(evidence.final_content, PreparedFileEvidence::ContentHash(_)) {
        return Err(String::from(
            "absent-final adoption evidence requires exact final content hash evidence",
        ));
    }
    Ok(())
}

fn validate_absent_final_adoption_evidence_record(
    record: &StoredOperationRecord,
) -> Result<(), String> {
    let Some(evidence) = record.absent_final_adoption_evidence.as_ref() else {
        return Ok(());
    };
    validate_absent_final_adoption_evidence_shape(evidence)?;
    if record.phase != OperationPhase::FilesystemStaged {
        return Err(String::from(
            "absent-final adoption evidence requires the filesystem-staged phase",
        ));
    }
    let Some(observation) = record.absent_final_recovery_observation.as_ref() else {
        return Err(String::from(
            "absent-final adoption evidence requires a recovery observation",
        ));
    };
    let Some(proof) = record.absent_final_transaction_owned_proof.as_ref() else {
        return Err(String::from(
            "absent-final adoption evidence requires transaction-owned proof",
        ));
    };
    validate_absent_final_transaction_owned_proof_record(record)?;
    if evidence.target_parent_stable_id != proof.target_parent_stable_id
        || evidence.final_stable_id != proof.final_stable_id
        || evidence.final_len != proof.final_len
        || evidence.final_content != proof.final_content
        || evidence.target_parent_stable_id != observation.target_parent_stable_id
        || evidence.final_stable_id != observation.final_stable_id
        || evidence.final_len != observation.final_len
        || evidence.final_content != observation.final_content
    {
        return Err(String::from(
            "absent-final adoption evidence does not match proof and observation",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SchemaV2EvidencePresence {
    prepared: bool,
    staged: bool,
    absent_final_recovery_observation: bool,
    absent_final_transaction_owned_proof: bool,
    absent_final_adoption_evidence: bool,
    published: bool,
}

impl SchemaV2EvidencePresence {
    const NONE: Self = Self {
        prepared: false,
        staged: false,
        absent_final_recovery_observation: false,
        absent_final_transaction_owned_proof: false,
        absent_final_adoption_evidence: false,
        published: false,
    };
    const PREPARED: Self = Self {
        prepared: true,
        staged: false,
        absent_final_recovery_observation: false,
        absent_final_transaction_owned_proof: false,
        absent_final_adoption_evidence: false,
        published: false,
    };
    const PREPARED_STAGED: Self = Self {
        prepared: true,
        staged: true,
        absent_final_recovery_observation: false,
        absent_final_transaction_owned_proof: false,
        absent_final_adoption_evidence: false,
        published: false,
    };
    const PREPARED_STAGED_WITH_ABSENT_FINAL_RECOVERY_OBSERVATION: Self = Self {
        prepared: true,
        staged: true,
        absent_final_recovery_observation: true,
        absent_final_transaction_owned_proof: false,
        absent_final_adoption_evidence: false,
        published: false,
    };
    const PREPARED_STAGED_WITH_ABSENT_FINAL_RECOVERY_PROOF: Self = Self {
        prepared: true,
        staged: true,
        absent_final_recovery_observation: true,
        absent_final_transaction_owned_proof: true,
        absent_final_adoption_evidence: false,
        published: false,
    };
    const PREPARED_STAGED_WITH_ABSENT_FINAL_ADOPTION: Self = Self {
        prepared: true,
        staged: true,
        absent_final_recovery_observation: true,
        absent_final_transaction_owned_proof: true,
        absent_final_adoption_evidence: true,
        published: false,
    };
    const ALL: Self = Self {
        prepared: true,
        staged: true,
        absent_final_recovery_observation: false,
        absent_final_transaction_owned_proof: false,
        absent_final_adoption_evidence: false,
        published: true,
    };

    fn from_record(record: &StoredOperationRecord) -> Self {
        Self {
            prepared: record.prepared.is_some(),
            staged: record.staged.is_some(),
            absent_final_recovery_observation: record.absent_final_recovery_observation.is_some(),
            absent_final_transaction_owned_proof: record
                .absent_final_transaction_owned_proof
                .is_some(),
            absent_final_adoption_evidence: record.absent_final_adoption_evidence.is_some(),
            published: record.published().is_some(),
        }
    }
}

fn schema_v2_phase_evidence_is_valid(
    phase: OperationPhase,
    disposition: OperationDisposition,
    evidence: SchemaV2EvidencePresence,
) -> bool {
    match phase {
        OperationPhase::IntentDurable => evidence == SchemaV2EvidencePresence::NONE,
        OperationPhase::Prepared => evidence == SchemaV2EvidencePresence::PREPARED,
        OperationPhase::FilesystemStaged => matches!(
            evidence,
            SchemaV2EvidencePresence::PREPARED_STAGED
                | SchemaV2EvidencePresence::PREPARED_STAGED_WITH_ABSENT_FINAL_RECOVERY_OBSERVATION
                | SchemaV2EvidencePresence::PREPARED_STAGED_WITH_ABSENT_FINAL_RECOVERY_PROOF
                | SchemaV2EvidencePresence::PREPARED_STAGED_WITH_ABSENT_FINAL_ADOPTION
        ),
        OperationPhase::FilesystemPublished
        | OperationPhase::SourceReconciled
        | OperationPhase::GlobalReconciled
        | OperationPhase::ProjectionPublished
        | OperationPhase::ReadinessScheduled => evidence == SchemaV2EvidencePresence::ALL,
        OperationPhase::Terminal if disposition == OperationDisposition::CancelledBeforePublish => {
            matches!(
                evidence,
                SchemaV2EvidencePresence::NONE
                    | SchemaV2EvidencePresence::PREPARED
                    | SchemaV2EvidencePresence::PREPARED_STAGED
            )
        }
        OperationPhase::Terminal => evidence == SchemaV2EvidencePresence::ALL,
    }
}

fn validate_schema_v2_phase_evidence_record(record: &StoredOperationRecord) -> Result<(), String> {
    let evidence = SchemaV2EvidencePresence::from_record(record);
    if !schema_v2_phase_evidence_is_valid(record.phase, record.disposition, evidence) {
        return Err(format!(
            "schema-v2 phase/evidence combination is invalid: phase={:?}, disposition={:?}, evidence={evidence:?}",
            record.phase, record.disposition,
        ));
    }
    validate_absent_final_recovery_observation_record(record)?;
    validate_absent_final_transaction_owned_proof_record(record)
        .and_then(|()| validate_absent_final_adoption_evidence_record(record))
}

fn is_absent_final_no_replace_publication(published: &FilesystemPublishedWaveformRestore) -> bool {
    matches!(
        published.final_claim,
        FinalNamespaceClaim::AbsentFinalNoReplace { .. }
    )
}

fn single_clean_normal_leaf(label: &str, path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(format!("{label} locator must be a non-empty relative path"));
    }
    let mut components = path.components();
    let Some(Component::Normal(_)) = components.next() else {
        return Err(format!("{label} locator is not a clean normal leaf"));
    };
    if components.next().is_some() {
        return Err(format!("{label} locator is not a single clean normal leaf"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const V1_EMPTY_FIXTURE: &[u8] = br#"{
  "schema_version": 1,
  "operation_id": "01234567-89ab-cdef-0123-456789abcdef",
  "intent": {"actor": "User", "kind": "FileHistory", "label": "v1"},
  "phase": "IntentDurable",
  "disposition": "None",
  "payload": null,
  "capacity_plan": null,
  "prepared": null,
  "staged": null,
  "published": null,
  "replacement_qualification": null,
  "created_unix_ms": 1,
  "updated_unix_ms": 1
}"#;

    const V2_COMPLETE_FIXTURE: &[u8] = br#"{
  "schema_version": 2,
  "operation_id": "fedcba98-7654-3210-fedc-ba9876543210",
  "intent": {"actor": "User", "kind": "FileHistory", "label": "complete"},
  "phase": "FilesystemPublished",
  "disposition": "None",
  "payload": {"bounded": true},
  "capacity_plan": null,
  "prepared": {
    "ExistingExpectedIdentity": {
      "direction": "Undo",
      "source_id": "source-1",
      "source_root": {"path": "/Volumes/Source", "identity": {"stable_id": "root-source", "change_marker": "m1", "len": 4096}},
      "target_root": {"path": "/Volumes/Target", "identity": {"stable_id": "root-target", "change_marker": null, "len": 4096}},
      "target": {"relative_path": "target.wav", "identity": {"stable_id": "target-1", "change_marker": "t1", "len": 12}},
      "backup_root": {"path": "/Volumes/Backup", "identity": {"stable_id": "root-backup", "change_marker": null, "len": 4096}},
      "backup": {"relative_path": "backup.wav", "identity": {"stable_id": "backup-1", "change_marker": "b1", "len": 12}},
      "replacement": {"Existing": {"stable_id": "target-1", "change_marker": "t1", "len": 12}},
      "staging": {"relative_path": "staging.wav", "absent": true},
      "evidence": {
        "target": {"Metadata": {"len": 12, "modified_ns": 10, "is_dir": false}},
        "backup": {"ContentHash": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31]}
      }
    }
  },
  "staged": {
    "participant": {
      "CopyValidated": {
        "staging": {"relative_path": "staging.wav", "identity": {"stable_id": "staging-1", "change_marker": "s1", "len": 12}},
        "evidence": {"ContentHash": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31]}
      }
    }
  },
  "absent_final_recovery_observation": null,
  "absent_final_transaction_owned_proof": null,
  "absent_final_adoption_evidence": null,
  "published": {
    "mode": "NonAtomicCopyValidatePublish",
    "final_claim": {
      "ExpectedIdentityReplacement": {
        "primitive": "QualifiedExpectedIdentityReplacement",
        "result": "ExpectedIdentityReplaced",
        "expected_target": {"stable_id": "target-1", "change_marker": "t1", "len": 12},
        "displaced_target": {"stable_id": "target-1", "change_marker": "t1", "len": 12}
      }
    },
    "reopened_final": {
      "identity": {"stable_id": "staging-1", "change_marker": "s1", "len": 12},
      "content": {"ContentHash": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31]}
    },
    "visibility": "VisibilityVerified",
    "whole_publication": "WholePublicationNonAtomic",
    "synchronization": "BestEffortSync"
  },
  "replacement_qualification": null,
  "created_unix_ms": 1,
  "updated_unix_ms": 2
}"#;

    fn value(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).expect("fixture JSON is valid")
    }

    fn host_valid_complete_fixture() -> Value {
        let mut fixture = value(V2_COMPLETE_FIXTURE);
        let prepared = fixture["prepared"]
            .as_object_mut()
            .expect("prepared contract object")
            .get_mut("ExistingExpectedIdentity")
            .expect("existing prepared contract");
        for (field, suffix) in [
            ("source_root", "Source"),
            ("target_root", "Target"),
            ("backup_root", "Backup"),
        ] {
            let path = if cfg!(windows) {
                format!(r"C:\wavecrate\{suffix}")
            } else {
                format!("/Volumes/{suffix}")
            };
            prepared[field]["path"] = Value::String(path);
        }
        fixture
    }

    fn decode_value(fixture: Value) -> StoredOperationRecord {
        let bytes = serde_json::to_vec(&fixture).expect("fixture serializes");
        super::super::codec::decode(&bytes).expect("fixture decodes")
    }

    fn complete_record() -> StoredOperationRecord {
        decode_value(host_valid_complete_fixture())
    }

    #[test]
    fn complete_decoded_record_is_consistent() {
        let record = complete_record();
        assert_eq!(classify_record(&record), RecordConsistency::Consistent);
    }

    #[test]
    fn impossible_statuses_are_rejected_even_for_legacy_records() {
        for (phase, disposition) in [
            ("IntentDurable", "Succeeded"),
            ("IntentDurable", "CancelRequestedAfterPublish"),
            ("Terminal", "None"),
            ("Terminal", "RetryPending"),
            ("GlobalReconciled", "PartialNeedsRetry"),
            ("SourceReconciled", "CancelRequestedBeforePublish"),
        ] {
            let mut fixture = value(V1_EMPTY_FIXTURE);
            fixture["phase"] = Value::from(phase);
            fixture["disposition"] = Value::from(disposition);
            assert!(
                matches!(
                    classify_record(&decode_value(fixture)),
                    RecordConsistency::Invalid(reason) if reason.contains("phase/disposition")
                ),
                "{phase}/{disposition}"
            );
        }
    }

    #[test]
    fn missing_prepared_or_staged_evidence_is_invalid() {
        let mut missing_prepared = host_valid_complete_fixture();
        missing_prepared["phase"] = Value::String(String::from("Prepared"));
        missing_prepared["prepared"] = Value::Null;
        missing_prepared["staged"] = Value::Null;
        missing_prepared["published"] = Value::Null;
        assert!(matches!(
            classify_record(&decode_value(missing_prepared)),
            RecordConsistency::Invalid(reason) if reason.contains("phase/evidence")
        ));

        let mut missing_staged = host_valid_complete_fixture();
        missing_staged["phase"] = Value::String(String::from("FilesystemStaged"));
        missing_staged["staged"] = Value::Null;
        missing_staged["published"] = Value::Null;
        assert!(matches!(
            classify_record(&decode_value(missing_staged)),
            RecordConsistency::Invalid(reason) if reason.contains("phase/evidence")
        ));
    }

    #[test]
    fn invalid_phase_prefix_is_rejected_before_relationship_checks() {
        let mut fixture = host_valid_complete_fixture();
        fixture["phase"] = Value::String(String::from("IntentDurable"));
        assert!(matches!(
            classify_record(&decode_value(fixture)),
            RecordConsistency::Invalid(reason) if reason.contains("phase/evidence")
        ));
    }

    #[test]
    fn legacy_v1_post_publication_without_evidence_is_read_only() {
        let mut fixture = value(V1_EMPTY_FIXTURE);
        fixture["phase"] = Value::String(String::from("Terminal"));
        fixture["disposition"] = Value::String(String::from("Succeeded"));
        let record = decode_value(fixture);
        assert_eq!(
            classify_record(&record),
            RecordConsistency::LegacyMissingPublicationEvidence
        );
    }

    #[test]
    fn v1_cancellation_accepts_a_missing_publication_prefix() {
        let mut fixture = value(V1_EMPTY_FIXTURE);
        fixture["phase"] = Value::String(String::from("Terminal"));
        fixture["disposition"] = Value::String(String::from("CancelledBeforePublish"));
        let record = decode_value(fixture);
        assert_eq!(classify_record(&record), RecordConsistency::Consistent);
    }

    #[test]
    fn v2_cancellation_accepts_a_missing_publication_prefix() {
        let mut fixture = value(V2_COMPLETE_FIXTURE);
        fixture["phase"] = Value::String(String::from("Terminal"));
        fixture["disposition"] = Value::String(String::from("CancelledBeforePublish"));
        fixture["published"] = Value::Null;
        let record = decode_value(host_valid_value(fixture));
        assert_eq!(classify_record(&record), RecordConsistency::Consistent);
    }

    fn host_valid_value(mut fixture: Value) -> Value {
        let prepared = fixture["prepared"]
            .as_object_mut()
            .expect("prepared contract object")
            .get_mut("ExistingExpectedIdentity")
            .expect("existing prepared contract");
        for (field, suffix) in [
            ("source_root", "Source"),
            ("target_root", "Target"),
            ("backup_root", "Backup"),
        ] {
            let path = if cfg!(windows) {
                format!(r"C:\wavecrate\{suffix}")
            } else {
                format!("/Volumes/{suffix}")
            };
            prepared[field]["path"] = Value::String(path);
        }
        fixture
    }

    #[test]
    fn mismatched_staged_identity_or_hash_is_invalid() {
        let mut identity = host_valid_complete_fixture();
        identity["staged"]["participant"]["CopyValidated"]["staging"]["identity"]["len"] =
            Value::from(13_u64);
        assert!(matches!(
            classify_record(&decode_value(identity)),
            RecordConsistency::Invalid(reason) if reason.contains("staged identity")
        ));

        let mut hash = host_valid_complete_fixture();
        hash["published"]["reopened_final"]["content"]["ContentHash"][0] = Value::from(99_u64);
        assert!(matches!(
            classify_record(&decode_value(hash)),
            RecordConsistency::Invalid(reason) if reason.contains("reopened final content")
        ));
    }

    #[test]
    fn invalid_capacity_plan_is_rejected() {
        let mut fixture = host_valid_complete_fixture();
        fixture["capacity_plan"] = serde_json::json!({"volumes": []});
        assert!(matches!(
            classify_record(&decode_value(fixture)),
            RecordConsistency::Invalid(reason) if reason.contains("capacity plan")
        ));
    }
}
