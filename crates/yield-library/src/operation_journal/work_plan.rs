//! Pure, versioned derivation of the bounded work required by one universal intent.
//!
//! A work plan is an immutable description of the participant obligations and their ordering.
//! It is derived from the typed command arguments and the complete initial participant inventory;
//! callers cannot supply a sparse or reordered obligation list.  This module does not open a
//! participant, reserve capacity, publish a file, or advance a journal checkpoint.  A later
//! coordinator may use a validated plan as input to those owner-bound operations.
//!
//! V1 requires one required, unfenced `Filesystem` participant.  That participant is a logical
//! filesystem coordinator which may cover several roots or devices; sharing its ID is never
//! treated as physical or volume identity equality.  The existing implicit filesystem commands
//! have no durable per-root binding, so a multi-filesystem inventory is rejected until that
//! binding is captured by a later contract.  Explicit filesystem fields must name this same
//! coordinator.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::coordinator::UniversalCommandArgsValidationError;
use super::coordinator::{CopyEndpoint, UniversalCommandArgs};
use super::external_commands::ExternalReconciliationIntentArgs;
use super::history_commands::{HistoryAction, HistoryDirection, HistoryUndoRedoIntentArgs};
use super::namespace_commands::{NamespaceCommandArgs, NamespaceEntryEndpoint, NamespaceEntryKind};
use super::participants::{
    ParticipantDescriptor, ParticipantId, ParticipantKind, ParticipantRequirement, ParticipantSet,
    ParticipantValidationError, StoredSourceFence,
};
use super::removal_commands::RemovalCommandArgs;
use super::render_commands::RenderCommandArgs;
use super::retained_resolution_commands::RetainedResolutionCommandArgs;
use super::selection_output_commands::{ExtractAndTrimIntentArgs, SelectionOutputCommandArgs};
use crate::sample_sources::SourceId;

/// Version of the frozen work-plan derivation algorithm.
pub const WORK_PLAN_VERSION_V1: u16 = 1;
/// Alias for the current work-plan version.
pub const WORK_PLAN_VERSION: u16 = WORK_PLAN_VERSION_V1;
/// Maximum number of derived participant work units in one plan.
pub const MAX_WORK_PLAN_UNITS: usize = 2048;
/// Maximum number of prerequisite edges in one plan.
pub const MAX_WORK_PLAN_EDGES: usize = 8192;

pub(crate) const GLOBAL_ACTION_ORDINAL: u8 = u8::MAX;
const STEP_SOURCE_READ: u16 = 10;
const STEP_RECOVERY_MATERIAL: u16 = 15;
const STEP_PREPARE: u16 = 20;
const STEP_RESTORE_RECORD: u16 = 22;
const STEP_ENSURE_PARENTS: u16 = 25;
const STEP_STAGE: u16 = 30;
const STEP_DIRECT_REMOVAL_INTENT: u16 = 35;
const STEP_COPY_VALIDATED: u16 = 40;
const STEP_PUBLISH: u16 = 50;
const STEP_QUALIFIED_RELOCATION: u16 = 50;
const STEP_RETAINED_PAYLOAD: u16 = 55;
const STEP_RETAINED_ABSENCE: u16 = 58;
const STEP_SOURCE_AFFECT: u16 = 60;
const STEP_DESTINATION_RECONCILE: u16 = 70;
const STEP_ORIGIN_REMOVAL_START: u16 = 80;
const STEP_ORIGIN_ABSENCE: u16 = 90;
const STEP_ORIGIN_RECONCILE: u16 = 100;
const STEP_EXTERNAL_AUDIT: u16 = 110;
const STEP_RETAINED_MERGE: u16 = 35;
pub(crate) const STEP_HISTORY_STRIDE: u16 = 256;
const STEP_GLOBAL_LIBRARY: u16 = 10;
const STEP_HARVEST: u16 = 20;
const STEP_PROJECTION: u16 = 30;
const STEP_READINESS: u16 = 40;
const STEP_ARTIFACT: u16 = 50;

/// Semantic obligation represented by one derived work unit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum WorkUnitRole {
    /// Read an existing source-database observation.
    SourceRead,
    /// Apply or reconcile an affected source-database entry.
    SourceAffect,
    /// Establish the owner-bound preparation for a filesystem mutation.
    Prepare,
    /// Persist a restore record and concrete recovery locator before a removal relocation.
    RestoreRecordPrepared,
    /// Retain recovery material before a destructive or restore mutation begins.
    RecoveryMaterialRetained,
    /// Create and verify contained destination parents for a qualified relocation.
    EnsureContainedParentDirectories,
    /// Stage a new filesystem object before publication.
    Stage,
    /// Verify a staged copy before publication.
    CopyValidated,
    /// Publish the prepared filesystem result.
    Publish,
    /// Perform a qualified no-replace relocation after contained-parent checks.
    QualifiedRelocation,
    /// Verify that a moved entry is retained before reconciling its source absence.
    RetainedPayloadVerified,
    /// Apply the selected retained-object disposition for a purge request.
    SelectedRetainedObjectDisposition,
    /// Verify that the selected retained object is absent after its disposition.
    RetainedObjectAbsence,
    /// Reconcile the destination source after a cross-source publication.
    DestinationSourceReconcile,
    /// Begin durable removal intent before a direct retention relocation, or after a
    /// cross-source destination checkpoint.
    OriginRemovalStart,
    /// Verify that the origin is absent after its removal begins.
    OriginAbsence,
    /// Reconcile the origin source after absence is verified.
    OriginSourceReconcile,
    /// Apply a bounded prepared manifest for a retained-folder merge.
    RetainedMerge,
    /// Reconcile the profile-global library.
    GlobalLibrary,
    /// Reconcile metadata harvest coverage.
    Harvest,
    /// Update the source projection.
    Projection,
    /// Update source readiness after projection.
    Readiness,
    /// Rebuild or verify one declared artifact.
    Artifact,
    /// Audit an external filesystem observation before source reconciliation.
    ExternalAudit,
}

/// Applicability derived by the planner for a work unit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum WorkUnitApplicability {
    /// The owner must establish and complete this obligation.
    Required,
    /// The command's fixed primitive does not use this staged-copy step.
    NotApplicable,
}

/// Stable key for one derived work unit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkUnitKey {
    action_ordinal: u8,
    step_ordinal: u16,
    participant_id: ParticipantId,
    role: WorkUnitRole,
}

impl WorkUnitKey {
    /// Construct one work-unit key.
    pub const fn new(
        action_ordinal: u8,
        step_ordinal: u16,
        participant_id: ParticipantId,
        role: WorkUnitRole,
    ) -> Self {
        Self {
            action_ordinal,
            step_ordinal,
            participant_id,
            role,
        }
    }

    /// Return the action ordinal represented by this key.
    pub const fn action_ordinal(self) -> u8 {
        self.action_ordinal
    }

    /// Return the deterministic step ordinal represented by this key.
    pub const fn step_ordinal(self) -> u16 {
        self.step_ordinal
    }

    /// Return the participant to which this unit belongs.
    pub const fn participant_id(self) -> ParticipantId {
        self.participant_id
    }

    /// Return the semantic role of this unit.
    pub const fn role(self) -> WorkUnitRole {
        self.role
    }
}

/// One derived obligation, including its participant durability and applicability.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkUnit {
    key: WorkUnitKey,
    requirement: ParticipantRequirement,
    applicability: WorkUnitApplicability,
}

impl WorkUnit {
    /// Return this unit's stable key.
    pub const fn key(&self) -> WorkUnitKey {
        self.key
    }

    /// Return the durability requirement inherited from its participant descriptor.
    pub const fn requirement(&self) -> ParticipantRequirement {
        self.requirement
    }

    /// Return the planner-derived applicability.
    pub const fn applicability(&self) -> WorkUnitApplicability {
        self.applicability
    }

    /// Return whether this unit is applicable to the current command.
    pub const fn is_applicable(&self) -> bool {
        matches!(self.applicability, WorkUnitApplicability::Required)
    }
}

/// One directed prerequisite edge between two derived work units.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkPrerequisite {
    prerequisite: WorkUnitKey,
    dependent: WorkUnitKey,
}

impl WorkPrerequisite {
    /// Construct one prerequisite edge.
    pub const fn new(prerequisite: WorkUnitKey, dependent: WorkUnitKey) -> Self {
        Self {
            prerequisite,
            dependent,
        }
    }

    /// Return the prerequisite unit.
    pub const fn prerequisite(self) -> WorkUnitKey {
        self.prerequisite
    }

    /// Return the dependent unit.
    pub const fn dependent(self) -> WorkUnitKey {
        self.dependent
    }
}

/// Complete frozen work-plan output for one universal command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkPlan {
    plan_version: u16,
    units: Vec<WorkUnit>,
    prerequisites: Vec<WorkPrerequisite>,
}

impl WorkPlan {
    /// Return the frozen derivation version.
    pub const fn plan_version(&self) -> u16 {
        self.plan_version
    }

    /// Borrow all deterministically sorted work units.
    pub fn units(&self) -> &[WorkUnit] {
        &self.units
    }

    /// Borrow all deterministically sorted prerequisite edges.
    pub fn prerequisites(&self) -> &[WorkPrerequisite] {
        &self.prerequisites
    }

    /// Alias for callers that refer to prerequisites as graph edges.
    pub fn edges(&self) -> &[WorkPrerequisite] {
        self.prerequisites()
    }

    /// Alias for callers that name edges explicitly.
    pub fn prerequisite_edges(&self) -> &[WorkPrerequisite] {
        self.prerequisites()
    }

    /// Validate the bounded, sorted, self-contained shape of a decoded plan.
    pub fn validate(&self) -> Result<(), WorkPlanError> {
        if self.plan_version != WORK_PLAN_VERSION_V1 {
            return Err(WorkPlanError::UnsupportedPlanVersion {
                actual: self.plan_version,
            });
        }
        if self.units.len() > MAX_WORK_PLAN_UNITS {
            return Err(WorkPlanError::TooManyUnits {
                actual: self.units.len(),
                maximum: MAX_WORK_PLAN_UNITS,
            });
        }
        if self.prerequisites.len() > MAX_WORK_PLAN_EDGES {
            return Err(WorkPlanError::TooManyEdges {
                actual: self.prerequisites.len(),
                maximum: MAX_WORK_PLAN_EDGES,
            });
        }
        if self.units.is_empty() {
            return Err(WorkPlanError::EmptyPlan);
        }

        for (index, unit) in self.units.iter().enumerate() {
            if index > 0 && self.units[index - 1].key >= unit.key {
                return Err(WorkPlanError::UnitsNotSortedOrUnique);
            }
            if unit.requirement == ParticipantRequirement::OptionalRebuildable
                && unit.key.role != WorkUnitRole::Artifact
            {
                return Err(WorkPlanError::OptionalNonArtifactUnit { key: unit.key });
            }
            if unit.applicability == WorkUnitApplicability::NotApplicable
                && !matches!(
                    unit.key.role,
                    WorkUnitRole::Stage | WorkUnitRole::CopyValidated
                )
            {
                return Err(WorkPlanError::InvalidNotApplicableRole { key: unit.key });
            }
        }

        for (index, edge) in self.prerequisites.iter().enumerate() {
            if index > 0 && self.prerequisites[index - 1] >= *edge {
                return Err(WorkPlanError::EdgesNotSortedOrUnique);
            }
            if edge.prerequisite == edge.dependent {
                return Err(WorkPlanError::SelfPrerequisite {
                    key: edge.prerequisite,
                });
            }
            let prerequisite = self.units.iter().find(|unit| unit.key == edge.prerequisite);
            let dependent = self.units.iter().find(|unit| unit.key == edge.dependent);
            if prerequisite.is_none() || dependent.is_none() {
                return Err(WorkPlanError::EdgeEndpointMissing { edge: *edge });
            }
            if prerequisite
                .is_some_and(|unit| unit.applicability == WorkUnitApplicability::NotApplicable)
                || dependent
                    .is_some_and(|unit| unit.applicability == WorkUnitApplicability::NotApplicable)
            {
                return Err(WorkPlanError::EdgeUsesNotApplicableUnit { edge: *edge });
            }
        }

        // A prerequisite graph with a cycle cannot make progress.  The vectors are already
        // bounded above, so this small index-based Kahn pass remains allocation-bounded and does
        // not need to retain any path or endpoint text.
        let mut indegrees = vec![0usize; self.units.len()];
        for edge in &self.prerequisites {
            let dependent_index = self
                .units
                .iter()
                .position(|unit| unit.key == edge.dependent)
                .ok_or(WorkPlanError::EdgeEndpointMissing { edge: *edge })?;
            indegrees[dependent_index] =
                indegrees[dependent_index]
                    .checked_add(1)
                    .ok_or(WorkPlanError::TooManyEdges {
                        actual: self.prerequisites.len(),
                        maximum: MAX_WORK_PLAN_EDGES,
                    })?;
        }
        let mut ready = self
            .units
            .iter()
            .enumerate()
            .filter_map(|(index, _)| (indegrees[index] == 0).then_some(index))
            .collect::<Vec<_>>();
        let mut visited = 0usize;
        while let Some(index) = ready.pop() {
            visited = visited.saturating_add(1);
            let key = self.units[index].key;
            for edge in &self.prerequisites {
                if edge.prerequisite == key {
                    let dependent_index = self
                        .units
                        .iter()
                        .position(|unit| unit.key == edge.dependent)
                        .ok_or(WorkPlanError::EdgeEndpointMissing { edge: *edge })?;
                    indegrees[dependent_index] = indegrees[dependent_index]
                        .checked_sub(1)
                        .ok_or(WorkPlanError::PrerequisiteCycle)?;
                    if indegrees[dependent_index] == 0 {
                        ready.push(dependent_index);
                    }
                }
            }
        }
        if visited != self.units.len() {
            return Err(WorkPlanError::PrerequisiteCycle);
        }
        Ok(())
    }

    /// Re-derive and compare this plan using the frozen V1 planner.
    pub fn validate_against(
        &self,
        args: &UniversalCommandArgs,
        participants: &ParticipantSet,
    ) -> Result<(), WorkPlanError> {
        validate_work_plan(self, args, participants)
    }
}

/// Derive a complete plan using the current frozen planner version.
pub fn derive_work_plan(
    args: &UniversalCommandArgs,
    participants: &ParticipantSet,
) -> Result<WorkPlan, WorkPlanError> {
    derive_work_plan_v1(args, participants)
}

