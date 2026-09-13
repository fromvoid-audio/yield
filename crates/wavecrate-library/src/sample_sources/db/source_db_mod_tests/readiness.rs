use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use super::*;
use crate::{
    app_dirs::{AppRootGuard, ProfileOwnershipError, WritableProfileGuard},
    sample_sources::{
        SourceLeaseError, SourcePublicationFenceError,
        readiness::{
            ArtifactPublishOutcome, ClaimedReadinessWork, ReadinessArtifact, ReadinessError,
            ReadinessFailureClassification, ReadinessRetryPolicy, ReadinessStage, ReadinessStore,
            ReadinessTarget, ReadinessTargetDeltaPublication, ReadinessTargetPublication,
            SourceAvailability,
        },
    },
};
use tempfile::tempdir;

struct ControlledReadinessBinding {
    fail: AtomicBool,
}

impl ControlledReadinessBinding {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            fail: AtomicBool::new(false),
        })
    }
}

impl DatabaseRootBindingGuard for ControlledReadinessBinding {
    fn validate_profile_guard(
        &self,
        profile_guard: &WritableProfileGuard,
    ) -> Result<(), ProfileOwnershipError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(ProfileOwnershipError::ProfileRootReplaced {
                path: profile_guard.profile_root().to_path_buf(),
            });
        }
        Ok(())
    }
}

fn publication<'a>(targets: &'a [ReadinessTarget]) -> ReadinessTargetPublication<'a> {
    ReadinessTargetPublication::new(
        "readiness-owned-source",
        1,
        1,
        SourceAvailability::Active,
        "readiness-contract-v1",
        targets,
        1,
    )
}

#[test]
fn readiness_ownership_rejection_happens_before_transaction() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "readiness-before-begin",
    )
    .expect("profile-owned source database");
    let binding = ControlledReadinessBinding::new();
    let mut database = database.retain_database_root_binding(binding.clone());
    let mut store = database.readiness_store().expect("owned readiness store");
    let origin = store
        .capture_publication_origin()
        .expect("readiness publication origin");
    binding.fail.store(true, Ordering::SeqCst);
    let target = ReadinessTarget::source(
        "readiness-owned-source",
        ReadinessStage::SimilarityLayout,
        "layout-v1",
        1,
        "membership-v1",
    );
    let targets = [target];

    let error = store
        .publish_targets(&origin, &publication(&targets))
        .expect_err("invalidated owner must reject before begin");
    assert!(matches!(
        error,
        ReadinessError::ProfileOwnership(ProfileOwnershipError::ProfileRootReplaced { .. })
    ));
    drop(store);
    let row_count = database
        .connection
        .query_row("SELECT COUNT(*) FROM source_readiness_sources", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("read readiness rows");
    assert_eq!(row_count, 0);
}

#[test]
fn readiness_ownership_rejection_before_commit_rolls_back_transaction() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "readiness-before-commit",
    )
    .expect("profile-owned source database");
    let binding = ControlledReadinessBinding::new();
    let mut database = database.retain_database_root_binding(binding.clone());
    // SQLite's update hook excludes WITHOUT ROWID tables, including
    // source_readiness_sources. Record that exact source-row mutation in a local rowid marker so
    // the existing update hook can invalidate ownership only after the source write executes.
    database
        .connection
        .execute_batch(
            "CREATE TABLE readiness_ownership_source_write_marker (
                 id INTEGER PRIMARY KEY,
                 writes INTEGER NOT NULL
             );
             INSERT INTO readiness_ownership_source_write_marker (id, writes) VALUES (1, 0);
             CREATE TRIGGER readiness_ownership_source_write
             AFTER INSERT ON source_readiness_sources
             BEGIN
                 UPDATE readiness_ownership_source_write_marker
                 SET writes = writes + 1
                 WHERE id = 1;
             END;",
        )
        .expect("install source-write test hook");
    let hook_fired = Arc::new(AtomicBool::new(false));
    let hook_fired_by_sqlite = Arc::clone(&hook_fired);
    let binding_by_sqlite = Arc::clone(&binding);
    database.connection.update_hook(Some(
        move |_action: rusqlite::hooks::Action,
              _database_name: &str,
              table_name: &str,
              _row_id: i64| {
            if table_name == "readiness_ownership_source_write_marker" {
                hook_fired_by_sqlite.store(true, Ordering::SeqCst);
                binding_by_sqlite.fail.store(true, Ordering::SeqCst);
            }
        },
    ));
    let mut store = database.readiness_store().expect("owned readiness store");
    let origin = store
        .capture_publication_origin()
        .expect("readiness publication origin");
    let target = ReadinessTarget::source(
        "readiness-owned-source",
        ReadinessStage::SimilarityLayout,
        "layout-v1",
        1,
        "membership-v1",
    );
    let targets = [target];

    let error = store
        .publish_targets(&origin, &publication(&targets))
        .expect_err("owner replacement before commit must reject");
    assert!(matches!(
        error,
        ReadinessError::ProfileOwnership(ProfileOwnershipError::ProfileRootReplaced { .. })
    ));
    assert!(
        hook_fired.load(Ordering::SeqCst),
        "ownership invalidation must follow an actual readiness row write"
    );
    drop(store);
    let row_count = database
        .connection
        .query_row("SELECT COUNT(*) FROM source_readiness_sources", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("read readiness rows");
    assert_eq!(row_count, 0);
}

