mod error;
mod persistence;
mod reconcile;
mod similarity;
mod view;
mod work;

pub use error::ReadinessError;
pub use similarity::{
    ReadinessEmbeddingArtifactTarget, ReadinessSimilarityManifest,
    ReadinessSimilarityManifestRequest, ReadinessSimilarityManifestRow,
    ReadinessSimilarityPayloadContract,
};
use std::{collections::BTreeSet, sync::atomic::AtomicBool};
pub use view::ReadinessView;

use rusqlite::{Connection, TransactionBehavior, params};

use super::{
    ClaimedReadinessWork, ReadinessArtifact, ReadinessDeficit, ReadinessFailureClassification,
    ReadinessFailureOutcome, ReadinessLeaseRenewalOutcome, ReadinessMembership,
    ReadinessRetryPolicy, ReadinessSnapshot, ReadinessStage, ReadinessTarget,
    ReadinessWorkMutationOutcome, ReadinessWorkStats, SourceAvailability,
};
use crate::sample_sources::db::{ReadinessOwnership, SourcePublicationFence};
use crate::sample_sources::readiness::ArtifactPublishOutcome;

/// Typed request for atomically publishing a source's complete desired readiness set.
///
/// Keeping the publication inputs together makes the generation, revision, availability, and
/// target set one durable operation instead of a convention spread across native orchestration.
#[derive(Debug)]
pub struct ReadinessTargetPublication<'a> {
    source_id: &'a str,
    source_generation: i64,
    readiness_revision: i64,
    availability: SourceAvailability,
    contract_version: &'a str,
    targets: &'a [ReadinessTarget],
    updated_at: i64,
}

impl<'a> ReadinessTargetPublication<'a> {
    /// Build one complete readiness publication for a source generation.
    pub fn new(
        source_id: &'a str,
        source_generation: i64,
        readiness_revision: i64,
        availability: SourceAvailability,
        contract_version: &'a str,
        targets: &'a [ReadinessTarget],
        updated_at: i64,
    ) -> Self {
        Self {
            source_id,
            source_generation,
            readiness_revision,
            availability,
            contract_version,
            targets,
            updated_at,
        }
    }
}

/// Typed request for atomically applying a bounded readiness target delta.
#[derive(Debug)]
pub struct ReadinessTargetDeltaPublication<'a> {
    source_id: &'a str,
    source_generation: i64,
    readiness_revision: i64,
    availability: SourceAvailability,
    contract_version: &'a str,
    upsert_targets: &'a [ReadinessTarget],
    deleted_file_scope_ids: &'a [String],
    similarity_artifact_version: &'a str,
    updated_at: i64,
}

impl<'a> ReadinessTargetDeltaPublication<'a> {
    /// Build one revision-fenced delta publication.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source_id: &'a str,
        source_generation: i64,
        readiness_revision: i64,
        availability: SourceAvailability,
        contract_version: &'a str,
        upsert_targets: &'a [ReadinessTarget],
        deleted_file_scope_ids: &'a [String],
        similarity_artifact_version: &'a str,
        updated_at: i64,
    ) -> Self {
        Self {
            source_id,
            source_generation,
            readiness_revision,
            availability,
            contract_version,
            upsert_targets,
            deleted_file_scope_ids,
            similarity_artifact_version,
            updated_at,
        }
    }
}

/// Opaque expected-source identity stamp for a readiness publication.
///
/// The origin retains only a weak source lease and an in-memory publication generation. It does
/// not keep the source database, SQLite control connection, or physical-source lock alive. Capture
/// one from the [`ReadinessStore`] at work admission and pass that same origin to the eventual
/// publication boundary, where current ownership and the original fence are validated.
#[derive(Clone, Debug)]
pub struct ReadinessPublicationOrigin {
    publication_fence: Option<SourcePublicationFence>,
}

impl ReadinessPublicationOrigin {
    fn from_fence(publication_fence: Option<SourcePublicationFence>) -> Self {
        Self { publication_fence }
    }

