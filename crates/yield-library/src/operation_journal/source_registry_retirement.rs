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
// Fixed streaming scratch; complete family encoding cannot grow beyond its existing bound.
struct ForwardPolicyDigest(blake3::Hasher, usize);
impl std::io::Write for ForwardPolicyDigest {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.1 = self
            .1
            .checked_add(bytes.len())
            .filter(|n| *n <= MAX_SOURCE_REGISTRY_RETIREMENT_BYTES)
            .ok_or_else(|| std::io::Error::other("retirement policy bound"))?;
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[derive(Serialize)]
struct CanonicalArgs<'a, F: Serialize> {
    draining: &'a str,
    global_participant: ParticipantId,
    lease_fence: F,
    retired: &'a str,
    source_participant: ParticipantId,
    version: u8,
}
#[derive(Serialize)]
enum CanonicalCommand<'a, F: Serialize> {
    SourceRegistryRetirement(CanonicalArgs<'a, F>),
}
#[derive(Serialize)]
enum BorrowedCommand<'a> {
    SourceRegistryRetirement(&'a SourceRegistryRetirementIntentArgs),
}
struct CommandDigest(blake3::Hasher, usize);
impl std::io::Write for CommandDigest {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.1 = self
            .1
            .checked_add(bytes.len())
            .filter(|n| *n <= MAX_SOURCE_REGISTRY_RETIREMENT_BYTES + 64)
            .ok_or(std::io::ErrorKind::InvalidInput)?;
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn lexical_codec_map() -> bool {
    // This fixed probe is prepaid by the caller and destroyed before hashing.
    const P: usize = std::mem::size_of::<usize>();
    const PAIR: usize = std::mem::size_of::<String>() + std::mem::size_of::<serde_json::Value>();
    const TREE: usize = 16 * P + 11 * PAIR;
    const INDEX: usize = 4 * (P + PAIR) + 8 * (P + 1) + 32 * P;
    const _: () = assert!(
        std::mem::size_of::<serde_json::Map<String, serde_json::Value>>()
            + if TREE > INDEX { TREE } else { INDEX }
            + 2
            <= 2048
    );
    let mut map = serde_json::Map::new();
    map.insert("b".into(), serde_json::Value::Null);
    map.insert("a".into(), serde_json::Value::Null);
    map.keys().next().is_some_and(|key| key == "a")
}
impl SourceRegistryRetirementIntentArgs {
    pub(crate) fn validation_scratch_bound(&self) -> Option<usize> {
        self.draining
            .len()
            .checked_add(self.retired.len())?
            .checked_mul(5)?
            .checked_add(4096)
    }
    pub(crate) fn canonical_command_digest(&self) -> Result<[u8; 32], SourceLeaseBindingError> {
        let lexical = lexical_codec_map();
        let mut digest = CommandDigest(
            blake3::Hasher::new_derive_key("yield.database-command-family.v1"),
            0,
        );
        digest.0.update(&super::model::SCHEMA_V18.to_le_bytes());
        digest.0.update(&[1]);
        self.write_command_ordered(&mut digest, lexical)?;
        Ok(*digest.0.finalize().as_bytes())
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn write_codec_normalized_command(
        &self,
        writer: impl std::io::Write,
    ) -> Result<(), SourceLeaseBindingError> {
        self.write_command_ordered(writer, lexical_codec_map())
    }
    fn write_command_ordered(
        &self,
        writer: impl std::io::Write,
        lexical: bool,
    ) -> Result<(), SourceLeaseBindingError> {
        if !lexical {
            return serde_json::to_writer(writer, &BorrowedCommand::SourceRegistryRetirement(self))
                .map_err(|_| SourceLeaseBindingError);
        }
        let command = CanonicalCommand::SourceRegistryRetirement(CanonicalArgs {
            draining: &self.draining,
            global_participant: self.global_participant,
            lease_fence: self.lease_fence.canonical_retirement_view(),
            retired: &self.retired,
            source_participant: self.source_participant,
            version: self.version,
        });
        serde_json::to_writer(writer, &command).map_err(|_| SourceLeaseBindingError)
    }

    /// Shared exact frozen-family correlation only. Callers validate the complete DTO first;
    /// this bounded streaming digest grants no current registration, policy or write authority.
    pub(crate) fn initial_forward_policy_digest(
        &self,
    ) -> Result<[u8; 32], SourceLeaseBindingError> {
        let mut digest = ForwardPolicyDigest(
            blake3::Hasher::new_derive_key("yield.global-retirement-initial-forward.v1"),
            0,
        );
        serde_json::to_writer(&mut digest, self).map_err(|_| SourceLeaseBindingError)?;
        Ok(*digest.0.finalize().as_bytes())
    }

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
        // Check the same complete family limit without a second owned encoding.
        self.initial_forward_policy_digest()?;
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
