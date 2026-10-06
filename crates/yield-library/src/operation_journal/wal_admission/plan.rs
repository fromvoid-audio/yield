//! Derive preparation coverage from immutable command topology, never caller counts.
use super::super::{
    AdmissionClaimInventory, FullCapacityAllocationClass as Class, FullCapacityPlan,
    ParticipantKind, ParticipantSet, UniversalCommandArgs,
};
use super::input::ClaimStorage;
use super::{HandshakeBudget, HandshakeCredit, SourceInput, SourcePreparation};
use crate::sample_sources::library::{GlobalPreparationRequest, GlobalWalPreparationClient};
use blake3::Hasher;
use std::{
    io::{self, Write},
    sync::Arc,
    time::{Duration, Instant},
};

/// Invalid, unavailable or unfinished pre-intent preparation; never a settlement result.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WalPreparationError {
    /// The original command, exact owner coverage or claim context is unsupported.
    #[error("unsupported all-owner WAL preparation input")]
    InvalidInput,
    /// Complete combined handshake backing/staging cannot fit its original ceiling.
    #[error("all-owner WAL preparation is full")]
    Full,
    /// One handshake is already active on the journal owner.
    #[error("all-owner WAL preparation is busy")]
    Busy,
    /// The minimum original cooperative deadline expired.
    #[error("all-owner WAL preparation deadline expired")]
    Deadline,
    /// The requester was canceled before durable admission.
    #[error("all-owner WAL preparation canceled")]
    Canceled,
    /// Original source preparation refused or lost custody.
    #[error("source WAL preparation refused: {0}")]
    Source(crate::sample_sources::wal_transport::SourceWalTransportRejection),
    /// Original global preparation refused or lost custody.
    #[error("global WAL preparation refused: {0}")]
    Global(crate::sample_sources::library::GlobalPreparationError),
    /// Actual cleanup/liveness could not be confirmed.
    #[error("all-owner WAL preparation outcome is unconfirmed")]
    OwnerLost,
}