#[test]
fn readiness_publication_origin_rejects_stale_artifact_and_targets_after_handoff() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let replacement_root = tempdir().expect("replacement source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let mut database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "readiness-publication-origin-handoff",
    )
    .expect("profile-owned source database");
    let target = ReadinessTarget::source(
        "readiness-owned-source",
        ReadinessStage::SimilarityLayout,
        "layout-v1",
        1,
        "membership-v1",
    );
    let original_origin = {
        let mut store = database.readiness_store().expect("owned readiness store");
        let origin = store
            .capture_publication_origin()
            .expect("readiness publication origin");
        let targets = [target.clone()];
        store
            .publish_targets(&origin, &publication(&targets))
            .expect("publish initial readiness target");
        assert_eq!(
            store
                .publish_artifact(&origin, &ReadinessArtifact::for_target(&target, 10))
                .expect("publish initial readiness artifact"),
            ArtifactPublishOutcome::Recorded
        );
        origin
    };

    // Retaining an origin must not retain the physical lease, so the consuming handoff can still
    // take ownership and reopen the same database while the old origin remains queued.
    let mut database = database
        .retarget_catalog_root(replacement_root.path(), &profile_guard)
        .expect("catalog root handoff");
    let mut store = database
        .readiness_store()
        .expect("new owned readiness store");
    let stale_artifact = ReadinessArtifact::for_target(&target, 99);
    assert!(matches!(
        store.publish_artifact(&original_origin, &stale_artifact),
        Err(ReadinessError::SourcePublicationFence(
            SourcePublicationFenceError::LeaseMismatch
        ))
    ));

    let stale_target = ReadinessTarget::source(
        "readiness-owned-source",
        ReadinessStage::SimilarityLayout,
        "layout-v2",
        1,
        "membership-v2",
    );
    let stale_targets = [stale_target];
    let stale_publication = ReadinessTargetPublication::new(
        "readiness-owned-source",
        1,
        2,
        SourceAvailability::Active,
        "readiness-contract-v1",
        &stale_targets,
        20,
    );
    assert!(matches!(
        store.publish_targets(&original_origin, &stale_publication),
        Err(ReadinessError::SourcePublicationFence(
            SourcePublicationFenceError::LeaseMismatch
        ))
    ));
    assert!(matches!(
        store.publish_targets_with_cancel(
            &original_origin,
            &stale_publication,
            &AtomicBool::new(false),
        ),
        Err(ReadinessError::SourcePublicationFence(
            SourcePublicationFenceError::LeaseMismatch
        ))
    ));

    let no_upserts: [ReadinessTarget; 0] = [];
    let no_deleted_scope_ids: [String; 0] = [];
    let stale_delta = ReadinessTargetDeltaPublication::new(
        "readiness-owned-source",
        2,
        3,
        SourceAvailability::Active,
        "readiness-contract-v1",
        &no_upserts,
        &no_deleted_scope_ids,
        "layout-v3",
        30,
    );
    assert!(matches!(
        store.publish_target_delta_with_cancel(
            &original_origin,
            &stale_delta,
            &AtomicBool::new(false),
        ),
        Err(ReadinessError::SourcePublicationFence(
            SourcePublicationFenceError::LeaseMismatch
        ))
    ));
    drop(store);

    let source_state: (i64, i64, String) = database
        .connection
        .query_row(
            "SELECT source_generation, readiness_revision, contract_version
             FROM source_readiness_sources
             WHERE source_id = 'readiness-owned-source'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("unchanged readiness source state");
    assert_eq!(source_state, (1, 1, String::from("readiness-contract-v1")));
    let target_state: (String, String) = database
        .connection
        .query_row(
            "SELECT required_version, content_generation
             FROM source_readiness_targets
             WHERE source_id = 'readiness-owned-source'
               AND scope_kind = 'source'
               AND scope_id = 'readiness-owned-source'
               AND stage = 'similarity_layout'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("unchanged readiness target");
    assert_eq!(
        target_state,
        (String::from("layout-v1"), String::from("membership-v1"))
    );
    let artifact_state: (String, String, i64) = database
        .connection
        .query_row(
            "SELECT artifact_version, content_generation, completed_at
             FROM source_readiness_artifacts
             WHERE source_id = 'readiness-owned-source'
               AND scope_kind = 'source'
               AND scope_id = 'readiness-owned-source'
               AND stage = 'similarity_layout'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("unchanged readiness artifact");
    assert_eq!(
        artifact_state,
        (String::from("layout-v1"), String::from("membership-v1"), 10)
    );

    let mut store = database
        .readiness_store()
        .expect("fresh owned readiness store");
    let fresh_origin = store
        .capture_publication_origin()
        .expect("fresh readiness publication origin");
    assert_eq!(
        store
            .publish_artifact(&fresh_origin, &stale_artifact)
            .expect("current origin publishes artifact"),
        ArtifactPublishOutcome::Recorded
    );
}

#[test]
fn readiness_publication_origin_rejects_cross_database_publications_without_renewal() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root_a = tempdir().expect("database root a");
    let database_root_b = tempdir().expect("database root b");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let mut database_a = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root_a.path(),
    )
    .expect("database a");
    let mut database_b = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root_b.path(),
    )
    .expect("database b");
    let target = ReadinessTarget::source(
        "readiness-cross-publication",
        ReadinessStage::SimilarityLayout,
        "layout-v1",
        1,
        "membership-v1",
    );
    let publication = ReadinessTargetPublication::new(
        "readiness-cross-publication",
        1,
        1,
        SourceAvailability::Active,
        "readiness-contract-v1",
        std::slice::from_ref(&target),
        1,
    );
    let origin_a = {
        let mut store_a = database_a.readiness_store().expect("store a");
        let origin = store_a
            .capture_publication_origin()
            .expect("database a publication origin");
        store_a
            .publish_targets(&origin, &publication)
            .expect("publish target a");
        origin
    };
    let mut store_b = database_b.readiness_store().expect("store b");
    let origin_b = store_b
        .capture_publication_origin()
        .expect("database b publication origin");
    store_b
        .publish_targets(&origin_b, &publication)
        .expect("publish target b");
    assert_eq!(
        store_b
            .publish_artifact(&origin_b, &ReadinessArtifact::for_target(&target, 10))
            .expect("publish database b artifact"),
        ArtifactPublishOutcome::Recorded
    );

    // Keep B's store alive while expiring only its durable lease. A wrong database origin must be
    // rejected before B's current ownership validation can renew that expired lease.
    let lease_probe_path = database_root_b
        .path()
        .join(crate::sample_sources::db::DB_FILE_NAME);
    let lease_probe = rusqlite::Connection::open(&lease_probe_path).expect("lease probe");
    lease_probe
        .execute(
            "UPDATE source_lease
             SET acquired_at = 1, expires_at = 1
             WHERE singleton = 1",
            [],
        )
        .expect("expire database b physical lease");
    let lease_before: (i64, i64, String, i64) = lease_probe
        .query_row(
            "SELECT acquired_at, expires_at, owner_identity, lease_epoch
             FROM source_lease
             WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("expired database b lease");
    let source_state_before: (i64, i64, String) = lease_probe
        .query_row(
            "SELECT source_generation, readiness_revision, contract_version
             FROM source_readiness_sources
             WHERE source_id = 'readiness-cross-publication'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("database b source state before mismatch");
    let artifact_before: (String, String, i64) = lease_probe
        .query_row(
            "SELECT artifact_version, content_generation, completed_at
             FROM source_readiness_artifacts
             WHERE source_id = 'readiness-cross-publication'
               AND scope_kind = 'source'
               AND scope_id = 'readiness-cross-publication'
               AND stage = 'similarity_layout'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("database b artifact before mismatch");

    let stale_artifact = ReadinessArtifact::for_target(&target, 99);
    assert!(matches!(
        store_b.publish_artifact(&origin_a, &stale_artifact),
        Err(ReadinessError::SourcePublicationFence(
            SourcePublicationFenceError::LeaseMismatch
        ))
    ));
    let replacement_target = ReadinessTarget::source(
        "readiness-cross-publication",
        ReadinessStage::SimilarityLayout,
        "layout-v2",
        1,
        "membership-v2",
    );
    let replacement_targets = [replacement_target];
    let stale_publication = ReadinessTargetPublication::new(
        "readiness-cross-publication",
        1,
        2,
        SourceAvailability::Active,
        "readiness-contract-v1",
        &replacement_targets,
        20,
    );
    assert!(matches!(
        store_b.publish_targets(&origin_a, &stale_publication),
        Err(ReadinessError::SourcePublicationFence(
            SourcePublicationFenceError::LeaseMismatch
        ))
    ));
    let no_upserts: [ReadinessTarget; 0] = [];
    let no_deleted_scope_ids: [String; 0] = [];
    let stale_delta = ReadinessTargetDeltaPublication::new(
        "readiness-cross-publication",
        2,
        3,
        SourceAvailability::Active,
        "readiness-contract-v1",
        &no_upserts,
        &no_deleted_scope_ids,
        "layout-v3",
        30,
    );
    assert!(matches!(
        store_b.publish_target_delta_with_cancel(&origin_a, &stale_delta, &AtomicBool::new(false),),
        Err(ReadinessError::SourcePublicationFence(
            SourcePublicationFenceError::LeaseMismatch
        ))
    ));

    let lease_after: (i64, i64, String, i64) = lease_probe
        .query_row(
            "SELECT acquired_at, expires_at, owner_identity, lease_epoch
             FROM source_lease
             WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("database b lease after mismatch");
    let source_state_after: (i64, i64, String) = lease_probe
        .query_row(
            "SELECT source_generation, readiness_revision, contract_version
             FROM source_readiness_sources
             WHERE source_id = 'readiness-cross-publication'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("database b source state after mismatch");
    let artifact_after: (String, String, i64) = lease_probe
        .query_row(
            "SELECT artifact_version, content_generation, completed_at
             FROM source_readiness_artifacts
             WHERE source_id = 'readiness-cross-publication'
               AND scope_kind = 'source'
               AND scope_id = 'readiness-cross-publication'
               AND stage = 'similarity_layout'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("database b artifact after mismatch");
    assert_eq!(lease_after, lease_before);
    assert_eq!(source_state_after, source_state_before);
    assert_eq!(artifact_after, artifact_before);
    drop(lease_probe);

    assert_eq!(
        store_b
            .publish_artifact(&origin_b, &ReadinessArtifact::for_target(&target, 11))
            .expect("database b own origin renews and publishes"),
        ArtifactPublishOutcome::Recorded
    );
    drop(store_b);
    let renewed_expiry = database_b
        .connection
        .query_row(
            "SELECT expires_at FROM source_lease WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("renewed database b lease");
    assert!(renewed_expiry > 1);
}

#[test]
fn readiness_claim_fence_rejects_every_old_owner_mutation_after_catalog_handoff() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let replacement_root = tempdir().expect("replacement source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let mut database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "readiness-claim-fence-handoff",
    )
    .expect("profile-owned source database");
    let target = ReadinessTarget::source(
        "readiness-owned-source",
        ReadinessStage::SimilarityLayout,
        "layout-v1",
        1,
        "membership-v1",
    );
    let claim = {
        let mut store = database.readiness_store().expect("owned readiness store");
        let origin = store
            .capture_publication_origin()
            .expect("readiness publication origin");
        let targets = [target.clone()];
        store
            .publish_targets(&origin, &publication(&targets))
            .expect("publish readiness target");
        let deficit = store
            .reconcile("readiness-owned-source", 10)
            .expect("reconcile readiness target")
            .deficits
            .into_iter()
            .next()
            .expect("readiness deficit");
        store
            .persist_deficits(&[deficit], 10)
            .expect("persist readiness work");
        let claim = store
            .claim(&target, 10, 100)
            .expect("claim readiness target")
            .expect("claim available");
        let missing_fence = ClaimedReadinessWork::from_legacy_fixture(
            claim.target().clone(),
            claim.claim_generation(),
            claim.failure_attempts(),
            claim.lease_expires_at(),
            claim.origin(),
        );
        assert!(matches!(
            store.complete(&missing_fence, 20),
            Err(ReadinessError::SourceLease(SourceLeaseError::Required))
        ));
        claim
    };

    let database = database
        .retarget_catalog_root(replacement_root.path(), &profile_guard)
        .expect("catalog root handoff");
    let mut database = database;
    let mut store = database
        .readiness_store()
        .expect("new owned readiness store");

    assert!(matches!(
        store.renew_lease(&claim, 20, 100),
        Err(error) if matches!(error, ReadinessError::SourcePublicationFence(
            SourcePublicationFenceError::LeaseMismatch
        ))
    ));
    assert!(matches!(
        store.complete(&claim, 20),
        Err(ReadinessError::SourcePublicationFence(
            SourcePublicationFenceError::LeaseMismatch
        ))
    ));
    assert!(matches!(
        store.complete_with_artifact_ref(&claim, 20, "artifact-ref"),
        Err(ReadinessError::SourcePublicationFence(
            SourcePublicationFenceError::LeaseMismatch
        ))
    ));
    let retry_policy = ReadinessRetryPolicy::new(3, 10, 2).expect("retry policy");
    assert!(matches!(
        store.fail(
            &claim,
            ReadinessFailureClassification::Retryable,
            "stale-owner",
            "stale owner must be rejected",
            20,
            retry_policy,
        ),
        Err(ReadinessError::SourcePublicationFence(
            SourcePublicationFenceError::LeaseMismatch
        ))
    ));
    assert!(matches!(
        store.release(&claim, 20),
        Err(ReadinessError::SourcePublicationFence(
            SourcePublicationFenceError::LeaseMismatch
        ))
    ));
    assert!(matches!(
        store.cancel(&claim, "stale owner", 20),
        Err(ReadinessError::SourcePublicationFence(
            SourcePublicationFenceError::LeaseMismatch
        ))
    ));
    drop(store);

    let state: (String, i64, Option<i64>) = database
        .connection
        .query_row(
            "SELECT status, readiness_claim_generation, lease_expires_at
             FROM analysis_jobs
             WHERE source_id = 'readiness-owned-source'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("unchanged claimed job");
    assert_eq!(state, ("running".to_string(), 1, Some(110)));
    assert_eq!(
        database
            .connection
            .query_row(
                "SELECT COUNT(*) FROM source_readiness_artifacts
                 WHERE source_id = 'readiness-owned-source'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("unchanged artifact rows"),
        0
    );

    let mut store = database
        .readiness_store()
        .expect("fresh owned readiness store");
    let fresh = store
        .claim(&target, 200, 100)
        .expect("claim after old lease expiry")
        .expect("fresh claim available");
    assert_eq!(fresh.claim_generation(), 2);
    assert_eq!(
        store.complete(&fresh, 201).expect("complete fresh claim"),
        ArtifactPublishOutcome::Recorded
    );
}

#[test]
fn readiness_claim_fence_rejects_cross_database_claims_and_accepts_own_claims() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let database_root_a = tempdir().expect("database root a");
    let database_root_b = tempdir().expect("database root b");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let mut database_a = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root_a.path(),
    )
    .expect("database a");
    let mut database_b = SourceDatabase::open_for_source_write_with_profile_guard(
        source_root.path(),
        &profile_guard,
        database_root_b.path(),
    )
    .expect("database b");
    let target = ReadinessTarget::source(
        "readiness-cross-database",
        ReadinessStage::SimilarityLayout,
        "layout-v1",
        1,
        "membership-v1",
    );
    let publication = ReadinessTargetPublication::new(
        "readiness-cross-database",
        1,
        1,
        SourceAvailability::Active,
        "readiness-contract-v1",
        std::slice::from_ref(&target),
        1,
    );
    let claim_a = {
        let mut store = database_a.readiness_store().expect("store a");
        let origin = store
            .capture_publication_origin()
            .expect("readiness publication origin");
        store
            .publish_targets(&origin, &publication)
            .expect("publish target a");
        let deficit = store
            .reconcile("readiness-cross-database", 10)
            .expect("reconcile target a")
            .deficits
            .into_iter()
            .next()
            .expect("target a deficit");
        store
            .persist_deficits(&[deficit], 10)
            .expect("persist target a work");
        store
            .claim(&target, 10, 100)
            .expect("claim a")
            .expect("claim a available")
    };
    let mut store_b = database_b.readiness_store().expect("store b");
    let origin_b = store_b
        .capture_publication_origin()
        .expect("database b publication origin");
    store_b
        .publish_targets(&origin_b, &publication)
        .expect("publish target b");
    let deficit = store_b
        .reconcile("readiness-cross-database", 10)
        .expect("reconcile target b")
        .deficits
        .into_iter()
        .next()
        .expect("target b deficit");
    store_b
        .persist_deficits(&[deficit], 10)
        .expect("persist target b work");
    let claim_b = store_b
        .claim(&target, 10, 100)
        .expect("claim b")
        .expect("claim b available");
    assert_eq!(claim_a.target(), claim_b.target());
    assert_eq!(claim_a.claim_generation(), claim_b.claim_generation());

    // Keep the receiving store open while expiring only its durable physical lease. This avoids
    // the SourceDatabase opener renewing the lease before the claim mutation can compare fences.
    let lease_probe_path = database_root_b
        .path()
        .join(crate::sample_sources::db::DB_FILE_NAME);
    let lease_probe = rusqlite::Connection::open(&lease_probe_path).expect("lease probe");
    lease_probe
        .execute(
            "UPDATE source_lease
             SET acquired_at = 1, expires_at = 1
             WHERE singleton = 1",
            [],
        )
        .expect("expire database b physical lease");
    let lease_before: (i64, i64, String, i64) = lease_probe
        .query_row(
            "SELECT acquired_at, expires_at, owner_identity, lease_epoch
             FROM source_lease
             WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("expired database b lease");
    let state_before: (String, i64, Option<i64>) = lease_probe
        .query_row(
            "SELECT status, readiness_claim_generation, lease_expires_at
             FROM analysis_jobs
             WHERE source_id = 'readiness-cross-database'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("database b claim before mismatch");
    let artifacts_before: i64 = lease_probe
        .query_row(
            "SELECT COUNT(*) FROM source_readiness_artifacts
             WHERE source_id = 'readiness-cross-database'",
            [],
            |row| row.get(0),
        )
        .expect("database b artifacts before mismatch");

    assert_eq!(state_before, ("running".to_string(), 1, Some(110)));
    assert_eq!(artifacts_before, 0);
    assert!(matches!(
        store_b.complete(&claim_a, 11),
        Err(ReadinessError::SourcePublicationFence(
            SourcePublicationFenceError::LeaseMismatch
        ))
    ));
    let lease_after: (i64, i64, String, i64) = lease_probe
        .query_row(
            "SELECT acquired_at, expires_at, owner_identity, lease_epoch
             FROM source_lease
             WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("database b lease after mismatch");
    let state_after: (String, i64, Option<i64>) = lease_probe
        .query_row(
            "SELECT status, readiness_claim_generation, lease_expires_at
             FROM analysis_jobs
             WHERE source_id = 'readiness-cross-database'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("database b claim after mismatch");
    let artifacts_after: i64 = lease_probe
        .query_row(
            "SELECT COUNT(*) FROM source_readiness_artifacts
             WHERE source_id = 'readiness-cross-database'",
            [],
            |row| row.get(0),
        )
        .expect("database b artifacts after mismatch");
    assert_eq!(lease_after, lease_before);
    assert_eq!(state_after, state_before);
    assert_eq!(artifacts_after, artifacts_before);
    drop(lease_probe);

    assert_eq!(
        store_b
            .complete(&claim_b, 11)
            .expect("complete b after renewal"),
        ArtifactPublishOutcome::Recorded
    );
    drop(store_b);
    assert!(matches!(
        ReadinessStore::new(&mut database_b.connection).complete(&claim_a, 11),
        Err(ReadinessError::OwnershipRequired)
    ));

    let mut store_a = database_a.readiness_store().expect("store a");
    assert_eq!(
        store_a.complete(&claim_a, 11).expect("complete a"),
        ArtifactPublishOutcome::Recorded
    );
}

#[test]
fn readiness_claim_fence_allows_same_owner_physical_lease_renewal() {
    let app_root = tempdir().expect("app root");
    let source_root = tempdir().expect("source root");
    let app_root_path = std::fs::canonicalize(app_root.path()).expect("canonical app root");
    let _app_root_guard = AppRootGuard::set(app_root_path).expect("app root override");
    let profile_guard = WritableProfileGuard::acquire_current().expect("profile owner");
    let mut database = SourceDatabase::open_for_catalog_with_profile_guard(
        source_root.path(),
        &profile_guard,
        "readiness-claim-fence-renewal",
    )
    .expect("profile-owned source database");
    let target = ReadinessTarget::source(
        "readiness-owned-source",
        ReadinessStage::SimilarityLayout,
        "layout-v1",
        1,
        "membership-v1",
    );
    {
        let mut store = database.readiness_store().expect("owned readiness store");
        let origin = store
            .capture_publication_origin()
            .expect("readiness publication origin");
        let targets = [target.clone()];
        store
            .publish_targets(&origin, &publication(&targets))
            .expect("publish readiness target");
        let deficit = store
            .reconcile("readiness-owned-source", 10)
            .expect("reconcile readiness target")
            .deficits
            .into_iter()
            .next()
            .expect("readiness deficit");
        store
            .persist_deficits(&[deficit], 10)
            .expect("persist readiness work");
    }
    database
        .connection
        .execute(
            "UPDATE source_lease
             SET acquired_at = 1, expires_at = 1
             WHERE singleton = 1",
            [],
        )
        .expect("expire physical lease for renewal test");

    let lease_probe_path = database.db_path.clone();
    let mut store = database
        .readiness_store()
        .expect("renewing readiness store");
    let claim = store
        .claim(&target, 10, 100)
        .expect("claim after physical lease expiry")
        .expect("same owner can renew and claim");
    assert_eq!(claim.claim_generation(), 1);

    // Expire the physical lease again after the claim is issued. The mutation must compare the
    // claim's own fence, then renew this same receiving owner before applying the claim.
    let lease_probe = rusqlite::Connection::open(&lease_probe_path).expect("lease probe");
    lease_probe
        .execute(
            "UPDATE source_lease
             SET acquired_at = 1, expires_at = 1
             WHERE singleton = 1",
            [],
        )
        .expect("expire physical lease after claim");
    assert_eq!(
        lease_probe
            .query_row(
                "SELECT expires_at FROM source_lease WHERE singleton = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("expired physical lease after claim"),
        1
    );
    drop(lease_probe);
    assert_eq!(
        store
            .complete(&claim, 11)
            .expect("complete after lease renewal"),
        ArtifactPublishOutcome::Recorded
    );
    drop(store);
    let renewed_expiry = database
        .connection
        .query_row(
            "SELECT expires_at FROM source_lease WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("renewed physical lease");
    assert!(renewed_expiry > 1);
}
