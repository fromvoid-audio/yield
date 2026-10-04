//! Bounded, non-authoritative observations of a newly admitted full-progress intent.
//!
//! This value keeps the exact command, participant inventory, derived work plan, capacity plan,
//! and admission-claim observations together so a later full-progress snapshot can validate its
//! topology without reconstructing facts that an earlier `WorkProgress` value discarded.  It
//! does not reserve capacity, create journal rows, issue permits, or authorize physical work.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::admission_claims::{AdmissionClaimInventory, AdmissionClaimValidationError};
use super::coordinator::{UniversalCommandArgs, UniversalCommandArgsValidationError};
use super::full_capacity::{FullCapacityPlan, FullCapacityValidationError};
use super::model::OperationId;
use super::participants::{ParticipantSet, ParticipantValidationError};
use super::work_plan::{WorkPlan, WorkPlanError};

/// One exact, frozen intent input set retained for later full-progress validation.
///
/// Every field is an observation captured at intent admission.  This type contains no live
/// participant, capacity, claim, filesystem, or dispatch authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FullProgressIntentObservation {
    operation_id: OperationId,
    args: UniversalCommandArgs,
    participants: ParticipantSet,
    plan: WorkPlan,
    capacity_plan: FullCapacityPlan,
    admission_claims: AdmissionClaimInventory,
}

impl FullProgressIntentObservation {
    /// Construct and validate one exact intent observation.
    ///
    /// The supplied operation identity is retained as-is.  This constructor does not generate an
    /// identity, reserve capacity, create journal rows, issue permits, or acquire any resource.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        operation_id: OperationId,
        args: UniversalCommandArgs,
        participants: ParticipantSet,
        plan: WorkPlan,
        capacity_plan: FullCapacityPlan,
        admission_claims: AdmissionClaimInventory,
    ) -> Result<Self, FullProgressIntentObservationError> {
        let observation = Self {
            operation_id,
            args,
            participants,
            plan,
            capacity_plan,
            admission_claims,
        };
        observation.validate()?;
        Ok(observation)
    }

    /// Return the exact operation identity captured by this observation.
    pub const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Borrow the exact typed command arguments captured by this observation.
    pub fn args(&self) -> &UniversalCommandArgs {
        &self.args
    }

    /// Borrow the exact participant descriptors and initial checkpoints.
    pub fn participants(&self) -> &ParticipantSet {
        &self.participants
    }

    /// Borrow the exact derived work plan captured by this observation.
    pub fn plan(&self) -> &WorkPlan {
        &self.plan
    }

    /// Borrow the exact capacity observations captured by this observation.
    pub fn capacity_plan(&self) -> &FullCapacityPlan {
        &self.capacity_plan
    }

    /// Borrow the exact admission-claim observations captured by this observation.
    pub fn admission_claims(&self) -> &AdmissionClaimInventory {
        &self.admission_claims
    }

    /// Validate shape, exact plan derivation, capacity arithmetic, and claim coverage.
    ///
    /// This proves only that the retained values are mutually consistent observations.  It does
    /// not prove that capacity remains available, that claims are live reservations, or that the
    /// values authorize a physical operation.
    pub fn validate(&self) -> Result<(), FullProgressIntentObservationError> {
        if matches!(&self.args, UniversalCommandArgs::SourceLegacyAdoption(_))
            || self
                .participants
                .descriptors()
                .iter()
                .any(|d| d.kind() == super::ParticipantKind::LegacySource)
        {
            return Err(FullProgressIntentObservationError::LegacyAdoptionAdmissionUnavailable);
        }
        if self.operation_id.as_uuid().is_nil() {
            return Err(FullProgressIntentObservationError::NilOperationId);
        }
        if matches!(&self.args, UniversalCommandArgs::Recording(proposal) if proposal.operation_id() != self.operation_id)
        {
            return Err(FullProgressIntentObservationError::RecordingOperationMismatch);
        }
        self.participants
            .validate_for_new_intent()
            .map_err(FullProgressIntentObservationError::Participants)?;
        self.args
            .validate_against_participants(&self.participants)
            .map_err(FullProgressIntentObservationError::Arguments)?;
        self.plan
            .validate_against(&self.args, &self.participants)
            .map_err(FullProgressIntentObservationError::Plan)?;
        self.capacity_plan
            .validate()
            .map_err(FullProgressIntentObservationError::Capacity)?;
        if let UniversalCommandArgs::SourceDerivedLayout(args) = &self.args {
            use super::full_capacity::FullCapacityAllocationClass as C;
            let mut source = 0u64;
            let mut journal = 0u64;
            let mut sidecars = 0u64;
            for allocation in self.capacity_plan.allocations() {
                let total = match allocation.class() {
                    C::SourceDatabaseCommit => &mut source,
                    C::JournalRecord => &mut journal,
                    C::SourceWalShm => &mut sidecars,
                    _ => return Err(FullProgressIntentObservationError::SourceLayoutAllocation),
                };
                *total = total
                    .checked_add(allocation.logical_bytes())
                    .ok_or(FullProgressIntentObservationError::SourceLayoutAllocation)?;
            }
            let database = args
                .allocation()
                .page_size()
                .checked_mul(args.allocation().maximum_pages())
                .ok_or(FullProgressIntentObservationError::SourceLayoutAllocation)?;
            let wal_shm = args
                .allocation()
                .required_metadata_bytes()
                .checked_sub(database)
                .ok_or(FullProgressIntentObservationError::SourceLayoutAllocation)?;
            if source < database
                || sidecars < wal_shm
                || journal < super::model::MAX_RECORD_BYTES as u64
            {
                return Err(FullProgressIntentObservationError::SourceLayoutAllocation);
            }
        }
        self.admission_claims
            .validate_against(&self.capacity_plan)
            .map_err(FullProgressIntentObservationError::AdmissionClaims)
    }
}