/// Derive a complete plan with the frozen V1 algorithm.
pub fn derive_work_plan_v1(
    args: &UniversalCommandArgs,
    participants: &ParticipantSet,
) -> Result<WorkPlan, WorkPlanError> {
    participants.validate_for_new_intent()?;
    args.validate_against_participants(participants)
        .map_err(WorkPlanError::CommandArguments)?;

    let mut collected = CollectedTopology::default();
    collect_topology(args, &mut collected)?;
    let topology = Topology::resolve(participants, collected)?;

    let mut builder = PlanBuilder::default();
    let mut action_spans = Vec::new();
    derive_actions(args, &topology, &mut builder, &mut action_spans)?;
    add_global_obligations(&topology, &mut builder, &action_spans)?;
    builder.finish()
}

/// Validate a plan by checking its shape and comparing exact re-derived units and edges.
pub fn validate_work_plan(
    plan: &WorkPlan,
    args: &UniversalCommandArgs,
    participants: &ParticipantSet,
) -> Result<(), WorkPlanError> {
    plan.validate()?;
    let expected = derive_work_plan_v1(args, participants)?;
    if plan != &expected {
        return Err(WorkPlanError::PlanMismatch);
    }
    Ok(())
}

/// Why a pure work-plan derivation or validation failed.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum WorkPlanError {
    /// The initial participant inventory was malformed or not pending.
    #[error("invalid work-plan participant inventory: {0}")]
    Participants(#[from] ParticipantValidationError),
    /// The command arguments did not pass their existing typed validator.
    #[error("invalid work-plan command arguments: {0}")]
    CommandArguments(#[source] UniversalCommandArgsValidationError),
    /// A required topology element was absent.
    #[error("work-plan topology is incomplete: {0}")]
    IncompleteTopology(&'static str),
    /// Two stored bindings would require the planner to guess their relationship.
    #[error("work-plan topology is ambiguous: {0}")]
    AmbiguousTopology(&'static str),
    /// A participant descriptor was not referenced by the command topology.
    #[error("work-plan participant {participant_id} is unused")]
    UnusedParticipant {
        /// The unreferenced participant identity.
        participant_id: ParticipantId,
    },
    /// A source fence did not match the endpoint or its derived source binding.
    #[error("work-plan source fence does not match the affected source")]
    SourceFenceMismatch,
    /// A source participant was used for two distinct source identities.
    #[error("work-plan source participant binding is inconsistent")]
    SourceBindingMismatch,
    /// The planner exceeded its unit bound.
    #[error("work-plan has {actual} units, maximum is {maximum}")]
    TooManyUnits {
        /// Number of units requested.
        actual: usize,
        /// Maximum units permitted by the frozen planner.
        maximum: usize,
    },
    /// The planner exceeded its prerequisite-edge bound.
    #[error("work-plan has {actual} prerequisite edges, maximum is {maximum}")]
    TooManyEdges {
        /// Number of edges requested.
        actual: usize,
        /// Maximum edges permitted by the frozen planner.
        maximum: usize,
    },
    /// A checked step ordinal calculation overflowed.
    #[error("work-plan step ordinal overflowed")]
    StepOrdinalOverflow,
    /// A unit key was inserted twice.
    #[error("work-plan unit key was duplicated: {key:?}")]
    DuplicateUnit {
        /// The duplicate unit key.
        key: WorkUnitKey,
    },
    /// An edge was requested with an unknown endpoint.
    #[error("work-plan edge endpoint is missing: {edge:?}")]
    EdgeEndpointMissing {
        /// The edge containing the unknown endpoint.
        edge: WorkPrerequisite,
    },
    /// An edge would point at itself.
    #[error("work-plan unit cannot depend on itself: {key:?}")]
    SelfPrerequisite {
        /// The self-dependent unit key.
        key: WorkUnitKey,
    },
    /// A decoded plan's units were not sorted strictly by key.
    #[error("work-plan units are not sorted uniquely")]
    UnitsNotSortedOrUnique,
    /// A decoded plan's edges were not sorted strictly by endpoints.
    #[error("work-plan prerequisite edges are not sorted uniquely")]
    EdgesNotSortedOrUnique,
    /// The plan contained no obligations.
    #[error("work-plan must contain at least one obligation")]
    EmptyPlan,
    /// The prerequisite graph contains a cycle.
    #[error("work-plan prerequisite graph contains a cycle")]
    PrerequisiteCycle,
    /// An edge referenced a fixed not-applicable unit.
    #[error("work-plan edge references a not-applicable unit: {edge:?}")]
    EdgeUsesNotApplicableUnit {
        /// The invalid edge.
        edge: WorkPrerequisite,
    },
    /// Optional durability was assigned to a non-artifact unit.
    #[error("work-plan optional durability is only valid for artifacts: {key:?}")]
    OptionalNonArtifactUnit {
        /// The unit carrying optional durability.
        key: WorkUnitKey,
    },
    /// A role other than Stage or CopyValidated was marked not applicable.
    #[error("work-plan not-applicable role is invalid: {key:?}")]
    InvalidNotApplicableRole {
        /// The unit with the invalid applicability.
        key: WorkUnitKey,
    },
    /// A decoded plan used a derivation version this binary does not understand.
    #[error("unsupported work-plan version {actual}")]
    UnsupportedPlanVersion {
        /// The unsupported version number.
        actual: u16,
    },
    /// The supplied plan differed from the exact frozen re-derivation.
    #[error("work-plan does not match command arguments and participants")]
    PlanMismatch,
}

#[derive(Clone, Debug)]
struct SourceBinding {
    participant_id: ParticipantId,
    source_id: SourceId,
    reads: bool,
    affects: bool,
}

#[derive(Default)]
struct CollectedTopology {
    sources: Vec<SourceBinding>,
    filesystem_hint: Option<ParticipantId>,
}

impl CollectedTopology {
    fn source(
        &mut self,
        participant_id: ParticipantId,
        source_id: &SourceId,
        reads: bool,
        affects: bool,
    ) -> Result<(), WorkPlanError> {
        if let Some(existing) = self
            .sources
            .iter_mut()
            .find(|binding| binding.participant_id == participant_id)
        {
            if existing.source_id != *source_id {
                return Err(WorkPlanError::SourceBindingMismatch);
            }
            existing.reads |= reads;
            existing.affects |= affects;
            return Ok(());
        }
        if self
            .sources
            .iter()
            .any(|binding| binding.source_id == *source_id)
        {
            return Err(WorkPlanError::AmbiguousTopology(
                "one source identity is bound through multiple participants",
            ));
        }
        self.sources.push(SourceBinding {
            participant_id,
            source_id: source_id.clone(),
            reads,
            affects,
        });
        Ok(())
    }

    fn filesystem(&mut self, participant_id: ParticipantId) -> Result<(), WorkPlanError> {
        match self.filesystem_hint {
            Some(existing) if existing != participant_id => Err(WorkPlanError::AmbiguousTopology(
                "history actions bind different filesystem participants",
            )),
            Some(_) => Ok(()),
            None => {
                self.filesystem_hint = Some(participant_id);
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug)]
struct ArtifactBinding {
    participant_id: ParticipantId,
    requirement: ParticipantRequirement,
}

#[derive(Clone, Debug)]
struct Topology {
    sources: Vec<SourceBinding>,
    filesystem: ParticipantId,
    global_library: ParticipantId,
    harvest: Vec<ParticipantId>,
    projections: Vec<(ParticipantId, ParticipantId)>,
    readiness: Vec<(ParticipantId, ParticipantId)>,
    artifacts: Vec<ArtifactBinding>,
}

impl Topology {
    fn resolve(
        participants: &ParticipantSet,
        mut collected: CollectedTopology,
    ) -> Result<Self, WorkPlanError> {
        if collected.sources.is_empty() {
            return Err(WorkPlanError::IncompleteTopology(
                "at least one affected source is required",
            ));
        }
        if !collected.sources.iter().any(|binding| binding.affects) {
            return Err(WorkPlanError::IncompleteTopology(
                "at least one source-affecting binding is required",
            ));
        }
        collected
            .sources
            .sort_by_key(|binding| binding.participant_id);

        let global_descriptors = descriptors_of_kind(participants, ParticipantKind::GlobalLibrary);
        if global_descriptors.len() != 1 {
            return Err(if global_descriptors.is_empty() {
                WorkPlanError::IncompleteTopology("one GlobalLibrary participant is required")
            } else {
                WorkPlanError::AmbiguousTopology(
                    "exactly one GlobalLibrary participant is required",
                )
            });
        }
        let global = global_descriptors[0];
        if global.requirement() != ParticipantRequirement::Required {
            return Err(WorkPlanError::AmbiguousTopology(
                "GlobalLibrary must be required",
            ));
        }

        let filesystem_descriptors: Vec<_> = participants
            .descriptors()
            .iter()
            .filter(|descriptor| descriptor.kind() == ParticipantKind::Filesystem)
            .collect();
        if filesystem_descriptors.len() != 1 {
            return Err(if filesystem_descriptors.is_empty() {
                WorkPlanError::IncompleteTopology("one Filesystem participant is required")
            } else {
                WorkPlanError::AmbiguousTopology("exactly one Filesystem participant is required")
            });
        }
        let filesystem = filesystem_descriptors[0];
        if filesystem.requirement() != ParticipantRequirement::Required {
            return Err(WorkPlanError::IncompleteTopology(
                "Filesystem participant must be required",
            ));
        }
        if filesystem.source_fence().is_some() {
            return Err(WorkPlanError::AmbiguousTopology(
                "the global Filesystem coordinator must be unfenced",
            ));
        }
        if let Some(hint) = collected.filesystem_hint
            && hint != filesystem.id()
        {
            return Err(WorkPlanError::SourceBindingMismatch);
        }

        for binding in &collected.sources {
            let descriptor = participants
                .descriptors()
                .iter()
                .find(|descriptor| descriptor.id() == binding.participant_id)
                .ok_or(WorkPlanError::IncompleteTopology(
                    "affected source participant is missing",
                ))?;
            if descriptor.kind() != ParticipantKind::SourceDatabase
                || descriptor.requirement() != ParticipantRequirement::Required
            {
                return Err(WorkPlanError::SourceBindingMismatch);
            }
            let fence = descriptor
                .source_fence()
                .ok_or(WorkPlanError::SourceBindingMismatch)?;
            if fence.source_id() != &binding.source_id {
                return Err(WorkPlanError::SourceFenceMismatch);
            }
        }

        for descriptor in participants.descriptors() {
            match descriptor.kind() {
                ParticipantKind::SourceDatabase => {
                    if !collected
                        .sources
                        .iter()
                        .any(|binding| binding.participant_id == descriptor.id())
                    {
                        return Err(WorkPlanError::UnusedParticipant {
                            participant_id: descriptor.id(),
                        });
                    }
                }
                ParticipantKind::Filesystem | ParticipantKind::GlobalLibrary => {}
                ParticipantKind::Harvest
                | ParticipantKind::Projection
                | ParticipantKind::Readiness
                | ParticipantKind::Artifact => {}
            }
        }

        let source_fences: Vec<_> = collected
            .sources
            .iter()
            .map(|binding| {
                participants
                    .descriptors()
                    .iter()
                    .find(|descriptor| descriptor.id() == binding.participant_id)
                    .and_then(ParticipantDescriptor::source_fence)
                    .ok_or(WorkPlanError::SourceBindingMismatch)
            })
            .collect::<Result<_, _>>()?;

        let harvest_descriptors = descriptors_of_kind(participants, ParticipantKind::Harvest);
        if harvest_descriptors.is_empty() {
            return Err(WorkPlanError::IncompleteTopology(
                "Harvest coverage is required",
            ));
        }
        let any_fenced_harvest = harvest_descriptors
            .iter()
            .any(|descriptor| descriptor.source_fence().is_some());
        let all_fenced_harvest = harvest_descriptors
            .iter()
            .all(|descriptor| descriptor.source_fence().is_some());
        let mut harvest = harvest_descriptors
            .iter()
            .map(|descriptor| descriptor.id())
            .collect::<Vec<_>>();
        harvest.sort();
        if any_fenced_harvest != all_fenced_harvest {
            return Err(WorkPlanError::AmbiguousTopology(
                "Harvest coverage cannot mix global and source-fenced descriptors",
            ));
        }
        if !all_fenced_harvest {
            if harvest_descriptors.len() != 1 {
                return Err(WorkPlanError::AmbiguousTopology(
                    "global Harvest coverage must have exactly one descriptor",
                ));
            }
        } else {
            let affected_fences = collected
                .sources
                .iter()
                .enumerate()
                .filter(|(_, binding)| binding.affects)
                .map(|(source_index, _)| source_fences[source_index])
                .collect::<Vec<_>>();
            for expected_fence in &affected_fences {
                let matches = harvest_descriptors
                    .iter()
                    .filter(|descriptor| {
                        descriptor
                            .source_fence()
                            .is_some_and(|fence| fence == *expected_fence)
                    })
                    .count();
                if matches != 1 {
                    return Err(WorkPlanError::IncompleteTopology(
                        "source-fenced Harvest coverage must contain one descriptor per source",
                    ));
                }
            }
            if harvest_descriptors.iter().any(|descriptor| {
                descriptor
                    .source_fence()
                    .is_none_or(|fence| !affected_fences.iter().any(|expected| fence == *expected))
            }) {
                return Err(WorkPlanError::UnusedParticipant {
                    participant_id: harvest_descriptors
                        .iter()
                        .find(|descriptor| {
                            descriptor.source_fence().is_none_or(|fence| {
                                !affected_fences.iter().any(|expected| fence == *expected)
                            })
                        })
                        .map_or(harvest_descriptors[0].id(), |descriptor| descriptor.id()),
                });
            }
        }

        let projections = resolve_fenced_role(
            participants,
            ParticipantKind::Projection,
            &collected.sources,
            &source_fences,
        )?;
        let readiness = resolve_fenced_role(
            participants,
            ParticipantKind::Readiness,
            &collected.sources,
            &source_fences,
        )?;

        let mut artifacts = Vec::new();
        for descriptor in descriptors_of_kind(participants, ParticipantKind::Artifact) {
            let fence = descriptor
                .source_fence()
                .ok_or(WorkPlanError::SourceBindingMismatch)?;
            if !collected
                .sources
                .iter()
                .any(|binding| fence.source_id() == &binding.source_id)
            {
                return Err(WorkPlanError::UnusedParticipant {
                    participant_id: descriptor.id(),
                });
            }
            artifacts.push(ArtifactBinding {
                participant_id: descriptor.id(),
                requirement: descriptor.requirement(),
            });
        }
        artifacts.sort_by_key(|artifact| artifact.participant_id);

        Ok(Self {
            sources: collected.sources,
            filesystem: filesystem.id(),
            global_library: global.id(),
            harvest,
            projections,
            readiness,
            artifacts,
        })
    }

    fn source(&self, participant_id: ParticipantId) -> Option<&SourceBinding> {
        self.sources
            .iter()
            .find(|binding| binding.participant_id == participant_id)
    }
}

fn descriptors_of_kind(
    participants: &ParticipantSet,
    kind: ParticipantKind,
) -> Vec<&ParticipantDescriptor> {
    participants
        .descriptors()
        .iter()
        .filter(|descriptor| descriptor.kind() == kind)
        .collect()
}

fn resolve_fenced_role(
    participants: &ParticipantSet,
    kind: ParticipantKind,
    sources: &[SourceBinding],
    source_fences: &[&StoredSourceFence],
) -> Result<Vec<(ParticipantId, ParticipantId)>, WorkPlanError> {
    let descriptors = descriptors_of_kind(participants, kind);
    let affected_count = sources.iter().filter(|source| source.affects).count();
    let mut resolved = Vec::with_capacity(affected_count);
    for (index, source) in sources
        .iter()
        .enumerate()
        .filter(|(_, source)| source.affects)
    {
        let matches = descriptors
            .iter()
            .filter(|descriptor| descriptor.source_fence() == Some(source_fences[index]))
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(if matches.is_empty() {
                WorkPlanError::IncompleteTopology(match kind {
                    ParticipantKind::Projection => {
                        "one matching Projection participant is required per source"
                    }
                    ParticipantKind::Readiness => {
                        "one matching Readiness participant is required per source"
                    }
                    _ => "matching fenced participant is required",
                })
            } else {
                WorkPlanError::AmbiguousTopology(match kind {
                    ParticipantKind::Projection => {
                        "one matching Projection participant is required per source"
                    }
                    ParticipantKind::Readiness => {
                        "one matching Readiness participant is required per source"
                    }
                    _ => "matching fenced participant is required",
                })
            });
        }
        resolved.push((matches[0].id(), source.participant_id));
    }
    for descriptor in descriptors {
        if !resolved
            .iter()
            .any(|(participant_id, _)| *participant_id == descriptor.id())
        {
            return Err(WorkPlanError::UnusedParticipant {
                participant_id: descriptor.id(),
            });
        }
    }
    resolved.sort_by_key(|(participant_id, _)| *participant_id);
    Ok(resolved)
}

fn collect_topology(
    args: &UniversalCommandArgs,
    collected: &mut CollectedTopology,
) -> Result<(), WorkPlanError> {
    match args {
        UniversalCommandArgs::Recording(command) => {
            collected.filesystem(command.work_key().participant_id())?;
            collect_namespace_directory(collected, command.destination().parent(), false, true)?;
        }
        UniversalCommandArgs::Copy(command) | UniversalCommandArgs::DuplicateSame(command) => {
            collect_copy_endpoint(collected, command.source(), true, false)?;
            collect_copy_endpoint(collected, command.destination(), false, true)?;
        }
        UniversalCommandArgs::Import(command) => {
            collected.filesystem(command.filesystem_participant_id())?;
            collect_copy_endpoint(collected, command.destination(), false, true)?;
        }
        UniversalCommandArgs::WaveformEdit(command) => {
            collect_copy_endpoint(collected, command.source(), true, true)?;
        }
        UniversalCommandArgs::DecodedEdit(command) => {
            collect_copy_endpoint(collected, command.source(), true, true)?;
        }
        UniversalCommandArgs::DecodedExport(command) => {
            collect_copy_endpoint(collected, command.source(), true, true)?;
            collect_copy_endpoint(collected, command.destination(), false, true)?;
        }
        UniversalCommandArgs::DecodedExportBatch(command) => {
            collect_copy_endpoint(collected, command.source(), true, true)?;
            for output in command.outputs() {
                collect_copy_endpoint(collected, output.destination(), false, true)?;
            }
        }
        UniversalCommandArgs::Namespace(command) => match command {
            NamespaceCommandArgs::Rename(command) => {
                collect_namespace_entry(collected, command.source(), true, true)?;
                collect_namespace_directory(
                    collected,
                    command.destination().parent(),
                    false,
                    true,
                )?;
            }
            NamespaceCommandArgs::Move(command) => {
                collect_namespace_entry(collected, command.source(), true, true)?;
                collect_namespace_directory(collected, command.destination_parent(), false, true)?;
            }
            NamespaceCommandArgs::CreateFolder(command) => {
                collect_namespace_directory(collected, command.parent(), true, true)?;
            }
        },
        UniversalCommandArgs::Render(command) => match command {
            RenderCommandArgs::Normalize(command) => {
                collect_copy_endpoint(collected, command.source(), true, true)?;
            }
            RenderCommandArgs::DuplicateDouble(command) => {
                collect_copy_endpoint(collected, command.source(), true, false)?;
                collect_copy_endpoint(collected, command.destination(), false, true)?;
            }
        },
        UniversalCommandArgs::Removal(command) => match command {
            RemovalCommandArgs::Trash(command) => {
                collected.filesystem(command.filesystem_participant_id())?;
                collect_namespace_entry(collected, command.source(), true, true)?;
            }
            RemovalCommandArgs::StageRetainedFolder(command) => {
                collect_namespace_entry(collected, command.source(), true, true)?;
            }
        },
        UniversalCommandArgs::RetainedResolution(command) => match command {
            RetainedResolutionCommandArgs::Restore(command) => {
                collect_namespace_entry(collected, command.observation().staged(), true, true)?;
            }
            RetainedResolutionCommandArgs::Purge(command) => {
                collect_namespace_entry(collected, command.observation().staged(), true, true)?;
            }
        },
        UniversalCommandArgs::SelectionNormalize(command) => {
            collect_copy_endpoint(collected, command.source(), true, true)?;
        }
        UniversalCommandArgs::SelectionOutput(command) => match command {
            SelectionOutputCommandArgs::Extract(command) => {
                collect_extract_topology(command, collected)?
            }
            SelectionOutputCommandArgs::ExtractAndTrim(command) => {
                collect_extract_and_trim_topology(command, collected)?;
            }
        },
        UniversalCommandArgs::History(command) => collect_history_topology(command, collected)?,
        UniversalCommandArgs::ExternalReconciliation(command) => {
            collected.filesystem(command.filesystem_participant_id())?;
            collect_external_topology(command, collected)?;
        }
    }
    Ok(())
}

fn collect_copy_endpoint(
    collected: &mut CollectedTopology,
    endpoint: &CopyEndpoint,
    reads: bool,
    affects: bool,
) -> Result<(), WorkPlanError> {
    collected.source(
        endpoint.participant_id(),
        endpoint.source_id(),
        reads,
        affects,
    )
}

fn collect_namespace_entry(
    collected: &mut CollectedTopology,
    endpoint: &NamespaceEntryEndpoint,
    reads: bool,
    affects: bool,
) -> Result<(), WorkPlanError> {
    collected.source(
        endpoint.participant_id(),
        endpoint.source_id(),
        reads,
        affects,
    )
}

fn collect_namespace_directory(
    collected: &mut CollectedTopology,
    endpoint: &super::namespace_commands::NamespaceDirectoryEndpoint,
    reads: bool,
    affects: bool,
) -> Result<(), WorkPlanError> {
    collected.source(
        endpoint.participant_id(),
        endpoint.source_id(),
        reads,
        affects,
    )
}

fn collect_extract_topology(
    command: &super::selection_output_commands::SelectionExtractIntentArgs,
    collected: &mut CollectedTopology,
) -> Result<(), WorkPlanError> {
    collect_copy_endpoint(collected, command.source(), true, false)?;
    collect_copy_endpoint(collected, command.destination(), false, true)
}

fn collect_extract_and_trim_topology(
    command: &ExtractAndTrimIntentArgs,
    collected: &mut CollectedTopology,
) -> Result<(), WorkPlanError> {
    collect_extract_topology(command.extraction(), collected)?;
    collect_copy_endpoint(collected, command.extraction().source(), true, true)
}

fn collect_external_topology(
    command: &ExternalReconciliationIntentArgs,
    collected: &mut CollectedTopology,
) -> Result<(), WorkPlanError> {
    collected.source(
        command.source_participant_id(),
        command.raw().provenance().source_id(),
        true,
        true,
    )
}

fn collect_history_topology(
    command: &HistoryUndoRedoIntentArgs,
    collected: &mut CollectedTopology,
) -> Result<(), WorkPlanError> {
    for action in command.actions() {
        match action {
            HistoryAction::FolderMove(folder_move) => {
                collect_namespace_directory(collected, folder_move.source_root(), true, true)?;
                for relocation in folder_move.moves() {
                    collect_namespace_entry(collected, relocation.source(), true, true)?;
                    collect_copy_endpoint(collected, relocation.destination(), false, true)?;
                }
            }
            HistoryAction::WaveformRestore(restore) => {
                collected.filesystem(restore.backups().filesystem_participant_id())?;
                collect_copy_endpoint(collected, restore.target(), true, true)?;
                if let Some(extracted) = restore.extracted_file() {
                    collect_copy_endpoint(collected, extracted.target(), false, true)?;
                }
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct PlanBuilder {
    units: Vec<WorkUnit>,
    prerequisites: Vec<WorkPrerequisite>,
}

impl PlanBuilder {
    fn add_unit(
        &mut self,
        key: WorkUnitKey,
        requirement: ParticipantRequirement,
        applicability: WorkUnitApplicability,
    ) -> Result<(), WorkPlanError> {
        if self.units.len() >= MAX_WORK_PLAN_UNITS {
            return Err(WorkPlanError::TooManyUnits {
                actual: self.units.len().saturating_add(1),
                maximum: MAX_WORK_PLAN_UNITS,
            });
        }
        if self.units.iter().any(|unit| unit.key == key) {
            return Err(WorkPlanError::DuplicateUnit { key });
        }
        self.units.push(WorkUnit {
            key,
            requirement,
            applicability,
        });
        Ok(())
    }

    fn add_edge(
        &mut self,
        prerequisite: WorkUnitKey,
        dependent: WorkUnitKey,
    ) -> Result<(), WorkPlanError> {
        if prerequisite == dependent {
            return Err(WorkPlanError::SelfPrerequisite { key: prerequisite });
        }
        let edge = WorkPrerequisite::new(prerequisite, dependent);
        if self.prerequisites.contains(&edge) {
            return Ok(());
        }
        if self.prerequisites.len() >= MAX_WORK_PLAN_EDGES {
            return Err(WorkPlanError::TooManyEdges {
                actual: self.prerequisites.len().saturating_add(1),
                maximum: MAX_WORK_PLAN_EDGES,
            });
        }
        self.prerequisites.push(edge);
        Ok(())
    }

    fn finish(mut self) -> Result<WorkPlan, WorkPlanError> {
        self.units.sort_by_key(|unit| unit.key);
        self.prerequisites.sort();
        let plan = WorkPlan {
            plan_version: WORK_PLAN_VERSION_V1,
            units: self.units,
            prerequisites: self.prerequisites,
        };
        plan.validate()?;
        Ok(plan)
    }
}

#[derive(Default)]
struct ActionSpan {
    first: Option<WorkUnitKey>,
    tail: Option<WorkUnitKey>,
    publication: Option<WorkUnitKey>,
}

#[derive(Clone, Copy)]
enum QualifiedRelocationKind {
    Namespace,
    Trash,
    RetainedFolder,
    HistoryFolder,
}

impl ActionSpan {
    fn record(&mut self, key: WorkUnitKey) {
        self.first.get_or_insert(key);
        self.tail = Some(key);
    }
}

fn derive_actions(
    args: &UniversalCommandArgs,
    topology: &Topology,
    builder: &mut PlanBuilder,
    action_spans: &mut Vec<ActionSpan>,
) -> Result<(), WorkPlanError> {
    match args {
        UniversalCommandArgs::Recording(command) => {
            let mut span = ActionSpan::default();
            add_staged_mutation_without_source_read(
                builder,
                &mut span,
                0,
                0,
                topology.filesystem,
                command.destination().parent().participant_id(),
            )?;
            action_spans.push(span);
        }
        UniversalCommandArgs::Copy(command) => {
            let mut span = ActionSpan::default();
            let source = required_source(topology, command.source().participant_id())?;
            let destination = required_source(topology, command.destination().participant_id())?;
            add_staged_mutation(
                builder,
                &mut span,
                0,
                0,
                topology.filesystem,
                source,
                destination.participant_id,
            )?;
            action_spans.push(span);
        }
        UniversalCommandArgs::DuplicateSame(command) => {
            let mut span = ActionSpan::default();
            let source = required_source(topology, command.source().participant_id())?;
            let destination = required_source(topology, command.destination().participant_id())?;
            add_staged_mutation(
                builder,
                &mut span,
                0,
                0,
                topology.filesystem,
                source,
                destination.participant_id,
            )?;
            action_spans.push(span);
        }
        UniversalCommandArgs::Import(command) => {
            let mut span = ActionSpan::default();
            let destination = required_source(topology, command.destination().participant_id())?;
            add_staged_mutation_without_source_read(
                builder,
                &mut span,
                0,
                0,
                topology.filesystem,
                destination.participant_id,
            )?;
            action_spans.push(span);
        }
        UniversalCommandArgs::WaveformEdit(command) => {
            let mut span = ActionSpan::default();
            let source = required_source(topology, command.source().participant_id())?;
            add_staged_mutation_with_recovery(
                builder,
                &mut span,
                0,
                0,
                topology.filesystem,
                source,
                source.participant_id,
            )?;
            action_spans.push(span);
        }
        UniversalCommandArgs::DecodedEdit(command) => {
            let mut span = ActionSpan::default();
            let source = required_source(topology, command.source().participant_id())?;
            add_staged_mutation_with_recovery(
                builder,
                &mut span,
                0,
                0,
                topology.filesystem,
                source,
                source.participant_id,
            )?;
            action_spans.push(span);
        }
        UniversalCommandArgs::DecodedExport(command) => {
            let mut span = ActionSpan::default();
            let source = required_source(topology, command.source().participant_id())?;
            let destination = required_source(topology, command.destination().participant_id())?;
            add_staged_mutation(
                builder,
                &mut span,
                0,
                0,
                topology.filesystem,
                source,
                destination.participant_id,
            )?;
            action_spans.push(span);
        }
        UniversalCommandArgs::DecodedExportBatch(command) => {
            let source = required_source(topology, command.source().participant_id())?;
            for (index, output) in command.outputs().iter().enumerate() {
                let action = u8::try_from(index).map_err(|_| WorkPlanError::StepOrdinalOverflow)?;
                let destination = required_source(topology, output.destination().participant_id())?;
                let mut span = ActionSpan::default();
                add_staged_mutation(
                    builder,
                    &mut span,
                    action,
                    0,
                    topology.filesystem,
                    source,
                    destination.participant_id,
                )?;
                action_spans.push(span);
            }
        }
        UniversalCommandArgs::Namespace(command) => {
            let mut span = ActionSpan::default();
            match command {
                NamespaceCommandArgs::Rename(command) => {
                    let source = required_source(topology, command.source().participant_id())?;
                    add_qualified_relocation(
                        builder,
                        &mut span,
                        0,
                        0,
                        topology.filesystem,
                        source,
                        source.participant_id,
                        QualifiedRelocationKind::Namespace,
                    )?;
                }
                NamespaceCommandArgs::Move(command) => {
                    let source = required_source(topology, command.source().participant_id())?;
                    let destination =
                        required_source(topology, command.destination_parent().participant_id())?;
                    if command.source().expectation().kind() == NamespaceEntryKind::RegularFile {
                        // A regular-file move always removes the origin after the staged
                        // destination publication has been reconciled.  This remains true when
                        // both endpoints use one SourceDB participant; sharing a source fence
                        // does not turn a move into a copy or prove a direct filesystem rename.
                        add_cross_source_move(
                            builder,
                            &mut span,
                            0,
                            0,
                            topology.filesystem,
                            source,
                            destination.participant_id,
                        )?;
                    } else {
                        add_qualified_relocation(
                            builder,
                            &mut span,
                            0,
                            0,
                            topology.filesystem,
                            source,
                            destination.participant_id,
                            QualifiedRelocationKind::Namespace,
                        )?;
                    }
                }
                NamespaceCommandArgs::CreateFolder(command) => {
                    let destination = required_source(topology, command.parent().participant_id())?;
                    add_direct_creation(
                        builder,
                        &mut span,
                        0,
                        0,
                        topology.filesystem,
                        destination,
                        destination.participant_id,
                    )?;
                }
            }
            action_spans.push(span);
        }
        UniversalCommandArgs::Render(command) => {
            let mut span = ActionSpan::default();
            match command {
                RenderCommandArgs::Normalize(command) => {
                    let source = required_source(topology, command.source().participant_id())?;
                    add_staged_mutation_with_recovery(
                        builder,
                        &mut span,
                        0,
                        0,
                        topology.filesystem,
                        source,
                        source.participant_id,
                    )?;
                }
                RenderCommandArgs::DuplicateDouble(command) => {
                    let source = required_source(topology, command.source().participant_id())?;
                    let destination =
                        required_source(topology, command.destination().participant_id())?;
                    add_staged_mutation(
                        builder,
                        &mut span,
                        0,
                        0,
                        topology.filesystem,
                        source,
                        destination.participant_id,
                    )?;
                }
            }
            action_spans.push(span);
        }
        UniversalCommandArgs::Removal(command) => {
            let mut span = ActionSpan::default();
            match command {
                RemovalCommandArgs::Trash(command) => {
                    let source = required_source(topology, command.source().participant_id())?;
                    add_qualified_relocation(
                        builder,
                        &mut span,
                        0,
                        0,
                        topology.filesystem,
                        source,
                        source.participant_id,
                        QualifiedRelocationKind::Trash,
                    )?;
                }
                RemovalCommandArgs::StageRetainedFolder(command) => {
                    let source = required_source(topology, command.source().participant_id())?;
                    add_qualified_relocation(
                        builder,
                        &mut span,
                        0,
                        0,
                        topology.filesystem,
                        source,
                        source.participant_id,
                        QualifiedRelocationKind::RetainedFolder,
                    )?;
                }
            }
            action_spans.push(span);
        }
        UniversalCommandArgs::RetainedResolution(command) => {
            let mut span = ActionSpan::default();
            match command {
                RetainedResolutionCommandArgs::Restore(command) => {
                    let source =
                        required_source(topology, command.observation().staged().participant_id())?;
                    add_retained_merge_mutation(
                        builder,
                        &mut span,
                        0,
                        0,
                        topology.filesystem,
                        source,
                        source.participant_id,
                    )?;
                }
                RetainedResolutionCommandArgs::Purge(command) => {
                    let source =
                        required_source(topology, command.observation().staged().participant_id())?;
                    add_retained_purge(
                        builder,
                        &mut span,
                        0,
                        0,
                        topology.filesystem,
                        source,
                        source.participant_id,
                    )?;
                }
            }
            action_spans.push(span);
        }
        UniversalCommandArgs::SelectionNormalize(command) => {
            let mut span = ActionSpan::default();
            let source = required_source(topology, command.source().participant_id())?;
            add_staged_mutation_with_recovery(
                builder,
                &mut span,
                0,
                0,
                topology.filesystem,
                source,
                source.participant_id,
            )?;
            action_spans.push(span);
        }
        UniversalCommandArgs::SelectionOutput(command) => match command {
            SelectionOutputCommandArgs::Extract(command) => {
                let mut span = ActionSpan::default();
                let source = required_source(topology, command.source().participant_id())?;
                let destination =
                    required_source(topology, command.destination().participant_id())?;
                add_staged_mutation(
                    builder,
                    &mut span,
                    0,
                    0,
                    topology.filesystem,
                    source,
                    destination.participant_id,
                )?;
                action_spans.push(span);
            }
            SelectionOutputCommandArgs::ExtractAndTrim(command) => {
                let extraction = command.extraction();
                let source = required_source(topology, extraction.source().participant_id())?;
                let destination =
                    required_source(topology, extraction.destination().participant_id())?;
                let mut extraction_span = ActionSpan::default();
                add_staged_mutation(
                    builder,
                    &mut extraction_span,
                    0,
                    0,
                    topology.filesystem,
                    source,
                    destination.participant_id,
                )?;
                let mut trim_span = ActionSpan::default();
                add_staged_mutation_with_recovery(
                    builder,
                    &mut trim_span,
                    1,
                    0,
                    topology.filesystem,
                    source,
                    source.participant_id,
                )?;
                chain_spans(builder, &extraction_span, &trim_span)?;
                if let (Some(published), Some(trim_first)) =
                    (extraction_span.publication, trim_span.first)
                {
                    // Keep the native compound order explicit: the trim cannot begin until the
                    // extraction's verified publication has completed.
                    builder.add_edge(published, trim_first)?;
                }
                action_spans.push(extraction_span);
                action_spans.push(trim_span);
            }
        },
        UniversalCommandArgs::History(command) => {
            derive_history_actions(command, topology, builder, action_spans)?
        }
        UniversalCommandArgs::ExternalReconciliation(command) => {
            let mut span = ActionSpan::default();
            let source = required_source(topology, command.source_participant_id())?;
            add_external_audit(builder, &mut span, 0, 0, topology.filesystem, source)?;
            action_spans.push(span);
        }
    }
    if !matches!(args, UniversalCommandArgs::DecodedExportBatch(_)) {
        for pair in action_spans.windows(2) {
            chain_spans(builder, &pair[0], &pair[1])?;
        }
    }
    Ok(())
}

fn derive_history_actions(
    command: &HistoryUndoRedoIntentArgs,
    topology: &Topology,
    builder: &mut PlanBuilder,
    action_spans: &mut Vec<ActionSpan>,
) -> Result<(), WorkPlanError> {
    for (action_index, action) in command.actions().iter().enumerate() {
        let action_ordinal =
            u8::try_from(action_index).map_err(|_| WorkPlanError::StepOrdinalOverflow)?;
        let mut span = ActionSpan::default();
        match action {
            HistoryAction::FolderMove(folder_move) => {
                for (relocation_index, relocation) in folder_move.moves().iter().enumerate() {
                    let base = history_step_base(relocation_index)?;
                    let source = required_source(topology, relocation.source().participant_id())?;
                    let destination =
                        required_source(topology, relocation.destination().participant_id())?;
                    let mut relocation_span = ActionSpan::default();
                    add_history_folder_relocation(
                        builder,
                        &mut relocation_span,
                        action_ordinal,
                        base,
                        topology.filesystem,
                        source,
                        destination.participant_id,
                    )?;
                    if let Some(previous) = span.tail {
                        if let Some(first) = relocation_span.first {
                            builder.add_edge(previous, first)?;
                        }
                    }
                    span.first = span.first.or(relocation_span.first);
                    span.tail = relocation_span.tail.or(span.tail);
                }
            }
            HistoryAction::WaveformRestore(restore) => {
                let source = required_source(topology, restore.target().participant_id())?;
                add_history_restore(
                    builder,
                    &mut span,
                    action_ordinal,
                    command.direction(),
                    topology.filesystem,
                    source,
                    restore
                        .extracted_file()
                        .map(|file| file.target().participant_id()),
                )?;
            }
        }
        action_spans.push(span);
    }
    for pair in action_spans.windows(2) {
        chain_spans(builder, &pair[0], &pair[1])?;
    }
    Ok(())
}

fn history_step_base(index: usize) -> Result<u16, WorkPlanError> {
    let index = u16::try_from(index).map_err(|_| WorkPlanError::StepOrdinalOverflow)?;
    index
        .checked_mul(STEP_HISTORY_STRIDE)
        .ok_or(WorkPlanError::StepOrdinalOverflow)
}

fn required_source<'a>(
    topology: &'a Topology,
    participant_id: ParticipantId,
) -> Result<&'a SourceBinding, WorkPlanError> {
    topology
        .source(participant_id)
        .ok_or(WorkPlanError::SourceBindingMismatch)
}

fn step(base: u16, offset: u16) -> Result<u16, WorkPlanError> {
    base.checked_add(offset)
        .ok_or(WorkPlanError::StepOrdinalOverflow)
}

fn add_required(
    builder: &mut PlanBuilder,
    span: &mut ActionSpan,
    action: u8,
    step_ordinal: u16,
    participant_id: ParticipantId,
    role: WorkUnitRole,
    requirement: ParticipantRequirement,
) -> Result<WorkUnitKey, WorkPlanError> {
    let key = WorkUnitKey::new(action, step_ordinal, participant_id, role);
    builder.add_unit(key, requirement, WorkUnitApplicability::Required)?;
    span.record(key);
    if role == WorkUnitRole::Publish {
        span.publication = Some(key);
    }
    Ok(key)
}

fn add_not_applicable(
    builder: &mut PlanBuilder,
    action: u8,
    step_ordinal: u16,
    participant_id: ParticipantId,
    role: WorkUnitRole,
) -> Result<WorkUnitKey, WorkPlanError> {
    let key = WorkUnitKey::new(action, step_ordinal, participant_id, role);
    builder.add_unit(
        key,
        ParticipantRequirement::Required,
        WorkUnitApplicability::NotApplicable,
    )?;
    Ok(key)
}

fn add_staged_mutation(
    builder: &mut PlanBuilder,
    span: &mut ActionSpan,
    action: u8,
    base: u16,
    filesystem: ParticipantId,
    source: &SourceBinding,
    affected: ParticipantId,
) -> Result<(), WorkPlanError> {
    add_staged_mutation_inner(
        builder, span, action, base, filesystem, source, affected, false,
    )
}

fn add_staged_mutation_with_recovery(
    builder: &mut PlanBuilder,
    span: &mut ActionSpan,
    action: u8,
    base: u16,
    filesystem: ParticipantId,
    source: &SourceBinding,
    affected: ParticipantId,
) -> Result<(), WorkPlanError> {
    add_staged_mutation_inner(
        builder, span, action, base, filesystem, source, affected, true,
    )
}

fn add_staged_mutation_inner(
    builder: &mut PlanBuilder,
    span: &mut ActionSpan,
    action: u8,
    base: u16,
    filesystem: ParticipantId,
    source: &SourceBinding,
    affected: ParticipantId,
    retain_recovery: bool,
) -> Result<(), WorkPlanError> {
    let source_read = add_required(
        builder,
        span,
        action,
        step(base, STEP_SOURCE_READ)?,
        source.participant_id,
        WorkUnitRole::SourceRead,
        ParticipantRequirement::Required,
    )?;
    let recovery = if retain_recovery {
        Some(add_required(
            builder,
            span,
            action,
            step(base, STEP_RECOVERY_MATERIAL)?,
            filesystem,
            WorkUnitRole::RecoveryMaterialRetained,
            ParticipantRequirement::Required,
        )?)
    } else {
        None
    };
    let prepare = add_required(
        builder,
        span,
        action,
        step(base, STEP_PREPARE)?,
        filesystem,
        WorkUnitRole::Prepare,
        ParticipantRequirement::Required,
    )?;
    let stage = add_required(
        builder,
        span,
        action,
        step(base, STEP_STAGE)?,
        filesystem,
        WorkUnitRole::Stage,
        ParticipantRequirement::Required,
    )?;
    let copied = add_required(
        builder,
        span,
        action,
        step(base, STEP_COPY_VALIDATED)?,
        filesystem,
        WorkUnitRole::CopyValidated,
        ParticipantRequirement::Required,
    )?;
    let publish = add_required(
        builder,
        span,
        action,
        step(base, STEP_PUBLISH)?,
        filesystem,
        WorkUnitRole::Publish,
        ParticipantRequirement::Required,
    )?;
    let affect = add_required(
        builder,
        span,
        action,
        step(base, STEP_SOURCE_AFFECT)?,
        affected,
        WorkUnitRole::SourceAffect,
        ParticipantRequirement::Required,
    )?;
    if let Some(recovery) = recovery {
        builder.add_edge(source_read, recovery)?;
        builder.add_edge(recovery, prepare)?;
    } else {
        builder.add_edge(source_read, prepare)?;
    }
    builder.add_edge(prepare, stage)?;
    builder.add_edge(stage, copied)?;
    builder.add_edge(copied, publish)?;
    builder.add_edge(publish, affect)?;
    Ok(())
}

fn add_staged_mutation_without_source_read(
    builder: &mut PlanBuilder,
    span: &mut ActionSpan,
    action: u8,
    base: u16,
    filesystem: ParticipantId,
    affected: ParticipantId,
) -> Result<(), WorkPlanError> {
    let prepare = add_required(
        builder,
        span,
        action,
        step(base, STEP_PREPARE)?,
        filesystem,
        WorkUnitRole::Prepare,
        ParticipantRequirement::Required,
    )?;
    let stage = add_required(
        builder,
        span,
        action,
        step(base, STEP_STAGE)?,
        filesystem,
        WorkUnitRole::Stage,
        ParticipantRequirement::Required,
    )?;
    let copied = add_required(
        builder,
        span,
        action,
        step(base, STEP_COPY_VALIDATED)?,
        filesystem,
        WorkUnitRole::CopyValidated,
        ParticipantRequirement::Required,
    )?;
    let publish = add_required(
        builder,
        span,
        action,
        step(base, STEP_PUBLISH)?,
        filesystem,
        WorkUnitRole::Publish,
        ParticipantRequirement::Required,
    )?;
    let affect = add_required(
        builder,
        span,
        action,
        step(base, STEP_SOURCE_AFFECT)?,
        affected,
        WorkUnitRole::SourceAffect,
        ParticipantRequirement::Required,
    )?;
    builder.add_edge(prepare, stage)?;
    builder.add_edge(stage, copied)?;
    builder.add_edge(copied, publish)?;
    builder.add_edge(publish, affect)?;
    Ok(())
}

fn add_retained_merge_mutation(
    builder: &mut PlanBuilder,
    span: &mut ActionSpan,
    action: u8,
    base: u16,
    filesystem: ParticipantId,
    source: &SourceBinding,
    affected: ParticipantId,
) -> Result<(), WorkPlanError> {
    let source_read = add_required(
        builder,
        span,
        action,
        step(base, STEP_SOURCE_READ)?,
        source.participant_id,
        WorkUnitRole::SourceRead,
        ParticipantRequirement::Required,
    )?;
    let prepare = add_required(
        builder,
        span,
        action,
        step(base, STEP_PREPARE)?,
        filesystem,
        WorkUnitRole::Prepare,
        ParticipantRequirement::Required,
    )?;
    // Retained restore uses a later bounded prepared manifest and merge owner. It is not a
    // single staged file copy, so the ordinary Stage and CopyValidated obligations are fixed
    // not-applicable for this aggregate recipe.
    add_not_applicable(
        builder,
        action,
        step(base, STEP_STAGE)?,
        filesystem,
        WorkUnitRole::Stage,
    )?;
    let merge = add_required(
        builder,
        span,
        action,
        step(base, STEP_RETAINED_MERGE)?,
        filesystem,
        WorkUnitRole::RetainedMerge,
        ParticipantRequirement::Required,
    )?;
    add_not_applicable(
        builder,
        action,
        step(base, STEP_COPY_VALIDATED)?,
        filesystem,
        WorkUnitRole::CopyValidated,
    )?;
    let publish = add_required(
        builder,
        span,
        action,
        step(base, STEP_PUBLISH)?,
        filesystem,
        WorkUnitRole::Publish,
        ParticipantRequirement::Required,
    )?;
    let affect = add_required(
        builder,
        span,
        action,
        step(base, STEP_SOURCE_AFFECT)?,
        affected,
        WorkUnitRole::SourceAffect,
        ParticipantRequirement::Required,
    )?;
    builder.add_edge(source_read, prepare)?;
    builder.add_edge(prepare, merge)?;
    builder.add_edge(merge, publish)?;
    builder.add_edge(publish, affect)?;
    Ok(())
}

fn add_qualified_relocation(
    builder: &mut PlanBuilder,
    span: &mut ActionSpan,
    action: u8,
    base: u16,
    filesystem: ParticipantId,
    source: &SourceBinding,
    affected: ParticipantId,
    kind: QualifiedRelocationKind,
) -> Result<(), WorkPlanError> {
    let (restore_record, ensure_parents, origin_removal_intent, retained_payload) = match kind {
        QualifiedRelocationKind::Namespace => (false, false, false, false),
        QualifiedRelocationKind::Trash => (true, false, true, true),
        QualifiedRelocationKind::RetainedFolder => (true, true, true, true),
        QualifiedRelocationKind::HistoryFolder => (false, true, false, false),
    };
    let source_read = add_required(
        builder,
        span,
        action,
        step(base, STEP_SOURCE_READ)?,
        source.participant_id,
        WorkUnitRole::SourceRead,
        ParticipantRequirement::Required,
    )?;
    let prepare = add_required(
        builder,
        span,
        action,
        step(base, STEP_PREPARE)?,
        filesystem,
        WorkUnitRole::Prepare,
        ParticipantRequirement::Required,
    )?;
    let mut predecessor = prepare;
    if restore_record {
        let record = add_required(
            builder,
            span,
            action,
            step(base, STEP_RESTORE_RECORD)?,
            filesystem,
            WorkUnitRole::RestoreRecordPrepared,
            ParticipantRequirement::Required,
        )?;
        builder.add_edge(predecessor, record)?;
        predecessor = record;
    }
    if ensure_parents {
        let parents = add_required(
            builder,
            span,
            action,
            step(base, STEP_ENSURE_PARENTS)?,
            filesystem,
            WorkUnitRole::EnsureContainedParentDirectories,
            ParticipantRequirement::Required,
        )?;
        builder.add_edge(predecessor, parents)?;
        predecessor = parents;
    }
    if origin_removal_intent {
        let removal_start = add_required(
            builder,
            span,
            action,
            step(base, STEP_DIRECT_REMOVAL_INTENT)?,
            filesystem,
            WorkUnitRole::OriginRemovalStart,
            ParticipantRequirement::Required,
        )?;
        builder.add_edge(predecessor, removal_start)?;
        predecessor = removal_start;
    }

    add_not_applicable(
        builder,
        action,
        step(base, STEP_STAGE)?,
        filesystem,
        WorkUnitRole::Stage,
    )?;
    add_not_applicable(
        builder,
        action,
        step(base, STEP_COPY_VALIDATED)?,
        filesystem,
        WorkUnitRole::CopyValidated,
    )?;
    let relocation = add_required(
        builder,
        span,
        action,
        step(base, STEP_QUALIFIED_RELOCATION)?,
        filesystem,
        WorkUnitRole::QualifiedRelocation,
        ParticipantRequirement::Required,
    )?;
    builder.add_edge(predecessor, relocation)?;
    predecessor = relocation;
    if retained_payload {
        let payload = add_required(
            builder,
            span,
            action,
            step(base, STEP_RETAINED_PAYLOAD)?,
            filesystem,
            WorkUnitRole::RetainedPayloadVerified,
            ParticipantRequirement::Required,
        )?;
        builder.add_edge(predecessor, payload)?;
        predecessor = payload;
    }
    let absence = add_required(
        builder,
        span,
        action,
        step(base, STEP_RETAINED_ABSENCE)?,
        filesystem,
        WorkUnitRole::OriginAbsence,
        ParticipantRequirement::Required,
    )?;
    let affect = add_required(
        builder,
        span,
        action,
        step(base, STEP_SOURCE_AFFECT)?,
        affected,
        WorkUnitRole::SourceAffect,
        ParticipantRequirement::Required,
    )?;
    builder.add_edge(predecessor, absence)?;
    builder.add_edge(absence, affect)?;
    builder.add_edge(source_read, prepare)?;
    Ok(())
}

fn add_history_folder_relocation(
    builder: &mut PlanBuilder,
    span: &mut ActionSpan,
    action: u8,
    base: u16,
    filesystem: ParticipantId,
    source: &SourceBinding,
    affected: ParticipantId,
) -> Result<(), WorkPlanError> {
    add_qualified_relocation(
        builder,
        span,
        action,
        base,
        filesystem,
        source,
        affected,
        QualifiedRelocationKind::HistoryFolder,
    )
}

fn add_direct_creation(
    builder: &mut PlanBuilder,
    span: &mut ActionSpan,
    action: u8,
    base: u16,
    filesystem: ParticipantId,
    source: &SourceBinding,
    affected: ParticipantId,
) -> Result<(), WorkPlanError> {
    let source_read = add_required(
        builder,
        span,
        action,
        step(base, STEP_SOURCE_READ)?,
        source.participant_id,
        WorkUnitRole::SourceRead,
        ParticipantRequirement::Required,
    )?;
    let prepare = add_required(
        builder,
        span,
        action,
        step(base, STEP_PREPARE)?,
        filesystem,
        WorkUnitRole::Prepare,
        ParticipantRequirement::Required,
    )?;
    let _stage = add_not_applicable(
        builder,
        action,
        step(base, STEP_STAGE)?,
        filesystem,
        WorkUnitRole::Stage,
    )?;
    let _copied = add_not_applicable(
        builder,
        action,
        step(base, STEP_COPY_VALIDATED)?,
        filesystem,
        WorkUnitRole::CopyValidated,
    )?;
    let publish = add_required(
        builder,
        span,
        action,
        step(base, STEP_PUBLISH)?,
        filesystem,
        WorkUnitRole::Publish,
        ParticipantRequirement::Required,
    )?;
    let affect = add_required(
        builder,
        span,
        action,
        step(base, STEP_SOURCE_AFFECT)?,
        affected,
        WorkUnitRole::SourceAffect,
        ParticipantRequirement::Required,
    )?;
    builder.add_edge(source_read, prepare)?;
    builder.add_edge(prepare, publish)?;
    builder.add_edge(publish, affect)?;
    Ok(())
}

fn add_retained_purge(
    builder: &mut PlanBuilder,
    span: &mut ActionSpan,
    action: u8,
    base: u16,
    filesystem: ParticipantId,
    source: &SourceBinding,
    affected: ParticipantId,
) -> Result<(), WorkPlanError> {
    let source_read = add_required(
        builder,
        span,
        action,
        step(base, STEP_SOURCE_READ)?,
        source.participant_id,
        WorkUnitRole::SourceRead,
        ParticipantRequirement::Required,
    )?;
    let prepare = add_required(
        builder,
        span,
        action,
        step(base, STEP_PREPARE)?,
        filesystem,
        WorkUnitRole::Prepare,
        ParticipantRequirement::Required,
    )?;
    add_not_applicable(
        builder,
        action,
        step(base, STEP_STAGE)?,
        filesystem,
        WorkUnitRole::Stage,
    )?;
    add_not_applicable(
        builder,
        action,
        step(base, STEP_COPY_VALIDATED)?,
        filesystem,
        WorkUnitRole::CopyValidated,
    )?;
    let disposition = add_required(
        builder,
        span,
        action,
        step(base, STEP_QUALIFIED_RELOCATION)?,
        filesystem,
        WorkUnitRole::SelectedRetainedObjectDisposition,
        ParticipantRequirement::Required,
    )?;
    let absence = add_required(
        builder,
        span,
        action,
        step(base, STEP_RETAINED_PAYLOAD)?,
        filesystem,
        WorkUnitRole::RetainedObjectAbsence,
        ParticipantRequirement::Required,
    )?;
    let affect = add_required(
        builder,
        span,
        action,
        step(base, STEP_SOURCE_AFFECT)?,
        affected,
        WorkUnitRole::SourceAffect,
        ParticipantRequirement::Required,
    )?;
    builder.add_edge(source_read, prepare)?;
    builder.add_edge(prepare, disposition)?;
    builder.add_edge(disposition, absence)?;
    builder.add_edge(absence, affect)?;
    Ok(())
}

fn add_cross_source_move(
    builder: &mut PlanBuilder,
    span: &mut ActionSpan,
    action: u8,
    base: u16,
    filesystem: ParticipantId,
    origin: &SourceBinding,
    destination: ParticipantId,
) -> Result<(), WorkPlanError> {
    let source_read = add_required(
        builder,
        span,
        action,
        step(base, STEP_SOURCE_READ)?,
        origin.participant_id,
        WorkUnitRole::SourceRead,
        ParticipantRequirement::Required,
    )?;
    let prepare = add_required(
        builder,
        span,
        action,
        step(base, STEP_PREPARE)?,
        filesystem,
        WorkUnitRole::Prepare,
        ParticipantRequirement::Required,
    )?;
    let stage = add_required(
        builder,
        span,
        action,
        step(base, STEP_STAGE)?,
        filesystem,
        WorkUnitRole::Stage,
        ParticipantRequirement::Required,
    )?;
    let copied = add_required(
        builder,
        span,
        action,
        step(base, STEP_COPY_VALIDATED)?,
        filesystem,
        WorkUnitRole::CopyValidated,
        ParticipantRequirement::Required,
    )?;
    let publish = add_required(
        builder,
        span,
        action,
        step(base, STEP_PUBLISH)?,
        filesystem,
        WorkUnitRole::Publish,
        ParticipantRequirement::Required,
    )?;
    let destination_reconcile = add_required(
        builder,
        span,
        action,
        step(base, STEP_DESTINATION_RECONCILE)?,
        destination,
        WorkUnitRole::DestinationSourceReconcile,
        ParticipantRequirement::Required,
    )?;
    let origin_start = add_required(
        builder,
        span,
        action,
        step(base, STEP_ORIGIN_REMOVAL_START)?,
        filesystem,
        WorkUnitRole::OriginRemovalStart,
        ParticipantRequirement::Required,
    )?;
    let origin_absence = add_required(
        builder,
        span,
        action,
        step(base, STEP_ORIGIN_ABSENCE)?,
        filesystem,
        WorkUnitRole::OriginAbsence,
        ParticipantRequirement::Required,
    )?;
    let origin_reconcile = add_required(
        builder,
        span,
        action,
        step(base, STEP_ORIGIN_RECONCILE)?,
        origin.participant_id,
        WorkUnitRole::OriginSourceReconcile,
        ParticipantRequirement::Required,
    )?;
    builder.add_edge(source_read, prepare)?;
    builder.add_edge(prepare, stage)?;
    builder.add_edge(stage, copied)?;
    builder.add_edge(copied, publish)?;
    builder.add_edge(publish, destination_reconcile)?;
    builder.add_edge(destination_reconcile, origin_start)?;
    builder.add_edge(origin_start, origin_absence)?;
    builder.add_edge(origin_absence, origin_reconcile)?;
    Ok(())
}

fn add_external_audit(
    builder: &mut PlanBuilder,
    span: &mut ActionSpan,
    action: u8,
    base: u16,
    filesystem: ParticipantId,
    source: &SourceBinding,
) -> Result<(), WorkPlanError> {
    let source_read = add_required(
        builder,
        span,
        action,
        step(base, STEP_SOURCE_READ)?,
        source.participant_id,
        WorkUnitRole::SourceRead,
        ParticipantRequirement::Required,
    )?;
    let audit = add_required(
        builder,
        span,
        action,
        step(base, STEP_EXTERNAL_AUDIT)?,
        filesystem,
        WorkUnitRole::ExternalAudit,
        ParticipantRequirement::Required,
    )?;
    let affect = add_required(
        builder,
        span,
        action,
        step(base, STEP_SOURCE_AFFECT)?,
        source.participant_id,
        WorkUnitRole::SourceAffect,
        ParticipantRequirement::Required,
    )?;
    builder.add_edge(source_read, audit)?;
    builder.add_edge(audit, affect)?;
    Ok(())
}

fn add_history_restore(
    builder: &mut PlanBuilder,
    span: &mut ActionSpan,
    action: u8,
    direction: HistoryDirection,
    filesystem: ParticipantId,
    source: &SourceBinding,
    extracted: Option<ParticipantId>,
) -> Result<(), WorkPlanError> {
    add_staged_mutation_with_recovery(
        builder,
        span,
        action,
        0,
        filesystem,
        source,
        source.participant_id,
    )?;
    let restore_tail = span.tail.ok_or(WorkPlanError::SourceBindingMismatch)?;
    if let Some(extracted) = extracted {
        match direction {
            HistoryDirection::Undo => {
                let remove_start = add_required(
                    builder,
                    span,
                    action,
                    STEP_HISTORY_STRIDE + STEP_ORIGIN_REMOVAL_START,
                    filesystem,
                    WorkUnitRole::OriginRemovalStart,
                    ParticipantRequirement::Required,
                )?;
                let absence = add_required(
                    builder,
                    span,
                    action,
                    STEP_HISTORY_STRIDE + STEP_ORIGIN_ABSENCE,
                    filesystem,
                    WorkUnitRole::OriginAbsence,
                    ParticipantRequirement::Required,
                )?;
                let reconcile = add_required(
                    builder,
                    span,
                    action,
                    STEP_HISTORY_STRIDE + STEP_ORIGIN_RECONCILE,
                    extracted,
                    WorkUnitRole::OriginSourceReconcile,
                    ParticipantRequirement::Required,
                )?;
                builder.add_edge(restore_tail, remove_start)?;
                builder.add_edge(remove_start, absence)?;
                builder.add_edge(absence, reconcile)?;
            }
            HistoryDirection::Redo => {
                let mut recreate = ActionSpan::default();
                add_staged_mutation(
                    builder,
                    &mut recreate,
                    action,
                    STEP_HISTORY_STRIDE,
                    filesystem,
                    source,
                    extracted,
                )?;
                if let Some(first) = recreate.first {
                    builder.add_edge(restore_tail, first)?;
                }
                span.tail = recreate.tail;
            }
        }
    }
    Ok(())
}

fn chain_spans(
    builder: &mut PlanBuilder,
    prerequisite: &ActionSpan,
    dependent: &ActionSpan,
) -> Result<(), WorkPlanError> {
    if let (Some(prerequisite), Some(dependent)) = (prerequisite.tail, dependent.first) {
        builder.add_edge(prerequisite, dependent)?;
    }
    Ok(())
}

fn add_global_obligations(
    topology: &Topology,
    builder: &mut PlanBuilder,
    action_spans: &[ActionSpan],
) -> Result<(), WorkPlanError> {
    let mut global_span = ActionSpan::default();
    let global = add_required(
        builder,
        &mut global_span,
        GLOBAL_ACTION_ORDINAL,
        STEP_GLOBAL_LIBRARY,
        topology.global_library,
        WorkUnitRole::GlobalLibrary,
        ParticipantRequirement::Required,
    )?;
    for span in action_spans {
        if let Some(tail) = span.tail {
            builder.add_edge(tail, global)?;
        }
    }

    let mut harvest_keys = Vec::with_capacity(topology.harvest.len());
    for (index, participant_id) in topology.harvest.iter().enumerate() {
        let offset = u16::try_from(index).map_err(|_| WorkPlanError::StepOrdinalOverflow)?;
        let key = add_required(
            builder,
            &mut global_span,
            GLOBAL_ACTION_ORDINAL,
            step(STEP_HARVEST, offset)?,
            *participant_id,
            WorkUnitRole::Harvest,
            ParticipantRequirement::Required,
        )?;
        builder.add_edge(global, key)?;
        harvest_keys.push(key);
    }

    let mut readiness_keys = Vec::with_capacity(topology.readiness.len());
    for (index, (participant_id, source_id)) in topology.projections.iter().enumerate() {
        let offset = u16::try_from(index).map_err(|_| WorkPlanError::StepOrdinalOverflow)?;
        let projection = add_required(
            builder,
            &mut global_span,
            GLOBAL_ACTION_ORDINAL,
            step(STEP_PROJECTION, offset)?,
            *participant_id,
            WorkUnitRole::Projection,
            ParticipantRequirement::Required,
        )?;
        for harvest in &harvest_keys {
            builder.add_edge(*harvest, projection)?;
        }
        let Some((readiness_id, _readiness_source_id)) = topology
            .readiness
            .iter()
            .find(|(_, source)| source == source_id)
        else {
            return Err(WorkPlanError::SourceBindingMismatch);
        };
        let readiness = add_required(
            builder,
            &mut global_span,
            GLOBAL_ACTION_ORDINAL,
            step(STEP_READINESS, offset)?,
            *readiness_id,
            WorkUnitRole::Readiness,
            ParticipantRequirement::Required,
        )?;
        builder.add_edge(projection, readiness)?;
        readiness_keys.push(readiness);
    }

    for (index, artifact) in topology.artifacts.iter().enumerate() {
        let offset = u16::try_from(index).map_err(|_| WorkPlanError::StepOrdinalOverflow)?;
        let artifact_key = add_required(
            builder,
            &mut global_span,
            GLOBAL_ACTION_ORDINAL,
            step(STEP_ARTIFACT, offset)?,
            artifact.participant_id,
            WorkUnitRole::Artifact,
            artifact.requirement,
        )?;
        for readiness in &readiness_keys {
            builder.add_edge(*readiness, artifact_key)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation_journal::{
        CopyCollisionPolicy, CopyEndpoint, CopyIntentArgs, CopySourceExpectation,
        DecodedEditIntentArgs, DecodedEditOperationArgs, DecodedExportBatchIntentArgs,
        DecodedExportBatchOutput, DecodedExportChannelBehavior, DecodedExportDither,
        DecodedExportIntentArgs, DecodedExportOutputPolicy, DecodedExportRegistrationMetadata,
        DecodedExportSampleFormat, DecodedExportSampleRate, ExternalRootBindingId, FiniteF32Bits,
        NamespaceCommandArgs, NamespaceDirectoryEndpoint, NamespaceDirectoryExpectation,
        NamespaceDirectoryLocator, NamespaceEntryEndpoint, NamespaceEntryExpectation,
        NamespaceFileExpectation, NamespaceLeafName, NamespaceMoveIntentArgs,
        NamespaceRenameIntentArgs, NamespaceTarget, ParticipantCheckpoint,
        ParticipantCheckpointState, ParticipantDescriptor, RawWavExtractionRecipe,
        RemovalCommandArgs, RetainedDeleteId, RetainedFolderObservation, RetainedPurgeDecision,
        RetainedPurgeIntentArgs, RetainedRecoveryRootKind, RetainedResolutionCommandArgs,
        RetainedResolutionStage, SelectionExtractIntentArgs, SelectionOutputRecipe,
        StageRetainedFolderIntentArgs, StoredRelativePath, StoredSourceFence, TrashIntentArgs,
        WaveformEditFrameRange, WaveformFileRevisionObservation, WaveformOutputSpec,
        WaveformSampleFormat, WaveformSourceSpec, fingerprint_selected_entry_json,
    };
    use crate::operation_journal::{
        HistoryAction, HistoryDirection, HistoryFolderMove, HistoryFolderRelocation,
        HistoryScopeId, HistoryUndoRedoIntentArgs,
    };
    use crate::sample_sources::SourcePlaybackType;
    use uuid::Uuid;

    fn valid_topology() -> (ParticipantSet, ParticipantId, ParticipantId, SourceId) {
        let source_participant = ParticipantId::from_uuid(Uuid::from_u128(1));
        let filesystem = ParticipantId::from_uuid(Uuid::from_u128(2));
        let global = ParticipantId::from_uuid(Uuid::from_u128(3));
        let harvest = ParticipantId::from_uuid(Uuid::from_u128(4));
        let projection = ParticipantId::from_uuid(Uuid::from_u128(5));
        let readiness = ParticipantId::from_uuid(Uuid::from_u128(6));
        let source_id = SourceId::from_string("source-a");
        let fence =
            StoredSourceFence::new(source_id.clone(), "root-a", "db-a", 1, 7, 1).expect("fence");
        let descriptors = vec![
            ParticipantDescriptor::new(
                source_participant,
                ParticipantKind::SourceDatabase,
                ParticipantRequirement::Required,
                Some(fence.clone()),
                None,
            )
            .expect("source"),
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
                projection,
                ParticipantKind::Projection,
                ParticipantRequirement::Required,
                Some(fence.clone()),
                None,
            )
            .expect("projection"),
            ParticipantDescriptor::new(
                readiness,
                ParticipantKind::Readiness,
                ParticipantRequirement::Required,
                Some(fence),
                None,
            )
            .expect("readiness"),
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
                .expect("checkpoint")
            })
            .collect();
        (
            ParticipantSet::new(descriptors, checkpoints).expect("participants"),
            source_participant,
            filesystem,
            source_id,
        )
    }

    fn copy_args(source_participant: ParticipantId, source_id: SourceId) -> UniversalCommandArgs {
        let source = CopyEndpoint::new(
            source_participant,
            source_id.clone(),
            StoredRelativePath::new("source.wav").expect("path"),
        )
        .expect("source endpoint");
        let destination = CopyEndpoint::new(
            source_participant,
            source_id,
            StoredRelativePath::new("copy.wav").expect("path"),
        )
        .expect("destination endpoint");
        UniversalCommandArgs::Copy(
            CopyIntentArgs::new(
                source,
                destination,
                CopySourceExpectation::new(7, "file-id", 12, 13).expect("source expectation"),
                CopyCollisionPolicy::NoReplace,
                None,
            )
            .expect("copy"),
        )
    }

    fn two_source_topology() -> (
        ParticipantSet,
        ParticipantId,
        SourceId,
        ParticipantId,
        SourceId,
        ParticipantId,
    ) {
        let source_a = ParticipantId::from_uuid(Uuid::from_u128(11));
        let source_b = ParticipantId::from_uuid(Uuid::from_u128(12));
        let filesystem = ParticipantId::from_uuid(Uuid::from_u128(13));
        let global = ParticipantId::from_uuid(Uuid::from_u128(14));
        let harvest = ParticipantId::from_uuid(Uuid::from_u128(15));
        let projection_a = ParticipantId::from_uuid(Uuid::from_u128(16));
        let readiness_a = ParticipantId::from_uuid(Uuid::from_u128(17));
        let projection_b = ParticipantId::from_uuid(Uuid::from_u128(18));
        let readiness_b = ParticipantId::from_uuid(Uuid::from_u128(19));
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
                Some(fence_a),
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
                Some(fence_b),
                None,
            )
            .expect("readiness B"),
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
                .expect("checkpoint")
            })
            .collect();
        (
            ParticipantSet::new(descriptors, checkpoints).expect("participants"),
            source_a,
            source_a_id,
            source_b,
            source_b_id,
            filesystem,
        )
    }

    fn history_root(source: ParticipantId, source_id: SourceId) -> NamespaceDirectoryEndpoint {
        NamespaceDirectoryEndpoint::new(
            source,
            source_id,
            NamespaceDirectoryLocator::Root,
            NamespaceDirectoryExpectation::new(7, "root-a", None).expect("root expectation"),
        )
        .expect("history root")
    }

    fn history_relocation(
        source: ParticipantId,
        source_id: SourceId,
        source_path: &str,
        source_identity: &str,
        destination_path: &str,
    ) -> HistoryFolderRelocation {
        let source_entry = NamespaceEntryEndpoint::new(
            source,
            source_id.clone(),
            StoredRelativePath::new(source_path).expect("source path"),
            NamespaceEntryExpectation::Directory(
                NamespaceDirectoryExpectation::new(7, source_identity, None)
                    .expect("source directory"),
            ),
        )
        .expect("source entry");
        let destination = CopyEndpoint::new(
            source,
            source_id,
            StoredRelativePath::new(destination_path).expect("destination path"),
        )
        .expect("destination");
        HistoryFolderRelocation::new(source_entry, destination).expect("history relocation")
    }

    #[test]
    fn sparse_topology_is_rejected() {
        let (participants, source, _filesystem, source_id) = valid_topology();
        let sparse = ParticipantSet::new(
            participants.descriptors().iter().take(2).cloned().collect(),
            participants.checkpoints().iter().take(2).cloned().collect(),
        )
        .expect("sparse participant shape");
        let error = derive_work_plan(&copy_args(source, source_id), &sparse).expect_err("sparse");
        assert!(matches!(error, WorkPlanError::IncompleteTopology(_)));
    }

    #[test]
    fn plan_is_deterministic_and_revalidates() {
        let (participants, source, _filesystem, source_id) = valid_topology();
        let args = copy_args(source, source_id);
        let first = derive_work_plan(&args, &participants).expect("plan");
        let second = derive_work_plan(&args, &participants).expect("plan");
        assert_eq!(first, second);
        validate_work_plan(&first, &args, &participants).expect("revalidation");
        assert!(first.units().iter().any(|unit| {
            unit.key().role() == WorkUnitRole::SourceRead && unit.key().participant_id() == source
        }));
    }

    #[test]
    fn direct_workflow_marks_staging_steps_not_applicable() {
        let (participants, source, filesystem, source_id) = valid_topology();
        let parent = NamespaceDirectoryEndpoint::new(
            source,
            source_id.clone(),
            NamespaceDirectoryLocator::Root,
            NamespaceDirectoryExpectation::new(7, "root-a", Some(1)).expect("parent"),
        )
        .expect("parent endpoint");
        let source_entry = NamespaceEntryEndpoint::new(
            source,
            source_id,
            StoredRelativePath::new("old.wav").expect("source path"),
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "file-a", 12, Some(2)).expect("file"),
            ),
        )
        .expect("source endpoint");
        let destination =
            NamespaceTarget::new(parent, NamespaceLeafName::new("new.wav").expect("leaf"))
                .expect("destination");
        let args = UniversalCommandArgs::Namespace(NamespaceCommandArgs::Rename(
            NamespaceRenameIntentArgs::new(source_entry, destination).expect("rename"),
        ));
        let key = WorkUnitKey::new(0, STEP_STAGE, filesystem, WorkUnitRole::Stage);
        let plan = derive_work_plan(&args, &participants).expect("plan");
        assert_eq!(
            plan.units()
                .iter()
                .find(|unit| unit.key() == key)
                .expect("staged unit")
                .applicability(),
            WorkUnitApplicability::NotApplicable
        );
        let copy_plan = derive_work_plan(
            &copy_args(source, SourceId::from_string("source-a")),
            &participants,
        )
        .expect("staged plan");
        assert_eq!(
            copy_plan
                .units()
                .iter()
                .find(|unit| unit.key() == key)
                .expect("staged unit")
                .applicability(),
            WorkUnitApplicability::Required
        );
    }

    #[test]
    fn directory_move_is_direct_but_same_source_file_move_has_origin_checkpoints() {
        let (participants, source, filesystem, source_id) = valid_topology();
        let destination_parent = NamespaceDirectoryEndpoint::new(
            source,
            source_id.clone(),
            NamespaceDirectoryLocator::Child(
                StoredRelativePath::new("archive").expect("destination path"),
            ),
            NamespaceDirectoryExpectation::new(7, "archive", None).expect("destination parent"),
        )
        .expect("destination endpoint");

        let directory_source = NamespaceEntryEndpoint::new(
            source,
            source_id.clone(),
            StoredRelativePath::new("folder").expect("directory path"),
            NamespaceEntryExpectation::Directory(
                NamespaceDirectoryExpectation::new(7, "folder", None)
                    .expect("directory expectation"),
            ),
        )
        .expect("directory source");
        let directory_args = UniversalCommandArgs::Namespace(NamespaceCommandArgs::Move(
            NamespaceMoveIntentArgs::new(directory_source, destination_parent.clone())
                .expect("directory move"),
        ));
        let directory_plan =
            derive_work_plan(&directory_args, &participants).expect("directory move plan");
        for role in [WorkUnitRole::Stage, WorkUnitRole::CopyValidated] {
            assert_eq!(
                directory_plan
                    .units()
                    .iter()
                    .find(|unit| {
                        unit.key()
                            == WorkUnitKey::new(
                                0,
                                if role == WorkUnitRole::Stage {
                                    STEP_STAGE
                                } else {
                                    STEP_COPY_VALIDATED
                                },
                                filesystem,
                                role,
                            )
                    })
                    .expect("directory applicability")
                    .applicability(),
                WorkUnitApplicability::NotApplicable
            );
        }
        let qualified_relocation = WorkUnitKey::new(
            0,
            STEP_QUALIFIED_RELOCATION,
            filesystem,
            WorkUnitRole::QualifiedRelocation,
        );
        let origin_absence = WorkUnitKey::new(
            0,
            STEP_RETAINED_ABSENCE,
            filesystem,
            WorkUnitRole::OriginAbsence,
        );
        let source_affect =
            WorkUnitKey::new(0, STEP_SOURCE_AFFECT, source, WorkUnitRole::SourceAffect);
        for (prerequisite, dependent) in [
            (qualified_relocation, origin_absence),
            (origin_absence, source_affect),
        ] {
            assert!(
                directory_plan
                    .prerequisites()
                    .contains(&WorkPrerequisite::new(prerequisite, dependent))
            );
        }

        let file_source = NamespaceEntryEndpoint::new(
            source,
            source_id.clone(),
            StoredRelativePath::new("file.wav").expect("file path"),
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "file", 16, Some(2)).expect("file expectation"),
            ),
        )
        .expect("file source");
        let file_args = UniversalCommandArgs::Namespace(NamespaceCommandArgs::Move(
            NamespaceMoveIntentArgs::new(file_source, destination_parent).expect("file move"),
        ));
        let file_plan = derive_work_plan(&file_args, &participants).expect("file move plan");
        for role in [WorkUnitRole::Stage, WorkUnitRole::CopyValidated] {
            let step_ordinal = if role == WorkUnitRole::Stage {
                STEP_STAGE
            } else {
                STEP_COPY_VALIDATED
            };
            assert_eq!(
                file_plan
                    .units()
                    .iter()
                    .find(|unit| {
                        unit.key() == WorkUnitKey::new(0, step_ordinal, filesystem, role)
                    })
                    .expect("file applicability")
                    .applicability(),
                WorkUnitApplicability::Required
            );
        }
        let publish = WorkUnitKey::new(0, STEP_PUBLISH, filesystem, WorkUnitRole::Publish);
        let destination_reconcile = WorkUnitKey::new(
            0,
            STEP_DESTINATION_RECONCILE,
            source,
            WorkUnitRole::DestinationSourceReconcile,
        );
        let removal_start = WorkUnitKey::new(
            0,
            STEP_ORIGIN_REMOVAL_START,
            filesystem,
            WorkUnitRole::OriginRemovalStart,
        );
        let absence = WorkUnitKey::new(
            0,
            STEP_ORIGIN_ABSENCE,
            filesystem,
            WorkUnitRole::OriginAbsence,
        );
        let origin_reconcile = WorkUnitKey::new(
            0,
            STEP_ORIGIN_RECONCILE,
            source,
            WorkUnitRole::OriginSourceReconcile,
        );
        for (prerequisite, dependent) in [
            (publish, destination_reconcile),
            (destination_reconcile, removal_start),
            (removal_start, absence),
            (absence, origin_reconcile),
        ] {
            assert!(
                file_plan
                    .prerequisites()
                    .contains(&WorkPrerequisite::new(prerequisite, dependent))
            );
        }
    }

    #[test]
    fn removal_recipes_require_retention_and_selected_purge_absence() {
        let (participants, source, filesystem, source_id) = valid_topology();
        let assert_chain = |plan: &WorkPlan, keys: &[WorkUnitKey]| {
            for pair in keys.windows(2) {
                assert!(
                    plan.prerequisites()
                        .contains(&WorkPrerequisite::new(pair[0], pair[1])),
                    "missing work-plan edge {:?} -> {:?}",
                    pair[0],
                    pair[1]
                );
            }
        };

        let trash_source = NamespaceEntryEndpoint::new(
            source,
            source_id.clone(),
            StoredRelativePath::new("trash-me.wav").expect("trash source path"),
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "trash-file", 24, Some(3))
                    .expect("trash source expectation"),
            ),
        )
        .expect("trash source endpoint");
        let trash_args = UniversalCommandArgs::Removal(RemovalCommandArgs::Trash(
            TrashIntentArgs::new(
                trash_source,
                ExternalRootBindingId::from_uuid(Uuid::from_u128(101)),
                filesystem,
                "trash-root",
            )
            .expect("trash arguments"),
        ));
        let trash_plan = derive_work_plan(&trash_args, &participants).expect("trash plan");
        assert_chain(
            &trash_plan,
            &[
                WorkUnitKey::new(0, STEP_SOURCE_READ, source, WorkUnitRole::SourceRead),
                WorkUnitKey::new(0, STEP_PREPARE, filesystem, WorkUnitRole::Prepare),
                WorkUnitKey::new(
                    0,
                    STEP_RESTORE_RECORD,
                    filesystem,
                    WorkUnitRole::RestoreRecordPrepared,
                ),
                WorkUnitKey::new(
                    0,
                    STEP_DIRECT_REMOVAL_INTENT,
                    filesystem,
                    WorkUnitRole::OriginRemovalStart,
                ),
                WorkUnitKey::new(
                    0,
                    STEP_QUALIFIED_RELOCATION,
                    filesystem,
                    WorkUnitRole::QualifiedRelocation,
                ),
                WorkUnitKey::new(
                    0,
                    STEP_RETAINED_PAYLOAD,
                    filesystem,
                    WorkUnitRole::RetainedPayloadVerified,
                ),
                WorkUnitKey::new(
                    0,
                    STEP_RETAINED_ABSENCE,
                    filesystem,
                    WorkUnitRole::OriginAbsence,
                ),
                WorkUnitKey::new(0, STEP_SOURCE_AFFECT, source, WorkUnitRole::SourceAffect),
            ],
        );

        let retained_source = NamespaceEntryEndpoint::new(
            source,
            source_id.clone(),
            StoredRelativePath::new("retained-folder").expect("retained source path"),
            NamespaceEntryExpectation::Directory(
                NamespaceDirectoryExpectation::new(7, "retained-folder", None)
                    .expect("retained source expectation"),
            ),
        )
        .expect("retained source endpoint");
        let retain_args = UniversalCommandArgs::Removal(RemovalCommandArgs::StageRetainedFolder(
            StageRetainedFolderIntentArgs::new(retained_source).expect("retained-folder arguments"),
        ));
        let retain_plan = derive_work_plan(&retain_args, &participants).expect("retain plan");
        assert_chain(
            &retain_plan,
            &[
                WorkUnitKey::new(0, STEP_SOURCE_READ, source, WorkUnitRole::SourceRead),
                WorkUnitKey::new(0, STEP_PREPARE, filesystem, WorkUnitRole::Prepare),
                WorkUnitKey::new(
                    0,
                    STEP_RESTORE_RECORD,
                    filesystem,
                    WorkUnitRole::RestoreRecordPrepared,
                ),
                WorkUnitKey::new(
                    0,
                    STEP_ENSURE_PARENTS,
                    filesystem,
                    WorkUnitRole::EnsureContainedParentDirectories,
                ),
                WorkUnitKey::new(
                    0,
                    STEP_DIRECT_REMOVAL_INTENT,
                    filesystem,
                    WorkUnitRole::OriginRemovalStart,
                ),
                WorkUnitKey::new(
                    0,
                    STEP_QUALIFIED_RELOCATION,
                    filesystem,
                    WorkUnitRole::QualifiedRelocation,
                ),
                WorkUnitKey::new(
                    0,
                    STEP_RETAINED_PAYLOAD,
                    filesystem,
                    WorkUnitRole::RetainedPayloadVerified,
                ),
                WorkUnitKey::new(
                    0,
                    STEP_RETAINED_ABSENCE,
                    filesystem,
                    WorkUnitRole::OriginAbsence,
                ),
                WorkUnitKey::new(0, STEP_SOURCE_AFFECT, source, WorkUnitRole::SourceAffect),
            ],
        );

        let staged = NamespaceEntryEndpoint::new(
            source,
            source_id.clone(),
            StoredRelativePath::new(".yield_delete_staging/drums/kick").expect("staged path"),
            NamespaceEntryExpectation::Directory(
                NamespaceDirectoryExpectation::new(7, "staged-directory", None)
                    .expect("staged expectation"),
            ),
        )
        .expect("staged endpoint");
        let observation = RetainedFolderObservation::new(
            StoredRelativePath::new("drums/kick").expect("original path"),
            staged,
            StoredRelativePath::new("drums/kick").expect("staged relative"),
            RetainedDeleteId::new(Uuid::from_u128(102)).expect("retained ID"),
            RetainedResolutionStage::Deleted,
            RetainedRecoveryRootKind::LegacyDeleteStaging,
            fingerprint_selected_entry_json(br#"{"id":"retained"}"#)
                .expect("selected entry fingerprint"),
        )
        .expect("retained observation");
        let purge_args =
            UniversalCommandArgs::RetainedResolution(RetainedResolutionCommandArgs::Purge(
                RetainedPurgeIntentArgs::new(observation, RetainedPurgeDecision::UserConfirmed)
                    .expect("purge arguments"),
            ));
        let purge_plan = derive_work_plan(&purge_args, &participants).expect("purge plan");
        assert_chain(
            &purge_plan,
            &[
                WorkUnitKey::new(0, STEP_SOURCE_READ, source, WorkUnitRole::SourceRead),
                WorkUnitKey::new(0, STEP_PREPARE, filesystem, WorkUnitRole::Prepare),
                WorkUnitKey::new(
                    0,
                    STEP_QUALIFIED_RELOCATION,
                    filesystem,
                    WorkUnitRole::SelectedRetainedObjectDisposition,
                ),
                WorkUnitKey::new(
                    0,
                    STEP_RETAINED_PAYLOAD,
                    filesystem,
                    WorkUnitRole::RetainedObjectAbsence,
                ),
                WorkUnitKey::new(0, STEP_SOURCE_AFFECT, source, WorkUnitRole::SourceAffect),
            ],
        );
        assert!(
            !purge_plan
                .units()
                .iter()
                .any(|unit| unit.key().role() == WorkUnitRole::OriginAbsence)
        );
    }

    #[test]
    fn cross_source_file_move_reconciles_destination_before_origin_removal() {
        let (participants, source_a, source_a_id, source_b, source_b_id, filesystem) =
            two_source_topology();
        let source = NamespaceEntryEndpoint::new(
            source_a,
            source_a_id,
            StoredRelativePath::new("source.wav").expect("source path"),
            NamespaceEntryExpectation::RegularFile(
                NamespaceFileExpectation::new(7, "source-file", 32, Some(4))
                    .expect("source expectation"),
            ),
        )
        .expect("source endpoint");
        let destination_parent = NamespaceDirectoryEndpoint::new(
            source_b,
            source_b_id,
            NamespaceDirectoryLocator::Root,
            NamespaceDirectoryExpectation::new(8, "root-b", None).expect("destination parent"),
        )
        .expect("destination endpoint");
        let args = UniversalCommandArgs::Namespace(NamespaceCommandArgs::Move(
            NamespaceMoveIntentArgs::new(source, destination_parent).expect("cross-source move"),
        ));
        let plan = derive_work_plan(&args, &participants).expect("cross-source move plan");

        let publish = WorkUnitKey::new(0, STEP_PUBLISH, filesystem, WorkUnitRole::Publish);
        let destination_reconcile = WorkUnitKey::new(
            0,
            STEP_DESTINATION_RECONCILE,
            source_b,
            WorkUnitRole::DestinationSourceReconcile,
        );
        let removal_start = WorkUnitKey::new(
            0,
            STEP_ORIGIN_REMOVAL_START,
            filesystem,
            WorkUnitRole::OriginRemovalStart,
        );
        let absence = WorkUnitKey::new(
            0,
            STEP_ORIGIN_ABSENCE,
            filesystem,
            WorkUnitRole::OriginAbsence,
        );
        let origin_reconcile = WorkUnitKey::new(
            0,
            STEP_ORIGIN_RECONCILE,
            source_a,
            WorkUnitRole::OriginSourceReconcile,
        );
        for (prerequisite, dependent) in [
            (publish, destination_reconcile),
            (destination_reconcile, removal_start),
            (removal_start, absence),
            (absence, origin_reconcile),
        ] {
            assert!(
                plan.prerequisites()
                    .contains(&WorkPrerequisite::new(prerequisite, dependent))
            );
        }
    }

    #[test]
    fn history_preserves_repeated_relocation_roles_and_action_order() {
        let (participants, source, _filesystem, source_id) = valid_topology();
        let root = history_root(source, source_id.clone());
        let first_action = HistoryAction::FolderMove(
            HistoryFolderMove::new(
                root.clone(),
                vec![
                    history_relocation(
                        source,
                        source_id.clone(),
                        "folder/a",
                        "folder-a",
                        "archive/a",
                    ),
                    history_relocation(
                        source,
                        source_id.clone(),
                        "folder/b",
                        "folder-b",
                        "archive/b",
                    ),
                ],
            )
            .expect("first history action"),
        );
        let second_action = HistoryAction::FolderMove(
            HistoryFolderMove::new(
                root,
                vec![
                    history_relocation(
                        source,
                        source_id.clone(),
                        "archive/a",
                        "archive-a",
                        "restored/a",
                    ),
                    history_relocation(
                        source,
                        source_id.clone(),
                        "archive/b",
                        "archive-b",
                        "restored/b",
                    ),
                ],
            )
            .expect("second history action"),
        );
        let history = HistoryUndoRedoIntentArgs::new(
            HistoryScopeId::from_uuid(Uuid::from_u128(41)),
            1,
            2,
            None,
            HistoryDirection::Undo,
            vec![first_action, second_action],
        )
        .expect("history arguments");
        let args = UniversalCommandArgs::History(history);
        let plan = derive_work_plan(&args, &participants).expect("history plan");

        assert_eq!(
            plan.units()
                .iter()
                .filter(|unit| unit.key().role() == WorkUnitRole::SourceRead)
                .count(),
            4
        );
        let first_relocation_tail =
            WorkUnitKey::new(0, STEP_SOURCE_AFFECT, source, WorkUnitRole::SourceAffect);
        let second_relocation_first = WorkUnitKey::new(
            0,
            STEP_HISTORY_STRIDE + STEP_SOURCE_READ,
            source,
            WorkUnitRole::SourceRead,
        );
        let second_action_first =
            WorkUnitKey::new(1, STEP_SOURCE_READ, source, WorkUnitRole::SourceRead);
        let second_relocation_tail = WorkUnitKey::new(
            0,
            STEP_HISTORY_STRIDE + STEP_SOURCE_AFFECT,
            source,
            WorkUnitRole::SourceAffect,
        );
        let second_action_relocation_first = WorkUnitKey::new(
            1,
            STEP_HISTORY_STRIDE + STEP_SOURCE_READ,
            source,
            WorkUnitRole::SourceRead,
        );
        for (prerequisite, dependent) in [
            (first_relocation_tail, second_relocation_first),
            (second_relocation_tail, second_action_first),
            (
                WorkUnitKey::new(1, STEP_SOURCE_AFFECT, source, WorkUnitRole::SourceAffect),
                second_action_relocation_first,
            ),
        ] {
            assert!(
                plan.prerequisites()
                    .contains(&WorkPrerequisite::new(prerequisite, dependent))
            );
        }
    }

    #[test]
    fn topology_rejects_mismatched_unused_and_source_fenced_filesystem_bindings() {
        let (participants, source, filesystem, source_id) = valid_topology();
        let mismatched = derive_work_plan(
            &copy_args(source, SourceId::from_string("other-source")),
            &participants,
        );
        assert!(matches!(
            mismatched,
            Err(WorkPlanError::CommandArguments(_))
        ));

        let (multi, source_a, source_a_id, source_b, _source_b_id, _filesystem) =
            two_source_topology();
        let unused = derive_work_plan(&copy_args(source_a, source_a_id), &multi);
        assert!(matches!(
            unused,
            Err(WorkPlanError::UnusedParticipant { participant_id }) if participant_id == source_b
        ));

        let fenced_filesystem_descriptors = participants
            .descriptors()
            .iter()
            .map(|descriptor| {
                if descriptor.id() == filesystem {
                    ParticipantDescriptor::new(
                        descriptor.id(),
                        descriptor.kind(),
                        descriptor.requirement(),
                        Some(
                            StoredSourceFence::new(
                                source_id.clone(),
                                "filesystem-root",
                                "filesystem-db",
                                1,
                                7,
                                1,
                            )
                            .expect("filesystem fence"),
                        ),
                        None,
                    )
                    .expect("fenced filesystem descriptor")
                } else {
                    descriptor.clone()
                }
            })
            .collect();
        let fenced_filesystem = ParticipantSet::new(
            fenced_filesystem_descriptors,
            participants.checkpoints().to_vec(),
        )
        .expect("fenced filesystem inventory");
        assert!(matches!(
            derive_work_plan(&copy_args(source, source_id), &fenced_filesystem),
            Err(WorkPlanError::AmbiguousTopology(
                "the global Filesystem coordinator must be unfenced"
            ))
        ));
    }

    #[test]
    fn frozen_validation_rejects_tampering_version_and_hard_bounds() {
        let (participants, source, _filesystem, source_id) = valid_topology();
        let args = copy_args(source, source_id);
        let plan = derive_work_plan(&args, &participants).expect("plan");

        let mut missing_unit = plan.clone();
        missing_unit.units.pop();
        assert!(validate_work_plan(&missing_unit, &args, &participants).is_err());

        let mut missing_edge = plan.clone();
        missing_edge.prerequisites.pop();
        assert!(matches!(
            validate_work_plan(&missing_edge, &args, &participants),
            Err(WorkPlanError::PlanMismatch)
        ));

        let mut wrong_version = plan.clone();
        wrong_version.plan_version = WORK_PLAN_VERSION_V1 + 1;
        assert!(matches!(
            validate_work_plan(&wrong_version, &args, &participants),
            Err(WorkPlanError::UnsupportedPlanVersion { actual }) if actual == WORK_PLAN_VERSION_V1 + 1
        ));

        let unit = WorkUnit {
            key: WorkUnitKey::new(0, 0, source, WorkUnitRole::SourceRead),
            requirement: ParticipantRequirement::Required,
            applicability: WorkUnitApplicability::Required,
        };
        let oversized_units = WorkPlan {
            plan_version: WORK_PLAN_VERSION_V1,
            units: vec![unit.clone(); MAX_WORK_PLAN_UNITS + 1],
            prerequisites: Vec::new(),
        };
        assert!(matches!(
            oversized_units.validate(),
            Err(WorkPlanError::TooManyUnits { .. })
        ));

        let unit_key = unit.key;
        let oversized_edges = WorkPlan {
            plan_version: WORK_PLAN_VERSION_V1,
            units: vec![unit],
            prerequisites: vec![WorkPrerequisite::new(unit_key, unit_key); MAX_WORK_PLAN_EDGES + 1],
        };
        assert!(matches!(
            oversized_edges.validate(),
            Err(WorkPlanError::TooManyEdges { .. })
        ));
    }

    #[test]
    fn compound_extraction_publish_precedes_trim_and_keeps_recovery_obligation() {
        let (participants, source, filesystem, source_id) = valid_topology();
        let source_endpoint = CopyEndpoint::new(
            source,
            source_id.clone(),
            StoredRelativePath::new("source.wav").expect("source path"),
        )
        .expect("source endpoint");
        let destination_endpoint = CopyEndpoint::new(
            source,
            source_id,
            StoredRelativePath::new("selected.wav").expect("destination path"),
        )
        .expect("destination endpoint");
        let source_spec = WaveformSourceSpec::new(2, 48_000, 16, WaveformSampleFormat::Integer)
            .expect("source spec");
        let extraction = SelectionExtractIntentArgs::new(
            source_endpoint,
            7,
            WaveformFileRevisionObservation::new("file", "change", 256, Some(3))
                .expect("file revision"),
            source_spec,
            10,
            WaveformEditFrameRange::new(2, 8).expect("selection range"),
            SelectionOutputRecipe::RawWav(
                RawWavExtractionRecipe::new(16, FiniteF32Bits::new(1.0).expect("gain"), 1)
                    .expect("raw recipe"),
            ),
            SourcePlaybackType::OneShot,
            destination_endpoint,
            CopyCollisionPolicy::NoReplace,
        )
        .expect("extraction");
        let args =
            UniversalCommandArgs::SelectionOutput(SelectionOutputCommandArgs::ExtractAndTrim(
                ExtractAndTrimIntentArgs::new(extraction, WaveformOutputSpec::float32(2, 48_000))
                    .expect("compound arguments"),
            ));

        let plan = derive_work_plan(&args, &participants).expect("compound plan");
        let extraction_publish =
            WorkUnitKey::new(0, STEP_PUBLISH, filesystem, WorkUnitRole::Publish);
        let trim_source_read =
            WorkUnitKey::new(1, STEP_SOURCE_READ, source, WorkUnitRole::SourceRead);
        assert!(
            plan.prerequisites()
                .contains(&WorkPrerequisite::new(extraction_publish, trim_source_read,))
        );
        assert!(plan.units().iter().any(|unit| {
            unit.key()
                == WorkUnitKey::new(
                    1,
                    STEP_RECOVERY_MATERIAL,
                    filesystem,
                    WorkUnitRole::RecoveryMaterialRetained,
                )
        }));
    }

    #[test]
    fn decoded_edit_uses_the_in_place_recovery_recipe() {
        let (participants, source, filesystem, source_id) = valid_topology();
        let endpoint = CopyEndpoint::new(
            source,
            source_id,
            StoredRelativePath::new("source.wav").expect("source path"),
        )
        .expect("source endpoint");
        let args = UniversalCommandArgs::DecodedEdit(
            DecodedEditIntentArgs::new(
                endpoint,
                WaveformFileRevisionObservation::new("file", "change", 256, Some(3))
                    .expect("file revision"),
                WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float)
                    .expect("source spec"),
                100,
                DecodedEditOperationArgs::click_repair(
                    WaveformEditFrameRange::new(1, 99).expect("repair range"),
                ),
                WaveformOutputSpec::float32(2, 48_000),
            )
            .expect("decoded edit args"),
        );
        let plan = derive_work_plan(&args, &participants).expect("decoded edit plan");
        plan.validate().expect("decoded edit plan validates");
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::RecoveryMaterialRetained,
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
            WorkUnitRole::SourceAffect,
        ] {
            assert!(plan.units().iter().any(|unit| unit.key().role() == role));
        }
        assert!(plan.units().iter().any(|unit| {
            unit.key().role() == WorkUnitRole::RecoveryMaterialRetained
                && unit.key().participant_id() == filesystem
        }));
    }

    #[test]
    fn decoded_export_uses_the_selection_extract_recipe() {
        let (participants, source, filesystem, source_id) = valid_topology();
        let source_endpoint = CopyEndpoint::new(
            source,
            source_id.clone(),
            StoredRelativePath::new("source.wav").expect("source path"),
        )
        .expect("source endpoint");
        let destination_endpoint = CopyEndpoint::new(
            source,
            source_id,
            StoredRelativePath::new("selection.wav").expect("destination path"),
        )
        .expect("destination endpoint");
        let args = UniversalCommandArgs::DecodedExport(
            DecodedExportIntentArgs::new(
                source_endpoint,
                destination_endpoint,
                7,
                WaveformFileRevisionObservation::new("file", "change", 256, Some(3))
                    .expect("file revision"),
                WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float)
                    .expect("source spec"),
                100,
                WaveformEditFrameRange::new(10, 40).expect("selection range"),
                DecodedExportOutputPolicy::new(
                    DecodedExportSampleRate::Hz(44_100),
                    DecodedExportSampleFormat::Pcm24,
                    DecodedExportChannelBehavior::PreserveMonoStereo,
                    DecodedExportDither::None,
                ),
                2,
                DecodedExportRegistrationMetadata::from_reference(Some(-2), true, Some(123.25))
                    .expect("registration"),
                CopyCollisionPolicy::NoReplace,
            )
            .expect("decoded export arguments"),
        );
        let plan = derive_work_plan(&args, &participants).expect("decoded export plan");
        plan.validate().expect("decoded export plan validates");
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
            WorkUnitRole::GlobalLibrary,
            WorkUnitRole::Harvest,
            WorkUnitRole::Projection,
            WorkUnitRole::Readiness,
        ] {
            assert!(plan.units().iter().any(|unit| unit.key().role() == role));
        }
        assert!(
            !plan
                .units()
                .iter()
                .any(|unit| { unit.key().role() == WorkUnitRole::RecoveryMaterialRetained })
        );
        assert!(plan.units().iter().any(|unit| {
            unit.key().role() == WorkUnitRole::Publish && unit.key().participant_id() == filesystem
        }));
        assert!(plan.units().iter().any(|unit| {
            unit.key().role() == WorkUnitRole::SourceAffect && unit.key().participant_id() == source
        }));
    }

    #[test]
    fn decoded_export_batch_keeps_output_spans_independent_and_fans_into_global() {
        let (participants, source, _filesystem, source_id) = valid_topology();
        let source_endpoint = CopyEndpoint::new(
            source,
            source_id.clone(),
            StoredRelativePath::new("source.wav").expect("source path"),
        )
        .expect("source endpoint");
        let outputs = ["a.wav", "b.wav"]
            .into_iter()
            .enumerate()
            .map(|(index, path)| {
                DecodedExportBatchOutput::new(
                    WaveformEditFrameRange::new(10 + index as u64, 40 + index as u64)
                        .expect("range"),
                    CopyEndpoint::new(
                        source,
                        source_id.clone(),
                        StoredRelativePath::new(path).expect("destination path"),
                    )
                    .expect("destination endpoint"),
                )
                .expect("batch output")
            })
            .collect();
        let batch = DecodedExportBatchIntentArgs::from_reference(
            source_endpoint,
            7,
            WaveformFileRevisionObservation::new("file", "change", 256, Some(3))
                .expect("file revision"),
            WaveformSourceSpec::new(2, 48_000, 32, WaveformSampleFormat::Float)
                .expect("source spec"),
            100,
            outputs,
            DecodedExportOutputPolicy::default(),
            None,
            4,
        )
        .expect("decoded export batch arguments");
        let args = UniversalCommandArgs::DecodedExportBatch(batch);
        let plan = derive_work_plan(&args, &participants).expect("decoded export batch plan");
        plan.validate()
            .expect("decoded export batch plan validates");

        let first_tail =
            WorkUnitKey::new(0, STEP_SOURCE_AFFECT, source, WorkUnitRole::SourceAffect);
        let second_tail =
            WorkUnitKey::new(1, STEP_SOURCE_AFFECT, source, WorkUnitRole::SourceAffect);
        let global = WorkUnitKey::new(
            GLOBAL_ACTION_ORDINAL,
            STEP_GLOBAL_LIBRARY,
            ParticipantId::from_uuid(Uuid::from_u128(3)),
            WorkUnitRole::GlobalLibrary,
        );
        assert!(
            plan.prerequisites()
                .contains(&WorkPrerequisite::new(first_tail, global,))
        );
        assert!(
            plan.prerequisites()
                .contains(&WorkPrerequisite::new(second_tail, global,))
        );
        assert!(!plan.prerequisites().contains(&WorkPrerequisite::new(
            first_tail,
            WorkUnitKey::new(1, STEP_SOURCE_READ, source, WorkUnitRole::SourceRead),
        )));
        assert_eq!(
            plan.units()
                .iter()
                .filter(|unit| unit.key().role() == WorkUnitRole::Publish)
                .count(),
            2
        );
    }
}