/// Validated bounded original-owner preparation input. It is not an admission capability.
/// Fields have no public constructor or decoder; factory validation cannot issue a latch.
/// ```compile_fail
/// use yield_library::operation_journal::owner::WalPreparationInput;
/// let invented = WalPreparationInput {};
/// ```
pub struct WalPreparationInput {
    pub(super) sources: Vec<SourcePreparation>,
    pub(super) global: Option<(GlobalWalPreparationClient, GlobalPreparationRequest)>,
    pub(super) digest: [u8; 32],
    pub(super) global_storage: Option<ClaimStorage>,
    pub(super) deadline: Instant,
    pub(super) budget: Arc<HandshakeBudget>,
    pub(super) credit: HandshakeCredit,
    pub(super) header_bytes: usize,
}
struct HashWriter {
    hash: Hasher,
    bytes: usize,
}
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .bytes
            .checked_add(bytes.len())
            .filter(|n| *n <= super::super::model::MAX_RECORD_BYTES)
            .ok_or_else(|| io::Error::other("bounded immutable command digest"))?;
        self.hash.update(bytes);
        self.bytes = next;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn digest_with_bytes(
    value: &impl serde::Serialize,
) -> Result<([u8; 32], usize), WalPreparationError> {
    let mut writer = HashWriter {
        hash: Hasher::new(),
        bytes: 0,
    };
    writer.hash.update(b"yield.original-wal-preparation.v1\0");
    serde_json::to_writer(&mut writer, value).map_err(|_| WalPreparationError::InvalidInput)?;
    Ok((*writer.hash.finalize().as_bytes(), writer.bytes))
}
pub(in crate::operation_journal) fn prepared_digest(
    args: &UniversalCommandArgs,
    participants: &ParticipantSet,
    plan: &FullCapacityPlan,
    claims: &AdmissionClaimInventory,
) -> Result<[u8; 32], WalPreparationError> {
    digest(&(args, participants, plan, claims))
}
fn digest(value: &impl serde::Serialize) -> Result<[u8; 32], WalPreparationError> {
    digest_with_bytes(value).map(|(digest, _)| digest)
}
// These are memory refusals, never reservations or physical capacity authority.
fn factory_credit(
    sources: &Vec<SourcePreparation>,
) -> Result<(Arc<HandshakeBudget>, HandshakeCredit, usize), WalPreparationError> {
    // The retained vector capacity is checked again by finish. At this prepass the
    // slice covers live elements and the fixed streaming hash/validation headers.
    let base = sources
        .capacity()
        .checked_mul(std::mem::size_of::<SourcePreparation>())
        .and_then(|n| n.checked_add(4096))
        .ok_or(WalPreparationError::Full)?;
    let base = sources
        .iter()
        .try_fold(base, |n, s| n.checked_add(s.input.retained_bytes()?))
        .ok_or(WalPreparationError::Full)?;
    let budget = HandshakeBudget::new();
    let mut credit = budget.credit();
    credit.resize(base).map_err(|_| WalPreparationError::Full)?;
    Ok((budget, credit, base))
}
fn factory_credit_one(
    source: &SourcePreparation,
) -> Result<(Arc<HandshakeBudget>, HandshakeCredit, usize), WalPreparationError> {
    let base = source
        .input
        .retained_bytes()
        .and_then(|n| n.checked_add(std::mem::size_of::<SourcePreparation>() + 4096))
        .ok_or(WalPreparationError::Full)?;
    let budget = HandshakeBudget::new();
    let mut credit = budget.credit();
    credit.resize(base).map_err(|_| WalPreparationError::Full)?;
    Ok((budget, credit, base))
}
fn topology_staging(
    args: &UniversalCommandArgs,
    participants: &ParticipantSet,
    encoded: usize,
) -> Result<usize, WalPreparationError> {
    use super::super::{
        history_commands::HistoryAction, selection_output_commands::SelectionOutputCommandArgs,
    };
    // At most twenty units and forty local/chain edges per original action.
    // Folder history expands by its actual relocation count. Global cross products
    // are bounded independently by the complete descriptor inventory. Threefold
    // backing covers Vec growth/old backing and stable sort; validation adds three
    // index vectors. Four canonical byte lengths cover cloned source/fence strings,
    // topology vectors and bounded DTO validation encodings, before any allocation.
    let actions = match args {
        UniversalCommandArgs::DecodedExportBatch(command) => command.outputs().len(),
        UniversalCommandArgs::History(command) => command
            .actions()
            .iter()
            .try_fold(0usize, |n, action| {
                n.checked_add(match action {
                    HistoryAction::FolderMove(folder) => folder.moves().len(),
                    _ => 1,
                })
            })
            .ok_or(WalPreparationError::Full)?,
        UniversalCommandArgs::SelectionOutput(SelectionOutputCommandArgs::ExtractAndTrim(_)) => 2,
        _ => 1,
    };
    let descriptors = participants.descriptors().len();
    let units = actions
        .checked_mul(20)
        .and_then(|n| n.checked_add(descriptors))
        .ok_or(WalPreparationError::Full)?;
    let edges = actions
        .checked_mul(40)
        .and_then(|n| n.checked_add(descriptors.checked_mul(descriptors)?))
        .and_then(|n| n.checked_add(descriptors.checked_mul(4)?))
        .ok_or(WalPreparationError::Full)?;
    let units_bytes = units
        .checked_mul(
            3 * std::mem::size_of::<super::super::WorkUnit>() + 3 * std::mem::size_of::<usize>(),
        )
        .ok_or(WalPreparationError::Full)?;
    let edges_bytes = edges
        .checked_mul(3 * std::mem::size_of::<super::super::WorkPrerequisite>())
        .ok_or(WalPreparationError::Full)?;
    encoded
        .checked_mul(4)
        .and_then(|n| n.checked_add(units_bytes))
        .and_then(|n| n.checked_add(edges_bytes))
        .and_then(|n| n.checked_add(4096))
        .ok_or(WalPreparationError::Full)
}
fn claims_for_source(
    source: &SourcePreparation,
    claims: &AdmissionClaimInventory,
    plan: &FullCapacityPlan,
) -> Result<ClaimStorage, WalPreparationError> {
    let main = claims
        .claims()
        .iter()
        .find(|c| c.claim_id() == source.database_claim)
        .ok_or(WalPreparationError::InvalidInput)?;
    let side = claims
        .claims()
        .iter()
        .find(|c| c.claim_id() == source.sidecar_claim)
        .ok_or(WalPreparationError::InvalidInput)?;
    if main.allocation_class() != Class::SourceDatabaseCommit
        || side.allocation_class() != Class::SourceWalShm
        || main.volume() != side.volume()
        || main.claim_id() == side.claim_id()
    {
        return Err(WalPreparationError::InvalidInput);
    }
    let allocation = |claim: &super::super::AdmissionClaim| {
        plan.allocations()
            .iter()
            .find(|a| a.volume() == claim.volume() && a.class() == claim.allocation_class())
            .ok_or(WalPreparationError::InvalidInput)
    };
    let main_unit = allocation(main)?.allocation_unit();
    if allocation(side)?.allocation_unit() != main_unit {
        return Err(WalPreparationError::InvalidInput);
    }
    Ok(ClaimStorage {
        device: main.volume().device,
        unit: main_unit,
    })
}
impl WalPreparationInput {
    pub(in crate::operation_journal) fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Derive exact preparation coverage for one validated universal command. Original source
    /// observations and limited requesters remain inputs; aggregate claims are not DB permits.
    pub fn from_universal(
        args: &UniversalCommandArgs,
        participants: &ParticipantSet,
        plan: &FullCapacityPlan,
        claims: &AdmissionClaimInventory,
        mut sources: Vec<SourcePreparation>,
        global: Option<GlobalWalPreparationClient>,
        deadline: Instant,
    ) -> Result<Self, WalPreparationError> {
        let bad = || WalPreparationError::InvalidInput;
        if sources
            .len()
            .checked_add(usize::from(global.is_some()))
            .is_none_or(|n| n > 64)
        {
            return Err(bad());
        }
        let now = Instant::now();
        if deadline <= now || deadline.duration_since(now) > Duration::from_secs(2) {
            return Err(WalPreparationError::Deadline);
        }
        let (budget, mut credit, base) = factory_credit(&sources)?;
        // Layout's existing Serialize validates/encodes its bounded binary generation
        // and base64 wire value. Fund that actual scratch before invoking Serialize;
        // a large original generation can legitimately exceed the combined ceiling.
        if let UniversalCommandArgs::SourceDerivedLayout(layout) = args {
            let scratch = layout
                .generation_bytes()
                .len()
                .checked_add(8 * 1024)
                .and_then(|n| n.checked_mul(4))
                .ok_or(WalPreparationError::Full)?;
            credit
                .resize(base.checked_add(scratch).ok_or(WalPreparationError::Full)?)
                .map_err(|_| WalPreparationError::Full)?;
        }
        let (digest, encoded) = digest_with_bytes(&(args, participants, plan, claims))?;
        let stage = topology_staging(args, participants, encoded)?;
        credit
            .resize(base.checked_add(stage).ok_or(WalPreparationError::Full)?)
            .map_err(|_| WalPreparationError::Full)?;
        participants.validate_for_new_intent().map_err(|_| bad())?;
        args.validate_against_participants(participants)
            .map_err(|_| bad())?;
        let work =
            super::super::work_plan::derive_work_plan(args, participants).map_err(|_| bad())?;
        claims.validate_against(plan).map_err(|_| bad())?;
        let descriptors = participants.descriptors();
        let mut expected_sources = 0;
        for descriptor in descriptors {
            match descriptor.kind() {
                ParticipantKind::SourceDatabase | ParticipantKind::SourceLeaseMaintenance => {
                    expected_sources += 1;
                    if !work
                        .units()
                        .iter()
                        .any(|unit| unit.key().participant_id() == descriptor.id())
                    {
                        return Err(bad());
                    }
                    let source = sources
                        .iter_mut()
                        .find(|source| source.participant == descriptor.id())
                        .ok_or_else(bad)?;
                    source.database_storage = Some(claims_for_source(source, claims, plan)?);
                    match (
                        &source.input,
                        descriptor.source_fence(),
                        descriptor.lease_fence(),
                    ) {
                        (SourceInput::Audited(revision), Some(fence), None)
                            if !matches!(args, UniversalCommandArgs::SourceDerivedLayout(_)) =>
                        {
                            if revision.source_id() != fence.source_id()
                                || revision.database_identity() != fence.database_identity()
                                || revision.root_identity().as_bytes()
                                    != fence.root_identity().as_bytes()
                                || revision.lifecycle_generation() != fence.lifecycle_generation()
                                || revision.revision() != fence.source_revision()
                                || revision.lease_epoch() != fence.lease_epoch()
                            {
                                return Err(bad());
                            }
                        }
                        (SourceInput::Layout(budget), Some(_), None) => {
                            let UniversalCommandArgs::SourceDerivedLayout(layout) = args else {
                                return Err(bad());
                            };
                            // V2 fences use the existing canonical layout root/database hashes.
                            // Args validation checked those hashes. Compare the actual budget
                            // against its complete raw scope rather than coercing those domains.
                            let revision = budget.revision();
                            let scope = layout.scope();
                            if revision.source_id().as_str() != scope.source_id()
                                || revision.root_identity().as_bytes() != scope.root_identity()
                                || revision.root_binding_identity() != scope.root_binding_identity()
                                || revision.database_identity() != scope.database_identity()
                                || revision.lifecycle_generation() != scope.lifecycle_generation()
                                || revision.lease_epoch() != scope.lease_epoch()
                                || revision.revision() != scope.revision()
                            {
                                return Err(bad());
                            }
                            source.profile_acquisition = Some(scope.profile_acquisition_identity());
                        }
                        (SourceInput::Lease(input), None, Some(fence))
                            if matches!(
                                args,
                                UniversalCommandArgs::SourceRegistryRetirement(_)
                            ) =>
                        {
                            if !fence.matches_allocation(input.subject(), input.allocation()) {
                                return Err(bad());
                            }
                        }
                        _ => return Err(bad()),
                    }
                }
                ParticipantKind::LegacySource => return Err(bad()),
                _ => {}
            }
        }
        if sources.len() != expected_sources || sources.is_empty() {
            return Err(bad());
        }
        // In-place unstable sort allocates no scratch. Pop yields ascending physical order.
        sources
            .sort_unstable_by(|a, b| b.input.database_identity().cmp(a.input.database_identity()));
        if sources.windows(2).any(|pair| {
            pair[0].input.database_identity() == pair[1].input.database_identity()
                || pair[0].participant == pair[1].participant
        }) {
            return Err(bad());
        }
        let needs_global = work.units().iter().any(|u| {
            matches!(
                u.key().role(),
                super::super::WorkUnitRole::GlobalLibrary | super::super::WorkUnitRole::Harvest
            )
        });
        if needs_global != global.is_some() {
            return Err(bad());
        }
        if needs_global
            && [Class::GlobalDatabaseCommit, Class::GlobalWalShm]
                .iter()
                .any(|class| {
                    !plan.allocations().iter().any(|allocation| {
                        allocation.class() == *class && allocation.allocated_bytes() > 0
                    })
                })
        {
            return Err(bad());
        }
        let global_storage = if needs_global {
            let main = plan
                .allocations()
                .iter()
                .find(|a| a.class() == Class::GlobalDatabaseCommit)
                .ok_or_else(bad)?;
            let storage = ClaimStorage {
                device: main.volume().device,
                unit: main.allocation_unit(),
            };
            if plan
                .allocations()
                .iter()
                .filter(|a| matches!(a.class(), Class::GlobalDatabaseCommit | Class::GlobalWalShm))
                .any(|a| a.volume().device != storage.device || a.allocation_unit() != storage.unit)
            {
                return Err(bad());
            }
            Some(storage)
        } else {
            None
        };
        // Build the fixed typed policy while the complete original topology/DTO scratch
        // is funded. finish releases that staging; afterward transfer no variable backing.
        let retirement = if let UniversalCommandArgs::SourceRegistryRetirement(args) = args {
            // The same genuine allocation moves through the original funded Source FIFO.
            // Attach the complete frozen RELEASE family before releasing DTO scratch.
            for source in &mut sources {
                let SourceInput::Lease(input) = &mut source.input else {
                    return Err(bad());
                };
                input.bind_retirement_policy(args).map_err(|_| bad())?;
            }
            Some(
                GlobalPreparationRequest::retirement_policy_input(args)
                    .map_err(WalPreparationError::Global)?,
            )
        } else {
            None
        };
        drop(work);
        let mut input = Self::finish(
            sources,
            global,
            digest,
            deadline,
            matches!(args, UniversalCommandArgs::SourceRegistryRetirement(_)),
            Some((budget, credit)),
        )?;
        if let Some(retirement) = retirement {
            let (client, request) = input.global.take().ok_or_else(bad)?;
            input.global = Some((client, request.with_retirement_policy(retirement)));
        }
        input.global_storage = global_storage;
        Ok(input)
    }
    /// Prepare one source-only metadata assignment scope from its complete genuine original
    /// observation and frozen DTO. Dictionary/global finalization is a separate parent scope.
    pub fn from_tag(
        writer: &crate::sample_sources::SourceWriter,
        args: &super::super::metadata_tag_commands::MetadataTagSourceIntentArgs,
        plan: &FullCapacityPlan,
        claims: &AdmissionClaimInventory,
        mut source: SourcePreparation,
        deadline: Instant,
    ) -> Result<Self, WalPreparationError> {
        let SourceInput::Tag(_) = &source.input else {
            return Err(WalPreparationError::InvalidInput);
        };
        let (budget, credit, _) = factory_credit_one(&source)?;
        claims
            .validate_against(plan)
            .map_err(|_| WalPreparationError::InvalidInput)?;
        source.database_storage = Some(claims_for_source(&source, claims, plan)?);
        if source.participant != args.participant_id() {
            return Err(WalPreparationError::InvalidInput);
        }
        let (digest, encoded) = digest_with_bytes(&("source-tag", args, plan, claims))?;
        let input = Self::finish(
            vec![source],
            None,
            digest,
            deadline,
            false,
            Some((budget, credit)),
        )?;
        // Full passive owner validation also shares this same ceiling, before its SQL
        // metadata decoding/subject temporaries allocate. It issues no readiness outcome.
        let mut staging = input.budget.credit();
        staging
            .resize(
                crate::sample_sources::wal_transport::VALIDATION_STAGING_BYTES
                    .max(encoded.checked_mul(2).ok_or(WalPreparationError::Full)?),
            )
            .map_err(|_| WalPreparationError::Full)?;
        let SourceInput::Tag(observation) = &input.sources[0].input else {
            unreachable!()
        };
        args.matches_original_observation(writer, observation)
            .map_err(|_| WalPreparationError::InvalidInput)?;
        drop(staging);
        Ok(input)
    }
    /// Prepare an explicit original lease scope without inventing an authoritative audit or
    /// treating its stored registration phase as current journal acknowledgement.
    pub fn from_lease_scope(
        participants: &ParticipantSet,
        plan: &FullCapacityPlan,
        claims: &AdmissionClaimInventory,
        mut source: SourcePreparation,
        deadline: Instant,
    ) -> Result<Self, WalPreparationError> {
        if participants.descriptors().len() != 1 {
            return Err(WalPreparationError::InvalidInput);
        }
        // Stored lease-fence validation encodes its original bounded descriptor.
        // Fund that allocation and the future single-element vector before either.
        let (budget, mut credit, base) = factory_credit_one(&source)?;
        credit
            .resize(
                base.checked_add(
                    3 * super::super::source_lease_binding::MAX_SOURCE_LEASE_SCOPE_BYTES,
                )
                .ok_or(WalPreparationError::Full)?,
            )
            .map_err(|_| WalPreparationError::Full)?;
        participants
            .validate_for_new_intent()
            .map_err(|_| WalPreparationError::InvalidInput)?;
        claims
            .validate_against(plan)
            .map_err(|_| WalPreparationError::InvalidInput)?;
        source.database_storage = Some(claims_for_source(&source, claims, plan)?);
        if participants.descriptors().len() != 1 {
            return Err(WalPreparationError::InvalidInput);
        }
        let descriptor = &participants.descriptors()[0];
        let SourceInput::Lease(input) = &source.input else {
            return Err(WalPreparationError::InvalidInput);
        };
        if descriptor.kind() != ParticipantKind::SourceLeaseMaintenance
            || descriptor.id() != source.participant
            || descriptor
                .lease_fence()
                .is_none_or(|fence| !fence.matches_allocation(input.subject(), input.allocation()))
        {
            return Err(WalPreparationError::InvalidInput);
        }
        let digest = digest(&("source-lease", participants, plan, claims))?;
        Self::finish(
            vec![source],
            None,
            digest,
            deadline,
            false,
            Some((budget, credit)),
        )
    }
    fn finish(
        sources: Vec<SourcePreparation>,
        global: Option<GlobalWalPreparationClient>,
        digest: [u8; 32],
        deadline: Instant,
        lifecycle: bool,
        funded: Option<(Arc<HandshakeBudget>, HandshakeCredit)>,
    ) -> Result<Self, WalPreparationError> {
        let deadline = sources
            .iter()
            .filter_map(|source| source.input.deadline())
            .fold(deadline, Instant::min);
        let now = Instant::now();
        if deadline <= now || deadline.duration_since(now) > Duration::from_secs(2) {
            return Err(WalPreparationError::Deadline);
        }
        if sources.len() + usize::from(global.is_some()) > 64 {
            return Err(WalPreparationError::InvalidInput);
        }
        let (budget, mut credit) = funded.unwrap_or_else(|| {
            let budget = HandshakeBudget::new();
            let credit = budget.credit();
            (budget, credit)
        });
        // Move the original caller vector, with its entire actual capacity. No option
        // vector, stable-sort scratch or second backing is allocated during transfer.
        let header_bytes = std::mem::size_of::<Self>()
            .checked_add(2048)
            .and_then(|n| {
                n.checked_add(
                    sources
                        .capacity()
                        .checked_mul(std::mem::size_of::<SourcePreparation>())?,
                )
            })
            .and_then(|n| {
                n.checked_add(
                    (sources.len() + usize::from(global.is_some()))
                        .checked_mul(std::mem::size_of::<super::collector::HeldOwner>())?,
                )
            })
            .ok_or(WalPreparationError::Full)?;
        let bytes = sources
            .iter()
            .try_fold(header_bytes, |n, source| {
                n.checked_add(source.input.retained_bytes()?)
            })
            .ok_or(WalPreparationError::Full)?;
        credit
            .resize(bytes)
            .map_err(|_| WalPreparationError::Full)?;
        let global = global
            .map(|client| {
                let mut request = GlobalPreparationRequest::new(
                    client.preparation_owner(),
                    digest,
                    deadline,
                    sources.len() + 1,
                    bytes,
                )
                .map_err(WalPreparationError::Global)?
                .with_wal_binding();
                if lifecycle {
                    request = request.with_lifecycle_policy();
                }
                Ok((client, request.in_handshake(Arc::clone(&budget))))
            })
            .transpose()?;
        Ok(Self {
            sources,
            global,
            digest,
            global_storage: None,
            deadline,
            budget,
            credit,
            header_bytes,
        })
    }
    #[cfg(all(test, unix))]
    pub(in crate::operation_journal) fn replace_source_retirement_policy_for_test(
        &mut self,
        args: Option<&super::super::source_registry_retirement::SourceRegistryRetirementIntentArgs>,
    ) {
        assert_eq!(self.sources.len(), 1);
        let SourceInput::Lease(input) = &mut self.sources[0].input else {
            panic!("original retirement requires lease input");
        };
        input.replace_retirement_policy_for_test(args).unwrap();
    }

    #[cfg(test)]
    pub(in crate::operation_journal) fn budget_for_test(&self) -> Arc<HandshakeBudget> {
        Arc::clone(&self.budget)
    }
    pub(in crate::operation_journal) fn retained_bytes(&self) -> Option<usize> {
        self.sources
            .iter()
            .try_fold(self.header_bytes, |n, source| {
                n.checked_add(source.input.retained_bytes()?)
            })
    }
}
