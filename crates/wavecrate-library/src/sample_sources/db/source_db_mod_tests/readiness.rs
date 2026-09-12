use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use super::*;
use crate::{
    app_dirs::{AppRootGuard, ProfileOwnershipError, WritableProfileGuard},
    sample_sources::readiness::{
        ReadinessError, ReadinessStage, ReadinessTarget, ReadinessTargetPublication,
        SourceAvailability,
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
        .publish_targets(&publication(&targets))
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
    let target = ReadinessTarget::source(
        "readiness-owned-source",
        ReadinessStage::SimilarityLayout,
        "layout-v1",
        1,
        "membership-v1",
    );
    let targets = [target];

    let error = store
        .publish_targets(&publication(&targets))
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
