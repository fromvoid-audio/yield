//! Bounded frozen retirement metadata. Storage/decoding confers no admission or release grant.
use super::source_lease_binding::{SourceLeaseBindingError, StoredSourceLeaseFence};
use super::work_plan::{WorkUnitKey, WorkUnitRole};
use super::{
    ParticipantDescriptor, ParticipantId, ParticipantKind, ParticipantRequirement, ParticipantSet,
};
use crate::sample_sources::db::{
    SourceRegistryReferenceProposal, SourceRegistryReferenceSubject,
    SourceRegistryReferenceTransition,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

/// Combined frozen scope/proposal argument ceiling, before the complete record's outer bound.
pub const MAX_SOURCE_REGISTRY_RETIREMENT_BYTES: usize = 64 * 1024;
/// Explicit drain/lease-release/retired proposal family; no constructor grants live permission.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRegistryRetirementIntentArgs {
    version: u8,
    source_participant: ParticipantId,
    global_participant: ParticipantId,
    lease_fence: StoredSourceLeaseFence,
    draining: String,
    retired: String,
}
impl SourceRegistryRetirementIntentArgs {
    /// Form finite historical arguments. Actual admission must authenticate all live owners.
    pub fn new(
        source_participant: ParticipantId,
        global_participant: ParticipantId,
        lease_fence: StoredSourceLeaseFence,
        draining: &SourceRegistryReferenceProposal,
        retired: &SourceRegistryReferenceProposal,
    ) -> Result<Self, SourceLeaseBindingError> {
        let value = Self {
            version: 1,
            source_participant,
            global_participant,
            lease_fence,
            draining: URL_SAFE_NO_PAD.encode(draining.canonical_bytes()),
            retired: URL_SAFE_NO_PAD.encode(retired.canonical_bytes()),
        };
        value.validate()?;
        Ok(value)
    }
    /// Validate exact bounded frozen shape only; observed registration phase is not readiness.
    pub fn validate(&self) -> Result<(), SourceLeaseBindingError> {
        self.lease_fence.validate()?;
        if self.version != 1
            || self.source_participant.as_uuid().is_nil()
            || self.global_participant.as_uuid().is_nil()
            || self.source_participant == self.global_participant
            || !self.lease_fence.lifecycle_observed_registered()
            || self.draining.len() > 21846
            || self.retired.len() > 21846
        {
            return Err(SourceLeaseBindingError);
        }
        let (draining, retired) = self.proposals()?;
        if draining.operation_id() != retired.operation_id()
            || draining.transition() != SourceRegistryReferenceTransition::Draining
            || retired.transition() != SourceRegistryReferenceTransition::Retired
            || draining.work_key() != self.draining_work()
            || retired.work_key() != self.retired_work()
            || draining.subject() != retired.subject()
            || !same_subject(self.lease_fence.subject(), draining.subject())
        {
            return Err(SourceLeaseBindingError);
        }
        if serde_json::to_vec(self)
            .map_err(|_| SourceLeaseBindingError)?
            .len()
            > MAX_SOURCE_REGISTRY_RETIREMENT_BYTES
        {
            return Err(SourceLeaseBindingError);
        }
        Ok(())
    }
    fn proposals(
        &self,
    ) -> Result<
        (
            SourceRegistryReferenceProposal,
            SourceRegistryReferenceProposal,
        ),
        SourceLeaseBindingError,
    > {
        let decode = |text: &str| {
            if text.len() > 21846 {
                return Err(SourceLeaseBindingError);
            }
            let bytes = URL_SAFE_NO_PAD
                .decode(text)
                .map_err(|_| SourceLeaseBindingError)?;
            let proposal = SourceRegistryReferenceProposal::decode(&bytes)
                .map_err(|_| SourceLeaseBindingError)?;
            if URL_SAFE_NO_PAD.encode(proposal.canonical_bytes()) != text {
                return Err(SourceLeaseBindingError);
            }
            Ok(proposal)
        };
        Ok((decode(&self.draining)?, decode(&self.retired)?))
    }
    /// Match exactly one required lease participant and one required global participant.
    pub fn validate_against_participants(
        &self,
        participants: &ParticipantSet,
    ) -> Result<(), SourceLeaseBindingError> {
        self.validate()?;
        participants
            .validate()
            .map_err(|_| SourceLeaseBindingError)?;
        let descriptor = |id| {
            participants
                .descriptors()
                .iter()
                .find(|d| d.id() == id)
                .ok_or(SourceLeaseBindingError)
        };
        let source = descriptor(self.source_participant)?;
        let global = descriptor(self.global_participant)?;
        if participants.descriptors().len() != 2
            || source.kind() != ParticipantKind::SourceLeaseMaintenance
            || source.requirement() != ParticipantRequirement::Required
            || source.lease_fence() != Some(&self.lease_fence)
            || source.source_fence().is_some()
            || global.kind() != ParticipantKind::GlobalLibrary
            || global.requirement() != ParticipantRequirement::Required
            || global.source_fence().is_some()
            || global.lease_fence().is_some()
        {
            return Err(SourceLeaseBindingError);
        }
        Ok(())
    }
    /// Exact stored source lease scope, without an authoritative revision.
    pub fn lease_fence(&self) -> &StoredSourceLeaseFence {
        &self.lease_fence
    }
    /// Stored participant identity, not the live owner.
    pub fn source_participant(&self) -> ParticipantId {
        self.source_participant
    }
    /// Source release's exact work correlation. Actual execution remains separately admitted.
    pub fn source_work(&self) -> WorkUnitKey {
        WorkUnitKey::new(
            0,
            1,
            self.source_participant,
            WorkUnitRole::SourceLeaseMaintenance,
        )
    }
    /// Frozen global drain work, never an applied receipt.
    pub fn draining_work(&self) -> WorkUnitKey {
        WorkUnitKey::new(0, 0, self.global_participant, WorkUnitRole::GlobalLibrary)
    }
    /// Frozen global retired work, requiring genuine admitted release/close at execution.
    pub fn retired_work(&self) -> WorkUnitKey {
        WorkUnitKey::new(0, 2, self.global_participant, WorkUnitRole::GlobalLibrary)
    }
    pub(crate) fn operation_id(&self) -> Result<super::OperationId, SourceLeaseBindingError> {
        Ok(self.proposals()?.0.operation_id())
    }
    /// Build the exact bounded stored source descriptor. It grants no prepared owner or permit.
    pub fn source_descriptor(&self) -> Result<ParticipantDescriptor, SourceLeaseBindingError> {
        self.validate()?;
        ParticipantDescriptor::source_lease_maintenance(
            self.source_participant,
            self.lease_fence.clone(),
        )
        .map_err(|_| SourceLeaseBindingError)
    }
}
fn same_subject(a: &SourceRegistryReferenceSubject, b: &SourceRegistryReferenceSubject) -> bool {
    a.source_id() == b.source_id()
        && a.lifecycle_generation() == b.lifecycle_generation()
        && a.profile_acquisition() == b.profile_acquisition()
        && a.lease_owner() == b.lease_owner()
        && a.lease_epoch() == b.lease_epoch()
        && a.media_root_identity() == b.media_root_identity()
        && a.database_root_identity() == b.database_root_identity()
        && a.database_identity() == b.database_identity()
        && a.root_binding_identity() == b.root_binding_identity()
        && a.scan_revision() == b.scan_revision()
        && a.audit() == b.audit()
        && a.registration().map(|r| r.registration_id())
            == b.registration().map(|r| r.registration_id())
}