    pub(crate) fn publication_fence(&self) -> Option<&SourcePublicationFence> {
        self.publication_fence.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn unowned_fixture() -> Self {
        Self::from_fence(None)
    }
}

/// Result of attempting a bounded target delta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadinessDeltaPublicationOutcome {
    /// The delta committed atomically.
    Applied {
        /// Current source-scoped membership generation after the delta.
        membership_generation: String,
        /// Number of durable target, job, and source rows changed.
        changed: usize,
    },
    /// Durable state was absent or incompatible, so a complete publication is required.
    RequiresFullPublication,
}

/// The only production persistence boundary for durable source readiness.
///
/// The repository borrows one source-database connection, owns readiness transactions, and
/// exposes domain operations rather than readiness table access.
pub struct ReadinessStore<'connection> {
    connection: &'connection mut Connection,
    ownership: ReadinessOwnership<'connection>,
}

/// The result of an idempotent compatibility cleanup after its transaction commits.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReadinessCompatibilityCleanup {
    /// Number of obsolete readiness rows removed.
    pub changed: usize,
    /// Cache payloads no longer referenced by the retired readiness rows.
    pub retired_artifact_refs: Vec<String>,
}

/// Durable readiness publication state for one configured source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessSourceState {
    /// Last accepted manifest generation.
    pub source_generation: i64,
    /// Last accepted readiness publication revision.
    pub readiness_revision: i64,
    /// Current processing availability.
    pub availability: SourceAvailability,
    /// Artifact/readiness contract that owns the current target set.
    pub contract_version: String,
    /// Incremental source similarity membership checkpoint.
    pub membership: ReadinessMembership,
}

/// Read-only metadata for the latest durably published readiness wake.
///
/// This value is an observation of committed source state. It is not a scheduler capability,
/// durable acknowledgement, or proof that a consumer has claimed the publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessWakeObservation {
    source_id: String,
    wake_id: String,
    owner_revision: i64,
    source_generation: i64,
    readiness_revision: i64,
    contract_version: String,
    target_digest: [u8; 32],
    target_count: i64,
}

impl ReadinessWakeObservation {
    /// Source identity covered by this wake.
    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    /// Non-nil UUID identifying this accepted publication.
    pub fn wake_id(&self) -> &str {
        &self.wake_id
    }

    /// Monotonic source-local observation revision.
    pub fn owner_revision(&self) -> i64 {
        self.owner_revision
    }

    /// Source generation covered by the wake.
    pub fn source_generation(&self) -> i64 {
        self.source_generation
    }

    /// Readiness publication revision covered by the wake.
    pub fn readiness_revision(&self) -> i64 {
        self.readiness_revision
    }

    /// Readiness contract identity covered by the wake.
    pub fn contract_version(&self) -> &str {
        &self.contract_version
    }

    /// Canonical digest of every current target row.
    pub fn target_digest(&self) -> &[u8; 32] {
        &self.target_digest
    }

    /// Number of target rows covered by the digest.
    pub fn target_count(&self) -> i64 {
        self.target_count
    }
}

/// A source-owned, read-time qualification of one durable readiness wake.
///
/// This is deliberately not serializable or clonable. It records the exact wake and source
/// revision observed by one coherent SQLite read, together with the physical source fence that
/// was live at that boundary. The physical root/database identities and persisted readiness
/// namespace are the source identity available at this layer; this value does not establish an
/// independent catalog source ID or lifecycle authority. An operation consumer must compare those
/// identities, the namespace and revision, its current execution epoch, and separately established
/// lifecycle context before using the observation. Fence revalidation proves only lease liveness;
/// it does not refresh or establish currentness of the wake, targets, or revision.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct ReadinessWakeQualification {
    observation: ReadinessWakeObservation,
    publication_fence: SourcePublicationFence,
    source_revision: u64,
    root_identity: String,
    database_identity: String,
}

#[allow(dead_code)]
impl ReadinessWakeQualification {
    /// Wake metadata observed in the source transaction.
    pub(crate) fn observation(&self) -> &ReadinessWakeObservation {
        &self.observation
    }

    /// Physical-source fence retained for a later liveness check.
    pub(crate) fn publication_fence(&self) -> &SourcePublicationFence {
        &self.publication_fence
    }

