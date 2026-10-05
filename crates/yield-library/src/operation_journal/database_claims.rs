//! Bounded immutable database suballocations. These are observations, never live permits.
//!
//! The journal's private settlement boundary persists this image with the original ordinary
//! claims. Genuine owner preparation, direction legality and capacity are separate checks.

use std::collections::BTreeSet;
use thiserror::Error;
use uuid::Uuid;

pub(crate) mod retirement_forward_policy;

use super::admission_claims::AdmissionClaimInventory;
use super::full_capacity::{FullCapacityAllocationClass, FullCapacityPlan};
use super::participants::{MAX_PARTICIPANTS, MAX_SOURCE_IDENTITY_BYTES};

pub(crate) const MAX_DATABASES: usize = 64;
pub(crate) const MAX_SLICES: usize = 256;
pub(crate) const MAX_INVENTORY_BYTES: usize = 64 * 1024;
pub(crate) const MAX_RELATION_ROWS: usize = 8192;
pub(crate) const READ_PAGE_ROWS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum DatabaseRole {
    Source = 1,
    Global = 2,
}
#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
#[repr(u8)]
pub(crate) enum ClaimDirection {
    Routine = 0,
    RequiredForward = 1,
    RequiredCompensation = 2,
    LeaseMaintenance = 3,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub(crate) enum RevisionDomain {
    SourceAuthoritative = 1,
    SourceLeaseLifecycle = 6,
    GlobalMutationGeneration = 2,
    GlobalRegistry = 3,
    GlobalPreferences = 4,
    GlobalVocabulary = 5,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InventoryHeader {
    pub profile: String,
    pub acquisition: String,
    pub ownership_generation: u64,
    pub decision_nonce: Uuid,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceBinding {
    pub source_id: String,
    pub media_root: String,
    pub lease_owner: String,
    pub lease_epoch: u64,
    pub lease_fence: Option<super::source_lease_binding::StoredSourceLeaseFence>,
}
impl SourceBinding {
    pub(crate) fn from_lease_fence(
        fence: &super::source_lease_binding::StoredSourceLeaseFence,
    ) -> Result<Self> {
        fence
            .validate()
            .map_err(|_| DatabaseClaimError("lease fence"))?;
        require(
            fence.lifecycle_observed_registered(),
            "lease registration phase",
        )?;
        let subject = fence.subject();
        Ok(Self {
            source_id: subject.source_id().into(),
            media_root: fence.claim_media_root(),
            lease_owner: subject.lease_owner().to_string(),
            lease_epoch: subject.lease_epoch(),
            lease_fence: Some(fence.clone()),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DatabaseParticipant {
    pub id: Uuid,
    /// Required=1, optional/rebuildable=2. Not an existing enum discriminant.
    pub requirement: u8,
    pub revision_domain: RevisionDomain,
    pub command_digest: [u8; 32],
    pub before: u64,
    pub after: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DatabaseBinding {
    pub id: Uuid,
    pub role: DatabaseRole,
    pub volume: u64,
    pub allocation_unit: u64,
    pub main_file: String,
    pub registration: Uuid,
    pub owner_generation: u64,
    pub lifecycle: u64,
    pub database_root: String,
    pub root_binding: String,
    pub source: Option<SourceBinding>,
    pub current_revision: u64,
    pub participants: Vec<DatabaseParticipant>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DatabaseClaimSlice {
    pub id: Uuid,
    pub claim: Uuid,
    pub binding: Uuid,
    pub participant: Uuid,
    pub direction: ClaimDirection,
    pub class: FullCapacityAllocationClass,
    pub ceiling: u64,
    pub wal_ceiling: u64,
    pub shm_ceiling: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InventoryPolicy {
    Historical,
    RetirementInitialForward,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DatabaseClaimInventory {
    policy: InventoryPolicy,
    header: InventoryHeader,
    bindings: Vec<DatabaseBinding>,
    slices: Vec<DatabaseClaimSlice>,
}

#[derive(Debug, Error)]
#[error("unsupported database claim inventory: {0}")]
pub(crate) struct DatabaseClaimError(pub(crate) &'static str);
type Result<T> = std::result::Result<T, DatabaseClaimError>;
fn require(ok: bool, reason: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(DatabaseClaimError(reason))
    }
}
fn identity(s: &str) -> Result<()> {
    require(
        !s.is_empty() && s.len() <= MAX_SOURCE_IDENTITY_BYTES,
        "identity bound",
    )
}
fn integer(n: u64, positive: bool) -> Result<()> {
    require(
        n <= i64::MAX as u64 && (!positive || n > 0),
        "SQLite integer range",
    )
}
fn uuid(id: Uuid) -> Result<()> {
    require(!id.is_nil(), "nil UUID")
}
fn class_tag(class: FullCapacityAllocationClass) -> Result<u8> {
    use FullCapacityAllocationClass::*;
    match class {
        SourceDatabaseCommit => Ok(1),
        SourceWalShm => Ok(2),
        GlobalDatabaseCommit => Ok(3),
        GlobalWalShm => Ok(4),
        _ => Err(DatabaseClaimError("non-database allocation class")),
    }
}

impl DatabaseClaimInventory {
    /// Compare the immutable participant envelope and actual codec-normalized family. This
    /// does not validate a live registration or decide whether compensation is currently legal.
    pub(crate) fn validate_record(
        &self,
        record: &super::codec::StoredOperationRecord,
    ) -> Result<()> {
        use super::participants::{ParticipantKind, ParticipantRequirement};
        self.validate()?;
        let universal = record
            .universal_intent
            .as_ref()
            .ok_or(DatabaseClaimError("missing participant envelope"))?;
        let descriptors = universal.participants().descriptors();
        let required_digest = command_digest(record, 1)?;
        let optional_digest = command_digest(record, 2)?;
        for b in &self.bindings {
            for p in &b.participants {
                let d = descriptors
                    .iter()
                    .find(|d| d.id().as_uuid() == p.id)
                    .ok_or(DatabaseClaimError("undeclared database participant"))?;
                let requirement = match d.requirement() {
                    ParticipantRequirement::Required => 1,
                    ParticipantRequirement::OptionalRebuildable => 2,
                };
                require(
                    p.requirement == requirement,
                    "participant requirement mismatch",
                )?;
                require(
                    p.command_digest
                        == if requirement == 1 {
                            required_digest
                        } else {
                            optional_digest
                        },
                    "immutable command digest mismatch",
                )?;
                match b.role {
                    DatabaseRole::Source => {
                        if p.revision_domain == RevisionDomain::SourceLeaseLifecycle {
                            require(
                                record.schema_version == super::model::SCHEMA_V18
                                    && record.intent.kind
                                        == super::model::OperationKind::SourceRegistryRetirement
                                    && d.kind() == ParticipantKind::SourceLeaseMaintenance
                                    && d.source_fence().is_none(),
                                "lease participant/family/version",
                            )?;
                            let Some(
                                super::coordinator::UniversalCommandArgs::SourceRegistryRetirement(
                                    args,
                                ),
                            ) = record.universal_command_args()
                            else {
                                return Err(DatabaseClaimError("lease command family"));
                            };
                            args.validate_against_participants(universal.participants())
                                .map_err(|_| DatabaseClaimError("lease command envelope"))?;
                            let fence = b
                                .source
                                .as_ref()
                                .and_then(|s| s.lease_fence.as_ref())
                                .ok_or(DatabaseClaimError("missing lease scope"))?;
                            require(
                                d.lease_fence() == Some(fence)
                                    && args.lease_fence() == fence
                                    && args.source_participant().as_uuid() == p.id,
                                "immutable lease fence mismatch",
                            )?;
                            continue;
                        }
                        require(
                            matches!(
                                d.kind(),
                                ParticipantKind::SourceDatabase
                                    | ParticipantKind::Readiness
                                    | ParticipantKind::Artifact
                            ),
                            "source participant role",
                        )?;
                        let f = d
                            .source_fence()
                            .ok_or(DatabaseClaimError("missing source fence"))?;
                        let s = b.source.as_ref().expect("validated source binding");
                        require(
                            f.source_id().as_str() == s.source_id
                                && f.root_identity() == s.media_root
                                && f.database_identity() == b.main_file
                                && f.lifecycle_generation() == b.lifecycle
                                && f.lease_epoch() == s.lease_epoch
                                && f.source_revision() == b.current_revision,
                            "immutable source fence mismatch",
                        )?;
                    }
                    DatabaseRole::Global => require(
                        matches!(
                            d.kind(),
                            ParticipantKind::GlobalLibrary | ParticipantKind::Harvest
                        ),
                        "global participant role",
                    )?,
                }
            }
        }
        for d in descriptors {
            if matches!(
                d.kind(),
                ParticipantKind::SourceDatabase
                    | ParticipantKind::SourceLeaseMaintenance
                    | ParticipantKind::GlobalLibrary
                    | ParticipantKind::Harvest
                    | ParticipantKind::Readiness
                    | ParticipantKind::Artifact
            ) {
                require(
                    self.bindings
                        .iter()
                        .any(|b| b.participants.iter().any(|p| p.id == d.id().as_uuid())),
                    "omitted database participant",
                )?;
            }
        }
        if self.policy == InventoryPolicy::RetirementInitialForward {
            retirement_forward_policy::validate_record(self, record)?;
        }
        Ok(())
    }
    /// Normalize validated bounded inputs. Physical aliases must already be deduplicated by
    /// genuine preparation; a second binding for the same file is rejected even if spelling differs.
    pub(crate) fn new(
        header: InventoryHeader,
        mut bindings: Vec<DatabaseBinding>,
        mut slices: Vec<DatabaseClaimSlice>,
    ) -> Result<Self> {
        // Count and field bounds are checked before sorting or building an owned encoding.
        require(
            !bindings.is_empty()
                && bindings.len() <= MAX_DATABASES
                && !slices.is_empty()
                && slices.len() <= MAX_SLICES,
            "inventory count",
        )?;
        for b in &bindings {
            require(
                b.participants.len() <= MAX_PARTICIPANTS,
                "participant count",
            )?;
        }
        bindings.sort_by(|a, b| {
            (a.volume, &a.main_file, a.role as u8).cmp(&(b.volume, &b.main_file, b.role as u8))
        });
        for b in &mut bindings {
            b.participants.sort_by_key(|p| p.id);
        }
        slices.sort_by_key(|s| (s.claim, s.binding, s.participant, s.direction, s.id));
        let value = Self {
            policy: InventoryPolicy::Historical,
            header,
            bindings,
            slices,
        };
        value.validate()?;
        Ok(value)
    }
    /// Explicit initial-forward storage policy, never a live acknowledgement or grant.
    /// Historical constructors/versions retain their existing observational semantics.
    pub(crate) fn new_retirement_forward(
        header: InventoryHeader,
        bindings: Vec<DatabaseBinding>,
        slices: Vec<DatabaseClaimSlice>,
    ) -> Result<Self> {
        let mut value = Self::new(header, bindings, slices)?;
        value.policy = InventoryPolicy::RetirementInitialForward;
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn header(&self) -> &InventoryHeader {
        &self.header
    }
    pub(crate) fn bindings(&self) -> &[DatabaseBinding] {
        &self.bindings
    }
    pub(crate) fn slices(&self) -> &[DatabaseClaimSlice] {
        &self.slices
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.policy == InventoryPolicy::RetirementInitialForward {
            retirement_forward_policy::validate_shape(self)?;
        }
        require(
            !self.bindings.is_empty()
                && self.bindings.len() <= MAX_DATABASES
                && !self.slices.is_empty()
                && self.slices.len() <= MAX_SLICES,
            "inventory count",
        )?;
        identity(&self.header.profile)?;
        identity(&self.header.acquisition)?;
        integer(self.header.ownership_generation, true)?;
        uuid(self.header.decision_nonce)?;
        let mut binding_ids = BTreeSet::new();
        let mut participants = BTreeSet::new();
        let mut physical = BTreeSet::new();
        for b in &self.bindings {
            uuid(b.id)?;
            uuid(b.registration)?;
            require(binding_ids.insert(b.id), "duplicate binding")?;
            require(
                physical.insert((b.volume, b.main_file.as_str())),
                "physical alias conflict",
            )?;
            integer(b.allocation_unit, true)?;
            integer(b.owner_generation, true)?;
            integer(b.lifecycle, true)?;
            integer(b.current_revision, false)?;
            identity(&b.main_file)?;
            identity(&b.database_root)?;
            identity(&b.root_binding)?;
            match (b.role, &b.source) {
                (DatabaseRole::Source, Some(s)) => {
                    identity(&s.source_id)?;
                    identity(&s.media_root)?;
                    identity(&s.lease_owner)?;
                    integer(s.lease_epoch, true)?;
                }
                (DatabaseRole::Global, None) => require(b.current_revision > 0, "global counter")?,
                _ => return Err(DatabaseClaimError("source/global shape")),
            }
            require(
                !b.participants.is_empty() && b.participants.len() <= MAX_PARTICIPANTS,
                "participant count",
            )?;
            for p in &b.participants {
                uuid(p.id)?;
                require(participants.insert(p.id), "duplicate participant")?;
                require(
                    participants.len() <= MAX_PARTICIPANTS,
                    "total participant bound",
                )?;
                require(matches!(p.requirement, 1 | 2), "requirement tag")?;
                let lease = b.source.as_ref().and_then(|s| s.lease_fence.as_ref());
                require(
                    match p.revision_domain {
                        RevisionDomain::SourceAuthoritative => {
                            b.role == DatabaseRole::Source && lease.is_none()
                        }
                        RevisionDomain::SourceLeaseLifecycle => {
                            b.role == DatabaseRole::Source && lease.is_some()
                        }
                        _ => b.role == DatabaseRole::Global && lease.is_none(),
                    },
                    "revision domain/role",
                )?;
                if let Some(fence) = lease {
                    fence
                        .validate()
                        .map_err(|_| DatabaseClaimError("lease fence"))?;
                    require(
                        fence.lifecycle_observed_registered(),
                        "lease registration phase",
                    )?;
                    let subject = fence.subject();
                    let source = b.source.as_ref().expect("source binding");
                    require(
                        source == &SourceBinding::from_lease_fence(fence)?
                            && b.main_file == subject.database_identity()
                            && b.database_root == subject.database_root_identity()
                            && b.root_binding == subject.root_binding_identity()
                            && b.lifecycle == subject.lifecycle_generation()
                            && b.owner_generation == subject.lease_epoch()
                            && b.current_revision == subject.scan_revision()
                            && b.registration
                                == subject
                                    .registration()
                                    .expect("validated registration")
                                    .registration_id()
                            && self.header.acquisition == subject.profile_acquisition().to_string()
                            && b.volume == fence.device()
                            && b.allocation_unit == fence.allocation_unit()
                            && p.requirement == 1
                            && p.before == subject.lease_epoch()
                            && p.after == subject.lease_epoch(),
                        "lease binding/epoch/context mismatch",
                    )?;
                }
                integer(p.before, false)?;
                integer(p.after, false)?;
                if matches!(
                    p.revision_domain,
                    RevisionDomain::SourceAuthoritative | RevisionDomain::GlobalMutationGeneration
                ) {
                    require(p.before == b.current_revision, "binding revision mismatch")?;
                }
                require(p.command_digest != [0; 32], "empty command digest")?;
            }
        }
        let mut slice_ids = BTreeSet::new();
        for s in &self.slices {
            for id in [s.id, s.claim, s.binding, s.participant] {
                uuid(id)?;
            }
            require(slice_ids.insert(s.id), "duplicate slice")?;
            integer(s.ceiling, true)?;
            integer(s.wal_ceiling, false)?;
            integer(s.shm_ceiling, false)?;
            let b = self
                .bindings
                .iter()
                .find(|b| b.id == s.binding)
                .ok_or(DatabaseClaimError("missing binding"))?;
            let p = b
                .participants
                .iter()
                .find(|p| p.id == s.participant)
                .ok_or(DatabaseClaimError("missing participant"))?;
            require(
                p.revision_domain != RevisionDomain::SourceLeaseLifecycle
                    || s.direction == ClaimDirection::LeaseMaintenance,
                "lease-only direction",
            )?;
            let tag = class_tag(s.class)?;
            if let Some(fence) = b.source.as_ref().and_then(|s| s.lease_fence.as_ref()) {
                let [main, wal, shm] = fence.write_ceilings();
                require(
                    if tag % 2 == 0 {
                        s.wal_ceiling <= wal && s.shm_ceiling <= shm
                    } else {
                        s.ceiling <= main
                    },
                    "lease slice exceeds captured logical policy",
                )?;
            }
            require(
                (tag <= 2) == (b.role == DatabaseRole::Source),
                "slice class/role",
            )?;
            require(
                s.ceiling % b.allocation_unit == 0
                    && s.wal_ceiling % b.allocation_unit == 0
                    && s.shm_ceiling % b.allocation_unit == 0,
                "allocation rounding",
            )?;
            if tag % 2 == 0 {
                require(
                    s.wal_ceiling.checked_add(s.shm_ceiling) == Some(s.ceiling),
                    "sidecar conservation",
                )?;
            } else {
                require(
                    s.wal_ceiling == 0 && s.shm_ceiling == 0,
                    "database sidecar ceiling",
                )?;
            }
            require(
                s.direction == ClaimDirection::Routine || p.requirement == 1,
                "optional required direction",
            )?;
            require(
                s.direction != ClaimDirection::LeaseMaintenance || b.role == DatabaseRole::Source,
                "global lease direction",
            )?;
        }
        for b in &self.bindings {
            for p in &b.participants {
                if p.revision_domain == RevisionDomain::SourceLeaseLifecycle {
                    let selected = self
                        .slices
                        .iter()
                        .filter(|s| s.binding == b.id && s.participant == p.id)
                        .collect::<Vec<_>>();
                    require(
                        selected.len() == 2
                            && selected
                                .iter()
                                .filter(|s| {
                                    s.class == FullCapacityAllocationClass::SourceDatabaseCommit
                                })
                                .count()
                                == 1
                            && selected
                                .iter()
                                .filter(|s| {
                                    s.class == FullCapacityAllocationClass::SourceWalShm
                                        && s.wal_ceiling > 0
                                        && s.shm_ceiling > 0
                                })
                                .count()
                                == 1,
                        "lease requires exact MAIN and WAL/SHM slice pair",
                    )?;
                }
                require(
                    self.slices
                        .iter()
                        .any(|s| s.binding == b.id && s.participant == p.id),
                    "uncovered participant",
                )?;
            }
        }
        require(
            self.encoded_len()? <= MAX_INVENTORY_BYTES,
            "aggregate encoding bound",
        )
    }

    pub(crate) fn validate_against(
        &self,
        plan: &FullCapacityPlan,
        claims: &AdmissionClaimInventory,
    ) -> Result<()> {
        self.validate()?;
        claims
            .validate_against(plan)
            .map_err(|_| DatabaseClaimError("ordinary coverage"))?;
        for s in &self.slices {
            let b = self
                .bindings
                .iter()
                .find(|b| b.id == s.binding)
                .expect("validated binding");
            let c = claims
                .claims()
                .iter()
                .find(|c| c.claim_id().as_uuid() == s.claim)
                .ok_or(DatabaseClaimError("unknown ordinary claim"))?;
            require(
                c.volume().device == b.volume && c.allocation_class() == s.class,
                "claim volume/class mismatch",
            )?;
            let allocation = plan
                .allocations()
                .iter()
                .find(|a| a.volume() == c.volume() && a.class() == s.class)
                .ok_or(DatabaseClaimError("missing plan allocation"))?;
            require(
                allocation.allocation_unit() == b.allocation_unit,
                "plan allocation unit mismatch",
            )?;
        }
        for c in claims.claims() {
            if class_tag(c.allocation_class()).is_ok() {
                let mut total = 0u64;
                for s in self
                    .slices
                    .iter()
                    .filter(|s| s.claim == c.claim_id().as_uuid())
                {
                    total = total
                        .checked_add(s.ceiling)
                        .ok_or(DatabaseClaimError("slice overflow"))?;
                }
                require(
                    total == c.allocated_bytes(),
                    "exact ordinary claim conservation",
                )?;
            }
        }
        Ok(())
    }

    fn encoded_len(&self) -> Result<usize> {
        let mut n =
            6 + 2 + self.header.profile.len() + 2 + self.header.acquisition.len() + 8 + 16 + 4;
        let version2 = self
            .bindings
            .iter()
            .any(|b| b.source.as_ref().is_some_and(|s| s.lease_fence.is_some()));
        for b in &self.bindings {
            n = n
                .checked_add(
                    16 + 1
                        + 8
                        + 8
                        + 2
                        + b.main_file.len()
                        + 16
                        + 8
                        + 8
                        + 2
                        + b.database_root.len()
                        + 2
                        + b.root_binding.len()
                        + 8
                        + 2,
                )
                .ok_or(DatabaseClaimError("encoding overflow"))?;
            if let Some(s) = &b.source {
                n = n
                    .checked_add(
                        2 + s.source_id.len()
                            + 2
                            + s.media_root.len()
                            + 2
                            + s.lease_owner.len()
                            + 8,
                    )
                    .ok_or(DatabaseClaimError("encoding overflow"))?;
            }
            if version2 && b.source.is_some() {
                n = n
                    .checked_add(4)
                    .ok_or(DatabaseClaimError("encoding overflow"))?;
                if let Some(fence) = b.source.as_ref().and_then(|s| s.lease_fence.as_ref()) {
                    n = n
                        .checked_add(
                            fence
                                .canonical_bytes()
                                .map_err(|_| DatabaseClaimError("lease fence"))?
                                .len(),
                        )
                        .ok_or(DatabaseClaimError("encoding overflow"))?;
                }
            }
            n = n
                .checked_add(
                    b.participants
                        .len()
                        .checked_mul(16 + 1 + 1 + 32 + 8 + 8)
                        .ok_or(DatabaseClaimError("encoding overflow"))?,
                )
                .ok_or(DatabaseClaimError("encoding overflow"))?;
        }
        n.checked_add(
            self.slices
                .len()
                .checked_mul(16 * 4 + 2 + 8 * 3)
                .ok_or(DatabaseClaimError("encoding overflow"))?,
        )
        .ok_or(DatabaseClaimError("encoding overflow"))
    }
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut out = Vec::with_capacity(self.encoded_len()?);
        let version2 = self
            .bindings
            .iter()
            .any(|b| b.source.as_ref().is_some_and(|s| s.lease_fence.is_some()));
        out.extend_from_slice(
            if self.policy == InventoryPolicy::RetirementInitialForward {
                b"YWCB\x03\x00"
            } else if version2 {
                b"YWCB\x02\x00"
            } else {
                b"YWCB\x01\x00"
            },
        );
        put_string(&mut out, &self.header.profile);
        put_string(&mut out, &self.header.acquisition);
        put_u64(&mut out, self.header.ownership_generation);
        put_uuid(&mut out, self.header.decision_nonce);
        out.extend_from_slice(&(self.bindings.len() as u16).to_le_bytes());
        out.extend_from_slice(&(self.slices.len() as u16).to_le_bytes());
        for b in &self.bindings {
            put_uuid(&mut out, b.id);
            out.push(b.role as u8);
            put_u64(&mut out, b.volume);
            put_u64(&mut out, b.allocation_unit);
            put_string(&mut out, &b.main_file);
            put_uuid(&mut out, b.registration);
            put_u64(&mut out, b.owner_generation);
            put_u64(&mut out, b.lifecycle);
            put_string(&mut out, &b.database_root);
            put_string(&mut out, &b.root_binding);
            if let Some(s) = &b.source {
                put_string(&mut out, &s.source_id);
                put_string(&mut out, &s.media_root);
                put_string(&mut out, &s.lease_owner);
                put_u64(&mut out, s.lease_epoch);
                if version2 {
                    let bytes = s
                        .lease_fence
                        .as_ref()
                        .map(|f| f.canonical_bytes())
                        .transpose()
                        .map_err(|_| DatabaseClaimError("lease fence"))?
                        .unwrap_or_default();
                    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                    out.extend_from_slice(&bytes);
                }
            }
            put_u64(&mut out, b.current_revision);
            out.extend_from_slice(&(b.participants.len() as u16).to_le_bytes());
            for p in &b.participants {
                put_uuid(&mut out, p.id);
                out.push(p.requirement);
                out.push(p.revision_domain as u8);
                out.extend_from_slice(&p.command_digest);
                put_u64(&mut out, p.before);
                put_u64(&mut out, p.after);
            }
        }
        for s in &self.slices {
            for id in [s.id, s.claim, s.binding, s.participant] {
                put_uuid(&mut out, id);
            }
            out.push(s.direction as u8);
            out.push(class_tag(s.class)?);
            put_u64(&mut out, s.ceiling);
            put_u64(&mut out, s.wal_ceiling);
            put_u64(&mut out, s.shm_ceiling);
        }
        Ok(out)
    }
    pub(crate) fn digest(&self) -> Result<[u8; 32]> {
        let bytes = self.encode()?;
        let domain = if bytes.starts_with(b"YWCB\x03\x00") {
            "yield.database-claim-inventory.v3"
        } else if bytes.starts_with(b"YWCB\x02\x00") {
            "yield.database-claim-inventory.v2"
        } else {
            "yield.database-claim-inventory.v1"
        };
        Ok(blake3::derive_key(domain, &bytes))
    }
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        require(
            bytes.len() <= MAX_INVENTORY_BYTES,
            "aggregate encoding bound",
        )?;
        let mut r = Reader(bytes);
        let magic = r.take(6)?;
        let retirement_forward = magic == b"YWCB\x03\x00";
        let version2 = retirement_forward || magic == b"YWCB\x02\x00";
        require(
            version2 || magic == b"YWCB\x01\x00",
            "inventory magic/version",
        )?;
        let header = InventoryHeader {
            profile: r.string()?,
            acquisition: r.string()?,
            ownership_generation: r.u64()?,
            decision_nonce: r.uuid()?,
        };
        let nb = r.u16()? as usize;
        let ns = r.u16()? as usize;
        require(
            nb > 0 && nb <= MAX_DATABASES && ns > 0 && ns <= MAX_SLICES,
            "inventory count",
        )?;
        let mut bindings = Vec::with_capacity(nb);
        let mut total = 0usize;
        for _ in 0..nb {
            let id = r.uuid()?;
            let role = match r.u8()? {
                1 => DatabaseRole::Source,
                2 => DatabaseRole::Global,
                _ => return Err(DatabaseClaimError("role tag")),
            };
            let volume = r.u64()?;
            let allocation_unit = r.u64()?;
            let main_file = r.string()?;
            let registration = r.uuid()?;
            let owner_generation = r.u64()?;
            let lifecycle = r.u64()?;
            let database_root = r.string()?;
            let root_binding = r.string()?;
            let source = if role == DatabaseRole::Source {
                Some(SourceBinding {
                    source_id: r.string()?,
                    media_root: r.string()?,
                    lease_owner: r.string()?,
                    lease_epoch: r.u64()?,
                    lease_fence: if version2 {
                        let len = u32::from_le_bytes(r.take(4)?.try_into().expect("scope length"))
                            as usize;
                        require(
                            len <= super::source_lease_binding::MAX_SOURCE_LEASE_SCOPE_BYTES,
                            "lease scope bound",
                        )?;
                        if len == 0 {
                            None
                        } else {
                            Some(
                                super::source_lease_binding::StoredSourceLeaseFence::decode(
                                    r.take(len)?,
                                )
                                .map_err(|_| DatabaseClaimError("lease scope image"))?,
                            )
                        }
                    } else {
                        None
                    },
                })
            } else {
                None
            };
            let current_revision = r.u64()?;
            let np = r.u16()? as usize;
            total = total
                .checked_add(np)
                .ok_or(DatabaseClaimError("participant overflow"))?;
            require(
                np > 0 && total <= MAX_PARTICIPANTS,
                "total participant bound",
            )?;
            let mut participants = Vec::with_capacity(np);
            for _ in 0..np {
                let id = r.uuid()?;
                let requirement = r.u8()?;
                let revision_domain = match r.u8()? {
                    1 => RevisionDomain::SourceAuthoritative,
                    2 => RevisionDomain::GlobalMutationGeneration,
                    3 => RevisionDomain::GlobalRegistry,
                    4 => RevisionDomain::GlobalPreferences,
                    5 => RevisionDomain::GlobalVocabulary,
                    6 if version2 => RevisionDomain::SourceLeaseLifecycle,
                    _ => return Err(DatabaseClaimError("revision tag")),
                };
                participants.push(DatabaseParticipant {
                    id,
                    requirement,
                    revision_domain,
                    command_digest: r.take(32)?.try_into().expect("fixed digest"),
                    before: r.u64()?,
                    after: r.u64()?,
                });
            }
            bindings.push(DatabaseBinding {
                id,
                role,
                volume,
                allocation_unit,
                main_file,
                registration,
                owner_generation,
                lifecycle,
                database_root,
                root_binding,
                source,
                current_revision,
                participants,
            });
        }
        let mut slices = Vec::with_capacity(ns);
        for _ in 0..ns {
            let id = r.uuid()?;
            let claim = r.uuid()?;
            let binding = r.uuid()?;
            let participant = r.uuid()?;
            let direction = match r.u8()? {
                0 => ClaimDirection::Routine,
                1 => ClaimDirection::RequiredForward,
                2 => ClaimDirection::RequiredCompensation,
                3 => ClaimDirection::LeaseMaintenance,
                _ => return Err(DatabaseClaimError("direction tag")),
            };
            let class = match r.u8()? {
                1 => FullCapacityAllocationClass::SourceDatabaseCommit,
                2 => FullCapacityAllocationClass::SourceWalShm,
                3 => FullCapacityAllocationClass::GlobalDatabaseCommit,
                4 => FullCapacityAllocationClass::GlobalWalShm,
                _ => return Err(DatabaseClaimError("class tag")),
            };
            slices.push(DatabaseClaimSlice {
                id,
                claim,
                binding,
                participant,
                direction,
                class,
                ceiling: r.u64()?,
                wal_ceiling: r.u64()?,
                shm_ceiling: r.u64()?,
            });
        }
        require(r.0.is_empty(), "trailing inventory bytes")?;
        let value = if retirement_forward {
            Self::new_retirement_forward(header, bindings, slices)?
        } else {
            Self::new(header, bindings, slices)?
        };
        require(value.encode()? == bytes, "noncanonical inventory order")?;
        Ok(value)
    }
}

/// Digest the actual bounded, validated family as represented by its existing versioned codec.
/// Canonical object ordering is chosen by serde_json::Value, not supplied JSON order. Mutable
/// phase/disposition/checkpoints, outer OperationId and timestamps are excluded. Requirement is bound.
/// Explicit frozen families retain their nested operation/work correlations (including V18).
pub(crate) fn command_digest(
    record: &super::codec::StoredOperationRecord,
    requirement: u8,
) -> Result<[u8; 32]> {
    require(matches!(requirement, 1 | 2), "requirement tag")?;
    let family = command_bytes(record)?;
    let mut hasher = blake3::Hasher::new_derive_key("yield.database-command-family.v1");
    hasher.update(&record.schema_version.to_le_bytes());
    hasher.update(&[requirement]);
    hasher.update(&family);
    Ok(*hasher.finalize().as_bytes())
}
pub(crate) fn command_bytes(record: &super::codec::StoredOperationRecord) -> Result<Vec<u8>> {
    let encoded = super::codec::encode(record)
        .map_err(|_| DatabaseClaimError("unsupported operation codec"))?;
    let value: serde_json::Value =
        serde_json::from_slice(&encoded).map_err(|_| DatabaseClaimError("operation encoding"))?;
    let args = value
        .get("universal_command_args")
        .or_else(|| value.get("command_args"))
        .ok_or(DatabaseClaimError(
            "untyped legacy command cannot bind database claims",
        ))?;
    serde_json::to_vec(args).map_err(|_| DatabaseClaimError("command encoding"))
}
fn put_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}
fn put_u64(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn put_uuid(out: &mut Vec<u8>, id: Uuid) {
    out.extend_from_slice(id.as_bytes());
}
struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        require(self.0.len() >= n, "truncated inventory")?;
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("fixed integer"),
        ))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("fixed integer"),
        ))
    }
    fn uuid(&mut self) -> Result<Uuid> {
        Ok(Uuid::from_bytes(
            self.take(16)?.try_into().expect("fixed UUID"),
        ))
    }
    fn string(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        require(n > 0 && n <= MAX_SOURCE_IDENTITY_BYTES, "identity bound")?;
        std::str::from_utf8(self.take(n)?)
            .map(str::to_owned)
            .map_err(|_| DatabaseClaimError("identity UTF-8"))
    }
}

#[cfg(test)]
mod tests;