/// Why a full-progress intent observation failed closed validation.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum FullProgressIntentObservationError {
    /// This passive family has no genuine capacity-class/claim and durable adoption bridge yet.
    #[error("legacy adoption requires the original-owner capacity and durable admission bridge")]
    LegacyAdoptionAdmissionUnavailable,
    /// Source layout must cover its complete finite source peak and bounded journal transport only.
    #[error("source layout allocation inventory omits its peak or includes unrelated effects")]
    SourceLayoutAllocation,
    /// The retained operation identity was nil.
    #[error("full-progress intent observation has a nil operation ID")]
    NilOperationId,
    /// Recording bytes and progress must retain the same frozen operation identity.
    #[error("recording proposal and full-progress operation identity differ")]
    RecordingOperationMismatch,
    /// The retained participant inventory was not a fresh initial intent inventory.
    #[error("invalid full-progress intent participants: {0}")]
    Participants(#[source] ParticipantValidationError),
    /// The retained command arguments did not match the participants.
    #[error("invalid full-progress intent command arguments: {0}")]
    Arguments(#[source] UniversalCommandArgsValidationError),
    /// The retained work plan did not match exact V1 derivation.
    #[error("invalid full-progress intent work plan: {0}")]
    Plan(#[source] WorkPlanError),
    /// The retained capacity observations failed arithmetic or shape validation.
    #[error("invalid full-progress intent capacity plan: {0}")]
    Capacity(#[source] FullCapacityValidationError),
    /// The retained claims did not exactly cover the capacity plan.
    #[error("invalid full-progress intent admission claims: {0}")]
    AdmissionClaims(#[source] AdmissionClaimValidationError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR;
    use crate::operation_journal::{
        AdmissionClaim, AdmissionClaimId, FullCapacityAllocation, FullCapacityAllocationClass,
        MAX_ADMISSION_CLAIMS, MAX_ARTIFACT_REFERENCE_BYTES, ParticipantCheckpoint,
        ParticipantCheckpointState, ParticipantDescriptor, ParticipantId, ParticipantKind,
        ParticipantRequirement, StoredRelativePath, StoredSourceFence,
    };
    use crate::operation_journal::{
        NamespaceCommandArgs, NamespaceDirectoryEndpoint, NamespaceDirectoryExpectation,
        NamespaceDirectoryLocator, NamespaceEntryEndpoint, NamespaceEntryExpectation,
        NamespaceFileExpectation, NamespaceMoveIntentArgs,
    };
    use crate::sample_sources::SourceId;
    use serde_json::json;
    use uuid::Uuid;

    fn participant_id(value: u128) -> ParticipantId {
        ParticipantId::from_uuid(Uuid::from_u128(value))
    }

    fn participant_set_with_two_sources_and_artifacts() -> ParticipantSet {
        let source_a = participant_id(11);
        let source_b = participant_id(12);
        let filesystem = participant_id(13);
        let global = participant_id(14);
        let harvest = participant_id(15);
        let projection_a = participant_id(16);
        let readiness_a = participant_id(17);
        let projection_b = participant_id(18);
        let readiness_b = participant_id(19);
        let artifact_a = participant_id(20);
        let artifact_b = participant_id(21);
        let source_a_id = SourceId::from_string("source-a");
        let source_b_id = SourceId::from_string("source-b");
        let fence_a = StoredSourceFence::new(source_a_id.clone(), "root-a", "db-a", 1, 7, 1)
            .expect("source A fence");
        let fence_b = StoredSourceFence::new(source_b_id.clone(), "root-b", "db-b", 1, 8, 1)
            .expect("source B fence");
        let descriptors = vec![
            ParticipantDescriptor::new(
                source_a,
                ParticipantKind::SourceDatabase,
                ParticipantRequirement::Required,
                Some(fence_a.clone()),
                None,
            )
            .expect("source A"),
            ParticipantDescriptor::new(
                source_b,
                ParticipantKind::SourceDatabase,
                ParticipantRequirement::Required,
                Some(fence_b.clone()),
                None,
            )
            .expect("source B"),
            ParticipantDescriptor::new(
                filesystem,
                ParticipantKind::Filesystem,
                ParticipantRequirement::Required,
                None,
                None,
            )
            .expect("filesystem"),
            ParticipantDescriptor::new(
                global,
                ParticipantKind::GlobalLibrary,
                ParticipantRequirement::Required,
                None,
                None,
            )
            .expect("global"),
            ParticipantDescriptor::new(
                harvest,
                ParticipantKind::Harvest,
                ParticipantRequirement::Required,
                None,
                None,
            )
            .expect("harvest"),
            ParticipantDescriptor::new(
                projection_a,
                ParticipantKind::Projection,
                ParticipantRequirement::Required,
                Some(fence_a.clone()),
                None,
            )
            .expect("projection A"),
            ParticipantDescriptor::new(
                readiness_a,
                ParticipantKind::Readiness,
                ParticipantRequirement::Required,
                Some(fence_a.clone()),
                None,
            )
            .expect("readiness A"),
            ParticipantDescriptor::new(
                projection_b,
                ParticipantKind::Projection,
                ParticipantRequirement::Required,
                Some(fence_b.clone()),
                None,
            )
            .expect("projection B"),
            ParticipantDescriptor::new(
                readiness_b,
                ParticipantKind::Readiness,
                ParticipantRequirement::Required,
                Some(fence_b.clone()),
                None,
            )
            .expect("readiness B"),
            ParticipantDescriptor::new(
                artifact_a,
                ParticipantKind::Artifact,
                ParticipantRequirement::OptionalRebuildable,
                Some(fence_a),
                Some("artifact-a".to_owned()),
            )
            .expect("artifact A"),
            ParticipantDescriptor::new(
                artifact_b,
                ParticipantKind::Artifact,
                ParticipantRequirement::OptionalRebuildable,
                Some(fence_b),
                Some("artifact-b".to_owned()),
            )
            .expect("artifact B"),
        ];
        let checkpoints = descriptors
            .iter()
            .map(|descriptor| {
                ParticipantCheckpoint::new(
                    descriptor.id(),
                    ParticipantCheckpointState::Pending,
                    None,
                    None,
                )
                .expect("pending checkpoint")
            })
            .collect();
        ParticipantSet::new(descriptors, checkpoints).expect("participants")
    }

    fn cross_source_move_args() -> UniversalCommandArgs {
        let source_id = SourceId::from_string("source-a");
        let destination_id = SourceId::from_string("source-b");
        let source = NamespaceEntryEndpoint::new(
            participant_id(11),
            source_id,
            StoredRelativePath::new("source.wav").expect("source path"),
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "source-file", 32, Some(4))
                    .expect("source expectation"),
            ),
        )
        .expect("source endpoint");
        let destination_parent = NamespaceDirectoryEndpoint::new(
            participant_id(12),
            destination_id,
            NamespaceDirectoryLocator::Root,
            NamespaceDirectoryExpectation::new(8, "root-b", None).expect("destination root"),
        )
        .expect("destination endpoint");
        UniversalCommandArgs::Namespace(NamespaceCommandArgs::Move(
            NamespaceMoveIntentArgs::new(source, destination_parent).expect("move args"),
        ))
    }

    fn valid_observation() -> FullProgressIntentObservation {
        let participants = participant_set_with_two_sources_and_artifacts();
        let args = cross_source_move_args();
        let plan = super::super::work_plan::derive_work_plan(&args, &participants)
            .expect("cross-source artifact plan");
        let capacity_plan = FullCapacityPlan::new(vec![
            FullCapacityAllocation::new(
                crate::durable_capacity::VolumeIdentity { device: 1 },
                4096,
                FullCapacityAllocationClass::DestinationStaging,
                4096,
                4096,
                PROTECTED_FREE_SPACE_FLOOR,
            )
            .expect("capacity allocation"),
        ])
        .expect("capacity plan");
        let admission_claims = AdmissionClaimInventory::new(vec![
            AdmissionClaim::new(
                AdmissionClaimId::from_uuid(Uuid::from_u128(300)).expect("claim ID"),
                crate::durable_capacity::VolumeIdentity { device: 1 },
                FullCapacityAllocationClass::DestinationStaging,
                4096,
            )
            .expect("claim"),
        ])
        .expect("claim inventory");
        FullProgressIntentObservation::new(
            OperationId::from_uuid(Uuid::from_u128(301)),
            args,
            participants,
            plan,
            capacity_plan,
            admission_claims,
        )
        .expect("valid full-progress intent observation")
    }

    #[test]
    fn valid_multisource_artifact_intent_round_trips_exactly() {
        let observation = valid_observation();
        observation.validate().expect("valid observation");
        assert_eq!(observation.participants().descriptors().len(), 11);
        assert!(
            observation
                .participants()
                .descriptors()
                .iter()
                .any(|descriptor| descriptor.artifact_key() == Some("artifact-a"))
        );
        assert_eq!(observation.plan().units().len(), 17);

        let encoded = serde_json::to_value(&observation).expect("observation JSON");
        let decoded: FullProgressIntentObservation =
            serde_json::from_value(encoded).expect("observation round trip");
        assert_eq!(decoded, observation);
        assert_eq!(decoded.args(), observation.args());
        assert_eq!(decoded.capacity_plan(), observation.capacity_plan());
        assert_eq!(decoded.admission_claims(), observation.admission_claims());
    }

    #[test]
    fn malformed_decoded_intent_values_fail_semantic_validation() {
        let observation = valid_observation();
        let base = serde_json::to_value(&observation).expect("observation JSON");

        let mut nil_id = base.clone();
        nil_id["operation_id"] = json!(Uuid::nil());
        let decoded: FullProgressIntentObservation =
            serde_json::from_value(nil_id).expect("nil ID shape");
        assert_eq!(
            decoded.validate(),
            Err(FullProgressIntentObservationError::NilOperationId)
        );

        let mut noninitial = base.clone();
        noninitial["participants"]["checkpoints"][0]["state"] = json!("Applied");
        let decoded: FullProgressIntentObservation =
            serde_json::from_value(noninitial).expect("noninitial shape");
        assert!(matches!(
            decoded.validate(),
            Err(FullProgressIntentObservationError::Participants(
                ParticipantValidationError::InitialCheckpointNotPending { .. }
            ))
        ));

        let mut mismatched_args = base.clone();
        mismatched_args["args"]["Namespace"]["Move"]["source"]["source_id"] = json!("source-b");
        let decoded: FullProgressIntentObservation =
            serde_json::from_value(mismatched_args).expect("mismatched args shape");
        assert!(matches!(
            decoded.validate(),
            Err(FullProgressIntentObservationError::Plan(_))
                | Err(FullProgressIntentObservationError::Arguments(_))
        ));

        let mut mismatched_plan = base.clone();
        mismatched_plan["plan"]["units"][0]["key"]["step_ordinal"] = json!(999);
        let decoded: FullProgressIntentObservation =
            serde_json::from_value(mismatched_plan).expect("mismatched plan shape");
        assert!(matches!(
            decoded.validate(),
            Err(FullProgressIntentObservationError::Plan(_))
        ));

        let mut invalid_capacity = base.clone();
        invalid_capacity["capacity_plan"]["allocations"][0]["allocated_bytes"] = json!(0);
        let decoded: FullProgressIntentObservation =
            serde_json::from_value(invalid_capacity).expect("capacity shape");
        assert!(matches!(
            decoded.validate(),
            Err(FullProgressIntentObservationError::Capacity(_))
        ));

        let mut overflow_capacity = base.clone();
        overflow_capacity["capacity_plan"]["allocations"][0]["allocation_unit"] = json!(3);
        overflow_capacity["capacity_plan"]["allocations"][0]["logical_bytes"] = json!(u64::MAX - 1);
        overflow_capacity["capacity_plan"]["allocations"][0]["allocated_bytes"] = json!(u64::MAX);
        let decoded: FullProgressIntentObservation =
            serde_json::from_value(overflow_capacity).expect("capacity overflow shape");
        assert!(matches!(
            decoded.validate(),
            Err(FullProgressIntentObservationError::Capacity(_))
        ));

        let mut duplicate_allocations = base.clone();
        let allocation = duplicate_allocations["capacity_plan"]["allocations"][0].clone();
        duplicate_allocations["capacity_plan"]["allocations"] =
            json!([allocation.clone(), allocation]);
        let decoded: FullProgressIntentObservation =
            serde_json::from_value(duplicate_allocations).expect("duplicate capacity shape");
        assert!(matches!(
            decoded.validate(),
            Err(FullProgressIntentObservationError::Capacity(_))
        ));

        let mut invalid_claims = base.clone();
        invalid_claims["admission_claims"]["claims"][0]["allocated_bytes"] = json!(1);
        let decoded: FullProgressIntentObservation =
            serde_json::from_value(invalid_claims).expect("claim shape");
        assert!(matches!(
            decoded.validate(),
            Err(FullProgressIntentObservationError::AdmissionClaims(_))
        ));

        let mut duplicate_claims = base.clone();
        let claim = duplicate_claims["admission_claims"]["claims"][0].clone();
        duplicate_claims["admission_claims"]["claims"] = json!([claim.clone(), claim]);
        let decoded: FullProgressIntentObservation =
            serde_json::from_value(duplicate_claims).expect("duplicate claims shape");
        assert!(matches!(
            decoded.validate(),
            Err(FullProgressIntentObservationError::AdmissionClaims(_))
        ));

        let mut too_many_claims = base.clone();
        too_many_claims["admission_claims"]["claims"] =
            json!(vec![claim; MAX_ADMISSION_CLAIMS + 1]);
        let decoded: FullProgressIntentObservation =
            serde_json::from_value(too_many_claims).expect("claim count shape");
        assert!(matches!(
            decoded.validate(),
            Err(FullProgressIntentObservationError::AdmissionClaims(_))
        ));

        let mut long_artifact_key = base;
        long_artifact_key["participants"]["descriptors"][9]["artifact_key"] =
            json!("x".repeat(MAX_ARTIFACT_REFERENCE_BYTES + 1));
        let decoded: FullProgressIntentObservation =
            serde_json::from_value(long_artifact_key).expect("artifact-key shape");
        assert!(matches!(
            decoded.validate(),
            Err(FullProgressIntentObservationError::Participants(_))
        ));
    }

    #[test]
    fn unknown_fields_are_rejected_by_the_observation_envelope() {
        let observation = valid_observation();
        let mut value = serde_json::to_value(observation).expect("observation JSON");
        value["unexpected"] = json!(true);
        let error = serde_json::from_value::<FullProgressIntentObservation>(value)
            .expect_err("unknown field");
        assert!(error.to_string().contains("unknown field"));
    }
}