    /// Source metadata revision observed in the same transaction as the wake.
    pub(crate) fn source_revision(&self) -> u64 {
        self.source_revision
    }

    /// Lease epoch captured with the coherent source-side read.
    pub(crate) fn lease_epoch(&self) -> i64 {
        self.publication_fence.lease_epoch()
    }

    /// Verified database-root identity retained by the owned source database.
    pub(crate) fn root_identity(&self) -> &str {
        &self.root_identity
    }

    /// Verified SQLite database-file identity retained by the owned source database.
    pub(crate) fn database_identity(&self) -> &str {
        &self.database_identity
    }

    /// Revalidate only the retained source lease/fence liveness.
    ///
    /// This does not re-read the wake or target set and therefore does not establish currentness
    /// of any source observation after the original qualification boundary.
    pub(crate) fn validate_source_liveness(&self) -> Result<(), ReadinessError> {
        self.publication_fence
            .validate()
            .map_err(ReadinessError::from)
    }
}

impl<'connection> ReadinessStore<'connection> {
    /// Bind readiness persistence to one caller-selected source database connection.
    ///
    /// This constructor is suitable for read-only queries. In normal builds, mutation methods
    /// return [`ReadinessError::OwnershipRequired`]; use
    /// [`crate::sample_sources::db::SourceDatabase::readiness_store`]
    /// through a profile-owned source database for writes.
    pub fn new(connection: &'connection mut Connection) -> Self {
        Self {
            connection,
            ownership: ReadinessOwnership::unowned(),
        }
    }

    /// Bind readiness persistence to a source database's retained ownership context.
    pub(crate) fn from_owned_connection(
        connection: &'connection mut Connection,
        ownership: ReadinessOwnership<'connection>,
    ) -> Self {
        Self {
            connection,
            ownership,
        }
    }

    /// Capture the expected physical-source identity for work admitted by this store.
    ///
    /// The returned origin is an immutable weak stamp and does not itself validate or renew
    /// ownership. In test-only unowned fixtures it may contain no physical-source fence; normal
    /// raw connections return [`ReadinessError::OwnershipRequired`].
    pub fn capture_publication_origin(&self) -> Result<ReadinessPublicationOrigin, ReadinessError> {
        self.ownership
            .publication_fence()
            .map(ReadinessPublicationOrigin::from_fence)
    }

    /// Atomically publish one complete, validated target generation.
    pub fn publish_targets(
        &mut self,
        origin: &ReadinessPublicationOrigin,
        publication: &ReadinessTargetPublication<'_>,
    ) -> Result<(), ReadinessError> {
        persistence::replace_readiness_targets_owned(
            self.connection,
            &self.ownership,
            origin,
            publication.source_id,
            publication.source_generation,
            publication.readiness_revision,
            publication.availability,
            publication.contract_version,
            publication.targets,
            publication.updated_at,
            None,
        )
    }

    /// Publish targets while rolling back if cancellation is observed.
    pub fn publish_targets_with_cancel(
        &mut self,
        origin: &ReadinessPublicationOrigin,
        publication: &ReadinessTargetPublication<'_>,
        cancel: &AtomicBool,
    ) -> Result<(), ReadinessError> {
        persistence::replace_readiness_targets_owned(
            self.connection,
            &self.ownership,
            origin,
            publication.source_id,
            publication.source_generation,
            publication.readiness_revision,
            publication.availability,
            publication.contract_version,
            publication.targets,
            publication.updated_at,
            Some(cancel),
        )
    }

    /// Apply a bounded readiness target delta or request a complete fallback publication.
    pub fn publish_target_delta_with_cancel(
        &mut self,
        origin: &ReadinessPublicationOrigin,
        publication: &ReadinessTargetDeltaPublication<'_>,
        cancel: &AtomicBool,
    ) -> Result<ReadinessDeltaPublicationOutcome, ReadinessError> {
        persistence::apply_readiness_target_delta_owned(
            self.connection,
            &self.ownership,
            origin,
            publication.source_id,
            publication.source_generation,
            publication.readiness_revision,
            publication.availability,
            publication.contract_version,
            publication.upsert_targets,
            publication.deleted_file_scope_ids,
            publication.similarity_artifact_version,
            publication.updated_at,
            Some(cancel),
        )
    }

