//! Near-cap real pre-audit SQL lease-row storage measurements, never admitted release.
#![cfg(all(test, target_os = "linux"))]
use super::*;
use crate::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    lifecycle_allocation::workloads::{evidence, isolated},
    sample_sources::{SourceDatabase, SourceId},
    sqlite_budget::{BudgetFailure, LogicalBudget},
};
use rusqlite::{ErrorCode, params};
use std::path::Path;
#[test]
fn lifecycle_vfs_workloads_source_near_cap_exact_lease_sql_shape() {
    let Some(case) = std::env::var("YIELD_LIFECYCLE_VFS_WORKLOAD").ok() else {
        for case in [
            "source-empty",
            "source-near-cap",
            "source-near-cap-lower",
            "source-wal-full",
        ] {
            isolated(
                &format!(
                    "{}::lifecycle_vfs_workloads_source_near_cap_exact_lease_sql_shape",
                    module_path!()
                ),
                case,
            );
        }
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let media = temp.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let writer = SourceWriter::new_for_scan(
        SourceDatabase::open_for_catalog_with_profile_guard(&media, &profile, Path::new("source"))
            .unwrap(),
        SourceId::from_string("source"),
        1,
    )
    .unwrap();
    assert!(writer.current_revision().is_err()); // No audit, SourceRevision or fake fence.
    let c = &writer.database.connection;
    c.execute_batch("PRAGMA wal_autocheckpoint=0; PRAGMA cache_spill=0; PRAGMA mmap_size=0")
        .unwrap();
    if case != "source-empty" {
        // Existing metadata table, unchanged exact compiled schema and required root keys.
        // Seed bounded opaque TEXT storage BEFORE capture/preparation; not admitted effects.
        c.execute_batch("BEGIN; WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<96) INSERT INTO metadata(key,value) SELECT printf('workload-%04d',x),replace(hex(zeroblob(327680)),'0','x') FROM n; COMMIT; PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    }
    let pages: u64 = c
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .unwrap();
    let page_size: u64 = c
        .pragma_query_value(None, "page_size", |r| r.get(0))
        .unwrap();
    if case != "source-empty" {
        assert!(pages * page_size > 60 * 1024 * 1024);
        assert!(pages * page_size < 64 * 1024 * 1024);
    }
    if case == "source-near-cap-lower" {
        c.pragma_update(None, "max_page_count", pages + 2).unwrap();
    }
    let original_max: u64 = c
        .pragma_query_value(None, "max_page_count", |r| r.get(0))
        .unwrap();
    let original: (String,i64,i64,String,Option<i64>)=c.query_row("SELECT owner_identity,lease_epoch,expires_at,state,released_at FROM source_lease WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
    assert_eq!(original.3, "active");
    assert_eq!(original.4, None);
    let deadline = Instant::now() + Duration::from_secs(2);
    let observation = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let allocation = observation.allocation().clone();
    assert_eq!(allocation.original_maximum_pages(), original_max);
    if case == "source-near-cap-lower" {
        assert_eq!(allocation.selected_maximum_pages(), pages + 2);
    }
    let prepared = writer
        .prepare_lease_maintenance(observation, deadline)
        .unwrap();
    prepared.revalidate().unwrap();
    let database = &prepared.custody.writer().database;
    let c = &database.connection;
    let unit = allocation.main().allocation_unit();
    let spec = if case == "source-wal-full" {
        LogicalBudget::new(
            [
                allocation.main().write_ceiling(),
                0,
                allocation.shm().write_ceiling(),
            ],
            unit,
            unit,
            unit,
        )
        .unwrap()
    } else {
        allocation.logical_restriction().unwrap()
    };
    let attempt = c.restrict(spec).unwrap();
    let wal_before = crate::sqlite_wal::retained_wal_bytes(c).unwrap();
    assert_eq!(c.execute("UPDATE source_lease SET state='released',released_at=acquired_at WHERE singleton=1 AND owner_identity=?1 AND lease_epoch=?2 AND expires_at=?3 AND state='active' AND released_at IS NULL",params![original.0,original.1,original.2]).unwrap(),1);
    // This is deliberately a test-private storage shape, not a production permit/release.
    let committed = c.execute_batch("COMMIT");
    if case == "source-wal-full" {
        assert_eq!(
            committed.unwrap_err().sqlite_error_code(),
            Some(ErrorCode::DiskFull)
        );
        if !c.is_autocommit() {
            c.execute_batch("ROLLBACK").unwrap();
        }
        assert_eq!(attempt.report().failure, Some(BudgetFailure::Exhausted));
        assert_eq!(attempt.report().charged[1], 0);
        assert_eq!(
            crate::sqlite_wal::retained_wal_bytes(c).unwrap(),
            wal_before
        );
    } else {
        committed.unwrap();
        assert_eq!(attempt.report().failure, None);
        assert!(attempt.report().charged[1] > 0);
        assert!(prepared.revalidate().is_err()); // Historical original tuple cannot authorize reuse.
    }
    let actual: (String,i64,i64,String,Option<i64>)=c.query_row("SELECT owner_identity,lease_epoch,expires_at,state,released_at FROM source_lease WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
    assert_eq!(
        (&actual.0, actual.1, actual.2),
        (&original.0, original.1, original.2)
    );
    if case == "source-wal-full" {
        assert_eq!(actual, original);
    } else {
        assert_eq!(actual.3, "released");
        assert!(actual.4.is_some());
    }
    let retained: u64=c.query_row("SELECT count(*) FROM metadata WHERE key LIKE 'workload-%' AND length(CAST(value AS BLOB))=655360",[],|r|r.get(0)).unwrap();
    assert_eq!(retained, if case == "source-empty" { 0 } else { 96 });
    let binding = database.database_file_binding.as_deref().unwrap();
    let actual = c
        .retained_wal_objects(
            binding.root(),
            binding.name(),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(actual.main.identity, allocation.main().identity());
    assert_eq!(actual.wal.identity, allocation.wal().identity());
    evidence(&case, c, &allocation, attempt.report(), actual.shm);
    drop(attempt);
    // A measured SQL commit has changed scope. Empty-abort MUST NOT return a reusable writer,
    // issue SourceRetireAck, restore a permit, claim settlement or treat Drop as sealed close.
    assert!(prepared.abort().is_err());
}
