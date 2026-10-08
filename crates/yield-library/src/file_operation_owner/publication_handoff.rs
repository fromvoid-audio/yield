//! Original private queue custody. Unsupported publication never enters a namespace gate.
use super::*;

pub(super) enum QueuedGrant {
    Chunk(LiveChunkEffectGrant),
    Publication(StagingPublicationGrant),
}
impl QueuedGrant {
    pub(super) fn original_worker_started(&self) {
        if let Self::Publication(grant) = self {
            grant.original_worker_started();
        }
    }
    pub(super) fn retain_worker_completion(
        &self,
        result: &FileOperationResult,
        worker: OwnerIdentity,
    ) {
        if let Self::Publication(grant) = self {
            grant.retain_worker_completion(result, worker);
        }
    }
    pub(super) fn into_chunk(self) -> Option<LiveChunkEffectGrant> {
        match self {
            Self::Chunk(grant) => Some(grant),
            Self::Publication(_) => None,
        }
    }
    pub(super) fn matches_command(&self, command: &FileOperationCommand) -> bool {
        match self {
            Self::Chunk(grant) => grant.matches_command(command),
            Self::Publication(grant) => grant
                .original_target()
                .publication_command()
                .and_then(|expected| expected.canonical_bytes())
                .ok()
                .zip(command.canonical_bytes().ok())
                .is_some_and(|(expected, actual)| expected == actual),
        }
    }
    pub(super) fn queued_backing_bytes(&self) -> Option<usize> {
        match self {
            Self::Chunk(grant) => grant.queued_backing_bytes(),
            // Original target/native/input backing stays debited in its original pools.
            // This queue retains the moved value, not a second copy of those Arc payloads.
            Self::Publication(_) => std::mem::size_of::<QueuedGrant>()
                .checked_add(std::mem::size_of::<FileOperationCommand>()),
        }
    }
    pub(super) fn begin_chunk_source_effect(
        &self,
    ) -> Result<
        Option<crate::sample_sources::db::SourceEffectPermit>,
        crate::sample_sources::db::SourceAuthorityError,
    > {
        match self {
            Self::Chunk(grant) => grant.begin_source_effect().map(Some),
            Self::Publication(_) => Ok(None),
        }
    }
    pub(super) fn validate_original_worker(
        &self,
        worker: &WorkerState,
        shared: &Arc<OwnerShared>,
    ) -> Result<(), EffectOutcome> {
        match self {
            Self::Chunk(_) => Ok(()),
            Self::Publication(grant) => worker
                .validate_original_publication_target(grant.original_target(), shared)
                .map_err(|error| EffectOutcome::SupersededBeforeEffect {
                    reason: format!("original publication binding refused: {error:?}"),
                }),
        }
    }
}

impl FileOperationOwner {
    /// Move the original grant exactly once. Confirmed rejection returns that same value.
    #[allow(dead_code)] // Ordinary publication family producer remains OPT-2216.
    pub(crate) fn submit_publication(
        &self,
        grant: StagingPublicationGrant,
    ) -> Result<FileOperationRequest, (FileOperationOwnerQueueError, StagingPublicationGrant)> {
        if grant.original_target().target().registration().owner != self.shared.owner {
            return Err((
                FileOperationOwnerQueueError::InvalidInput(FileOperationValidationError::new(
                    "original publication belongs to another file owner",
                )),
                grant,
            ));
        }
        let command = match grant.original_target().publication_command() {
            Ok(command) => command,
            Err(error) => return Err((FileOperationOwnerQueueError::InvalidInput(error), grant)),
        };
        let backing = command
            .canonical_bytes()
            .ok()
            .and_then(|bytes| bytes.len().checked_mul(2))
            .and_then(|bytes| {
                bytes
                    .checked_add(std::mem::size_of::<QueuedGrant>())
                    .and_then(|bytes| {
                        bytes.checked_add(std::mem::size_of::<FileOperationCommand>())
                    })
            });
        if backing.is_none_or(|bytes| bytes > 64 * 1024) {
            return Err((
                FileOperationOwnerQueueError::RecordTooLarge {
                    actual: backing.unwrap_or(usize::MAX),
                },
                grant,
            ));
        }
        self.submit_inner(command, Some(QueuedGrant::Publication(grant)))
            .map_err(|(error, original)| match original {
                Some(QueuedGrant::Publication(grant)) => (error, grant),
                _ => unreachable!("original publication returned on confirmed rejection"),
            })
    }
}

// Matching observations cannot publish a completion: only the original moved grant owns
// its journal slot. This uses borrowed fields and stack hashing, with no owned projection.
pub(crate) fn completion_observation(
    target: &PreparedPublicationTarget,
    fingerprint: [u8; 32],
    result: &FileOperationResult,
    worker: OwnerIdentity,
) -> ([u8; 32], bool) {
    let (tag, reason, before_effect) = match result.outcome() {
        EffectOutcome::Unsupported { reason } => (1u8, reason.as_str(), true),
        EffectOutcome::SupersededBeforeEffect { reason } => (2, reason.as_str(), true),
        EffectOutcome::RetryableBeforeEffect { reason } => (3, reason.as_str(), true),
        EffectOutcome::Collision { reason } => (4, reason.as_str(), true),
        EffectOutcome::RetainedUncertain { reason } => (5, reason.as_str(), false),
        _ => (0, "unqualified publication completion", false),
    };
    let roots_exact = match (
        result.root_bindings(),
        target.validation().root_bindings(),
        target.validation().outcome(),
    ) {
        ([staging, destination], [original], EffectOutcome::Applied { evidence }) => {
            staging.role == RootRole::Staging
                && staging.registration == original.registration
                && staging.authority == original.authority
                && staging.locator == original.locator
                && evidence
                    .observed_identity
                    .is_some_and(|identity| staging.expected == ExpectedObject::Exact(identity))
                && destination.role == RootRole::Destination
                && destination.registration == target.target().registration
                && destination.authority == target.target().authority
                && destination.locator == target.target().locator
                && destination.expected == target.target().expected
        }
        _ => false,
    };
    let exact = before_effect
        && reason.len() <= FILE_OPERATION_OWNER_MAX_DIAGNOSTIC_BYTES
        && worker == target.target().registration.owner
        && result.operation_id() == target.validation().operation_id()
        && result.participant_id() == target.validation().participant_id()
        && result.effect_id() == target.publication_effect()
        && result.target_registration() == target.target().registration
        && result.source_fences() == target.validation().source_fences()
        && result.expected() == ExpectedObject::Absent
        && fingerprint == result.fingerprint().as_bytes()
        && roots_exact;
    (
        publication_no_effect_digest(tag, result.fingerprint().as_bytes(), reason),
        exact,
    )
}