    /// Classify durable readiness for one source at a point in time.
    pub fn reconcile(
        &mut self,
        source_id: &str,
        now: i64,
    ) -> Result<ReadinessSnapshot, ReadinessError> {
        reconcile::reconcile_readiness_inner(self.connection, source_id, now, None, &mut |_| {})
    }

    /// Reconcile a source while reporting checkpoints and honoring cancellation.
    pub fn reconcile_with_cancel_and_progress(
        &mut self,
        source_id: &str,
        now: i64,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(super::ReadinessProgress),
    ) -> Result<ReadinessSnapshot, ReadinessError> {
        reconcile::reconcile_readiness_inner(
            self.connection,
            source_id,
            now,
            Some(cancel),
            progress,
        )
    }

    /// Reconcile only committed file identities plus the source-scoped similarity target.
    ///
    /// This is valid after an atomic readiness delta publication: unchanged file targets retain
    /// their exact artifacts and work, while the source target is always included because its
    /// membership generation changes with the affected identities.
    pub fn reconcile_scopes_with_cancel_and_progress(
        &mut self,
        source_id: &str,
        scope_ids: &BTreeSet<String>,
        now: i64,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(super::ReadinessProgress),
    ) -> Result<ReadinessSnapshot, ReadinessError> {
        reconcile::reconcile_readiness_scopes_inner(
            self.connection,
            source_id,
            scope_ids,
            now,
            Some(cancel),
            progress,
        )
    }

    /// Atomically enqueue actionable deficits.
    pub fn persist_deficits(
        &mut self,
        deficits: &[ReadinessDeficit],
        created_at: i64,
    ) -> Result<usize, ReadinessError> {
        persistence::persist_readiness_deficits_owned(
            self.connection,
            &self.ownership,
            deficits,
            created_at,
            None,
            &mut |_| {},
        )
    }

    /// Enqueue deficits with cancellation-safe progress reporting.
    pub fn persist_deficits_with_cancel_and_progress(
        &mut self,
        deficits: &[ReadinessDeficit],
        created_at: i64,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(super::ReadinessProgress),
    ) -> Result<usize, ReadinessError> {
        persistence::persist_readiness_deficits_owned(
            self.connection,
            &self.ownership,
            deficits,
            created_at,
            Some(cancel),
            progress,
        )
    }

    /// Claim one current target under a durable lease.
    pub fn claim(
        &mut self,
        target: &ReadinessTarget,
        now: i64,
        lease_duration_seconds: i64,
    ) -> Result<Option<ClaimedReadinessWork>, ReadinessError> {
        work::claim_readiness_target_owned(
            self.connection,
            &self.ownership,
            target,
            now,
            lease_duration_seconds,
        )
    }
    /// Renew an active claim without changing its generation.
    pub fn renew_lease(
        &mut self,
        claim: &ClaimedReadinessWork,
        now: i64,
        lease_duration_seconds: i64,
    ) -> Result<ReadinessLeaseRenewalOutcome, ReadinessError> {
        work::renew_readiness_lease_owned(
            self.connection,
            &self.ownership,
            claim,
            now,
            lease_duration_seconds,
        )
    }
    /// Publish a claimed completion without an external artifact reference.
    pub fn complete(
        &mut self,
        claim: &ClaimedReadinessWork,
        completed_at: i64,
    ) -> Result<ArtifactPublishOutcome, ReadinessError> {
        work::complete_readiness_work_owned(self.connection, &self.ownership, claim, completed_at)
    }
    /// Publish a claimed completion with its durable artifact reference.
    pub fn complete_with_artifact_ref(
        &mut self,
        claim: &ClaimedReadinessWork,
        completed_at: i64,
        artifact_ref: &str,
    ) -> Result<ArtifactPublishOutcome, ReadinessError> {
        work::complete_readiness_work_with_artifact_ref_owned(
            self.connection,
            &self.ownership,
            claim,
            completed_at,
            artifact_ref,
        )
    }
    /// Record a classified failure for a current claim.
    pub fn fail(
        &mut self,
        claim: &ClaimedReadinessWork,
        classification: ReadinessFailureClassification,
        code: &str,
        reason: &str,
        failed_at: i64,
        retry_policy: ReadinessRetryPolicy,
    ) -> Result<ReadinessFailureOutcome, ReadinessError> {
        work::fail_readiness_work_owned(
            self.connection,
            &self.ownership,
            claim,
            classification,
            code,
            reason,
            failed_at,
            retry_policy,
        )
    }
    /// Return a current claim to pending work.
    pub fn release(
        &mut self,
        claim: &ClaimedReadinessWork,
        released_at: i64,
    ) -> Result<ReadinessWorkMutationOutcome, ReadinessError> {
        work::release_readiness_work_owned(self.connection, &self.ownership, claim, released_at)
    }
    /// Return a current claim to pending work with a cancellation diagnostic.
    pub fn cancel(
        &mut self,
        claim: &ClaimedReadinessWork,
        reason: &str,
        cancelled_at: i64,
    ) -> Result<ReadinessWorkMutationOutcome, ReadinessError> {
        work::cancel_readiness_work_owned(
            self.connection,
            &self.ownership,
            claim,
            reason,
            cancelled_at,
        )
    }
    /// Invalidate an exact artifact only when its target remains current.
    pub fn invalidate_artifact(
        &mut self,
        target: &ReadinessTarget,
    ) -> Result<bool, ReadinessError> {
        persistence::invalidate_readiness_artifact_owned(self.connection, &self.ownership, target)
    }

    /// Invalidate source-owned analysis outputs and requeue their exact current
    /// readiness targets for an explicit asynchronous reanalysis request.
    pub fn requeue_source_analysis(
        &mut self,
        source_id: &str,
        requested_at: i64,
    ) -> Result<usize, ReadinessError> {
        persistence::requeue_source_analysis_owned(
            self.connection,
            &self.ownership,
            source_id,
            requested_at,
        )
    }
    /// Load queue and lease-recovery telemetry for the current durable work set.
    pub fn work_stats(&mut self, now: i64) -> Result<ReadinessWorkStats, ReadinessError> {
        work::readiness_work_stats(self.connection, now)
    }

    /// Load bounded queue and lease-recovery telemetry for one source.
    pub fn source_work_stats(
        &mut self,
        source_id: &str,
        now: i64,
    ) -> Result<ReadinessWorkStats, ReadinessError> {
        work::readiness_work_stats_for_source(self.connection, source_id, now)
    }

    /// Publish an independently produced artifact if its target is still current.
    pub fn publish_artifact(
        &mut self,
        origin: &ReadinessPublicationOrigin,
        artifact: &ReadinessArtifact,
    ) -> Result<ArtifactPublishOutcome, ReadinessError> {
        persistence::publish_readiness_artifact_owned(
            self.connection,
            &self.ownership,
            origin,
            artifact,
        )
    }

    /// Whether the readiness schema required for source processing is present.
    pub fn schema_available(&mut self) -> Result<bool, ReadinessError> {
        self.connection.query_row(
            "SELECT COUNT(*) = 3 FROM sqlite_master WHERE type = 'table' AND name IN ('source_readiness_sources', 'source_readiness_targets', 'source_readiness_artifacts')",
            [], |row| row.get(0),
        ).map_err(Into::into)
    }

    /// Whether the source database has every readiness-owned table and work column.
    pub fn processing_schema_available(&mut self) -> Result<bool, ReadinessError> {
        for (table, columns) in [
            (
                "analysis_jobs",
                &[
                    "id",
                    "relative_path",
                    "job_type",
                    "created_at",
                    "status",
                    "readiness_managed",
                ][..],
            ),
            (
                "source_readiness_artifacts",
                &[
                    "source_id",
                    "scope_kind",
                    "scope_id",
                    "relative_path",
                    "stage",
                    "artifact_version",
                    "content_generation",
                    "artifact_ref",
                ][..],
            ),
            (
                "source_readiness_sources",
                &[
                    "source_id",
                    "source_generation",
                    "readiness_revision",
                    "availability",
                    "contract_version",
                    "membership_digest",
                    "membership_count",
                ][..],
            ),
            (
                "source_readiness_wakes",
                &[
                    "source_id",
                    "wake_id",
                    "owner_revision",
                    "source_generation",
                    "readiness_revision",
                    "contract_version",
                    "target_digest",
                    "target_count",
                ][..],
            ),
        ] {
            let pragma = format!("PRAGMA table_info({table})");
            let mut statement = self.connection.prepare(&pragma)?;
            let found = statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<BTreeSet<_>, _>>()?;
            if columns.iter().any(|column| !found.contains(*column)) {
                return Ok(false);
            }
        }
        self.schema_available()
    }

    /// Whether one source has durable readiness state.
    pub fn source_exists(&mut self, source_id: &str) -> Result<bool, ReadinessError> {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM source_readiness_sources WHERE source_id = ?1)",
                [source_id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Whether an exact file generation is already known to be permanently unsupported.
    pub fn generation_is_known_unsupported(
        &mut self,
        source_id: &str,
        scope_id: &str,
        content_generation: &str,
    ) -> Result<bool, ReadinessError> {
        work::readiness_generation_is_known_unsupported(
            self.connection,
            source_id,
            scope_id,
            content_generation,
        )
    }

    /// Load the state required to decide whether a complete target publication is still current.
    pub fn source_state(
        &mut self,
        source_id: &str,
    ) -> Result<Option<ReadinessSourceState>, ReadinessError> {
        persistence::read_source_state(self.connection, source_id)
    }

    /// Read and validate the latest committed readiness wake for one source.
    ///
    /// A missing wake is expected for sources migrated before wake publication was introduced.
    /// Stored wake metadata and the complete current target set are validated together, so a
    /// malformed or stale row remains an actionable error rather than becoming a capability.
    pub fn readiness_wake(
        &mut self,
        source_id: &str,
    ) -> Result<Option<ReadinessWakeObservation>, ReadinessError> {
        persistence::read_readiness_wake(self.connection, source_id)
    }

    /// Qualify one committed readiness wake from a fully owned source database.
    ///
    /// The wake, source revision, source readiness state, and complete target digest are read in
    /// one transaction. The retained profile, database binding, and source lease are checked both
    /// before and after that read. This produces a read-time source observation only, bound to the
    /// persisted readiness namespace and physical identities available here. Consumers must add
    /// exact operation namespace, revision, current-epoch, and lifecycle comparisons; the retained
    /// fence can only revalidate lease liveness and does not refresh this wake.
    #[allow(dead_code)]
    pub(crate) fn qualify_readiness_wake(
        &mut self,
        source_id: &str,
    ) -> Result<ReadinessWakeQualification, ReadinessError> {
        self.ownership.validate_read_only(&*self.connection)?;
        let publication_fence = self
            .ownership
            .publication_fence()?
            .ok_or(ReadinessError::OwnershipRequired)?;
        let (root_identity, database_identity) = self.ownership.verified_binding_identities()?;
        let observation_and_revision =
            persistence::read_readiness_wake_with_source_revision(self.connection, source_id)?;
        let Some((observation, source_revision)) = observation_and_revision else {
            return Err(ReadinessError::ReadinessWakeMissing {
                source_id: source_id.to_string(),
            });
        };
        self.ownership.validate_read_only(&*self.connection)?;
        self.ownership
            .validate_publication_fence(Some(&publication_fence))?;
        Ok(ReadinessWakeQualification {
            observation,
            publication_fence,
            source_revision,
            root_identity,
            database_identity,
        })
    }

    /// Mark a source offline without modifying its generation or target set.
    pub fn mark_temporarily_unavailable(
        &mut self,
        source_id: &str,
        now: i64,
    ) -> Result<(), ReadinessError> {
        self.ownership.validate(&*self.connection)?;
        let tx = self.connection.transaction()?;
        tx.execute(
            "UPDATE source_readiness_sources SET availability = 'offline', readiness_revision = readiness_revision + 1, updated_at = ?2 WHERE source_id = ?1 AND availability != 'offline'",
            params![source_id, now],
        )?;
        self.ownership.validate(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Drop queued readiness work for files that are still being actively recorded.
    pub fn defer_active_recordings(
        &mut self,
        scope_ids: &BTreeSet<String>,
    ) -> Result<(), ReadinessError> {
        self.ownership.validate(&*self.connection)?;
        let tx = self.connection.transaction()?;
        for scope_id in scope_ids {
            tx.execute(
                "DELETE FROM analysis_jobs WHERE readiness_managed = 1 AND readiness_scope_id = ?1 AND status != 'running'",
                [scope_id],
            )?;
        }
        self.ownership.validate(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Return file content generations that have durably failed as unsupported.
    pub fn unsupported_content_generations(
        &mut self,
        source_id: &str,
    ) -> Result<BTreeSet<(String, String)>, ReadinessError> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT readiness_scope_id, content_generation FROM analysis_jobs WHERE source_id = ?1 AND readiness_managed = 1 AND readiness_scope_kind = 'file' AND status = 'failed' AND failure_kind = 'unsupported' AND readiness_scope_id IS NOT NULL AND content_generation IS NOT NULL",
        )?;
        statement
            .query_map([source_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(Into::into)
    }

    /// Check whether a prerequisite stage is terminally unsupported for a file generation.
    pub fn stage_is_unsupported(
        &mut self,
        target: &ReadinessTarget,
        stage: ReadinessStage,
    ) -> Result<bool, ReadinessError> {
        ReadinessView::new(self.connection).stage_is_unsupported(target, stage)
    }

    /// Upgrade deterministic legacy decode failures to the terminal unsupported classification.
    pub fn reclassify_known_unsupported_failures(
        &mut self,
        is_unsupported: impl Fn(&str) -> bool,
    ) -> Result<usize, ReadinessError> {
        self.ownership.validate(&*self.connection)?;
        let failures = {
            let mut statement = self.connection.prepare(
                "SELECT id, COALESCE(last_error, '') FROM analysis_jobs WHERE readiness_managed = 1 AND status = 'failed' AND failure_code IS NULL AND failure_kind IN ('retryable', 'permanent')",
            )?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        self.ownership.validate(&*self.connection)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut changed = 0;
        for id in failures
            .into_iter()
            .filter_map(|(id, error)| is_unsupported(&error).then_some(id))
        {
            changed += tx.execute(
                "UPDATE analysis_jobs SET failure_kind = 'unsupported', failure_code = 'legacy_decoder_unsupported', retry_at = NULL WHERE id = ?1 AND readiness_managed = 1 AND status = 'failed' AND failure_code IS NULL AND failure_kind IN ('retryable', 'permanent')", [id],
            )?;
        }
        changed += tx.execute(
            "UPDATE analysis_jobs AS dependent SET failure_kind = 'unsupported', failure_code = 'legacy_prerequisite_unsupported', retry_at = NULL WHERE dependent.readiness_managed = 1 AND dependent.status = 'failed' AND dependent.failure_code IS NULL AND dependent.failure_kind IN ('retryable', 'permanent') AND dependent.readiness_stage = 'embedding_aspects' AND dependent.last_error = 'embedding feature prerequisite is not durable yet' AND EXISTS (SELECT 1 FROM analysis_jobs AS prerequisite WHERE prerequisite.readiness_managed = 1 AND prerequisite.source_id = dependent.source_id AND prerequisite.readiness_scope_kind = dependent.readiness_scope_kind AND prerequisite.readiness_scope_id = dependent.readiness_scope_id AND prerequisite.readiness_stage = 'analysis_features' AND prerequisite.content_generation = dependent.content_generation AND prerequisite.status = 'failed' AND prerequisite.failure_kind = 'unsupported')", [],
        )?;
        self.ownership.validate(&tx)?;
        tx.commit()?;
        Ok(changed)
    }

    /// Retire rows belonging to the removed playback-summary stage, atomically collecting cache refs.
    pub fn retire_legacy_playback(
        &mut self,
        source_id: &str,
    ) -> Result<ReadinessCompatibilityCleanup, ReadinessError> {
        self.ownership.validate(&*self.connection)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let refs = {
            let mut statement = tx.prepare("SELECT artifact_ref FROM source_readiness_artifacts WHERE source_id = ?1 AND stage = 'playback_summary' AND artifact_ref IS NOT NULL AND length(trim(artifact_ref)) > 0")?;
            statement
                .query_map([source_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut changed = tx.execute("DELETE FROM analysis_jobs WHERE source_id = ?1 AND readiness_managed = 1 AND readiness_stage = 'playback_summary'", [source_id])?;
        changed += tx.execute("DELETE FROM source_readiness_artifacts WHERE source_id = ?1 AND stage = 'playback_summary'", [source_id])?;
        changed += tx.execute("DELETE FROM source_readiness_targets WHERE source_id = ?1 AND stage = 'playback_summary'", [source_id])?;
        self.ownership.validate(&tx)?;
        tx.commit()?;
        Ok(ReadinessCompatibilityCleanup {
            changed,
            retired_artifact_refs: refs,
        })
    }

    /// Disable a retired source and delete all readiness-owned work, retaining cache cleanup data.
    pub fn retire_source(
        &mut self,
        source_id: &str,
        now: i64,
    ) -> Result<ReadinessCompatibilityCleanup, ReadinessError> {
        self.ownership.validate(&*self.connection)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let refs = {
            let mut statement = tx.prepare("SELECT artifact_ref FROM source_readiness_artifacts WHERE source_id = ?1 AND stage = 'playback_summary' AND artifact_ref IS NOT NULL AND length(trim(artifact_ref)) > 0")?;
            statement
                .query_map([source_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        tx.execute("UPDATE source_readiness_sources SET availability = 'disabled', readiness_revision = readiness_revision + 1, updated_at = ?2 WHERE source_id = ?1 AND availability != 'disabled'", params![source_id, now])?;
        let mut changed = tx.execute(
            "DELETE FROM analysis_jobs WHERE source_id = ?1 AND readiness_managed = 1",
            [source_id],
        )?;
        changed += tx.execute("DELETE FROM source_readiness_artifacts WHERE source_id = ?1 AND stage = 'playback_summary'", [source_id])?;
        changed += tx.execute("DELETE FROM source_readiness_targets WHERE source_id = ?1 AND stage = 'playback_summary'", [source_id])?;
        self.ownership.validate(&tx)?;
        tx.commit()?;
        Ok(ReadinessCompatibilityCleanup {
            changed,
            retired_artifact_refs: refs,
        })
    }

    /// Return legacy playback cache references for bounded app-global cache collection.
    pub fn legacy_playback_artifact_refs(&mut self) -> Result<Vec<String>, ReadinessError> {
        if !self.schema_available()? {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            "SELECT artifact_ref FROM source_readiness_artifacts WHERE stage = 'playback_summary' AND artifact_ref IS NOT NULL AND length(trim(artifact_ref)) > 0",
        )?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Check legacy playback cache ownership without exposing the artifact table to callers.
    pub fn legacy_playback_artifact_ref_is_owned(
        &mut self,
        artifact_ref: &str,
    ) -> Result<bool, ReadinessError> {
        if !self.schema_available()? {
            return Ok(false);
        }
        self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM source_readiness_artifacts WHERE stage = 'playback_summary' AND artifact_ref = ?1)",
            [artifact_ref], |row| row.get(0),
        ).map_err(Into::into)
    }
}

#[cfg(test)]
pub(crate) use persistence::{
    invalidate_readiness_artifact, persist_readiness_deficits, publish_readiness_artifact,
    replace_readiness_targets,
};
#[cfg(test)]
pub(crate) use reconcile::reconcile_readiness;
#[cfg(test)]
pub(crate) use reconcile::reconcile_readiness_with_hook;
#[cfg(test)]
pub(crate) use work::{
    cancel_readiness_work, claim_readiness_target, complete_readiness_work,
    complete_readiness_work_with_artifact_ref, fail_readiness_work, readiness_work_stats,
    release_readiness_work, renew_readiness_lease,
};
